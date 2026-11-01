use veta_core::datetime::date_from_ymd;
use veta_core::{DataType, Field, Schema, Value};

use crate::ast::Statement;
use crate::catalog::MemoryCatalog;
use crate::expr::BoundExprKind;
use crate::parser::parse_statement;
use crate::plan::{JoinKind, LogicalPlan};

use super::bind_query;

fn dec(precision: u8, scale: u8) -> DataType {
    DataType::Decimal { precision, scale }
}

/// `hits` reduced to the columns the ClickBench queries use, with the types of its
/// `create.sql`, plus three small TPC-H-like tables for joins.
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
            ("Title", Varchar),
            ("EventTime", Timestamp),
            ("EventDate", Date),
            ("CounterID", Int32),
            ("ClientIP", Int32),
            ("RegionID", Int32),
            ("UserID", Int64),
            ("URL", Varchar),
            ("Referer", Varchar),
            ("IsRefresh", Int16),
            ("ResolutionWidth", Int16),
            ("MobilePhone", Int16),
            ("MobilePhoneModel", Varchar),
            ("TraficSourceID", Int16),
            ("SearchEngineID", Int16),
            ("SearchPhrase", Varchar),
            ("AdvEngineID", Int16),
            ("WindowClientWidth", Int16),
            ("WindowClientHeight", Int16),
            ("IsLink", Int16),
            ("IsDownload", Int16),
            ("DontCountHits", Int16),
            ("RefererHash", Int64),
            ("URLHash", Int64),
        ]),
        vec![4, 3, 7, 2, 0],
    );
    c.add_table(
        "orders",
        fields(&[
            ("o_orderkey", Int64),
            ("o_custkey", Int64),
            ("o_orderdate", Date),
            ("o_totalprice", dec(15, 2)),
        ]),
        vec![0],
    );
    c.add_table(
        "lineitem",
        fields(&[
            ("l_orderkey", Int64),
            ("l_quantity", dec(15, 2)),
            ("l_extendedprice", dec(15, 2)),
            ("l_discount", dec(15, 2)),
            ("l_shipdate", Date),
        ]),
        vec![0],
    );
    c.add_table(
        "customer",
        fields(&[("c_custkey", Int64), ("c_name", Varchar)]),
        vec![0],
    );
    c.add_parquet(
        "/data/hits_0.parquet",
        fields(&[("UserID", Int64), ("URL", Varchar)]),
    );
    c
}

fn bind(sql: &str) -> veta_core::Result<LogicalPlan> {
    let query = match parse_statement(sql)? {
        Statement::Query(q) => q,
        other => panic!("not a query: {other:?}"),
    };
    bind_query(&catalog(), &query)
}

fn plan(sql: &str) -> LogicalPlan {
    bind(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn types(sql: &str) -> Vec<DataType> {
    plan(sql).schema().data_types()
}

fn error(sql: &str) -> String {
    match bind(sql) {
        Ok(p) => panic!("{sql} bound to\n{}", p.display_tree()),
        Err(e) => {
            assert_eq!(e.exit_code(), 1, "{sql}: {e}");
            e.to_string()
        }
    }
}

/// The predicate of the first `Filter` found top-down.
fn first_filter(plan: &LogicalPlan) -> Option<&crate::expr::BoundExpr> {
    match plan {
        LogicalPlan::Filter { predicate, .. } => Some(predicate),
        other => other.children().into_iter().find_map(first_filter),
    }
}

#[test]
fn aggregate_result_types() {
    assert_eq!(
        types("SELECT SUM(UserID), SUM(AdvEngineID), AVG(ResolutionWidth), COUNT(*) FROM hits"),
        vec![
            DataType::Int128,
            DataType::Int64,
            DataType::Float64,
            DataType::Int64
        ]
    );
    assert_eq!(
        types("SELECT SUM(l_extendedprice), AVG(l_quantity), MIN(l_shipdate) FROM lineitem"),
        vec![dec(38, 2), DataType::Float64, DataType::Date]
    );
    assert_eq!(
        types("SELECT COUNT(DISTINCT UserID), MAX(SearchPhrase) FROM hits"),
        vec![DataType::Int64, DataType::Varchar]
    );
}

#[test]
fn integer_promotion() {
    // ClickBench Q36.
    let p = plan(
        "SELECT ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3, COUNT(*) AS c FROM hits \
         GROUP BY ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3 ORDER BY c DESC LIMIT 10",
    );
    assert_eq!(
        p.schema().data_types(),
        vec![
            DataType::Int32,
            DataType::Int64,
            DataType::Int64,
            DataType::Int64,
            DataType::Int64
        ]
    );
    assert_eq!(p.schema().field(1).name, "ClientIP - 1");
    assert_eq!(
        types("SELECT ResolutionWidth + 1, -AdvEngineID, UserID % 7 FROM hits"),
        vec![DataType::Int64, DataType::Int64, DataType::Int64]
    );
    assert_eq!(
        types("SELECT ResolutionWidth / 2, SUM(UserID) + 1 FROM hits GROUP BY ResolutionWidth"),
        vec![DataType::Float64, DataType::Int128]
    );
}

#[test]
fn decimal_arithmetic() {
    // §4: subtraction keeps scale max(s1,s2) = 2 and the integer literal 1 is DECIMAL(1,0),
    // so `1 - l_discount` is DECIMAL(16,2); multiplication adds scales, 2 + 2 = 4.
    assert_eq!(
        types("SELECT 1 - l_discount FROM lineitem"),
        vec![dec(16, 2)]
    );
    assert_eq!(
        types("SELECT l_extendedprice * (1 - l_discount) FROM lineitem"),
        vec![dec(31, 4)]
    );
    assert_eq!(
        types("SELECT SUM(l_extendedprice * (1 - l_discount)) FROM lineitem"),
        vec![dec(38, 4)]
    );
    assert_eq!(
        types(
            "SELECT l_quantity + l_extendedprice, l_quantity / 2, l_quantity * 0.5 FROM lineitem"
        ),
        vec![dec(16, 2), DataType::Float64, dec(17, 3)]
    );
}

#[test]
fn select_alias_in_group_by_and_order_by() {
    // ClickBench Q43 written with aliases.
    let p = plan(
        "SELECT DATE_TRUNC('minute', EventTime) AS M, COUNT(*) AS PageViews FROM hits \
         WHERE CounterID = 62 GROUP BY M ORDER BY M LIMIT 10 OFFSET 1000",
    );
    assert_eq!(
        p.display_tree(),
        "Limit 10 offset=1000\n\
         \x20 Sort M ASC NULLS LAST\n\
         \x20   Project date_trunc('minute', hits.EventTime), count(*)\n\
         \x20     Aggregate groups=[date_trunc('minute', hits.EventTime)] aggregates=[count(*)]\n\
         \x20       Filter (hits.CounterID = 62)\n\
         \x20         Scan hits AS hits\n"
    );
    assert_eq!(
        p.schema().data_types(),
        vec![DataType::Timestamp, DataType::Int64]
    );
    assert_eq!(p.schema().field(0).name, "M");

    let p = plan(
        "SELECT SearchPhrase, COUNT(*) AS c FROM hits GROUP BY SearchPhrase \
         ORDER BY c DESC, SearchPhrase",
    );
    let LogicalPlan::Sort { keys, .. } = &p else {
        panic!("{}", p.display_tree())
    };
    let indices: Vec<_> = keys.iter().map(|k| k.expr.columns()).collect();
    assert_eq!(indices, vec![vec![1], vec![0]]);
    assert!(keys[0].desc && keys[0].nulls_first);
    assert!(!keys[1].desc && !keys[1].nulls_first);

    // HAVING sees aliases too.
    let p = plan("SELECT CounterID, COUNT(*) AS c FROM hits GROUP BY CounterID HAVING c > 100000");
    assert_eq!(first_filter(&p).unwrap().to_sql(), "(count(*) > 100000)");
}

#[test]
fn group_by_ordinal_with_constant_select() {
    // ClickBench Q35.
    let p = plan("SELECT 1, URL, COUNT(*) AS c FROM hits GROUP BY 1, URL ORDER BY c DESC LIMIT 10");
    let mut node = &p;
    let (groups, project) = loop {
        match node {
            LogicalPlan::Project { exprs, input, .. } => {
                if let LogicalPlan::Aggregate { groups, .. } = &**input {
                    break (groups.clone(), exprs.clone());
                }
                node = &**input;
            }
            other => node = other.children()[0],
        }
    };
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].as_literal(), Some(&Value::Int64(1)));
    assert!(matches!(
        project[0].kind,
        BoundExprKind::Column { index: 0, .. }
    ));
    assert!(matches!(
        project[2].kind,
        BoundExprKind::AggregateRef { index: 2, .. }
    ));
    assert_eq!(
        p.schema().data_types(),
        vec![DataType::Int64, DataType::Varchar, DataType::Int64]
    );
    assert!(error("SELECT URL FROM hits GROUP BY 2").contains("position 2"));
}

#[test]
fn order_by_unselected_column_is_hidden_and_dropped() {
    // ClickBench Q25.
    let p =
        plan("SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime LIMIT 10");
    assert_eq!(
        p.display_tree(),
        "Project SearchPhrase\n\
         \x20 Limit 10 offset=0\n\
         \x20   Sort __order_1 ASC NULLS LAST\n\
         \x20     Project hits.SearchPhrase, hits.EventTime\n\
         \x20       Filter (hits.SearchPhrase <> '')\n\
         \x20         Scan hits AS hits\n"
    );
    assert_eq!(p.schema().len(), 1);
    assert_eq!(p.schema().field(0).name, "SearchPhrase");

    // An ORDER BY expression already selected is reused, not hidden.
    let p = plan(
        "SELECT UserID, extract(minute FROM EventTime) AS m, SearchPhrase, COUNT(*) FROM hits \
         GROUP BY UserID, m, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10",
    );
    assert_eq!(p.schema().len(), 4);
    assert!(matches!(p, LogicalPlan::Limit { .. }));
    assert_eq!(p.schema().field(1).data_type, DataType::Int64);

    assert!(
        error("SELECT DISTINCT SearchPhrase FROM hits ORDER BY EventTime").contains("DISTINCT")
    );
}

#[test]
fn date_compared_with_string() {
    let p = plan(
        "SELECT COUNT(*) FROM hits WHERE EventDate >= '2013-07-01' AND EventDate <= '2013-07-31'",
    );
    let predicate = first_filter(&p).unwrap();
    assert_eq!(
        predicate.to_sql(),
        "((hits.EventDate >= CAST('2013-07-01' AS DATE)) AND \
         (hits.EventDate <= CAST('2013-07-31' AS DATE)))"
    );
    let BoundExprKind::Binary { left, .. } = &predicate.kind else {
        panic!()
    };
    let BoundExprKind::Binary { right, .. } = &left.kind else {
        panic!()
    };
    assert_eq!(right.data_type, DataType::Date);
    assert_eq!(
        right.as_literal(),
        Some(&Value::Date(date_from_ymd(2013, 7, 1).unwrap()))
    );

    // A timestamp string widens the date side to TIMESTAMP.
    let p = plan("SELECT COUNT(*) FROM hits WHERE EventDate < '2013-07-01 10:00:00'");
    assert_eq!(
        first_filter(&p).unwrap().to_sql(),
        "(CAST(hits.EventDate AS TIMESTAMP) < CAST('2013-07-01 10:00:00' AS TIMESTAMP))"
    );
    // Integer constants take the column's type when they fit.
    let p = plan("SELECT COUNT(*) FROM hits WHERE CounterID = 62 AND IsRefresh IN (0, 1)");
    let conjuncts = first_filter(&p).unwrap().clone().into_conjuncts();
    let BoundExprKind::Binary { right, .. } = &conjuncts[0].kind else {
        panic!()
    };
    assert_eq!(right.data_type, DataType::Int32);
    assert_eq!(conjuncts[1].to_sql(), "(hits.IsRefresh IN (0, 1))");
    assert!(error("SELECT COUNT(*) FROM hits WHERE EventDate = 'July'").contains("date"));
    assert!(error("SELECT COUNT(*) FROM hits WHERE URL = 1").contains("compare"));
}

#[test]
fn name_resolution_errors() {
    let e = error("SELECT o_orderkey FROM orders a, orders b");
    assert!(e.contains("ambiguous column reference: o_orderkey"), "{e}");
    let e = error("SELECT nope FROM hits");
    assert!(e.contains("column not found: nope"), "{e}");
    let e = error("SELECT h.URL FROM hits AS x");
    assert!(e.contains("column not found: h.URL"), "{e}");
    assert!(error("SELECT * FROM missing").contains("table not found"));
    assert!(error("SELECT 1 FROM orders, orders").contains("duplicate table alias"));
    assert!(error("SELECT URL, COUNT(*) FROM hits").contains("GROUP BY"));
    assert!(error("SELECT COUNT(*) FROM hits WHERE COUNT(*) > 1").contains("WHERE"));
    assert!(error("SELECT URL FROM hits GROUP BY COUNT(*)").contains("GROUP BY"));
    // Qualified names, table aliases and case-insensitive identifiers resolve.
    assert_eq!(
        types(
            "SELECT a.O_ORDERKEY, B.o_totalprice FROM orders a, orders b WHERE a.o_orderkey = b.o_orderkey"
        ),
        vec![DataType::Int64, dec(15, 2)]
    );
}

#[test]
fn comma_join_plan_shape() {
    let p = plan(
        "SELECT o_orderdate, SUM(l_extendedprice) FROM orders, lineitem \
         WHERE o_orderkey = l_orderkey AND o_orderdate < '1995-03-15' GROUP BY o_orderdate",
    );
    assert_eq!(
        p.display_tree(),
        "Project orders.o_orderdate, sum(lineitem.l_extendedprice)\n\
         \x20 Aggregate groups=[orders.o_orderdate] aggregates=[sum(lineitem.l_extendedprice)]\n\
         \x20   Filter (orders.o_orderdate < CAST('1995-03-15' AS DATE))\n\
         \x20     Join inner keys=[orders.o_orderkey = lineitem.l_orderkey]\n\
         \x20       Scan orders AS orders\n\
         \x20       Scan lineitem AS lineitem\n"
    );
    let LogicalPlan::Project { input, .. } = &p else {
        panic!()
    };
    let LogicalPlan::Aggregate { input, .. } = &**input else {
        panic!()
    };
    let LogicalPlan::Filter { input, .. } = &**input else {
        panic!()
    };
    let LogicalPlan::Join {
        keys, kind, schema, ..
    } = &**input
    else {
        panic!()
    };
    assert_eq!(*kind, JoinKind::Inner);
    assert_eq!(keys[0].0.columns(), vec![0]);
    assert_eq!(keys[0].1.columns(), vec![0]);
    assert_eq!(schema.len(), 9);
    assert_eq!(p.schema().data_types(), vec![DataType::Date, dec(38, 2)]);
}

#[test]
fn comma_join_follows_connected_syntactic_order() {
    let p = plan(
        "SELECT c_name, l_extendedprice FROM customer, lineitem, orders \
         WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey",
    );
    let tree = p.display_tree();
    let lines: Vec<&str> = tree.lines().collect();
    assert_eq!(
        lines[0],
        "Project customer.c_name, lineitem.l_extendedprice"
    );
    assert!(
        lines[1].starts_with("  Project customer.c_custkey, customer.c_name, lineitem.l_orderkey")
    );
    assert_eq!(
        lines[2],
        "    Join inner keys=[orders.o_orderkey = lineitem.l_orderkey]"
    );
    assert_eq!(
        lines[3],
        "      Join inner keys=[customer.c_custkey = orders.o_custkey]"
    );
    assert_eq!(lines[4], "        Scan customer AS customer");
    assert_eq!(lines[5], "        Scan orders AS orders");
    assert_eq!(lines[6], "      Scan lineitem AS lineitem");
    assert_eq!(p.schema().data_types(), vec![DataType::Varchar, dec(15, 2)]);

    // No join predicate: a cross product.
    let p = plan("SELECT c_name FROM customer CROSS JOIN orders");
    assert!(p.display_tree().contains("Join cross"));
}

#[test]
fn explicit_joins() {
    let p = plan(
        "SELECT c_name, COUNT(o_orderkey) FROM customer LEFT JOIN orders \
         ON c_custkey = o_custkey AND o_totalprice > 100 GROUP BY c_name",
    );
    let tree = p.display_tree();
    assert!(
        tree.contains(
            "Join left keys=[customer.c_custkey = orders.o_custkey] \
             residual=(orders.o_totalprice > 100.00)"
        ),
        "{tree}"
    );
    let p = plan(
        "SELECT o_orderkey, l_quantity FROM orders INNER JOIN lineitem ON o_orderkey = l_orderkey \
         WHERE l_quantity > 10",
    );
    assert!(
        p.display_tree()
            .contains("Join inner keys=[orders.o_orderkey = lineitem.l_orderkey]")
    );
    // The right side of a LEFT JOIN becomes nullable.
    let p = plan("SELECT o_orderkey FROM customer LEFT JOIN orders ON c_custkey = o_custkey");
    assert!(p.schema().field(0).nullable);
}

#[test]
fn ctes_subqueries_and_parquet() {
    let p = plan(
        "WITH t AS (SELECT CounterID, COUNT(*) AS c FROM hits GROUP BY CounterID) \
         SELECT x.k, x.n FROM (SELECT CounterID, c FROM t) AS x (k, n) WHERE x.n > 10",
    );
    assert_eq!(
        p.schema().data_types(),
        vec![DataType::Int32, DataType::Int64]
    );
    let p = plan("SELECT COUNT(*), MIN(p.URL) FROM read_parquet('/data/hits_0.parquet') AS p");
    assert!(
        p.display_tree()
            .contains("ParquetScan '/data/hits_0.parquet' AS p")
    );
    let missing = bind("SELECT COUNT(*) FROM read_parquet('/nope.parquet')").unwrap_err();
    assert_eq!(missing.exit_code(), 2);
}

#[test]
fn subqueries_in_expressions_are_unsupported() {
    for sql in [
        "SELECT * FROM hits WHERE UserID IN (SELECT UserID FROM hits)",
        "SELECT * FROM hits WHERE EXISTS (SELECT 1 FROM orders)",
        "SELECT (SELECT COUNT(*) FROM orders) FROM hits",
    ] {
        assert_eq!(error(sql), "unsupported: subquery");
    }
}

#[test]
fn select_without_from_and_distinct() {
    let p = plan("SELECT 1 + 2 AS x, 'a' AS y");
    assert_eq!(p.display_tree(), "Project (1 + 2), 'a'\n  SingleRow\n");
    assert_eq!(
        p.schema().data_types(),
        vec![DataType::Int64, DataType::Varchar]
    );
    let p = plan("SELECT DISTINCT SearchPhrase FROM hits");
    let LogicalPlan::Aggregate {
        groups, aggregates, ..
    } = &p
    else {
        panic!("{}", p.display_tree())
    };
    assert_eq!(groups.len(), 1);
    assert!(aggregates.is_empty());
    assert_eq!(p.schema().field(0).name, "SearchPhrase");
    assert!(error("SELECT *").contains("FROM"));
}

#[test]
fn clickbench_shapes_bind() {
    for sql in [
        "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0",
        "SELECT MIN(EventDate), MAX(EventDate) FROM hits",
        "SELECT RegionID, SUM(AdvEngineID), COUNT(*) AS c, AVG(ResolutionWidth), \
         COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID ORDER BY c DESC LIMIT 10",
        "SELECT MobilePhone, MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits \
         WHERE MobilePhoneModel <> '' GROUP BY MobilePhone, MobilePhoneModel ORDER BY u DESC LIMIT 10",
        "SELECT CounterID, AVG(length(URL)) AS l, COUNT(*) AS c FROM hits WHERE URL <> '' \
         GROUP BY CounterID HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25",
        "SELECT REGEXP_REPLACE(Referer, '^https?://(?:www\\.)?([^/]+)/.*$', '\\1') AS k, \
         AVG(length(Referer)) AS l, COUNT(*) AS c, MIN(Referer) FROM hits WHERE Referer <> '' \
         GROUP BY k HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25",
        "SELECT SUM(ResolutionWidth), SUM(ResolutionWidth + 1), SUM(ResolutionWidth + 2) FROM hits",
        "SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 \
         AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND DontCountHits = 0 \
         AND IsRefresh = 0 AND URL <> '' GROUP BY URL ORDER BY PageViews DESC LIMIT 10",
        "SELECT TraficSourceID, SearchEngineID, AdvEngineID, CASE WHEN (SearchEngineID = 0 \
         AND AdvEngineID = 0) THEN Referer ELSE '' END AS Src, URL AS Dst, COUNT(*) AS PageViews \
         FROM hits WHERE CounterID = 62 AND IsRefresh = 0 GROUP BY TraficSourceID, \
         SearchEngineID, AdvEngineID, Src, Dst ORDER BY PageViews DESC LIMIT 10 OFFSET 1000",
        "SELECT URLHash, EventDate, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 \
         AND TraficSourceID IN (-1, 6) AND RefererHash = 3594120000172545465 \
         GROUP BY URLHash, EventDate ORDER BY PageViews DESC LIMIT 10 OFFSET 100",
        "SELECT SearchPhrase, MIN(URL), MIN(Title), COUNT(*) AS c, COUNT(DISTINCT UserID) FROM hits \
         WHERE Title LIKE '%Google%' AND URL NOT LIKE '%.google.%' AND SearchPhrase <> '' \
         GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10",
        "SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10",
        "SELECT UserID FROM hits WHERE UserID = 435090932899640449",
        "SELECT substring(URL, 1, 10), substring(URL FROM 2 FOR 3) FROM hits",
    ] {
        plan(sql);
    }
}
