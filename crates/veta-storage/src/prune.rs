//! Pruning of parts and granules with the sparse primary index and `stats.bin`
//! (contract §11.4).
//!
//! The key range of granule `g` is `[idx[g], idx[g+1]]`: the first row of the next granule
//! (or the last row of the part) bounds the last value of `g`. Over that pair of key tuples
//! the analysis goes column by column: key column 0 lies in `[lo0, hi0]`, and key column
//! `k` is only bounded when columns `0..k` are equal at both ends, because only then are
//! its values sorted between them. NULL sorts after every value (§4): a range starting at
//! NULL holds only NULLs, and one ending at NULL is unbounded above.
//!
//! Filters name columns by their index in the table schema; columns outside the key only
//! prune whole parts, through their min/max.

use veta_core::Value;
use veta_core::predicate::ScanFilter;
use veta_core::stats::ColumnStats;

use crate::part::PartReader;

/// Whether some non-NULL value between `lo` and `hi` (values of one sorted column) may
/// satisfy `filter`.
fn column_may_match(filter: &ScanFilter, lo: &Value, hi: &Value) -> bool {
    if hi.is_null() {
        filter.may_match_from(lo)
    } else {
        filter.may_match(lo, hi)
    }
}

/// Whether some row whose key tuple lies in `[lo, hi]` may satisfy every filter.
/// `key[i]` is the schema index of key column `i`; `lo[i]` and `hi[i]` its values.
pub fn key_range_may_match(
    key: &[usize],
    lo: &[Value],
    hi: &[Value],
    filters: &[ScanFilter],
) -> bool {
    for (pos, &column) in key.iter().enumerate() {
        let (l, h) = (&lo[pos], &hi[pos]);
        if filters
            .iter()
            .filter(|f| f.column() == column)
            .any(|f| !column_may_match(f, l, h))
        {
            return false;
        }
        // Equality in the sort order: NULL equals NULL here.
        if l != h {
            break;
        }
    }
    true
}

/// Whether the min/max of one column of a part may satisfy `filter`. A column of only
/// NULLs never does; one without min/max (long strings) always may.
fn stats_may_match(filter: &ScanFilter, stats: &ColumnStats) -> bool {
    if stats.rows > 0 && stats.nulls == stats.rows {
        return false;
    }
    match (&stats.min, &stats.max) {
        (Some(min), Some(max)) => filter.may_match(min, max),
        _ => true,
    }
}

fn index_tuple(part: &PartReader, position: usize) -> Vec<Value> {
    part.index().iter().map(|v| v.get(position)).collect()
}

/// Whether part `part` may hold a row that satisfies every filter: neither its full key
/// range nor the min/max in `stats.bin` of a filtered column excludes them.
pub fn part_may_match(part: &PartReader, filters: &[ScanFilter]) -> bool {
    if part.granules() == 0 {
        return false;
    }
    let stats = &part.stats().columns;
    if filters.iter().any(|f| {
        stats
            .get(f.column())
            .is_some_and(|s| !stats_may_match(f, s))
    }) {
        return false;
    }
    let lo = index_tuple(part, 0);
    let hi = index_tuple(part, part.granules());
    key_range_may_match(part.primary_key(), &lo, &hi, filters)
}

/// The granules of `part` that may hold a row satisfying every filter, in order. Without
/// filters on key columns every granule is selected.
pub fn select_granules(part: &PartReader, filters: &[ScanFilter]) -> Vec<usize> {
    let granules = part.granules();
    let key = part.primary_key();
    if !filters.iter().any(|f| key.contains(&f.column())) {
        return (0..granules).collect();
    }
    let mut out = Vec::new();
    let mut lo = index_tuple(part, 0);
    for g in 0..granules {
        let next = index_tuple(part, g + 1);
        let hi = &next;
        // Mutant (contract §18): the range of `g` ends at `idx[g]` instead of `idx[g+1]`.
        #[cfg(feature = "mutant-prune-wrong-bound")]
        let hi = &lo;
        if key_range_may_match(key, &lo, hi, filters) {
            out.push(g);
        }
        lo = next;
    }
    // Mutant (contract §18): the last granule of each run of selected granules is dropped.
    #[cfg(feature = "mutant-prune-extra-granule")]
    {
        let selected = out.clone();
        out.retain(|g| selected.contains(&(g + 1)));
    }
    out
}

/// The granules to read from `part`: none if the part is excluded, otherwise those of
/// [`select_granules`].
pub fn prune_part(part: &PartReader, filters: &[ScanFilter]) -> Vec<usize> {
    if filters.is_empty() {
        return (0..part.granules()).collect();
    }
    if !part_may_match(part, filters) {
        return Vec::new();
    }
    select_granules(part, filters)
}

#[cfg(test)]
mod tests;
