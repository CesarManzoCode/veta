//! Vectorized evaluation of bound expressions (contract §4, §10.3, §12).
//!
//! An expression is compiled once per query ([`CompiledExpr`]): literals are converted to
//! the expression type, `LIKE` patterns and `REGEXP_REPLACE` patterns with constant
//! arguments are compiled there. Evaluation works on a [`Chunk`] and an optional selection
//! vector (chunk row indices); the result has one row per selected row.
//!
//! Kernels match the type once and then loop over slices; a constant operand is never
//! materialized (column-literal fast paths). Logic is three-valued; integer overflow and
//! decimal overflow are errors (exit 1) on non-NULL rows; `/` and `%` by zero give NULL
//! (the binder types both as nullable).
//!
//! Predicates: a conjunction whose conjuncts can never fail is evaluated cheapest first
//! (the order changes neither the rows nor the errors, since none can fail). A leaf that
//! compares a column with a constant, tests it for NULL or matches it with a constant `LIKE`
//! runs on the selected rows in place, without gathering the column; on a `Dict`-encoded
//! string block it is evaluated once per dictionary entry and the codes pick the rows.

mod cast;

use std::borrow::Cow;
use std::cmp::Ordering;
use std::sync::Arc;

use veta_codec::DictCodes;
use veta_core::datetime::{DatePart, TruncUnit};
use veta_core::types::MAX_DECIMAL64_PRECISION;
use veta_core::value::{cmp_f64, cmp_scaled};
use veta_core::{Bitmap, Chunk, DataType, Result, Value, Vector, VectorData, VetaError, decimal};
use veta_sql::expr::{BinaryOp, UnaryOp, cast_value};
use veta_sql::{BoundExpr, BoundExprKind, ScalarFunc};

pub use cast::cast_vector;

use crate::functions::{self, IntArg, RegexpReplace, StrArg};
use crate::like::LikePattern;

/// Evaluates `expr` over the selected rows of `chunk` (all rows without a selection).
pub fn eval(expr: &BoundExpr, chunk: &Chunk, selection: Option<&[u32]>) -> Result<Vector> {
    CompiledExpr::new(expr)?.eval(chunk, selection)
}

/// Rows of `chunk` (among `selection`, in its order) where the predicate is true; NULL and
/// false rows are dropped.
pub fn eval_predicate(
    expr: &BoundExpr,
    chunk: &Chunk,
    selection: Option<&[u32]>,
) -> Result<Vec<u32>> {
    CompiledExpr::new(expr)?.eval_predicate(chunk, selection)
}

#[derive(Debug)]
struct Expr {
    node: Node,
    ty: DataType,
}

#[derive(Debug)]
enum LikeArg {
    /// Compiled once; `None` is a NULL pattern.
    Const(Option<LikePattern>),
    Dynamic(Box<Expr>),
}

#[derive(Debug)]
enum Node {
    Column(usize),
    Literal(Value),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Cast(Box<Expr>),
    Case {
        branches: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    Like {
        expr: Box<Expr>,
        pattern: LikeArg,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Length(Box<Expr>),
    Extract(DatePart, Box<Expr>),
    DateTrunc(TruncUnit, Box<Expr>),
    Substring(Vec<Expr>),
    RegexpReplace {
        args: Vec<Expr>,
        /// Pattern and replacement compiled once when both are non-NULL literals.
        compiled: Option<RegexpReplace>,
    },
}

/// An expression compiled for repeated evaluation; it can be shared between threads.
#[derive(Debug)]
pub struct CompiledExpr {
    root: Expr,
}

fn compile(e: &BoundExpr) -> Result<Expr> {
    let boxed = |e: &BoundExpr| compile(e).map(Box::new);
    let node = match &e.kind {
        BoundExprKind::Column { index, .. } | BoundExprKind::AggregateRef { index, .. } => {
            Node::Column(*index)
        }
        BoundExprKind::Literal(v) => {
            if v.is_null() || v.data_type() == Some(e.data_type) {
                Node::Literal(v.clone())
            } else {
                Node::Literal(cast_value(v, e.data_type)?)
            }
        }
        BoundExprKind::Unary { op, expr } => match op {
            UnaryOp::Neg => Node::Neg(boxed(expr)?),
            UnaryOp::Not => Node::Not(boxed(expr)?),
        },
        BoundExprKind::Binary {
            op: BinaryOp::And, ..
        } => return compile_conjunction(e),
        BoundExprKind::Binary { op, left, right } => Node::Binary {
            op: *op,
            left: boxed(left)?,
            right: boxed(right)?,
        },
        BoundExprKind::Cast(inner) => Node::Cast(boxed(inner)?),
        BoundExprKind::Case { branches, else_ } => Node::Case {
            branches: branches
                .iter()
                .map(|(c, v)| -> Result<(Expr, Expr)> { Ok((compile(c)?, compile(v)?)) })
                .collect::<Result<_>>()?,
            else_: else_.as_deref().map(boxed).transpose()?,
        },
        BoundExprKind::Like {
            expr,
            pattern,
            negated,
        } => Node::Like {
            expr: boxed(expr)?,
            pattern: match pattern.as_literal() {
                Some(Value::Null) => LikeArg::Const(None),
                Some(Value::Varchar(p)) => LikeArg::Const(Some(LikePattern::new(p)?)),
                _ => LikeArg::Dynamic(boxed(pattern)?),
            },
            negated: *negated,
        },
        BoundExprKind::InList {
            expr,
            list,
            negated,
        } => Node::InList {
            expr: boxed(expr)?,
            list: list.iter().map(compile).collect::<Result<_>>()?,
            negated: *negated,
        },
        BoundExprKind::IsNull { expr, negated } => Node::IsNull {
            expr: boxed(expr)?,
            negated: *negated,
        },
        BoundExprKind::Function { func, args } => {
            let arity = |min: usize, max: usize| {
                if args.len() < min || args.len() > max {
                    Err(VetaError::internal(format!(
                        "{} with {} arguments",
                        func.name(),
                        args.len()
                    )))
                } else {
                    Ok(())
                }
            };
            match func {
                ScalarFunc::Length | ScalarFunc::Strlen => {
                    arity(1, 1)?;
                    Node::Length(boxed(&args[0])?)
                }
                ScalarFunc::Extract(part) => {
                    arity(1, 1)?;
                    Node::Extract(*part, boxed(&args[0])?)
                }
                ScalarFunc::DateTrunc(unit) => {
                    arity(1, 1)?;
                    Node::DateTrunc(*unit, boxed(&args[0])?)
                }
                ScalarFunc::Substring => {
                    arity(2, 3)?;
                    Node::Substring(args.iter().map(compile).collect::<Result<_>>()?)
                }
                ScalarFunc::RegexpReplace => {
                    arity(3, 3)?;
                    let compiled = match (args[1].as_literal(), args[2].as_literal()) {
                        (Some(Value::Varchar(p)), Some(Value::Varchar(r))) => {
                            Some(RegexpReplace::new(p, r)?)
                        }
                        _ => None,
                    };
                    Node::RegexpReplace {
                        args: args.iter().map(compile).collect::<Result<_>>()?,
                        compiled,
                    }
                }
            }
        }
    };
    Ok(Expr {
        node,
        ty: e.data_type,
    })
}

/// The conjuncts of a chain of `AND`s, left to right.
fn conjuncts<'e>(e: &'e BoundExpr, out: &mut Vec<&'e BoundExpr>) {
    match &e.kind {
        BoundExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        _ => out.push(e),
    }
}

/// A chain of `AND`s as a left-deep tree. When no conjunct can fail, they go cheapest first
/// (stable, so equal costs keep the query order): the right side of an `AND` only sees the
/// rows its left side kept, and with infallible conjuncts the order changes neither the
/// result nor the errors. Otherwise the query order stays.
fn compile_conjunction(e: &BoundExpr) -> Result<Expr> {
    let mut parts = Vec::new();
    conjuncts(e, &mut parts);
    let mut compiled = parts
        .into_iter()
        .map(compile)
        .collect::<Result<Vec<Expr>>>()?;
    if compiled.iter().all(infallible) {
        compiled.sort_by_key(cost);
    }
    let mut items = compiled.into_iter();
    let mut tree = items
        .next()
        .ok_or_else(|| VetaError::internal("empty conjunction"))?;
    for next in items {
        tree = Expr {
            node: Node::Binary {
                op: BinaryOp::And,
                left: Box::new(tree),
                right: Box::new(next),
            },
            ty: DataType::Boolean,
        };
    }
    Ok(tree)
}

fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
    )
}

/// Whether evaluating `e` can never fail, whatever the rows: columns, constants,
/// comparisons, `IS NULL`, constant `LIKE`, `IN` lists and the logic between them.
fn infallible(e: &Expr) -> bool {
    match &e.node {
        Node::Column(_) | Node::Literal(_) => true,
        Node::Not(inner) => infallible(inner),
        Node::IsNull { expr, .. } => infallible(expr),
        Node::Like {
            expr,
            pattern: LikeArg::Const(_),
            ..
        } => infallible(expr),
        Node::Binary { op, left, right } => {
            (matches!(op, BinaryOp::And | BinaryOp::Or) || is_comparison(*op))
                && infallible(left)
                && infallible(right)
        }
        Node::InList { expr, list, .. } => infallible(expr) && list.iter().all(infallible),
        _ => false,
    }
}

/// Relative cost of evaluating `e` per row, to order the conjuncts of a filter: fixed-width
/// comparisons first (equality before ranges), then strings, then `LIKE` by pattern shape.
fn cost(e: &Expr) -> u32 {
    let compare = |ty: DataType| match ty {
        DataType::Varchar => 8,
        DataType::Int128 | DataType::Decimal { .. } => 3,
        _ => 2,
    };
    match &e.node {
        Node::Column(_) | Node::Literal(_) => 0,
        Node::Not(inner) => cost(inner),
        Node::IsNull { expr, .. } => 1 + cost(expr),
        Node::Like { expr, pattern, .. } => {
            let pattern = match pattern {
                LikeArg::Const(Some(p)) => p.cost(),
                LikeArg::Const(None) => 0,
                LikeArg::Dynamic(p) => 64 + cost(p),
            };
            pattern + cost(expr)
        }
        Node::Binary { op, left, right } if is_comparison(*op) => {
            compare(left.ty) + u32::from(*op != BinaryOp::Eq) + cost(left) + cost(right)
        }
        Node::Binary { left, right, .. } => cost(left) + cost(right) + 1,
        Node::InList { expr, list, .. } => {
            cost(expr) + list.iter().map(|i| compare(i.ty) + cost(i)).sum::<u32>()
        }
        _ => 100,
    }
}

/// Every column `e` reads, possibly repeated.
fn visit_columns(e: &Expr, out: &mut Vec<usize>) {
    match &e.node {
        Node::Column(i) => out.push(*i),
        Node::Literal(_) => {}
        Node::Neg(x)
        | Node::Not(x)
        | Node::Cast(x)
        | Node::Length(x)
        | Node::Extract(_, x)
        | Node::DateTrunc(_, x) => visit_columns(x, out),
        Node::Binary { left, right, .. } => {
            visit_columns(left, out);
            visit_columns(right, out);
        }
        Node::Case { branches, else_ } => {
            for (c, v) in branches {
                visit_columns(c, out);
                visit_columns(v, out);
            }
            if let Some(x) = else_ {
                visit_columns(x, out);
            }
        }
        Node::Like { expr, pattern, .. } => {
            visit_columns(expr, out);
            if let LikeArg::Dynamic(p) = pattern {
                visit_columns(p, out);
            }
        }
        Node::InList { expr, list, .. } => {
            visit_columns(expr, out);
            list.iter().for_each(|i| visit_columns(i, out));
        }
        Node::IsNull { expr, .. } => visit_columns(expr, out),
        Node::Substring(args) | Node::RegexpReplace { args, .. } => {
            args.iter().for_each(|a| visit_columns(a, out))
        }
    }
}

/// The chunk rows among `sel` (all `n` rows without it), in order, for which `keep` holds.
#[inline]
fn filter_rows(n: usize, sel: Option<&[u32]>, mut keep: impl FnMut(usize) -> bool) -> Vec<u32> {
    match sel {
        None => (0..n as u32).filter(|&r| keep(r as usize)).collect(),
        Some(s) => s.iter().copied().filter(|&r| keep(r as usize)).collect(),
    }
}

/// `op` with its operands swapped (`c < x` is `x > c`).
fn flipped(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

/// Whether `order` (of the left operand against the right) satisfies comparison `op`.
#[inline]
fn satisfies(op: BinaryOp, order: Ordering) -> bool {
    match op {
        BinaryOp::Eq => order == Ordering::Equal,
        BinaryOp::NotEq => order != Ordering::Equal,
        BinaryOp::Lt => order == Ordering::Less,
        BinaryOp::LtEq => order != Ordering::Greater,
        BinaryOp::Gt => order == Ordering::Greater,
        _ => order != Ordering::Less,
    }
}

/// Rows of a fixed-width column whose value compares with `c` as `op` asks; NULL rows never.
fn compare_rows<T: Copy>(
    values: &[T],
    validity: Option<&Bitmap>,
    c: T,
    op: BinaryOp,
    sel: Option<&[u32]>,
    cmp: impl Fn(T, T) -> Ordering,
) -> Vec<u32> {
    let n = values.len();
    // The comparison is chosen outside the loop, so each loop is a plain scan.
    macro_rules! scan {
        ($test:expr) => {{
            let test = $test;
            match validity {
                None => filter_rows(n, sel, |r| test(values[r])),
                Some(valid) => filter_rows(n, sel, |r| valid.get(r) && test(values[r])),
            }
        }};
    }
    match op {
        BinaryOp::Eq => scan!(|x: T| cmp(x, c) == Ordering::Equal),
        BinaryOp::NotEq => scan!(|x: T| cmp(x, c) != Ordering::Equal),
        BinaryOp::Lt => scan!(|x: T| cmp(x, c) == Ordering::Less),
        BinaryOp::LtEq => scan!(|x: T| cmp(x, c) != Ordering::Greater),
        BinaryOp::Gt => scan!(|x: T| cmp(x, c) == Ordering::Greater),
        _ => scan!(|x: T| cmp(x, c) != Ordering::Less),
    }
}

/// Rows of a string column for which `test` holds, NULL rows never. With the codes of a
/// `Dict` block, `test` runs once per dictionary entry.
fn string_rows(
    v: &Vector,
    dict: Option<&DictCodes>,
    sel: Option<&[u32]>,
    entry_hits: impl Fn(&[u32], &[u8]) -> Vec<bool>,
    test: impl Fn(&[u8]) -> bool,
) -> Result<Vec<u32>> {
    let VectorData::Varchar { offsets, bytes } = &v.data else {
        return Err(mismatch("string predicate"));
    };
    let n = v.len();
    let valid = |r: usize| v.is_valid(r);
    if let Some(dict) = dict
        && let VectorData::Varchar {
            offsets: dict_offsets,
            bytes: dict_bytes,
        } = &dict.entries.data
        && dict.codes.len() == n
    {
        let hits = entry_hits(dict_offsets, dict_bytes);
        let codes = &dict.codes;
        return Ok(match &v.validity {
            None => filter_rows(n, sel, |r| hits[codes[r] as usize]),
            Some(_) => filter_rows(n, sel, |r| valid(r) && hits[codes[r] as usize]),
        });
    }
    let at = |r: usize| &bytes[offsets[r] as usize..offsets[r + 1] as usize];
    Ok(match &v.validity {
        None => filter_rows(n, sel, |r| test(at(r))),
        Some(_) => filter_rows(n, sel, |r| valid(r) && test(at(r))),
    })
}

/// Result of a node: a vector with one row per selected row (borrowed from the chunk when
/// possible) or a constant.
enum Datum<'a> {
    Vector(Cow<'a, Vector>),
    Scalar(Value),
}

impl Datum<'_> {
    fn into_vector(self, n: usize, ty: DataType) -> Result<Vector> {
        match self {
            Datum::Vector(v) => Ok(v.into_owned()),
            Datum::Scalar(v) => Vector::constant(ty, &v, n),
        }
    }
}

fn owned(v: Vector) -> Datum<'static> {
    Datum::Vector(Cow::Owned(v))
}

/// An operand of a binary kernel.
#[derive(Clone, Copy)]
enum Arg<'b> {
    V(&'b Vector),
    S(&'b Value),
}

impl Arg<'_> {
    fn len(self) -> Option<usize> {
        match self {
            Arg::V(v) => Some(v.len()),
            Arg::S(_) => None,
        }
    }
}

fn mismatch(what: &str) -> VetaError {
    VetaError::internal(format!("type mismatch in {what}"))
}

/// One side of a fixed-width kernel: a slice or a constant.
#[derive(Clone, Copy)]
enum Side<'a, T> {
    S(&'a [T]),
    C(T),
}

impl<T: Copy> Side<'_, T> {
    #[inline]
    fn get(&self, i: usize) -> T {
        match self {
            Side::S(x) => x[i],
            Side::C(c) => *c,
        }
    }
}

/// `f(a[i], b[i])` for every row, with the slice/constant combination chosen outside the
/// loop.
#[inline]
fn binary_loop<T: Copy, U>(
    a: Side<T>,
    b: Side<T>,
    n: usize,
    mut f: impl FnMut(T, T) -> U,
) -> Vec<U> {
    match (a, b) {
        (Side::S(x), Side::S(y)) => x.iter().zip(y).map(|(&p, &q)| f(p, q)).collect(),
        (Side::S(x), Side::C(c)) => x.iter().map(|&p| f(p, c)).collect(),
        (Side::C(c), Side::S(y)) => y.iter().map(|&q| f(c, q)).collect(),
        (Side::C(p), Side::C(q)) => (0..n).map(|_| f(p, q)).collect(),
    }
}

/// Typed side of a fixed-width variant.
macro_rules! side {
    ($arg:expr, $variant:ident, $what:expr) => {
        match $arg {
            Arg::V(v) => match &v.data {
                VectorData::$variant(x) => Side::S(x.as_slice()),
                _ => return Err(mismatch($what)),
            },
            Arg::S(s) => match s {
                Value::$variant(x) => Side::C(*x),
                _ => return Err(mismatch($what)),
            },
        }
    };
}

fn and_validity(a: Option<&Bitmap>, b: Option<&Bitmap>) -> Option<Bitmap> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (Some(x), Some(y)) => {
            let bytes: Vec<u8> = x
                .as_bytes()
                .iter()
                .zip(y.as_bytes())
                .map(|(p, q)| p & q)
                .collect();
            Bitmap::from_bytes(&bytes, x.len())
        }
    }
}

fn arg_validity(a: Arg<'_>) -> Option<&Bitmap> {
    match a {
        Arg::V(v) => v.validity.as_ref(),
        Arg::S(_) => None,
    }
}

fn is_valid(validity: Option<&Bitmap>, i: usize) -> bool {
    validity.is_none_or(|b| b.get(i))
}

/// Error if any row in `rows` (candidates from a kernel) is valid.
fn overflow_check(rows: &[usize], validity: Option<&Bitmap>, what: &str) -> Result<()> {
    if rows.iter().any(|&i| is_valid(validity, i)) {
        return Err(VetaError::user(format!("{what} overflow")));
    }
    Ok(())
}

/// Clears the validity of rows where `zero[i]` (division by zero → NULL).
fn null_where(validity: Option<Bitmap>, zero: &[bool]) -> Option<Bitmap> {
    if !zero.iter().any(|&z| z) {
        return validity;
    }
    let mut b = validity.unwrap_or_else(|| Bitmap::new_set(zero.len()));
    for (i, &z) in zero.iter().enumerate() {
        if z {
            b.set(i, false);
        }
    }
    Some(b)
}

trait CheckedInt: Copy + Default + PartialEq {
    fn add(self, o: Self) -> Option<Self>;
    fn sub(self, o: Self) -> Option<Self>;
    fn mul(self, o: Self) -> Option<Self>;
    fn rem(self, o: Self) -> Self;
    fn neg(self) -> Option<Self>;
}

macro_rules! checked_int {
    ($($t:ty),*) => {$(
        impl CheckedInt for $t {
            #[inline] fn add(self, o: Self) -> Option<Self> { self.checked_add(o) }
            #[inline] fn sub(self, o: Self) -> Option<Self> { self.checked_sub(o) }
            #[inline] fn mul(self, o: Self) -> Option<Self> { self.checked_mul(o) }
            #[inline] fn rem(self, o: Self) -> Self { if o == 0 { 0 } else { self.wrapping_rem(o) } }
            #[inline] fn neg(self) -> Option<Self> { self.checked_neg() }
        }
    )*};
}
checked_int!(i16, i32, i64, i128);

/// Integer `+ - * %` with overflow → error and `% 0` → NULL.
fn int_arith<T: CheckedInt>(
    op: BinaryOp,
    a: Side<T>,
    b: Side<T>,
    n: usize,
    validity: Option<Bitmap>,
) -> Result<(Vec<T>, Option<Bitmap>)> {
    let checked = |f: fn(T, T) -> Option<T>| -> Result<Vec<T>> {
        let mut overflow = false;
        let out = binary_loop(a, b, n, |x, y| match f(x, y) {
            Some(v) => v,
            None => {
                overflow = true;
                T::default()
            }
        });
        if overflow {
            let rows: Vec<usize> = (0..n)
                .filter(|&i| f(a.get(i), b.get(i)).is_none())
                .collect();
            overflow_check(&rows, validity.as_ref(), "integer")?;
        }
        Ok(out)
    };
    match op {
        BinaryOp::Add => Ok((checked(T::add)?, validity)),
        BinaryOp::Sub => Ok((checked(T::sub)?, validity)),
        BinaryOp::Mul => Ok((checked(T::mul)?, validity)),
        BinaryOp::Mod => {
            let out = binary_loop(a, b, n, T::rem);
            let zero = binary_loop(a, b, n, |_, y| y == T::default());
            Ok((out, null_where(validity, &zero)))
        }
        _ => Err(VetaError::internal(format!("{} on integers", op.symbol()))),
    }
}

fn float_arith(
    op: BinaryOp,
    a: Side<f64>,
    b: Side<f64>,
    n: usize,
    validity: Option<Bitmap>,
) -> Result<(Vec<f64>, Option<Bitmap>)> {
    Ok(match op {
        BinaryOp::Add => (binary_loop(a, b, n, |x, y| x + y), validity),
        BinaryOp::Sub => (binary_loop(a, b, n, |x, y| x - y), validity),
        BinaryOp::Mul => (binary_loop(a, b, n, |x, y| x * y), validity),
        BinaryOp::Div => {
            let out = binary_loop(a, b, n, |x, y| x / y);
            let zero = binary_loop(a, b, n, |_, y| y == 0.0);
            (out, null_where(validity, &zero))
        }
        BinaryOp::Mod => {
            let out = binary_loop(a, b, n, |x, y| x % y);
            let zero = binary_loop(a, b, n, |_, y| y == 0.0);
            (out, null_where(validity, &zero))
        }
        _ => return Err(VetaError::internal(format!("{} on floats", op.symbol()))),
    })
}

/// Unscaled decimal values of an operand, widened to `i128`.
enum Dec<'b> {
    V(Cow<'b, [i128]>),
    C(i128),
}

impl Dec<'_> {
    fn side(&self) -> Side<'_, i128> {
        match self {
            Dec::V(v) => Side::S(v),
            Dec::C(c) => Side::C(*c),
        }
    }
}

/// Decimal view of an operand and its scale; integers are decimals of scale 0.
fn decimal_operand(arg: Arg<'_>) -> Result<(Dec<'_>, u8)> {
    match arg {
        Arg::S(v) => Ok(match v {
            Value::Decimal { value, scale, .. } => (Dec::C(*value), *scale),
            other => (
                Dec::C(other.as_i128().ok_or_else(|| mismatch("decimal"))?),
                0,
            ),
        }),
        Arg::V(v) => {
            let widen = |x: &[i64]| Dec::V(Cow::Owned(x.iter().map(|&x| x as i128).collect()));
            Ok(match &v.data {
                VectorData::Decimal128 { values, scale, .. } => {
                    (Dec::V(Cow::Borrowed(values.as_slice())), *scale)
                }
                VectorData::Decimal64 { values, scale, .. } => (widen(values), *scale),
                VectorData::Int16(x) => (
                    Dec::V(Cow::Owned(x.iter().map(|&x| x as i128).collect())),
                    0,
                ),
                VectorData::Int32(x) => (
                    Dec::V(Cow::Owned(x.iter().map(|&x| x as i128).collect())),
                    0,
                ),
                VectorData::Int64(x) => (widen(x), 0),
                VectorData::Int128(x) => (Dec::V(Cow::Borrowed(x.as_slice())), 0),
                _ => return Err(mismatch("decimal")),
            })
        }
    }
}

/// Rescales every value (rounding half away from zero when the scale decreases).
fn rescale_dec(d: Dec<'_>, from: u8, to: u8) -> Result<Dec<'_>> {
    if from == to {
        return Ok(d);
    }
    Ok(match d {
        Dec::C(c) => Dec::C(decimal::rescale(c, from, to)?),
        Dec::V(v) => Dec::V(Cow::Owned(
            v.iter()
                .map(|&x| decimal::rescale(x, from, to))
                .collect::<Result<Vec<_>>>()?,
        )),
    })
}

/// Builds a decimal vector of `(precision, scale)` from unscaled values.
fn decimal_data(values: Vec<i128>, precision: u8, scale: u8) -> VectorData {
    if precision <= MAX_DECIMAL64_PRECISION {
        VectorData::Decimal64 {
            values: values.into_iter().map(|v| v as i64).collect(),
            precision,
            scale,
        }
    } else {
        VectorData::Decimal128 {
            values,
            precision,
            scale,
        }
    }
}

/// Exact decimal `+ - *` (§4): operands at their own scales, result at `out`'s scale and
/// checked against its precision.
fn decimal_arith(
    op: BinaryOp,
    l: Arg<'_>,
    r: Arg<'_>,
    out: DataType,
    n: usize,
    validity: Option<Bitmap>,
) -> Result<Vector> {
    let DataType::Decimal { precision, scale } = out else {
        return Err(mismatch("decimal arithmetic"));
    };
    let (mut da, mut sa) = decimal_operand(l)?;
    let (mut db, mut sb) = decimal_operand(r)?;
    // Operands with a scale above the result scale are rounded first (only for `+ -`).
    if op != BinaryOp::Mul {
        if sa > scale {
            da = rescale_dec(da, sa, scale)?;
            sa = scale;
        }
        if sb > scale {
            db = rescale_dec(db, sb, scale)?;
            sb = scale;
        }
    }
    let (a, b) = (da.side(), db.side());
    let limit = decimal::pow10(precision.min(38)) as u128;
    let fits = move |v: i128| v.unsigned_abs() < limit;
    let mut overflow = false;
    let values: Vec<i128> = match op {
        BinaryOp::Add | BinaryOp::Sub => {
            let fa = decimal::pow10(scale - sa);
            let fb = decimal::pow10(scale - sb);
            let sub = op == BinaryOp::Sub;
            let f = move |x: i128, y: i128| -> Option<i128> {
                let x = x.checked_mul(fa)?;
                let y = y.checked_mul(fb)?;
                let v = if sub {
                    x.checked_sub(y)?
                } else {
                    x.checked_add(y)?
                };
                fits(v).then_some(v)
            };
            let out = if fa == 1 && fb == 1 {
                if sub {
                    binary_loop(a, b, n, |x, y| {
                        match x.checked_sub(y).filter(|&v| fits(v)) {
                            Some(v) => v,
                            None => {
                                overflow = true;
                                0
                            }
                        }
                    })
                } else {
                    binary_loop(a, b, n, |x, y| {
                        match x.checked_add(y).filter(|&v| fits(v)) {
                            Some(v) => v,
                            None => {
                                overflow = true;
                                0
                            }
                        }
                    })
                }
            } else {
                binary_loop(a, b, n, |x, y| match f(x, y) {
                    Some(v) => v,
                    None => {
                        overflow = true;
                        0
                    }
                })
            };
            if overflow {
                let rows: Vec<usize> = (0..n)
                    .filter(|&i| f(a.get(i), b.get(i)).is_none())
                    .collect();
                overflow_check(&rows, validity.as_ref(), "decimal")?;
            }
            out
        }
        BinaryOp::Mul => {
            let natural = sa + sb;
            let f = move |x: i128, y: i128| -> Option<i128> {
                let p = x.checked_mul(y)?;
                let p = if natural == scale {
                    p
                } else {
                    decimal::rescale(p, natural, scale).ok()?
                };
                fits(p).then_some(p)
            };
            let out = binary_loop(a, b, n, |x, y| match f(x, y) {
                Some(v) => v,
                None => {
                    overflow = true;
                    0
                }
            });
            if overflow {
                let rows: Vec<usize> = (0..n)
                    .filter(|&i| f(a.get(i), b.get(i)).is_none())
                    .collect();
                overflow_check(&rows, validity.as_ref(), "decimal")?;
            }
            out
        }
        _ => return Err(VetaError::internal(format!("{} on decimals", op.symbol()))),
    };
    Vector::new(decimal_data(values, precision, scale), validity)
}

/// `+ - * / %` on two non-NULL-constant operands with the result type `out`.
fn arithmetic(op: BinaryOp, l: Arg<'_>, r: Arg<'_>, out: DataType) -> Result<Vector> {
    let n = l.len().or(r.len()).unwrap_or(1);
    let validity = and_validity(arg_validity(l), arg_validity(r));
    let (data, validity) = match out {
        DataType::Int64 => {
            let (v, b) = int_arith(
                op,
                side!(l, Int64, "integer arithmetic"),
                side!(r, Int64, "integer arithmetic"),
                n,
                validity,
            )?;
            (VectorData::Int64(v), b)
        }
        DataType::Int128 => {
            let (v, b) = int_arith(
                op,
                side!(l, Int128, "integer arithmetic"),
                side!(r, Int128, "integer arithmetic"),
                n,
                validity,
            )?;
            (VectorData::Int128(v), b)
        }
        DataType::Int32 => {
            let (v, b) = int_arith(
                op,
                side!(l, Int32, "integer arithmetic"),
                side!(r, Int32, "integer arithmetic"),
                n,
                validity,
            )?;
            (VectorData::Int32(v), b)
        }
        DataType::Int16 => {
            let (v, b) = int_arith(
                op,
                side!(l, Int16, "integer arithmetic"),
                side!(r, Int16, "integer arithmetic"),
                n,
                validity,
            )?;
            (VectorData::Int16(v), b)
        }
        DataType::Float64 => {
            let (v, b) = float_arith(
                op,
                side!(l, Float64, "float arithmetic"),
                side!(r, Float64, "float arithmetic"),
                n,
                validity,
            )?;
            (VectorData::Float64(v), b)
        }
        DataType::Decimal { .. } => return decimal_arith(op, l, r, out, n, validity),
        _ => return Err(VetaError::internal(format!("arithmetic producing {out}"))),
    };
    Vector::new(data, validity)
}

/// Comparison with the operator chosen outside the loop.
fn cmp_loop<T: Copy>(
    op: BinaryOp,
    a: Side<T>,
    b: Side<T>,
    n: usize,
    cmp: impl Fn(T, T) -> Ordering,
) -> Result<Vec<bool>> {
    Ok(match op {
        BinaryOp::Eq => binary_loop(a, b, n, |x, y| cmp(x, y) == Ordering::Equal),
        BinaryOp::NotEq => binary_loop(a, b, n, |x, y| cmp(x, y) != Ordering::Equal),
        BinaryOp::Lt => binary_loop(a, b, n, |x, y| cmp(x, y) == Ordering::Less),
        BinaryOp::LtEq => binary_loop(a, b, n, |x, y| cmp(x, y) != Ordering::Greater),
        BinaryOp::Gt => binary_loop(a, b, n, |x, y| cmp(x, y) == Ordering::Greater),
        BinaryOp::GtEq => binary_loop(a, b, n, |x, y| cmp(x, y) != Ordering::Less),
        _ => {
            return Err(VetaError::internal(format!(
                "{} is not a comparison",
                op.symbol()
            )));
        }
    })
}

/// Strings of a `Varchar` operand.
#[derive(Clone, Copy)]
enum StrSide<'a> {
    S(&'a [u32], &'a [u8]),
    C(&'a [u8]),
}

impl<'a> StrSide<'a> {
    #[inline]
    fn get(&self, i: usize) -> &'a [u8] {
        match *self {
            StrSide::S(offsets, bytes) => &bytes[offsets[i] as usize..offsets[i + 1] as usize],
            StrSide::C(c) => c,
        }
    }
}

fn str_side(arg: Arg<'_>) -> Result<StrSide<'_>> {
    match arg {
        Arg::V(v) => match &v.data {
            VectorData::Varchar { offsets, bytes } => Ok(StrSide::S(offsets, bytes)),
            _ => Err(mismatch("string comparison")),
        },
        Arg::S(Value::Varchar(s)) => Ok(StrSide::C(s)),
        Arg::S(_) => Err(mismatch("string comparison")),
    }
}

fn str_cmp(op: BinaryOp, a: StrSide<'_>, b: StrSide<'_>, n: usize) -> Result<Vec<bool>> {
    Ok(match op {
        BinaryOp::Eq => (0..n).map(|i| a.get(i) == b.get(i)).collect(),
        BinaryOp::NotEq => (0..n).map(|i| a.get(i) != b.get(i)).collect(),
        BinaryOp::Lt => (0..n).map(|i| a.get(i) < b.get(i)).collect(),
        BinaryOp::LtEq => (0..n).map(|i| a.get(i) <= b.get(i)).collect(),
        BinaryOp::Gt => (0..n).map(|i| a.get(i) > b.get(i)).collect(),
        BinaryOp::GtEq => (0..n).map(|i| a.get(i) >= b.get(i)).collect(),
        _ => {
            return Err(VetaError::internal(format!(
                "{} is not a comparison",
                op.symbol()
            )));
        }
    })
}

/// Row-by-row comparison through [`Value`] for operand types the binder never pairs.
fn generic_cmp(op: BinaryOp, l: Arg<'_>, r: Arg<'_>, n: usize) -> Result<Vec<bool>> {
    let get = |a: Arg<'_>, i: usize| match a {
        Arg::V(v) => v.data.get(i),
        Arg::S(s) => s.clone(),
    };
    let test: fn(Ordering) -> bool = match op {
        BinaryOp::Eq => |o| o == Ordering::Equal,
        BinaryOp::NotEq => |o| o != Ordering::Equal,
        BinaryOp::Lt => |o| o == Ordering::Less,
        BinaryOp::LtEq => |o| o != Ordering::Greater,
        BinaryOp::Gt => |o| o == Ordering::Greater,
        BinaryOp::GtEq => |o| o != Ordering::Less,
        _ => {
            return Err(VetaError::internal(format!(
                "{} is not a comparison",
                op.symbol()
            )));
        }
    };
    Ok((0..n).map(|i| test(get(l, i).cmp(&get(r, i)))).collect())
}

/// `= <> < <= > >=` of two non-NULL-constant operands.
fn compare(op: BinaryOp, l: Arg<'_>, r: Arg<'_>, lty: DataType, rty: DataType) -> Result<Vector> {
    let n = l.len().or(r.len()).unwrap_or(1);
    let validity = and_validity(arg_validity(l), arg_validity(r));
    const W: &str = "comparison";
    let values = match (lty, rty) {
        (DataType::Boolean, DataType::Boolean) => cmp_loop(
            op,
            side!(l, Boolean, W),
            side!(r, Boolean, W),
            n,
            |x: bool, y| x.cmp(&y),
        )?,
        (DataType::Int16, DataType::Int16) => cmp_loop(
            op,
            side!(l, Int16, W),
            side!(r, Int16, W),
            n,
            |x: i16, y| x.cmp(&y),
        )?,
        (DataType::Int32, DataType::Int32) => cmp_loop(
            op,
            side!(l, Int32, W),
            side!(r, Int32, W),
            n,
            |x: i32, y| x.cmp(&y),
        )?,
        (DataType::Int64, DataType::Int64) => cmp_loop(
            op,
            side!(l, Int64, W),
            side!(r, Int64, W),
            n,
            |x: i64, y| x.cmp(&y),
        )?,
        (DataType::Int128, DataType::Int128) => cmp_loop(
            op,
            side!(l, Int128, W),
            side!(r, Int128, W),
            n,
            |x: i128, y| x.cmp(&y),
        )?,
        (DataType::Float64, DataType::Float64) => {
            cmp_loop(op, side!(l, Float64, W), side!(r, Float64, W), n, cmp_f64)?
        }
        (DataType::Date, DataType::Date) => {
            cmp_loop(op, side!(l, Date, W), side!(r, Date, W), n, |x: i32, y| {
                x.cmp(&y)
            })?
        }
        (DataType::Timestamp, DataType::Timestamp) => cmp_loop(
            op,
            side!(l, Timestamp, W),
            side!(r, Timestamp, W),
            n,
            |x: i64, y| x.cmp(&y),
        )?,
        (DataType::Varchar, DataType::Varchar) => str_cmp(op, str_side(l)?, str_side(r)?, n)?,
        (DataType::Decimal { .. }, DataType::Decimal { .. }) => {
            let (da, sa) = decimal_operand(l)?;
            let (db, sb) = decimal_operand(r)?;
            let (a, b) = (da.side(), db.side());
            if sa == sb {
                cmp_loop(op, a, b, n, |x: i128, y| x.cmp(&y))?
            } else {
                cmp_loop(op, a, b, n, |x, y| cmp_scaled(x, sa, y, sb))?
            }
        }
        _ => generic_cmp(op, l, r, n)?,
    };
    Vector::new(VectorData::Boolean(values), validity)
}

/// Three-valued `AND` / `OR`.
fn logic(op: BinaryOp, a: &Vector, b: &Vector) -> Result<Vector> {
    let (VectorData::Boolean(x), VectorData::Boolean(y)) = (&a.data, &b.data) else {
        return Err(mismatch("AND/OR"));
    };
    let n = a.len();
    if a.validity.is_none() && b.validity.is_none() {
        let values = match op {
            BinaryOp::And => x.iter().zip(y).map(|(&p, &q)| p && q).collect(),
            _ => x.iter().zip(y).map(|(&p, &q)| p || q).collect(),
        };
        return Ok(Vector::from_data(VectorData::Boolean(values)));
    }
    let mut values = Vec::with_capacity(n);
    let mut validity = Bitmap::new_set(n);
    let and = op == BinaryOp::And;
    for i in 0..n {
        let (av, bv) = (a.is_valid(i), b.is_valid(i));
        let (at, af) = (av && x[i], av && !x[i]);
        let (bt, bf) = (bv && y[i], bv && !y[i]);
        let (value, valid) = if and {
            (at && bt, af || bf || (at && bt))
        } else {
            (at || bt, at || bt || (af && bf))
        };
        values.push(value);
        if !valid {
            validity.set(i, false);
        }
    }
    let validity = (!validity.all_set()).then_some(validity);
    Vector::new(VectorData::Boolean(values), validity)
}

fn not(v: &Vector) -> Result<Vector> {
    let VectorData::Boolean(x) = &v.data else {
        return Err(mismatch("NOT"));
    };
    Vector::new(
        VectorData::Boolean(x.iter().map(|&b| !b).collect()),
        v.validity.clone(),
    )
}

fn negate(v: &Vector) -> Result<Vector> {
    fn neg_ints<T: CheckedInt>(x: &[T], v: &Vector) -> Result<Vec<T>> {
        let mut overflow = Vec::new();
        let out = x
            .iter()
            .enumerate()
            .map(|(i, &a)| {
                a.neg().unwrap_or_else(|| {
                    overflow.push(i);
                    T::default()
                })
            })
            .collect();
        overflow_check(&overflow, v.validity.as_ref(), "integer")?;
        Ok(out)
    }
    let data = match &v.data {
        VectorData::Int16(x) => VectorData::Int16(neg_ints(x, v)?),
        VectorData::Int32(x) => VectorData::Int32(neg_ints(x, v)?),
        VectorData::Int64(x) => VectorData::Int64(neg_ints(x, v)?),
        VectorData::Int128(x) => VectorData::Int128(neg_ints(x, v)?),
        VectorData::Float64(x) => VectorData::Float64(x.iter().map(|&a| -a).collect()),
        VectorData::Decimal64 {
            values,
            precision,
            scale,
        } => VectorData::Decimal64 {
            values: values.iter().map(|&a| -a).collect(),
            precision: *precision,
            scale: *scale,
        },
        VectorData::Decimal128 {
            values,
            precision,
            scale,
        } => VectorData::Decimal128 {
            values: values.iter().map(|&a| -a).collect(),
            precision: *precision,
            scale: *scale,
        },
        _ => return Err(mismatch("negation")),
    };
    Vector::new(data, v.validity.clone())
}

fn is_null(v: &Vector, negated: bool) -> Vector {
    let values = match &v.validity {
        None => vec![negated; v.len()],
        Some(b) => (0..v.len()).map(|i| b.get(i) == negated).collect(),
    };
    Vector::from_data(VectorData::Boolean(values))
}

fn like_vector(pattern: &LikePattern, v: &Vector, negated: bool) -> Result<Vector> {
    let VectorData::Varchar { offsets, bytes } = &v.data else {
        return Err(mismatch("LIKE"));
    };
    let mut values = pattern.matches_all(offsets, bytes);
    if negated {
        values.iter_mut().for_each(|b| *b = !*b);
    }
    Vector::new(VectorData::Boolean(values), v.validity.clone())
}

/// Builds a vector whose row `i` is row `which[i].1` of `pieces[which[i].0]`.
fn interleave(pieces: &[Vector], which: &[(u32, u32)], ty: DataType) -> Result<Vector> {
    macro_rules! fixed {
        ($variant:ident) => {{
            let slices = pieces
                .iter()
                .map(|p| match &p.data {
                    VectorData::$variant(x) => Ok(x.as_slice()),
                    _ => Err(mismatch("CASE")),
                })
                .collect::<Result<Vec<_>>>()?;
            VectorData::$variant(
                which
                    .iter()
                    .map(|&(p, j)| slices[p as usize][j as usize])
                    .collect(),
            )
        }};
    }
    let data = match ty {
        DataType::Boolean => fixed!(Boolean),
        DataType::Int16 => fixed!(Int16),
        DataType::Int32 => fixed!(Int32),
        DataType::Int64 => fixed!(Int64),
        DataType::Int128 => fixed!(Int128),
        DataType::Float64 => fixed!(Float64),
        DataType::Date => fixed!(Date),
        DataType::Timestamp => fixed!(Timestamp),
        DataType::Decimal { precision, scale } => {
            let slices = pieces
                .iter()
                .map(|p| match &p.data {
                    VectorData::Decimal64 {
                        values, scale: s, ..
                    } if *s == scale => Ok(Cow::Owned(
                        values.iter().map(|&v| v as i128).collect::<Vec<_>>(),
                    )),
                    VectorData::Decimal128 {
                        values, scale: s, ..
                    } if *s == scale => Ok(Cow::Borrowed(values.as_slice())),
                    _ => Err(mismatch("CASE")),
                })
                .collect::<Result<Vec<Cow<[i128]>>>>()?;
            let values = which
                .iter()
                .map(|&(p, j)| slices[p as usize][j as usize])
                .collect();
            decimal_data(values, precision, scale)
        }
        DataType::Varchar => {
            let views = pieces
                .iter()
                .map(|p| match &p.data {
                    VectorData::Varchar { offsets, bytes } => Ok(StrSide::S(offsets, bytes)),
                    _ => Err(mismatch("CASE")),
                })
                .collect::<Result<Vec<_>>>()?;
            let mut out = functions::StringBuilder::with_capacity(which.len(), 0);
            for &(p, j) in which {
                out.push(views[p as usize].get(j as usize))?;
            }
            out.finish()
        }
    };
    let validity = if pieces.iter().any(|p| p.validity.is_some()) {
        let mut b = Bitmap::new_set(which.len());
        for (i, &(p, j)) in which.iter().enumerate() {
            if !pieces[p as usize].is_valid(j as usize) {
                b.set(i, false);
            }
        }
        (!b.all_set()).then_some(b)
    } else {
        None
    };
    Vector::new(data, validity)
}

/// Positions `k` (into a boolean datum of `n` rows) whose value is true and not NULL.
fn true_positions(d: &Datum<'_>, n: usize) -> Result<Vec<u32>> {
    match d {
        Datum::Scalar(Value::Boolean(true)) => Ok((0..n as u32).collect()),
        Datum::Scalar(Value::Boolean(false) | Value::Null) => Ok(Vec::new()),
        Datum::Scalar(_) => Err(mismatch("predicate")),
        Datum::Vector(v) => {
            let VectorData::Boolean(b) = &v.data else {
                return Err(mismatch("predicate"));
            };
            Ok(match &v.validity {
                None => (0..n as u32).filter(|&k| b[k as usize]).collect(),
                Some(valid) => (0..n as u32)
                    .filter(|&k| b[k as usize] && valid.get(k as usize))
                    .collect(),
            })
        }
    }
}

impl CompiledExpr {
    pub fn new(expr: &BoundExpr) -> Result<CompiledExpr> {
        Ok(CompiledExpr {
            root: compile(expr)?,
        })
    }

    pub fn data_type(&self) -> DataType {
        self.root.ty
    }

    /// The columns the expression reads, sorted, each once.
    pub fn columns(&self) -> Vec<usize> {
        let mut columns = Vec::new();
        visit_columns(&self.root, &mut columns);
        columns.sort_unstable();
        columns.dedup();
        columns
    }

    /// Evaluates over the selected rows of `chunk`; one output row per selected row.
    pub fn eval(&self, chunk: &Chunk, selection: Option<&[u32]>) -> Result<Vector> {
        let n = selection.map_or(chunk.len, <[u32]>::len);
        self.node(&self.root, chunk, selection)?
            .into_vector(n, self.root.ty)
    }

    /// Chunk rows (among `selection`, keeping its order) where the predicate is true.
    /// `AND` and `OR` short-circuit: the right side only sees the rows still undecided.
    pub fn eval_predicate(&self, chunk: &Chunk, selection: Option<&[u32]>) -> Result<Vec<u32>> {
        self.eval_predicate_with_dicts(chunk, &[], selection)
    }

    /// [`CompiledExpr::eval_predicate`] with the dictionary codes of the `Dict`-encoded
    /// string columns of `chunk` (`dicts` empty or one entry per column).
    pub fn eval_predicate_with_dicts(
        &self,
        chunk: &Chunk,
        dicts: &[Option<Arc<DictCodes>>],
        selection: Option<&[u32]>,
    ) -> Result<Vec<u32>> {
        if self.root.ty != DataType::Boolean {
            return Err(VetaError::internal("predicate is not BOOLEAN"));
        }
        self.select(&self.root, chunk, dicts, selection)
    }

    /// The general evaluation of a predicate leaf, for tests of the in-place paths.
    #[cfg(test)]
    fn select_general(&self, chunk: &Chunk, sel: Option<&[u32]>) -> Result<Vec<u32>> {
        let n = sel.map_or(chunk.len, <[u32]>::len);
        let d = self.node(&self.root, chunk, sel)?;
        let positions = true_positions(&d, n)?;
        Ok(match sel {
            Some(s) => positions.into_iter().map(|k| s[k as usize]).collect(),
            None => positions,
        })
    }

    /// A leaf predicate over one column evaluated on the selected rows in place; `None`
    /// when `e` has another shape and takes the general path.
    fn select_leaf(
        &self,
        e: &Expr,
        chunk: &Chunk,
        dicts: &[Option<Arc<DictCodes>>],
        sel: Option<&[u32]>,
    ) -> Result<Option<Vec<u32>>> {
        let column = |i: usize| {
            chunk
                .columns
                .get(i)
                .ok_or_else(|| VetaError::internal(format!("column {i} out of range")))
        };
        let dict = |i: usize| dicts.get(i).and_then(Option::as_deref);
        match &e.node {
            Node::IsNull { expr, negated } => {
                let Node::Column(i) = expr.node else {
                    return Ok(None);
                };
                let v = column(i)?;
                Ok(Some(filter_rows(chunk.len, sel, |r| {
                    v.is_valid(r) == *negated
                })))
            }
            Node::Like {
                expr,
                pattern: LikeArg::Const(Some(p)),
                negated,
            } => {
                let Node::Column(i) = expr.node else {
                    return Ok(None);
                };
                let v = column(i)?;
                // A whole column without codes searches its buffer at once (`matches_all`).
                if sel.is_none() && dict(i).is_none() {
                    return Ok(None);
                }
                let negated = *negated;
                string_rows(
                    v,
                    dict(i),
                    sel,
                    |offsets, bytes| {
                        let mut hits = p.matches_all(offsets, bytes);
                        if negated {
                            hits.iter_mut().for_each(|h| *h = !*h);
                        }
                        hits
                    },
                    |s| p.matches(s) != negated,
                )
                .map(Some)
            }
            Node::Binary { op, left, right } if is_comparison(*op) && left.ty == right.ty => {
                let (i, value, op) = match (&left.node, &right.node) {
                    (Node::Column(i), Node::Literal(v)) => (*i, v, *op),
                    (Node::Literal(v), Node::Column(i)) => (*i, v, flipped(*op)),
                    _ => return Ok(None),
                };
                let v = column(i)?;
                let validity = v.validity.as_ref();
                Ok(match (&v.data, value) {
                    (VectorData::Varchar { .. }, Value::Varchar(lit)) => Some(string_rows(
                        v,
                        dict(i),
                        sel,
                        |offsets, bytes| {
                            offsets
                                .windows(2)
                                .map(|w| {
                                    let s = &bytes[w[0] as usize..w[1] as usize];
                                    satisfies(op, s.cmp(lit.as_slice()))
                                })
                                .collect()
                        },
                        |s| satisfies(op, s.cmp(lit.as_slice())),
                    )?),
                    (VectorData::Int16(x), Value::Int16(c)) => {
                        Some(compare_rows(x, validity, *c, op, sel, |a: i16, b| {
                            a.cmp(&b)
                        }))
                    }
                    (VectorData::Int32(x), Value::Int32(c))
                    | (VectorData::Date(x), Value::Date(c)) => {
                        Some(compare_rows(x, validity, *c, op, sel, |a: i32, b| {
                            a.cmp(&b)
                        }))
                    }
                    (VectorData::Int64(x), Value::Int64(c))
                    | (VectorData::Timestamp(x), Value::Timestamp(c)) => {
                        Some(compare_rows(x, validity, *c, op, sel, |a: i64, b| {
                            a.cmp(&b)
                        }))
                    }
                    (VectorData::Float64(x), Value::Float64(c)) => {
                        Some(compare_rows(x, validity, *c, op, sel, cmp_f64))
                    }
                    _ => None,
                })
            }
            _ => Ok(None),
        }
    }

    fn select(
        &self,
        e: &Expr,
        chunk: &Chunk,
        dicts: &[Option<Arc<DictCodes>>],
        sel: Option<&[u32]>,
    ) -> Result<Vec<u32>> {
        match &e.node {
            Node::Binary {
                op: BinaryOp::And,
                left,
                right,
            } => {
                let first = self.select(left, chunk, dicts, sel)?;
                if first.is_empty() {
                    return Ok(first);
                }
                self.select(right, chunk, dicts, Some(&first))
            }
            Node::Binary {
                op: BinaryOp::Or,
                left,
                right,
            } => {
                let rows: Cow<[u32]> = match sel {
                    Some(s) => Cow::Borrowed(s),
                    None => Cow::Owned((0..chunk.len as u32).collect()),
                };
                let first = self.select(left, chunk, dicts, Some(&rows))?;
                let mut rest = Vec::with_capacity(rows.len() - first.len());
                let mut k = 0;
                for &r in rows.iter() {
                    if k < first.len() && first[k] == r {
                        k += 1;
                    } else {
                        rest.push(r);
                    }
                }
                if rest.is_empty() {
                    return Ok(first);
                }
                let second = self.select(right, chunk, dicts, Some(&rest))?;
                let (mut i, mut j) = (0, 0);
                let mut out = Vec::with_capacity(first.len() + second.len());
                for &r in rows.iter() {
                    if i < first.len() && first[i] == r {
                        out.push(r);
                        i += 1;
                    } else if j < second.len() && second[j] == r {
                        out.push(r);
                        j += 1;
                    }
                }
                Ok(out)
            }
            _ => {
                if let Some(rows) = self.select_leaf(e, chunk, dicts, sel)? {
                    return Ok(rows);
                }
                let n = sel.map_or(chunk.len, <[u32]>::len);
                let d = self.node(e, chunk, sel)?;
                let positions = true_positions(&d, n)?;
                Ok(match sel {
                    Some(s) => positions.into_iter().map(|k| s[k as usize]).collect(),
                    None => positions,
                })
            }
        }
    }

    fn node<'a>(&self, e: &Expr, chunk: &'a Chunk, sel: Option<&[u32]>) -> Result<Datum<'a>> {
        let n = sel.map_or(chunk.len, <[u32]>::len);
        Ok(match &e.node {
            Node::Column(i) => {
                let col = chunk
                    .columns
                    .get(*i)
                    .ok_or_else(|| VetaError::internal(format!("column {i} out of range")))?;
                match sel {
                    None => Datum::Vector(Cow::Borrowed(col)),
                    Some(s) => owned(col.gather(s)),
                }
            }
            Node::Literal(v) => Datum::Scalar(v.clone()),
            Node::Neg(inner) => {
                let d = self.node(inner, chunk, sel)?;
                self.unary(d, inner.ty, negate)?
            }
            Node::Not(inner) => {
                let d = self.node(inner, chunk, sel)?;
                self.unary(d, inner.ty, not)?
            }
            Node::Cast(inner) => {
                let to = e.ty;
                let d = self.node(inner, chunk, sel)?;
                match d {
                    Datum::Scalar(v) => Datum::Scalar(cast_value(&v, to)?),
                    Datum::Vector(v) => owned(cast_vector(&v, to)?),
                }
            }
            Node::Binary { op, left, right } => {
                let l = self.node(left, chunk, sel)?;
                let r = self.node(right, chunk, sel)?;
                match op {
                    BinaryOp::And | BinaryOp::Or => logic_datum(*op, l, r, n)?,
                    BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq => {
                        let (lty, rty) = (left.ty, right.ty);
                        self.binary(l, r, lty, e.ty, |a, b| compare(*op, a, b, lty, rty))?
                    }
                    _ => {
                        // The binder converts operands to the result type, except decimals
                        // (each keeps its scale) and `/` (always on Float64).
                        let out = e.ty;
                        let compute = |d: Datum<'a>, ty: DataType| -> Result<Datum<'a>> {
                            if ty == out || matches!(out, DataType::Decimal { .. }) {
                                return Ok(d);
                            }
                            Ok(match d {
                                Datum::Scalar(v) => Datum::Scalar(cast_value(&v, out)?),
                                Datum::Vector(v) => owned(cast_vector(&v, out)?),
                            })
                        };
                        let lty = if matches!(out, DataType::Decimal { .. }) {
                            left.ty
                        } else {
                            out
                        };
                        let l = compute(l, left.ty)?;
                        let r = compute(r, right.ty)?;
                        self.binary(l, r, lty, out, |a, b| arithmetic(*op, a, b, out))?
                    }
                }
            }
            Node::IsNull { expr, negated } => match self.node(expr, chunk, sel)? {
                Datum::Scalar(v) => Datum::Scalar(Value::Boolean(v.is_null() != *negated)),
                Datum::Vector(v) => owned(is_null(&v, *negated)),
            },
            Node::Like {
                expr,
                pattern,
                negated,
            } => {
                let d = self.node(expr, chunk, sel)?;
                match pattern {
                    LikeArg::Const(None) => Datum::Scalar(Value::Null),
                    LikeArg::Const(Some(p)) => {
                        self.unary(d, DataType::Varchar, |v| like_vector(p, v, *negated))?
                    }
                    LikeArg::Dynamic(pe) => {
                        let pd = self.node(pe, chunk, sel)?;
                        let s = d.into_vector(n, DataType::Varchar)?;
                        let p = pd.into_vector(n, DataType::Varchar)?;
                        owned(like_rows(&s, &p, *negated)?)
                    }
                }
            }
            Node::InList {
                expr,
                list,
                negated,
            } => {
                let value = self.node(expr, chunk, sel)?;
                let mut acc: Option<Datum<'a>> = None;
                for item in list {
                    let d = self.node(item, chunk, sel)?;
                    let v = match &value {
                        Datum::Scalar(s) => Datum::Scalar(s.clone()),
                        Datum::Vector(v) => Datum::Vector(Cow::Borrowed(&**v)),
                    };
                    let hit = self.binary(v, d, expr.ty, DataType::Boolean, |a, b| {
                        compare(BinaryOp::Eq, a, b, expr.ty, item.ty)
                    })?;
                    acc = Some(match acc {
                        None => hit,
                        Some(prev) => logic_datum(BinaryOp::Or, prev, hit, n)?,
                    });
                }
                let out = acc.unwrap_or(Datum::Scalar(Value::Boolean(false)));
                if *negated {
                    self.unary(out, DataType::Boolean, not)?
                } else {
                    out
                }
            }
            Node::Case { branches, else_ } => {
                self.case(branches, else_.as_deref(), e.ty, chunk, sel)?
            }
            Node::Length(inner) => {
                let d = self.node(inner, chunk, sel)?;
                self.unary(d, inner.ty, functions::length)?
            }
            Node::Extract(part, inner) => {
                let d = self.node(inner, chunk, sel)?;
                self.unary(d, inner.ty, |v| functions::extract(*part, v))?
            }
            Node::DateTrunc(unit, inner) => {
                let d = self.node(inner, chunk, sel)?;
                self.unary(d, inner.ty, |v| functions::date_trunc(*unit, v))?
            }
            Node::Substring(args) => {
                let s = self.node(&args[0], chunk, sel)?;
                let from = self.int_arg(&args[1], chunk, sel)?;
                let len = args
                    .get(2)
                    .map(|a| self.int_arg(a, chunk, sel))
                    .transpose()?;
                let all_const =
                    matches!(from, IntDatum::Const(_)) && !matches!(len, Some(IntDatum::Vector(_)));
                if all_const {
                    self.unary(s, DataType::Varchar, |v| {
                        functions::substring(v, from.as_arg(), len.as_ref().map(IntDatum::as_arg))
                    })?
                } else {
                    let s = s.into_vector(n, DataType::Varchar)?;
                    owned(functions::substring(
                        &s,
                        from.as_arg(),
                        len.as_ref().map(IntDatum::as_arg),
                    )?)
                }
            }
            Node::RegexpReplace { args, compiled } => {
                let s = self.node(&args[0], chunk, sel)?;
                match compiled {
                    Some(rr) => self.unary(s, DataType::Varchar, |v| rr.apply(v))?,
                    None => {
                        let p = self.node(&args[1], chunk, sel)?;
                        let r = self.node(&args[2], chunk, sel)?;
                        let all_scalar = [&s, &p, &r].iter().all(|d| matches!(d, Datum::Scalar(_)));
                        let s = s.into_vector(if all_scalar { 1 } else { n }, DataType::Varchar)?;
                        let (pa, ra) = (StrArgOwned::from_datum(&p), StrArgOwned::from_datum(&r));
                        let out = functions::regexp_replace(&s, pa.as_arg(), ra.as_arg())?;
                        if all_scalar {
                            Datum::Scalar(out.get(0))
                        } else {
                            owned(out)
                        }
                    }
                }
            }
        })
    }

    /// Applies a vector kernel to a datum; a constant goes through a one-row vector.
    fn unary<'a>(
        &self,
        d: Datum<'a>,
        ty: DataType,
        f: impl FnOnce(&Vector) -> Result<Vector>,
    ) -> Result<Datum<'a>> {
        Ok(match d {
            Datum::Vector(v) => owned(f(&v)?),
            Datum::Scalar(Value::Null) => Datum::Scalar(Value::Null),
            Datum::Scalar(s) => {
                let v = Vector::from_values(ty, &[s])?;
                Datum::Scalar(f(&v)?.get(0))
            }
        })
    }

    /// Applies a binary kernel; a NULL constant gives all NULL, two constants go through a
    /// one-row vector, and a constant against a vector is passed as is.
    fn binary<'a>(
        &self,
        l: Datum<'_>,
        r: Datum<'_>,
        lty: DataType,
        out: DataType,
        f: impl FnOnce(Arg<'_>, Arg<'_>) -> Result<Vector>,
    ) -> Result<Datum<'a>> {
        let rows = |d: &Datum<'_>| match d {
            Datum::Vector(v) => Some(v.len()),
            Datum::Scalar(_) => None,
        };
        let n = rows(&l).or(rows(&r));
        let null_const =
            matches!(l, Datum::Scalar(Value::Null)) || matches!(r, Datum::Scalar(Value::Null));
        if null_const {
            return Ok(match n {
                Some(n) => owned(Vector::nulls(out, n)),
                None => Datum::Scalar(Value::Null),
            });
        }
        Ok(match (&l, &r) {
            (Datum::Scalar(a), Datum::Scalar(b)) => {
                let one = Vector::from_values(lty, std::slice::from_ref(a))?;
                Datum::Scalar(f(Arg::V(&one), Arg::S(b))?.get(0))
            }
            (Datum::Vector(a), Datum::Scalar(b)) => owned(f(Arg::V(a), Arg::S(b))?),
            (Datum::Scalar(a), Datum::Vector(b)) => owned(f(Arg::S(a), Arg::V(b))?),
            (Datum::Vector(a), Datum::Vector(b)) => owned(f(Arg::V(a), Arg::V(b))?),
        })
    }

    fn int_arg(&self, e: &Expr, chunk: &Chunk, sel: Option<&[u32]>) -> Result<IntDatum> {
        Ok(match self.node(e, chunk, sel)? {
            Datum::Scalar(Value::Null) => IntDatum::Const(None),
            Datum::Scalar(v) => IntDatum::Const(Some(
                v.as_i128()
                    .and_then(|x| i64::try_from(x).ok())
                    .ok_or_else(|| mismatch("integer argument"))?,
            )),
            Datum::Vector(v) => IntDatum::Vector(cast_vector(&v, DataType::Int64)?),
        })
    }

    /// `CASE`: each branch value is only evaluated on the rows its condition selects, so
    /// errors (overflow, invalid casts) of rows that take another branch do not surface.
    fn case<'a>(
        &self,
        branches: &[(Expr, Expr)],
        else_: Option<&Expr>,
        ty: DataType,
        chunk: &'a Chunk,
        sel: Option<&[u32]>,
    ) -> Result<Datum<'a>> {
        let all: Cow<[u32]> = match sel {
            Some(s) => Cow::Borrowed(s),
            None => Cow::Owned((0..chunk.len as u32).collect()),
        };
        let n = all.len();
        let mut remaining: Vec<u32> = (0..n as u32).collect();
        let mut pieces: Vec<Vector> = Vec::new();
        let mut which: Vec<(u32, u32)> = vec![(0, 0); n];
        for (cond, value) in branches {
            if remaining.is_empty() {
                break;
            }
            let rows: Vec<u32> = remaining.iter().map(|&k| all[k as usize]).collect();
            let truth = self.node(cond, chunk, Some(&rows))?;
            let hits = true_positions(&truth, rows.len())?;
            if hits.is_empty() {
                continue;
            }
            let hit_rows: Vec<u32> = hits.iter().map(|&h| rows[h as usize]).collect();
            let hit_positions: Vec<u32> = hits.iter().map(|&h| remaining[h as usize]).collect();
            let d = self.node(value, chunk, Some(&hit_rows))?;
            place(&mut pieces, &mut which, ty, d, &hit_positions)?;
            let mut h = 0;
            remaining = remaining
                .iter()
                .enumerate()
                .filter(|&(k, _)| {
                    if h < hits.len() && hits[h] as usize == k {
                        h += 1;
                        false
                    } else {
                        true
                    }
                })
                .map(|(_, &pos)| pos)
                .collect();
        }
        if !remaining.is_empty() {
            let d = match else_ {
                Some(e) => {
                    let rows: Vec<u32> = remaining.iter().map(|&k| all[k as usize]).collect();
                    self.node(e, chunk, Some(&rows))?
                }
                None => Datum::Scalar(Value::Null),
            };
            place(&mut pieces, &mut which, ty, d, &remaining)?;
        }
        Ok(owned(interleave(&pieces, &which, ty)?))
    }
}

/// Adds a `CASE` result piece for output positions `positions`.
fn place(
    pieces: &mut Vec<Vector>,
    which: &mut [(u32, u32)],
    ty: DataType,
    d: Datum<'_>,
    positions: &[u32],
) -> Result<()> {
    let p = pieces.len() as u32;
    match d {
        Datum::Scalar(v) => {
            pieces.push(Vector::from_values(ty, &[v])?);
            for &k in positions {
                which[k as usize] = (p, 0);
            }
        }
        Datum::Vector(v) => {
            pieces.push(v.into_owned());
            for (j, &k) in positions.iter().enumerate() {
                which[k as usize] = (p, j as u32);
            }
        }
    }
    Ok(())
}

/// Three-valued `AND` / `OR` of two boolean datums of `n` rows.
fn logic_datum(op: BinaryOp, l: Datum<'_>, r: Datum<'_>, n: usize) -> Result<Datum<'static>> {
    Ok(match (l, r) {
        (Datum::Scalar(a), Datum::Scalar(b)) => {
            let a = Vector::from_values(DataType::Boolean, &[a])?;
            let b = Vector::from_values(DataType::Boolean, &[b])?;
            Datum::Scalar(logic(op, &a, &b)?.get(0))
        }
        (l, r) => {
            let a = l.into_vector(n, DataType::Boolean)?;
            let b = r.into_vector(n, DataType::Boolean)?;
            owned(logic(op, &a, &b)?)
        }
    })
}

enum IntDatum {
    Const(Option<i64>),
    Vector(Vector),
}

enum StrArgOwned {
    Const(Option<Vec<u8>>),
    Vector(Vector),
}

impl IntDatum {
    fn as_arg(&self) -> IntArg<'_> {
        match self {
            IntDatum::Const(c) => IntArg::Const(*c),
            IntDatum::Vector(v) => IntArg::Vector(v),
        }
    }
}

impl StrArgOwned {
    fn from_datum(d: &Datum<'_>) -> StrArgOwned {
        match d {
            Datum::Scalar(Value::Varchar(b)) => StrArgOwned::Const(Some(b.clone())),
            Datum::Scalar(_) => StrArgOwned::Const(None),
            Datum::Vector(v) => StrArgOwned::Vector((**v).clone()),
        }
    }

    fn as_arg(&self) -> StrArg<'_> {
        match self {
            StrArgOwned::Const(c) => StrArg::Const(c.as_deref()),
            StrArgOwned::Vector(v) => StrArg::Vector(v),
        }
    }
}

/// `LIKE` with a pattern that varies by row; each distinct pattern is compiled when it
/// first appears in a run of rows.
fn like_rows(s: &Vector, p: &Vector, negated: bool) -> Result<Vector> {
    let n = s.len();
    let validity = and_validity(s.validity.as_ref(), p.validity.as_ref());
    let mut values = Vec::with_capacity(n);
    let mut last: Option<(Vec<u8>, LikePattern)> = None;
    for i in 0..n {
        if !is_valid(validity.as_ref(), i) {
            values.push(false);
            continue;
        }
        let (Some(text), Some(pat)) = (s.data.str_at(i), p.data.str_at(i)) else {
            return Err(mismatch("LIKE"));
        };
        if !matches!(&last, Some((lp, _)) if lp == pat) {
            last = Some((pat.to_vec(), LikePattern::new(pat)?));
        }
        let hit = last.as_ref().is_some_and(|(_, lp)| lp.matches(text));
        values.push(hit != negated);
    }
    Vector::new(VectorData::Boolean(values), validity)
}

#[cfg(test)]
mod tests;
