//! Runs a logical plan: splits it into pipelines at the pipeline breaks of contract §12.
//!
//! `Filter` and `Project` stream over the pipeline below them; `Aggregate`, `Sort`, `TopN`
//! and `Limit` end one, and their output is the source of the next. Joins arrive with K2.
//! `Scan` reads the columns the plan names, so the plan decides what is read (§11.2 rule 5),
//! and skips the parts, granules and row groups its filters exclude unless `--no-prune`
//! (§11.4).

use veta_core::memory::Reservation;
use veta_core::{Chunk, Result, Schema, VetaError};
use veta_sql::LogicalPlan;

use crate::aggregate;
use crate::context::{ExecContext, ExecOptions, ExecStats};
use crate::database::Database;
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
        LogicalPlan::Join { .. } => Err(VetaError::unsupported("join")),
        LogicalPlan::Scan { .. }
        | LogicalPlan::ParquetScan { .. }
        | LogicalPlan::SingleRow
        | LogicalPlan::Filter { .. }
        | LogicalPlan::Project { .. } => collect(build_pipeline(plan, ctx)?, ctx, None),
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
        _ => Ok(Pipeline::over(execute_plan(plan, ctx)?)),
    }
}

#[cfg(test)]
mod prune_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::{DataType, Value, Vector};
    use veta_sql::{Statement, bind_query, parse_statement};
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

    #[test]
    fn joins_are_unsupported_for_now() {
        let (db, dir) = table_with_rows("join");
        let err = query(&db, "SELECT count(*) FROM t AS x, t AS y", 1).unwrap_err();
        assert_eq!(err.to_string(), "unsupported: join");
        assert_eq!(err.exit_code(), 1);
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
