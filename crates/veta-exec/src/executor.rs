//! Runs a logical plan: splits it into pipelines at the pipeline breaks of contract §12.
//!
//! `Filter` and `Project` stream over the pipeline below them; `Aggregate`, `Sort`, `TopN`
//! and `Limit` end one, and their output is the source of the next. A `Join` ends the
//! pipeline of its right input, which builds, and streams over the pipeline of its left
//! input, which probes (§12): left-deep join trees are one pipeline.
//! `Scan` reads the columns the plan names, so the plan decides what is read (§11.2 rule 5),
//! and skips the parts, granules and row groups its filters exclude unless `--no-prune`
//! (§11.4).

use veta_core::memory::Reservation;
use veta_core::{Chunk, Result, Schema};
use veta_sql::LogicalPlan;

use crate::aggregate;
use crate::context::{ExecContext, ExecOptions, ExecStats};
use crate::database::Database;
use crate::join;
use crate::pipeline::{Materialized, Pipeline, Source, StreamOp, collect, slice_chunks};
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
    };
    let materialized = execute_plan(plan, &ctx)?;
    Ok(QueryResult {
        schema: plan.schema(),
        chunks: materialized.chunks,
        _hold: materialized.hold,
    })
}

/// Runs `plan` up to its pipeline break and materializes the output.
fn execute_plan(plan: &LogicalPlan, ctx: &ExecContext<'_>) -> Result<Materialized> {
    match plan {
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
        } => sort::top_n(
            build_pipeline(input, ctx)?,
            keys,
            *limit,
            *offset,
            &input.schema().data_types(),
            ctx,
        ),
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
        | LogicalPlan::Join { .. } => collect(build_pipeline(plan, ctx)?, ctx, None),
    }
}

/// The pipeline that ends at `plan`: its streaming operators over a source, which is the
/// materialized output of the pipeline break below, if any.
fn build_pipeline(plan: &LogicalPlan, ctx: &ExecContext<'_>) -> Result<Pipeline> {
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
    use veta_sql::{OptimizerOptions, Statement, bind_query, optimize, parse_statement};
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
        let plan = optimize(bind_query(db, &q)?, OptimizerOptions { enabled });
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
