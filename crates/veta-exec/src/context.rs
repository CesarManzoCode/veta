//! What every operator of a query shares: the database, the thread count, the counters of
//! `VETA_STATS` and, under EXPLAIN ANALYZE, the rows of each node (contract §11.6, §12, §13).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use veta_core::Result;
use veta_core::memory::{self, Reservation};
use veta_sql::LogicalPlan;

use crate::database::Database;

/// Options of one query execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOptions {
    /// Worker threads, at least 1.
    pub threads: usize,
    /// `false` with `--no-prune`: every part, granule and row group is read (§11.3).
    pub prune: bool,
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
            prune: true,
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

/// Output rows of every node of one plan under EXPLAIN ANALYZE (§11.6). A node is known by
/// its address, which stays put while the plan is borrowed for the execution.
#[derive(Debug)]
pub(crate) struct Profile {
    by_node: HashMap<usize, Arc<AtomicU64>>,
    /// In preorder, as [`veta_sql::explain_json`] reads them.
    preorder: Vec<Arc<AtomicU64>>,
}

fn node_key(plan: &LogicalPlan) -> usize {
    plan as *const LogicalPlan as usize
}

impl Profile {
    pub fn new(plan: &LogicalPlan) -> Profile {
        fn visit(plan: &LogicalPlan, profile: &mut Profile) {
            let counter = Arc::new(AtomicU64::new(0));
            profile.by_node.insert(node_key(plan), counter.clone());
            profile.preorder.push(counter);
            for child in plan.children() {
                visit(child, profile);
            }
        }
        let mut profile = Profile {
            by_node: HashMap::new(),
            preorder: Vec::new(),
        };
        visit(plan, &mut profile);
        profile
    }

    pub fn counter(&self, plan: &LogicalPlan) -> Option<Arc<AtomicU64>> {
        self.by_node.get(&node_key(plan)).cloned()
    }

    pub fn counts(&self) -> Vec<u64> {
        self.preorder
            .iter()
            .map(|counter| counter.load(Ordering::Relaxed))
            .collect()
    }
}

/// Shared, read-only context of the operators of a query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecContext<'a> {
    pub db: &'a Database,
    pub threads: usize,
    pub stats: &'a ExecStats,
    /// Whether scans use their filters to skip parts, granules and row groups.
    pub prune: bool,
    /// Under EXPLAIN ANALYZE, where each node counts its output rows.
    pub profile: Option<&'a Profile>,
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
    fn a_profile_counts_each_node_in_preorder() {
        let scan = |name: &str| {
            let schema = veta_core::Schema::new(vec![veta_core::Field::new(
                "x",
                veta_core::DataType::Int64,
                false,
            )]);
            LogicalPlan::scan(name, name, schema)
        };
        let plan = LogicalPlan::join(
            scan("a"),
            scan("b"),
            veta_sql::JoinKind::Cross,
            Vec::new(),
            None,
        );
        let profile = Profile::new(&plan);
        let children = plan.children();
        add(&profile.counter(&plan).unwrap(), 3);
        add(&profile.counter(children[0]).unwrap(), 1);
        add(&profile.counter(children[1]).unwrap(), 2);
        assert_eq!(profile.counts(), vec![3, 1, 2]);
        assert!(profile.counter(&scan("other")).is_none());
    }

    #[test]
    fn default_threads_is_positive() {
        assert!(ExecOptions::default().threads >= 1);
        assert!(ExecOptions::default().prune);
    }
}
