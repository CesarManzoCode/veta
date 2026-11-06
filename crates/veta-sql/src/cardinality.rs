//! Cardinality estimation of contract §11.5: an estimate of the output rows of every node of
//! a [`LogicalPlan`].
//!
//! Formulas of §11.5, as implemented:
//!
//! - Table rows: the exact sum over the active parts (`TableStats::rows`).
//! - Single-table predicates: the conjuncts that read columns of one base relation are
//!   rewritten over its table schema and measured together on the union of the part
//!   samples ([`Catalog::selectivity`]); a conjunct added above earlier ones on the same
//!   base contributes `sel(all) / sel(earlier)`, so correlated predicates are measured
//!   jointly. Without a sample (`read_parquet`) each conjunct has selectivity 0.1.
//! - `ndv' = ndv · (1 − (1 − s)^(n/ndv))` after any reduction of `n` rows by `s`.
//! - `nf = nulls / rows`; a conjunct that rejects NULL in a column (a comparison, `LIKE`,
//!   `IN` or `IS NOT NULL` on it) sets its `nf` to 0, since the sample already removed
//!   those rows.
//! - One-column inner equi-join: `|L| · |R| · (1 − nf_L)(1 − nf_R) / max(ndv'_L, ndv'_R)`.
//! - Composite keys: a side whose key columns are a prefix of its table's primary key (as a
//!   set) takes the NDV of that prefix's HLL; the side without a known NDV takes
//!   `min(rows, Π ndv_i, NDV of the other side)`; with neither known, the product of the
//!   per-key selectivities `1 / max(ndv_L,i, ndv_R,i)` with exponential backoff.
//! - `GROUP BY`: `min(input rows, Π ndv')`.
//!
//! Decisions within the contract (§11.5 does not fix them), labelled as design:
//!
//! - An unknown NDV of a join key counts as the row count of its side (each value once).
//! - A table without statistics counts [`DEFAULT_ROWS`] rows; an unknown predicate shape
//!   (mixing relations, or without a sample) counts [`DEFAULT_SELECTIVITY`] per conjunct,
//!   except an `OR` mixing relations: each branch is estimated as a filter on its own and
//!   the branches combine as independent, `1 − Π (1 − s_i)`;
//!   `a = b` between two relations counts like a join key.
//! - Semi joins keep the fraction `min(1, NDV_R / NDV_L) · (1 − nf_L)` of the left rows
//!   (containment); anti joins keep the rest; a null-aware anti join also drops left rows
//!   with a NULL key, and every row if the right side has a NULL key. Without keys the
//!   fraction is `min(1, |inner| / |L|)`.
//! - `Left` keeps `max(|inner|, |L|)` rows; `Single` keeps `|L|`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use veta_core::Value;

use crate::catalog::{Catalog, CatalogTable};
use crate::expr::{BinaryOp, BoundExpr, BoundExprKind};
use crate::plan::{JoinKind, LogicalPlan};

/// Selectivity of a conjunct that cannot be measured (§11.5 for `read_parquet`).
pub const DEFAULT_SELECTIVITY: f64 = 0.1;
/// Rows of a table without statistics or of an unreadable Parquet file.
pub const DEFAULT_ROWS: f64 = 1000.0;

/// `ndv' = ndv · (1 − (1 − s)^(n/ndv))`: distinct values left after keeping a fraction `s`
/// of `n` rows (§11.5).
pub fn ndv_after_filter(ndv: f64, rows: f64, s: f64) -> f64 {
    if ndv <= 0.0 || rows <= 0.0 {
        return 0.0;
    }
    let s = s.clamp(0.0, 1.0);
    ndv * (1.0 - (1.0 - s).powf(rows / ndv))
}

/// Product of selectivities with exponential backoff: `s1 · s2^½ · s3^¼ …`, sorted from
/// the smallest (§11.5).
pub fn backoff(mut selectivities: Vec<f64>) -> f64 {
    selectivities.sort_by(f64::total_cmp);
    let mut out = 1.0;
    let mut exponent = 1.0;
    for s in selectivities {
        out *= s.powf(exponent);
        exponent /= 2.0;
    }
    out
}

/// Estimated output rows of `plan`.
pub fn estimate(plan: &LogicalPlan, catalog: &dyn Catalog) -> f64 {
    Estimator::new(catalog).estimate(plan)
}

/// One `Scan` or `ParquetScan` met while estimating.
struct Base {
    /// The stored table; `None` for `read_parquet`.
    table: Option<Rc<CatalogTable>>,
    rows: f64,
}

/// What is known about one output column of a node.
#[derive(Debug, Clone)]
struct ColumnEst {
    /// Distinct non-NULL values (`ndv'`); `None` if unknown.
    ndv: Option<f64>,
    null_frac: f64,
    /// The base relation the column comes from and its expression over that base's
    /// schema; `None` once it no longer describes the base rows (aggregates, the
    /// NULL-extended side of an outer join).
    origin: Option<(usize, BoundExpr)>,
}

/// Estimate of one node.
#[derive(Debug, Clone)]
struct Rel {
    rows: f64,
    columns: Vec<ColumnEst>,
    /// Conjuncts already applied to each base, over the base schema.
    applied: Vec<(usize, Vec<BoundExpr>)>,
}

impl Rel {
    /// Same columns over `rows` rows: every NDV reduced by `ndv'` and capped.
    fn reduced(mut self, rows: f64) -> Rel {
        let s = if self.rows > 0.0 {
            (rows / self.rows).min(1.0)
        } else {
            0.0
        };
        let n = self.rows;
        for c in &mut self.columns {
            c.ndv = c.ndv.map(|d| ndv_after_filter(d, n, s).min(rows));
        }
        self.rows = rows;
        self
    }
}

/// Estimates plans against a catalog, caching table lookups and sample selectivities.
pub struct Estimator<'a> {
    catalog: &'a dyn Catalog,
    tables: RefCell<HashMap<String, Option<Rc<CatalogTable>>>>,
    selectivities: RefCell<HashMap<(String, String), Option<f64>>>,
    bases: RefCell<Vec<Base>>,
}

/// Estimate of one input of a join tree being reordered (§11.2 rule 3). Its columns are the
/// ones the caller asked for in [`Estimator::join_input`] and, after a join, the columns of
/// the left input followed by those of the right one.
#[derive(Debug, Clone)]
pub struct JoinEstimate(Rel);

impl JoinEstimate {
    pub fn rows(&self) -> f64 {
        self.0.rows
    }
}

impl<'a> Estimator<'a> {
    pub fn new(catalog: &'a dyn Catalog) -> Estimator<'a> {
        Estimator {
            catalog,
            tables: RefCell::new(HashMap::new()),
            selectivities: RefCell::new(HashMap::new()),
            bases: RefCell::new(Vec::new()),
        }
    }

    /// Estimated output rows of `plan`.
    pub fn estimate(&self, plan: &LogicalPlan) -> f64 {
        self.bases.borrow_mut().clear();
        self.rel(plan, &mut Vec::new()).rows
    }

    /// Estimated output rows of every node of `plan`, in preorder (a node, then its
    /// children left to right), as in [`LogicalPlan::children`].
    pub fn estimate_tree(&self, plan: &LogicalPlan) -> Vec<f64> {
        self.bases.borrow_mut().clear();
        let mut trace = Vec::new();
        self.rel(plan, &mut trace);
        trace
    }

    /// Forgets the base relations met so far. The [`JoinEstimate`]s of one join tree must
    /// come from the same period between resets.
    pub fn reset(&self) {
        self.bases.borrow_mut().clear();
    }

    /// Estimate of `plan` as an input of a join tree, with only the output `columns` that
    /// the predicates of the tree read, in that order.
    pub fn join_input(&self, plan: &LogicalPlan, columns: &[usize]) -> JoinEstimate {
        let mut rel = self.rel(plan, &mut Vec::new());
        rel.columns = columns.iter().map(|&c| rel.columns[c].clone()).collect();
        JoinEstimate(rel)
    }

    /// Estimate of the inner join of two inputs; `keys` and `residual` are bound over the
    /// columns of the estimates, as in a [`LogicalPlan::Join`].
    pub fn inner_join(
        &self,
        left: &JoinEstimate,
        right: &JoinEstimate,
        keys: &[(BoundExpr, BoundExpr)],
        residual: Option<&BoundExpr>,
    ) -> JoinEstimate {
        JoinEstimate(self.inner_rel(&left.0, &right.0, keys, residual))
    }

    fn table(&self, name: &str) -> Option<Rc<CatalogTable>> {
        let key = name.to_ascii_lowercase();
        self.tables
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| self.catalog.table(name).map(Rc::new))
            .clone()
    }

    fn add_base(&self, base: Base) -> usize {
        let mut bases = self.bases.borrow_mut();
        bases.push(base);
        bases.len() - 1
    }

    fn rel(&self, plan: &LogicalPlan, trace: &mut Vec<f64>) -> Rel {
        let slot = trace.len();
        trace.push(0.0);
        let rel = self.node(plan, trace);
        trace[slot] = rel.rows;
        rel
    }

    fn node(&self, plan: &LogicalPlan, trace: &mut Vec<f64>) -> Rel {
        match plan {
            LogicalPlan::Scan {
                table,
                table_schema,
                columns,
                ..
            } => {
                let info = self.table(table);
                let stats = info.as_ref().and_then(|t| t.stats.as_ref());
                let rows = stats.map_or(DEFAULT_ROWS, |s| s.rows as f64);
                let id = self.add_base(Base {
                    table: info.clone(),
                    rows,
                });
                let columns = columns
                    .iter()
                    .map(|&c| {
                        let field = &table_schema.fields[c];
                        let column = stats.and_then(|s| s.columns.get(c));
                        ColumnEst {
                            ndv: column.map(|s| s.ndv().min(rows)),
                            null_frac: column.map_or(0.0, |s| s.null_fraction()),
                            origin: Some((
                                id,
                                BoundExpr::column(
                                    c,
                                    field.name.clone(),
                                    field.data_type,
                                    field.nullable,
                                ),
                            )),
                        }
                    })
                    .collect();
                Rel {
                    rows,
                    columns,
                    applied: Vec::new(),
                }
            }
            LogicalPlan::ParquetScan {
                path,
                file_schema,
                columns,
                ..
            } => {
                let rows = self
                    .catalog
                    .parquet_rows(path)
                    .map_or(DEFAULT_ROWS, |r| r as f64);
                let id = self.add_base(Base { table: None, rows });
                let columns = columns
                    .iter()
                    .map(|&c| {
                        let field = &file_schema.fields[c];
                        ColumnEst {
                            ndv: None,
                            null_frac: 0.0,
                            origin: Some((
                                id,
                                BoundExpr::column(
                                    c,
                                    field.name.clone(),
                                    field.data_type,
                                    field.nullable,
                                ),
                            )),
                        }
                    })
                    .collect();
                Rel {
                    rows,
                    columns,
                    applied: Vec::new(),
                }
            }
            LogicalPlan::SingleRow => Rel {
                rows: 1.0,
                columns: Vec::new(),
                applied: Vec::new(),
            },
            LogicalPlan::Filter { input, predicate } => {
                let input = self.rel(input, trace);
                self.filter(input, predicate.clone().into_conjuncts())
            }
            LogicalPlan::Project { input, exprs, .. } => {
                let input = self.rel(input, trace);
                let columns = exprs.iter().map(|e| project_column(e, &input)).collect();
                Rel {
                    rows: input.rows,
                    columns,
                    applied: input.applied,
                }
            }
            LogicalPlan::Aggregate {
                input,
                groups,
                aggregates,
                ..
            } => {
                let input = self.rel(input, trace);
                let group_columns: Vec<ColumnEst> =
                    groups.iter().map(|g| project_column(g, &input)).collect();
                let rows = if groups.is_empty() {
                    1.0
                } else {
                    let product: f64 = group_columns
                        .iter()
                        .map(|c| c.ndv.unwrap_or(input.rows))
                        .product();
                    input.rows.min(product)
                };
                let mut columns: Vec<ColumnEst> = group_columns
                    .into_iter()
                    .map(|c| ColumnEst {
                        ndv: c.ndv.map(|d| d.min(rows)),
                        null_frac: c.null_frac,
                        origin: None,
                    })
                    .collect();
                columns.extend(aggregates.iter().map(|_| ColumnEst {
                    ndv: Some(rows),
                    null_frac: 0.0,
                    origin: None,
                }));
                Rel {
                    rows,
                    columns,
                    applied: Vec::new(),
                }
            }
            LogicalPlan::Join {
                left,
                right,
                kind,
                keys,
                residual,
                ..
            } => {
                let left = self.rel(left, trace);
                let right = self.rel(right, trace);
                self.join(left, right, *kind, keys, residual.as_ref())
            }
            LogicalPlan::Sort { input, .. } => self.rel(input, trace),
            LogicalPlan::TopN {
                input,
                limit,
                offset,
                ..
            } => {
                let input = self.rel(input, trace);
                let rows = (input.rows - *offset as f64).max(0.0).min(*limit as f64);
                input.reduced(rows)
            }
            LogicalPlan::Limit {
                input,
                limit,
                offset,
            } => {
                let input = self.rel(input, trace);
                let mut rows = (input.rows - *offset as f64).max(0.0);
                if let Some(limit) = limit {
                    rows = rows.min(*limit as f64);
                }
                input.reduced(rows)
            }
        }
    }

    /// Selectivity of `conjuncts` (over the schema of base `id`) on its rows: measured on
    /// the sample of a stored table, 0.1 per conjunct otherwise.
    fn base_selectivity(&self, id: usize, conjuncts: &[BoundExpr]) -> f64 {
        if conjuncts.is_empty() {
            return 1.0;
        }
        let fallback = DEFAULT_SELECTIVITY.powi(conjuncts.len() as i32);
        let table = self.bases.borrow()[id].table.clone();
        let Some(table) = table else {
            return fallback;
        };
        let predicate = BoundExpr::and_all(conjuncts.to_vec()).expect("non-empty conjuncts");
        let key = (table.name.clone(), predicate.to_sql());
        let measured = *self
            .selectivities
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| self.catalog.selectivity(&table.name, &predicate));
        measured.map_or(fallback, |s| s.clamp(0.0, 1.0))
    }

    /// Applies `conjuncts` (over the columns of `rel`) to `rel`.
    fn filter(&self, mut rel: Rel, conjuncts: Vec<BoundExpr>) -> Rel {
        let mut sel = 1.0;
        let mut per_base: Vec<(usize, Vec<BoundExpr>)> = Vec::new();
        let mut not_null: Vec<usize> = Vec::new();
        for conjunct in conjuncts {
            not_null.extend(null_rejected_columns(&conjunct));
            if let Some(value) = conjunct.as_literal() {
                if *value != Value::Boolean(true) {
                    sel = 0.0;
                }
                continue;
            }
            if let Some((base, expr)) = substitute(&conjunct, &rel.columns) {
                match per_base.iter_mut().find(|(b, _)| *b == base) {
                    Some((_, list)) => list.push(expr),
                    None => per_base.push((base, vec![expr])),
                }
            } else if let Some(s) = column_equality(&conjunct, &rel) {
                sel *= s;
            } else if let Some(s) = self.disjunction(&rel, &conjunct) {
                sel *= s;
            } else {
                sel *= DEFAULT_SELECTIVITY;
            }
        }
        for (base, new) in per_base {
            let position = rel.applied.iter().position(|(b, _)| *b == base);
            let earlier: Vec<BoundExpr> = position
                .map(|p| rel.applied[p].1.clone())
                .unwrap_or_default();
            let before = self.base_selectivity(base, &earlier);
            let mut all = earlier;
            all.extend(new);
            let after = self.base_selectivity(base, &all);
            sel *= if before > 0.0 {
                (after / before).min(1.0)
            } else {
                after
            };
            match position {
                Some(p) => rel.applied[p].1 = all,
                None => rel.applied.push((base, all)),
            }
        }
        let rows = rel.rows * sel;
        let mut rel = rel.reduced(rows);
        for index in not_null {
            if let Some(c) = rel.columns.get_mut(index) {
                c.null_frac = 0.0;
            }
        }
        rel
    }

    /// Selectivity on `rel` of an `OR` that mixes relations: each branch is estimated as a
    /// filter of `rel` and the branches combine as independent, `1 − Π (1 − s_i)`.
    fn disjunction(&self, rel: &Rel, conjunct: &BoundExpr) -> Option<f64> {
        if !matches!(
            conjunct.kind,
            BoundExprKind::Binary {
                op: BinaryOp::Or,
                ..
            }
        ) || rel.rows <= 0.0
        {
            return None;
        }
        let none: f64 = conjunct
            .clone()
            .into_disjuncts()
            .into_iter()
            .map(|branch| {
                let kept = self.filter(rel.clone(), branch.into_conjuncts()).rows / rel.rows;
                1.0 - kept.clamp(0.0, 1.0)
            })
            .product();
        Some(1.0 - none)
    }

    /// NDV of the composite key `columns` of `side` from the HLL of a primary-key prefix:
    /// the key expressions must be columns of one stored base forming, as a set, the first
    /// `columns.len()` primary-key columns of its table.
    fn prefix_ndv(&self, side: &Rel, columns: &[ColumnEst]) -> Option<f64> {
        let mut base = None;
        let mut indices = Vec::with_capacity(columns.len());
        for c in columns {
            let (b, expr) = c.origin.as_ref()?;
            let BoundExprKind::Column { index, .. } = &expr.kind else {
                return None;
            };
            if base.is_some_and(|x| x != *b) {
                return None;
            }
            base = Some(*b);
            indices.push(*index);
        }
        let bases = self.bases.borrow();
        let base = &bases[base?];
        let table = base.table.as_ref()?;
        let stats = table.stats.as_ref()?;
        let k = indices.len();
        if k == 0 || table.primary_key.len() < k {
            return None;
        }
        let mut prefix = table.primary_key[..k].to_vec();
        prefix.sort_unstable();
        indices.sort_unstable();
        if prefix != indices {
            return None;
        }
        let ndv = stats.key_prefix_hll.get(k - 1)?.estimate().min(base.rows);
        let s = if base.rows > 0.0 {
            (side.rows / base.rows).min(1.0)
        } else {
            0.0
        };
        Some(ndv_after_filter(ndv, base.rows, s).min(side.rows))
    }

    /// Combined NDV of a key of `side`: the prefix HLL, or `min(rows, Π ndv_i)`.
    fn key_ndv(&self, side: &Rel, columns: &[ColumnEst]) -> f64 {
        if columns.len() > 1
            && let Some(ndv) = self.prefix_ndv(side, columns)
        {
            return ndv;
        }
        let product: f64 = columns.iter().map(|c| c.ndv.unwrap_or(side.rows)).product();
        side.rows.min(product)
    }

    /// Rows of the inner equi-join of `left` and `right` on `keys`, before any residual.
    fn inner_rows(&self, left: &Rel, right: &Rel, keys: &[(BoundExpr, BoundExpr)]) -> f64 {
        let cross = left.rows * right.rows;
        if keys.is_empty() {
            return cross;
        }
        let lk: Vec<ColumnEst> = keys.iter().map(|(l, _)| project_column(l, left)).collect();
        let rk: Vec<ColumnEst> = keys.iter().map(|(_, r)| project_column(r, right)).collect();
        let null_factor: f64 = lk.iter().chain(&rk).map(|c| 1.0 - c.null_frac).product();
        if keys.len() == 1 {
            let l = lk[0].ndv.unwrap_or(left.rows);
            let r = rk[0].ndv.unwrap_or(right.rows);
            return cross * null_factor / l.max(r).max(1.0);
        }
        let contained = |side: &Rel, columns: &[ColumnEst], other: f64| {
            let product: f64 = columns.iter().map(|c| c.ndv.unwrap_or(side.rows)).product();
            side.rows.min(product).min(other)
        };
        let ndvs = match (self.prefix_ndv(left, &lk), self.prefix_ndv(right, &rk)) {
            (Some(l), Some(r)) => Some((l, r)),
            (Some(l), None) => Some((l, contained(right, &rk, l))),
            (None, Some(r)) => Some((contained(left, &lk, r), r)),
            (None, None) => None,
        };
        match ndvs {
            Some((l, r)) => cross * null_factor / l.max(r).max(1.0),
            None => {
                let selectivities = lk
                    .iter()
                    .zip(&rk)
                    .map(|(l, r)| {
                        let l = l.ndv.unwrap_or(left.rows);
                        let r = r.ndv.unwrap_or(right.rows);
                        1.0 / l.max(r).max(1.0)
                    })
                    .collect();
                cross * null_factor * backoff(selectivities)
            }
        }
    }

    /// Fraction of the left rows of a semi join that have a match.
    fn semi_fraction(
        &self,
        left: &Rel,
        right: &Rel,
        keys: &[(BoundExpr, BoundExpr)],
        residual: Option<&BoundExpr>,
    ) -> f64 {
        if left.rows <= 0.0 || right.rows <= 0.0 {
            return 0.0;
        }
        if keys.is_empty() {
            let inner = self.concat(left.clone(), right.clone(), left.rows * right.rows);
            let inner = match residual {
                Some(r) => self.filter(inner, r.clone().into_conjuncts()).rows,
                None => inner.rows,
            };
            return (inner / left.rows).min(1.0);
        }
        let lk: Vec<ColumnEst> = keys.iter().map(|(l, _)| project_column(l, left)).collect();
        let rk: Vec<ColumnEst> = keys.iter().map(|(_, r)| project_column(r, right)).collect();
        let not_null: f64 = lk.iter().map(|c| 1.0 - c.null_frac).product();
        let l = self.key_ndv(left, &lk).max(1.0);
        let r = self.key_ndv(right, &rk);
        (r / l).min(1.0) * not_null
    }

    /// `left ++ right` columns over `rows` rows.
    fn concat(&self, left: Rel, right: Rel, rows: f64) -> Rel {
        let left_rows = rows.min(left.rows);
        let right_rows = rows.min(right.rows);
        let left = left.reduced(left_rows);
        let right = right.reduced(right_rows);
        let mut columns = left.columns;
        columns.extend(right.columns);
        for c in &mut columns {
            c.ndv = c.ndv.map(|d| d.min(rows));
        }
        let mut applied = left.applied;
        applied.extend(right.applied);
        Rel {
            rows,
            columns,
            applied,
        }
    }

    /// The inner join of `left` and `right`, residual included.
    fn inner_rel(
        &self,
        left: &Rel,
        right: &Rel,
        keys: &[(BoundExpr, BoundExpr)],
        residual: Option<&BoundExpr>,
    ) -> Rel {
        let rows = self.inner_rows(left, right, keys);
        let rel = self.concat(left.clone(), right.clone(), rows);
        match residual {
            Some(r) => self.filter(rel, r.clone().into_conjuncts()),
            None => rel,
        }
    }

    fn join(
        &self,
        left: Rel,
        right: Rel,
        kind: JoinKind,
        keys: &[(BoundExpr, BoundExpr)],
        residual: Option<&BoundExpr>,
    ) -> Rel {
        match kind {
            JoinKind::Inner | JoinKind::Cross => self.inner_rel(&left, &right, keys, residual),
            JoinKind::Left => {
                let matched = self.inner_rel(&left, &right, keys, residual).rows;
                let rows = matched.max(left.rows);
                let fill = if rows > 0.0 { matched / rows } else { 0.0 };
                let right = null_extended(right, fill);
                self.concat(left, right, rows)
            }
            JoinKind::Single => {
                let fill = self.semi_fraction(&left, &right, keys, residual);
                let rows = left.rows;
                let right = null_extended(right, fill);
                self.concat(left, right, rows)
            }
            JoinKind::Semi | JoinKind::Anti | JoinKind::NullAwareAnti => {
                let fraction = self.semi_fraction(&left, &right, keys, residual);
                let rows = match kind {
                    JoinKind::Semi => left.rows * fraction,
                    JoinKind::Anti => left.rows * (1.0 - fraction),
                    _ => {
                        let right_nulls = keys
                            .iter()
                            .any(|(_, r)| project_column(r, &right).null_frac * right.rows >= 1.0);
                        if right_nulls {
                            0.0
                        } else {
                            let not_null: f64 = keys
                                .iter()
                                .map(|(l, _)| 1.0 - project_column(l, &left).null_frac)
                                .product();
                            // `fraction` already counts only left rows with a key.
                            left.rows * (not_null - fraction).max(0.0)
                        }
                    }
                };
                left.reduced(rows)
            }
        }
    }
}

/// Columns of the NULL-supplying side of an outer join, of which a fraction `fill` of the
/// output rows have a match.
fn null_extended(mut rel: Rel, fill: f64) -> Rel {
    for c in &mut rel.columns {
        c.null_frac = 1.0 - (1.0 - c.null_frac) * fill;
        c.origin = None;
    }
    rel.applied.clear();
    rel
}

/// Estimate of an expression over the columns of `input`.
fn project_column(expr: &BoundExpr, input: &Rel) -> ColumnEst {
    if let BoundExprKind::Column { index, .. } = &expr.kind
        && let Some(c) = input.columns.get(*index)
    {
        return c.clone();
    }
    if let Some(value) = expr.as_literal() {
        return ColumnEst {
            ndv: Some(if value.is_null() { 0.0 } else { 1.0 }),
            null_frac: if value.is_null() { 1.0 } else { 0.0 },
            origin: None,
        };
    }
    let read: Vec<&ColumnEst> = expr
        .columns()
        .into_iter()
        .filter_map(|i| input.columns.get(i))
        .collect();
    let ndv = read
        .iter()
        .map(|c| c.ndv)
        .try_fold(1.0, |acc, n| n.map(|n| acc * n))
        .map(|n| n.min(input.rows));
    let null_frac = if expr.nullable {
        read.iter().map(|c| c.null_frac).fold(0.0, f64::max)
    } else {
        0.0
    };
    ColumnEst {
        ndv,
        null_frac,
        origin: substitute(expr, &input.columns),
    }
}

/// `expr` rewritten over the schema of the one base relation all its columns come from.
fn substitute(expr: &BoundExpr, columns: &[ColumnEst]) -> Option<(usize, BoundExpr)> {
    fn rewrite(
        e: &BoundExpr,
        columns: &[ColumnEst],
        base: &mut Option<usize>,
    ) -> Option<BoundExpr> {
        match &e.kind {
            BoundExprKind::Column { index, .. } => {
                let (b, origin) = columns.get(*index)?.origin.as_ref()?;
                if base.is_some_and(|x| x != *b) {
                    return None;
                }
                *base = Some(*b);
                Some(origin.clone())
            }
            BoundExprKind::AggregateRef { .. } => None,
            _ => {
                let mut out = e.clone();
                for child in out.children_mut() {
                    let new = rewrite(child, columns, base)?;
                    *child = new;
                }
                Some(out)
            }
        }
    }
    let mut base = None;
    let out = rewrite(expr, columns, &mut base)?;
    Some((base?, out))
}

/// The column read directly (through casts) by `expr`, if it is one.
fn direct_column(expr: &BoundExpr) -> Option<usize> {
    match &expr.kind {
        BoundExprKind::Column { index, .. } => Some(*index),
        BoundExprKind::Cast(inner) => direct_column(inner),
        _ => None,
    }
}

/// Columns in which `conjunct` is never true for NULL: operands of comparisons, `LIKE`,
/// `IN` and `IS NOT NULL`.
fn null_rejected_columns(conjunct: &BoundExpr) -> Vec<usize> {
    match &conjunct.kind {
        BoundExprKind::Binary {
            op:
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq,
            left,
            right,
        } => [direct_column(left), direct_column(right)]
            .into_iter()
            .flatten()
            .collect(),
        BoundExprKind::Like { expr, .. } | BoundExprKind::InList { expr, .. } => {
            direct_column(expr).into_iter().collect()
        }
        BoundExprKind::IsNull {
            expr,
            negated: true,
        } => direct_column(expr).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Selectivity of `a = b` between columns of `rel` that are not from one base: the
/// one-column join formula.
fn column_equality(conjunct: &BoundExpr, rel: &Rel) -> Option<f64> {
    let BoundExprKind::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = &conjunct.kind
    else {
        return None;
    };
    let a = rel.columns.get(direct_column(left)?)?;
    let b = rel.columns.get(direct_column(right)?)?;
    let ndv = match (a.ndv, b.ndv) {
        (None, None) => return None,
        (x, y) => x.unwrap_or(0.0).max(y.unwrap_or(0.0)),
    };
    Some((1.0 - a.null_frac) * (1.0 - b.null_frac) / ndv.max(1.0))
}

#[cfg(test)]
mod tests;
