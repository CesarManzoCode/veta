//! Typed, name-resolved expressions produced by the binder (contract §4, §10.3, §11.6).
//!
//! Columns are indices into the output schema of the plan node below the expression. The
//! binder inserts every implicit conversion as an explicit [`BoundExprKind::Cast`], so the
//! operands of `+ - * %` have one common type (integers, `Float64`, or decimals whose
//! scales may differ), `/` always works on `Float64`, and both sides of a comparison have
//! the same type.

use std::fmt::Write;

use veta_core::datetime::{self, DatePart, TruncUnit};
use veta_core::decimal;
use veta_core::types::MAX_DECIMAL_PRECISION;
use veta_core::{DataType, Result, Value, VetaError};

pub use crate::ast::{BinaryOp, UnaryOp};
use crate::display::write_string;

/// Scalar functions of §10.3, identified by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    /// `length(s)`, bytes.
    Length,
    /// `STRLEN(s)`, bytes.
    Strlen,
    /// `substring(s, from[, length])`, byte positions, base 1.
    Substring,
    /// `REGEXP_REPLACE(s, pattern, replacement)`.
    RegexpReplace,
    /// `extract(part FROM e)`.
    Extract(DatePart),
    /// `DATE_TRUNC('unit', e)`.
    DateTrunc(TruncUnit),
}

impl ScalarFunc {
    pub fn name(self) -> &'static str {
        match self {
            ScalarFunc::Length => "length",
            ScalarFunc::Strlen => "strlen",
            ScalarFunc::Substring => "substring",
            ScalarFunc::RegexpReplace => "regexp_replace",
            ScalarFunc::Extract(_) => "extract",
            ScalarFunc::DateTrunc(_) => "date_trunc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::CountStar | AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        }
    }
}

/// One aggregate of an `Aggregate` node; its argument is bound over the node's input.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateExpr {
    pub func: AggFunc,
    pub distinct: bool,
    /// `None` only for `COUNT(*)`.
    pub arg: Option<BoundExpr>,
    pub data_type: DataType,
    pub nullable: bool,
}

impl AggregateExpr {
    /// Result type of §4: `COUNT` → `Int64`; `SUM(Int16|Int32)` → `Int64`;
    /// `SUM(Int64)` → `Int128`; `SUM(Decimal(p,s))` → `Decimal(38,s)`; `SUM(Float64)` →
    /// `Float64`; `AVG` → `Float64`; `MIN`/`MAX` keep the input type. Only `COUNT` is
    /// never NULL.
    pub fn new(func: AggFunc, distinct: bool, arg: Option<BoundExpr>) -> Result<AggregateExpr> {
        let input = arg.as_ref().map(|a| a.data_type);
        let data_type = match (func, input) {
            (AggFunc::CountStar, _) | (AggFunc::Count, _) => DataType::Int64,
            (AggFunc::Sum, Some(DataType::Int16 | DataType::Int32)) => DataType::Int64,
            (AggFunc::Sum, Some(DataType::Int64 | DataType::Int128)) => DataType::Int128,
            (AggFunc::Sum, Some(DataType::Decimal { scale, .. })) => DataType::Decimal {
                precision: MAX_DECIMAL_PRECISION,
                scale,
            },
            (AggFunc::Sum, Some(DataType::Float64)) => DataType::Float64,
            (AggFunc::Avg, Some(t)) if t.is_numeric() => DataType::Float64,
            (AggFunc::Min | AggFunc::Max, Some(t)) => t,
            (_, t) => {
                return Err(VetaError::user(format!(
                    "cannot apply {} to {}",
                    func.name(),
                    t.map(|t| t.sql_name()).unwrap_or_else(|| "*".into())
                )));
            }
        };
        let nullable = !matches!(func, AggFunc::CountStar | AggFunc::Count);
        Ok(AggregateExpr {
            func,
            distinct,
            arg,
            data_type,
            nullable,
        })
    }

    pub fn to_sql(&self) -> String {
        self.call_sql(self.arg.as_ref().map(BoundExpr::to_sql))
    }

    /// [`Self::to_sql`] with the columns as `columns` gives them (see [`BoundExpr::to_sql_over`]).
    pub fn to_sql_over(&self, columns: &[String]) -> String {
        self.call_sql(self.arg.as_ref().map(|arg| arg.to_sql_over(columns)))
    }

    fn call_sql(&self, arg: Option<String>) -> String {
        match arg {
            None => "count(*)".into(),
            Some(arg) => format!(
                "{}({}{arg})",
                self.func.name(),
                if self.distinct { "DISTINCT " } else { "" },
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundExpr {
    pub kind: BoundExprKind,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundExprKind {
    /// Column `index` of the input; `name` is its SQL text (`alias.col`, §11.6).
    Column {
        index: usize,
        name: String,
    },
    Literal(Value),
    Unary {
        op: UnaryOp,
        expr: Box<BoundExpr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<BoundExpr>,
        right: Box<BoundExpr>,
    },
    /// Conversion to the expression's own `data_type`.
    Cast(Box<BoundExpr>),
    Case {
        branches: Vec<(BoundExpr, BoundExpr)>,
        else_: Option<Box<BoundExpr>>,
    },
    Like {
        expr: Box<BoundExpr>,
        pattern: Box<BoundExpr>,
        negated: bool,
    },
    InList {
        expr: Box<BoundExpr>,
        list: Vec<BoundExpr>,
        negated: bool,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    Function {
        func: ScalarFunc,
        args: Vec<BoundExpr>,
    },
    /// Column `index` of an `Aggregate` output that holds an aggregate; `sql` is the text
    /// of the aggregate call.
    AggregateRef {
        index: usize,
        sql: String,
    },
}

impl BoundExpr {
    pub fn new(kind: BoundExprKind, data_type: DataType, nullable: bool) -> BoundExpr {
        BoundExpr {
            kind,
            data_type,
            nullable,
        }
    }

    /// A literal; an untyped `NULL` gets `Int32` until a coercion retypes it.
    pub fn literal(value: Value) -> BoundExpr {
        let data_type = value.data_type().unwrap_or(DataType::Int32);
        let nullable = value.is_null();
        BoundExpr::new(BoundExprKind::Literal(value), data_type, nullable)
    }

    /// A literal with an explicit type (used for typed `NULL`).
    pub fn typed_literal(value: Value, data_type: DataType) -> BoundExpr {
        let nullable = value.is_null();
        BoundExpr::new(BoundExprKind::Literal(value), data_type, nullable)
    }

    pub fn column(
        index: usize,
        name: impl Into<String>,
        data_type: DataType,
        nullable: bool,
    ) -> Self {
        BoundExpr::new(
            BoundExprKind::Column {
                index,
                name: name.into(),
            },
            data_type,
            nullable,
        )
    }

    pub fn as_literal(&self) -> Option<&Value> {
        match &self.kind {
            BoundExprKind::Literal(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_null_literal(&self) -> bool {
        matches!(&self.kind, BoundExprKind::Literal(Value::Null))
    }

    pub fn children(&self) -> Vec<&BoundExpr> {
        match &self.kind {
            BoundExprKind::Column { .. }
            | BoundExprKind::Literal(_)
            | BoundExprKind::AggregateRef { .. } => vec![],
            BoundExprKind::Unary { expr, .. }
            | BoundExprKind::Cast(expr)
            | BoundExprKind::IsNull { expr, .. } => vec![&**expr],
            BoundExprKind::Binary { left, right, .. } => vec![&**left, &**right],
            BoundExprKind::Like { expr, pattern, .. } => vec![&**expr, &**pattern],
            BoundExprKind::InList { expr, list, .. } => {
                std::iter::once(&**expr).chain(list.iter()).collect()
            }
            BoundExprKind::Case { branches, else_ } => {
                let mut out: Vec<&BoundExpr> = branches.iter().flat_map(|(c, v)| [c, v]).collect();
                out.extend(else_.as_deref());
                out
            }
            BoundExprKind::Function { args, .. } => args.iter().collect(),
        }
    }

    pub fn children_mut(&mut self) -> Vec<&mut BoundExpr> {
        match &mut self.kind {
            BoundExprKind::Column { .. }
            | BoundExprKind::Literal(_)
            | BoundExprKind::AggregateRef { .. } => vec![],
            BoundExprKind::Unary { expr, .. }
            | BoundExprKind::Cast(expr)
            | BoundExprKind::IsNull { expr, .. } => vec![&mut **expr],
            BoundExprKind::Binary { left, right, .. } => vec![&mut **left, &mut **right],
            BoundExprKind::Like { expr, pattern, .. } => vec![&mut **expr, &mut **pattern],
            BoundExprKind::InList { expr, list, .. } => std::iter::once(&mut **expr)
                .chain(list.iter_mut())
                .collect(),
            BoundExprKind::Case { branches, else_ } => {
                let mut out: Vec<&mut BoundExpr> = Vec::new();
                for (c, v) in branches.iter_mut() {
                    out.push(c);
                    out.push(v);
                }
                if let Some(e) = else_ {
                    out.push(&mut **e);
                }
                out
            }
            BoundExprKind::Function { args, .. } => args.iter_mut().collect(),
        }
    }

    /// Input column indices read by the expression (columns and aggregate references),
    /// sorted and without duplicates.
    pub fn columns(&self) -> Vec<usize> {
        fn walk(e: &BoundExpr, out: &mut Vec<usize>) {
            match &e.kind {
                BoundExprKind::Column { index, .. } | BoundExprKind::AggregateRef { index, .. } => {
                    out.push(*index)
                }
                _ => e.children().into_iter().for_each(|c| walk(c, out)),
            }
        }
        let mut out = Vec::new();
        walk(self, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Rewrites every input column index with `f`.
    pub fn map_columns(&mut self, f: &mut dyn FnMut(usize) -> usize) {
        if let BoundExprKind::Column { index, .. } | BoundExprKind::AggregateRef { index, .. } =
            &mut self.kind
        {
            *index = f(*index);
            return;
        }
        for c in self.children_mut() {
            c.map_columns(f);
        }
    }

    /// Splits nested `AND`s into their conjuncts.
    pub fn into_conjuncts(self) -> Vec<BoundExpr> {
        match self.kind {
            BoundExprKind::Binary {
                op: BinaryOp::And,
                left,
                right,
            } => {
                let mut out = left.into_conjuncts();
                out.extend(right.into_conjuncts());
                out
            }
            kind => vec![BoundExpr { kind, ..self }],
        }
    }

    /// [`Self::into_conjuncts`], and from each `OR` conjunct the conjuncts that every one
    /// of its branches has are taken out: `(a AND b) OR (a AND c)` gives `a` and `b OR c`,
    /// and `a OR (a AND b)` gives `a`. Both are identities of three-valued logic, so the
    /// result is equivalent; they let an equality written in every branch become a join
    /// key (TPC-H Q19).
    pub fn into_factored_conjuncts(self) -> Vec<BoundExpr> {
        let mut out = Vec::new();
        for conjunct in self.into_conjuncts() {
            if !matches!(
                conjunct.kind,
                BoundExprKind::Binary {
                    op: BinaryOp::Or,
                    ..
                }
            ) {
                out.push(conjunct);
                continue;
            }
            let mut branches: Vec<Vec<BoundExpr>> = conjunct
                .clone()
                .into_disjuncts()
                .into_iter()
                .map(BoundExpr::into_conjuncts)
                .collect();
            let mut common = Vec::new();
            for c in &branches[0] {
                if !common.contains(c) && branches[1..].iter().all(|b| b.contains(c)) {
                    common.push(c.clone());
                }
            }
            if common.is_empty() {
                out.push(conjunct);
                continue;
            }
            for b in &mut branches {
                b.retain(|c| !common.contains(c));
            }
            out.extend(common);
            if branches.iter().all(|b| !b.is_empty()) {
                let rest = branches
                    .into_iter()
                    .map(|b| BoundExpr::and_all(b).expect("non-empty branch"))
                    .reduce(|left, right| {
                        let nullable = left.nullable || right.nullable;
                        BoundExpr::new(
                            BoundExprKind::Binary {
                                op: BinaryOp::Or,
                                left: Box::new(left),
                                right: Box::new(right),
                            },
                            DataType::Boolean,
                            nullable,
                        )
                    })
                    .expect("an OR has branches");
                out.push(rest);
            }
        }
        out
    }

    /// Splits nested `OR`s into their branches.
    pub fn into_disjuncts(self) -> Vec<BoundExpr> {
        match self.kind {
            BoundExprKind::Binary {
                op: BinaryOp::Or,
                left,
                right,
            } => {
                let mut out = left.into_disjuncts();
                out.extend(right.into_disjuncts());
                out
            }
            kind => vec![BoundExpr { kind, ..self }],
        }
    }

    /// `c1 AND c2 AND …`, left-deep; `None` for an empty list.
    pub fn and_all(conjuncts: Vec<BoundExpr>) -> Option<BoundExpr> {
        conjuncts.into_iter().reduce(|left, right| {
            let nullable = left.nullable || right.nullable;
            BoundExpr::new(
                BoundExprKind::Binary {
                    op: BinaryOp::And,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                DataType::Boolean,
                nullable,
            )
        })
    }

    /// SQL text of §11.6: `alias.col` columns, `CAST('…' AS DATE|TIMESTAMP)` for dates and
    /// timestamps, strings with `''`, and explicit parentheses around every operator.
    pub fn to_sql(&self) -> String {
        let mut out = String::new();
        self.write_sql(&mut out);
        out
    }

    /// [`Self::to_sql`] with each column read as the text `columns` gives its index, so the
    /// text speaks of what the input computes and not of its name in the query.
    pub fn to_sql_over(&self, columns: &[String]) -> String {
        fn rename(e: &mut BoundExpr, columns: &[String]) {
            if let BoundExprKind::Column { index, name } = &mut e.kind {
                *name = columns[*index].clone();
                return;
            }
            if let BoundExprKind::AggregateRef { index, sql } = &mut e.kind {
                *sql = columns[*index].clone();
                return;
            }
            for child in e.children_mut() {
                rename(child, columns);
            }
        }
        let mut e = self.clone();
        rename(&mut e, columns);
        e.to_sql()
    }

    fn write_sql(&self, out: &mut String) {
        let not = |negated: bool| if negated { "NOT " } else { "" };
        match &self.kind {
            BoundExprKind::Column { name, .. } => out.push_str(name),
            BoundExprKind::AggregateRef { sql, .. } => out.push_str(sql),
            BoundExprKind::Literal(v) => write_value_sql(out, v, self.data_type),
            BoundExprKind::Unary { op, expr } => {
                out.push_str(match op {
                    UnaryOp::Neg => "(- ",
                    UnaryOp::Not => "(NOT ",
                });
                expr.write_sql(out);
                out.push(')');
            }
            BoundExprKind::Binary { op, left, right } => {
                out.push('(');
                left.write_sql(out);
                let _ = write!(out, " {} ", op.symbol());
                right.write_sql(out);
                out.push(')');
            }
            BoundExprKind::Cast(expr) => {
                out.push_str("CAST(");
                expr.write_sql(out);
                let _ = write!(out, " AS {})", self.data_type.sql_name());
            }
            BoundExprKind::Case { branches, else_ } => {
                out.push_str("(CASE");
                for (cond, value) in branches {
                    out.push_str(" WHEN ");
                    cond.write_sql(out);
                    out.push_str(" THEN ");
                    value.write_sql(out);
                }
                if let Some(e) = else_ {
                    out.push_str(" ELSE ");
                    e.write_sql(out);
                }
                out.push_str(" END)");
            }
            BoundExprKind::Like {
                expr,
                pattern,
                negated,
            } => {
                out.push('(');
                expr.write_sql(out);
                let _ = write!(out, " {}LIKE ", not(*negated));
                pattern.write_sql(out);
                out.push(')');
            }
            BoundExprKind::InList {
                expr,
                list,
                negated,
            } => {
                out.push('(');
                expr.write_sql(out);
                let _ = write!(out, " {}IN (", not(*negated));
                for (i, item) in list.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write_sql(out);
                }
                out.push_str("))");
            }
            BoundExprKind::IsNull { expr, negated } => {
                out.push('(');
                expr.write_sql(out);
                let _ = write!(out, " IS {}NULL)", not(*negated));
            }
            BoundExprKind::Function { func, args } => match func {
                ScalarFunc::Extract(part) => {
                    let _ = write!(out, "extract({} FROM ", part.name());
                    args[0].write_sql(out);
                    out.push(')');
                }
                ScalarFunc::DateTrunc(unit) => {
                    let _ = write!(out, "date_trunc('{}', ", trunc_unit_name(*unit));
                    args[0].write_sql(out);
                    out.push(')');
                }
                _ => {
                    out.push_str(func.name());
                    out.push('(');
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        arg.write_sql(out);
                    }
                    out.push(')');
                }
            },
        }
    }
}

pub fn trunc_unit_name(unit: TruncUnit) -> &'static str {
    match unit {
        TruncUnit::Minute => "minute",
        TruncUnit::Hour => "hour",
        TruncUnit::Day => "day",
        TruncUnit::Month => "month",
        TruncUnit::Year => "year",
    }
}

/// A constant as SQL text, typed by its own value (an untyped `NULL` prints as `NULL`).
pub fn value_to_sql(value: &Value) -> String {
    let mut out = String::new();
    write_value_sql(
        &mut out,
        value,
        value.data_type().unwrap_or(DataType::Int32),
    );
    out
}

/// Writes a constant as SQL that DuckDB and veta read back as the same value and type
/// family.
fn write_value_sql(out: &mut String, value: &Value, data_type: DataType) {
    match value {
        Value::Null => {
            if data_type == DataType::Int32 {
                out.push_str("NULL")
            } else {
                let _ = write!(out, "CAST(NULL AS {})", data_type.sql_name());
            }
        }
        Value::Boolean(true) => out.push_str("TRUE"),
        Value::Boolean(false) => out.push_str("FALSE"),
        Value::Int16(_) | Value::Int32(_) | Value::Int64(_) | Value::Int128(_) => {
            let _ = write!(out, "{}", value.as_i128().unwrap_or(0));
        }
        Value::Float64(f) => {
            if f.is_finite() {
                let _ = write!(out, "CAST({f:?} AS DOUBLE)");
            } else {
                let text = if f.is_nan() {
                    "nan"
                } else if *f > 0.0 {
                    "inf"
                } else {
                    "-inf"
                };
                let _ = write!(out, "CAST('{text}' AS DOUBLE)");
            }
        }
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            if *scale == 0 {
                let _ = write!(out, "CAST({value} AS DECIMAL({precision},0))");
            } else {
                out.push_str(&decimal::format(*value, *scale));
            }
        }
        Value::Varchar(bytes) => {
            let _ = write_string(out, &String::from_utf8_lossy(bytes));
        }
        Value::Date(days) => {
            let _ = write!(out, "CAST('{}' AS DATE)", datetime::format_date(*days));
        }
        Value::Timestamp(seconds) => {
            let _ = write!(
                out,
                "CAST('{}' AS TIMESTAMP)",
                datetime::format_timestamp(*seconds)
            );
        }
    }
}

fn integer_range(data_type: DataType) -> Option<(i128, i128)> {
    Some(match data_type {
        DataType::Int16 => (i16::MIN as i128, i16::MAX as i128),
        DataType::Int32 => (i32::MIN as i128, i32::MAX as i128),
        DataType::Int64 => (i64::MIN as i128, i64::MAX as i128),
        DataType::Int128 => (i128::MIN, i128::MAX),
        _ => return None,
    })
}

fn make_integer(v: i128, to: DataType) -> Result<Value> {
    let (lo, hi) = integer_range(to).ok_or_else(|| VetaError::internal("not an integer type"))?;
    if v < lo || v > hi {
        return Err(VetaError::user(format!(
            "value {v} out of range for {}",
            to.sql_name()
        )));
    }
    Ok(match to {
        DataType::Int16 => Value::Int16(v as i16),
        DataType::Int32 => Value::Int32(v as i32),
        DataType::Int64 => Value::Int64(v as i64),
        _ => Value::Int128(v),
    })
}

fn make_decimal(value: i128, precision: u8, scale: u8) -> Result<Value> {
    if !decimal::fits_precision(value, precision) {
        return Err(VetaError::user("decimal overflow"));
    }
    Ok(Value::Decimal {
        value,
        precision,
        scale,
    })
}

fn f64_to_i128(f: f64) -> Result<i128> {
    let r = f.round();
    if !r.is_finite() || r.abs() >= 1.7e38 {
        return Err(VetaError::user(format!("value {f} out of integer range")));
    }
    Ok(r as i128)
}

fn value_text(value: &Value, data_type: DataType) -> String {
    match value {
        Value::Varchar(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Decimal { value, scale, .. } => decimal::format(*value, *scale),
        Value::Date(d) => datetime::format_date(*d),
        Value::Timestamp(t) => datetime::format_timestamp(*t),
        other => other
            .as_i128()
            .map(|v| v.to_string())
            .unwrap_or_else(|| data_type.sql_name()),
    }
}

/// Converts a constant to `to` (constant folding of `CAST` and of the binder's implicit
/// conversions). Narrowing numeric conversions round half away from zero and fail when
/// the value does not fit.
pub fn cast_value(value: &Value, to: DataType) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let from = value.data_type().unwrap_or(to);
    if from == to {
        return Ok(value.clone());
    }
    let invalid = || {
        VetaError::user(format!(
            "cannot cast {} to {}",
            from.sql_name(),
            to.sql_name()
        ))
    };
    match (value, to) {
        (Value::Varchar(b), _) => {
            let text = String::from_utf8_lossy(b);
            let text = text.trim();
            match to {
                DataType::Date => Ok(Value::Date(datetime::parse_date(text.as_bytes())?)),
                DataType::Timestamp => Ok(Value::Timestamp(datetime::parse_timestamp(
                    text.as_bytes(),
                )?)),
                DataType::Boolean => match text.to_ascii_lowercase().as_str() {
                    "true" | "1" => Ok(Value::Boolean(true)),
                    "false" | "0" => Ok(Value::Boolean(false)),
                    _ => Err(VetaError::user(format!("invalid boolean: {text}"))),
                },
                DataType::Float64 => text
                    .parse::<f64>()
                    .map(Value::Float64)
                    .map_err(|_| VetaError::user(format!("invalid number: {text}"))),
                DataType::Decimal { precision, scale } => Ok(Value::Decimal {
                    value: decimal::parse(text, precision, scale)?,
                    precision,
                    scale,
                }),
                _ => {
                    let v = text
                        .parse::<i128>()
                        .map_err(|_| VetaError::user(format!("invalid integer: {text}")))?;
                    make_integer(v, to)
                }
            }
        }
        (_, DataType::Varchar) => Ok(Value::Varchar(value_text(value, from).into_bytes())),
        (Value::Boolean(b), _) if to.is_integer() => make_integer(*b as i128, to),
        (_, DataType::Boolean) if from.is_integer() => {
            Ok(Value::Boolean(value.as_i128().unwrap_or(0) != 0))
        }
        (Value::Date(d), DataType::Timestamp) => {
            Ok(Value::Timestamp(*d as i64 * datetime::SECONDS_PER_DAY))
        }
        (Value::Timestamp(t), DataType::Date) => {
            Ok(Value::Date(t.div_euclid(datetime::SECONDS_PER_DAY) as i32))
        }
        (Value::Float64(f), DataType::Decimal { precision, scale }) => {
            let scaled = f * decimal::pow10(scale) as f64;
            make_decimal(f64_to_i128(scaled)?, precision, scale)
        }
        (Value::Float64(f), _) if to.is_integer() => make_integer(f64_to_i128(*f)?, to),
        (Value::Decimal { value, scale, .. }, DataType::Float64) => {
            Ok(Value::Float64(decimal::to_f64(*value, *scale)))
        }
        (
            Value::Decimal { value, scale, .. },
            DataType::Decimal {
                precision: p,
                scale: s,
            },
        ) => make_decimal(decimal::rescale(*value, *scale, s)?, p, s),
        (Value::Decimal { value, scale, .. }, _) if to.is_integer() => {
            make_integer(decimal::rescale(*value, *scale, 0)?, to)
        }
        (_, DataType::Float64) if from.is_integer() => {
            Ok(Value::Float64(value.as_i128().unwrap_or(0) as f64))
        }
        (_, DataType::Decimal { precision, scale }) if from.is_integer() => {
            let v = value
                .as_i128()
                .unwrap_or(0)
                .checked_mul(decimal::pow10(scale))
                .ok_or_else(|| VetaError::user("decimal overflow"))?;
            make_decimal(v, precision, scale)
        }
        _ if from.is_integer() && to.is_integer() => make_integer(value.as_i128().unwrap_or(0), to),
        _ => Err(invalid()),
    }
}

/// Whether `CAST(x AS to)` is accepted for a value of type `from`.
pub fn can_cast(from: DataType, to: DataType) -> bool {
    use DataType::*;
    from == to
        || from == Varchar
        || to == Varchar
        || (from.is_numeric() && to.is_numeric())
        || (from == Boolean && to.is_integer())
        || (from.is_integer() && to == Boolean)
        || matches!((from, to), (Date, Timestamp) | (Timestamp, Date))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(index: usize, name: &str, dt: DataType) -> BoundExpr {
        BoundExpr::column(index, name, dt, false)
    }

    fn bin(op: BinaryOp, l: BoundExpr, r: BoundExpr, dt: DataType) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::Binary {
                op,
                left: Box::new(l),
                right: Box::new(r),
            },
            dt,
            false,
        )
    }

    #[test]
    fn to_sql_follows_explain_rules() {
        let date = BoundExpr::literal(Value::Date(datetime::date_from_ymd(1995, 3, 15).unwrap()));
        let e = bin(
            BinaryOp::Lt,
            col(0, "o.o_orderdate", DataType::Date),
            date,
            DataType::Boolean,
        );
        assert_eq!(e.to_sql(), "(o.o_orderdate < CAST('1995-03-15' AS DATE))");

        let s = BoundExpr::new(
            BoundExprKind::Like {
                expr: Box::new(col(1, "h.URL", DataType::Varchar)),
                pattern: Box::new(BoundExpr::literal(Value::varchar("%it's%"))),
                negated: true,
            },
            DataType::Boolean,
            false,
        );
        assert_eq!(s.to_sql(), "(h.URL NOT LIKE '%it''s%')");

        let ts = BoundExpr::literal(Value::Timestamp(3600));
        assert_eq!(ts.to_sql(), "CAST('1970-01-01 01:00:00' AS TIMESTAMP)");
        let d = BoundExpr::literal(Value::Decimal {
            value: 5,
            precision: 3,
            scale: 2,
        });
        assert_eq!(d.to_sql(), "0.05");
        assert_eq!(BoundExpr::literal(Value::Int64(-3)).to_sql(), "-3");
    }

    #[test]
    fn functions_and_aggregates_print() {
        let e = BoundExpr::new(
            BoundExprKind::Function {
                func: ScalarFunc::Extract(DatePart::Minute),
                args: vec![col(0, "hits.EventTime", DataType::Timestamp)],
            },
            DataType::Int64,
            false,
        );
        assert_eq!(e.to_sql(), "extract(minute FROM hits.EventTime)");
        let t = BoundExpr::new(
            BoundExprKind::Function {
                func: ScalarFunc::DateTrunc(TruncUnit::Minute),
                args: vec![col(0, "hits.EventTime", DataType::Timestamp)],
            },
            DataType::Timestamp,
            false,
        );
        assert_eq!(t.to_sql(), "date_trunc('minute', hits.EventTime)");
        let agg = AggregateExpr::new(
            AggFunc::Count,
            true,
            Some(col(0, "hits.UserID", DataType::Int64)),
        )
        .unwrap();
        assert_eq!(agg.to_sql(), "count(DISTINCT hits.UserID)");
        assert_eq!(
            AggregateExpr::new(AggFunc::CountStar, false, None)
                .unwrap()
                .to_sql(),
            "count(*)"
        );
    }

    #[test]
    fn aggregate_result_types() {
        let arg = |dt| Some(col(0, "x", dt));
        let ty = |f, dt| AggregateExpr::new(f, false, arg(dt)).unwrap().data_type;
        assert_eq!(ty(AggFunc::Sum, DataType::Int16), DataType::Int64);
        assert_eq!(ty(AggFunc::Sum, DataType::Int32), DataType::Int64);
        assert_eq!(ty(AggFunc::Sum, DataType::Int64), DataType::Int128);
        assert_eq!(
            ty(
                AggFunc::Sum,
                DataType::Decimal {
                    precision: 15,
                    scale: 2
                }
            ),
            DataType::Decimal {
                precision: 38,
                scale: 2
            }
        );
        assert_eq!(ty(AggFunc::Sum, DataType::Float64), DataType::Float64);
        assert_eq!(ty(AggFunc::Avg, DataType::Int16), DataType::Float64);
        assert_eq!(ty(AggFunc::Min, DataType::Varchar), DataType::Varchar);
        assert_eq!(ty(AggFunc::Count, DataType::Varchar), DataType::Int64);
        assert!(AggregateExpr::new(AggFunc::Sum, false, arg(DataType::Varchar)).is_err());
        assert!(AggregateExpr::new(AggFunc::Avg, false, arg(DataType::Date)).is_err());
        let count = AggregateExpr::new(AggFunc::CountStar, false, None).unwrap();
        assert!(!count.nullable);
    }

    #[test]
    fn conjuncts_columns_and_remap() {
        let a = bin(
            BinaryOp::Eq,
            col(0, "a", DataType::Int64),
            col(3, "b", DataType::Int64),
            DataType::Boolean,
        );
        let b = bin(
            BinaryOp::Gt,
            col(2, "c", DataType::Int64),
            BoundExpr::literal(Value::Int64(1)),
            DataType::Boolean,
        );
        let mut both = BoundExpr::and_all(vec![a.clone(), b.clone()]).unwrap();
        assert_eq!(both.columns(), vec![0, 2, 3]);
        both.map_columns(&mut |i| i + 10);
        assert_eq!(both.columns(), vec![10, 12, 13]);
        let parts = both.into_conjuncts();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].columns(), vec![12]);
    }

    #[test]
    fn factors_conjuncts_common_to_every_or_branch() {
        let key = bin(
            BinaryOp::Eq,
            col(0, "a", DataType::Int64),
            col(3, "b", DataType::Int64),
            DataType::Boolean,
        );
        let gt = |c: usize, v: i64| {
            bin(
                BinaryOp::Gt,
                col(c, "c", DataType::Int64),
                BoundExpr::literal(Value::Int64(v)),
                DataType::Boolean,
            )
        };
        let or = |l: BoundExpr, r: BoundExpr| bin(BinaryOp::Or, l, r, DataType::Boolean);
        let and = |l: BoundExpr, r: BoundExpr| BoundExpr::and_all(vec![l, r]).unwrap();

        // (key AND c > 1) OR (key AND c > 2) OR (c > 3 AND key) → key, (c > 1 OR c > 2 OR c > 3).
        let e = or(
            or(and(key.clone(), gt(2, 1)), and(key.clone(), gt(2, 2))),
            and(gt(2, 3), key.clone()),
        );
        let parts = e.into_factored_conjuncts();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], key);
        assert_eq!(parts[1], or(or(gt(2, 1), gt(2, 2)), gt(2, 3)));

        // Absorption: key OR (key AND c > 1) → key.
        let e = or(key.clone(), and(key.clone(), gt(2, 1)));
        assert_eq!(e.into_factored_conjuncts(), vec![key.clone()]);

        // Nothing in common: the OR stays whole, next to the other conjuncts.
        let e = and(gt(2, 0), or(and(key.clone(), gt(2, 1)), gt(2, 2)));
        let parts = e.clone().into_factored_conjuncts();
        assert_eq!(parts, e.into_conjuncts());
    }

    #[test]
    fn casts_constants() {
        assert_eq!(
            cast_value(&Value::varchar("2013-07-01"), DataType::Date).unwrap(),
            Value::Date(datetime::date_from_ymd(2013, 7, 1).unwrap())
        );
        assert_eq!(
            cast_value(&Value::Int64(62), DataType::Int32).unwrap(),
            Value::Int32(62)
        );
        assert!(cast_value(&Value::Int64(1 << 40), DataType::Int32).is_err());
        assert_eq!(
            cast_value(
                &Value::Int64(1),
                DataType::Decimal {
                    precision: 5,
                    scale: 2
                }
            )
            .unwrap(),
            Value::Decimal {
                value: 100,
                precision: 5,
                scale: 2
            }
        );
        assert_eq!(
            cast_value(&Value::Date(1), DataType::Timestamp).unwrap(),
            Value::Timestamp(86_400)
        );
        assert_eq!(
            cast_value(&Value::Int64(7), DataType::Varchar).unwrap(),
            Value::varchar("7")
        );
        assert!(cast_value(&Value::Date(1), DataType::Int64).is_err());
        assert!(can_cast(DataType::Varchar, DataType::Date));
        assert!(!can_cast(DataType::Date, DataType::Int64));
    }
}
