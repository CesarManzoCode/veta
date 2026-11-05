use std::collections::HashSet;

use veta_core::rand::Rng;
use veta_core::stats::TableStats;
use veta_core::{DataType, Field, Result, Schema, Value};

use super::*;
use crate::catalog::MemoryCatalog;
use crate::expr::{AggFunc, AggregateExpr};

/// A memory catalog whose selectivities are given per `(table, predicate SQL)`.
#[derive(Default)]
struct TestCatalog {
    inner: MemoryCatalog,
    selectivities: Vec<(String, String, f64)>,
}

impl TestCatalog {
    fn add(&mut self, name: &str, schema: Schema, key: Vec<usize>, stats: TableStats) {
        self.inner.add_table(name, schema, key);
        self.inner.set_stats(name, stats);
    }

    fn set_selectivity(&mut self, table: &str, predicate: &BoundExpr, s: f64) {
        self.selectivities
            .push((table.to_string(), predicate.to_sql(), s));
    }
}

impl Catalog for TestCatalog {
    fn table(&self, name: &str) -> Option<CatalogTable> {
        self.inner.table(name)
    }

    fn parquet_schema(&self, path: &str) -> Result<Schema> {
        self.inner.parquet_schema(path)
    }

    fn parquet_rows(&self, path: &str) -> Option<u64> {
        self.inner.parquet_rows(path)
    }

    fn selectivity(&self, table: &str, predicate: &BoundExpr) -> Option<f64> {
        let sql = predicate.to_sql();
        self.selectivities
            .iter()
            .find(|(t, p, _)| t == table && *p == sql)
            .map(|&(_, _, s)| s)
    }
}

fn int_schema(names: &[&str], nullable: bool) -> Schema {
    Schema::new(
        names
            .iter()
            .map(|n| Field::new(*n, DataType::Int64, nullable))
            .collect(),
    )
}

/// Statistics of `rows` with primary key `key`; a prefix HLL hashes the text of its values.
fn stats_of(schema: &Schema, rows: &[Vec<Value>], key: &[usize]) -> TableStats {
    let mut stats = TableStats::new(&schema.data_types(), key.len());
    stats.rows = rows.len() as u64;
    for row in rows {
        for (column, value) in stats.columns.iter_mut().zip(row) {
            column.update(value);
        }
        for j in 0..key.len() {
            let prefix: Vec<&Value> = key[..=j].iter().map(|&k| &row[k]).collect();
            if prefix.iter().any(|v| v.is_null()) {
                break;
            }
            stats.key_prefix_hll[j].add(format!("{prefix:?}").as_bytes());
        }
    }
    stats
}

fn col(index: usize, name: &str) -> BoundExpr {
    BoundExpr::column(index, name, DataType::Int64, true)
}

fn binary(op: BinaryOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        DataType::Boolean,
        true,
    )
}

fn int(v: i64) -> BoundExpr {
    BoundExpr::literal(Value::Int64(v))
}

fn q_error(estimate: f64, actual: f64) -> f64 {
    let (e, a) = (estimate.max(1.0), actual.max(1.0));
    (e / a).max(a / e)
}

fn inner_join(
    left: LogicalPlan,
    right: LogicalPlan,
    keys: Vec<(BoundExpr, BoundExpr)>,
) -> LogicalPlan {
    LogicalPlan::join(left, right, JoinKind::Inner, keys, None)
}

#[test]
fn ndv_after_filter_and_backoff() {
    assert_eq!(ndv_after_filter(100.0, 1000.0, 1.0), 100.0);
    assert_eq!(ndv_after_filter(100.0, 1000.0, 0.0), 0.0);
    assert_eq!(ndv_after_filter(0.0, 1000.0, 0.5), 0.0);
    // A unique column keeps one value per surviving row.
    assert!((ndv_after_filter(1000.0, 1000.0, 0.01) - 10.0).abs() < 1e-9);
    // Ten rows per value: almost every value survives half the rows.
    let ndv = ndv_after_filter(100.0, 1000.0, 0.5);
    assert!((ndv - 100.0 * (1.0 - 0.5f64.powi(10))).abs() < 1e-9);

    let expected = 0.01 * 0.1f64.sqrt() * 0.5f64.powf(0.25);
    assert!((backoff(vec![0.5, 0.01, 0.1]) - expected).abs() < 1e-15);
    assert_eq!(backoff(Vec::new()), 1.0);
}

#[test]
fn filters_use_the_sample_selectivity_of_their_table() {
    let schema = int_schema(&["a", "b"], false);
    let rows: Vec<Vec<Value>> = (0..1000)
        .map(|i| vec![Value::Int64(i), Value::Int64(i % 40)])
        .collect();
    let mut catalog = TestCatalog::default();
    catalog.add("t", schema.clone(), vec![0], stats_of(&schema, &rows, &[0]));
    // The predicates as the catalog sees them: over the table schema.
    let b_lt = binary(BinaryOp::Lt, col(1, "b"), int(10));
    let b_gt = binary(BinaryOp::Gt, col(1, "b"), int(2));
    catalog.set_selectivity("t", &b_lt, 0.25);
    let both = BoundExpr::and_all(vec![b_lt.clone(), b_gt.clone()]).unwrap();
    catalog.set_selectivity("t", &both, 0.2);

    // A scan of column b only: the filter's column 0 is table column 1.
    let scan = LogicalPlan::Scan {
        table: "t".into(),
        alias: "t".into(),
        table_schema: schema.clone(),
        columns: vec![1],
        filters: Vec::new(),
        schema: int_schema(&["b"], false),
    };
    let lower = LogicalPlan::filter(scan, binary(BinaryOp::Lt, col(0, "t.b"), int(10)));
    let upper = LogicalPlan::filter(lower.clone(), binary(BinaryOp::Gt, col(0, "t.b"), int(2)));
    let estimator = Estimator::new(&catalog);
    assert!((estimator.estimate(&lower) - 250.0).abs() < 1e-9);
    // The second filter contributes sel(both) / sel(first): measured jointly.
    let tree = estimator.estimate_tree(&upper);
    assert_eq!(tree.len(), 3);
    assert!((tree[0] - 200.0).abs() < 1e-9, "{tree:?}");
    assert!((tree[1] - 250.0).abs() < 1e-9);
    assert_eq!(tree[2], 1000.0);

    // A predicate the catalog cannot measure counts 0.1.
    let unknown = LogicalPlan::filter(
        LogicalPlan::scan("t", "t", schema.clone()),
        binary(BinaryOp::Eq, col(0, "t.a"), int(7)),
    );
    assert!((estimate(&unknown, &catalog) - 100.0).abs() < 1e-9);
    // A FALSE conjunct empties the input.
    let none = LogicalPlan::filter(
        LogicalPlan::scan("t", "t", schema),
        BoundExpr::literal(Value::Boolean(false)),
    );
    assert_eq!(estimate(&none, &catalog), 0.0);
}

#[test]
fn parquet_rows_come_from_the_metadata_with_default_selectivity() {
    let schema = int_schema(&["x", "y"], true);
    let mut catalog = TestCatalog::default();
    catalog.inner.add_parquet("/data/f.parquet", schema.clone());
    catalog.inner.set_parquet_rows("/data/f.parquet", 5000);
    let scan = LogicalPlan::parquet_scan("/data/f.parquet", "f", schema);
    assert_eq!(estimate(&scan, &catalog), 5000.0);
    let predicate = BoundExpr::and_all(vec![
        binary(BinaryOp::Lt, col(0, "f.x"), int(3)),
        binary(BinaryOp::Gt, col(1, "f.y"), int(4)),
    ])
    .unwrap();
    let filtered = LogicalPlan::filter(scan, predicate);
    assert!((estimate(&filtered, &catalog) - 50.0).abs() < 1e-9);
}

/// `na(k, v)`: 20 000 rows, `k = i / 10` (0..2000) or NULL on `tenths` of every ten rows;
/// `nb(k, w)`: 2000 rows with `k` unique. Every non-NULL `na.k` matches one `nb` row.
fn null_suite(tenths: i64) -> (TestCatalog, f64) {
    let na_schema = int_schema(&["k", "v"], true);
    let nb_schema = int_schema(&["k", "w"], false);
    let na: Vec<Vec<Value>> = (0..20_000i64)
        .map(|i| {
            let k = if i % 10 < tenths {
                Value::Null
            } else {
                Value::Int64(i / 10)
            };
            vec![k, Value::Int64(i)]
        })
        .collect();
    let nb: Vec<Vec<Value>> = (0..2000i64)
        .map(|k| vec![Value::Int64(k), Value::Int64(k * 3)])
        .collect();
    let actual = na.iter().filter(|r| !r[0].is_null()).count() as f64;
    let mut catalog = TestCatalog::default();
    catalog.add(
        "na",
        na_schema.clone(),
        vec![],
        stats_of(&na_schema, &na, &[]),
    );
    catalog.add(
        "nb",
        nb_schema.clone(),
        vec![0],
        stats_of(&nb_schema, &nb, &[0]),
    );
    (catalog, actual)
}

fn null_join(catalog: &TestCatalog) -> f64 {
    let na = LogicalPlan::scan("na", "na", int_schema(&["k", "v"], true));
    let nb = LogicalPlan::scan("nb", "nb", int_schema(&["k", "w"], false));
    let plan = inner_join(na, nb, vec![(col(0, "na.k"), col(0, "nb.k"))]);
    estimate(&plan, catalog)
}

#[test]
fn one_column_join_follows_the_null_fractions() {
    let (catalog, actual) = null_suite(0);
    let base = null_join(&catalog);
    assert!(q_error(base, actual) < 1.05, "{base} vs {actual}");
    for (tenths, nf) in [(5, 0.5), (9, 0.9)] {
        let (catalog, actual) = null_suite(tenths);
        let est = null_join(&catalog);
        assert!(q_error(est, actual) < 1.05, "nf {nf}: {est} vs {actual}");
        // Same NDVs on both sides: the estimate scales by (1 − nf).
        let ratio = est / base;
        assert!(
            (ratio - (1.0 - nf)).abs() < 0.03 * (1.0 - nf),
            "nf {nf}: ratio {ratio}"
        );
    }

    // `na.k IS NOT NULL` already removed the NULLs: no second (1 − nf) factor.
    let (mut catalog, actual) = null_suite(9);
    let not_null = BoundExpr::new(
        BoundExprKind::IsNull {
            expr: Box::new(col(0, "k")),
            negated: true,
        },
        DataType::Boolean,
        false,
    );
    catalog.set_selectivity("na", &not_null, 0.1);
    let na = LogicalPlan::filter(
        LogicalPlan::scan("na", "na", int_schema(&["k", "v"], true)),
        BoundExpr::new(
            BoundExprKind::IsNull {
                expr: Box::new(col(0, "na.k")),
                negated: true,
            },
            DataType::Boolean,
            false,
        ),
    );
    let nb = LogicalPlan::scan("nb", "nb", int_schema(&["k", "w"], false));
    let plan = inner_join(na, nb, vec![(col(0, "na.k"), col(0, "nb.k"))]);
    let est = estimate(&plan, &catalog);
    assert!(q_error(est, actual) < 1.1, "{est} vs {actual}");
}

/// `partsupp(ps_partkey, ps_suppkey, ps_availqty)` with key `(ps_partkey, ps_suppkey)`,
/// four suppliers per part, and `lineitem(l_orderkey, l_partkey, l_suppkey, l_linenumber)`
/// with key `(l_orderkey, l_linenumber)`, whose `(l_partkey, l_suppkey)` is always a
/// `partsupp` row: the join has exactly `|lineitem|` rows.
fn lineitem_partsupp(with_prefix_hll: bool) -> (TestCatalog, f64) {
    const PARTS: i64 = 2000;
    const SUPPLIERS: i64 = 100;
    let ps_schema = int_schema(&["ps_partkey", "ps_suppkey", "ps_availqty"], false);
    let l_schema = int_schema(
        &["l_orderkey", "l_partkey", "l_suppkey", "l_linenumber"],
        false,
    );
    let supplier = |part: i64, j: i64| (part + j * (SUPPLIERS / 4)) % SUPPLIERS;
    let mut partsupp = Vec::new();
    for part in 0..PARTS {
        for j in 0..4 {
            partsupp.push(vec![
                Value::Int64(part),
                Value::Int64(supplier(part, j)),
                Value::Int64(part * 4 + j),
            ]);
        }
    }
    let mut rng = Rng::new(7);
    let lineitem: Vec<Vec<Value>> = (0..30_000i64)
        .map(|i| {
            let part = rng.below(PARTS as u64) as i64;
            let supp = supplier(part, rng.below(4) as i64);
            vec![
                Value::Int64(i / 4),
                Value::Int64(part),
                Value::Int64(supp),
                Value::Int64(i % 4),
            ]
        })
        .collect();
    let int_of = |v: &Value| match v {
        Value::Int64(x) => *x,
        other => panic!("not an Int64: {other:?}"),
    };
    let pairs: HashSet<(i64, i64)> = partsupp
        .iter()
        .map(|r| (int_of(&r[0]), int_of(&r[1])))
        .collect();
    let actual = lineitem
        .iter()
        .filter(|r| pairs.contains(&(int_of(&r[1]), int_of(&r[2]))))
        .count() as f64;
    let ps_key: &[usize] = if with_prefix_hll { &[0, 1] } else { &[] };
    let mut catalog = TestCatalog::default();
    let ps_stats = stats_of(&ps_schema, &partsupp, ps_key);
    catalog.add("partsupp", ps_schema, vec![0, 1], ps_stats);
    let l_stats = stats_of(&l_schema, &lineitem, &[0, 3]);
    catalog.add("lineitem", l_schema, vec![0, 3], l_stats);
    (catalog, actual)
}

fn lineitem_partsupp_join(catalog: &TestCatalog, swapped: bool) -> f64 {
    let l = LogicalPlan::scan(
        "lineitem",
        "l",
        int_schema(
            &["l_orderkey", "l_partkey", "l_suppkey", "l_linenumber"],
            false,
        ),
    );
    let ps = LogicalPlan::scan(
        "partsupp",
        "ps",
        int_schema(&["ps_partkey", "ps_suppkey", "ps_availqty"], false),
    );
    let mut keys = vec![
        (col(1, "l.l_partkey"), col(0, "ps.ps_partkey")),
        (col(2, "l.l_suppkey"), col(1, "ps.ps_suppkey")),
    ];
    if swapped {
        keys.reverse();
    }
    estimate(&inner_join(l, ps, keys), catalog)
}

#[test]
fn composite_key_on_a_primary_key_prefix_uses_its_hll() {
    let (catalog, actual) = lineitem_partsupp(true);
    assert_eq!(actual, 30_000.0);
    for swapped in [false, true] {
        let est = lineitem_partsupp_join(&catalog, swapped);
        let q = q_error(est, actual);
        assert!(q <= 2.0, "swapped {swapped}: {est} vs {actual}, q {q}");
    }

    // Without the prefix HLL neither side is known: exponential backoff over
    // 1/max(ndv) per key, about 30 000 · 8000 · (1/2000) · (1/100)^½ = 12 000 rows.
    let (catalog, actual) = lineitem_partsupp(false);
    let est = lineitem_partsupp_join(&catalog, false);
    let ps = catalog.table("partsupp").unwrap().stats.unwrap();
    let l = catalog.table("lineitem").unwrap().stats.unwrap();
    let expected = 30_000.0
        * 8000.0
        * backoff(vec![
            1.0 / ps.columns[0].ndv().max(l.columns[1].ndv()),
            1.0 / ps.columns[1].ndv().max(l.columns[2].ndv()),
        ]);
    assert!(
        (est - expected).abs() < 1e-6 * expected,
        "{est} vs {expected}"
    );
    assert!(q_error(est, actual) > 2.0);
}

#[test]
fn group_by_is_bounded_by_the_input_rows() {
    let schema = int_schema(&["a", "b", "c"], false);
    let rows: Vec<Vec<Value>> = (0..1000)
        .map(|i| vec![Value::Int64(i % 50), Value::Int64(i % 40), Value::Int64(i)])
        .collect();
    let mut catalog = TestCatalog::default();
    catalog.add("g", schema.clone(), vec![2], stats_of(&schema, &rows, &[2]));
    let c_lt = binary(BinaryOp::Lt, col(2, "c"), int(100));
    catalog.set_selectivity("g", &c_lt, 0.1);
    let scan = || LogicalPlan::scan("g", "g", schema.clone());
    let count = || vec![AggregateExpr::new(AggFunc::CountStar, false, None).unwrap()];
    let group = |input: LogicalPlan, cols: &[usize]| {
        let groups = cols
            .iter()
            .map(|&c| col(c, ["g.a", "g.b", "g.c"][c]))
            .collect();
        LogicalPlan::aggregate(input, groups, count())
    };

    let by_a = estimate(&group(scan(), &[0]), &catalog);
    assert!(q_error(by_a, 50.0) < 1.05, "{by_a}");
    // 50 · 40 = 2000 combinations, but only 1000 rows.
    assert_eq!(estimate(&group(scan(), &[0, 1]), &catalog), 1000.0);
    assert_eq!(estimate(&group(scan(), &[2]), &catalog), 1000.0);
    assert_eq!(estimate(&group(scan(), &[]), &catalog), 1.0);

    // After keeping 100 rows: c has ndv' = 100, a has 50 · (1 − 0.9^20).
    let filtered = || LogicalPlan::filter(scan(), binary(BinaryOp::Lt, col(2, "g.c"), int(100)));
    let by_c = estimate(&group(filtered(), &[2]), &catalog);
    assert!(q_error(by_c, 100.0) < 1.05, "{by_c}");
    let by_a = estimate(&group(filtered(), &[0]), &catalog);
    let expected = 50.0 * (1.0 - 0.9f64.powf(20.0));
    assert!(q_error(by_a, expected) < 1.05, "{by_a} vs {expected}");
    for cols in [&[0, 1][..], &[0, 1, 2][..]] {
        assert!(estimate(&group(filtered(), cols), &catalog) <= 100.0 + 1e-9);
    }
}

#[test]
fn semi_anti_left_and_limits() {
    let c_schema = int_schema(&["c_custkey"], false);
    let o_schema = int_schema(&["o_orderkey", "o_custkey"], false);
    let customers: Vec<Vec<Value>> = (0..1500).map(|k| vec![Value::Int64(k)]).collect();
    let orders: Vec<Vec<Value>> = (0..10_000)
        .map(|i| vec![Value::Int64(i), Value::Int64(i % 1000)])
        .collect();
    let mut catalog = TestCatalog::default();
    catalog.add(
        "customer",
        c_schema.clone(),
        vec![0],
        stats_of(&c_schema, &customers, &[0]),
    );
    catalog.add(
        "orders",
        o_schema.clone(),
        vec![0],
        stats_of(&o_schema, &orders, &[0]),
    );
    let c = || LogicalPlan::scan("customer", "c", c_schema.clone());
    let o = || LogicalPlan::scan("orders", "o", o_schema.clone());
    let keys = || vec![(col(0, "c.c_custkey"), col(1, "o.o_custkey"))];
    let join = |kind| LogicalPlan::join(c(), o(), kind, keys(), None);

    let semi = estimate(&join(JoinKind::Semi), &catalog);
    let anti = estimate(&join(JoinKind::Anti), &catalog);
    assert!(q_error(semi, 1000.0) < 1.05, "{semi}");
    assert!((semi + anti - 1500.0).abs() < 1e-6);
    let inner = estimate(&join(JoinKind::Inner), &catalog);
    assert!(q_error(inner, 10_000.0) < 1.05, "{inner}");
    let left = estimate(&join(JoinKind::Left), &catalog);
    assert!(left >= inner && left >= 1500.0);
    assert_eq!(estimate(&join(JoinKind::Single), &catalog), 1500.0);
    let cross = LogicalPlan::join(c(), o(), JoinKind::Cross, Vec::new(), None);
    assert_eq!(estimate(&cross, &catalog), 15_000_000.0);

    let limited = LogicalPlan::limit(o(), Some(10), 0);
    assert_eq!(estimate(&limited, &catalog), 10.0);
    let past_end = LogicalPlan::limit(o(), None, 9_995);
    assert_eq!(estimate(&past_end, &catalog), 5.0);
    let top = LogicalPlan::TopN {
        input: Box::new(o()),
        keys: Vec::new(),
        limit: 100,
        offset: 9_950,
    };
    assert_eq!(estimate(&top, &catalog), 50.0);
    let tree = Estimator::new(&catalog).estimate_tree(&LogicalPlan::limit(
        join(JoinKind::Semi),
        Some(3),
        0,
    ));
    assert_eq!(tree.len(), 4);
    assert_eq!(tree[0], 3.0);
    assert_eq!(tree[2], 1500.0);
    assert_eq!(tree[3], 10_000.0);
}
