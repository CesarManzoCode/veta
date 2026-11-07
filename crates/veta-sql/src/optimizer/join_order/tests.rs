use std::collections::HashSet;

use veta_core::stats::TableStats;
use veta_core::{DataType, Field, Schema, Value};

use super::*;
use crate::ast::Statement;
use crate::binder::bind_query;
use crate::cardinality::estimate;
use crate::catalog::MemoryCatalog;
use crate::optimizer::{JoinOrder, OptimizerOptions, optimize};
use crate::parser::parse_statement;

// ---------------------------------------------------------------------------------------
// The enumeration

fn graph(n: usize, edges: &[(usize, usize)]) -> Vec<u64> {
    let mut neighbors = vec![0u64; n];
    for &(a, b) in edges {
        neighbors[a] |= 1 << b;
        neighbors[b] |= 1 << a;
    }
    neighbors
}

fn chain(n: usize) -> Vec<(usize, usize)> {
    (1..n).map(|i| (i - 1, i)).collect()
}

fn star(n: usize) -> Vec<(usize, usize)> {
    (1..n).map(|i| (0, i)).collect()
}

fn cycle(n: usize) -> Vec<(usize, usize)> {
    let mut edges = chain(n);
    edges.push((n - 1, 0));
    edges
}

fn clique(n: usize) -> Vec<(usize, usize)> {
    let mut edges = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            edges.push((a, b));
        }
    }
    edges
}

/// The graph numbered breadth-first, as the optimizer numbers it.
fn numbered(n: usize, edges: &[(usize, usize)]) -> Vec<u64> {
    let order = breadth_first_order(&graph(n, edges));
    let mut label = vec![0; n];
    for (new, &old) in order.iter().enumerate() {
        label[old] = new;
    }
    let relabeled: Vec<(usize, usize)> = edges.iter().map(|&(a, b)| (label[a], label[b])).collect();
    graph(n, &relabeled)
}

/// Counts the csg-cmp pairs, checking each one and the order they come in.
fn count_pairs(neighbors: &[u64]) -> u64 {
    let mut seen: HashSet<u64> = (0..neighbors.len()).map(|i| 1u64 << i).collect();
    let mut pairs = HashSet::new();
    for_each_csg_cmp(neighbors, &mut |a, b| {
        assert_eq!(a & b, 0, "{a:b} and {b:b} overlap");
        assert_ne!(
            neighborhood(neighbors, a) & b,
            0,
            "{a:b} and {b:b} have no edge"
        );
        assert!(seen.contains(&a), "{a:b} comes before its plan exists");
        assert!(seen.contains(&b), "{b:b} comes before its plan exists");
        assert!(pairs.insert((a, b)), "{a:b} with {b:b} comes twice");
        assert!(!pairs.contains(&(b, a)), "{a:b} with {b:b} comes both ways");
        seen.insert(a | b);
    });
    pairs.len() as u64
}

#[test]
fn csg_cmp_pairs_of_a_chain() {
    // (n³ − n) / 6: the #ccp column of Figure 3 of the paper gives 20 and 165.
    for (n, pairs) in [(2, 1), (3, 4), (4, 10), (5, 20), (6, 35), (10, 165)] {
        assert_eq!(count_pairs(&numbered(n, &chain(n))), pairs, "chain of {n}");
    }
}

#[test]
fn csg_cmp_pairs_of_a_star() {
    // (n − 1)·2^(n − 2): Figure 3 gives 32 and 2304.
    for (n, pairs) in [(2, 1), (4, 12), (5, 32), (6, 80), (10, 2304)] {
        assert_eq!(count_pairs(&numbered(n, &star(n))), pairs, "star of {n}");
    }
}

#[test]
fn csg_cmp_pairs_of_a_cycle() {
    // (n³ − 2n² + n) / 2: Figure 3 gives 40 and 405.
    for (n, pairs) in [(4, 18), (5, 40), (6, 75), (10, 405)] {
        assert_eq!(count_pairs(&numbered(n, &cycle(n))), pairs, "cycle of {n}");
    }
}

#[test]
fn csg_cmp_pairs_of_a_clique() {
    // (3ⁿ − 2ⁿ⁺¹ + 1) / 2: Figure 3 gives 90 and 28501. A triangle needs `{0}` with `{1, 2}`,
    // which the excluded set printed in the paper would lose.
    for (n, pairs) in [(3, 6), (4, 25), (5, 90), (10, 28501)] {
        assert_eq!(
            count_pairs(&numbered(n, &clique(n))),
            pairs,
            "clique of {n}"
        );
    }
}

#[test]
fn disconnected_graphs_pair_only_inside_a_component() {
    let neighbors = numbered(5, &[(0, 3), (3, 4), (1, 2)]);
    assert_eq!(count_pairs(&neighbors), 4 + 1);
    assert_eq!(connected_components(&neighbors).len(), 2);
}

#[test]
fn numbering_is_breadth_first_and_covers_each_vertex() {
    // 0 − 3 − 1 − 4 and 2 alone.
    let neighbors = graph(5, &[(0, 3), (3, 1), (1, 4)]);
    assert_eq!(breadth_first_order(&neighbors), vec![0, 3, 1, 4, 2]);
    let square = graph(4, &[(0, 3), (0, 1), (3, 2), (1, 2)]);
    assert_eq!(breadth_first_order(&square), vec![0, 1, 3, 2]);
}

#[test]
fn subsets_come_after_their_own_subsets() {
    assert_eq!(subsets(0).count(), 0);
    assert_eq!(
        subsets(0b101).collect::<Vec<_>>(),
        vec![0b001, 0b100, 0b101]
    );
    assert_eq!(subsets(0b1011).count(), 7);
}

// ---------------------------------------------------------------------------------------
// Plans

/// A table with `rows`, its column names and statistics with its values.
fn add_table(catalog: &mut MemoryCatalog, name: &str, columns: &[&str], rows: &[Vec<i64>]) {
    let schema = Schema::new(
        columns
            .iter()
            .map(|c| Field::new(*c, DataType::Int64, false))
            .collect(),
    );
    let mut stats = TableStats::new(&schema.data_types(), 1);
    stats.rows = rows.len() as u64;
    for row in rows {
        for (column, value) in stats.columns.iter_mut().zip(row) {
            column.update(&Value::Int64(*value));
        }
    }
    catalog.add_table(name, schema, vec![0]);
    catalog.set_stats(name, stats);
}

/// `a(id, b_id)`, 1000 rows; `b(id, c_id)`, 100 rows; `c(id)`, 10 rows.
fn small_catalog() -> MemoryCatalog {
    let mut catalog = MemoryCatalog::new();
    let a: Vec<Vec<i64>> = (0..1000).map(|i| vec![i, i % 100]).collect();
    let b: Vec<Vec<i64>> = (0..100).map(|i| vec![i, i % 10]).collect();
    let c: Vec<Vec<i64>> = (0..10).map(|i| vec![i]).collect();
    add_table(&mut catalog, "a", &["id", "b_id"], &a);
    add_table(&mut catalog, "b", &["id", "c_id"], &b);
    add_table(&mut catalog, "c", &["id"], &c);
    catalog
}

fn bound(catalog: &MemoryCatalog, sql: &str) -> LogicalPlan {
    let Statement::Query(query) = parse_statement(sql).unwrap() else {
        panic!("not a query: {sql}");
    };
    bind_query(catalog, &query).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn optimized(catalog: &MemoryCatalog, sql: &str, join_order: JoinOrder) -> LogicalPlan {
    let options = OptimizerOptions {
        enabled: true,
        join_order,
    };
    optimize(bound(catalog, sql), options, catalog)
}

fn by_cost(catalog: &MemoryCatalog, sql: &str) -> LogicalPlan {
    optimized(catalog, sql, JoinOrder::Cost)
}

/// The joins and base relations of `plan`: `(left right)` for a join, the alias for a scan.
fn shape(plan: &LogicalPlan) -> String {
    match plan {
        LogicalPlan::Join { left, right, .. } => format!("({} {})", shape(left), shape(right)),
        LogicalPlan::Scan { alias, .. } | LogicalPlan::ParquetScan { alias, .. } => alias.clone(),
        other => other
            .children()
            .into_iter()
            .map(shape)
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// `C` of §11.2 over every join of `plan`, each estimated as its own subtree.
fn cost(plan: &LogicalPlan, catalog: &MemoryCatalog) -> f64 {
    let own = match plan {
        LogicalPlan::Join { left, right, .. } => {
            let (l, r) = (estimate(left, catalog), estimate(right, catalog));
            2.0 * l.min(r) + l.max(r) + estimate(plan, catalog)
        }
        _ => 0.0,
    };
    own + plan
        .children()
        .into_iter()
        .map(|c| cost(c, catalog))
        .sum::<f64>()
}

fn joins(plan: &LogicalPlan) -> Vec<&LogicalPlan> {
    let mut out = Vec::new();
    if matches!(plan, LogicalPlan::Join { .. }) {
        out.push(plan);
    }
    for child in plan.children() {
        out.extend(joins(child));
    }
    out
}

fn assert_close(got: f64, expected: f64) {
    assert!(
        (got - expected).abs() <= expected * 0.1,
        "{got} is not near {expected}"
    );
}

#[test]
fn the_optimal_plan_of_a_small_case() {
    // By hand, with NDV a.b_id = b.id = 100 and b.c_id = c.id = 10:
    //   |a ⋈ b| = 1000 · 100 / 100 = 1000, |b ⋈ c| = 100 · 10 / 10 = 100, all three = 1000.
    //   (a ⋈ b) ⋈ c: (2·100 + 1000 + 1000) + (2·10 + 1000 + 1000) = 4220.
    //   a ⋈ (b ⋈ c): (2·10 + 100 + 100) + (2·100 + 1000 + 1000) = 2420.
    let catalog = small_catalog();
    let sql = "SELECT * FROM a, b, c WHERE a.b_id = b.id AND b.c_id = c.id";
    let best = by_cost(&catalog, sql);
    assert_eq!(shape(&best), "(a (b c))", "{}", best.display_tree());
    assert_close(cost(&best, &catalog), 2420.0);
    let syntactic = optimized(&catalog, sql, JoinOrder::Syntactic);
    assert_eq!(shape(&syntactic), "((a b) c)");
    assert_close(cost(&syntactic, &catalog), 4220.0);
    assert_close(estimate(&best, &catalog), 1000.0);
}

#[test]
fn the_side_with_fewer_rows_builds() {
    let catalog = small_catalog();
    for sql in [
        "SELECT * FROM a, b WHERE a.b_id = b.id",
        "SELECT * FROM b, a WHERE a.b_id = b.id",
    ] {
        assert_eq!(shape(&by_cost(&catalog, sql)), "(a b)", "{sql}");
    }
    // A filter counts: two conjuncts without a sample leave 1 of the 100 rows of b, fewer than
    // the 10 rows of c.
    let filtered = by_cost(
        &catalog,
        "SELECT * FROM b, c WHERE c.id = b.c_id AND b.id IN (1, 2, 3) AND b.c_id < 5",
    );
    assert_eq!(shape(&filtered), "(c b)", "{}", filtered.display_tree());
}

#[test]
fn equal_estimates_keep_the_order_of_from() {
    let mut catalog = MemoryCatalog::new();
    let rows: Vec<Vec<i64>> = (0..50).map(|i| vec![i]).collect();
    add_table(&mut catalog, "x", &["id"], &rows);
    add_table(&mut catalog, "y", &["id"], &rows);
    let plan = by_cost(&catalog, "SELECT * FROM y, x WHERE x.id = y.id");
    assert_eq!(shape(&plan), "(y x)");
}

#[test]
fn the_output_keeps_its_columns_when_the_sides_swap() {
    let catalog = small_catalog();
    let sql = "SELECT * FROM c, a WHERE c.id = a.b_id";
    let plan = by_cost(&catalog, sql);
    assert_eq!(plan.schema(), bound(&catalog, sql).schema());
    assert_eq!(shape(&plan), "(a c)");
    let LogicalPlan::Project { exprs, .. } = &plan else {
        panic!("no Project on top:\n{}", plan.display_tree());
    };
    // a.id, a.b_id, c.id under the join; c.id, a.id, a.b_id above it.
    let columns: Vec<Vec<usize>> = exprs.iter().map(BoundExpr::columns).collect();
    assert_eq!(columns, vec![vec![2], vec![0], vec![1]]);
    let names: Vec<String> = exprs.iter().map(BoundExpr::to_sql).collect();
    assert_eq!(names, vec!["c.id", "a.id", "a.b_id"]);

    // A self join swapped keeps its order too, although the schemas look alike.
    let mut catalog = MemoryCatalog::new();
    let rows: Vec<Vec<i64>> = (0..50).map(|i| vec![i]).collect();
    add_table(&mut catalog, "t", &["id"], &rows);
    let plan = by_cost(
        &catalog,
        "SELECT * FROM t AS p, t AS q WHERE p.id = q.id AND p.id < 5",
    );
    assert_eq!(shape(&plan), "(q p)", "{}", plan.display_tree());
    let LogicalPlan::Project { exprs, .. } = &plan else {
        panic!("no Project on top:\n{}", plan.display_tree());
    };
    let columns: Vec<Vec<usize>> = exprs.iter().map(BoundExpr::columns).collect();
    assert_eq!(columns, vec![vec![1], vec![0]]);
}

/// `t0 … t(n-1)` with sizes that are not in `FROM` order, joined on `id`.
fn numbered_tables(n: usize) -> MemoryCatalog {
    let mut catalog = MemoryCatalog::new();
    for i in 0..n {
        let size = 10 + (i as i64 * 37) % 90;
        let rows: Vec<Vec<i64>> = (0..size).map(|v| vec![v]).collect();
        add_table(&mut catalog, &format!("t{i}"), &["id"], &rows);
    }
    catalog
}

fn query_over(n: usize, edges: &[(usize, usize)]) -> String {
    let from: Vec<String> = (0..n).map(|i| format!("t{i}")).collect();
    let predicates: Vec<String> = edges
        .iter()
        .map(|(a, b)| format!("t{a}.id = t{b}.id"))
        .collect();
    format!(
        "SELECT * FROM {} WHERE {}",
        from.join(", "),
        predicates.join(" AND ")
    )
}

fn assert_no_cross_product(plan: &LogicalPlan, relations: usize) {
    let all = joins(plan);
    assert_eq!(all.len(), relations - 1, "{}", plan.display_tree());
    for join in all {
        let LogicalPlan::Join { kind, keys, .. } = join else {
            unreachable!();
        };
        assert_eq!(*kind, JoinKind::Inner, "{}", plan.display_tree());
        assert!(!keys.is_empty(), "{}", plan.display_tree());
    }
}

#[test]
fn a_connected_graph_gets_no_cross_product() {
    for n in 4..=6 {
        let catalog = numbered_tables(n);
        for edges in [chain(n), star(n), cycle(n)] {
            assert_no_cross_product(&by_cost(&catalog, &query_over(n, &edges)), n);
        }
    }
    let catalog = numbered_tables(12);
    assert_no_cross_product(&by_cost(&catalog, &query_over(12, &chain(12))), 12);
}

#[test]
fn above_twelve_relations_the_greedy_order_has_no_cross_product_either() {
    let n = DPCCP_MAX_RELATIONS + 3;
    let catalog = numbered_tables(n);
    for edges in [chain(n), star(n), cycle(n)] {
        let sql = query_over(n, &edges);
        let plan = by_cost(&catalog, &sql);
        assert_no_cross_product(&plan, n);
        assert_eq!(plan.schema(), bound(&catalog, &sql).schema());
    }
}

#[test]
fn greedy_joins_the_fewest_rows_first() {
    // A chain of 100-row tables on one key, except t6 with 3 rows: any join with it has 3 rows
    // and any other has 100, so GOO starts at t6 and grows from there.
    let n = DPCCP_MAX_RELATIONS + 1;
    let mut catalog = MemoryCatalog::new();
    for i in 0..n {
        let size = if i == 6 { 3 } else { 100 };
        let rows: Vec<Vec<i64>> = (0..size).map(|v| vec![v]).collect();
        add_table(&mut catalog, &format!("t{i}"), &["id"], &rows);
    }
    let sql = query_over(n, &chain(n));
    let plan = by_cost(&catalog, &sql);
    let deepest = joins(&plan).pop().expect("a join");
    assert!(shape(deepest).contains("t6"), "{}", plan.display_tree());
    let syntactic = optimized(&catalog, &sql, JoinOrder::Syntactic);
    assert!(cost(&plan, &catalog) < cost(&syntactic, &catalog));
}

#[test]
fn only_without_any_predicate_is_there_a_cross_product() {
    let catalog = small_catalog();
    let plan = by_cost(&catalog, "SELECT * FROM c, a");
    assert_eq!(shape(&plan), "(a c)");
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("no Project on top:\n{}", plan.display_tree());
    };
    let LogicalPlan::Join { kind, keys, .. } = &**input else {
        panic!("no join:\n{}", plan.display_tree());
    };
    assert_eq!(*kind, JoinKind::Cross);
    assert!(keys.is_empty());
    // The connected relations join first and the other one comes last.
    let plan = by_cost(&catalog, "SELECT * FROM c, a, b WHERE a.b_id = b.id");
    assert_eq!(shape(&plan), "((a b) c)", "{}", plan.display_tree());
}

#[test]
fn predicates_that_are_not_keys_join_at_the_lowest_join_that_covers_them() {
    let catalog = small_catalog();
    let text = by_cost(
        &catalog,
        "SELECT * FROM a, b WHERE a.b_id = b.id AND a.id < b.c_id",
    )
    .display_tree();
    assert!(text.contains("keys=[a.b_id = b.id]"), "{text}");
    assert!(text.contains("residual=(a.id < b.c_id)"), "{text}");
    assert!(!text.contains("Filter"), "{text}");

    // Over two relations it is an edge: no cross product even without an equality.
    let plan = by_cost(&catalog, "SELECT * FROM a, c WHERE a.id < c.id");
    let text = plan.display_tree();
    assert!(text.contains("Join inner"), "{text}");
    assert!(text.contains("residual=(a.id < c.id)"), "{text}");

    // Over three relations it is no edge; it still ends at the join that covers them, here
    // as a key because each side reads one input.
    let text = by_cost(
        &catalog,
        "SELECT * FROM a, b, c WHERE a.b_id = b.id AND a.id + b.id = c.id",
    )
    .display_tree();
    assert!(text.contains("= c.id]"), "{text}");
    assert!(text.contains("a.id + b.id"), "{text}");
    assert!(!text.contains("Join cross"), "{text}");
    assert!(!text.contains("Filter"), "{text}");
}

#[test]
fn rule_4_still_sees_the_filters_of_a_reordered_relation() {
    let catalog = small_catalog();
    let plan = by_cost(
        &catalog,
        "SELECT * FROM b, a WHERE a.b_id = b.id AND b.id = 7",
    );
    assert_eq!(shape(&plan), "(a b)", "{}", plan.display_tree());
    let text = plan.display_tree();
    assert!(text.contains("filters=[id = 7]"), "{text}");
}

/// The `Left` join of `plan`.
fn left_join(plan: &LogicalPlan) -> (&LogicalPlan, &LogicalPlan) {
    for join in joins(plan) {
        if let LogicalPlan::Join {
            left,
            right,
            kind: JoinKind::Left,
            ..
        } = join
        {
            return (left, right);
        }
    }
    panic!("no left join:\n{}", plan.display_tree());
}

#[test]
fn a_left_join_in_a_query_of_four_tables_keeps_its_sides() {
    let sql = "SELECT * FROM a JOIN b ON a.b_id = b.id LEFT JOIN c ON b.c_id = c.id \
               JOIN d ON a.id = d.id";
    // With c small and with c the largest of all, whichever way an inner join would choose.
    for c_rows in [5, 5000] {
        let mut catalog = MemoryCatalog::new();
        let a: Vec<Vec<i64>> = (0..1000).map(|i| vec![i, i % 100]).collect();
        let b: Vec<Vec<i64>> = (0..100).map(|i| vec![i, i % 10]).collect();
        let c: Vec<Vec<i64>> = (0..c_rows).map(|i| vec![i]).collect();
        let d: Vec<Vec<i64>> = (0..200).map(|i| vec![i]).collect();
        add_table(&mut catalog, "a", &["id", "b_id"], &a);
        add_table(&mut catalog, "b", &["id", "c_id"], &b);
        add_table(&mut catalog, "c", &["id"], &c);
        add_table(&mut catalog, "d", &["id"], &d);
        let plan = by_cost(&catalog, sql);
        let (left, right) = left_join(&plan);
        assert_eq!(shape(left), "(a b)", "{}", plan.display_tree());
        assert_eq!(shape(right), "c", "{}", plan.display_tree());
        assert!(shape(&plan).contains('d'), "{}", plan.display_tree());
        assert_eq!(plan.schema(), bound(&catalog, sql).schema());
    }
}

#[test]
fn the_inputs_of_the_other_join_kinds_are_reordered_on_their_own() {
    let catalog = small_catalog();
    // c stays the right input of the semi join, and its left input orders a and b by itself.
    let plan = by_cost(
        &catalog,
        "SELECT a.id FROM b, a WHERE a.b_id = b.id AND EXISTS (SELECT * FROM c WHERE c.id = a.id)",
    );
    let semi = joins(&plan)
        .into_iter()
        .find(|j| {
            matches!(
                j,
                LogicalPlan::Join {
                    kind: JoinKind::Semi,
                    ..
                }
            )
        })
        .expect("a semi join");
    let LogicalPlan::Join { left, right, .. } = semi else {
        unreachable!();
    };
    assert_eq!(shape(left), "(a b)", "{}", plan.display_tree());
    assert_eq!(shape(right), "c", "{}", plan.display_tree());
}

#[test]
fn syntactic_order_is_rule_3_left_out() {
    let catalog = small_catalog();
    let sql = "SELECT * FROM c, a WHERE c.id = a.b_id AND a.id > 3";
    let plan = optimized(&catalog, sql, JoinOrder::Syntactic);
    assert_eq!(shape(&plan), "(c a)");
    // The other rules stay on: the filter went down and became a scan filter.
    let text = plan.display_tree();
    assert!(text.contains("filters=[id > 3]"), "{text}");
    // With the optimizer off nothing moves.
    let off = optimize(
        bound(&catalog, sql),
        OptimizerOptions {
            enabled: false,
            ..Default::default()
        },
        &catalog,
    );
    assert_eq!(shape(&off), "(c a)");
    assert!(
        off.display_tree().contains("Filter"),
        "{}",
        off.display_tree()
    );
    assert!(
        !off.display_tree().contains("filters="),
        "{}",
        off.display_tree()
    );
}

#[test]
fn a_tree_above_the_limit_of_a_set_keeps_the_order_of_the_binder() {
    let n = MAX_RELATIONS + 1;
    let schema = Schema::new(vec![Field::new("id", DataType::Int64, false)]);
    let mut catalog = MemoryCatalog::new();
    for i in 0..n {
        catalog.add_table(&format!("t{i}"), schema.clone(), vec![0]);
    }
    let sql = query_over(n, &chain(n));
    assert_eq!(
        shape(&by_cost(&catalog, &sql)),
        shape(&optimized(&catalog, &sql, JoinOrder::Syntactic))
    );
}

#[test]
fn a_left_join_is_not_a_node_of_the_join_tree() {
    let catalog = small_catalog();
    let plan = bound(
        &catalog,
        "SELECT * FROM a LEFT JOIN b ON a.b_id = b.id JOIN c ON b.c_id = c.id",
    );
    // The inner join is a tree of two relations: the left join and c.
    assert!(is_tree_node(&plan), "{}", plan.display_tree());
    assert_eq!(relation_count(&plan), 2, "{}", plan.display_tree());
    let simple = bound(&catalog, "SELECT * FROM a LEFT JOIN b ON a.b_id = b.id");
    assert!(!is_tree_node(&simple), "{}", simple.display_tree());
    assert_eq!(relation_count(&simple), 1);
}
