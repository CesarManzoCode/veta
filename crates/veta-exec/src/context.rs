//! What every operator of a query shares: the database, the thread count and the counters
//! of `VETA_STATS` (contract §12, §13).

use std::sync::atomic::{AtomicU64, Ordering};

use veta_core::Result;
use veta_core::memory::{self, Reservation};

use crate::database::Database;

/// Options of one query execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOptions {
    /// Worker threads, at least 1.
    pub threads: usize,
}

impl ExecOptions {
    /// `--threads` by default: `available_parallelism` (contract §12).
    pub fn default_threads() -> usize {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    }
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            threads: ExecOptions::default_threads(),
        }
    }
}

/// Counters of `VETA_STATS` (contract §13).
#[derive(Debug, Default)]
pub struct ExecStats {
    pub parts_total: AtomicU64,
    pub parts_read: AtomicU64,
    pub granules_total: AtomicU64,
    pub granules_read: AtomicU64,
    pub rows_read: AtomicU64,
    pub row_groups_total: AtomicU64,
    pub row_groups_read: AtomicU64,
}

impl ExecStats {
    /// The JSON object of the `VETA_STATS` line, with every key always present.
    pub fn to_json(&self) -> String {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        format!(
            "{{\"parts_total\":{},\"parts_read\":{},\"granules_total\":{},\"granules_read\":{},\"rows_read\":{},\"row_groups_total\":{},\"row_groups_read\":{}}}",
            get(&self.parts_total),
            get(&self.parts_read),
            get(&self.granules_total),
            get(&self.granules_read),
            get(&self.rows_read),
            get(&self.row_groups_total),
            get(&self.row_groups_read),
        )
    }
}

pub(crate) fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// Shared, read-only context of the operators of a query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecContext<'a> {
    pub db: &'a Database,
    pub threads: usize,
    pub stats: &'a ExecStats,
}

/// An empty reservation on the process-wide memory tracker.
pub(crate) fn new_reservation() -> Result<Reservation<'static>> {
    memory::global().reserve(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_json_has_every_key() {
        let stats = ExecStats::default();
        add(&stats.rows_read, 5);
        add(&stats.rows_read, 2);
        add(&stats.row_groups_total, 1);
        assert_eq!(
            stats.to_json(),
            "{\"parts_total\":0,\"parts_read\":0,\"granules_total\":0,\"granules_read\":0,\"rows_read\":7,\"row_groups_total\":1,\"row_groups_read\":0}"
        );
    }

    #[test]
    fn default_threads_is_positive() {
        assert!(ExecOptions::default().threads >= 1);
    }
}
