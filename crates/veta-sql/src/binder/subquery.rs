//! Unnesting of subqueries inside expressions (contract §10.4, §19 decision 27).
//!
//! A subquery is bound with the `FROM` scope of the query right above as its outer scope.
//! When its `WHERE` references no outer column it is planned whole ([`Subquery::Plain`]);
//! otherwise the conjuncts that do are pulled out of it and become the keys and the
//! residual of the join that replaces it:
//!
//! - `[NOT] EXISTS (q)` → `Semi`/`Anti` join; the equalities between the subquery and the
//!   outer query are the keys and the other correlated conjuncts the residual. Without one
//!   such equality the correlation is unsupported.
//! - `x IN (q)` → `Semi` join with the key `x = q.col` (plus the correlation, as above).
//! - `x NOT IN (q)` → `NullAwareAnti` join with the key `x = q.col`; only uncorrelated.
//! - Uncorrelated scalar subquery → `Single` join without keys: it is evaluated once and
//!   more than one row is an error when the join runs.
//! - Correlated scalar subquery with aggregates and without `GROUP BY` → `Aggregate`
//!   grouped by the subquery side of its correlation equalities, then a `Single` join on
//!   those columns. An outer row without a group sees every aggregate as computed over no
//!   rows: `COUNT` is 0 and the others are NULL, and the select expression is evaluated
//!   with those values.
//!
//! The value of a scalar subquery is one more column after the columns of the plan it is
//! joined to; the expression that contained it reads that column by its `$subqueryN` name.

use veta_core::{DataType, Result, Value, VetaError};

use super::{
    Binder, FromWhere, Scope, ScopeColumn, bind_expr, finalize_aggregated, has_aggregate, ident,
    map_ast, require_type, split_keys, unify,
};
use crate::ast::{BinaryOp, Expr, Query, SelectItem, UnaryOp};
use crate::expr::{AggFunc, BoundExpr, BoundExprKind};
use crate::plan::{JoinKind, LogicalPlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubqueryKind {
    Exists,
    In,
    Scalar,
}

/// A bound subquery, ready to become the right input of a join.
pub(super) enum Subquery {
    /// Uncorrelated: the whole subquery plan.
    Plain(LogicalPlan),
    /// Correlated `EXISTS` or `IN` subquery: its `FROM` with the local `WHERE` applied,
    /// the `IN` select expression over it, and the correlated conjuncts over
    /// `plan ++ outer FROM`.
    Correlated {
        plan: LogicalPlan,
        item: Option<BoundExpr>,
        correlated: Vec<BoundExpr>,
    },
    /// Correlated scalar subquery: an `Aggregate` grouped by the subquery side of the
    /// correlation, the join keys `(outer expression, group column)`, the select
    /// expression over the `Aggregate` output and the output indices of its `COUNT`s.
    Grouped {
        plan: LogicalPlan,
        keys: Vec<(BoundExpr, BoundExpr)>,
        value: BoundExpr,
        counts: Vec<usize>,
    },
}

impl Subquery {
    pub(super) fn is_plain(&self) -> bool {
        matches!(self, Subquery::Plain(_))
    }

    /// Type of the value of a scalar subquery.
    pub(super) fn scalar_type(&self) -> Result<DataType> {
        match self {
            Subquery::Plain(plan) => {
                let schema = plan.schema();
                if schema.len() != 1 {
                    return Err(VetaError::user(format!(
                        "scalar subquery must return one column, got {}",
                        schema.len()
                    )));
                }
                Ok(schema.field(0).data_type)
            }
            Subquery::Grouped { value, .. } => Ok(value.data_type),
            Subquery::Correlated { .. } => Err(VetaError::internal(
                "correlated scalar subquery without grouping",
            )),
        }
    }
}

/// `[NOT] EXISTS (q)`, `x [NOT] IN (q)`, or the `NOT` of one of them.
pub(super) struct SemiForm<'e> {
    pub(super) query: &'e Query,
    /// The left operand of `IN`; `None` for `EXISTS`.
    pub(super) value: Option<&'e Expr>,
    pub(super) negated: bool,
}

impl SemiForm<'_> {
    pub(super) fn kind(&self) -> SubqueryKind {
        if self.value.is_some() {
            SubqueryKind::In
        } else {
            SubqueryKind::Exists
        }
    }
}

pub(super) fn semi_form(e: &Expr) -> Option<SemiForm<'_>> {
    match e {
        Expr::Exists { query, negated } => Some(SemiForm {
            query: &**query,
            value: None,
            negated: *negated,
        }),
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Some(SemiForm {
            query: &**query,
            value: Some(&**expr),
            negated: *negated,
        }),
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => semi_form(expr).map(|f| SemiForm {
            negated: !f.negated,
            ..f
        }),
        _ => None,
    }
}

/// The conjuncts of a syntax tree of nested `AND`s.
pub(super) fn ast_conjuncts(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut out = ast_conjuncts(left);
            out.extend(ast_conjuncts(right));
            out
        }
        _ => vec![e],
    }
}

/// Whether `e` contains a subquery (not counting those nested inside another subquery).
pub(super) fn has_subquery(e: &Expr) -> bool {
    let mut found = false;
    map_ast(e, &mut |x| {
        if matches!(
            x,
            Expr::Exists { .. } | Expr::InSubquery { .. } | Expr::ScalarSubquery(_)
        ) {
            found = true;
        }
        None
    });
    found
}

/// Whether `e` reads an unqualified column called one of `names`.
pub(super) fn mentions(e: &Expr, names: &[String]) -> bool {
    let mut found = false;
    map_ast(e, &mut |x| {
        if let Expr::Column { table: None, name } = x
            && names.contains(name)
        {
            found = true;
        }
        None
    });
    found
}

/// After the scalar subqueries are lifted, any subquery left is an `IN` or `EXISTS` where
/// no semi join can replace it.
pub(super) fn reject_subqueries(e: &Expr, place: &str) -> Result<()> {
    if has_subquery(e) {
        return Err(VetaError::unsupported(format!(
            "IN or EXISTS subquery in {place}"
        )));
    }
    Ok(())
}

/// The scope column that holds the value of a scalar subquery.
pub(super) fn subquery_column(name: &str, data_type: DataType) -> ScopeColumn {
    ScopeColumn {
        qualifier: String::new(),
        name: name.to_string(),
        data_type,
        nullable: true,
    }
}

impl Binder<'_> {
    /// Binds `q` as a subquery of the query whose `FROM` columns are `outer`.
    pub(super) fn subquery(
        &mut self,
        q: &Query,
        outer: &Scope,
        kind: SubqueryKind,
    ) -> Result<Subquery> {
        let outer = Scope {
            columns: outer.columns.clone(),
            ..Scope::default()
        };
        self.with_outer(Some(outer), |b| {
            b.with_ctes(q, |b| {
                let body = b.from_where(&q.body)?;
                if body.correlated.is_empty() {
                    return Ok(Subquery::Plain(b.select(q, body)?));
                }
                unnest(q, body, kind)
            })
        })
    }

    /// Applies the `WHERE` conjunct `c`, which contains subqueries, on top of `plan`, whose
    /// columns are `scope`. The result has the same columns.
    pub(super) fn where_subquery(
        &mut self,
        plan: LogicalPlan,
        scope: &Scope,
        c: &Expr,
    ) -> Result<LogicalPlan> {
        if let Some(form) = semi_form(c) {
            let value = match form.value {
                Some(v) => {
                    if has_subquery(v) {
                        return Err(VetaError::unsupported("subquery on the left of IN"));
                    }
                    Some(bind_expr(v, scope, None)?)
                }
                None => None,
            };
            let sub = self.subquery(form.query, scope, form.kind())?;
            return attach_semi(plan, sub, value, form.negated);
        }
        let mut lifted = Vec::new();
        let c = self.lift_scalars(c, &mut lifted);
        reject_subqueries(&c, "WHERE outside a conjunct")?;
        let mut plan = plan;
        let mut joined_scope = scope.clone();
        for (name, q) in &lifted {
            let sub = self.subquery(q, scope, SubqueryKind::Scalar)?;
            let (joined, column) = attach_scalar(plan, sub, &joined_scope, name)?;
            plan = joined;
            joined_scope.columns.push(column);
        }
        let predicate = require_type(
            bind_expr(&c, &joined_scope, None)?,
            DataType::Boolean,
            "WHERE",
        )?;
        Ok(keep_columns(LogicalPlan::filter(plan, predicate), scope))
    }

    /// `e` with each scalar subquery (not nested in another subquery) replaced by an
    /// unqualified column `$subqueryN`; the subqueries are appended to `lifted` under those
    /// names.
    pub(super) fn lift_scalars(&mut self, e: &Expr, lifted: &mut Vec<(String, Query)>) -> Expr {
        let counter = &mut self.subqueries;
        map_ast(e, &mut |x| match x {
            Expr::ScalarSubquery(q) => {
                *counter += 1;
                let name = format!("$subquery{}", *counter);
                lifted.push((name.clone(), (**q).clone()));
                Some(Expr::Column { table: None, name })
            }
            _ => None,
        })
    }
}

/// The correlated forms of §10.4; `body` has at least one correlated conjunct.
fn unnest(q: &Query, body: FromWhere, kind: SubqueryKind) -> Result<Subquery> {
    let s = &q.body;
    if !s.group_by.is_empty()
        || s.having.is_some()
        || s.distinct
        || !q.order_by.is_empty()
        || q.limit.is_some()
        || q.offset.is_some()
    {
        return Err(VetaError::unsupported(
            "correlated subquery with GROUP BY, HAVING, DISTINCT, ORDER BY, LIMIT or OFFSET",
        ));
    }
    let FromWhere {
        plan,
        scope,
        correlated,
    } = body;
    if kind == SubqueryKind::Exists {
        return Ok(Subquery::Correlated {
            plan,
            item: None,
            correlated,
        });
    }
    let item = match s.items.as_slice() {
        [SelectItem::Expr { expr, .. }] => expr,
        _ => return Err(VetaError::user("subquery must return one column")),
    };
    if has_subquery(item) {
        return Err(VetaError::unsupported(
            "correlated subquery with a subquery in its select list",
        ));
    }
    if kind == SubqueryKind::In {
        if has_aggregate(item) {
            return Err(VetaError::unsupported(
                "correlated IN subquery with aggregates",
            ));
        }
        let item = bind_expr(item, &scope, None)?;
        return Ok(Subquery::Correlated {
            plan,
            item: Some(item),
            correlated,
        });
    }
    if !has_aggregate(item) {
        return Err(VetaError::unsupported(
            "correlated scalar subquery without an aggregate",
        ));
    }
    let width = scope.len();
    let (pairs, rest) = split_keys(correlated, width);
    if let Some(p) = rest.first() {
        return Err(VetaError::unsupported(format!(
            "correlated subquery: {} is not an equality between the subquery and the outer query",
            p.to_sql()
        )));
    }
    let groups: Vec<BoundExpr> = pairs.iter().map(|(inner, _)| inner.clone()).collect();
    let mut aggs = Vec::new();
    let mut value = bind_expr(item, &scope, Some(&mut aggs))?;
    finalize_aggregated(&mut value, &groups, width)?;
    let counts = aggs
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a.func, AggFunc::Count | AggFunc::CountStar))
        .map(|(i, _)| groups.len() + i)
        .collect();
    let keys = pairs
        .into_iter()
        .enumerate()
        .map(|(i, (inner, outer))| {
            let group = BoundExpr::column(i, inner.to_sql(), inner.data_type, inner.nullable);
            (outer, group)
        })
        .collect();
    Ok(Subquery::Grouped {
        plan: LogicalPlan::aggregate(plan, groups, aggs),
        keys,
        value,
        counts,
    })
}

/// Joins `plan` with an `EXISTS` or `IN` subquery; `value` is the left operand of `IN`,
/// bound over `plan`.
pub(super) fn attach_semi(
    plan: LogicalPlan,
    sub: Subquery,
    value: Option<BoundExpr>,
    negated: bool,
) -> Result<LogicalPlan> {
    let kind = match (value.is_some(), negated) {
        (_, false) => JoinKind::Semi,
        (false, true) => JoinKind::Anti,
        (true, true) => JoinKind::NullAwareAnti,
    };
    match sub {
        Subquery::Plain(right) => {
            let mut keys = Vec::new();
            if let Some(v) = value {
                let schema = right.schema();
                if schema.len() != 1 {
                    return Err(VetaError::user(format!(
                        "IN subquery must return one column, got {}",
                        schema.len()
                    )));
                }
                let f = schema.field(0);
                let column = BoundExpr::column(0, output_sql(&right, 0), f.data_type, f.nullable);
                keys.push(key_pair(v, column)?);
            }
            Ok(LogicalPlan::join(plan, right, kind, keys, None))
        }
        Subquery::Correlated {
            plan: right,
            item,
            correlated,
        } => {
            if kind == JoinKind::NullAwareAnti {
                return Err(VetaError::unsupported("correlated NOT IN subquery"));
            }
            let width = plan.schema().len();
            let inner = right.schema().len();
            let (pairs, rest) = split_keys(correlated, inner);
            let mut keys = Vec::new();
            if let (Some(v), Some(item)) = (value, item) {
                keys.push(key_pair(v, item)?);
            }
            keys.extend(
                pairs
                    .into_iter()
                    .map(|(sub_side, outer_side)| (outer_side, sub_side)),
            );
            if keys.is_empty() {
                return Err(VetaError::unsupported(
                    "correlated subquery without an equality between the subquery and the \
                     outer query",
                ));
            }
            // Over `subquery ++ outer FROM`; the join wants `plan ++ subquery`.
            let residual = BoundExpr::and_all(rest).map(|mut r| {
                r.map_columns(&mut |c| if c < inner { width + c } else { c - inner });
                r
            });
            Ok(LogicalPlan::join(plan, right, kind, keys, residual))
        }
        Subquery::Grouped { .. } => Err(VetaError::internal(
            "aggregated subquery under EXISTS or IN",
        )),
    }
}

/// Joins `plan` with a scalar subquery; the output is the columns of `plan` (described by
/// `scope` for a correlated subquery) followed by the value, which the returned scope
/// column names.
pub(super) fn attach_scalar(
    plan: LogicalPlan,
    sub: Subquery,
    scope: &Scope,
    name: &str,
) -> Result<(LogicalPlan, ScopeColumn)> {
    let column = subquery_column(name, sub.scalar_type()?);
    match sub {
        Subquery::Plain(right) => {
            let joined = LogicalPlan::join(plan, right, JoinKind::Single, Vec::new(), None);
            Ok((joined, column))
        }
        Subquery::Grouped {
            plan: right,
            keys,
            value,
            counts,
        } => {
            let width = plan.schema().len();
            let joined = LogicalPlan::join(plan, right, JoinKind::Single, keys, None);
            let schema = joined.schema();
            let mut exprs: Vec<BoundExpr> = (0..width)
                .map(|i| {
                    let f = schema.field(i);
                    BoundExpr::column(i, scope.sql_name(i), f.data_type, f.nullable)
                })
                .collect();
            let mut names: Vec<String> = schema.fields[..width]
                .iter()
                .map(|f| f.name.clone())
                .collect();
            exprs.push(over_single_join(value, width, &counts));
            names.push(name.to_string());
            Ok((LogicalPlan::project(joined, exprs, names), column))
        }
        Subquery::Correlated { .. } => Err(VetaError::internal(
            "correlated scalar subquery without grouping",
        )),
    }
}

/// `plan` reduced to its first `scope.len()` columns, which `scope` describes.
fn keep_columns(plan: LogicalPlan, scope: &Scope) -> LogicalPlan {
    let schema = plan.schema();
    if schema.len() == scope.len() {
        return plan;
    }
    let exprs = (0..scope.len())
        .map(|i| {
            let f = schema.field(i);
            BoundExpr::column(i, scope.sql_name(i), f.data_type, f.nullable)
        })
        .collect();
    let names = schema.fields[..scope.len()]
        .iter()
        .map(|f| f.name.clone())
        .collect();
    LogicalPlan::project(plan, exprs, names)
}

/// Both sides of a join key converted to one type, as in a comparison.
fn key_pair(left: BoundExpr, right: BoundExpr) -> Result<(BoundExpr, BoundExpr)> {
    let mut sides = unify(vec![left, right], "compare")?;
    let right = sides.pop().expect("two sides");
    let left = sides.pop().expect("two sides");
    Ok((left, right))
}

/// SQL text of output column `i` of a subquery plan: the projected expression when there
/// is one (`alias.col`), the column name otherwise.
fn output_sql(plan: &LogicalPlan, i: usize) -> String {
    match plan {
        LogicalPlan::Project { exprs, .. } => exprs[i].to_sql(),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::TopN { input, .. }
        | LogicalPlan::Limit { input, .. } => output_sql(input, i),
        other => ident(&other.schema().field(i).name),
    }
}

/// `value`, bound over the `Aggregate` output of a grouped subquery, re-bound over the
/// output of its `Single` join (`left_width` outer columns first). There every column may
/// be NULL, and a `COUNT` that is NULL had no rows to count, so it reads as 0.
fn over_single_join(mut value: BoundExpr, left_width: usize, counts: &[usize]) -> BoundExpr {
    fn walk(e: &mut BoundExpr, left_width: usize, counts: &[usize]) {
        let target = match &e.kind {
            BoundExprKind::Column { index, name } => Some((*index, name.clone())),
            BoundExprKind::AggregateRef { index, sql } => Some((*index, sql.clone())),
            _ => None,
        };
        if let Some((index, name)) = target {
            let column = BoundExpr::column(left_width + index, name, e.data_type, true);
            *e = if counts.contains(&index) {
                zero_if_null(column)
            } else {
                column
            };
            return;
        }
        if !matches!(e.kind, BoundExprKind::Literal(_)) {
            e.nullable = true;
        }
        for c in e.children_mut() {
            walk(c, left_width, counts);
        }
    }
    walk(&mut value, left_width, counts);
    value
}

/// `CASE WHEN count IS NULL THEN 0 ELSE count END`.
fn zero_if_null(count: BoundExpr) -> BoundExpr {
    let data_type = count.data_type;
    let is_null = BoundExpr::new(
        BoundExprKind::IsNull {
            expr: Box::new(count.clone()),
            negated: false,
        },
        DataType::Boolean,
        false,
    );
    let zero = BoundExpr::typed_literal(Value::Int64(0), data_type);
    BoundExpr::new(
        BoundExprKind::Case {
            branches: vec![(is_null, zero)],
            else_: Some(Box::new(count)),
        },
        data_type,
        false,
    )
}
