//! Plan shapes of the subquery patterns of TPC-H (contract §10.4) over a catalog with the
//! eight TPC-H tables, and binding of the 22 fetched queries when they are present.
//!
//! The patterns are written here in reduced form; the query texts themselves are
//! downloaded, not versioned (§19 decision 29).

use veta_core::{DataType, Field, Schema};

use crate::ast::Statement;
use crate::catalog::MemoryCatalog;
use crate::expr::BoundExpr;
use crate::parser::parse_statement;
use crate::plan::{JoinKind, LogicalPlan};

use super::bind_query;

fn dec(precision: u8, scale: u8) -> DataType {
    DataType::Decimal { precision, scale }
}

/// The TPC-H schema with integer keys as `BIGINT` and money as `DECIMAL(15,2)`.
fn tpch_catalog() -> MemoryCatalog {
    use DataType::*;
    let money = dec(15, 2);
    let fields = |cols: &[(&str, DataType)]| {
        Schema::new(
            cols.iter()
                .map(|(n, t)| Field::new(*n, *t, false))
                .collect(),
        )
    };
    let mut c = MemoryCatalog::new();
    c.add_table(
        "part",
        fields(&[
            ("p_partkey", Int64),
            ("p_name", Varchar),
            ("p_mfgr", Varchar),
            ("p_brand", Varchar),
            ("p_type", Varchar),
            ("p_size", Int32),
            ("p_container", Varchar),
            ("p_retailprice", money),
            ("p_comment", Varchar),
        ]),
        vec![0],
    );
    c.add_table(
        "supplier",
        fields(&[
            ("s_suppkey", Int64),
            ("s_name", Varchar),
            ("s_address", Varchar),
            ("s_nationkey", Int64),
            ("s_phone", Varchar),
            ("s_acctbal", money),
            ("s_comment", Varchar),
        ]),
        vec![0],
    );
    c.add_table(
        "partsupp",
        fields(&[
            ("ps_partkey", Int64),
            ("ps_suppkey", Int64),
            ("ps_availqty", Int32),
            ("ps_supplycost", money),
            ("ps_comment", Varchar),
        ]),
        vec![0, 1],
    );
    c.add_table(
        "customer",
        fields(&[
            ("c_custkey", Int64),
            ("c_name", Varchar),
            ("c_address", Varchar),
            ("c_nationkey", Int64),
            ("c_phone", Varchar),
            ("c_acctbal", money),
            ("c_mktsegment", Varchar),
            ("c_comment", Varchar),
        ]),
        vec![0],
    );
    c.add_table(
        "orders",
        fields(&[
            ("o_orderkey", Int64),
            ("o_custkey", Int64),
            ("o_orderstatus", Varchar),
            ("o_totalprice", money),
            ("o_orderdate", Date),
            ("o_orderpriority", Varchar),
            ("o_clerk", Varchar),
            ("o_shippriority", Int32),
            ("o_comment", Varchar),
        ]),
        vec![0],
    );
    c.add_table(
        "lineitem",
        fields(&[
            ("l_orderkey", Int64),
            ("l_partkey", Int64),
            ("l_suppkey", Int64),
            ("l_linenumber", Int32),
            ("l_quantity", money),
            ("l_extendedprice", money),
            ("l_discount", money),
            ("l_tax", money),
            ("l_returnflag", Varchar),
            ("l_linestatus", Varchar),
            ("l_shipdate", Date),
            ("l_commitdate", Date),
            ("l_receiptdate", Date),
            ("l_shipinstruct", Varchar),
            ("l_shipmode", Varchar),
            ("l_comment", Varchar),
        ]),
        vec![0, 3],
    );
    c.add_table(
        "nation",
        fields(&[
            ("n_nationkey", Int64),
            ("n_name", Varchar),
            ("n_regionkey", Int64),
            ("n_comment", Varchar),
        ]),
        vec![0],
    );
    c.add_table(
        "region",
        fields(&[
            ("r_regionkey", Int64),
            ("r_name", Varchar),
            ("r_comment", Varchar),
        ]),
        vec![0],
    );
    c
}

fn bind(sql: &str) -> veta_core::Result<LogicalPlan> {
    let query = match parse_statement(sql)? {
        Statement::Query(q) => q,
        other => panic!("not a query: {other:?}"),
    };
    bind_query(&tpch_catalog(), &query)
}

fn plan(sql: &str) -> LogicalPlan {
    bind(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
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

/// The lines of the plan tree without their indentation.
fn lines(plan: &LogicalPlan) -> Vec<String> {
    plan.display_tree()
        .lines()
        .map(|l| l.trim_start().to_string())
        .collect()
}

fn has_line(plan: &LogicalPlan, line: &str) -> bool {
    lines(plan).iter().any(|l| l == line)
}

#[track_caller]
fn assert_line(plan: &LogicalPlan, line: &str) {
    assert!(
        has_line(plan, line),
        "no line {line:?} in\n{}",
        plan.display_tree()
    );
}

/// Position of the first line equal to `line`.
fn line_index(plan: &LogicalPlan, line: &str) -> usize {
    lines(plan)
        .iter()
        .position(|l| l == line)
        .unwrap_or_else(|| panic!("no line {line:?} in\n{}", plan.display_tree()))
}

/// The first node, top-down and left to right, for which `f` holds.
fn find<'p>(plan: &'p LogicalPlan, f: &dyn Fn(&LogicalPlan) -> bool) -> Option<&'p LogicalPlan> {
    if f(plan) {
        return Some(plan);
    }
    plan.children().into_iter().find_map(|c| find(c, f))
}

fn join_of(plan: &LogicalPlan, kind: JoinKind) -> &LogicalPlan {
    find(
        plan,
        &|p| matches!(p, LogicalPlan::Join { kind: k, .. } if *k == kind),
    )
    .unwrap_or_else(|| panic!("no {} join in\n{}", kind.name(), plan.display_tree()))
}

fn key_text(keys: &[(BoundExpr, BoundExpr)]) -> Vec<String> {
    keys.iter()
        .map(|(l, r)| format!("{} = {}", l.to_sql(), r.to_sql()))
        .collect()
}

#[test]
fn q04_exists_becomes_a_semi_join_on_the_correlation() {
    let p = plan(
        "SELECT o_orderpriority, count(*) AS order_count FROM orders \
         WHERE o_orderdate >= DATE '1993-07-01' AND o_orderdate < DATE '1993-10-01' \
         AND EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey \
         AND l_commitdate < l_receiptdate) \
         GROUP BY o_orderpriority ORDER BY o_orderpriority",
    );
    assert_eq!(
        p.display_tree(),
        "Sort o_orderpriority ASC NULLS LAST\n\
         \x20 Project orders.o_orderpriority, count(*)\n\
         \x20   Aggregate groups=[orders.o_orderpriority] aggregates=[count(*)]\n\
         \x20     Join semi keys=[orders.o_orderkey = lineitem.l_orderkey]\n\
         \x20       Filter ((orders.o_orderdate >= CAST('1993-07-01' AS DATE)) AND \
         (orders.o_orderdate < CAST('1993-10-01' AS DATE)))\n\
         \x20         Scan orders AS orders\n\
         \x20       Filter (lineitem.l_commitdate < lineitem.l_receiptdate)\n\
         \x20         Scan lineitem AS lineitem\n"
    );
    // The semi join keeps only the left columns.
    let LogicalPlan::Join {
        schema, residual, ..
    } = join_of(&p, JoinKind::Semi)
    else {
        unreachable!()
    };
    assert_eq!(schema.len(), 9);
    assert!(residual.is_none());
}

#[test]
fn q21_exists_and_not_exists_keep_the_inequality_as_residual() {
    let p = plan(
        "SELECT s_name, count(*) AS numwait FROM supplier, lineitem l1, orders \
         WHERE s_suppkey = l1.l_suppkey AND o_orderkey = l1.l_orderkey \
         AND o_orderstatus = 'F' AND l1.l_receiptdate > l1.l_commitdate \
         AND EXISTS (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey \
         AND l2.l_suppkey <> l1.l_suppkey) \
         AND NOT EXISTS (SELECT * FROM lineitem l3 WHERE l3.l_orderkey = l1.l_orderkey \
         AND l3.l_suppkey <> l1.l_suppkey AND l3.l_receiptdate > l3.l_commitdate) \
         GROUP BY s_name ORDER BY numwait DESC, s_name LIMIT 100",
    );
    let semi = "Join semi keys=[l1.l_orderkey = l2.l_orderkey] \
                residual=(l2.l_suppkey <> l1.l_suppkey)";
    let anti = "Join anti keys=[l1.l_orderkey = l3.l_orderkey] \
                residual=(l3.l_suppkey <> l1.l_suppkey)";
    // NOT EXISTS is applied above EXISTS, in WHERE order.
    assert!(line_index(&p, anti) < line_index(&p, semi));
    // The uncorrelated conjunct of the subquery stays inside it.
    assert_line(&p, "Filter (l3.l_receiptdate > l3.l_commitdate)");

    let LogicalPlan::Join {
        left,
        right,
        residual,
        ..
    } = join_of(&p, JoinKind::Anti)
    else {
        unreachable!()
    };
    // supplier, l1 and orders in FROM order; the residual reads l1.l_suppkey on the left
    // and l3.l_suppkey on the right.
    let width = left.schema().len();
    assert_eq!(width, 7 + 16 + 9);
    let columns = residual.as_ref().unwrap().columns();
    assert_eq!(columns, vec![7 + 2, width + 2]);
    assert_eq!(right.schema().len(), 16);
}

#[test]
fn q16_not_in_becomes_a_null_aware_anti_join() {
    let p = plan(
        "SELECT p_brand, p_type, p_size, count(DISTINCT ps_suppkey) AS supplier_cnt \
         FROM partsupp, part WHERE p_partkey = ps_partkey AND p_brand <> 'Brand#45' \
         AND ps_suppkey NOT IN (SELECT s_suppkey FROM supplier \
         WHERE s_comment LIKE '%Customer%Complaints%') \
         GROUP BY p_brand, p_type, p_size",
    );
    assert_line(
        &p,
        "Join null_aware_anti keys=[partsupp.ps_suppkey = supplier.s_suppkey]",
    );
    let LogicalPlan::Join { right, .. } = join_of(&p, JoinKind::NullAwareAnti) else {
        unreachable!()
    };
    assert_eq!(right.op(), "Project");
    assert_eq!(right.schema().len(), 1);

    // NOT (x IN …) is the same; IN without NOT is a semi join.
    let p = plan(
        "SELECT ps_partkey FROM partsupp WHERE NOT (ps_suppkey IN (SELECT s_suppkey FROM supplier))",
    );
    assert_line(
        &p,
        "Join null_aware_anti keys=[partsupp.ps_suppkey = supplier.s_suppkey]",
    );
    let p = plan(
        "SELECT ps_partkey FROM partsupp WHERE ps_suppkey IN (SELECT s_suppkey FROM supplier)",
    );
    assert_line(
        &p,
        "Join semi keys=[partsupp.ps_suppkey = supplier.s_suppkey]",
    );
}

#[test]
fn q18_in_with_group_by_and_having() {
    let p = plan(
        "SELECT c_name, c_custkey, o_orderkey, sum(l_quantity) FROM customer, orders, lineitem \
         WHERE o_orderkey IN (SELECT l_orderkey FROM lineitem GROUP BY l_orderkey \
         HAVING sum(l_quantity) > 300) \
         AND c_custkey = o_custkey AND o_orderkey = l_orderkey \
         GROUP BY c_name, c_custkey, o_orderkey ORDER BY o_orderkey LIMIT 100",
    );
    assert_line(
        &p,
        "Join semi keys=[orders.o_orderkey = lineitem.l_orderkey]",
    );
    let LogicalPlan::Join { left, right, .. } = join_of(&p, JoinKind::Semi) else {
        unreachable!()
    };
    // Left: the outer join tree under its filters; right: the grouped subquery.
    assert!(
        find(left, &|n| matches!(
            n,
            LogicalPlan::Join {
                kind: JoinKind::Inner,
                ..
            }
        ))
        .is_some()
    );
    assert!(find(right, &|n| n.op() == "Aggregate").is_some());
    assert_eq!(right.schema().len(), 1);
}

#[test]
fn q20_nested_in_with_a_correlated_aggregate() {
    let p = plan(
        "SELECT s_name, s_address FROM supplier, nation \
         WHERE s_suppkey IN (SELECT ps_suppkey FROM partsupp \
         WHERE ps_partkey IN (SELECT p_partkey FROM part WHERE p_name LIKE 'forest%') \
         AND ps_availqty > (SELECT 0.5 * sum(l_quantity) FROM lineitem \
         WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey \
         AND l_shipdate >= DATE '1994-01-01' AND l_shipdate < DATE '1995-01-01')) \
         AND s_nationkey = n_nationkey AND n_name = 'CANADA' ORDER BY s_name",
    );
    assert_line(
        &p,
        "Join semi keys=[supplier.s_suppkey = partsupp.ps_suppkey]",
    );
    assert_line(&p, "Join semi keys=[partsupp.ps_partkey = part.p_partkey]");
    assert_line(
        &p,
        "Join single keys=[partsupp.ps_partkey = lineitem.l_partkey, \
         partsupp.ps_suppkey = lineitem.l_suppkey]",
    );
    assert_line(
        &p,
        "Aggregate groups=[lineitem.l_partkey, lineitem.l_suppkey] \
         aggregates=[sum(lineitem.l_quantity)]",
    );
    assert_line(
        &p,
        "Filter ((lineitem.l_shipdate >= CAST('1994-01-01' AS DATE)) AND \
         (lineitem.l_shipdate < CAST('1995-01-01' AS DATE)))",
    );
    // The single join is inside the right input of the outer semi join.
    let LogicalPlan::Join { right, .. } = join_of(&p, JoinKind::Semi) else {
        unreachable!()
    };
    let single = join_of(right, JoinKind::Single);
    let LogicalPlan::Join {
        right: grouped,
        schema,
        ..
    } = single
    else {
        unreachable!()
    };
    assert_eq!(grouped.op(), "Aggregate");
    // partsupp, then the two group columns and the sum.
    assert_eq!(schema.len(), 5 + 3);
    assert!(schema.fields[5..].iter().all(|f| f.nullable));
}

#[test]
fn q02_and_q17_correlated_scalar_aggregates() {
    let p = plan(
        "SELECT s_acctbal, s_name, n_name, p_partkey FROM part, supplier, partsupp, nation, region \
         WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND p_size = 15 \
         AND p_type LIKE '%BRASS' AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey \
         AND r_name = 'EUROPE' AND ps_supplycost = (SELECT min(ps_supplycost) \
         FROM partsupp, supplier, nation, region WHERE p_partkey = ps_partkey \
         AND s_suppkey = ps_suppkey AND s_nationkey = n_nationkey \
         AND n_regionkey = r_regionkey AND r_name = 'EUROPE') \
         ORDER BY s_acctbal DESC, n_name, s_name, p_partkey LIMIT 100",
    );
    assert_line(
        &p,
        "Join single keys=[part.p_partkey = partsupp.ps_partkey]",
    );
    assert_line(
        &p,
        "Aggregate groups=[partsupp.ps_partkey] aggregates=[min(partsupp.ps_supplycost)]",
    );
    assert_line(&p, "Filter (partsupp.ps_supplycost = \"$subquery1\")");
    // The value column is dropped right after the filter that uses it.
    let filter = find(&p, &|n| {
        matches!(n, LogicalPlan::Filter { predicate, .. } if predicate.to_sql().contains("$subquery1"))
    })
    .unwrap();
    let outer_width = 9 + 7 + 5 + 4 + 3;
    assert_eq!(filter.schema().len(), outer_width + 1);
    let above = find(&p, &|n| {
        matches!(n, LogicalPlan::Project { input, .. } if input.op() == "Filter"
            && input.schema().len() == outer_width + 1)
    })
    .unwrap();
    assert_eq!(above.schema().len(), outer_width);

    let p = plan(
        "SELECT sum(l_extendedprice) / 7.0 AS avg_yearly FROM lineitem, part \
         WHERE p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container = 'MED BOX' \
         AND l_quantity < (SELECT 0.2 * avg(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)",
    );
    assert_line(&p, "Join single keys=[part.p_partkey = lineitem.l_partkey]");
    assert_line(
        &p,
        "Aggregate groups=[lineitem.l_partkey] aggregates=[avg(lineitem.l_quantity)]",
    );
    let LogicalPlan::Join {
        left, right, keys, ..
    } = join_of(&p, JoinKind::Single)
    else {
        unreachable!()
    };
    // The outer side is the joined FROM of the query; the key reads its p_partkey.
    assert_eq!(left.schema().len(), 16 + 9);
    assert_eq!(keys[0].0.columns(), vec![16]);
    assert_eq!(keys[0].1.columns(), vec![0]);
    assert_eq!(right.schema().len(), 2);
    assert_eq!(p.schema().data_types(), vec![DataType::Float64]);
}

#[test]
fn correlated_count_without_matches_is_zero() {
    let p = plan(
        "SELECT c_name, (SELECT count(*) FROM orders WHERE o_custkey = c_custkey) AS n \
         FROM customer",
    );
    assert_line(
        &p,
        "Join single keys=[customer.c_custkey = orders.o_custkey]",
    );
    let value = find(
        &p,
        &|n| matches!(n, LogicalPlan::Project { input, .. } if input.op() == "Join"),
    )
    .unwrap();
    let LogicalPlan::Project { exprs, .. } = value else {
        unreachable!()
    };
    assert_eq!(
        exprs.last().unwrap().to_sql(),
        "(CASE WHEN (count(*) IS NULL) THEN 0 ELSE count(*) END)"
    );
    assert!(!exprs.last().unwrap().nullable);
    assert_eq!(
        p.schema().data_types(),
        vec![DataType::Varchar, DataType::Int64]
    );

    // Other aggregates stay NULL; an expression over COUNT sees 0.
    let p = plan(
        "SELECT c_name FROM customer \
         WHERE (SELECT count(*) + 1 FROM orders WHERE o_custkey = c_custkey) > 1",
    );
    assert!(
        p.display_tree()
            .contains("((CASE WHEN (count(*) IS NULL) THEN 0 ELSE count(*) END) + 1)"),
        "{}",
        p.display_tree()
    );
}

#[test]
fn q22_uncorrelated_scalar_and_not_exists_in_a_from_subquery() {
    let p = plan(
        "SELECT cntrycode, count(*) AS numcust, sum(c_acctbal) AS totacctbal FROM ( \
         SELECT substring(c_phone FROM 1 FOR 2) AS cntrycode, c_acctbal FROM customer \
         WHERE substring(c_phone FROM 1 FOR 2) IN ('13', '31', '23') \
         AND c_acctbal > (SELECT avg(c_acctbal) FROM customer WHERE c_acctbal > 0.00 \
         AND substring(c_phone FROM 1 FOR 2) IN ('13', '31', '23')) \
         AND NOT EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey)) AS custsale \
         GROUP BY cntrycode ORDER BY cntrycode",
    );
    let anti = "Join anti keys=[customer.c_custkey = orders.o_custkey]";
    assert!(line_index(&p, anti) < line_index(&p, "Join single"));
    let LogicalPlan::Join {
        keys,
        residual,
        right,
        ..
    } = join_of(&p, JoinKind::Single)
    else {
        unreachable!()
    };
    // Evaluated once: no keys, one column.
    assert!(keys.is_empty() && residual.is_none());
    assert_eq!(right.schema().len(), 1);
    assert_eq!(right.schema().field(0).data_type, DataType::Float64);
}

#[test]
fn q11_scalar_subquery_in_having_joins_above_the_aggregate() {
    let p = plan(
        "SELECT ps_partkey, sum(ps_supplycost * ps_availqty) AS total FROM partsupp, supplier, \
         nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY' \
         GROUP BY ps_partkey HAVING sum(ps_supplycost * ps_availqty) > ( \
         SELECT sum(ps_supplycost * ps_availqty) * 0.0001000000 FROM partsupp, supplier, nation \
         WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY') \
         ORDER BY total DESC",
    );
    let having = find(
        &p,
        &|n| matches!(n, LogicalPlan::Filter { input, .. } if input.op() == "Join"),
    )
    .unwrap();
    let LogicalPlan::Filter { input, predicate } = having else {
        unreachable!()
    };
    let LogicalPlan::Join {
        kind,
        left,
        keys,
        right,
        ..
    } = &**input
    else {
        unreachable!()
    };
    assert_eq!(*kind, JoinKind::Single);
    assert!(keys.is_empty());
    assert_eq!(left.op(), "Aggregate");
    assert_eq!(right.schema().len(), 1);
    // groups (1) + aggregates (1), then the subquery value.
    assert_eq!(predicate.columns(), vec![1, 2]);
    assert_eq!(p.schema().len(), 2);
}

#[test]
fn q15_cte_used_twice_with_a_scalar_subquery() {
    let p = plan(
        "WITH revenue AS (SELECT l_suppkey AS supplier_no, \
         sum(l_extendedprice * (1 - l_discount)) AS total_revenue FROM lineitem \
         WHERE l_shipdate >= DATE '1996-01-01' AND l_shipdate < DATE '1996-04-01' \
         GROUP BY supplier_no) \
         SELECT s_suppkey, s_name, s_address, s_phone, total_revenue FROM supplier, revenue \
         WHERE s_suppkey = supplier_no AND total_revenue = (SELECT max(total_revenue) FROM revenue) \
         ORDER BY s_suppkey",
    );
    let tree = p.display_tree();
    let grouped = lines(&p)
        .iter()
        .filter(|l| l.starts_with("Aggregate groups=[lineitem.l_suppkey]"))
        .count();
    assert_eq!(grouped, 2, "{tree}");
    assert_line(
        &p,
        "Join inner keys=[supplier.s_suppkey = revenue.supplier_no]",
    );
    assert_line(&p, "Join single");
    assert_line(&p, "Filter (revenue.total_revenue = \"$subquery1\")");
    assert_eq!(p.schema().len(), 5);
}

#[test]
fn q13_left_join_keeps_its_on_predicate_and_column_aliases() {
    let p = plan(
        "SELECT c_count, count(*) AS custdist FROM ( \
         SELECT c_custkey, count(o_orderkey) FROM customer LEFT OUTER JOIN orders \
         ON c_custkey = o_custkey AND o_comment NOT LIKE '%special%requests%' \
         GROUP BY c_custkey) AS c_orders (c_custkey, c_count) \
         GROUP BY c_count ORDER BY custdist DESC, c_count DESC",
    );
    assert_line(
        &p,
        "Join left keys=[customer.c_custkey = orders.o_custkey] \
         residual=(orders.o_comment NOT LIKE '%special%requests%')",
    );
    assert_line(
        &p,
        "Aggregate groups=[c_orders.c_count] aggregates=[count(*)]",
    );
    // Nothing of the ON clause became a filter.
    assert!(
        find(&p, &|n| n.op() == "Filter").is_none(),
        "{}",
        p.display_tree()
    );
    let names: Vec<String> = p.schema().fields.iter().map(|f| f.name.clone()).collect();
    assert_eq!(names, vec!["c_count", "custdist"]);
}

#[test]
fn uncorrelated_exists_and_select_list_scalars() {
    let p = plan("SELECT o_orderkey FROM orders WHERE NOT EXISTS (SELECT * FROM region)");
    let LogicalPlan::Join { keys, .. } = join_of(&p, JoinKind::Anti) else {
        unreachable!()
    };
    assert!(keys.is_empty());

    // Two scalar subqueries of the select list are two single joins, in order.
    let p = plan(
        "SELECT (SELECT max(o_totalprice) FROM orders) AS hi, \
         (SELECT min(o_totalprice) FROM orders) AS lo, c_name FROM customer",
    );
    let singles = lines(&p).iter().filter(|l| *l == "Join single").count();
    assert_eq!(singles, 2);
    assert_eq!(
        p.schema().data_types(),
        vec![dec(15, 2), dec(15, 2), DataType::Varchar]
    );
    assert!(p.schema().field(0).nullable);

    // In an aggregated query the scalar joins above the aggregate.
    let p = plan(
        "SELECT o_orderstatus, count(*) - (SELECT count(*) FROM region) FROM orders \
         GROUP BY o_orderstatus",
    );
    let LogicalPlan::Join { left, .. } = join_of(&p, JoinKind::Single) else {
        unreachable!()
    };
    assert_eq!(left.op(), "Aggregate");

    // An uncorrelated IN in HAVING is a semi join above the aggregate.
    let p = plan(
        "SELECT o_custkey, count(*) FROM orders GROUP BY o_custkey \
         HAVING o_custkey IN (SELECT c_custkey FROM customer WHERE c_mktsegment = 'BUILDING')",
    );
    let LogicalPlan::Join { left, keys, .. } = join_of(&p, JoinKind::Semi) else {
        unreachable!()
    };
    assert_eq!(left.op(), "Aggregate");
    assert_eq!(
        key_text(keys),
        vec!["orders.o_custkey = customer.c_custkey"]
    );
}

#[test]
fn inner_columns_shadow_outer_ones() {
    // `l_quantity` and `l_partkey` exist in both queries: the subquery's own win, so only
    // `p_partkey` correlates.
    let p = plan(
        "SELECT l_orderkey FROM lineitem, part WHERE p_partkey = l_partkey \
         AND l_quantity < (SELECT avg(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)",
    );
    let LogicalPlan::Join { keys, .. } = join_of(&p, JoinKind::Single) else {
        unreachable!()
    };
    assert_eq!(key_text(keys), vec!["part.p_partkey = lineitem.l_partkey"]);
}

#[test]
fn unsupported_correlations() {
    let cases = [
        // No equality with the outer query.
        (
            "SELECT o_orderkey FROM orders WHERE EXISTS \
             (SELECT * FROM lineitem WHERE l_orderkey > o_orderkey)",
            "unsupported: correlated subquery without an equality",
        ),
        (
            "SELECT o_orderkey FROM orders WHERE o_totalprice > \
             (SELECT max(l_extendedprice) FROM lineitem WHERE l_orderkey < o_orderkey)",
            "unsupported: correlated subquery: ",
        ),
        (
            "SELECT o_orderkey FROM orders WHERE o_totalprice > (SELECT max(l_extendedprice) \
             FROM lineitem WHERE l_orderkey = o_orderkey AND l_shipdate > o_orderdate)",
            "is not an equality between the subquery and the outer query",
        ),
        (
            "SELECT o_orderkey FROM orders WHERE o_totalprice > \
             (SELECT l_extendedprice FROM lineitem WHERE l_orderkey = o_orderkey)",
            "unsupported: correlated scalar subquery without an aggregate",
        ),
        (
            "SELECT o_orderkey FROM orders WHERE o_custkey NOT IN \
             (SELECT c_custkey FROM customer WHERE c_name = o_clerk)",
            "unsupported: correlated NOT IN subquery",
        ),
        (
            "SELECT o_orderkey FROM orders WHERE EXISTS (SELECT l_orderkey FROM lineitem \
             WHERE l_orderkey = o_orderkey GROUP BY l_orderkey)",
            "unsupported: correlated subquery with GROUP BY",
        ),
        // Two levels up.
        (
            "SELECT o_orderkey FROM orders WHERE EXISTS (SELECT * FROM lineitem \
             WHERE l_orderkey = o_orderkey AND EXISTS (SELECT * FROM part \
             WHERE p_partkey = l_partkey AND p_size = o_shippriority))",
            "unsupported: correlated subquery reference to o_shippriority",
        ),
        // Outside the WHERE of the subquery.
        (
            "SELECT o_orderkey FROM orders WHERE EXISTS (SELECT o_custkey FROM lineitem)",
            "unsupported: correlated subquery reference to o_custkey",
        ),
        // Correlated scalar in an aggregated select list.
        (
            "SELECT o_custkey, (SELECT count(*) FROM customer WHERE c_custkey = o_custkey) \
             FROM orders GROUP BY o_custkey",
            "unsupported: correlated subquery in the select list",
        ),
        // EXISTS that is not a conjunct.
        (
            "SELECT o_orderkey FROM orders WHERE o_orderkey = 1 OR EXISTS (SELECT * FROM region)",
            "unsupported: IN or EXISTS subquery in WHERE",
        ),
        (
            "SELECT EXISTS (SELECT * FROM region) FROM orders",
            "unsupported: IN or EXISTS subquery in the select list",
        ),
        (
            "SELECT count(*) FROM orders GROUP BY (SELECT max(r_regionkey) FROM region)",
            "unsupported: subquery in GROUP BY",
        ),
    ];
    for (sql, expected) in cases {
        let e = error(sql);
        assert!(e.contains(expected), "{sql}: {e}");
    }
    let e = error(
        "SELECT o_orderkey FROM orders WHERE o_custkey = (SELECT c_custkey, c_name FROM customer)",
    );
    assert!(e.contains("one column"), "{e}");
    let e = error("SELECT o_orderkey FROM orders WHERE o_custkey IN (SELECT * FROM customer)");
    assert!(e.contains("one column"), "{e}");
}

fn data_dir() -> std::path::PathBuf {
    match std::env::var_os("VETA_DATA") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.veta-data"),
    }
}

/// Binds the 22 fetched TPC-H queries; does nothing when they have not been fetched.
/// A bound plan has no subqueries left (they have no plan node), so binding is the check;
/// the queries with subqueries must also show their join kinds.
#[test]
fn fetched_tpch_queries_bind() {
    let dir = data_dir().join("tpch/queries");
    let paths: Vec<_> = (1..=22).map(|i| dir.join(format!("q{i:02}.sql"))).collect();
    if !paths.iter().all(|p| p.exists()) {
        return;
    }
    let expected: [(usize, &[JoinKind]); 10] = [
        (2, &[JoinKind::Single]),
        (4, &[JoinKind::Semi]),
        (11, &[JoinKind::Single]),
        (15, &[JoinKind::Single]),
        (16, &[JoinKind::NullAwareAnti]),
        (17, &[JoinKind::Single]),
        (18, &[JoinKind::Semi]),
        (20, &[JoinKind::Semi, JoinKind::Single]),
        (21, &[JoinKind::Semi, JoinKind::Anti]),
        (22, &[JoinKind::Single, JoinKind::Anti]),
    ];
    for (i, path) in paths.iter().enumerate() {
        let text = std::fs::read_to_string(path).unwrap();
        let p = plan(&text);
        if let Some((_, kinds)) = expected.iter().find(|(q, _)| *q == i + 1) {
            for &kind in kinds.iter() {
                join_of(&p, kind);
            }
        }
    }
    let q13 = plan(&std::fs::read_to_string(&paths[12]).unwrap());
    join_of(&q13, JoinKind::Left);
}
