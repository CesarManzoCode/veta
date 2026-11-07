//! Property and malformed-input tests of the column encodings (contract §8.1).

use super::*;
use veta_core::Bitmap;
use veta_core::rand::Rng;

const TYPES: [DataType; 11] = [
    DataType::Boolean,
    DataType::Int16,
    DataType::Int32,
    DataType::Int64,
    DataType::Int128,
    DataType::Float64,
    DataType::Decimal {
        precision: 12,
        scale: 2,
    },
    DataType::Decimal {
        precision: 30,
        scale: 4,
    },
    DataType::Varchar,
    DataType::Date,
    DataType::Timestamp,
];

const LENGTHS: [usize; 7] = [0, 1, 127, 128, 129, 8191, 8192];

// ---------------------------------------------------------------------------------------
// Generators.

/// Uniform in `[a, b]` over `u128` (modulo bias is irrelevant here).
fn uniform(rng: &mut Rng, a: u128, b: u128) -> u128 {
    let r = ((rng.next_u64() as u128) << 64) | rng.next_u64() as u128;
    if b - a == u128::MAX {
        r
    } else {
        a + r % (b - a + 1)
    }
}

fn int_range(kind: IntKind) -> (i128, i128) {
    match kind {
        IntKind::I16 => (i16::MIN as i128, i16::MAX as i128),
        IntKind::I32 => (i32::MIN as i128, i32::MAX as i128),
        IntKind::I64 => (i64::MIN as i128, i64::MAX as i128),
        IntKind::I128 => (i128::MIN, i128::MAX),
    }
}

const INT_PATTERNS: usize = 9;

/// Integers of `kind` following `pattern`: constant (width 0), widths 1, 63 and 64 (capped
/// at the type range), the full type range, sorted, long runs, few and many distinct.
fn gen_ints(rng: &mut Rng, kind: IntKind, n: usize, pattern: usize) -> Vec<i128> {
    let (lo, hi) = int_range(kind);
    let total = hi.wrapping_sub(lo) as u128;
    let spanned = |rng: &mut Rng, span: u128| -> Vec<u128> {
        let span = span.min(total);
        let base = uniform(rng, 0, total - span);
        let mut v: Vec<u128> = (0..n).map(|_| base + uniform(rng, 0, span)).collect();
        if n >= 2 {
            v[0] = base;
            v[n - 1] = base + span;
        }
        v
    };
    let offsets: Vec<u128> = match pattern {
        0 => vec![uniform(rng, 0, total); n],
        1 => spanned(rng, 1),
        2 => spanned(rng, (1u128 << 63) - 1),
        3 => spanned(rng, u64::MAX as u128),
        4 => spanned(rng, total),
        5 => {
            let mut x = uniform(rng, 0, total / 2);
            (0..n)
                .map(|_| {
                    x = (x + rng.below(1000) as u128).min(total);
                    x
                })
                .collect()
        }
        6 => {
            let mut v = Vec::with_capacity(n);
            while v.len() < n {
                let len = (1 + rng.below(2000) as usize).min(n - v.len());
                let x = uniform(rng, 0, total);
                v.extend(std::iter::repeat_n(x, len));
            }
            v
        }
        7 => {
            let few: Vec<u128> = (0..4).map(|_| uniform(rng, 0, total)).collect();
            (0..n).map(|_| few[rng.below(4) as usize]).collect()
        }
        _ => spanned(rng, 1 << 40),
    };
    offsets
        .into_iter()
        .map(|o| lo.wrapping_add(o as i128))
        .collect()
}

fn int_data(data_type: DataType, v: &[i128]) -> VectorData {
    match data_type {
        DataType::Int16 => VectorData::Int16(v.iter().map(|&x| x as i16).collect()),
        DataType::Int32 => VectorData::Int32(v.iter().map(|&x| x as i32).collect()),
        DataType::Date => VectorData::Date(v.iter().map(|&x| x as i32).collect()),
        DataType::Int64 => VectorData::Int64(v.iter().map(|&x| x as i64).collect()),
        DataType::Timestamp => VectorData::Timestamp(v.iter().map(|&x| x as i64).collect()),
        DataType::Int128 => VectorData::Int128(v.to_vec()),
        DataType::Decimal { precision, scale } if precision <= MAX_DECIMAL64_PRECISION => {
            VectorData::Decimal64 {
                values: v.iter().map(|&x| x as i64).collect(),
                precision,
                scale,
            }
        }
        DataType::Decimal { precision, scale } => VectorData::Decimal128 {
            values: v.to_vec(),
            precision,
            scale,
        },
        other => panic!("{other} is not an integer type"),
    }
}

fn str_data(strings: &[Vec<u8>]) -> VectorData {
    let mut offsets = vec![0u32];
    let mut bytes = Vec::new();
    for s in strings {
        bytes.extend_from_slice(s);
        offsets.push(bytes.len() as u32);
    }
    VectorData::Varchar { offsets, bytes }
}

fn random_string(rng: &mut Rng, max_len: u64) -> Vec<u8> {
    let mut s = vec![0u8; rng.below(max_len + 1) as usize];
    rng.fill_bytes(&mut s);
    s
}

fn patterns(data_type: DataType) -> usize {
    match data_type {
        DataType::Boolean => 4,
        DataType::Float64 => 3,
        DataType::Varchar => 6,
        _ => INT_PATTERNS,
    }
}

/// Values of `data_type` following `pattern`, with `default` (zero, `false`, empty) where
/// `valid` is false, as `Vector::push` leaves NULL slots.
fn gen_data(
    rng: &mut Rng,
    data_type: DataType,
    n: usize,
    pattern: usize,
    valid: &[bool],
) -> VectorData {
    match data_type {
        DataType::Boolean => {
            let v: Vec<bool> = match pattern {
                0 => vec![true; n],
                1 => vec![false; n],
                2 => (0..n).map(|_| rng.next_bool(0.5)).collect(),
                _ => {
                    let mut v = Vec::with_capacity(n);
                    while v.len() < n {
                        let len = (1 + rng.below(500) as usize).min(n - v.len());
                        let b = rng.next_bool(0.5);
                        v.extend(std::iter::repeat_n(b, len));
                    }
                    v
                }
            };
            VectorData::Boolean(v.iter().zip(valid).map(|(&b, &ok)| b && ok).collect())
        }
        DataType::Float64 => {
            let specials = [
                0.0,
                -0.0,
                1.5,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MIN,
                f64::MAX,
                f64::MIN_POSITIVE,
            ];
            let v: Vec<f64> = (0..n)
                .map(|i| match pattern {
                    0 => specials[i % specials.len()],
                    1 => (rng.next_f64() - 0.5) * 1e12,
                    _ => (rng.below(10) as f64) * 0.25,
                })
                .collect();
            VectorData::Float64(
                v.iter()
                    .zip(valid)
                    .map(|(&f, &ok)| if ok { f } else { 0.0 })
                    .collect(),
            )
        }
        DataType::Varchar => {
            let v: Vec<Vec<u8>> = match pattern {
                0 => vec![Vec::new(); n],
                1 => {
                    let few: Vec<Vec<u8>> = (0..3).map(|_| random_string(rng, 12)).collect();
                    (0..n).map(|_| few[rng.below(3) as usize].clone()).collect()
                }
                2 => (0..n).map(|_| random_string(rng, 40)).collect(),
                3 => {
                    let mut v = Vec::with_capacity(n);
                    while v.len() < n {
                        let len = (1 + rng.below(1000) as usize).min(n - v.len());
                        let s = random_string(rng, 20);
                        v.extend(std::iter::repeat_n(s, len));
                    }
                    v
                }
                4 => (0..n).map(|i| format!("row-{i:08}").into_bytes()).collect(),
                _ => (0..n)
                    .map(|i| {
                        if i % 97 == 0 {
                            random_string(rng, 3000)
                        } else {
                            b"x".to_vec()
                        }
                    })
                    .collect(),
            };
            let v: Vec<Vec<u8>> = v
                .into_iter()
                .zip(valid)
                .map(|(s, &ok)| if ok { s } else { Vec::new() })
                .collect();
            str_data(&v)
        }
        _ => {
            let kind = IntKind::of(&data_type).unwrap();
            let v: Vec<i128> = gen_ints(rng, kind, n, pattern)
                .into_iter()
                .zip(valid)
                .map(|(x, &ok)| if ok { x } else { 0 })
                .collect();
            int_data(data_type, &v)
        }
    }
}

/// Validity modes: every row valid, sparse NULLs (~3 %) and every row NULL.
fn gen_validity(rng: &mut Rng, n: usize, mode: usize) -> Option<Vec<bool>> {
    match mode {
        0 => None,
        1 => Some((0..n).map(|_| !rng.next_bool(0.03)).collect()),
        _ => Some(vec![false; n]),
    }
}

fn make_vector(
    rng: &mut Rng,
    data_type: DataType,
    n: usize,
    pattern: usize,
    mode: usize,
) -> Vector {
    let validity = gen_validity(rng, n, mode);
    let valid = validity.clone().unwrap_or_else(|| vec![true; n]);
    let data = gen_data(rng, data_type, n, pattern, &valid);
    Vector::new(data, validity.map(|v| Bitmap::from_bools(&v))).unwrap()
}

// ---------------------------------------------------------------------------------------
// Round trip.

fn is_wide(data_type: &DataType) -> bool {
    IntKind::of(data_type) == Some(IntKind::I128)
}

/// `decode(encode_with(x, e)) == x` (validity reattached) for every applicable encoding and
/// for the encoding `encode` picks.
fn check_roundtrip(x: &Vector, label: &str) {
    let data_type = x.data_type();
    let back = |encoding: Encoding, bytes: &[u8]| -> Vector {
        let y = decode(encoding, bytes, &data_type, x.len())
            .unwrap_or_else(|e| panic!("{label} {encoding:?}: decode failed: {e}"));
        assert!(
            y.validity.is_none(),
            "{label} {encoding:?}: decode added validity"
        );
        Vector::new(y.data, x.validity.clone()).unwrap()
    };
    for encoding in Encoding::ALL {
        match encode_with(x, encoding) {
            Ok(bytes) => {
                assert!(encoding.applies_to(&data_type), "{label} {encoding:?}");
                assert!(
                    back(encoding, &bytes) == *x,
                    "{label} {encoding:?}: round trip differs"
                );
            }
            Err(_) => assert!(
                !encoding.applies_to(&data_type)
                    || (is_wide(&data_type)
                        && matches!(encoding, Encoding::ForBitpack | Encoding::DeltaBitpack)),
                "{label} {encoding:?}: unexpected encode error"
            ),
        }
    }
    let (encoding, bytes) = encode(x);
    assert!(
        back(encoding, &bytes) == *x,
        "{label} encode() {encoding:?}: round trip differs"
    );
}

fn property(data_type: DataType, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut lengths = LENGTHS.to_vec();
    lengths.extend((0..2).map(|_| rng.below(20_000) as usize));
    for &n in &lengths {
        for pattern in 0..patterns(data_type) {
            for mode in 0..2 {
                let x = make_vector(&mut rng, data_type, n, pattern, mode);
                check_roundtrip(
                    &x,
                    &format!("{data_type} n={n} pattern={pattern} nulls={mode}"),
                );
            }
        }
        let x = make_vector(&mut rng, data_type, n, 0, 2);
        check_roundtrip(&x, &format!("{data_type} n={n} all NULL"));
    }
}

#[test]
fn roundtrip_boolean() {
    property(TYPES[0], 1);
}

#[test]
fn roundtrip_int16() {
    property(TYPES[1], 2);
}

#[test]
fn roundtrip_int32() {
    property(TYPES[2], 3);
}

#[test]
fn roundtrip_int64() {
    property(TYPES[3], 4);
}

#[test]
fn roundtrip_int128() {
    property(TYPES[4], 5);
}

#[test]
fn roundtrip_float64() {
    property(TYPES[5], 6);
}

#[test]
fn roundtrip_decimal64() {
    property(TYPES[6], 7);
}

#[test]
fn roundtrip_decimal128() {
    property(TYPES[7], 8);
}

#[test]
fn roundtrip_varchar() {
    property(TYPES[8], 9);
}

#[test]
fn roundtrip_date() {
    property(TYPES[9], 10);
}

#[test]
fn roundtrip_timestamp() {
    property(TYPES[10], 11);
}

#[test]
fn float_bits_survive_plain() {
    let values = [f64::NAN, -0.0, 0.0, f64::from_bits(0x7FF0_0000_0000_0001)];
    let x = Vector::from_data(VectorData::Float64(values.to_vec()));
    let bytes = encode_with(&x, Encoding::Plain).unwrap();
    let y = decode(Encoding::Plain, &bytes, &DataType::Float64, values.len()).unwrap();
    let VectorData::Float64(back) = y.data else {
        panic!("not a Float64 vector")
    };
    let bits = |v: &[f64]| v.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&back), bits(&values));
}

#[test]
fn values_under_null_are_normalized() {
    let validity = Bitmap::from_bools(&[true, false, true, false]);
    let x = Vector::new(VectorData::Int64(vec![5, 999, 7, -3]), Some(validity)).unwrap();
    let s = Vector::new(
        str_data(&[b"a".to_vec(), b"junk".to_vec()]),
        Some(Bitmap::from_bools(&[true, false])),
    )
    .unwrap();
    for encoding in Encoding::ALL {
        let bytes = encode_with(&x, encoding).unwrap();
        let y = decode(encoding, &bytes, &DataType::Int64, 4).unwrap();
        assert_eq!(y.data, VectorData::Int64(vec![5, 0, 7, 0]), "{encoding:?}");
        if encoding.applies_to(&DataType::Varchar) {
            let bytes = encode_with(&s, encoding).unwrap();
            let y = decode(encoding, &bytes, &DataType::Varchar, 2).unwrap();
            assert_eq!(
                y.data,
                str_data(&[b"a".to_vec(), Vec::new()]),
                "{encoding:?}"
            );
        }
    }
}

#[test]
fn for_bitpack_width_byte_matches_the_range() {
    // (offset span, expected width) for an Int64 column: widths 0, 1, 63 and 64.
    let cases: [(i64, i64, u8); 4] = [
        (42, 42, 0),
        (10, 11, 1),
        (0, i64::MAX, 63),
        (i64::MIN, i64::MAX, 64),
    ];
    for (lo, hi, width) in cases {
        let x = Vector::from_data(VectorData::Int64(vec![lo, hi, lo]));
        let bytes = encode_with(&x, Encoding::ForBitpack).unwrap();
        assert_eq!(&bytes[..8], &lo.to_le_bytes());
        assert_eq!(bytes[8], width, "range [{lo}, {hi}]");
        assert_eq!(bytes.len(), 9 + bitpack::packed_len(3, width).unwrap());
    }
}

#[test]
fn wide_range_beyond_64_bits_skips_for_and_delta() {
    let x = Vector::from_data(VectorData::Int128(vec![i128::MIN, 0, i128::MAX]));
    assert!(encode_with(&x, Encoding::ForBitpack).is_err());
    assert!(encode_with(&x, Encoding::DeltaBitpack).is_err());
    check_roundtrip(&x, "Int128 full range");
    // A 128-bit range of exactly 2⁶⁴ − 1 still fits.
    let base = 1i128 << 100;
    let y = Vector::from_data(VectorData::Int128(vec![base, base + u64::MAX as i128]));
    let bytes = encode_with(&y, Encoding::ForBitpack).unwrap();
    assert_eq!(bytes[16], 64);
    check_roundtrip(&y, "Int128 64-bit range");
}

#[test]
fn inapplicable_encodings_are_rejected() {
    let f = Vector::from_data(VectorData::Float64(vec![1.0]));
    let b = Vector::from_data(VectorData::Boolean(vec![true]));
    let s = Vector::from_data(str_data(&[b"a".to_vec()]));
    let d = Vector::from_data(VectorData::Date(vec![1]));
    for encoding in [
        Encoding::ForBitpack,
        Encoding::DeltaBitpack,
        Encoding::Dict,
        Encoding::Rle,
    ] {
        assert!(encode_with(&f, encoding).is_err());
        assert!(encode_with(&b, encoding).is_err());
    }
    assert!(encode_with(&s, Encoding::ForBitpack).is_err());
    assert!(encode_with(&s, Encoding::DeltaBitpack).is_err());
    assert!(encode_with(&d, Encoding::Dict).is_err());
    assert!(encode_with(&d, Encoding::Rle).is_err());
}

#[test]
fn encoding_ids_match_the_contract() {
    let ids: Vec<u8> = Encoding::ALL.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    for encoding in Encoding::ALL {
        assert_eq!(Encoding::from_id(encoding.id()).unwrap(), encoding);
    }
}

// ---------------------------------------------------------------------------------------
// `encode` picks the smallest.

#[test]
fn encode_picks_the_smallest_candidate() {
    let mut rng = Rng::new(0x5A11);
    for data_type in TYPES {
        for n in [0usize, 1, 129, 8192] {
            for pattern in 0..patterns(data_type) {
                let x = make_vector(&mut rng, data_type, n, pattern, 1);
                let (chosen, bytes) = encode(&x);
                let sizes: Vec<(Encoding, usize)> = Encoding::ALL
                    .into_iter()
                    .filter_map(|e| encode_with(&x, e).ok().map(|b| (e, b.len())))
                    .collect();
                let min = sizes.iter().map(|&(_, len)| len).min().unwrap();
                assert_eq!(
                    bytes.len(),
                    min,
                    "{data_type} n={n} pattern={pattern}: {sizes:?}"
                );
                let first = sizes.iter().find(|&&(_, len)| len == min).unwrap().0;
                assert_eq!(chosen, first, "ties go to the lowest id");
                assert_eq!(encode_with(&x, chosen).unwrap(), bytes);
            }
        }
    }
}

#[test]
fn encode_choices_on_typical_columns() {
    let constant = Vector::from_data(VectorData::Int64(vec![7; 8192]));
    assert_eq!(encode(&constant).0, Encoding::ForBitpack);
    let mut rng = Rng::new(3);
    let random = Vector::from_data(VectorData::Int64(
        (0..8192).map(|_| rng.next_u64() as i64).collect(),
    ));
    assert_eq!(encode(&random).0, Encoding::Plain);
    let sorted = Vector::from_data(VectorData::Int64(
        (0..8192).map(|i| 1_000_000_000 + i * 3).collect(),
    ));
    assert_eq!(encode(&sorted).0, Encoding::DeltaBitpack);
    let runs: Vec<Vec<u8>> = (0..8192)
        .map(|i| format!("value-{}", i / 1000).into_bytes())
        .collect();
    assert_eq!(encode(&Vector::from_data(str_data(&runs))).0, Encoding::Rle);
    let few: Vec<Vec<u8>> = (0..8192)
        .map(|i| format!("category-{}", i % 5).into_bytes())
        .collect();
    assert_eq!(encode(&Vector::from_data(str_data(&few))).0, Encoding::Dict);
}

// ---------------------------------------------------------------------------------------
// Malformed input.

fn le_i64(v: i64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// `ForBitpack` payload with an 8-byte `min`.
fn for64(min: i64, width: u8, values: &[u64]) -> Vec<u8> {
    let mut out = le_i64(min);
    out.push(width);
    bitpack::pack(values, width, &mut out);
    out
}

#[test]
fn malformed_unknown_encoding_id() {
    assert!(Encoding::from_id(5).is_err());
    assert!(Encoding::from_id(255).is_err());
}

#[test]
fn malformed_encoding_not_valid_for_type() {
    assert!(decode(Encoding::Dict, &[0; 64], &DataType::Float64, 1).is_err());
    assert!(decode(Encoding::ForBitpack, &[0; 64], &DataType::Varchar, 1).is_err());
    assert!(decode(Encoding::Rle, &[0; 64], &DataType::Boolean, 1).is_err());
    assert!(decode(Encoding::Rle, &[0; 64], &DataType::Timestamp, 1).is_err());
}

#[test]
fn malformed_truncated_input() {
    let mut rng = Rng::new(0x7E);
    for data_type in TYPES {
        for pattern in 0..patterns(data_type) {
            let x = make_vector(&mut rng, data_type, 300, pattern, 1);
            for encoding in Encoding::ALL {
                let Ok(bytes) = encode_with(&x, encoding) else {
                    continue;
                };
                for cut in 0..bytes.len() {
                    assert!(
                        decode(encoding, &bytes[..cut], &data_type, x.len()).is_err(),
                        "{data_type} pattern={pattern} {encoding:?} cut at {cut} of {}",
                        bytes.len()
                    );
                }
            }
        }
    }
}

#[test]
fn malformed_trailing_bytes() {
    let x = Vector::from_data(VectorData::Int32(vec![1, 2, 3, 3, 3]));
    for encoding in Encoding::ALL {
        let mut bytes = encode_with(&x, encoding).unwrap();
        bytes.push(0);
        assert!(
            decode(encoding, &bytes, &DataType::Int32, 5).is_err(),
            "{encoding:?}"
        );
    }
}

/// `Plain` and `Rle` detect a row count that disagrees with the payload. The bit-packed
/// formats cannot always (padding bits of the last word), so the block's `rows` header and
/// CRC carry that check (§5.3).
#[test]
fn malformed_wrong_row_count() {
    let x = Vector::from_data(VectorData::Int64(vec![4, 4, 5, 9]));
    for encoding in [Encoding::Plain, Encoding::Rle] {
        let bytes = encode_with(&x, encoding).unwrap();
        assert!(
            decode(encoding, &bytes, &DataType::Int64, 5).is_err(),
            "{encoding:?} +1"
        );
        assert!(
            decode(encoding, &bytes, &DataType::Int64, 3).is_err(),
            "{encoding:?} -1"
        );
    }
}

#[test]
fn malformed_invalid_bit_width() {
    let mut bytes = le_i64(0);
    bytes.push(65);
    bytes.extend_from_slice(&[0; 1024]);
    assert!(decode(Encoding::ForBitpack, &bytes, &DataType::Int64, 4).is_err());
    let mut delta = le_i64(0);
    delta.extend_from_slice(&bytes);
    assert!(decode(Encoding::DeltaBitpack, &delta, &DataType::Int64, 5).is_err());
    let mut wide = 0i128.to_le_bytes().to_vec();
    wide.push(200);
    assert!(decode(Encoding::ForBitpack, &wide, &DataType::Int128, 0).is_err());
}

#[test]
fn malformed_dict_code_out_of_range() {
    let mut bytes = 2u32.to_le_bytes().to_vec();
    bytes.push(Encoding::Plain.id());
    bytes.extend(le_i64(10));
    bytes.extend(le_i64(20));
    bytes.extend(for64(0, 2, &[0, 1, 2]));
    assert!(decode(Encoding::Dict, &bytes, &DataType::Int64, 3).is_err());
    // The same block with code 1 instead of 2 decodes.
    let ok_len = bytes.len();
    let mut good = bytes[..ok_len - 17].to_vec();
    good.extend(for64(0, 2, &[0, 1, 1]));
    let y = decode(Encoding::Dict, &good, &DataType::Int64, 3).unwrap();
    assert_eq!(y.data, VectorData::Int64(vec![10, 20, 20]));
}

#[test]
fn malformed_dict_negative_code() {
    let mut bytes = 1u32.to_le_bytes().to_vec();
    bytes.push(Encoding::Plain.id());
    bytes.extend(le_i64(10));
    bytes.extend(for64(-1, 1, &[1, 0]));
    assert!(decode(Encoding::Dict, &bytes, &DataType::Int64, 2).is_err());
}

#[test]
fn malformed_string_dict_code_out_of_range() {
    let mut bytes = 1u32.to_le_bytes().to_vec();
    bytes.push(Encoding::Plain.id());
    bytes.extend(for64(1, 0, &[0])); // one string of length 1
    bytes.push(b'a');
    bytes.extend(for64(0, 1, &[0, 1]));
    assert!(decode(Encoding::Dict, &bytes, &DataType::Varchar, 2).is_err());
}

#[test]
fn malformed_dict_larger_than_rows() {
    let mut bytes = u32::MAX.to_le_bytes().to_vec();
    bytes.push(Encoding::ForBitpack.id());
    bytes.extend(for64(0, 0, &[]));
    bytes.extend(for64(0, 0, &[]));
    assert!(decode(Encoding::Dict, &bytes, &DataType::Int64, 2).is_err());
    assert!(decode(Encoding::Dict, &bytes, &DataType::Varchar, 2).is_err());
}

#[test]
fn malformed_invalid_sub_encoding() {
    for tag in [2u8, 3, 4, 9] {
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.push(tag);
        bytes.extend(le_i64(10));
        bytes.extend(for64(0, 0, &[0]));
        assert!(
            decode(Encoding::Dict, &bytes, &DataType::Int64, 1).is_err(),
            "tag {tag}"
        );
    }
    let mut bytes = 1u32.to_le_bytes().to_vec();
    bytes.push(Encoding::ForBitpack.id());
    bytes.extend(for64(0, 0, &[0]));
    bytes.extend(for64(0, 0, &[0]));
    assert!(decode(Encoding::Dict, &bytes, &DataType::Varchar, 1).is_err());
}

#[test]
fn malformed_rle_lengths_do_not_add_up() {
    let rle = |lengths: &[u64]| {
        let mut bytes = (lengths.len() as u32).to_le_bytes().to_vec();
        bytes.push(Encoding::Plain.id());
        for i in 0..lengths.len() {
            bytes.extend(le_i64(i as i64));
        }
        bytes.extend(for64(0, 8, lengths));
        bytes
    };
    assert!(decode(Encoding::Rle, &rle(&[2, 2]), &DataType::Int64, 5).is_err());
    assert!(decode(Encoding::Rle, &rle(&[2, 4]), &DataType::Int64, 5).is_err());
    assert!(decode(Encoding::Rle, &rle(&[5, 0]), &DataType::Int64, 5).is_err());
    assert!(decode(Encoding::Rle, &rle(&[]), &DataType::Int64, 5).is_err());
    let y = decode(Encoding::Rle, &rle(&[2, 3]), &DataType::Int64, 5).unwrap();
    assert_eq!(y.data, VectorData::Int64(vec![0, 0, 1, 1, 1]));
}

#[test]
fn malformed_rle_more_runs_than_rows() {
    let mut bytes = 9u32.to_le_bytes().to_vec();
    bytes.push(Encoding::Plain.id());
    assert!(decode(Encoding::Rle, &bytes, &DataType::Int32, 3).is_err());
}

#[test]
fn malformed_value_outside_the_type() {
    let bytes = for64(40_000, 0, &[0, 0]);
    assert!(decode(Encoding::ForBitpack, &bytes, &DataType::Int16, 2).is_err());
    let bytes = for64(i64::from(i32::MAX), 1, &[1]);
    assert!(decode(Encoding::ForBitpack, &bytes, &DataType::Date, 1).is_err());
    let bytes = for64(i64::MAX, 1, &[1]);
    assert!(decode(Encoding::ForBitpack, &bytes, &DataType::Int64, 1).is_err());
    let mut wide = i128::MAX.to_le_bytes().to_vec();
    wide.push(1);
    bitpack::pack(&[1], 1, &mut wide);
    assert!(decode(Encoding::ForBitpack, &wide, &DataType::Int128, 1).is_err());
}

#[test]
fn malformed_string_longer_than_the_buffer() {
    let mut bytes = for64(100, 0, &[0]);
    bytes.extend_from_slice(b"short");
    assert!(decode(Encoding::Plain, &bytes, &DataType::Varchar, 1).is_err());
    let bytes = for64(-1, 0, &[0]);
    assert!(decode(Encoding::Plain, &bytes, &DataType::Varchar, 1).is_err());
}

#[test]
fn malformed_random_bytes_never_panic() {
    let mut rng = Rng::new(0xF022);
    for data_type in TYPES {
        for encoding in Encoding::ALL {
            for _ in 0..200 {
                let mut bytes = vec![0u8; rng.below(200) as usize];
                rng.fill_bytes(&mut bytes);
                let rows = rng.below(300) as usize;
                let _ = decode(encoding, &bytes, &data_type, rows);
            }
        }
    }
}

#[test]
fn malformed_bit_flips_never_panic() {
    let mut rng = Rng::new(0xF11B);
    for data_type in TYPES {
        for pattern in 0..patterns(data_type) {
            let x = make_vector(&mut rng, data_type, 200, pattern, 1);
            for encoding in Encoding::ALL {
                let Ok(bytes) = encode_with(&x, encoding) else {
                    continue;
                };
                if bytes.is_empty() {
                    continue;
                }
                for _ in 0..50 {
                    let mut bad = bytes.clone();
                    let i = rng.below(bad.len() as u64) as usize;
                    bad[i] ^= 1 << rng.below(8);
                    let _ = decode(encoding, &bad, &data_type, x.len());
                }
            }
        }
    }
}

#[test]
fn decoding_with_codes_matches_plain_decoding() {
    let mut rng = Rng::new(41);
    for &n in &LENGTHS {
        for distinct in [1u64, 3, 50, 5000] {
            let values: Vec<veta_core::Value> = (0..n)
                .map(|_| veta_core::Value::varchar(format!("v{}", rng.below(distinct))))
                .collect();
            let vector = Vector::from_values(DataType::Varchar, &values).unwrap();
            for encoding in [Encoding::Plain, Encoding::Dict, Encoding::Rle] {
                let bytes = encode_with(&vector, encoding).unwrap();
                let plain = decode(encoding, &bytes, &DataType::Varchar, n).unwrap();
                let (kept, codes) =
                    decode_keeping_codes(encoding, &bytes, &DataType::Varchar, n).unwrap();
                assert_eq!(kept, plain);
                match codes {
                    Some(dict) => {
                        assert_eq!(encoding, Encoding::Dict);
                        assert_eq!(dict.codes.len(), n);
                        for (i, &code) in dict.codes.iter().enumerate() {
                            assert_eq!(dict.entries.get(code as usize), plain.get(i), "row {i}");
                        }
                    }
                    None => assert_ne!(encoding, Encoding::Dict),
                }
                // A truncated block fails the same way in both decoders.
                if !bytes.is_empty() {
                    let cut = &bytes[..bytes.len() - 1];
                    assert_eq!(
                        decode(encoding, cut, &DataType::Varchar, n).is_err(),
                        decode_keeping_codes(encoding, cut, &DataType::Varchar, n).is_err()
                    );
                }
            }
        }
    }
}
