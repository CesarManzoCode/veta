//! Scan filters derived from single-table conjunctions, used to prune parts, granules and
//! Parquet row groups by their min/max (contract §11.2 rule 4, §11.4).
//!
//! A filter never decides which rows match: it only says whether a range of values may
//! contain a match. The full predicate is still evaluated over the rows.

use std::ops::Bound;

use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ScanFilter {
    /// `lower ≤/< column ≤/< upper`, each bound included, excluded or absent.
    Range {
        column: usize,
        lower: Bound<Value>,
        upper: Bound<Value>,
    },
    /// `column IN (values)`; NULL entries never match.
    In { column: usize, values: Vec<Value> },
}

fn above_lower(lower: &Bound<Value>, value: &Value) -> bool {
    match lower {
        Bound::Unbounded => true,
        Bound::Included(l) => value >= l,
        Bound::Excluded(l) => value > l,
    }
}

fn below_upper(upper: &Bound<Value>, value: &Value) -> bool {
    match upper {
        Bound::Unbounded => true,
        Bound::Included(u) => value <= u,
        Bound::Excluded(u) => value < u,
    }
}

fn bound_is_null(bound: &Bound<Value>) -> bool {
    matches!(bound, Bound::Included(v) | Bound::Excluded(v) if v.is_null())
}

impl ScanFilter {
    /// `column = value`.
    pub fn equal(column: usize, value: Value) -> ScanFilter {
        ScanFilter::Range {
            column,
            lower: Bound::Included(value.clone()),
            upper: Bound::Included(value),
        }
    }

    pub fn column(&self) -> usize {
        match self {
            ScanFilter::Range { column, .. } | ScanFilter::In { column, .. } => *column,
        }
    }

    /// Whether some value in `[min, max]` (the bounds of the non-NULL values of a part,
    /// granule or row group) may satisfy the filter. `min` or `max` NULL means the range
    /// holds only NULLs, which no comparison accepts.
    pub fn may_match(&self, min: &Value, max: &Value) -> bool {
        if min.is_null() || max.is_null() {
            return false;
        }
        match self {
            ScanFilter::Range { lower, upper, .. } => {
                // A comparison with NULL is never true.
                if bound_is_null(lower) || bound_is_null(upper) {
                    return false;
                }
                above_lower(lower, max) && below_upper(upper, min)
            }
            ScanFilter::In { values, .. } => {
                values.iter().any(|v| !v.is_null() && v >= min && v <= max)
            }
        }
    }

    /// Whether some value `≥ min` may satisfy the filter: the range of a granule whose
    /// upper end is NULL, which sorts after every value (contract §4), so its non-NULL
    /// values are only bounded below. `min` NULL means the range holds only NULLs.
    pub fn may_match_from(&self, min: &Value) -> bool {
        if min.is_null() {
            return false;
        }
        match self {
            ScanFilter::Range { lower, upper, .. } => {
                !bound_is_null(lower) && !bound_is_null(upper) && below_upper(upper, min)
            }
            ScanFilter::In { values, .. } => values.iter().any(|v| !v.is_null() && v >= min),
        }
    }

    /// Whether one value satisfies the filter.
    pub fn matches(&self, value: &Value) -> bool {
        self.may_match(value, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(v: i64) -> Value {
        Value::Int64(v)
    }

    fn range(lower: Bound<Value>, upper: Bound<Value>) -> ScanFilter {
        ScanFilter::Range {
            column: 0,
            lower,
            upper,
        }
    }

    #[test]
    fn closed_bounds_include_the_edges() {
        let f = range(Bound::Included(int(10)), Bound::Included(int(20)));
        assert!(f.may_match(&int(0), &int(10)));
        assert!(f.may_match(&int(20), &int(30)));
        assert!(f.may_match(&int(12), &int(15)));
        assert!(f.may_match(&int(0), &int(100)));
        assert!(!f.may_match(&int(0), &int(9)));
        assert!(!f.may_match(&int(21), &int(30)));
    }

    #[test]
    fn open_bounds_exclude_the_edges() {
        let f = range(Bound::Excluded(int(10)), Bound::Excluded(int(20)));
        assert!(!f.may_match(&int(0), &int(10)));
        assert!(!f.may_match(&int(20), &int(30)));
        assert!(f.may_match(&int(0), &int(11)));
        assert!(f.may_match(&int(19), &int(30)));
        assert!(f.may_match(&int(10), &int(20)));
        // A granule holding only 10 cannot satisfy `x > 10`.
        assert!(!f.may_match(&int(10), &int(10)));
    }

    #[test]
    fn half_open_and_unbounded() {
        let lt = range(Bound::Unbounded, Bound::Excluded(int(5)));
        assert!(lt.may_match(&int(-100), &int(-50)));
        assert!(lt.may_match(&int(4), &int(9)));
        assert!(!lt.may_match(&int(5), &int(9)));
        let ge = range(Bound::Included(int(5)), Bound::Unbounded);
        assert!(ge.may_match(&int(5), &int(5)));
        assert!(!ge.may_match(&int(1), &int(4)));
        let all = range(Bound::Unbounded, Bound::Unbounded);
        assert!(all.may_match(&int(1), &int(1)));
    }

    #[test]
    fn equality_and_in() {
        let eq = ScanFilter::equal(2, Value::varchar("google"));
        assert_eq!(eq.column(), 2);
        assert!(eq.may_match(&Value::varchar("a"), &Value::varchar("z")));
        assert!(!eq.may_match(&Value::varchar("h"), &Value::varchar("z")));
        let set = ScanFilter::In {
            column: 1,
            values: vec![int(3), Value::Null, int(50)],
        };
        assert!(set.may_match(&int(0), &int(3)));
        assert!(set.may_match(&int(40), &int(60)));
        assert!(!set.may_match(&int(4), &int(49)));
        assert!(set.matches(&int(50)));
        assert!(!set.matches(&int(51)));
    }

    #[test]
    fn nulls_never_match() {
        let f = range(Bound::Included(int(1)), Bound::Unbounded);
        assert!(!f.may_match(&Value::Null, &Value::Null));
        let null_bound = range(Bound::Included(Value::Null), Bound::Unbounded);
        assert!(!null_bound.may_match(&int(0), &int(10)));
        assert!(
            !ScanFilter::In {
                column: 0,
                values: vec![Value::Null]
            }
            .may_match(&int(0), &int(10))
        );
    }

    #[test]
    fn bounded_below_only() {
        let lt = range(Bound::Unbounded, Bound::Excluded(int(5)));
        assert!(lt.may_match_from(&int(4)));
        assert!(!lt.may_match_from(&int(5)));
        let gt = range(Bound::Excluded(int(100)), Bound::Unbounded);
        assert!(gt.may_match_from(&int(1000)));
        assert!(gt.may_match_from(&int(0)));
        let set = ScanFilter::In {
            column: 0,
            values: vec![int(3), Value::Null],
        };
        assert!(set.may_match_from(&int(3)));
        assert!(!set.may_match_from(&int(4)));
        assert!(!gt.may_match_from(&Value::Null));
        let null_bound = range(Bound::Unbounded, Bound::Included(Value::Null));
        assert!(!null_bound.may_match_from(&int(0)));
    }

    #[test]
    fn mixed_numeric_and_temporal_bounds() {
        let dec = Value::Decimal {
            value: 5,
            precision: 3,
            scale: 2,
        };
        let f = range(Bound::Included(dec), Bound::Unbounded);
        assert!(f.may_match(&Value::Float64(0.0), &Value::Float64(0.05)));
        assert!(!f.may_match(&Value::Float64(0.0), &Value::Float64(0.04)));
        let day = range(
            Bound::Included(Value::Date(1)),
            Bound::Excluded(Value::Date(2)),
        );
        assert!(day.may_match(&Value::Timestamp(0), &Value::Timestamp(86_400)));
        assert!(!day.may_match(&Value::Timestamp(0), &Value::Timestamp(86_399)));
    }
}
