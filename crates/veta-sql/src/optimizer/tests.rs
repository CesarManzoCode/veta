use std::ops::Bound;

use veta_core::predicate::ScanFilter;
use veta_core::{DataType, Field, Schema, Value};

use super::*;
use crate::ast::Statement;
use crate::binder::bind_query;
use crate::catalog::MemoryCatalog;
use crate::parser::parse_statement;

fn catalog() -> MemoryCatalog {
    use DataType::*;
    let fields = |cols: &[(&str, DataType)]| {
        Schema::new(
            cols.iter()
                .map(|(n, t)| Field::new(*n, *t, false))
                .collect(),
        )
    };
    let mut c = MemoryCatalog::new();
    c.add_table(
        "hits",
        fields(&[
            ("WatchID", Int64),
            ("EventTime", Timestamp),
            ("EventDate", Date),
            ("CounterID", Int32),
            ("UserID", Int64),
            ("URL", Varchar),
            ("SearchPhrase", Varchar),
            ("IsRefresh", Int16),
        ]),
        vec![3, 2, 4],
    );
    c.add_table(
        "orders",
        fields(&[
            ("o_orderkey", Int64),
            ("o_custkey", Int64),
            ("o_orderdate", Date),
        ]),
        vec![0],
    );
    c.add_table(
        "lineitem",
        fields(&[
            ("l_orderkey", Int64),
            ("l_quantity", Int64),
            ("l_shipdate", Date),
        ]),
        vec![0],
    );
    c.add_parquet(
        "/data/hits.parquet",
        fields(&[("UserID", Int64), ("URL", Varchar), ("CounterID", Int32)]),
    );
    c
}

fn bind(sql: &str) -> LogicalPlan {
    let query = match parse_statement(sql).unwrap() {
        Statement::Query(q) => q,
        other => panic!("not a query: {other:?}"),
    };
    bind_query(&catalog(), &query).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn optimized(sql: &str) -> LogicalPlan {
    optimize(bind(sql), OptimizerOptions::default(), &catalog())
}

fn tree(plan: &LogicalPlan) -> String {
    plan.display_tree()
}

/// The first scan found top-down.
fn first_scan(plan: &LogicalPlan) -> Option<&LogicalPlan> {
    match plan {
        LogicalPlan::Scan { .. } | LogicalPlan::ParquetScan { .. } => Some(plan),
        other => other.children().into_iter().find_map(first_scan),
    }
}

fn scan_filters(plan: &LogicalPlan) -> Vec<ScanFilter> {
    match first_scan(plan) {
        Some(LogicalPlan::Scan { filters, .. } | LogicalPlan::ParquetScan { filters, .. }) => {
            filters.clone()
        }
        _ => panic!("no scan in\n{}", tree(plan)),
    }
}

#[test]
fn constant_folding_before_and_after() {
    let before = bind("SELECT UserID FROM hits WHERE CounterID = 60 + 2 AND (1 = 1 OR URL = 'x')");
    assert_eq!(
        tree(&before),
        "Project hits.UserID\n\
         \x20 Filter ((CAST(hits.CounterID AS BIGINT) = (60 + 2)) AND ((1 = 1) OR (hits.URL = 'x')))\n\
         \x20   Scan hits AS hits\n"
    );
    let after = fold_constants(before);
    assert_eq!(
        tree(&after),
        "Project hits.UserID\n\
         \x20 Filter (CAST(hits.CounterID AS BIGINT) = 62)\n\
         \x20   Scan hits AS hits\n"
    );
    // A filter that folds to TRUE disappears; one that folds to FALSE stays.
    let gone = fold_constants(bind("SELECT UserID FROM hits WHERE 2 > 1"));
    assert_eq!(tree(&gone), "Project hits.UserID\n  Scan hits AS hits\n");
    let never = fold_constants(bind("SELECT UserID FROM hits WHERE 1 > 2 AND URL = 'x'"));
    assert_eq!(
        tree(&never),
        "Project hits.UserID\n  Filter FALSE\n    Scan hits AS hits\n"
    );
}

#[test]
fn folding_simplifies_booleans_and_keeps_runtime_errors() {
    let fold = |e: BoundExpr| {
        let mut e = e;
        fold_expr(&mut e);
        e
    };
    let t = || BoundExpr::literal(Value::Boolean(true));
    let f = || BoundExpr::literal(Value::Boolean(false));
    let null = || BoundExpr::typed_literal(Value::Null, DataType::Boolean);
    let x = || BoundExpr::column(0, "t.x", DataType::Boolean, true);
    let bin = |op, l: BoundExpr, r: BoundExpr, dt| {
        BoundExpr::new(
            BoundExprKind::Binary {
                op,
                left: Box::new(l),
                right: Box::new(r),
            },
            dt,
            true,
        )
    };
    assert_eq!(fold(bin(BinaryOp::And, x(), t(), DataType::Boolean)), x());
    assert_eq!(
        fold(bin(BinaryOp::And, null(), f(), DataType::Boolean)).as_literal(),
        Some(&Value::Boolean(false))
    );
    assert_eq!(fold(bin(BinaryOp::Or, f(), x(), DataType::Boolean)), x());
    assert_eq!(
        fold(bin(BinaryOp::Or, x(), t(), DataType::Boolean)).as_literal(),
        Some(&Value::Boolean(true))
    );
    // NULL AND x stays: it is FALSE or NULL depending on x.
    assert!(
        fold(bin(BinaryOp::And, null(), x(), DataType::Boolean))
            .as_literal()
            .is_none()
    );
    let not_null = BoundExpr::new(
        BoundExprKind::Unary {
            op: UnaryOp::Not,
            expr: Box::new(null()),
        },
        DataType::Boolean,
        true,
    );
    assert_eq!(fold(not_null).as_literal(), Some(&Value::Null));
    // An overflowing constant is left for the executor to report.
    let big = bin(
        BinaryOp::Mul,
        BoundExpr::literal(Value::Int64(i64::MAX)),
        BoundExpr::literal(Value::Int64(2)),
        DataType::Int64,
    );
    assert!(matches!(fold(big).kind, BoundExprKind::Binary { .. }));
    let sum = bin(
        BinaryOp::Add,
        BoundExpr::literal(Value::Int32(40)),
        BoundExpr::literal(Value::Int32(2)),
        DataType::Int32,
    );
    assert_eq!(fold(sum).as_literal(), Some(&Value::Int32(42)));
    // NULL compared with anything is NULL.
    let cmp = bin(
        BinaryOp::Lt,
        BoundExpr::typed_literal(Value::Null, DataType::Int64),
        BoundExpr::literal(Value::Int64(1)),
        DataType::Boolean,
    );
    assert_eq!(fold(cmp).as_literal(), Some(&Value::Null));
}

#[test]
fn pushdown_through_project_and_inner_join_to_both_sides() {
    let sql = "SELECT * FROM (SELECT o_orderkey AS k, o_custkey AS c FROM orders) AS o, lineitem \
               WHERE o.k = l_orderkey AND o.c = 7 AND l_quantity > 3";
    let before = bind(sql);
    assert_eq!(
        tree(&before),
        "Project o.k, o.c, lineitem.l_orderkey, lineitem.l_quantity, lineitem.l_shipdate\n\
         \x20 Filter ((o.c = 7) AND (lineitem.l_quantity > 3))\n\
         \x20   Join inner keys=[o.k = lineitem.l_orderkey]\n\
         \x20     Project orders.o_orderkey, orders.o_custkey\n\
         \x20       Scan orders AS orders\n\
         \x20     Scan lineitem AS lineitem\n"
    );
    let after = push_down_predicates(before);
    assert_eq!(
        tree(&after),
        "Project o.k, o.c, lineitem.l_orderkey, lineitem.l_quantity, lineitem.l_shipdate\n\
         \x20 Join inner keys=[o.k = lineitem.l_orderkey]\n\
         \x20   Project orders.o_orderkey, orders.o_custkey\n\
         \x20     Filter (orders.o_custkey = 7)\n\
         \x20       Scan orders AS orders\n\
         \x20   Filter (lineitem.l_quantity > 3)\n\
         \x20     Scan lineitem AS lineitem\n"
    );
}

#[test]
fn pushdown_turns_cross_equalities_into_keys() {
    // The equality reaches the cross join from a Filter above a subquery.
    let sql = "SELECT * FROM (SELECT * FROM orders, lineitem) AS x WHERE o_orderkey = l_orderkey";
    let after = push_down_predicates(bind(sql));
    let text = tree(&after);
    assert!(
        text.contains("Join inner keys=[orders.o_orderkey = lineitem.l_orderkey]"),
        "{text}"
    );
    assert!(!text.contains("Filter"), "{text}");
}

#[test]
fn pushdown_never_enters_the_null_side_of_a_left_join() {
    let sql = "SELECT * FROM orders LEFT JOIN lineitem ON o_orderkey = l_orderkey \
               WHERE l_quantity > 3 AND o_custkey = 7";
    let before = bind(sql);
    let project = "Project orders.o_orderkey, orders.o_custkey, orders.o_orderdate, \
                   lineitem.l_orderkey, lineitem.l_quantity, lineitem.l_shipdate\n";
    assert_eq!(
        tree(&before),
        format!(
            "{project}\
             \x20 Filter ((lineitem.l_quantity > 3) AND (orders.o_custkey = 7))\n\
             \x20   Join left keys=[orders.o_orderkey = lineitem.l_orderkey]\n\
             \x20     Scan orders AS orders\n\
             \x20     Scan lineitem AS lineitem\n"
        )
    );
    // The condition on the preserved side goes down; the one on the NULL side stays above.
    let after = push_down_predicates(before);
    assert_eq!(
        tree(&after),
        format!(
            "{project}\
             \x20 Filter (lineitem.l_quantity > 3)\n\
             \x20   Join left keys=[orders.o_orderkey = lineitem.l_orderkey]\n\
             \x20     Filter (orders.o_custkey = 7)\n\
             \x20       Scan orders AS orders\n\
             \x20     Scan lineitem AS lineitem\n"
        )
    );
}

#[test]
fn an_is_null_filter_on_the_null_side_of_a_left_join_stays_above_it() {
    // Pushed into lineitem it would keep only stored NULLs instead of the unmatched orders.
    let sql = "SELECT o_orderkey FROM orders LEFT JOIN lineitem ON o_orderkey = l_orderkey \
               WHERE l_orderkey IS NULL";
    let text = tree(&push_down_predicates(bind(sql)));
    assert!(
        text.contains("Filter (lineitem.l_orderkey IS NULL)\n    Join left"),
        "{text}"
    );
    assert!(text.contains("\n      Scan lineitem AS lineitem"), "{text}");
}

#[test]
fn pushdown_moves_right_only_on_conditions_of_a_left_join_into_the_right_side() {
    let sql = "SELECT o_orderkey, l_quantity FROM orders LEFT JOIN lineitem \
               ON o_orderkey = l_orderkey AND l_quantity > 3 AND o_custkey = 7";
    let after = push_down_predicates(bind(sql));
    let text = tree(&after);
    assert!(text.contains("residual=(orders.o_custkey = 7)"), "{text}");
    assert!(
        text.contains("Filter (lineitem.l_quantity > 3)\n      Scan lineitem AS lineitem"),
        "{text}"
    );
    assert!(!text.contains("Filter (orders.o_custkey = 7)"), "{text}");
}

#[test]
fn pushdown_stops_at_aggregates_and_limits() {
    let sql = "SELECT * FROM (SELECT CounterID, count(*) AS c FROM hits GROUP BY CounterID) AS a \
               WHERE a.c > 10";
    let after = push_down_predicates(bind(sql));
    let text = tree(&after);
    let lines: Vec<&str> = text.lines().collect();
    let filter = lines.iter().position(|l| l.contains("Filter")).unwrap();
    let aggregate = lines.iter().position(|l| l.contains("Aggregate")).unwrap();
    assert!(filter < aggregate, "{text}");

    let sql = "SELECT * FROM (SELECT UserID FROM hits LIMIT 5) AS l WHERE l.UserID = 1";
    let text = tree(&push_down_predicates(bind(sql)));
    let lines: Vec<&str> = text.lines().collect();
    let filter = lines.iter().position(|l| l.contains("Filter")).unwrap();
    let limit = lines.iter().position(|l| l.contains("Limit")).unwrap();
    assert!(filter < limit, "{text}");
}

#[test]
fn scan_filters_from_comparisons_between_and_in() {
    let plan = optimized(
        "SELECT URL FROM hits WHERE CounterID = 62 AND EventDate BETWEEN '2013-07-01' AND '2013-07-31' \
         AND 5 < UserID AND IsRefresh IN (0, 1) AND URL <> '' AND URL LIKE '%x%'",
    );
    let date = |text: &str| {
        Value::Date(veta_core::datetime::parse_date(text.as_bytes()).expect("a valid date"))
    };
    assert_eq!(
        scan_filters(&plan),
        vec![
            ScanFilter::equal(3, Value::Int32(62)),
            ScanFilter::Range {
                column: 2,
                lower: Bound::Included(date("2013-07-01")),
                upper: Bound::Unbounded,
            },
            ScanFilter::Range {
                column: 2,
                lower: Bound::Unbounded,
                upper: Bound::Included(date("2013-07-31")),
            },
            ScanFilter::Range {
                column: 4,
                lower: Bound::Excluded(Value::Int64(5)),
                upper: Bound::Unbounded,
            },
            ScanFilter::In {
                column: 7,
                values: vec![Value::Int16(0), Value::Int16(1)],
            },
        ]
    );
    // The full predicate is still evaluated above the scan.
    let text = tree(&plan);
    assert!(
        text.contains("Filter (((((((hits.CounterID = 62)"),
        "{text}"
    );
}

#[test]
fn scan_filters_on_parquet_and_none_without_the_optimizer() {
    let sql = "SELECT URL FROM read_parquet('/data/hits.parquet') AS p WHERE CounterID >= 10";
    assert_eq!(
        scan_filters(&optimized(sql)),
        vec![ScanFilter::Range {
            column: 2,
            lower: Bound::Included(Value::Int32(10)),
            upper: Bound::Unbounded,
        }]
    );
    let off = optimize(
        bind(sql),
        OptimizerOptions {
            enabled: false,
            ..Default::default()
        },
        &catalog(),
    );
    assert!(scan_filters(&off).is_empty());
}

#[test]
fn column_pruning_before_and_after() {
    let sql = "SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime LIMIT 10";
    let before = bind(sql);
    assert_eq!(
        tree(&before),
        "Project SearchPhrase\n\
         \x20 Limit 10 offset=0\n\
         \x20   Sort __order_1 ASC NULLS LAST\n\
         \x20     Project hits.SearchPhrase, hits.EventTime\n\
         \x20       Filter (hits.SearchPhrase <> '')\n\
         \x20         Scan hits AS hits\n"
    );
    let after = prune_columns(before.clone());
    assert_eq!(
        tree(&after),
        "Project SearchPhrase\n\
         \x20 Limit 10 offset=0\n\
         \x20   Sort __order_1 ASC NULLS LAST\n\
         \x20     Project hits.SearchPhrase, hits.EventTime\n\
         \x20       Filter (hits.SearchPhrase <> '')\n\
         \x20         Scan hits AS hits columns=[EventTime, SearchPhrase]\n"
    );
    assert_eq!(after.schema(), before.schema());
    let Some(LogicalPlan::Scan { columns, .. }) = first_scan(&after) else {
        panic!()
    };
    assert_eq!(columns, &vec![1, 6]);
    // The projection reads the scan output by its new positions.
    let mut node = &after;
    let exprs = loop {
        match node {
            LogicalPlan::Project { exprs, input, .. }
                if matches!(**input, LogicalPlan::Filter { .. }) =>
            {
                break exprs.clone();
            }
            other => node = other.children()[0],
        }
    };
    assert_eq!(exprs[0].columns(), vec![1]);
    assert_eq!(exprs[1].columns(), vec![0]);

    // Rule 5 also runs with the optimizer off.
    let off = optimize(
        bind(sql),
        OptimizerOptions {
            enabled: false,
            ..Default::default()
        },
        &catalog(),
    );
    assert_eq!(tree(&off), tree(&after));
}

#[test]
fn column_pruning_of_count_star_and_unused_aggregates() {
    let after = prune_columns(bind("SELECT count(*) FROM hits"));
    let Some(LogicalPlan::Scan { columns, .. }) = first_scan(&after) else {
        panic!()
    };
    // One column, the narrowest, so the scan still produces its rows.
    assert_eq!(columns, &vec![7]);

    let sql = "SELECT c FROM (SELECT CounterID, count(*) AS c, sum(UserID) AS s FROM hits \
               GROUP BY CounterID) AS a";
    let after = prune_columns(bind(sql));
    let text = tree(&after);
    assert!(
        text.contains("Aggregate groups=[hits.CounterID] aggregates=[count(*)]"),
        "{text}"
    );
    assert!(text.contains("columns=[CounterID]"), "{text}");
}

#[test]
fn column_pruning_through_joins() {
    let sql = "SELECT o_orderdate FROM orders, lineitem WHERE o_orderkey = l_orderkey";
    let after = prune_columns(bind(sql));
    let text = tree(&after);
    assert!(
        text.contains("Scan orders AS orders columns=[o_orderkey, o_orderdate]"),
        "{text}"
    );
    assert!(
        text.contains("Scan lineitem AS lineitem columns=[l_orderkey]"),
        "{text}"
    );
    assert!(
        text.contains("Join inner keys=[orders.o_orderkey = lineitem.l_orderkey]"),
        "{text}"
    );
    let LogicalPlan::Project { input, exprs, .. } = &after else {
        panic!("{text}")
    };
    assert_eq!(exprs[0].columns(), vec![1]);
    let LogicalPlan::Join { keys, schema, .. } = &**input else {
        panic!("{text}")
    };
    assert_eq!(schema.len(), 3);
    assert_eq!(keys[0].0.columns(), vec![0]);
    assert_eq!(keys[0].1.columns(), vec![0]);
}

#[test]
fn sort_under_limit_becomes_top_n() {
    let sql = "SELECT UserID, count(*) AS c FROM hits GROUP BY UserID ORDER BY c DESC LIMIT 10";
    let before = bind(sql);
    assert!(tree(&before).starts_with("Limit 10 offset=0\n  Sort "));
    let after = sort_limit_to_top_n(before);
    assert!(
        tree(&after).starts_with("TopN c DESC NULLS FIRST limit=10 offset=0\n  Project "),
        "{}",
        tree(&after)
    );
    // Only OFFSET: no TopN.
    let only_offset = sort_limit_to_top_n(bind("SELECT UserID FROM hits ORDER BY UserID OFFSET 5"));
    assert!(tree(&only_offset).starts_with("Limit offset=5\n  Sort "));
    // Without the optimizer the Sort and the Limit stay.
    let off = optimize(
        bind(sql),
        OptimizerOptions {
            enabled: false,
            ..Default::default()
        },
        &catalog(),
    );
    assert!(tree(&off).starts_with("Limit 10 offset=0\n  Sort "));
}

#[test]
fn full_pipeline_of_rules() {
    let plan = optimized(
        "SELECT SearchPhrase, count(*) AS c FROM hits WHERE CounterID = 60 + 2 AND 1 = 1 \
         GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10",
    );
    assert_eq!(
        tree(&plan),
        "TopN c DESC NULLS FIRST limit=10 offset=0\n\
         \x20 Project hits.SearchPhrase, count(*)\n\
         \x20   Aggregate groups=[hits.SearchPhrase] aggregates=[count(*)]\n\
         \x20     Filter (CAST(hits.CounterID AS BIGINT) = 62)\n\
         \x20       Scan hits AS hits columns=[CounterID, SearchPhrase] filters=[CounterID = 62]\n"
    );
}
