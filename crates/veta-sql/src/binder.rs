//! Name resolution, typing and logical plan construction (contract §4, §10, §11.1).
//!
//! The plan of a `SELECT` is built as
//! `FROM (Scan/ParquetScan/Join) → Filter(WHERE) → Aggregate → Filter(HAVING) → Project →
//! [DISTINCT Aggregate] → Sort → Limit → [Project that drops hidden ORDER BY columns]`.
//!
//! Joins: every comma entry and every `INNER`/`CROSS JOIN` of a `FROM` is one relation of a
//! join group; a `LEFT JOIN` is a single relation of its enclosing group. The `ON`
//! predicates of inner joins and the `WHERE` conjuncts form the group's predicate pool.
//! Relations are joined in the connected syntactic order of §11.3 and each equality
//! between the relations already joined and the next one becomes a join key (§11.1); the
//! other predicates stay in a `Filter` above the join tree. When that order differs from
//! the `FROM` order a `Project` restores the `FROM` column order.
//!
//! Subqueries inside expressions are `unsupported: subquery` until K2.

use std::collections::HashSet;

use veta_core::{DataType, Result, Value, VetaError};

use crate::ast::{
    BinaryOp, Expr, Function, JoinKind as AstJoinKind, Literal, Query, SelectItem, TableRef,
    UnaryOp,
};
use crate::catalog::Catalog;
use crate::display::write_ident;
use crate::expr::{
    AggFunc, AggregateExpr, BoundExpr, BoundExprKind, ScalarFunc, can_cast, cast_value,
};
use crate::plan::{JoinKind, LogicalPlan, SortKey};

/// Binds a query against `catalog` and returns its typed logical plan. The output schema
/// names are the select aliases or, without alias, the expression text (§10.1).
pub fn bind_query(catalog: &dyn Catalog, query: &Query) -> Result<LogicalPlan> {
    Binder {
        catalog,
        ctes: Vec::new(),
    }
    .query(query)
}

// ---------------------------------------------------------------------------------------
// Scopes

#[derive(Debug, Clone)]
struct ScopeColumn {
    qualifier: String,
    name: String,
    data_type: DataType,
    nullable: bool,
}

#[derive(Debug, Clone, Default)]
struct Scope {
    columns: Vec<ScopeColumn>,
}

fn ident(name: &str) -> String {
    let mut out = String::new();
    let _ = write_ident(&mut out, name);
    out
}

impl Scope {
    fn from_plan(plan: &LogicalPlan, qualifier: &str, renames: &[String]) -> Scope {
        let schema = plan.schema();
        Scope {
            columns: schema
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| ScopeColumn {
                    qualifier: qualifier.to_string(),
                    name: renames.get(i).cloned().unwrap_or_else(|| f.name.clone()),
                    data_type: f.data_type,
                    nullable: f.nullable,
                })
                .collect(),
        }
    }

    fn len(&self) -> usize {
        self.columns.len()
    }

    fn concat(&self, other: &Scope) -> Scope {
        let mut columns = self.columns.clone();
        columns.extend(other.columns.iter().cloned());
        Scope { columns }
    }

    fn qualifiers(&self) -> HashSet<String> {
        self.columns
            .iter()
            .map(|c| c.qualifier.to_ascii_lowercase())
            .collect()
    }

    /// Index of `[table.]name`; `Ok(None)` when no column matches.
    fn resolve(&self, table: Option<&str>, name: &str) -> Result<Option<usize>> {
        let mut found = None;
        for (i, c) in self.columns.iter().enumerate() {
            let table_matches = table.is_none_or(|t| c.qualifier.eq_ignore_ascii_case(t));
            if table_matches && c.name.eq_ignore_ascii_case(name) {
                if found.is_some() {
                    return Err(VetaError::user(format!(
                        "ambiguous column reference: {}",
                        column_text(table, name)
                    )));
                }
                found = Some(i);
            }
        }
        Ok(found)
    }

    fn sql_name(&self, index: usize) -> String {
        let c = &self.columns[index];
        format!("{}.{}", ident(&c.qualifier), ident(&c.name))
    }

    fn expr(&self, index: usize) -> BoundExpr {
        let c = &self.columns[index];
        BoundExpr::column(index, self.sql_name(index), c.data_type, c.nullable)
    }
}

fn column_text(table: Option<&str>, name: &str) -> String {
    match table {
        Some(t) => format!("{t}.{name}"),
        None => name.to_string(),
    }
}

fn check_disjoint(a: &Scope, b: &Scope) -> Result<()> {
    let left = a.qualifiers();
    for q in b.qualifiers() {
        if left.contains(&q) {
            return Err(VetaError::user(format!("duplicate table alias: {q}")));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Types and coercions (§4)

fn integer_rank(t: DataType) -> Option<u8> {
    match t {
        DataType::Int16 => Some(0),
        DataType::Int32 => Some(1),
        DataType::Int64 => Some(2),
        DataType::Int128 => Some(3),
        _ => None,
    }
}

/// Decimal digits needed by any value of an integer type.
fn integer_digits(t: DataType) -> u8 {
    match t {
        DataType::Int16 => 5,
        DataType::Int32 => 10,
        DataType::Int64 => 19,
        _ => 38,
    }
}

fn cap_precision(p: u32) -> u8 {
    p.min(veta_core::types::MAX_DECIMAL_PRECISION as u32) as u8
}

/// Type both sides of a comparison, `IN` or `CASE` are converted to.
fn common_type(a: DataType, b: DataType) -> Option<DataType> {
    use DataType::*;
    if a == b {
        return Some(a);
    }
    if let (Some(ra), Some(rb)) = (integer_rank(a), integer_rank(b)) {
        return Some(if ra >= rb { a } else { b });
    }
    match (a, b) {
        (Decimal { precision, scale }, i) | (i, Decimal { precision, scale }) if i.is_integer() => {
            let int = (precision - scale).max(integer_digits(i)) as u32;
            Some(Decimal {
                precision: cap_precision(int + scale as u32),
                scale,
            })
        }
        (
            Decimal {
                precision: p1,
                scale: s1,
            },
            Decimal {
                precision: p2,
                scale: s2,
            },
        ) => {
            let scale = s1.max(s2);
            let int = (p1 - s1).max(p2 - s2) as u32;
            Some(Decimal {
                precision: cap_precision(int + scale as u32),
                scale,
            })
        }
        (Float64, x) | (x, Float64) if x.is_numeric() => Some(Float64),
        (Date, Timestamp) | (Timestamp, Date) => Some(Timestamp),
        (Varchar, t @ (Date | Timestamp)) | (t @ (Date | Timestamp), Varchar) => Some(t),
        _ => None,
    }
}

/// The constant `value` converted to `to` when the conversion keeps its meaning: an equal
/// number, or a string read as a date or timestamp (§4).
fn exact_literal(value: &Value, to: DataType) -> Option<Value> {
    let converted = cast_value(value, to).ok()?;
    let from = value.data_type()?;
    let numeric = from.is_numeric() && to.is_numeric();
    let temporal_string =
        from == DataType::Varchar && matches!(to, DataType::Date | DataType::Timestamp);
    let date_widening = from == DataType::Date && to == DataType::Timestamp;
    if temporal_string || date_widening || (numeric && converted == *value) {
        Some(converted)
    } else {
        None
    }
}

/// Converts `e` to type `to`, folding constants and inserting a `Cast` otherwise.
fn coerce(e: BoundExpr, to: DataType) -> Result<BoundExpr> {
    if e.data_type == to {
        return Ok(e);
    }
    if let Some(v) = e.as_literal() {
        let nullable = e.nullable;
        let mut lit = BoundExpr::typed_literal(cast_value(v, to)?, to);
        lit.nullable = nullable;
        return Ok(lit);
    }
    if !can_cast(e.data_type, to) {
        return Err(VetaError::user(format!(
            "cannot convert {} to {}",
            e.data_type.sql_name(),
            to.sql_name()
        )));
    }
    let nullable = e.nullable;
    Ok(BoundExpr::new(
        BoundExprKind::Cast(Box::new(e)),
        to,
        nullable,
    ))
}

/// Converts every expression to one common type. The type comes from the non-constant
/// expressions; a constant only widens it when it does not convert exactly (so
/// `CounterID = 62` stays `Int32` and `EventDate >= '2013-07-01'` compares dates).
fn unify(exprs: Vec<BoundExpr>, what: &str) -> Result<Vec<BoundExpr>> {
    let incompatible = |a: DataType, b: DataType| {
        VetaError::user(format!(
            "cannot {what} {} and {}",
            a.sql_name(),
            b.sql_name()
        ))
    };
    let mut target: Option<DataType> = None;
    for e in exprs.iter().filter(|e| e.as_literal().is_none()) {
        target = Some(match target {
            None => e.data_type,
            Some(t) => common_type(t, e.data_type).ok_or_else(|| incompatible(t, e.data_type))?,
        });
    }
    for e in exprs.iter() {
        let Some(v) = e.as_literal() else { continue };
        if v.is_null() {
            continue;
        }
        target = Some(match target {
            None => e.data_type,
            Some(t) if exact_literal(v, t).is_some() => t,
            Some(DataType::Date)
                if e.data_type == DataType::Varchar
                    && exact_literal(v, DataType::Timestamp).is_some() =>
            {
                DataType::Timestamp
            }
            Some(t) => common_type(t, e.data_type).ok_or_else(|| incompatible(t, e.data_type))?,
        });
    }
    let target = target.unwrap_or(DataType::Int32);
    exprs
        .into_iter()
        .map(|e| {
            let constant = e
                .as_literal()
                .filter(|v| !v.is_null() && v.data_type() != Some(target))
                .cloned();
            match constant {
                Some(v) => {
                    let converted = match exact_literal(&v, target) {
                        Some(c) => c,
                        None => cast_value(&v, target)?,
                    };
                    Ok(BoundExpr::typed_literal(converted, target))
                }
                None => coerce(e, target),
            }
        })
        .collect()
}

fn require_type(e: BoundExpr, to: DataType, context: &str) -> Result<BoundExpr> {
    if e.data_type == to {
        Ok(e)
    } else if e.is_null_literal() {
        Ok(BoundExpr::typed_literal(Value::Null, to))
    } else {
        Err(VetaError::user(format!(
            "{context} must be {}, got {}",
            to.sql_name(),
            e.data_type.sql_name()
        )))
    }
}

fn require_integer(e: BoundExpr, context: &str) -> Result<BoundExpr> {
    if e.data_type.is_integer() || e.is_null_literal() {
        coerce(e, DataType::Int64)
    } else {
        Err(VetaError::user(format!(
            "{context} must be an integer, got {}",
            e.data_type.sql_name()
        )))
    }
}

fn binary(
    op: BinaryOp,
    left: BoundExpr,
    right: BoundExpr,
    data_type: DataType,
    nullable: bool,
) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        data_type,
        nullable,
    )
}

fn not(e: BoundExpr) -> BoundExpr {
    let nullable = e.nullable;
    BoundExpr::new(
        BoundExprKind::Unary {
            op: UnaryOp::Not,
            expr: Box::new(e),
        },
        DataType::Boolean,
        nullable,
    )
}

fn comparison(op: BinaryOp, left: BoundExpr, right: BoundExpr) -> Result<BoundExpr> {
    let mut sides = unify(vec![left, right], "compare")?;
    let right = sides.pop().expect("two sides");
    let left = sides.pop().expect("two sides");
    let nullable = left.nullable || right.nullable;
    Ok(binary(op, left, right, DataType::Boolean, nullable))
}

/// Digits of an integer constant (at least one), its precision as a decimal of scale 0.
fn literal_digits(v: i128) -> u8 {
    let mut digits = 1u8;
    let mut rest = v.unsigned_abs() / 10;
    while rest > 0 {
        digits += 1;
        rest /= 10;
    }
    digits
}

/// An integer operand of decimal arithmetic as `Decimal(p, 0)` (§4).
fn integer_as_decimal(e: BoundExpr) -> Result<BoundExpr> {
    let precision = match e.as_literal().and_then(Value::as_i128) {
        Some(v) => literal_digits(v),
        None => integer_digits(e.data_type),
    };
    coerce(
        e,
        DataType::Decimal {
            precision,
            scale: 0,
        },
    )
}

/// `+ - * / %` with the result types of §4.
fn arithmetic(op: BinaryOp, left: BoundExpr, right: BoundExpr) -> Result<BoundExpr> {
    let (left, right) = match (left.is_null_literal(), right.is_null_literal()) {
        (true, true) => (
            BoundExpr::typed_literal(Value::Null, DataType::Int64),
            BoundExpr::typed_literal(Value::Null, DataType::Int64),
        ),
        (true, false) => (
            BoundExpr::typed_literal(Value::Null, right.data_type),
            right,
        ),
        (false, true) => {
            let t = left.data_type;
            (left, BoundExpr::typed_literal(Value::Null, t))
        }
        (false, false) => (left, right),
    };
    let (lt, rt) = (left.data_type, right.data_type);
    if !lt.is_numeric() || !rt.is_numeric() {
        return Err(VetaError::user(format!(
            "cannot apply {} to {} and {}",
            op.symbol(),
            lt.sql_name(),
            rt.sql_name()
        )));
    }
    let nullable = left.nullable || right.nullable;
    let wide_int = if lt == DataType::Int128 || rt == DataType::Int128 {
        DataType::Int128
    } else {
        DataType::Int64
    };
    match op {
        BinaryOp::Div => {
            let l = coerce(left, DataType::Float64)?;
            let r = coerce(right, DataType::Float64)?;
            Ok(binary(op, l, r, DataType::Float64, true))
        }
        BinaryOp::Mod => {
            if !lt.is_integer() || !rt.is_integer() {
                return Err(VetaError::unsupported(format!(
                    "% on {} and {}",
                    lt.sql_name(),
                    rt.sql_name()
                )));
            }
            let l = coerce(left, wide_int)?;
            let r = coerce(right, wide_int)?;
            Ok(binary(op, l, r, wide_int, true))
        }
        _ if lt.is_integer() && rt.is_integer() => {
            let l = coerce(left, wide_int)?;
            let r = coerce(right, wide_int)?;
            Ok(binary(op, l, r, wide_int, nullable))
        }
        _ if lt == DataType::Float64 || rt == DataType::Float64 => {
            let l = coerce(left, DataType::Float64)?;
            let r = coerce(right, DataType::Float64)?;
            Ok(binary(op, l, r, DataType::Float64, nullable))
        }
        _ => {
            let l = if lt.is_integer() {
                integer_as_decimal(left)?
            } else {
                left
            };
            let r = if rt.is_integer() {
                integer_as_decimal(right)?
            } else {
                right
            };
            let (
                DataType::Decimal {
                    precision: p1,
                    scale: s1,
                },
                DataType::Decimal {
                    precision: p2,
                    scale: s2,
                },
            ) = (l.data_type, r.data_type)
            else {
                return Err(VetaError::internal("decimal arithmetic without decimals"));
            };
            let (precision, scale) = if op == BinaryOp::Mul {
                veta_core::decimal::mul_type(p1, s1, p2, s2)?
            } else {
                veta_core::decimal::add_type(p1, s1, p2, s2)?
            };
            Ok(binary(
                op,
                l,
                r,
                DataType::Decimal { precision, scale },
                nullable,
            ))
        }
    }
}

fn negate_literal(v: &Value) -> Option<Value> {
    Some(match v {
        Value::Int16(x) => Value::Int64(-(*x as i64)),
        Value::Int32(x) => Value::Int64(-(*x as i64)),
        Value::Int64(x) => Value::Int64(x.checked_neg()?),
        Value::Int128(x) => Value::Int128(x.checked_neg()?),
        Value::Float64(x) => Value::Float64(-x),
        Value::Decimal {
            value,
            precision,
            scale,
        } => Value::Decimal {
            value: -value,
            precision: *precision,
            scale: *scale,
        },
        _ => return None,
    })
}

fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Boolean(b) => Value::Boolean(*b),
        Literal::Integer(v) => Value::Int64(*v),
        Literal::Decimal(d) => Value::Decimal {
            value: d.value,
            precision: d.precision,
            scale: d.scale,
        },
        Literal::String(s) => Value::varchar(s),
        Literal::Date(d) => Value::Date(*d),
        Literal::Timestamp(t) => Value::Timestamp(*t),
    }
}

// ---------------------------------------------------------------------------------------
// Expressions

/// Binds `e` over `scope`. With `aggs`, aggregate calls are allowed: each distinct one is
/// added to `aggs` and replaced by an `AggregateRef` whose index is its position there
/// (made final by [`finalize_aggregated`]).
fn bind_expr(
    e: &Expr,
    scope: &Scope,
    mut aggs: Option<&mut Vec<AggregateExpr>>,
) -> Result<BoundExpr> {
    let sub = |e: &Expr| bind_expr(e, scope, None);
    Ok(match e {
        Expr::Literal(lit) => BoundExpr::literal(literal_value(lit)),
        Expr::Column { table, name } => match scope.resolve(table.as_deref(), name)? {
            Some(i) => scope.expr(i),
            None => {
                return Err(VetaError::user(format!(
                    "column not found: {}",
                    column_text(table.as_deref(), name)
                )));
            }
        },
        Expr::Unary { op, expr } => {
            let inner = bind_expr(expr, scope, aggs)?;
            match op {
                UnaryOp::Not => not(require_type(inner, DataType::Boolean, "NOT operand")?),
                UnaryOp::Neg => {
                    if let Some(v) = inner.as_literal().and_then(negate_literal) {
                        return Ok(BoundExpr::literal(v));
                    }
                    if inner.is_null_literal() {
                        return Ok(BoundExpr::typed_literal(Value::Null, DataType::Int64));
                    }
                    let t = inner.data_type;
                    let operand = match t {
                        DataType::Int16 | DataType::Int32 => coerce(inner, DataType::Int64)?,
                        _ if t.is_numeric() => inner,
                        _ => {
                            return Err(VetaError::user(format!("cannot negate {}", t.sql_name())));
                        }
                    };
                    let (dt, nullable) = (operand.data_type, operand.nullable);
                    BoundExpr::new(
                        BoundExprKind::Unary {
                            op: UnaryOp::Neg,
                            expr: Box::new(operand),
                        },
                        dt,
                        nullable,
                    )
                }
            }
        }
        Expr::Binary { op, left, right } => {
            let l = bind_expr(left, scope, aggs.as_deref_mut())?;
            let r = bind_expr(right, scope, aggs)?;
            match op {
                BinaryOp::And | BinaryOp::Or => {
                    let l = require_type(l, DataType::Boolean, op.symbol())?;
                    let r = require_type(r, DataType::Boolean, op.symbol())?;
                    let nullable = l.nullable || r.nullable;
                    binary(*op, l, r, DataType::Boolean, nullable)
                }
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq => comparison(*op, l, r)?,
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                    arithmetic(*op, l, r)?
                }
            }
        }
        Expr::IsNull { expr, negated } => {
            let inner = bind_expr(expr, scope, aggs)?;
            BoundExpr::new(
                BoundExprKind::IsNull {
                    expr: Box::new(inner),
                    negated: *negated,
                },
                DataType::Boolean,
                false,
            )
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = bind_expr(expr, scope, aggs.as_deref_mut())?;
            let lo = bind_expr(low, scope, aggs.as_deref_mut())?;
            let hi = bind_expr(high, scope, aggs)?;
            let ge = comparison(BinaryOp::GtEq, v.clone(), lo)?;
            let le = comparison(BinaryOp::LtEq, v, hi)?;
            let nullable = ge.nullable || le.nullable;
            let both = binary(BinaryOp::And, ge, le, DataType::Boolean, nullable);
            if *negated { not(both) } else { both }
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let mut all = vec![bind_expr(expr, scope, aggs.as_deref_mut())?];
            for item in list {
                all.push(bind_expr(item, scope, aggs.as_deref_mut())?);
            }
            let mut all = unify(all, "compare")?;
            let value = all.remove(0);
            let nullable = value.nullable || all.iter().any(|e| e.nullable);
            BoundExpr::new(
                BoundExprKind::InList {
                    expr: Box::new(value),
                    list: all,
                    negated: *negated,
                },
                DataType::Boolean,
                nullable,
            )
        }
        Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::ScalarSubquery(_) => {
            return Err(VetaError::unsupported("subquery"));
        }
        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = require_type(
                bind_expr(expr, scope, aggs.as_deref_mut())?,
                DataType::Varchar,
                "LIKE operand",
            )?;
            let p = require_type(
                bind_expr(pattern, scope, aggs)?,
                DataType::Varchar,
                "LIKE pattern",
            )?;
            let nullable = v.nullable || p.nullable;
            BoundExpr::new(
                BoundExprKind::Like {
                    expr: Box::new(v),
                    pattern: Box::new(p),
                    negated: *negated,
                },
                DataType::Boolean,
                nullable,
            )
        }
        Expr::Case { branches, else_ } => {
            let mut conditions = Vec::new();
            let mut values = Vec::new();
            for (cond, value) in branches {
                let c = bind_expr(cond, scope, aggs.as_deref_mut())?;
                conditions.push(require_type(c, DataType::Boolean, "CASE condition")?);
                values.push(bind_expr(value, scope, aggs.as_deref_mut())?);
            }
            let has_else = else_.is_some();
            if let Some(e) = else_ {
                values.push(bind_expr(e, scope, aggs)?);
            }
            let mut values = unify(values, "mix in CASE")?;
            let else_ = if has_else { values.pop() } else { None };
            let data_type = values[0].data_type;
            let nullable = !has_else || values.iter().chain(else_.iter()).any(|v| v.nullable);
            BoundExpr::new(
                BoundExprKind::Case {
                    branches: conditions.into_iter().zip(values).collect(),
                    else_: else_.map(Box::new),
                },
                data_type,
                nullable,
            )
        }
        Expr::Cast { expr, data_type } => {
            let inner = bind_expr(expr, scope, aggs)?;
            if inner.is_null_literal() {
                return Ok(BoundExpr::typed_literal(Value::Null, *data_type));
            }
            if !can_cast(inner.data_type, *data_type) {
                return Err(VetaError::user(format!(
                    "cannot cast {} to {}",
                    inner.data_type.sql_name(),
                    data_type.sql_name()
                )));
            }
            coerce(inner, *data_type)?
        }
        Expr::Extract { part, expr } => {
            let inner = bind_expr(expr, scope, aggs)?;
            match inner.data_type {
                DataType::Timestamp => {}
                DataType::Date => {
                    use veta_core::datetime::DatePart;
                    if matches!(part, DatePart::Hour | DatePart::Minute | DatePart::Second) {
                        return Err(VetaError::unsupported(format!(
                            "extract({} FROM DATE)",
                            part.name()
                        )));
                    }
                }
                t => {
                    return Err(VetaError::user(format!(
                        "extract needs DATE or TIMESTAMP, got {}",
                        t.sql_name()
                    )));
                }
            }
            let nullable = inner.nullable;
            BoundExpr::new(
                BoundExprKind::Function {
                    func: ScalarFunc::Extract(*part),
                    args: vec![inner],
                },
                DataType::Int64,
                nullable,
            )
        }
        Expr::Substring { expr, from, length } => {
            let s = require_type(
                bind_expr(expr, scope, aggs.as_deref_mut())?,
                DataType::Varchar,
                "substring argument",
            )?;
            let mut args = vec![s];
            args.push(require_integer(
                bind_expr(from, scope, aggs.as_deref_mut())?,
                "substring position",
            )?);
            if let Some(length) = length {
                args.push(require_integer(
                    bind_expr(length, scope, aggs)?,
                    "substring length",
                )?);
            }
            let nullable = args.iter().any(|a| a.nullable);
            BoundExpr::new(
                BoundExprKind::Function {
                    func: ScalarFunc::Substring,
                    args,
                },
                DataType::Varchar,
                nullable,
            )
        }
        Expr::DateTrunc { unit, expr } => {
            let inner = bind_expr(expr, scope, aggs)?;
            let t = inner.data_type;
            if !matches!(t, DataType::Date | DataType::Timestamp) {
                return Err(VetaError::user(format!(
                    "date_trunc needs DATE or TIMESTAMP, got {}",
                    t.sql_name()
                )));
            }
            let nullable = inner.nullable;
            BoundExpr::new(
                BoundExprKind::Function {
                    func: ScalarFunc::DateTrunc(*unit),
                    args: vec![inner],
                },
                t,
                nullable,
            )
        }
        Expr::CountStar => {
            let aggs = aggs.ok_or_else(aggregate_not_allowed)?;
            register_aggregate(aggs, AggregateExpr::new(AggFunc::CountStar, false, None)?)?
        }
        Expr::Function {
            func,
            distinct,
            args,
        } => {
            let (min, max) = func.arity();
            if args.len() < min || args.len() > max {
                return Err(VetaError::user(format!(
                    "{} takes {} argument(s), got {}",
                    func.name(),
                    min,
                    args.len()
                )));
            }
            if func.is_aggregate() {
                let aggs = aggs.ok_or_else(aggregate_not_allowed)?;
                let arg = sub(&args[0])?;
                let agg_func = match func {
                    Function::Count => AggFunc::Count,
                    Function::Sum => AggFunc::Sum,
                    Function::Avg => AggFunc::Avg,
                    Function::Min => AggFunc::Min,
                    _ => AggFunc::Max,
                };
                let agg = AggregateExpr::new(agg_func, *distinct, Some(arg))?;
                return register_aggregate(aggs, agg);
            }
            if *distinct {
                return Err(VetaError::user(format!(
                    "DISTINCT is only allowed in aggregates, not in {}",
                    func.name()
                )));
            }
            let mut bound = Vec::new();
            for a in args {
                let b = bind_expr(a, scope, aggs.as_deref_mut())?;
                bound.push(require_type(b, DataType::Varchar, func.name())?);
            }
            let nullable = bound.iter().any(|a| a.nullable);
            let (scalar, data_type) = match func {
                Function::Length => (ScalarFunc::Length, DataType::Int64),
                Function::Strlen => (ScalarFunc::Strlen, DataType::Int64),
                _ => (ScalarFunc::RegexpReplace, DataType::Varchar),
            };
            BoundExpr::new(
                BoundExprKind::Function {
                    func: scalar,
                    args: bound,
                },
                data_type,
                nullable,
            )
        }
    })
}

fn aggregate_not_allowed() -> VetaError {
    VetaError::user("aggregate functions are not allowed here")
}

fn register_aggregate(aggs: &mut Vec<AggregateExpr>, agg: AggregateExpr) -> Result<BoundExpr> {
    let index = match aggs.iter().position(|a| *a == agg) {
        Some(i) => i,
        None => {
            aggs.push(agg.clone());
            aggs.len() - 1
        }
    };
    Ok(BoundExpr::new(
        BoundExprKind::AggregateRef {
            index,
            sql: agg.to_sql(),
        },
        agg.data_type,
        agg.nullable,
    ))
}

/// Rewrites an expression bound over the `FROM` scope into one over the `Aggregate`
/// output (groups, then aggregates): subexpressions equal to a group become that group
/// column, aggregate references move past the groups, and any other column is an error.
fn finalize_aggregated(e: &mut BoundExpr, groups: &[BoundExpr]) -> Result<()> {
    if let Some(i) = groups.iter().position(|g| *g == *e) {
        *e = BoundExpr::column(
            i,
            groups[i].to_sql(),
            groups[i].data_type,
            groups[i].nullable,
        );
        return Ok(());
    }
    if let BoundExprKind::AggregateRef { index, .. } = &mut e.kind {
        *index += groups.len();
        return Ok(());
    }
    if let BoundExprKind::Column { name, .. } = &e.kind {
        return Err(VetaError::user(format!(
            "column {name} must appear in GROUP BY or be used in an aggregate function"
        )));
    }
    e.children_mut()
        .into_iter()
        .try_for_each(|c| finalize_aggregated(c, groups))
}

/// Copy of `e` where `f` replaces the subexpressions it returns `Some` for.
fn map_ast(e: &Expr, f: &mut dyn FnMut(&Expr) -> Option<Expr>) -> Expr {
    if let Some(replaced) = f(e) {
        return replaced;
    }
    let mut m = |x: &Expr| Box::new(map_ast(x, f));
    match e {
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::CountStar
        | Expr::Exists { .. }
        | Expr::ScalarSubquery(_) => e.clone(),
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: m(expr),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: m(left),
            right: m(right),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: m(expr),
            negated: *negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: m(expr),
            low: m(low),
            high: m(high),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: m(expr),
            list: list.iter().map(|x| *m(x)).collect(),
            negated: *negated,
        },
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: m(expr),
            query: query.clone(),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: m(expr),
            pattern: m(pattern),
            negated: *negated,
        },
        Expr::Case { branches, else_ } => Expr::Case {
            branches: branches.iter().map(|(c, v)| (*m(c), *m(v))).collect(),
            else_: else_.as_ref().map(|x| m(x)),
        },
        Expr::Cast { expr, data_type } => Expr::Cast {
            expr: m(expr),
            data_type: *data_type,
        },
        Expr::Extract { part, expr } => Expr::Extract {
            part: *part,
            expr: m(expr),
        },
        Expr::Substring { expr, from, length } => Expr::Substring {
            expr: m(expr),
            from: m(from),
            length: length.as_ref().map(|x| m(x)),
        },
        Expr::DateTrunc { unit, expr } => Expr::DateTrunc {
            unit: *unit,
            expr: m(expr),
        },
        Expr::Function {
            func,
            distinct,
            args,
        } => Expr::Function {
            func: *func,
            distinct: *distinct,
            args: args.iter().map(|x| *m(x)).collect(),
        },
    }
}

fn has_aggregate(e: &Expr) -> bool {
    let mut found = false;
    map_ast(e, &mut |x| {
        if matches!(x, Expr::CountStar)
            || matches!(x, Expr::Function { func, .. } if func.is_aggregate())
        {
            found = true;
        }
        None
    });
    found
}

/// Replaces unqualified names that are select aliases by the aliased expression. With
/// `prefer_column` a name that is also a `FROM` column keeps meaning the column (GROUP BY
/// and HAVING); otherwise the alias wins (ORDER BY).
fn substitute_aliases(
    e: &Expr,
    aliases: &[(String, Expr)],
    scope: &Scope,
    prefer_column: bool,
) -> Expr {
    map_ast(e, &mut |x| match x {
        Expr::Column { table: None, name } => {
            let (_, aliased) = aliases.iter().find(|(a, _)| a.eq_ignore_ascii_case(name))?;
            if prefer_column && !matches!(scope.resolve(None, name), Ok(None)) {
                return None;
            }
            Some(aliased.clone())
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------
// Relations and joins

struct Relation {
    plan: LogicalPlan,
    scope: Scope,
}

fn collect_relations<'q>(t: &'q TableRef, rels: &mut Vec<&'q TableRef>, ons: &mut Vec<&'q Expr>) {
    match t {
        TableRef::Join {
            left,
            right,
            kind: AstJoinKind::Inner | AstJoinKind::Cross,
            on,
        } => {
            collect_relations(left, rels, ons);
            collect_relations(right, rels, ons);
            ons.extend(on.as_ref());
        }
        _ => rels.push(t),
    }
}

/// `left = right` with each side reading columns, as `(left, right)`.
fn as_equality(p: &BoundExpr) -> Option<(&BoundExpr, &BoundExpr)> {
    match &p.kind {
        BoundExprKind::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => Some((left, right)),
        _ => None,
    }
}

/// Splits `ON`/`WHERE` conjuncts over `left ++ right` into join keys (equalities with one
/// side on each input; the right side re-indexed over `right`) and the rest.
fn split_keys(
    conjuncts: Vec<BoundExpr>,
    left_width: usize,
) -> (Vec<(BoundExpr, BoundExpr)>, Vec<BoundExpr>) {
    let mut keys = Vec::new();
    let mut rest = Vec::new();
    let only_left = |e: &BoundExpr| {
        let c = e.columns();
        !c.is_empty() && c.iter().all(|&i| i < left_width)
    };
    let only_right = |e: &BoundExpr| {
        let c = e.columns();
        !c.is_empty() && c.iter().all(|&i| i >= left_width)
    };
    for p in conjuncts {
        let key = as_equality(&p).and_then(|(a, b)| {
            if only_left(a) && only_right(b) {
                Some((a.clone(), b.clone()))
            } else if only_left(b) && only_right(a) {
                Some((b.clone(), a.clone()))
            } else {
                None
            }
        });
        match key {
            Some((l, mut r)) => {
                r.map_columns(&mut |i| i - left_width);
                keys.push((l, r));
            }
            None => rest.push(p),
        }
    }
    (keys, rest)
}

// ---------------------------------------------------------------------------------------
// Queries

struct Binder<'a> {
    catalog: &'a dyn Catalog,
    /// Visible `WITH` entries, innermost last.
    ctes: Vec<(String, LogicalPlan)>,
}

/// One select-list entry after `*` expansion.
struct Item {
    expr: Expr,
    name: String,
    alias: Option<String>,
}

impl Binder<'_> {
    fn query(&mut self, q: &Query) -> Result<LogicalPlan> {
        let depth = self.ctes.len();
        let result = self.query_with_ctes(q);
        self.ctes.truncate(depth);
        result
    }

    fn query_with_ctes(&mut self, q: &Query) -> Result<LogicalPlan> {
        for cte in &q.ctes {
            let plan = self.query(&cte.query)?;
            self.ctes.push((cte.name.clone(), plan));
        }
        self.select(q)
    }

    fn select(&mut self, q: &Query) -> Result<LogicalPlan> {
        let s = &q.body;
        let (mut plan, scope, filters) = self.from_group(&s.from, s.where_.as_ref())?;
        if let Some(predicate) = BoundExpr::and_all(filters) {
            plan = LogicalPlan::filter(plan, predicate);
        }

        let mut items = Vec::new();
        for item in &s.items {
            match item {
                SelectItem::Wildcard => {
                    if scope.columns.is_empty() {
                        return Err(VetaError::user("SELECT * needs a FROM clause"));
                    }
                    for c in &scope.columns {
                        items.push(Item {
                            expr: Expr::Column {
                                table: Some(c.qualifier.clone()),
                                name: c.name.clone(),
                            },
                            name: c.name.clone(),
                            alias: None,
                        });
                    }
                }
                SelectItem::Expr { expr, alias, text } => items.push(Item {
                    expr: expr.clone(),
                    name: alias.clone().unwrap_or_else(|| text.clone()),
                    alias: alias.clone(),
                }),
            }
        }
        let aliases: Vec<(String, Expr)> = items
            .iter()
            .filter_map(|i| i.alias.clone().map(|a| (a, i.expr.clone())))
            .collect();

        // GROUP BY entries: ordinals, aliases (a FROM column wins) or expressions.
        let mut group_asts = Vec::new();
        for g in &s.group_by {
            group_asts.push(match g {
                Expr::Literal(Literal::Integer(k)) => {
                    let k = *k;
                    if k < 1 || k as usize > items.len() {
                        return Err(VetaError::user(format!(
                            "GROUP BY position {k} is not in the select list"
                        )));
                    }
                    items[k as usize - 1].expr.clone()
                }
                _ => substitute_aliases(g, &aliases, &scope, true),
            });
        }
        let having_ast = s
            .having
            .as_ref()
            .map(|h| substitute_aliases(h, &aliases, &scope, true));
        let aggregated = !group_asts.is_empty()
            || having_ast.is_some()
            || items.iter().any(|i| has_aggregate(&i.expr))
            || q.order_by.iter().any(|o| has_aggregate(&o.expr));

        let mut groups = Vec::new();
        for g in &group_asts {
            if has_aggregate(g) {
                return Err(VetaError::user(
                    "aggregate functions are not allowed in GROUP BY",
                ));
            }
            groups.push(bind_expr(g, &scope, None)?);
        }
        let mut aggs: Vec<AggregateExpr> = Vec::new();
        // Binds an expression for the level above the aggregation (or the FROM level).
        let bind_top = |e: &Expr, aggs: &mut Vec<AggregateExpr>| -> Result<BoundExpr> {
            if aggregated {
                let mut b = bind_expr(e, &scope, Some(aggs))?;
                finalize_aggregated(&mut b, &groups)?;
                Ok(b)
            } else {
                bind_expr(e, &scope, None)
            }
        };

        let mut exprs = Vec::new();
        for item in &items {
            exprs.push(bind_top(&item.expr, &mut aggs)?);
        }
        let having = match &having_ast {
            Some(h) => Some(require_type(
                bind_top(h, &mut aggs)?,
                DataType::Boolean,
                "HAVING",
            )?),
            None => None,
        };

        // ORDER BY: ordinal, alias, an expression of the select list, or a hidden column.
        let mut names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
        let visible = items.len();
        let mut sort_targets = Vec::new();
        for o in &q.order_by {
            let target = match &o.expr {
                Expr::Literal(Literal::Integer(k)) => {
                    let k = *k;
                    if k < 1 || k as usize > visible {
                        return Err(VetaError::user(format!(
                            "ORDER BY position {k} is not in the select list"
                        )));
                    }
                    k as usize - 1
                }
                Expr::Column { table: None, name }
                    if items.iter().any(|i| {
                        i.alias
                            .as_deref()
                            .is_some_and(|a| a.eq_ignore_ascii_case(name))
                    }) =>
                {
                    items
                        .iter()
                        .position(|i| {
                            i.alias
                                .as_deref()
                                .is_some_and(|a| a.eq_ignore_ascii_case(name))
                        })
                        .expect("alias exists")
                }
                other => {
                    let ast = substitute_aliases(other, &aliases, &scope, false);
                    let bound = bind_top(&ast, &mut aggs)?;
                    match exprs.iter().position(|e| *e == bound) {
                        Some(i) => i,
                        None => {
                            if s.distinct {
                                return Err(VetaError::user(
                                    "for SELECT DISTINCT, ORDER BY expressions must appear in the select list",
                                ));
                            }
                            exprs.push(bound);
                            names.push(format!("__order_{}", exprs.len() - visible));
                            exprs.len() - 1
                        }
                    }
                }
            };
            sort_targets.push((target, o.desc, o.nulls_first.unwrap_or(o.desc)));
        }

        if aggregated {
            plan = LogicalPlan::aggregate(plan, groups.clone(), aggs);
        }
        if let Some(h) = having {
            plan = LogicalPlan::filter(plan, h);
        }
        plan = LogicalPlan::project(plan, exprs, names);
        let projected = plan.schema();
        let output_ref = |i: usize| {
            let f = projected.field(i);
            BoundExpr::column(i, ident(&f.name), f.data_type, f.nullable)
        };
        if s.distinct {
            let columns: Vec<BoundExpr> = (0..projected.len()).map(output_ref).collect();
            plan = LogicalPlan::Aggregate {
                input: Box::new(plan),
                groups: columns,
                aggregates: Vec::new(),
                schema: projected.clone(),
            };
        }
        if !sort_targets.is_empty() {
            let keys = sort_targets
                .iter()
                .map(|&(i, desc, nulls_first)| SortKey {
                    expr: output_ref(i),
                    desc,
                    nulls_first,
                })
                .collect();
            plan = LogicalPlan::sort(plan, keys);
        }
        if q.limit.is_some() || q.offset.is_some() {
            plan = LogicalPlan::limit(plan, q.limit, q.offset.unwrap_or(0));
        }
        if projected.len() > visible {
            let kept = (0..visible).map(output_ref).collect();
            let kept_names = projected.fields[..visible]
                .iter()
                .map(|f| f.name.clone())
                .collect();
            plan = LogicalPlan::project(plan, kept, kept_names);
        }
        Ok(plan)
    }

    /// Binds the `FROM` entries as one join group (module docs). Returns the plan, its
    /// scope in `FROM` order and the conjuncts left for the `Filter` above it.
    fn from_group(
        &mut self,
        from: &[TableRef],
        where_: Option<&Expr>,
    ) -> Result<(LogicalPlan, Scope, Vec<BoundExpr>)> {
        let mut refs = Vec::new();
        let mut ons = Vec::new();
        for t in from {
            collect_relations(t, &mut refs, &mut ons);
        }
        let mut rels = Vec::new();
        let mut scope = Scope::default();
        let mut offsets = Vec::new();
        let mut rel_of_column = Vec::new();
        for t in refs {
            let rel = self.relation(t)?;
            check_disjoint(&scope, &rel.scope)?;
            offsets.push(scope.len());
            rel_of_column.extend(std::iter::repeat_n(rels.len(), rel.scope.len()));
            scope = scope.concat(&rel.scope);
            rels.push(rel);
        }

        let mut pool = Vec::new();
        for on in ons {
            let p = require_type(bind_expr(on, &scope, None)?, DataType::Boolean, "ON")?;
            pool.extend(p.into_conjuncts());
        }
        if let Some(w) = where_ {
            if has_aggregate(w) {
                return Err(VetaError::user(
                    "aggregate functions are not allowed in WHERE",
                ));
            }
            let p = require_type(bind_expr(w, &scope, None)?, DataType::Boolean, "WHERE")?;
            pool.extend(p.into_conjuncts());
        }

        if rels.is_empty() {
            return Ok((LogicalPlan::SingleRow, scope, pool));
        }

        let rels_of = |e: &BoundExpr| -> HashSet<usize> {
            e.columns().into_iter().map(|c| rel_of_column[c]).collect()
        };
        // Whether `p` is an equality between the joined set and relation `k`.
        let connects = |p: &BoundExpr, joined: &HashSet<usize>, k: usize| -> Option<bool> {
            let (a, b) = as_equality(p)?;
            let (ra, rb) = (rels_of(a), rels_of(b));
            let one = |r: &HashSet<usize>| r.len() == 1 && r.contains(&k);
            let inside = |r: &HashSet<usize>| !r.is_empty() && r.is_subset(joined);
            if inside(&ra) && one(&rb) {
                Some(false)
            } else if inside(&rb) && one(&ra) {
                Some(true)
            } else {
                None
            }
        };

        let n = rels.len();
        let mut rels: Vec<Option<Relation>> = rels.into_iter().map(Some).collect();
        let mut pool: Vec<Option<BoundExpr>> = pool.into_iter().map(Some).collect();
        let mut joined: HashSet<usize> = HashSet::from([0]);
        let mut order = vec![0usize];
        // Position of each FROM column in the current join output.
        let mut position: Vec<Option<usize>> = vec![None; scope.len()];
        let first = rels[0].take().expect("relation");
        for (i, p) in position.iter_mut().enumerate().take(first.scope.len()) {
            *p = Some(i);
        }
        let mut width = first.scope.len();
        let mut plan = first.plan;
        while order.len() < n {
            let next = (0..n)
                .filter(|k| !joined.contains(k))
                .find(|&k| {
                    pool.iter()
                        .flatten()
                        .any(|p| connects(p, &joined, k).is_some())
                })
                .or_else(|| (0..n).find(|k| !joined.contains(k)))
                .expect("a relation is left");
            let mut keys = Vec::new();
            for slot in pool.iter_mut() {
                let Some(p) = slot else { continue };
                let Some(swapped) = connects(&*p, &joined, next) else {
                    continue;
                };
                let (a, b) = as_equality(&*p).expect("equality");
                let (mut l, mut r) = if swapped {
                    (b.clone(), a.clone())
                } else {
                    (a.clone(), b.clone())
                };
                l.map_columns(&mut |c| position[c].expect("joined column"));
                r.map_columns(&mut |c| c - offsets[next]);
                keys.push((l, r));
                *slot = None;
            }
            let rel = rels[next].take().expect("relation");
            for local in 0..rel.scope.len() {
                position[offsets[next] + local] = Some(width + local);
            }
            width += rel.scope.len();
            let kind = if keys.is_empty() {
                JoinKind::Cross
            } else {
                JoinKind::Inner
            };
            plan = LogicalPlan::join(plan, rel.plan, kind, keys, None);
            joined.insert(next);
            order.push(next);
        }

        let in_from_order = position.iter().enumerate().all(|(i, p)| *p == Some(i));
        if !in_from_order {
            let joined_schema = plan.schema();
            let exprs = position
                .iter()
                .enumerate()
                .map(|(global, p)| {
                    let i = p.expect("every column joined");
                    let f = joined_schema.field(i);
                    BoundExpr::column(i, scope.sql_name(global), f.data_type, f.nullable)
                })
                .collect();
            let names = scope.columns.iter().map(|c| c.name.clone()).collect();
            plan = LogicalPlan::project(plan, exprs, names);
        }
        Ok((plan, scope, pool.into_iter().flatten().collect()))
    }

    fn relation(&mut self, t: &TableRef) -> Result<Relation> {
        match t {
            TableRef::Table { name, alias } => {
                let qualifier = alias.clone().unwrap_or_else(|| name.clone());
                if let Some((_, plan)) = self
                    .ctes
                    .iter()
                    .rev()
                    .find(|(cte, _)| cte.eq_ignore_ascii_case(name))
                {
                    let plan = plan.clone();
                    let scope = Scope::from_plan(&plan, &qualifier, &[]);
                    return Ok(Relation { plan, scope });
                }
                let table = self
                    .catalog
                    .table(name)
                    .ok_or_else(|| VetaError::user(format!("table not found: {name}")))?;
                let plan = LogicalPlan::scan(&table.name, &qualifier, table.schema);
                let scope = Scope::from_plan(&plan, &qualifier, &[]);
                Ok(Relation { plan, scope })
            }
            TableRef::Subquery {
                query,
                alias,
                column_aliases,
            } => {
                let plan = self.query(query)?;
                if column_aliases.len() > plan.schema().len() {
                    return Err(VetaError::user(format!(
                        "{alias} has {} columns but {} column aliases",
                        plan.schema().len(),
                        column_aliases.len()
                    )));
                }
                let scope = Scope::from_plan(&plan, alias, column_aliases);
                Ok(Relation { plan, scope })
            }
            TableRef::Function { name, args, alias } => {
                if !name.eq_ignore_ascii_case("read_parquet") {
                    return Err(VetaError::unsupported(format!("table function {name}")));
                }
                let path = match args.as_slice() {
                    [Expr::Literal(Literal::String(path))] => path,
                    _ => {
                        return Err(VetaError::user(
                            "read_parquet takes one string literal path",
                        ));
                    }
                };
                let qualifier = alias.clone().unwrap_or_else(|| "read_parquet".into());
                let schema = self.catalog.parquet_schema(path)?;
                let plan = LogicalPlan::parquet_scan(path, &qualifier, schema);
                let scope = Scope::from_plan(&plan, &qualifier, &[]);
                Ok(Relation { plan, scope })
            }
            TableRef::Join {
                left,
                right,
                kind: AstJoinKind::Left,
                on,
            } => {
                let l = self.group_relation(left)?;
                let mut r = self.group_relation(right)?;
                check_disjoint(&l.scope, &r.scope)?;
                for c in r.scope.columns.iter_mut() {
                    c.nullable = true;
                }
                let scope = l.scope.concat(&r.scope);
                let on = on
                    .as_ref()
                    .ok_or_else(|| VetaError::user("LEFT JOIN needs ON"))?;
                if has_aggregate(on) {
                    return Err(VetaError::user("aggregate functions are not allowed in ON"));
                }
                let condition =
                    require_type(bind_expr(on, &scope, None)?, DataType::Boolean, "ON")?;
                let (keys, rest) = split_keys(condition.into_conjuncts(), l.scope.len());
                let plan = LogicalPlan::join(
                    l.plan,
                    r.plan,
                    JoinKind::Left,
                    keys,
                    BoundExpr::and_all(rest),
                );
                Ok(Relation { plan, scope })
            }
            TableRef::Join { .. } => self.group_relation(t),
        }
    }

    /// A join tree bound as its own group, with its leftover predicates applied.
    fn group_relation(&mut self, t: &TableRef) -> Result<Relation> {
        let (mut plan, scope, filters) = self.from_group(std::slice::from_ref(t), None)?;
        if let Some(predicate) = BoundExpr::and_all(filters) {
            plan = LogicalPlan::filter(plan, predicate);
        }
        Ok(Relation { plan, scope })
    }
}

#[cfg(test)]
mod tests;
