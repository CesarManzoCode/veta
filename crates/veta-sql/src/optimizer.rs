//! Rewrite rules of contract §11.2 over a bound logical plan, in this order:
//!
//! 1. constant folding and boolean simplification;
//! 2. predicate pushdown through `Project` and joins (only to the preserved side of a
//!    `Left` join, never from above into the side that supplies NULLs);
//! 3. join graph reordering by estimated cost ([`join_order`]);
//! 4. scan filters derived from single-table conjunctions (`col op literal`, `BETWEEN`,
//!    `IN (literals)`), which prune parts, granules and Parquet row groups;
//! 5. column pruning, so a scan reads only the columns something above it uses;
//! 6. `Sort` under `Limit` becomes `TopN`.
//!
//! With `--no-optimizer` only rule 5 runs (§11.3). The binder already builds joins in the
//! connected syntactic order with the remaining predicates in a `Filter` above them, so
//! `--join-order syntactic` is rule 3 left out and the other rules on.

use std::collections::BTreeSet;
use std::ops::Bound;

use veta_core::predicate::ScanFilter;
use veta_core::{DataType, Field, Schema, Value};

use crate::catalog::Catalog;
use crate::expr::{BinaryOp, BoundExpr, BoundExprKind, UnaryOp, cast_value};
use crate::plan::{JoinKind, LogicalPlan};

pub mod join_order;

/// `--join-order` (contract §11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinOrder {
    /// Rule 3: DPccp or GOO over the estimated cost.
    Cost,
    /// The connected syntactic order the binder builds.
    Syntactic,
}

/// What the optimizer does (contract §11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizerOptions {
    /// `false` with `--no-optimizer`: only column pruning runs.
    pub enabled: bool,
    pub join_order: JoinOrder,
}

impl Default for OptimizerOptions {
    fn default() -> Self {
        OptimizerOptions {
            enabled: true,
            join_order: JoinOrder::Cost,
        }
    }
}

/// Applies the rules of §11.2 that `options` turns on, in their order. `catalog` supplies
/// the statistics rule 3 estimates with.
pub fn optimize(
    plan: LogicalPlan,
    options: OptimizerOptions,
    catalog: &dyn Catalog,
) -> LogicalPlan {
    let mut plan = plan;
    if options.enabled {
        plan = fold_constants(plan);
        plan = push_down_predicates(plan);
        if options.join_order == JoinOrder::Cost {
            plan = join_order::reorder_joins(plan, catalog);
        }
        plan = derive_scan_filters(plan);
    }
    plan = prune_columns(plan);
    if options.enabled {
        plan = sort_limit_to_top_n(plan);
    }
    plan
}

/// Rebuilds `plan` with `f` applied to each child; schemas stay as they were.
fn map_children(plan: LogicalPlan, f: &mut dyn FnMut(LogicalPlan) -> LogicalPlan) -> LogicalPlan {
    let mut apply = |input: Box<LogicalPlan>| Box::new(f(*input));
    match plan {
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: apply(input),
            predicate,
        },
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => LogicalPlan::Project {
            input: apply(input),
            exprs,
            schema,
        },
        LogicalPlan::Aggregate {
            input,
            groups,
            aggregates,
            schema,
        } => LogicalPlan::Aggregate {
            input: apply(input),
            groups,
            aggregates,
            schema,
        },
        LogicalPlan::Join {
            left,
            right,
            kind,
            keys,
            residual,
            schema,
        } => {
            let left = apply(left);
            let right = apply(right);
            LogicalPlan::Join {
                left,
                right,
                kind,
                keys,
                residual,
                schema,
            }
        }
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input: apply(input),
            keys,
        },
        LogicalPlan::TopN {
            input,
            keys,
            limit,
            offset,
        } => LogicalPlan::TopN {
            input: apply(input),
            keys,
            limit,
            offset,
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: apply(input),
            limit,
            offset,
        },
        leaf @ (LogicalPlan::Scan { .. }
        | LogicalPlan::ParquetScan { .. }
        | LogicalPlan::SingleRow) => leaf,
    }
}

fn with_filter(plan: LogicalPlan, conjuncts: Vec<BoundExpr>) -> LogicalPlan {
    match BoundExpr::and_all(conjuncts) {
        Some(predicate) => LogicalPlan::filter(plan, predicate),
        None => plan,
    }
}

// ---------------------------------------------------------------------------------------
// Rule 1: constant folding and boolean simplification

/// Folds the constant subexpressions of every expression of the plan and drops filters
/// that became `TRUE`.
pub fn fold_constants(plan: LogicalPlan) -> LogicalPlan {
    let plan = map_children(plan, &mut fold_constants);
    match plan {
        LogicalPlan::Filter {
            input,
            mut predicate,
        } => {
            fold_expr(&mut predicate);
            if predicate.as_literal() == Some(&Value::Boolean(true)) {
                *input
            } else {
                LogicalPlan::Filter { input, predicate }
            }
        }
        LogicalPlan::Project {
            input,
            mut exprs,
            schema,
        } => {
            exprs.iter_mut().for_each(fold_expr);
            LogicalPlan::Project {
                input,
                exprs,
                schema,
            }
        }
        LogicalPlan::Aggregate {
            input,
            mut groups,
            mut aggregates,
            schema,
        } => {
            groups.iter_mut().for_each(fold_expr);
            for agg in aggregates.iter_mut() {
                if let Some(arg) = agg.arg.as_mut() {
                    fold_expr(arg);
                }
            }
            LogicalPlan::Aggregate {
                input,
                groups,
                aggregates,
                schema,
            }
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            mut keys,
            mut residual,
            schema,
        } => {
            for (l, r) in keys.iter_mut() {
                fold_expr(l);
                fold_expr(r);
            }
            if let Some(r) = residual.as_mut() {
                fold_expr(r);
            }
            if residual
                .as_ref()
                .is_some_and(|r| r.as_literal() == Some(&Value::Boolean(true)))
            {
                residual = None;
            }
            LogicalPlan::Join {
                left,
                right,
                kind,
                keys,
                residual,
                schema,
            }
        }
        LogicalPlan::Sort { input, mut keys } => {
            keys.iter_mut().for_each(|k| fold_expr(&mut k.expr));
            LogicalPlan::Sort { input, keys }
        }
        LogicalPlan::TopN {
            input,
            mut keys,
            limit,
            offset,
        } => {
            keys.iter_mut().for_each(|k| fold_expr(&mut k.expr));
            LogicalPlan::TopN {
                input,
                keys,
                limit,
                offset,
            }
        }
        other => other,
    }
}

/// Folds `e` bottom-up in place.
pub fn fold_expr(e: &mut BoundExpr) {
    for child in e.children_mut() {
        fold_expr(child);
    }
    if let Some(folded) = simplify(e) {
        *e = folded;
    }
}

fn boolean(value: Option<bool>) -> BoundExpr {
    match value {
        Some(b) => BoundExpr::literal(Value::Boolean(b)),
        None => BoundExpr::typed_literal(Value::Null, DataType::Boolean),
    }
}

/// `Some(Some(b))` for a boolean literal, `Some(None)` for a NULL literal.
fn as_boolean(e: &BoundExpr) -> Option<Option<bool>> {
    match e.as_literal()? {
        Value::Boolean(b) => Some(Some(*b)),
        Value::Null => Some(None),
        _ => None,
    }
}

/// The literal `e` becomes, keeping its type; `None` when it does not fold. Anything that
/// could fail at run time (an overflow, a bad cast) is left for the executor, so folding
/// never changes whether a query errs.
fn simplify(e: &BoundExpr) -> Option<BoundExpr> {
    let typed = |v: Value| BoundExpr::typed_literal(v, e.data_type);
    match &e.kind {
        BoundExprKind::Cast(inner) => {
            let v = inner.as_literal()?;
            Some(typed(cast_value(v, e.data_type).ok()?))
        }
        BoundExprKind::Unary { op, expr } => {
            let v = expr.as_literal()?;
            match op {
                UnaryOp::Not => {
                    if e.data_type != DataType::Boolean {
                        return None;
                    }
                    Some(boolean(as_boolean(expr)?.map(|b| !b)))
                }
                UnaryOp::Neg => match v {
                    Value::Null => Some(typed(Value::Null)),
                    Value::Float64(f) => Some(typed(Value::Float64(-f))),
                    _ if e.data_type.is_integer() => {
                        let n = v.as_i128()?.checked_neg()?;
                        Some(typed(cast_value(&Value::Int128(n), e.data_type).ok()?))
                    }
                    _ => None,
                },
            }
        }
        BoundExprKind::Binary { op, left, right } => simplify_binary(e, *op, left, right),
        BoundExprKind::IsNull { expr, negated } => {
            let v = expr.as_literal()?;
            Some(boolean(Some(v.is_null() != *negated)))
        }
        _ => None,
    }
}

fn simplify_binary(
    e: &BoundExpr,
    op: BinaryOp,
    left: &BoundExpr,
    right: &BoundExpr,
) -> Option<BoundExpr> {
    let typed = |v: Value| BoundExpr::typed_literal(v, e.data_type);
    match op {
        BinaryOp::And => match (as_boolean(left), as_boolean(right)) {
            (Some(Some(false)), _) | (_, Some(Some(false))) => Some(boolean(Some(false))),
            (Some(Some(true)), _) => Some(right.clone()),
            (_, Some(Some(true))) => Some(left.clone()),
            (Some(None), Some(None)) => Some(boolean(None)),
            _ => None,
        },
        BinaryOp::Or => match (as_boolean(left), as_boolean(right)) {
            (Some(Some(true)), _) | (_, Some(Some(true))) => Some(boolean(Some(true))),
            (Some(Some(false)), _) => Some(right.clone()),
            (_, Some(Some(false))) => Some(left.clone()),
            (Some(None), Some(None)) => Some(boolean(None)),
            _ => None,
        },
        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => {
            let (l, r) = (left.as_literal()?, right.as_literal()?);
            if l.is_null() || r.is_null() {
                return Some(boolean(None));
            }
            // Float comparisons (NaN, -0.0) stay with the executor.
            if matches!(l, Value::Float64(_)) || matches!(r, Value::Float64(_)) {
                return None;
            }
            let ord = l.cmp(r);
            Some(boolean(Some(match op {
                BinaryOp::Eq => ord.is_eq(),
                BinaryOp::NotEq => ord.is_ne(),
                BinaryOp::Lt => ord.is_lt(),
                BinaryOp::LtEq => ord.is_le(),
                BinaryOp::Gt => ord.is_gt(),
                _ => ord.is_ge(),
            })))
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => {
            let (l, r) = (left.as_literal()?, right.as_literal()?);
            if l.is_null() || r.is_null() {
                return Some(typed(Value::Null));
            }
            if !e.data_type.is_integer() {
                return None;
            }
            let (a, b) = (l.as_i128()?, r.as_i128()?);
            let v = match op {
                BinaryOp::Add => a.checked_add(b)?,
                BinaryOp::Sub => a.checked_sub(b)?,
                _ => a.checked_mul(b)?,
            };
            Some(typed(cast_value(&Value::Int128(v), e.data_type).ok()?))
        }
        BinaryOp::Div | BinaryOp::Mod => None,
    }
}

// ---------------------------------------------------------------------------------------
// Rule 2: predicate pushdown

/// Splits every `Filter` into conjuncts and moves each as far down as rule 2 allows.
pub fn push_down_predicates(plan: LogicalPlan) -> LogicalPlan {
    push_down(plan, Vec::new())
}

/// Replaces each column reference of `e` (an index into a `Project` output) by the
/// projected expression.
fn substitute(e: &mut BoundExpr, exprs: &[BoundExpr]) {
    let reference = match &e.kind {
        BoundExprKind::Column { index, .. } | BoundExprKind::AggregateRef { index, .. } => {
            Some(*index)
        }
        _ => None,
    };
    match reference {
        Some(index) => *e = exprs[index].clone(),
        None => e
            .children_mut()
            .into_iter()
            .for_each(|c| substitute(c, exprs)),
    }
}

/// `(left side, right side)` of an equality whose sides read only one input each, with the
/// right side rebased on the right input.
fn join_key(p: &BoundExpr, left_width: usize) -> Option<(BoundExpr, BoundExpr)> {
    let BoundExprKind::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = &p.kind
    else {
        return None;
    };
    let side = |e: &BoundExpr| {
        let c = e.columns();
        if c.is_empty() {
            None
        } else if c.iter().all(|&i| i < left_width) {
            Some(true)
        } else if c.iter().all(|&i| i >= left_width) {
            Some(false)
        } else {
            None
        }
    };
    let (l, r) = match (side(left)?, side(right)?) {
        (true, false) => (left, right),
        (false, true) => (right, left),
        _ => return None,
    };
    let mut r = (**r).clone();
    r.map_columns(&mut |c| c - left_width);
    Some(((**l).clone(), r))
}

fn rebase(mut e: BoundExpr, left_width: usize) -> BoundExpr {
    e.map_columns(&mut |c| c - left_width);
    e
}

/// `preds` are conjuncts over the output of `plan` still to be placed.
fn push_down(plan: LogicalPlan, mut preds: Vec<BoundExpr>) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            preds.extend(predicate.into_conjuncts());
            push_down(*input, preds)
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let below = preds
                .into_iter()
                .map(|mut p| {
                    substitute(&mut p, &exprs);
                    p
                })
                .collect();
            LogicalPlan::Project {
                input: Box::new(push_down(*input, below)),
                exprs,
                schema,
            }
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            mut keys,
            residual,
            schema,
        } => {
            let width = left.schema().len();
            let only_left = |p: &BoundExpr| p.columns().iter().all(|&c| c < width);
            let only_right = |p: &BoundExpr| {
                let c = p.columns();
                !c.is_empty() && c.iter().all(|&i| i >= width)
            };
            let mut to_left = Vec::new();
            let mut to_right = Vec::new();
            let mut above = Vec::new();
            let mut kind = kind;
            let mut residual = residual;
            match kind {
                JoinKind::Inner | JoinKind::Cross => {
                    for p in preds {
                        if only_left(&p) {
                            to_left.push(p);
                        } else if only_right(&p) {
                            to_right.push(rebase(p, width));
                        } else if let Some(key) = join_key(&p, width) {
                            keys.push(key);
                        } else {
                            above.push(p);
                        }
                    }
                    if kind == JoinKind::Cross && !keys.is_empty() {
                        kind = JoinKind::Inner;
                    }
                }
                JoinKind::Left => {
                    // From above, only into the preserved side.
                    for p in preds {
                        if only_left(&p) {
                            to_left.push(p);
                        } else {
                            above.push(p);
                        }
                    }
                    // A condition of the ON clause on the right input alone only decides
                    // which right rows can match, so it filters that input.
                    if let Some(r) = residual.take() {
                        let mut kept = Vec::new();
                        for c in r.into_conjuncts() {
                            if only_right(&c) {
                                to_right.push(rebase(c, width));
                            } else {
                                kept.push(c);
                            }
                        }
                        residual = BoundExpr::and_all(kept);
                    }
                }
                JoinKind::Semi | JoinKind::Anti | JoinKind::NullAwareAnti | JoinKind::Single => {
                    for p in preds {
                        if only_left(&p) {
                            to_left.push(p);
                        } else {
                            above.push(p);
                        }
                    }
                }
            }
            let join = LogicalPlan::Join {
                left: Box::new(push_down(*left, to_left)),
                right: Box::new(push_down(*right, to_right)),
                kind,
                keys,
                residual,
                schema,
            };
            with_filter(join, above)
        }
        leaf @ (LogicalPlan::Scan { .. }
        | LogicalPlan::ParquetScan { .. }
        | LogicalPlan::SingleRow) => with_filter(leaf, preds),
        barrier => {
            let plan = map_children(barrier, &mut |child| push_down(child, Vec::new()));
            with_filter(plan, preds)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Rule 4: scan filters

/// Adds to every scan right under a `Filter` the scan filters of its conjuncts.
pub fn derive_scan_filters(plan: LogicalPlan) -> LogicalPlan {
    let plan = map_children(plan, &mut derive_scan_filters);
    let (input, predicate) = match plan {
        LogicalPlan::Filter { input, predicate } => (input, predicate),
        other => return other,
    };
    let input = match *input {
        LogicalPlan::Scan {
            table,
            alias,
            table_schema,
            columns,
            mut filters,
            schema,
        } => {
            add_scan_filters(&predicate, &columns, &mut filters);
            LogicalPlan::Scan {
                table,
                alias,
                table_schema,
                columns,
                filters,
                schema,
            }
        }
        LogicalPlan::ParquetScan {
            path,
            alias,
            file_schema,
            columns,
            mut filters,
            schema,
        } => {
            add_scan_filters(&predicate, &columns, &mut filters);
            LogicalPlan::ParquetScan {
                path,
                alias,
                file_schema,
                columns,
                filters,
                schema,
            }
        }
        other => other,
    };
    LogicalPlan::Filter {
        input: Box::new(input),
        predicate,
    }
}

fn add_scan_filters(predicate: &BoundExpr, columns: &[usize], filters: &mut Vec<ScanFilter>) {
    for conjunct in predicate.clone().into_conjuncts() {
        if let Some(filter) = scan_filter(&conjunct, columns)
            && !filters.contains(&filter)
        {
            filters.push(filter);
        }
    }
}

/// Integer width rank, for the widening casts that keep every value and its order.
fn integer_rank(t: DataType) -> Option<u8> {
    match t {
        DataType::Int16 => Some(1),
        DataType::Int32 => Some(2),
        DataType::Int64 => Some(3),
        DataType::Int128 => Some(4),
        _ => None,
    }
}

/// Whether `CAST(x AS to)` of a `from` value is exact and preserves order, so comparing
/// the cast with a constant is comparing `x` with it.
fn exact_cast(from: DataType, to: DataType) -> bool {
    match (from, to) {
        _ if from == to => true,
        (DataType::Date, DataType::Timestamp) => true,
        (DataType::Decimal { scale: a, .. }, DataType::Decimal { scale: b, .. }) => b >= a,
        (_, DataType::Decimal { .. }) => from.is_integer(),
        _ => match (integer_rank(from), integer_rank(to)) {
            (Some(a), Some(b)) => b >= a,
            _ => false,
        },
    }
}

/// The scan output column `e` reads, through exact casts only.
fn scan_column(e: &BoundExpr) -> Option<usize> {
    match &e.kind {
        BoundExprKind::Column { index, .. } => Some(*index),
        BoundExprKind::Cast(inner) if exact_cast(inner.data_type, e.data_type) => {
            scan_column(inner)
        }
        _ => None,
    }
}

/// The scan filter of one conjunct, over table (or file) schema indices.
pub fn scan_filter(conjunct: &BoundExpr, columns: &[usize]) -> Option<ScanFilter> {
    match &conjunct.kind {
        BoundExprKind::Binary { op, left, right } => {
            let (column, value, op) = match (scan_column(left), right.as_literal()) {
                (Some(c), Some(v)) => (c, v, *op),
                _ => {
                    let flipped = match op {
                        BinaryOp::Lt => BinaryOp::Gt,
                        BinaryOp::LtEq => BinaryOp::GtEq,
                        BinaryOp::Gt => BinaryOp::Lt,
                        BinaryOp::GtEq => BinaryOp::LtEq,
                        other => *other,
                    };
                    (scan_column(right)?, left.as_literal()?, flipped)
                }
            };
            let column = *columns.get(column)?;
            let v = value.clone();
            let (lower, upper) = match op {
                BinaryOp::Eq => return Some(ScanFilter::equal(column, v)),
                BinaryOp::Lt => (Bound::Unbounded, Bound::Excluded(v)),
                BinaryOp::LtEq => (Bound::Unbounded, Bound::Included(v)),
                BinaryOp::Gt => (Bound::Excluded(v), Bound::Unbounded),
                BinaryOp::GtEq => (Bound::Included(v), Bound::Unbounded),
                _ => return None,
            };
            Some(ScanFilter::Range {
                column,
                lower,
                upper,
            })
        }
        BoundExprKind::InList {
            expr,
            list,
            negated: false,
        } => {
            let column = *columns.get(scan_column(expr)?)?;
            let values = list
                .iter()
                .map(|e| e.as_literal().cloned())
                .collect::<Option<Vec<Value>>>()?;
            Some(ScanFilter::In { column, values })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// Rule 5: column pruning

/// Drops every column nothing above uses, down to the scans. The output of the plan keeps
/// all its columns.
pub fn prune_columns(plan: LogicalPlan) -> LogicalPlan {
    let required: BTreeSet<usize> = (0..plan.schema().len()).collect();
    prune(plan, &required).0
}

fn remap(e: &mut BoundExpr, map: &[Option<usize>]) {
    e.map_columns(&mut |c| map[c].expect("a required column is kept"));
}

/// Position of each kept index of `0..len` among `kept`.
fn position_map(len: usize, kept: &[usize]) -> Vec<Option<usize>> {
    let mut map = vec![None; len];
    for (new, &old) in kept.iter().enumerate() {
        map[old] = Some(new);
    }
    map
}

fn expr_columns<'a>(exprs: impl IntoIterator<Item = &'a BoundExpr>, out: &mut BTreeSet<usize>) {
    for e in exprs {
        out.extend(e.columns());
    }
}

/// The scan columns to keep: the required ones or, when none is, the narrowest one, so the
/// scan still produces its rows.
fn kept_scan_columns(required: &BTreeSet<usize>, schema: &Schema) -> Vec<usize> {
    let kept: Vec<usize> = required.iter().copied().collect();
    if !kept.is_empty() || schema.is_empty() {
        return kept;
    }
    let width = |f: &Field| f.data_type.fixed_width().unwrap_or(usize::MAX);
    let narrowest = (0..schema.len())
        .min_by_key(|&i| width(schema.field(i)))
        .unwrap_or(0);
    vec![narrowest]
}

fn project_schema(schema: &Schema, kept: &[usize]) -> Schema {
    Schema::new(kept.iter().map(|&i| schema.field(i).clone()).collect())
}

/// Prunes `plan` so that its output holds at least the `required` columns. Returns the new
/// plan and the new position of every old output column (`None` if dropped).
fn prune(plan: LogicalPlan, required: &BTreeSet<usize>) -> (LogicalPlan, Vec<Option<usize>>) {
    match plan {
        LogicalPlan::Scan {
            table,
            alias,
            table_schema,
            columns,
            filters,
            schema,
        } => {
            let kept = kept_scan_columns(required, &schema);
            let map = position_map(columns.len(), &kept);
            let plan = LogicalPlan::Scan {
                table,
                alias,
                table_schema,
                columns: kept.iter().map(|&i| columns[i]).collect(),
                filters,
                schema: project_schema(&schema, &kept),
            };
            (plan, map)
        }
        LogicalPlan::ParquetScan {
            path,
            alias,
            file_schema,
            columns,
            filters,
            schema,
        } => {
            let kept = kept_scan_columns(required, &schema);
            let map = position_map(columns.len(), &kept);
            let plan = LogicalPlan::ParquetScan {
                path,
                alias,
                file_schema,
                columns: kept.iter().map(|&i| columns[i]).collect(),
                filters,
                schema: project_schema(&schema, &kept),
            };
            (plan, map)
        }
        LogicalPlan::SingleRow => (LogicalPlan::SingleRow, Vec::new()),
        LogicalPlan::Filter {
            input,
            mut predicate,
        } => {
            let mut needed = required.clone();
            needed.extend(predicate.columns());
            let (input, map) = prune(*input, &needed);
            remap(&mut predicate, &map);
            (LogicalPlan::filter(input, predicate), map)
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let mut kept: Vec<usize> = required.iter().copied().collect();
            if kept.is_empty() && !exprs.is_empty() {
                // Keep one output, a plain column if there is one, so rows still flow.
                let cheap = exprs
                    .iter()
                    .position(|e| matches!(e.kind, BoundExprKind::Column { .. }))
                    .unwrap_or(0);
                kept.push(cheap);
            }
            let mut needed = BTreeSet::new();
            expr_columns(kept.iter().map(|&i| &exprs[i]), &mut needed);
            let (input, child_map) = prune(*input, &needed);
            let mut new_exprs = Vec::with_capacity(kept.len());
            for &i in &kept {
                let mut e = exprs[i].clone();
                remap(&mut e, &child_map);
                new_exprs.push(e);
            }
            let plan = LogicalPlan::Project {
                input: Box::new(input),
                exprs: new_exprs,
                schema: project_schema(&schema, &kept),
            };
            (plan, position_map(schema.len(), &kept))
        }
        LogicalPlan::Aggregate {
            input,
            mut groups,
            aggregates,
            schema,
        } => {
            let g = groups.len();
            let mut kept_aggs: Vec<usize> = (0..aggregates.len())
                .filter(|j| required.contains(&(g + j)))
                .collect();
            if g == 0 && kept_aggs.is_empty() && !aggregates.is_empty() {
                kept_aggs.push(0);
            }
            let mut needed = BTreeSet::new();
            expr_columns(groups.iter(), &mut needed);
            expr_columns(
                kept_aggs.iter().filter_map(|&j| aggregates[j].arg.as_ref()),
                &mut needed,
            );
            let (input, child_map) = prune(*input, &needed);
            groups.iter_mut().for_each(|e| remap(e, &child_map));
            let mut new_aggs = Vec::with_capacity(kept_aggs.len());
            for &j in &kept_aggs {
                let mut agg = aggregates[j].clone();
                if let Some(arg) = agg.arg.as_mut() {
                    remap(arg, &child_map);
                }
                new_aggs.push(agg);
            }
            let kept: Vec<usize> = (0..g).chain(kept_aggs.iter().map(|j| g + j)).collect();
            let plan = LogicalPlan::Aggregate {
                input: Box::new(input),
                groups,
                aggregates: new_aggs,
                schema: project_schema(&schema, &kept),
            };
            (plan, position_map(schema.len(), &kept))
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            mut keys,
            mut residual,
            schema,
        } => {
            let width = left.schema().len();
            let mut left_needed: BTreeSet<usize> =
                required.iter().copied().filter(|&c| c < width).collect();
            let mut right_needed: BTreeSet<usize> = required
                .iter()
                .filter(|&&c| c >= width)
                .map(|&c| c - width)
                .collect();
            for (l, r) in &keys {
                left_needed.extend(l.columns());
                right_needed.extend(r.columns());
            }
            if let Some(r) = &residual {
                for c in r.columns() {
                    if c < width {
                        left_needed.insert(c);
                    } else {
                        right_needed.insert(c - width);
                    }
                }
            }
            let (left, left_map) = prune(*left, &left_needed);
            let (right, right_map) = prune(*right, &right_needed);
            let new_width = left.schema().len();
            for (l, r) in keys.iter_mut() {
                remap(l, &left_map);
                remap(r, &right_map);
            }
            let combined: Vec<Option<usize>> = left_map
                .iter()
                .copied()
                .chain(right_map.iter().map(|p| p.map(|p| p + new_width)))
                .collect();
            if let Some(r) = residual.as_mut() {
                remap(r, &combined);
            }
            let map = match kind {
                JoinKind::Semi | JoinKind::Anti | JoinKind::NullAwareAnti => left_map,
                _ => combined,
            };
            debug_assert_eq!(map.len(), schema.len());
            (LogicalPlan::join(left, right, kind, keys, residual), map)
        }
        LogicalPlan::Sort { input, mut keys } => {
            let mut needed = required.clone();
            expr_columns(keys.iter().map(|k| &k.expr), &mut needed);
            let (input, map) = prune(*input, &needed);
            keys.iter_mut().for_each(|k| remap(&mut k.expr, &map));
            (LogicalPlan::sort(input, keys), map)
        }
        LogicalPlan::TopN {
            input,
            mut keys,
            limit,
            offset,
        } => {
            let mut needed = required.clone();
            expr_columns(keys.iter().map(|k| &k.expr), &mut needed);
            let (input, map) = prune(*input, &needed);
            keys.iter_mut().for_each(|k| remap(&mut k.expr, &map));
            let plan = LogicalPlan::TopN {
                input: Box::new(input),
                keys,
                limit,
                offset,
            };
            (plan, map)
        }
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let (input, map) = prune(*input, required);
            (LogicalPlan::limit(input, limit, offset), map)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Rule 6: TopN

/// `Limit n` directly over a `Sort` becomes one `TopN`.
pub fn sort_limit_to_top_n(plan: LogicalPlan) -> LogicalPlan {
    let plan = map_children(plan, &mut sort_limit_to_top_n);
    match plan {
        LogicalPlan::Limit {
            input,
            limit: Some(limit),
            offset,
        } => match *input {
            LogicalPlan::Sort { input, keys } => LogicalPlan::TopN {
                input,
                keys,
                limit,
                offset,
            },
            other => LogicalPlan::limit(other, Some(limit), offset),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests;
