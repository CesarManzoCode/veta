//! Runs a logical plan: splits it into pipelines at the pipeline breaks of contract §12.
//!
//! `Filter` and `Project` stream over the pipeline below them; `Aggregate`, `Sort`, `TopN`
//! and `Limit` end one, and their output is the source of the next. A `Join` ends the
//! pipeline of its right input, which builds, and streams over the pipeline of its left
//! input, which probes (§12): left-deep join trees are one pipeline.
//! `Scan` reads the columns the plan names, so the plan decides what is read (§11.2 rule 5),
//! and skips the parts, granules and row groups its filters exclude unless `--no-prune`
//! (§11.4). Under EXPLAIN ANALYZE each node counts its output rows: a streaming node with a
//! counting operator right after it, a pipeline break on the rows it materialized (§11.6).

use veta_core::memory::Reservation;
use veta_core::{Chunk, Result, Schema};
use veta_sql::{Estimator, LogicalPlan, explain_json};

use crate::aggregate;
use crate::context::{ExecContext, ExecOptions, ExecStats, Profile, add};
use crate::database::Database;
use crate::join;
use crate::pipeline::{Materialized, Pipeline, Source, StreamOp, collect, slice_chunks, stream};
use crate::sort;

/// The rows of a query, in result order.
#[derive(Debug)]
pub struct QueryResult {
    pub schema: Schema,
    pub chunks: Vec<Chunk>,
    _hold: Vec<Reservation<'static>>,
}

/// Executes `plan` against `db`, counting in `stats`.
pub fn execute(
    db: &Database,
    plan: &LogicalPlan,
    options: &ExecOptions,
    stats: &ExecStats,
) -> Result<QueryResult> {
    let ctx = ExecContext {
        db,
        threads: options.threads.max(1),
        stats,
        prune: options.prune,
        profile: None,
    };
    let materialized = execute_plan(plan, &ctx)?;
    Ok(QueryResult {
        schema: plan.schema(),
        chunks: materialized.chunks,
        _hold: materialized.hold,
    })
}

/// Executes `plan` and hands its chunks to `sink`, in result order, as they are ready. A plan
/// whose top is a pipeline break materializes that break first; any other keeps in memory
/// only the morsels on the way (`veta export`).
pub fn execute_streaming(
    db: &Database,
    plan: &LogicalPlan,
    options: &ExecOptions,
    stats: &ExecStats,
    sink: &mut (dyn FnMut(Chunk) -> Result<()> + Send),
) -> Result<()> {
    let ctx = ExecContext {
        db,
        threads: options.threads.max(1),
        stats,
        prune: options.prune,
        profile: None,
    };
    stream(build_pipeline(plan, &ctx)?, &ctx, sink)
}

/// Executes `plan`, counting the rows of each node, and returns the JSON of §11.6 in place of
/// the rows.
pub fn explain_analyze(
    db: &Database,
    plan: &LogicalPlan,
    options: &ExecOptions,
    stats: &ExecStats,
) -> Result<String> {
    let profile = Profile::new(plan);
    let ctx = ExecContext {
        db,
        threads: options.threads.max(1),
        stats,
        prune: options.prune,
        profile: Some(&profile),
    };
    execute_plan(plan, &ctx)?;
    let est_rows = Estimator::new(db).estimate_tree(plan);
    Ok(explain_json(plan, &est_rows, Some(&profile.counts())))
}

/// The JSON of §11.6 without executing `plan`: every `actual_rows` is `null`.
pub fn explain(db: &Database, plan: &LogicalPlan) -> String {
    explain_json(plan, &Estimator::new(db).estimate_tree(plan), None)
}

/// Runs `plan` up to its pipeline break and materializes the output.
fn execute_plan(plan: &LogicalPlan, ctx: &ExecContext<'_>) -> Result<Materialized> {
    let output = match plan {
        LogicalPlan::Aggregate {
            input,
            groups,
            aggregates,
            ..
        } => aggregate::run(build_pipeline(input, ctx)?, groups, aggregates, ctx),
        LogicalPlan::Sort { input, keys } => sort::sort(build_pipeline(input, ctx)?, keys, ctx),
        LogicalPlan::TopN {
            input,
            keys,
            limit,
            offset,
        } => match sort::LateTopN::plan(input, keys).filter(|_| ctx.profile.is_none()) {
            // EXPLAIN ANALYZE counts the rows of every node, so it keeps the one-pass form.
            Some(late) => sort::late_top_n(&late, *limit, *offset, ctx),
            None => sort::top_n(build_pipeline(input, ctx)?, keys, *limit, *offset, ctx),
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            if *limit == Some(0) {
                return Ok(Materialized {
                    chunks: Vec::new(),
                    hold: Vec::new(),
                });
            }
            let needed = limit.map(|l| offset.saturating_add(l));
            let rows = collect(build_pipeline(input, ctx)?, ctx, needed)?;
            Ok(Materialized {
                chunks: slice_chunks(rows.chunks, *offset, *limit),
                hold: rows.hold,
            })
        }
        LogicalPlan::Scan { .. }
        | LogicalPlan::ParquetScan { .. }
        | LogicalPlan::SingleRow
        | LogicalPlan::Filter { .. }
        | LogicalPlan::Project { .. }
        | LogicalPlan::Join { .. } => return collect(build_pipeline(plan, ctx)?, ctx, None),
    }?;
    if let Some(counter) = ctx.profile.and_then(|profile| profile.counter(plan)) {
        let rows: usize = output.chunks.iter().map(|chunk| chunk.len).sum();
        add(&counter, rows as u64);
    }
    Ok(output)
}

/// The pipeline that ends at `plan`, which under EXPLAIN ANALYZE counts the rows of a
/// streaming node; a pipeline break counts its own in [`execute_plan`].
fn build_pipeline(plan: &LogicalPlan, ctx: &ExecContext<'_>) -> Result<Pipeline> {
    let mut pipeline = plan_pipeline(plan, ctx)?;
    let streams = !matches!(
        plan,
        LogicalPlan::Aggregate { .. }
            | LogicalPlan::Sort { .. }
            | LogicalPlan::TopN { .. }
            | LogicalPlan::Limit { .. }
    );
    if streams && let Some(counter) = ctx.profile.and_then(|profile| profile.counter(plan)) {
        pipeline.push_op(StreamOp::Count(counter));
    }
    Ok(pipeline)
}

/// The streaming operators of `plan` over a source, which is the materialized output of the
/// pipeline break below, if any.
fn plan_pipeline(plan: &LogicalPlan, ctx: &ExecContext<'_>) -> Result<Pipeline> {
    match plan {
        LogicalPlan::Scan {
            table,
            columns,
            filters,
            ..
        } => Ok(Pipeline::new(Source::table(ctx, table, columns, filters)?)),
        LogicalPlan::ParquetScan {
            path,
            columns,
            filters,
            ..
        } => Ok(Pipeline::new(Source::parquet(ctx, path, columns, filters)?)),
        LogicalPlan::SingleRow => Ok(Pipeline::new(Source::single_row()?)),
        LogicalPlan::Filter { input, predicate } => {
            let mut pipeline = build_pipeline(input, ctx)?;
            pipeline.push_op(StreamOp::filter(predicate)?);
            Ok(pipeline)
        }
        LogicalPlan::Project { input, exprs, .. } => {
            let mut pipeline = build_pipeline(input, ctx)?;
            pipeline.push_op(StreamOp::project(exprs)?);
            Ok(pipeline)
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            keys,
            residual,
            ..
        } => {
            let probe = join::build(
                build_pipeline(right, ctx)?,
                *kind,
                keys,
                residual.as_ref(),
                left.schema().len(),
                right.schema().data_types(),
                ctx,
            )?;
            if probe.produces_nothing() {
                return Ok(Pipeline::over(Materialized {
                    chunks: Vec::new(),
                    hold: Vec::new(),
                }));
            }
            let mut pipeline = build_pipeline(left, ctx)?;
            pipeline.push_op(StreamOp::Join(Box::new(probe)));
            Ok(pipeline)
        }
        _ => Ok(Pipeline::over(execute_plan(plan, ctx)?)),
    }
}

#[cfg(test)]
mod prune_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::{DataType, Value, Vector};
    use veta_sql::{JoinOrder, OptimizerOptions, Statement, bind_query, optimize, parse_statement};
    use veta_storage::insert;

    fn database(tag: &str) -> (Database, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("veta-exec-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Database::open(&dir), dir)
    }

    fn create(db: &Database, sql: &str) {
        match parse_statement(sql).unwrap() {
            Statement::CreateTable(create) => {
                db.create_table(&create).unwrap();
            }
            other => panic!("not a CREATE TABLE: {other:?}"),
        }
    }

    fn query(db: &Database, sql: &str, threads: usize) -> Result<Vec<Vec<Value>>> {
        let Statement::Query(q) = parse_statement(sql)? else {
            panic!("not a query: {sql}");
        };
        let plan = bind_query(db, &q)?;
        let stats = ExecStats::default();
        let result = execute(
            db,
            &plan,
            &ExecOptions {
                threads,
                prune: true,
            },
            &stats,
        )?;
        Ok(result
            .chunks
            .iter()
            .flat_map(|c| (0..c.len).map(move |i| c.row(i)))
            .collect())
    }

    fn table_with_rows(tag: &str) -> (Database, std::path::PathBuf) {
        let (db, dir) = database(tag);
        create(
            &db,
            "CREATE TABLE t (a BIGINT NOT NULL, s VARCHAR, PRIMARY KEY (a))",
        );
        let table = db.open_table("t").unwrap();
        for batch in 0..3i64 {
            let a: Vec<Value> = (0..1000).map(|i| Value::Int64(batch * 1000 + i)).collect();
            let s: Vec<Value> = (0..1000)
                .map(|i| {
                    if i % 10 == 0 {
                        Value::Null
                    } else {
                        Value::varchar(format!("v{}", i % 7))
                    }
                })
                .collect();
            let chunk = Chunk::new(vec![
                Vector::from_values(DataType::Int64, &a).unwrap(),
                Vector::from_values(DataType::Varchar, &s).unwrap(),
            ])
            .unwrap();
            insert(&table, &chunk).unwrap();
        }
        (db, dir)
    }

    #[test]
    fn streaming_gives_the_rows_of_a_query_in_order() {
        let (db, dir) = table_with_rows("stream");
        for sql in [
            "SELECT a, s FROM t",
            "SELECT a FROM t WHERE a % 3 = 0",
            "SELECT s, a FROM t ORDER BY a DESC",
            "SELECT s, count(*) FROM t GROUP BY s ORDER BY s",
            "SELECT a FROM t ORDER BY a LIMIT 5 OFFSET 2",
            "SELECT a FROM t WHERE a < 0",
            "SELECT 1 + 1",
        ] {
            let Statement::Query(q) = parse_statement(sql).unwrap() else {
                panic!("not a query: {sql}");
            };
            let plan = bind_query(&db, &q).unwrap();
            for threads in [1, 8] {
                let options = ExecOptions {
                    threads,
                    prune: true,
                };
                let expected = query(&db, sql, threads).unwrap();
                let mut rows = Vec::new();
                execute_streaming(&db, &plan, &options, &ExecStats::default(), &mut |c| {
                    rows.extend((0..c.len).map(|i| c.row(i)));
                    Ok(())
                })
                .unwrap();
                assert_eq!(rows, expected, "{sql}");
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn select_without_from() {
        let (db, _dir) = database("no-from");
        assert_eq!(
            query(&db, "SELECT 1 + 1", 2).unwrap(),
            vec![vec![Value::Int64(2)]]
        );
    }

    #[test]
    fn scans_filters_and_limits_stop_early_with_the_same_rows() {
        let (db, dir) = table_with_rows("scan");
        for threads in [1, 8] {
            assert_eq!(
                query(&db, "SELECT count(*) FROM t", threads).unwrap(),
                vec![vec![Value::Int64(3000)]]
            );
            assert_eq!(
                query(&db, "SELECT count(*) FROM t WHERE s IS NULL", threads).unwrap(),
                vec![vec![Value::Int64(300)]]
            );
            let rows = query(
                &db,
                "SELECT a FROM t ORDER BY a DESC LIMIT 2 OFFSET 1",
                threads,
            )
            .unwrap();
            assert_eq!(
                rows,
                vec![vec![Value::Int64(2998)], vec![Value::Int64(2997)]]
            );
            assert_eq!(
                query(&db, "SELECT a FROM t LIMIT 0", threads)
                    .unwrap()
                    .len(),
                0
            );
            assert_eq!(
                query(&db, "SELECT a FROM t LIMIT 5", threads)
                    .unwrap()
                    .len(),
                5
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stats_count_what_was_read() {
        let (db, dir) = table_with_rows("stats");
        let Statement::Query(q) = parse_statement("SELECT a FROM t").unwrap() else {
            panic!("not a query");
        };
        let plan = bind_query(&db, &q).unwrap();
        let stats = ExecStats::default();
        execute(
            &db,
            &plan,
            &ExecOptions {
                threads: 2,
                prune: true,
            },
            &stats,
        )
        .unwrap();
        assert_eq!(
            stats.to_json(),
            "{\"parts_total\":3,\"parts_read\":3,\"granules_total\":3,\"granules_read\":3,\"rows_read\":3000,\"row_groups_total\":0,\"row_groups_read\":0}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Rows of `sql` with the optimizer on or off, as the CLI plans it.
    fn optimized_query(
        db: &Database,
        sql: &str,
        threads: usize,
        enabled: bool,
    ) -> Result<Vec<Vec<Value>>> {
        let Statement::Query(q) = parse_statement(sql)? else {
            panic!("not a query: {sql}");
        };
        let plan = optimize(
            bind_query(db, &q)?,
            OptimizerOptions {
                enabled,
                ..Default::default()
            },
            db,
        );
        let stats = ExecStats::default();
        let result = execute(
            db,
            &plan,
            &ExecOptions {
                threads,
                prune: true,
            },
            &stats,
        )?;
        Ok(result
            .chunks
            .iter()
            .flat_map(|c| (0..c.len).map(move |i| c.row(i)))
            .collect())
    }

    /// `a(id, k)` = (1, 10), (2, 20), (3, NULL), (4, 10), (5, 30) and
    /// `b(id, k)` = (1, 10), (2, 10), (3, 20), (4, NULL), (5, 40).
    fn join_tables(tag: &str) -> (Database, std::path::PathBuf) {
        let (db, dir) = database(tag);
        for (name, keys) in [
            ("a", [Some(10), Some(20), None, Some(10), Some(30)]),
            ("b", [Some(10), Some(10), Some(20), None, Some(40)]),
        ] {
            create(
                &db,
                &format!("CREATE TABLE {name} (id BIGINT NOT NULL, k BIGINT, PRIMARY KEY (id))"),
            );
            let table = db.open_table(name).unwrap();
            let ids: Vec<Value> = (1..=5).map(Value::Int64).collect();
            let ks: Vec<Value> = keys
                .iter()
                .map(|k| k.map_or(Value::Null, Value::Int64))
                .collect();
            let chunk = Chunk::new(vec![
                Vector::from_values(DataType::Int64, &ids).unwrap(),
                Vector::from_values(DataType::Int64, &ks).unwrap(),
            ])
            .unwrap();
            insert(&table, &chunk).unwrap();
        }
        (db, dir)
    }

    fn ints(rows: &[&[Option<i64>]]) -> Vec<Vec<Value>> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.map_or(Value::Null, Value::Int64))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn every_join_kind_runs_from_sql() {
        let (db, dir) = join_tables("joins");
        let cases: Vec<(&str, Vec<Vec<Value>>)> = vec![
            (
                "SELECT count(*) FROM a JOIN b ON a.k = b.k",
                ints(&[&[Some(5)]]),
            ),
            (
                "SELECT count(*) FROM a, b WHERE a.k = b.k AND a.id <> b.id",
                ints(&[&[Some(4)]]),
            ),
            ("SELECT count(*) FROM a CROSS JOIN b", ints(&[&[Some(25)]])),
            (
                "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.id, b.id",
                ints(&[
                    &[Some(1), Some(1)],
                    &[Some(1), Some(2)],
                    &[Some(2), Some(3)],
                    &[Some(3), None],
                    &[Some(4), Some(1)],
                    &[Some(4), Some(2)],
                    &[Some(5), None],
                ]),
            ),
            (
                "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k AND b.id > 1 ORDER BY a.id",
                ints(&[
                    &[Some(1), Some(2)],
                    &[Some(2), Some(3)],
                    &[Some(3), None],
                    &[Some(4), Some(2)],
                    &[Some(5), None],
                ]),
            ),
            (
                "SELECT id FROM a WHERE k IN (SELECT k FROM b) ORDER BY id",
                ints(&[&[Some(1)], &[Some(2)], &[Some(4)]]),
            ),
            (
                "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b) ORDER BY id",
                Vec::new(),
            ),
            (
                "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b WHERE k IS NOT NULL) ORDER BY id",
                ints(&[&[Some(5)]]),
            ),
            (
                "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b WHERE id > 100) ORDER BY id",
                ints(&[&[Some(1)], &[Some(2)], &[Some(3)], &[Some(4)], &[Some(5)]]),
            ),
            (
                "SELECT id FROM a WHERE EXISTS (SELECT * FROM b WHERE b.k = a.k AND b.id <> a.id) \
                 ORDER BY id",
                ints(&[&[Some(1)], &[Some(2)], &[Some(4)]]),
            ),
            (
                "SELECT id FROM a WHERE NOT EXISTS \
                 (SELECT * FROM b WHERE b.k = a.k AND b.id <> a.id) ORDER BY id",
                ints(&[&[Some(3)], &[Some(5)]]),
            ),
            (
                "SELECT id, (SELECT max(id) FROM b) FROM a ORDER BY id",
                ints(&[
                    &[Some(1), Some(5)],
                    &[Some(2), Some(5)],
                    &[Some(3), Some(5)],
                    &[Some(4), Some(5)],
                    &[Some(5), Some(5)],
                ]),
            ),
            (
                "SELECT id, (SELECT id FROM b WHERE id > 100) FROM a ORDER BY id LIMIT 1",
                ints(&[&[Some(1), None]]),
            ),
            (
                "SELECT id, (SELECT count(*) FROM b WHERE b.k = a.k) FROM a ORDER BY id",
                ints(&[
                    &[Some(1), Some(2)],
                    &[Some(2), Some(1)],
                    &[Some(3), Some(0)],
                    &[Some(4), Some(2)],
                    &[Some(5), Some(0)],
                ]),
            ),
        ];
        for (sql, expected) in &cases {
            for threads in [1, 8] {
                for enabled in [true, false] {
                    let got = optimized_query(&db, sql, threads, enabled)
                        .unwrap_or_else(|e| panic!("{sql}: {e}"));
                    assert_eq!(
                        &got, expected,
                        "{sql} threads {threads} optimizer {enabled}"
                    );
                }
            }
        }
        for threads in [1, 8] {
            let err = optimized_query(&db, "SELECT id, (SELECT id FROM b) FROM a", threads, true)
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                "more than one row returned by a subquery used as an expression"
            );
            assert_eq!(err.exit_code(), 1);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `(op, actual_rows)` of every node of the JSON of §11.6, in preorder.
    fn actual_rows(json: &str) -> Vec<(String, u64)> {
        json.split("{\"op\":\"")
            .skip(1)
            .map(|node| {
                let (op, rest) = node.split_once('"').unwrap();
                let (_, rest) = rest.split_once("\"actual_rows\":").unwrap();
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                (op.to_string(), digits.parse().unwrap())
            })
            .collect()
    }

    fn explained(db: &Database, sql: &str, threads: usize) -> String {
        let Statement::Query(q) = parse_statement(sql).unwrap() else {
            panic!("not a query: {sql}");
        };
        let plan = optimize(bind_query(db, &q).unwrap(), OptimizerOptions::default(), db);
        let stats = ExecStats::default();
        let options = ExecOptions {
            threads,
            prune: true,
        };
        explain_analyze(db, &plan, &options, &stats).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    #[test]
    fn explain_analyze_counts_the_rows_of_each_node() {
        let (db, dir) = join_tables("explain");
        for threads in [1, 8] {
            // a.k = b.k matches 1-1, 1-2, 2-3, 4-1 and 4-2.
            assert_eq!(
                actual_rows(&explained(
                    &db,
                    "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k",
                    threads
                )),
                [("Project", 5u64), ("HashJoin", 5), ("Scan", 5), ("Scan", 5)]
                    .map(|(op, rows)| (op.to_string(), rows))
            );
            let rows = actual_rows(&explained(
                &db,
                "SELECT k, count(*) FROM a WHERE id > 2 GROUP BY k ORDER BY k LIMIT 2",
                threads,
            ));
            let of = |op: &str| {
                rows.iter()
                    .find(|(name, _)| name == op)
                    .unwrap_or_else(|| panic!("no {op} in {rows:?}"))
                    .1
            };
            // Rows with id 3, 4 and 5 have the keys NULL, 10 and 30.
            assert_eq!(of("Scan"), 5);
            assert_eq!(of("Filter"), 3);
            assert_eq!(of("HashAggregate"), 3);
            assert_eq!(of("TopN"), 2);
            let rows = actual_rows(&explained(
                &db,
                "SELECT id FROM a WHERE k IN (SELECT k FROM b) LIMIT 2",
                threads,
            ));
            assert_eq!(rows[0], ("Limit".to_string(), 2));
            assert!(rows.iter().any(|(op, _)| op == "HashJoin"), "{rows:?}");
        }
        let json = explained(&db, "SELECT a.id FROM a, b", 2);
        assert!(
            json.contains("\"op\":\"NestedLoopJoin\",\"kind\":\"cross\""),
            "{json}"
        );
        assert_eq!(actual_rows(&json)[0], ("Project".to_string(), 25));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn explain_without_analyze_does_not_execute() {
        let (db, dir) = join_tables("explain-plain");
        let Statement::Query(q) = parse_statement("SELECT id FROM a").unwrap() else {
            panic!("not a query");
        };
        let plan = bind_query(&db, &q).unwrap();
        let json = explain(&db, &plan);
        assert!(json.contains("\"actual_rows\":null"), "{json}");
        assert!(!json.contains("\"actual_rows\":5"), "{json}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Rows of `sql` under one set of optimizer options, sorted.
    fn sorted_rows(db: &Database, sql: &str, options: OptimizerOptions) -> Vec<Vec<Value>> {
        let Statement::Query(q) = parse_statement(sql).unwrap() else {
            panic!("not a query: {sql}");
        };
        let plan = optimize(bind_query(db, &q).unwrap(), options, db);
        let stats = ExecStats::default();
        let exec = ExecOptions {
            threads: 4,
            prune: true,
        };
        let result = execute(db, &plan, &exec, &stats).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let mut rows: Vec<Vec<Value>> = result
            .chunks
            .iter()
            .flat_map(|c| (0..c.len).map(move |i| c.row(i)))
            .collect();
        rows.sort();
        rows
    }

    /// `r0(a, b) … r3(a, b)` of different sizes, with repeated values and NULLs in `b`.
    fn reorder_tables(tag: &str) -> (Database, std::path::PathBuf) {
        let (db, dir) = database(tag);
        for (r, size) in [40i64, 300, 5, 90].into_iter().enumerate() {
            let name = format!("r{r}");
            create(
                &db,
                &format!("CREATE TABLE {name} (a BIGINT NOT NULL, b BIGINT, PRIMARY KEY (a))"),
            );
            let table = db.open_table(&name).unwrap();
            let a: Vec<Value> = (0..size).map(Value::Int64).collect();
            let b: Vec<Value> = (0..size)
                .map(|i| {
                    if i % 11 == 3 {
                        Value::Null
                    } else {
                        Value::Int64(i % 7)
                    }
                })
                .collect();
            let chunk = Chunk::new(vec![
                Vector::from_values(DataType::Int64, &a).unwrap(),
                Vector::from_values(DataType::Int64, &b).unwrap(),
            ])
            .unwrap();
            insert(&table, &chunk).unwrap();
        }
        (db, dir)
    }

    #[test]
    fn every_join_order_returns_the_same_rows() {
        let (db, dir) = reorder_tables("reorder");
        let off = OptimizerOptions {
            enabled: false,
            ..Default::default()
        };
        let syntactic = OptimizerOptions {
            join_order: JoinOrder::Syntactic,
            ..Default::default()
        };
        for sql in [
            "SELECT r0.a, r1.a, r2.a, r3.a FROM r0, r1, r2, r3 \
             WHERE r0.a = r1.b AND r1.a = r2.b AND r2.a = r3.b",
            "SELECT * FROM r1, r3, r2 WHERE r1.b = r3.b AND r3.a = r2.a AND r1.a > 5",
            "SELECT r1.a, r2.a FROM r1, r2 WHERE r1.a < r2.a",
            "SELECT r2.a, r1.a FROM r2, r1",
            "SELECT r1.a, r2.b, r3.a FROM r1 JOIN r2 ON r1.a = r2.a \
             LEFT JOIN r3 ON r2.b = r3.b JOIN r0 ON r1.a = r0.a",
            "SELECT r1.a FROM r1, r3 WHERE r1.b = r3.b AND EXISTS \
             (SELECT * FROM r2 WHERE r2.a = r1.a)",
            "SELECT r1.b, count(*), sum(r3.a) FROM r1, r3, r2 \
             WHERE r1.a = r3.a AND r3.b = r2.b AND r1.a + r2.a > r3.a GROUP BY r1.b",
        ] {
            let by_cost = sorted_rows(&db, sql, OptimizerOptions::default());
            assert_eq!(by_cost, sorted_rows(&db, sql, syntactic), "{sql}");
            assert_eq!(by_cost, sorted_rows(&db, sql, off), "{sql}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `t(id, k, s, w)` in three parts: `k` with ties and NULLs, `s` and `w` strings.
    fn top_n_table(tag: &str) -> (Database, std::path::PathBuf) {
        let (db, dir) = database(tag);
        create(
            &db,
            "CREATE TABLE t (id BIGINT NOT NULL, k BIGINT, s VARCHAR, w VARCHAR, PRIMARY KEY (id))",
        );
        let table = db.open_table("t").unwrap();
        let mut rng = veta_core::rand::Rng::new(77);
        for batch in 0..3i64 {
            let rows: Vec<Vec<Value>> = (0..9000)
                .map(|i| {
                    let k = if rng.next_bool(0.05) {
                        Value::Null
                    } else {
                        Value::Int64(rng.range_i64(0, 40))
                    };
                    let s = Value::varchar(["abc", "xyz", "bab", ""][rng.below(4) as usize]);
                    let w = Value::varchar(format!("w{}", rng.below(1000)));
                    vec![Value::Int64(batch * 100_000 + i), k, s, w]
                })
                .collect();
            let types = [
                DataType::Int64,
                DataType::Int64,
                DataType::Varchar,
                DataType::Varchar,
            ];
            insert(&table, &Chunk::from_rows(&types, &rows).unwrap()).unwrap();
        }
        (db, dir)
    }

    fn rows_of(chunks: &[Chunk]) -> Vec<Vec<Value>> {
        chunks
            .iter()
            .flat_map(|c| (0..c.len).map(move |i| c.row(i)))
            .collect()
    }

    /// The top-N node of an optimized plan.
    fn find_top_n(plan: &LogicalPlan) -> Option<&LogicalPlan> {
        match plan {
            LogicalPlan::TopN { .. } => Some(plan),
            LogicalPlan::Project { input, .. }
            | LogicalPlan::Filter { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Sort { input, .. } => find_top_n(input),
            _ => None,
        }
    }

    #[test]
    fn late_top_n_matches_the_one_pass_top_n_and_the_full_sort() {
        let (db, dir) = top_n_table("late-top-n");
        let cases = [
            ("SELECT * FROM t WHERE s LIKE '%b%' ORDER BY k DESC", 7, 3),
            ("SELECT id, w FROM t ORDER BY k", 20, 0),
            (
                "SELECT w, id, k FROM t WHERE k > 3 ORDER BY k NULLS FIRST",
                15,
                40,
            ),
            ("SELECT * FROM t WHERE s <> '' ORDER BY k, s DESC", 9000, 5),
        ];
        for (sql, limit, offset) in cases {
            let parse = |sql: &str| {
                let Statement::Query(q) = parse_statement(sql).unwrap() else {
                    panic!("not a query: {sql}");
                };
                optimize(
                    bind_query(&db, &q).unwrap(),
                    OptimizerOptions::default(),
                    &db,
                )
            };
            // The full sort on one thread keeps the table order between equal keys.
            let full = optimized_query(&db, sql, 1, true).unwrap();
            let start = (offset as usize).min(full.len());
            let end = (start + limit as usize).min(full.len());
            let expected = full[start..end].to_vec();
            let limited = format!("{sql} LIMIT {limit} OFFSET {offset}");
            let plan = parse(&limited);
            let Some(LogicalPlan::TopN { input, keys, .. }) = find_top_n(&plan) else {
                panic!("no TopN in the plan of {limited}");
            };
            let late = sort::LateTopN::plan(input, keys)
                .unwrap_or_else(|| panic!("{limited} has no late form"));
            for threads in [1, 8] {
                let stats = ExecStats::default();
                let ctx = ExecContext {
                    db: &db,
                    threads,
                    stats: &stats,
                    prune: true,
                    profile: None,
                };
                let two_pass = sort::late_top_n(&late, limit, offset, &ctx).unwrap();
                let one_pass = sort::top_n(
                    build_pipeline(input, &ctx).unwrap(),
                    keys,
                    limit,
                    offset,
                    &ctx,
                )
                .unwrap();
                assert_eq!(rows_of(&two_pass.chunks), expected, "{limited}, {threads}");
                assert_eq!(rows_of(&one_pass.chunks), expected, "{limited}, {threads}");
                assert_eq!(
                    optimized_query(&db, &limited, threads, true).unwrap(),
                    expected,
                    "{limited}, {threads}"
                );
            }
        }
        // Sorting on every output column leaves nothing for a second pass.
        let Statement::Query(q) = parse_statement("SELECT k FROM t ORDER BY k LIMIT 3").unwrap()
        else {
            panic!("not a query");
        };
        let plan = optimize(
            bind_query(&db, &q).unwrap(),
            OptimizerOptions::default(),
            &db,
        );
        if let Some(LogicalPlan::TopN { input, keys, .. }) = find_top_n(&plan) {
            assert!(sort::LateTopN::plan(input, keys).is_none());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn early_filters_and_array_aggregation_give_the_rows_of_the_plain_paths() {
        let (db, dir) = top_n_table("early-filter");
        for sql in [
            "SELECT count(*), sum(id) FROM t WHERE s = 'xyz'",
            "SELECT id, w FROM t WHERE k = 7 AND s LIKE '%b%' ORDER BY id",
            "SELECT k, count(*), min(w) FROM t GROUP BY k ORDER BY k",
            "SELECT k, count(*) FROM t WHERE w LIKE 'w1%' GROUP BY k ORDER BY k",
            "SELECT s, count(*) FROM t WHERE s <> '' GROUP BY s ORDER BY s",
            // `id` is NOT NULL and its range fits the array of the aggregation.
            "SELECT id, count(*), max(w) FROM t WHERE k = 3 GROUP BY id ORDER BY id",
        ] {
            // The optimizer off keeps the filters and projections in their written form;
            // both plans run the same paths, so they only check each other with the rows
            // computed below by hand.
            for threads in [1, 8] {
                assert_eq!(
                    optimized_query(&db, sql, threads, true).unwrap(),
                    optimized_query(&db, sql, threads, false).unwrap(),
                    "{sql}"
                );
            }
        }
        // Rows by hand: every row of the table, then the query in Rust.
        let all = optimized_query(&db, "SELECT id, k, s, w FROM t ORDER BY id", 1, true).unwrap();
        let mut counts: std::collections::BTreeMap<Value, i64> = Default::default();
        for row in &all {
            *counts.entry(row[1].clone()).or_insert(0) += 1;
        }
        let mut expected: Vec<Vec<Value>> = counts
            .into_iter()
            .map(|(k, c)| vec![k, Value::Int64(c)])
            .collect();
        // NULLS LAST in ascending order.
        expected.rotate_left(usize::from(
            expected.first().is_some_and(|r| r[0].is_null()),
        ));
        assert_eq!(
            optimized_query(
                &db,
                "SELECT k, count(*) FROM t GROUP BY k ORDER BY k",
                4,
                true
            )
            .unwrap(),
            expected
        );
        let filtered: Vec<Vec<Value>> = all
            .iter()
            .filter(|r| {
                r[1] == Value::Int64(7) && r[2].as_bytes().is_some_and(|s| s.contains(&b'b'))
            })
            .map(|r| vec![r[0].clone(), r[3].clone()])
            .collect();
        assert_eq!(
            optimized_query(
                &db,
                "SELECT id, w FROM t WHERE k = 7 AND s LIKE '%b%' ORDER BY id",
                4,
                true
            )
            .unwrap(),
            filtered
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_block_fails_with_exit_3_and_detaches_the_part() {
        let (db, dir) = table_with_rows("corrupt");
        let bin = dir.join("t").join("all_2_2_0").join("c0.bin");
        let mut bytes = std::fs::read(&bin).unwrap();
        bytes[20] ^= 0xFF;
        std::fs::write(&bin, bytes).unwrap();
        // The checksums of column data are only verified block by block.
        let err = query(&db, "SELECT sum(a) FROM t", 4).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(
            dir.join("t")
                .join("detached")
                .join("broken_all_2_2_0")
                .is_dir()
        );
        assert_eq!(
            query(&db, "SELECT count(*) FROM t", 4).unwrap(),
            vec![vec![Value::Int64(2000)]]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
