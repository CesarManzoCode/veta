use veta_core::datetime::{parse_date, parse_timestamp};

use super::*;

fn col(index: usize, ty: DataType) -> BoundExpr {
    BoundExpr::column(index, format!("c{index}"), ty, true)
}

fn lit(v: Value) -> BoundExpr {
    BoundExpr::literal(v)
}

fn bin(op: BinaryOp, l: BoundExpr, r: BoundExpr, ty: DataType) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Binary {
            op,
            left: Box::new(l),
            right: Box::new(r),
        },
        ty,
        true,
    )
}

fn unary(op: UnaryOp, e: BoundExpr) -> BoundExpr {
    let ty = e.data_type;
    BoundExpr::new(
        BoundExprKind::Unary {
            op,
            expr: Box::new(e),
        },
        ty,
        true,
    )
}

fn cast(e: BoundExpr, to: DataType) -> BoundExpr {
    BoundExpr::new(BoundExprKind::Cast(Box::new(e)), to, true)
}

fn func(f: ScalarFunc, args: Vec<BoundExpr>, ty: DataType) -> BoundExpr {
    BoundExpr::new(BoundExprKind::Function { func: f, args }, ty, true)
}

fn dec(value: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal {
        value,
        precision,
        scale,
    }
}

fn dect(precision: u8, scale: u8) -> DataType {
    DataType::Decimal { precision, scale }
}

fn chunk(columns: Vec<(DataType, Vec<Value>)>) -> Chunk {
    Chunk::new(
        columns
            .into_iter()
            .map(|(t, v)| Vector::from_values(t, &v).unwrap())
            .collect(),
    )
    .unwrap()
}

fn values(v: &Vector) -> Vec<Value> {
    (0..v.len()).map(|i| v.get(i)).collect()
}

fn run(e: &BoundExpr, c: &Chunk) -> Vec<Value> {
    values(&eval(e, c, None).unwrap())
}

fn b(x: Option<bool>) -> Value {
    x.map_or(Value::Null, Value::Boolean)
}

fn s(x: &str) -> Value {
    Value::varchar(x)
}

#[test]
fn three_valued_logic_truth_tables() {
    let all = [Some(true), Some(false), None];
    let mut left = Vec::new();
    let mut right = Vec::new();
    for x in all {
        for y in all {
            left.push(b(x));
            right.push(b(y));
        }
    }
    let c = chunk(vec![(DataType::Boolean, left), (DataType::Boolean, right)]);
    let (t, f, n) = (b(Some(true)), b(Some(false)), Value::Null);
    let and = bin(
        BinaryOp::And,
        col(0, DataType::Boolean),
        col(1, DataType::Boolean),
        DataType::Boolean,
    );
    // Rows: TT TF TN FT FF FN NT NF NN.
    assert_eq!(
        run(&and, &c),
        vec![
            t.clone(),
            f.clone(),
            n.clone(),
            f.clone(),
            f.clone(),
            f.clone(),
            n.clone(),
            f.clone(),
            n.clone()
        ]
    );
    let or = bin(
        BinaryOp::Or,
        col(0, DataType::Boolean),
        col(1, DataType::Boolean),
        DataType::Boolean,
    );
    assert_eq!(
        run(&or, &c),
        vec![
            t.clone(),
            t.clone(),
            t.clone(),
            t.clone(),
            f.clone(),
            n.clone(),
            t.clone(),
            n.clone(),
            n.clone()
        ]
    );
    let not = unary(UnaryOp::Not, col(0, DataType::Boolean));
    assert_eq!(
        &run(&not, &c)[..],
        &[
            f.clone(),
            f.clone(),
            f.clone(),
            t.clone(),
            t.clone(),
            t.clone(),
            n.clone(),
            n.clone(),
            n.clone()
        ][..]
    );

    // Predicates keep only true rows; the short-circuit gives the same rows.
    assert_eq!(eval_predicate(&and, &c, None).unwrap(), vec![0]);
    assert_eq!(eval_predicate(&or, &c, None).unwrap(), vec![0, 1, 2, 3, 6]);
    assert_eq!(eval_predicate(&not, &c, None).unwrap(), vec![3, 4, 5]);
    assert_eq!(
        eval_predicate(&or, &c, Some(&[8, 6, 4, 1])).unwrap(),
        vec![6, 1]
    );

    // Constants against a column.
    let and_null = bin(
        BinaryOp::And,
        col(0, DataType::Boolean),
        BoundExpr::typed_literal(Value::Null, DataType::Boolean),
        DataType::Boolean,
    );
    // Rows 2 and 3: TRUE AND NULL is NULL, FALSE AND NULL is FALSE.
    assert_eq!(&run(&and_null, &c)[2..4], &[n.clone(), f.clone()]);
    let or_true = bin(
        BinaryOp::Or,
        col(0, DataType::Boolean),
        lit(Value::Boolean(true)),
        DataType::Boolean,
    );
    assert!(run(&or_true, &c).iter().all(|v| *v == t));
}

#[test]
fn is_null_and_in_list_with_nulls() {
    let c = chunk(vec![(
        DataType::Int64,
        vec![Value::Int64(1), Value::Int64(2), Value::Null],
    )]);
    let x = || col(0, DataType::Int64);
    let is_null = BoundExpr::new(
        BoundExprKind::IsNull {
            expr: Box::new(x()),
            negated: false,
        },
        DataType::Boolean,
        false,
    );
    assert_eq!(
        run(&is_null, &c),
        vec![b(Some(false)), b(Some(false)), b(Some(true))]
    );
    let in_list = |negated| {
        BoundExpr::new(
            BoundExprKind::InList {
                expr: Box::new(x()),
                list: vec![
                    lit(Value::Int64(1)),
                    BoundExpr::typed_literal(Value::Null, DataType::Int64),
                ],
                negated,
            },
            DataType::Boolean,
            true,
        )
    };
    assert_eq!(
        run(&in_list(false), &c),
        vec![b(Some(true)), Value::Null, Value::Null]
    );
    assert_eq!(
        run(&in_list(true), &c),
        vec![b(Some(false)), Value::Null, Value::Null]
    );
    let plain = BoundExpr::new(
        BoundExprKind::InList {
            expr: Box::new(x()),
            list: vec![lit(Value::Int64(2)), lit(Value::Int64(3))],
            negated: true,
        },
        DataType::Boolean,
        true,
    );
    assert_eq!(
        run(&plain, &c),
        vec![b(Some(true)), b(Some(false)), Value::Null]
    );
}

#[test]
fn integer_arithmetic_overflow_is_an_error() {
    let c = chunk(vec![(
        DataType::Int64,
        vec![Value::Int64(i64::MAX), Value::Int64(1)],
    )]);
    let add = bin(
        BinaryOp::Add,
        col(0, DataType::Int64),
        lit(Value::Int64(1)),
        DataType::Int64,
    );
    let err = eval(&add, &c, None).unwrap_err();
    assert_eq!(err.exit_code(), 1);
    // Without the overflowing row there is no error.
    assert_eq!(
        values(&eval(&add, &c, Some(&[1])).unwrap()),
        vec![Value::Int64(2)]
    );
    let mul = bin(
        BinaryOp::Mul,
        col(0, DataType::Int64),
        col(0, DataType::Int64),
        DataType::Int64,
    );
    assert!(eval(&mul, &c, None).is_err());
    let c_min = chunk(vec![(DataType::Int64, vec![Value::Int64(i64::MIN)])]);
    assert!(eval(&unary(UnaryOp::Neg, col(0, DataType::Int64)), &c_min, None).is_err());
    let sub = bin(
        BinaryOp::Sub,
        col(0, DataType::Int64),
        lit(Value::Int64(1)),
        DataType::Int64,
    );
    assert!(eval(&sub, &c_min, None).is_err());

    // A NULL row whose buffer slot holds a value that would overflow is not an error.
    let hidden = Vector::new(
        VectorData::Int64(vec![i64::MAX, 3]),
        Some(Bitmap::from_bools(&[false, true])),
    )
    .unwrap();
    let c_null = Chunk::new(vec![hidden]).unwrap();
    assert_eq!(run(&add, &c_null), vec![Value::Null, Value::Int64(4)]);

    // Int128 (SUM(Int64) results) also checks.
    let c128 = chunk(vec![(DataType::Int128, vec![Value::Int128(i128::MAX)])]);
    let add128 = bin(
        BinaryOp::Add,
        col(0, DataType::Int128),
        lit(Value::Int128(1)),
        DataType::Int128,
    );
    assert!(eval(&add128, &c128, None).is_err());
}

#[test]
fn modulo_division_and_promotions() {
    let c = chunk(vec![
        (
            DataType::Int64,
            vec![Value::Int64(7), Value::Int64(-7), Value::Int64(5)],
        ),
        (
            DataType::Int64,
            vec![Value::Int64(3), Value::Int64(3), Value::Int64(0)],
        ),
        (
            DataType::Int32,
            vec![Value::Int32(10), Value::Int32(20), Value::Null],
        ),
    ]);
    let m = bin(
        BinaryOp::Mod,
        col(0, DataType::Int64),
        col(1, DataType::Int64),
        DataType::Int64,
    );
    assert_eq!(
        run(&m, &c),
        vec![Value::Int64(1), Value::Int64(-1), Value::Null]
    );
    let d = bin(
        BinaryOp::Div,
        cast(col(0, DataType::Int64), DataType::Float64),
        cast(col(1, DataType::Int64), DataType::Float64),
        DataType::Float64,
    );
    let out = run(&d, &c);
    assert_eq!(out[2], Value::Null);
    assert_eq!(out[0], Value::Float64(7.0 / 3.0));
    // Int32 promoted to Int64 by an explicit cast (the binder's form) or by the kernel.
    let p = bin(
        BinaryOp::Add,
        cast(col(2, DataType::Int32), DataType::Int64),
        lit(Value::Int64(1)),
        DataType::Int64,
    );
    assert_eq!(
        run(&p, &c),
        vec![Value::Int64(11), Value::Int64(21), Value::Null]
    );
    let q = bin(
        BinaryOp::Mul,
        col(2, DataType::Int32),
        lit(Value::Int64(2)),
        DataType::Int64,
    );
    assert_eq!(
        run(&q, &c),
        vec![Value::Int64(20), Value::Int64(40), Value::Null]
    );
    // Constant folding through the one-row path.
    let k = bin(
        BinaryOp::Sub,
        lit(Value::Int64(5)),
        lit(Value::Int64(8)),
        DataType::Int64,
    );
    assert_eq!(run(&k, &c), vec![Value::Int64(-3); 3]);
    let neg = unary(UnaryOp::Neg, col(0, DataType::Int64));
    assert_eq!(
        run(&neg, &c),
        vec![Value::Int64(-7), Value::Int64(7), Value::Int64(-5)]
    );
}

#[test]
fn decimal_arithmetic_and_comparison_with_different_scales() {
    // 1.25 and 2.50 as DECIMAL(15,2); literal 0.5 as DECIMAL(3,1).
    let c = chunk(vec![(
        dect(15, 2),
        vec![dec(125, 15, 2), dec(250, 15, 2), Value::Null],
    )]);
    let x = || col(0, dect(15, 2));
    let half = || lit(dec(5, 3, 1));
    let (p, sc) = decimal::add_type(15, 2, 3, 1).unwrap();
    let add = bin(BinaryOp::Add, x(), half(), dect(p, sc));
    assert_eq!(
        run(&add, &c),
        vec![dec(175, p, sc), dec(300, p, sc), Value::Null]
    );
    let sub = bin(BinaryOp::Sub, half(), x(), dect(p, sc));
    assert_eq!(
        run(&sub, &c),
        vec![dec(-75, p, sc), dec(-200, p, sc), Value::Null]
    );
    let (mp, ms) = decimal::mul_type(15, 2, 3, 1).unwrap();
    let mul = bin(BinaryOp::Mul, x(), half(), dect(mp, ms));
    assert_eq!(
        run(&mul, &c),
        vec![dec(625, mp, ms), dec(1250, mp, ms), Value::Null]
    );

    // TPC-H shape: l_extendedprice * (1 - l_discount).
    let disc = chunk(vec![
        (dect(15, 2), vec![dec(100_000, 15, 2)]),
        (dect(15, 2), vec![dec(5, 15, 2)]),
    ]);
    let one = lit(dec(1, 1, 0));
    let (p1, s1) = decimal::add_type(1, 0, 15, 2).unwrap();
    let one_minus = bin(BinaryOp::Sub, one, col(1, dect(15, 2)), dect(p1, s1));
    let (p2, s2) = decimal::mul_type(15, 2, p1, s1).unwrap();
    let price = bin(BinaryOp::Mul, col(0, dect(15, 2)), one_minus, dect(p2, s2));
    // 1000.00 * 0.95 = 950.0000
    assert_eq!(run(&price, &disc), vec![dec(9_500_000, p2, s2)]);

    // Comparisons rescale without loss: 1.25 < 1.5, 2.50 = 2.5.
    let cmp = |op, v: Value| bin(op, x(), lit(v), DataType::Boolean);
    assert_eq!(
        run(&cmp(BinaryOp::Lt, dec(15, 3, 1)), &c),
        vec![b(Some(true)), b(Some(false)), Value::Null]
    );
    assert_eq!(
        run(&cmp(BinaryOp::Eq, dec(25, 3, 1)), &c),
        vec![b(Some(false)), b(Some(true)), Value::Null]
    );
    assert_eq!(
        run(&cmp(BinaryOp::GtEq, dec(1251, 6, 3)), &c),
        vec![b(Some(false)), b(Some(true)), Value::Null]
    );
    let wide = chunk(vec![
        (dect(38, 10), vec![dec(12_500_000_000, 38, 10)]),
        (dect(4, 2), vec![dec(125, 4, 2)]),
    ]);
    let eq = bin(
        BinaryOp::Eq,
        col(0, dect(38, 10)),
        col(1, dect(4, 2)),
        DataType::Boolean,
    );
    assert_eq!(run(&eq, &wide), vec![b(Some(true))]);

    // Precision overflow is an error.
    let big = chunk(vec![(dect(38, 0), vec![dec(9 * 10i128.pow(37), 38, 0)])]);
    let times = bin(
        BinaryOp::Mul,
        col(0, dect(38, 0)),
        lit(dec(100, 3, 0)),
        dect(38, 0),
    );
    assert_eq!(eval(&times, &big, None).unwrap_err().exit_code(), 1);
    let plus = bin(
        BinaryOp::Add,
        col(0, dect(38, 0)),
        col(0, dect(38, 0)),
        dect(38, 0),
    );
    assert!(eval(&plus, &big, None).is_err());
}

#[test]
fn comparisons_of_every_family() {
    let c = chunk(vec![
        (
            DataType::Varchar,
            vec![s("apple"), s("banana"), s(""), Value::Null],
        ),
        (
            DataType::Float64,
            vec![
                Value::Float64(1.5),
                Value::Float64(f64::NAN),
                Value::Float64(-0.0),
                Value::Float64(2.0),
            ],
        ),
        (
            DataType::Date,
            vec![
                Value::Date(1),
                Value::Date(2),
                Value::Date(3),
                Value::Date(4),
            ],
        ),
    ]);
    let sc = bin(
        BinaryOp::Gt,
        col(0, DataType::Varchar),
        lit(s("apple")),
        DataType::Boolean,
    );
    assert_eq!(
        run(&sc, &c),
        vec![b(Some(false)), b(Some(true)), b(Some(false)), Value::Null]
    );
    let ne = bin(
        BinaryOp::NotEq,
        lit(s("")),
        col(0, DataType::Varchar),
        DataType::Boolean,
    );
    assert_eq!(
        run(&ne, &c),
        vec![b(Some(true)), b(Some(true)), b(Some(false)), Value::Null]
    );
    let fz = bin(
        BinaryOp::Eq,
        col(1, DataType::Float64),
        lit(Value::Float64(0.0)),
        DataType::Boolean,
    );
    assert_eq!(
        run(&fz, &c),
        vec![
            b(Some(false)),
            b(Some(false)),
            b(Some(true)),
            b(Some(false))
        ]
    );
    let dl = bin(
        BinaryOp::LtEq,
        col(2, DataType::Date),
        lit(Value::Date(2)),
        DataType::Boolean,
    );
    assert_eq!(eval_predicate(&dl, &c, None).unwrap(), vec![0, 1]);
    let null_cmp = bin(
        BinaryOp::Eq,
        col(2, DataType::Date),
        BoundExpr::typed_literal(Value::Null, DataType::Date),
        DataType::Boolean,
    );
    assert_eq!(run(&null_cmp, &c), vec![Value::Null; 4]);
    assert!(eval_predicate(&null_cmp, &c, None).unwrap().is_empty());
}

#[test]
fn casts_between_types() {
    let c = chunk(vec![
        (
            DataType::Varchar,
            vec![s("2013-07-01"), s(" 42 "), Value::Null],
        ),
        (
            DataType::Int64,
            vec![Value::Int64(1), Value::Int64(1 << 40), Value::Int64(-3)],
        ),
        (
            DataType::Float64,
            vec![
                Value::Float64(2.5),
                Value::Float64(-2.5),
                Value::Float64(0.125),
            ],
        ),
        (
            dect(4, 2),
            vec![dec(125, 4, 2), dec(-125, 4, 2), dec(5, 4, 2)],
        ),
        (
            DataType::Timestamp,
            vec![
                Value::Timestamp(parse_timestamp(b"2013-07-01 10:00:00").unwrap()),
                Value::Timestamp(-1),
                Value::Null,
            ],
        ),
    ]);
    let cv = |i: usize, from: DataType, to: DataType, rows: &[u32]| {
        values(&eval(&cast(col(i, from), to), &c, Some(rows)).unwrap())
    };
    let date = parse_date(b"2013-07-01").unwrap();
    assert_eq!(
        cv(0, DataType::Varchar, DataType::Date, &[0, 2]),
        vec![Value::Date(date), Value::Null]
    );
    assert_eq!(
        cv(0, DataType::Varchar, DataType::Int32, &[1, 2]),
        vec![Value::Int32(42), Value::Null]
    );
    assert_eq!(
        cv(0, DataType::Varchar, dect(5, 1), &[1]),
        vec![dec(420, 5, 1)]
    );
    assert_eq!(
        cv(0, DataType::Varchar, DataType::Timestamp, &[0]),
        vec![Value::Timestamp(date as i64 * 86_400)]
    );
    assert!(eval(&cast(col(0, DataType::Varchar), DataType::Int64), &c, None).is_err());
    assert!(
        eval(
            &cast(col(0, DataType::Varchar), DataType::Date),
            &c,
            Some(&[1])
        )
        .is_err()
    );

    assert_eq!(
        cv(1, DataType::Int64, DataType::Int32, &[0, 2]),
        vec![Value::Int32(1), Value::Int32(-3)]
    );
    assert!(eval(&cast(col(1, DataType::Int64), DataType::Int32), &c, None).is_err());
    assert_eq!(
        cv(1, DataType::Int64, dect(5, 2), &[0]),
        vec![dec(100, 5, 2)]
    );
    assert_eq!(
        cv(1, DataType::Int64, DataType::Varchar, &[2]),
        vec![s("-3")]
    );
    assert_eq!(
        cv(1, DataType::Int64, DataType::Float64, &[2]),
        vec![Value::Float64(-3.0)]
    );
    assert_eq!(
        cv(1, DataType::Int64, DataType::Boolean, &[0]),
        vec![Value::Boolean(true)]
    );

    // Half away from zero.
    assert_eq!(
        cv(2, DataType::Float64, DataType::Int64, &[0, 1]),
        vec![Value::Int64(3), Value::Int64(-3)]
    );
    assert_eq!(
        cv(2, DataType::Float64, dect(5, 2), &[2]),
        vec![dec(13, 5, 2)]
    );
    assert_eq!(
        cv(3, dect(4, 2), dect(3, 1), &[0, 1]),
        vec![dec(13, 3, 1), dec(-13, 3, 1)]
    );
    assert_eq!(
        cv(3, dect(4, 2), DataType::Int64, &[0, 2]),
        vec![Value::Int64(1), Value::Int64(0)]
    );
    assert_eq!(
        cv(3, dect(4, 2), DataType::Float64, &[0]),
        vec![Value::Float64(1.25)]
    );
    assert_eq!(
        cv(3, dect(4, 2), DataType::Varchar, &[1, 2]),
        vec![s("-1.25"), s("0.05")]
    );
    assert!(eval(&cast(col(3, dect(4, 2)), dect(1, 1)), &c, None).is_err());

    assert_eq!(
        cv(4, DataType::Timestamp, DataType::Varchar, &[0, 2]),
        vec![s("2013-07-01 10:00:00"), Value::Null]
    );
    assert_eq!(
        cv(4, DataType::Timestamp, DataType::Date, &[0, 1]),
        vec![Value::Date(date), Value::Date(-1)]
    );
    let dates = chunk(vec![(DataType::Date, vec![Value::Date(date)])]);
    assert_eq!(
        run(&cast(col(0, DataType::Date), DataType::Timestamp), &dates),
        vec![Value::Timestamp(date as i64 * 86_400)]
    );
    assert_eq!(
        run(&cast(col(0, DataType::Date), DataType::Varchar), &dates),
        vec![s("2013-07-01")]
    );
    assert!(eval(&cast(col(0, DataType::Date), DataType::Int64), &dates, None).is_err());

    // Vector casts agree with constant folding.
    for (v, to) in [
        (s("17"), DataType::Int16),
        (Value::Float64(1.5), dect(4, 1)),
        (dec(999, 4, 1), DataType::Int32),
        (Value::Boolean(true), DataType::Int64),
        (s("true"), DataType::Boolean),
    ] {
        let ty = v.data_type().unwrap();
        let one = chunk(vec![(ty, vec![v.clone()])]);
        assert_eq!(
            run(&cast(col(0, ty), to), &one),
            vec![cast_value(&v, to).unwrap()]
        );
    }
}

#[test]
fn extract_and_date_trunc_through_eval() {
    let ts = parse_timestamp(b"2013-07-15 13:45:59").unwrap();
    let c = chunk(vec![(
        DataType::Timestamp,
        vec![Value::Timestamp(ts), Value::Null],
    )]);
    let ex = |part| {
        func(
            ScalarFunc::Extract(part),
            vec![col(0, DataType::Timestamp)],
            DataType::Int64,
        )
    };
    assert_eq!(
        run(&ex(DatePart::Minute), &c),
        vec![Value::Int64(45), Value::Null]
    );
    assert_eq!(
        run(&ex(DatePart::Year), &c),
        vec![Value::Int64(2013), Value::Null]
    );
    let tr = func(
        ScalarFunc::DateTrunc(TruncUnit::Hour),
        vec![col(0, DataType::Timestamp)],
        DataType::Timestamp,
    );
    assert_eq!(
        run(&tr, &c),
        vec![
            Value::Timestamp(parse_timestamp(b"2013-07-15 13:00:00").unwrap()),
            Value::Null
        ]
    );
    // A constant argument folds through the one-row path.
    let k = func(
        ScalarFunc::Extract(DatePart::Month),
        vec![lit(Value::Timestamp(ts))],
        DataType::Int64,
    );
    assert_eq!(run(&k, &c), vec![Value::Int64(7); 2]);
}

#[test]
fn string_functions_through_eval() {
    let c = chunk(vec![
        (DataType::Varchar, vec![s("hello"), s(""), Value::Null]),
        (
            DataType::Int32,
            vec![Value::Int32(2), Value::Int32(1), Value::Int32(1)],
        ),
    ]);
    let len = func(
        ScalarFunc::Strlen,
        vec![col(0, DataType::Varchar)],
        DataType::Int64,
    );
    assert_eq!(
        run(&len, &c),
        vec![Value::Int64(5), Value::Int64(0), Value::Null]
    );
    let sub = func(
        ScalarFunc::Substring,
        vec![
            col(0, DataType::Varchar),
            lit(Value::Int64(2)),
            lit(Value::Int64(3)),
        ],
        DataType::Varchar,
    );
    assert_eq!(run(&sub, &c), vec![s("ell"), s(""), Value::Null]);
    let tail = func(
        ScalarFunc::Substring,
        vec![col(0, DataType::Varchar), lit(Value::Int64(-2))],
        DataType::Varchar,
    );
    assert_eq!(run(&tail, &c), vec![s("lo"), s(""), Value::Null]);
    let per_row = func(
        ScalarFunc::Substring,
        vec![col(0, DataType::Varchar), col(1, DataType::Int32)],
        DataType::Varchar,
    );
    assert_eq!(run(&per_row, &c), vec![s("ello"), s(""), Value::Null]);
}

fn like(e: BoundExpr, pattern: &str, negated: bool) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Like {
            expr: Box::new(e),
            pattern: Box::new(lit(s(pattern))),
            negated,
        },
        DataType::Boolean,
        true,
    )
}

#[test]
fn like_and_not_like() {
    let c = chunk(vec![(
        DataType::Varchar,
        vec![
            s("www.google.com"),
            s("a_b"),
            s("50%"),
            Value::Null,
            s("axb"),
        ],
    )]);
    let x = || col(0, DataType::Varchar);
    assert_eq!(
        run(&like(x(), "%google%", false), &c),
        vec![
            b(Some(true)),
            b(Some(false)),
            b(Some(false)),
            Value::Null,
            b(Some(false))
        ]
    );
    assert_eq!(
        run(&like(x(), "%google%", true), &c),
        vec![
            b(Some(false)),
            b(Some(true)),
            b(Some(true)),
            Value::Null,
            b(Some(true))
        ]
    );
    assert_eq!(
        eval_predicate(&like(x(), "a_b", false), &c, None).unwrap(),
        vec![1, 4]
    );
    assert_eq!(
        eval_predicate(&like(x(), r"a\_b", false), &c, None).unwrap(),
        vec![1]
    );
    assert_eq!(
        eval_predicate(&like(x(), r"%\%", false), &c, None).unwrap(),
        vec![2]
    );
    assert_eq!(
        eval_predicate(&like(x(), "%", true), &c, None).unwrap(),
        Vec::<u32>::new()
    );
    assert!(CompiledExpr::new(&like(x(), r"bad\", false)).is_err());
    // A pattern from a column.
    let pats = chunk(vec![
        (DataType::Varchar, vec![s("abc"), s("abc")]),
        (DataType::Varchar, vec![s("a%"), s("%z")]),
    ]);
    let dynamic = BoundExpr::new(
        BoundExprKind::Like {
            expr: Box::new(col(0, DataType::Varchar)),
            pattern: Box::new(col(1, DataType::Varchar)),
            negated: false,
        },
        DataType::Boolean,
        true,
    );
    assert_eq!(run(&dynamic, &pats), vec![b(Some(true)), b(Some(false))]);
}

fn regexp(e: BoundExpr, pattern: &str, replacement: &str) -> BoundExpr {
    func(
        ScalarFunc::RegexpReplace,
        vec![e, lit(s(pattern)), lit(s(replacement))],
        DataType::Varchar,
    )
}

#[test]
fn regexp_replace_clickbench_q29() {
    let c = chunk(vec![(
        DataType::Varchar,
        vec![
            s("https://www.example.com/page"),
            s("http://example.com/"),
            s("https://www.example.com/a\nb"),
            s("mailto:someone"),
            Value::Null,
        ],
    )]);
    let e = regexp(
        col(0, DataType::Varchar),
        r"^https?://(?:www\.)?([^/]+)/.*$",
        r"\1",
    );
    assert_eq!(
        run(&e, &c),
        vec![
            s("example.com"),
            s("example.com"),
            s("example.com"),
            s("mailto:someone"),
            Value::Null
        ]
    );
    // The invalid pattern fails when the expression is compiled, even without rows.
    let bad = regexp(col(0, DataType::Varchar), "a(b", "");
    assert_eq!(CompiledExpr::new(&bad).unwrap_err().exit_code(), 1);
    assert!(eval(&bad, &Chunk::empty(&[DataType::Varchar]), None).is_err());
}

#[test]
fn case_evaluates_branches_only_on_their_rows() {
    let c = chunk(vec![(
        DataType::Int64,
        vec![
            Value::Int64(1),
            Value::Int64(i64::MAX),
            Value::Int64(-4),
            Value::Null,
        ],
    )]);
    let x = || col(0, DataType::Int64);
    let small = bin(BinaryOp::Lt, x(), lit(Value::Int64(100)), DataType::Boolean);
    let doubled = bin(BinaryOp::Mul, x(), lit(Value::Int64(2)), DataType::Int64);
    let case = BoundExpr::new(
        BoundExprKind::Case {
            branches: vec![(small.clone(), doubled.clone())],
            else_: Some(Box::new(lit(Value::Int64(0)))),
        },
        DataType::Int64,
        true,
    );
    // Row 1 would overflow in `x * 2` but takes the ELSE branch; row 3 (NULL) too.
    assert_eq!(
        run(&case, &c),
        vec![
            Value::Int64(2),
            Value::Int64(0),
            Value::Int64(-8),
            Value::Int64(0)
        ]
    );
    let no_else = BoundExpr::new(
        BoundExprKind::Case {
            branches: vec![
                (
                    bin(BinaryOp::Gt, x(), lit(Value::Int64(0)), DataType::Boolean),
                    lit(s("pos")),
                ),
                (
                    bin(BinaryOp::Lt, x(), lit(Value::Int64(0)), DataType::Boolean),
                    cast(x(), DataType::Varchar),
                ),
            ],
            else_: None,
        },
        DataType::Varchar,
        true,
    );
    assert_eq!(
        run(&no_else, &c),
        vec![s("pos"), s("pos"), s("-4"), Value::Null]
    );
    // With a selection the output follows it.
    assert_eq!(
        values(&eval(&case, &c, Some(&[2, 0])).unwrap()),
        vec![Value::Int64(-8), Value::Int64(2)]
    );
}

#[test]
fn selection_vectors_and_borrowed_columns() {
    let c = chunk(vec![
        (
            DataType::Int16,
            vec![
                Value::Int16(5),
                Value::Int16(6),
                Value::Int16(7),
                Value::Int16(8),
            ],
        ),
        (DataType::Varchar, vec![s("a"), s("b"), s("c"), s("d")]),
    ]);
    let x = col(0, DataType::Int16);
    assert_eq!(
        values(&eval(&x, &c, Some(&[3, 1])).unwrap()),
        vec![Value::Int16(8), Value::Int16(6)]
    );
    let gt = bin(
        BinaryOp::Gt,
        cast(col(0, DataType::Int16), DataType::Int64),
        lit(Value::Int64(5)),
        DataType::Boolean,
    );
    let eq = bin(
        BinaryOp::NotEq,
        col(1, DataType::Varchar),
        lit(s("c")),
        DataType::Boolean,
    );
    let both = bin(BinaryOp::And, gt.clone(), eq, DataType::Boolean);
    assert_eq!(eval_predicate(&both, &c, None).unwrap(), vec![1, 3]);
    assert_eq!(
        eval_predicate(&both, &c, Some(&[3, 2, 0])).unwrap(),
        vec![3]
    );
    let compiled = CompiledExpr::new(&gt).unwrap();
    assert_eq!(compiled.data_type(), DataType::Boolean);
    assert_eq!(compiled.eval_predicate(&c, Some(&[0, 2])).unwrap(), vec![2]);
    assert!(
        CompiledExpr::new(&x)
            .unwrap()
            .eval_predicate(&c, None)
            .is_err()
    );
}
