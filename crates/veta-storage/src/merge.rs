//! Merging parts (contract §6.2, §7).
//!
//! **Selection.** [`select_window`] takes the active parts sorted by `min_block` and looks
//! at every contiguous window of 2 to 10 of them whose rows fit `--max-merge-rows` and whose
//! estimated memory `rows × (key bytes + 2)` fits `--merge-memory-mb`. The score is the sum
//! of rows over the rows of the largest part, at least 1.5; the highest score wins, then the
//! fewest rows, then the lowest blocks. `--final` ignores the threshold and the row limit.
//! §7 does not say how wide a string key is: here it is the mean length in the part's sample
//! plus its 4-byte offset.
//!
//! **Algorithm.** Vertical: the key columns of the sources are loaded granule by granule, a
//! stable k-way merge ([`merge_order`]) gives the source (`u8`) of every output row, and the
//! keys are dropped. Then each column is written alone, reading each source sequentially one
//! granule at a time, so a column holds at most one granule per source and one of output.
//! The new part's statistics and sample come out of [`PartWriter`] as its rows go through.
//! Rows with equal keys keep the order of their blocks, and a table without a key ends up
//! in block order.
//!
//! **Protocol (§6.2).** `.merge.lock` is held for the whole merge; the part is built in
//! `tmp_merge_<name>` with a held `owner.lock`, fsynced and renamed to its name; then, under
//! an exclusive `.readers.lock`, every part it covers is renamed to `delete_tmp_<name>` and
//! deleted. Readers are only stopped for that last step and the sources stay readable through
//! their open descriptors. A `tmp_merge_*` left behind by a dead merge is removed as soon as
//! the same window is merged again, since `.merge.lock` proves its owner gone.
//!
//! **Blocks in flight.** Beyond §6.2: a window must not span the block of an insert that is
//! still running, because the part published later would be covered, and so lost, at birth.
//! The directories are listed under a shared `.insert.lock`, which [`crate::insert`] holds
//! from assigning a block until its `tmp_insert_<b>` exists and is locked. So a block in the
//! listed range is either a part, a `tmp_insert_<b>` with a held `owner.lock` (in flight) or
//! gone for good (dead insert, detached part), and only the second one stops a window.

use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::Path;
use std::thread;
use std::time::Duration;

use veta_core::{Result, Vector, VectorData, VetaError};

use crate::fsutil::{io_err, remove_dir_all_if_exists, subdirectories, sync_dir};
use crate::insert::publish_part;
use crate::locks::{FileLock, lock_exclusive, lock_shared, try_lock_exclusive};
use crate::part::{
    GRANULE_ROWS, OWNER_LOCK, PartName, PartReader, PartWriter, active_parts, list_parts,
};
use crate::recover::{DetachedPart, delete_covered_part, detach_part};
use crate::sort::SortKey;
use crate::table::{
    INSERT_LOCK, MERGE_LOCK, READERS_LOCK, TMP_INSERT_PREFIX, TMP_MERGE_PREFIX, Table,
};

pub const DEFAULT_MAX_MERGE_ROWS: u64 = 20_000_000;
pub const DEFAULT_MERGE_MEMORY_MB: u64 = 2048;
/// Most parts in a window (contract §7), which is why a source index fits in a `u8`.
pub const MAX_WINDOW_PARTS: usize = 10;

/// How long `--final` waits before looking again when only an insert in flight stops it.
const FINAL_RETRY: Duration = Duration::from_millis(50);

/// Limits of the merge policy (contract §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeOptions {
    pub max_merge_rows: u64,
    pub merge_memory_mb: u64,
    /// `--final`: no score threshold, no row limit, repeat until one active part is left.
    pub final_merge: bool,
}

impl Default for MergeOptions {
    fn default() -> MergeOptions {
        MergeOptions {
            max_merge_rows: DEFAULT_MAX_MERGE_ROWS,
            merge_memory_mb: DEFAULT_MERGE_MEMORY_MB,
            final_merge: false,
        }
    }
}

/// What selection needs to know about an active part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeCandidate {
    pub name: PartName,
    pub rows: u64,
    /// Estimated bytes of the key of one row.
    pub key_bytes: u64,
}

/// The window to merge among `parts` (active, sorted by `min_block`), as a range of indices.
/// `in_flight` are blocks of running inserts, which no window may span.
pub fn select_window(
    parts: &[MergeCandidate],
    in_flight: &[u64],
    options: &MergeOptions,
) -> Option<Range<usize>> {
    let memory_limit = (options.merge_memory_mb as u128) << 20;
    // (start, end, rows, rows of the largest part); scores are compared as exact fractions.
    let mut best: Option<(usize, usize, u128, u128)> = None;
    for start in 0..parts.len() {
        let (mut rows, mut largest, mut memory) = (0u128, 0u128, 0u128);
        for end in start..parts.len().min(start + MAX_WINDOW_PARTS) {
            let part = &parts[end];
            if end > start
                && in_flight
                    .iter()
                    .any(|&b| parts[end - 1].name.max_block < b && b < part.name.min_block)
            {
                break;
            }
            rows += part.rows as u128;
            largest = largest.max(part.rows as u128);
            memory += part.rows as u128 * (part.key_bytes as u128 + 2);
            if memory > memory_limit
                || (!options.final_merge && rows > options.max_merge_rows as u128)
            {
                break;
            }
            if end == start || (!options.final_merge && 2 * rows < 3 * largest) {
                continue;
            }
            let better = best.is_none_or(|(_, _, best_rows, best_largest)| {
                let (score, best_score) = (rows * best_largest, best_rows * largest);
                score > best_score || (score == best_score && rows < best_rows)
            });
            if better {
                best = Some((start, end, rows, largest));
            }
        }
    }
    best.map(|(start, end, _, _)| start..end + 1)
}

/// Estimated bytes of the key of one row of `part`: the width of each fixed-size column,
/// and the mean length in the sample plus a 4-byte offset for each string.
fn key_row_bytes(part: &PartReader) -> u64 {
    let sample = part.sample();
    part.primary_key()
        .iter()
        .map(|&c| match part.schema().field(c).data_type.fixed_width() {
            Some(width) => width as u64,
            None => match &sample.column(c).data {
                VectorData::Varchar { bytes, .. } => {
                    (bytes.len() as u64).div_ceil(sample.len.max(1) as u64) + 4
                }
                _ => 4,
            },
        })
        .sum()
}

/// The key of one source: `rows` rows and, per granule, its key columns in key order.
struct SourceKeys {
    rows: u64,
    granules: Vec<Vec<Vector>>,
}

/// The read position of one source during the k-way merge.
struct Cursor<'a> {
    source: usize,
    row: u64,
    rows: u64,
    granules: Vec<Vec<SortKey<'a>>>,
}

/// Whether the next row of `a` goes before the next row of `b`; equal keys go in source
/// order, which makes the merge stable.
#[inline]
fn goes_first(a: &Cursor<'_>, b: &Cursor<'_>) -> bool {
    let granule = GRANULE_ROWS as u64;
    let keys_a = &a.granules[(a.row / granule) as usize];
    let keys_b = &b.granules[(b.row / granule) as usize];
    let (row_a, row_b) = ((a.row % granule) as usize, (b.row % granule) as usize);
    for (x, y) in keys_a.iter().zip(keys_b) {
        match x.cmp_with(row_a, y, row_b) {
            std::cmp::Ordering::Less => return true,
            std::cmp::Ordering::Greater => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    a.source < b.source
}

fn sift_down(heap: &mut [usize], cursors: &[Cursor<'_>], mut at: usize) {
    loop {
        let (left, right) = (2 * at + 1, 2 * at + 2);
        let mut first = at;
        if left < heap.len() && goes_first(&cursors[heap[left]], &cursors[heap[first]]) {
            first = left;
        }
        if right < heap.len() && goes_first(&cursors[heap[right]], &cursors[heap[first]]) {
            first = right;
        }
        if first == at {
            return;
        }
        heap.swap(at, first);
        at = first;
    }
}

/// The source of every output row: a stable k-way merge of sources sorted by their key. A
/// source without key columns compares equal to everything, so sources simply follow one
/// another.
fn merge_order(sources: &[SourceKeys]) -> Vec<u8> {
    let total: u64 = sources.iter().map(|s| s.rows).sum();
    let mut order = Vec::with_capacity(total as usize);
    if sources.iter().all(|s| s.granules.is_empty()) {
        for (source, keys) in sources.iter().enumerate() {
            order.resize(order.len() + keys.rows as usize, source as u8);
        }
        return order;
    }
    let mut cursors: Vec<Cursor<'_>> = sources
        .iter()
        .enumerate()
        .map(|(source, keys)| Cursor {
            source,
            row: 0,
            rows: keys.rows,
            granules: keys
                .granules
                .iter()
                .map(|columns| columns.iter().map(SortKey::new).collect())
                .collect(),
        })
        .collect();
    let mut heap: Vec<usize> = (0..cursors.len())
        .filter(|&c| cursors[c].rows > 0)
        .collect();
    for at in (0..heap.len() / 2).rev() {
        sift_down(&mut heap, &cursors, at);
    }
    while let Some(&next) = heap.first() {
        order.push(next as u8);
        let cursor = &mut cursors[next];
        cursor.row += 1;
        if cursor.row == cursor.rows {
            heap.swap_remove(0);
        }
        sift_down(&mut heap, &cursors, 0);
    }
    order
}

/// Loads the key columns of `sources` granule by granule, adding them to the key prefix
/// HLLs of `writer` on the way.
fn load_keys(
    sources: &[PartReader],
    key: &[usize],
    writer: &mut PartWriter,
) -> Result<Vec<SourceKeys>> {
    let mut out = Vec::with_capacity(sources.len());
    for part in sources {
        let mut granules = Vec::new();
        if !key.is_empty() {
            granules.reserve(part.granules());
            for granule in 0..part.granules() {
                let columns = key
                    .iter()
                    .map(|&c| part.read_granule(c, granule))
                    .collect::<Result<Vec<Vector>>>()?;
                writer.add_key_rows(&columns.iter().collect::<Vec<_>>())?;
                granules.push(columns);
            }
        }
        out.push(SourceKeys {
            rows: part.rows(),
            granules,
        });
    }
    Ok(out)
}

/// One column of one source, read sequentially one granule at a time.
struct ColumnReader<'a> {
    part: &'a PartReader,
    column: usize,
    next_granule: usize,
    current: Vector,
    position: usize,
}

impl ColumnReader<'_> {
    /// Appends the next `count` rows to `out`.
    fn take(&mut self, mut count: usize, out: &mut Vector) -> Result<()> {
        while count > 0 {
            if self.position == self.current.len() {
                self.current = self.part.read_granule(self.column, self.next_granule)?;
                self.next_granule += 1;
                self.position = 0;
            }
            let rows = count.min(self.current.len() - self.position);
            out.extend(&self.current.slice(self.position, rows))?;
            self.position += rows;
            count -= rows;
        }
        Ok(())
    }
}

/// Writes `column` of the merged part: for each output granule, the rows each source gives
/// (they come in that source's order) are appended and then put in `order`.
fn write_merged_column(
    sources: &[PartReader],
    order: &[u8],
    column: usize,
    writer: &mut PartWriter,
) -> Result<()> {
    let data_type = sources[0].schema().field(column).data_type;
    let mut readers: Vec<ColumnReader<'_>> = sources
        .iter()
        .map(|part| ColumnReader {
            part,
            column,
            next_granule: 0,
            current: Vector::with_capacity(data_type, 0),
            position: 0,
        })
        .collect();
    let mut counts = vec![0usize; sources.len()];
    let mut next = vec![0u32; sources.len()];
    let mut permutation = Vec::with_capacity(GRANULE_ROWS);
    for batch in order.chunks(GRANULE_ROWS) {
        counts.fill(0);
        for &source in batch {
            counts[source as usize] += 1;
        }
        let mut rows = Vector::with_capacity(data_type, batch.len());
        let mut base = 0;
        for (source, reader) in readers.iter_mut().enumerate() {
            next[source] = base;
            base += counts[source] as u32;
            reader.take(counts[source], &mut rows)?;
        }
        if counts.contains(&batch.len()) {
            writer.write_column(column, &rows)?;
        } else {
            permutation.clear();
            for &source in batch {
                permutation.push(next[source as usize]);
                next[source as usize] += 1;
            }
            writer.write_column(column, &rows.gather(&permutation))?;
        }
    }
    Ok(())
}

/// The block numbers of the inserts still running: a `tmp_insert_<b>` whose `owner.lock` is
/// held. The caller holds `.insert.lock`, so nobody is between creating one and locking it.
fn live_insert_blocks(table_dir: &Path) -> Result<Vec<u64>> {
    let mut blocks = Vec::new();
    for name in subdirectories(table_dir)? {
        let Some(block) = name
            .strip_prefix(TMP_INSERT_PREFIX)
            .and_then(|b| b.parse::<u64>().ok())
        else {
            continue;
        };
        let path = table_dir.join(&name);
        match try_lock_exclusive(&path.join(OWNER_LOCK)) {
            Ok(Some(_dead)) => {}
            Ok(None) => blocks.push(block),
            // Published meanwhile, so the listing of parts that follows sees it.
            Err(_) if !path.is_dir() => {}
            Err(err) => return Err(err),
        }
    }
    Ok(blocks)
}

/// A finished merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedPart {
    pub name: PartName,
    pub rows: u64,
    pub sources: Vec<PartName>,
}

/// The outcome of one selection and merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStep {
    Merged(MergedPart),
    /// No window qualifies; `in_flight` inserts may be what stops one.
    Idle {
        active: usize,
        in_flight: usize,
    },
    /// Another merger holds `.merge.lock` ([`Merger::try_merge_once`]).
    Busy,
}

/// What [`Merger::run`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeSummary {
    pub merges: u64,
    pub active: usize,
}

/// Merges parts of one table. It remembers what selection read from each part, since parts
/// are immutable and a background merger looks at them over and over.
#[derive(Debug)]
pub struct Merger {
    table: Table,
    options: MergeOptions,
    known: HashMap<PartName, MergeCandidate>,
    detached: Vec<DetachedPart>,
}

impl Merger {
    pub fn new(table: Table, options: MergeOptions) -> Merger {
        Merger {
            table,
            options,
            known: HashMap::new(),
            detached: Vec::new(),
        }
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    /// Parts found broken when opened and detached since the last call; the caller reports
    /// each with [`DetachedPart::message`] on stderr.
    pub fn take_detached(&mut self) -> Vec<DetachedPart> {
        std::mem::take(&mut self.detached)
    }

    /// Selects a window and merges it, waiting for `.merge.lock`.
    pub fn merge_once(&mut self) -> Result<MergeStep> {
        let lock = lock_exclusive(&self.table.lock_path(MERGE_LOCK))?;
        self.step(lock)
    }

    /// Selects a window and merges it unless another merger holds `.merge.lock`.
    pub fn try_merge_once(&mut self) -> Result<MergeStep> {
        match try_lock_exclusive(&self.table.lock_path(MERGE_LOCK))? {
            Some(lock) => self.step(lock),
            None => Ok(MergeStep::Busy),
        }
    }

    /// Repeats selection and merge until no window qualifies. With `--final` that must leave
    /// one active part: it waits for inserts in flight, and if only the memory limit stops
    /// it, the result is a `User` error.
    pub fn run(&mut self) -> Result<MergeSummary> {
        let mut merges = 0;
        loop {
            match self.merge_once()? {
                MergeStep::Merged(_) => merges += 1,
                MergeStep::Idle { active, .. } if !self.options.final_merge || active <= 1 => {
                    return Ok(MergeSummary { merges, active });
                }
                MergeStep::Idle { in_flight, .. } if in_flight > 0 => thread::sleep(FINAL_RETRY),
                MergeStep::Idle { active, .. } => {
                    return Err(VetaError::user(format!(
                        "merge --final: {active} active parts of {} remain, merging them would \
                         exceed --merge-memory-mb {}",
                        self.table.name(),
                        self.options.merge_memory_mb
                    )));
                }
                MergeStep::Busy => return Err(VetaError::internal(".merge.lock was not waited")),
            }
        }
    }

    fn step(&mut self, _merge_lock: FileLock) -> Result<MergeStep> {
        loop {
            let (candidates, in_flight) = self.candidates()?;
            let Some(window) = select_window(&candidates, &in_flight, &self.options) else {
                return Ok(MergeStep::Idle {
                    active: candidates.len(),
                    in_flight: in_flight.len(),
                });
            };
            // A source that vanished or broke is out of the next listing.
            if let Some(sources) = self.open_sources(&candidates[window])? {
                return self.merge_sources(&sources).map(MergeStep::Merged);
            }
        }
    }

    /// The active parts and the blocks in flight. The `tmp_insert_*` are listed before the
    /// parts: a block published between both listings then is seen at least once.
    fn candidates(&mut self) -> Result<(Vec<MergeCandidate>, Vec<u64>)> {
        let dir = self.table.dir().to_path_buf();
        let (names, in_flight) = {
            let _inserts = lock_shared(&self.table.lock_path(INSERT_LOCK))?;
            let in_flight = live_insert_blocks(&dir)?;
            (active_parts(&list_parts(&dir)?), in_flight)
        };
        self.known.retain(|name, _| names.contains(name));
        let mut candidates = Vec::with_capacity(names.len());
        for name in names {
            if let Some(known) = self.known.get(&name) {
                candidates.push(*known);
                continue;
            }
            let path = dir.join(name.to_string());
            let opened =
                PartReader::open_columns(&path, self.table.schema(), self.table.primary_key(), &[]);
            match opened {
                Ok(part) => {
                    let candidate = MergeCandidate {
                        name,
                        rows: part.rows(),
                        key_bytes: key_row_bytes(&part),
                    };
                    self.known.insert(name, candidate);
                    candidates.push(candidate);
                }
                Err(_) if !path.is_dir() => {}
                Err(VetaError::Corrupt { reason, .. }) => self.detach(name, reason)?,
                Err(err) => return Err(err),
            }
        }
        Ok((candidates, in_flight))
    }

    fn detach(&mut self, name: PartName, reason: String) -> Result<()> {
        detach_part(self.table.dir(), name)?;
        self.known.remove(&name);
        self.detached.push(DetachedPart { name, reason });
        Ok(())
    }

    /// Opens the parts of `window` as a reader does (§6.3); `None` if one is not there or
    /// not whole any more.
    fn open_sources(&mut self, window: &[MergeCandidate]) -> Result<Option<Vec<PartReader>>> {
        let _readers = lock_shared(&self.table.lock_path(READERS_LOCK))?;
        let mut sources = Vec::with_capacity(window.len());
        for candidate in window {
            let path = self.table.dir().join(candidate.name.to_string());
            match PartReader::open(&path, self.table.schema(), self.table.primary_key()) {
                Ok(part) => sources.push(part),
                Err(_) if !path.is_dir() => return Ok(None),
                Err(VetaError::Corrupt { reason, .. }) => {
                    self.detach(candidate.name, reason)?;
                    return Ok(None);
                }
                Err(err) => return Err(err),
            }
        }
        Ok(Some(sources))
    }

    /// Steps 2 to 4 of §6.2 on `sources`, sorted by block. A corrupt block in a source fails
    /// the merge, and that source is detached first (§6.4).
    fn merge_sources(&mut self, sources: &[PartReader]) -> Result<MergedPart> {
        let names: Vec<PartName> = sources.iter().map(PartReader::name).collect();
        let (first, last) = match (names.first(), names.last()) {
            (Some(first), Some(last)) => (first, last),
            _ => return Err(VetaError::internal("a merge needs sources")),
        };
        let level = names.iter().map(|n| n.level).max().unwrap_or(0);
        let name = PartName::new(first.min_block, last.max_block, level + 1);

        let dir = self.table.dir();
        let tmp = dir.join(format!("{TMP_MERGE_PREFIX}{name}"));
        remove_abandoned_tmp(&tmp)?;
        fs::create_dir(&tmp).map_err(|e| io_err(&tmp, e))?;
        let owner = match lock_exclusive(&tmp.join(OWNER_LOCK)) {
            Ok(lock) => lock,
            Err(err) => {
                let _ = remove_dir_all_if_exists(&tmp);
                return Err(err);
            }
        };
        let written = write_and_publish(&self.table, sources, name, &tmp);
        if let Err(err) = &written {
            let _ = remove_dir_all_if_exists(&tmp);
            // The merge fails with `err` whatever the detaching does.
            if let VetaError::Corrupt { part, .. } = err
                && let Some(broken) = PartName::parse(part)
            {
                let _ = detach_part(self.table.dir(), broken);
                self.known.remove(&broken);
            }
        }
        drop(owner);
        let rows = written?;
        delete_covered(&self.table, name)?;
        Ok(MergedPart {
            name,
            rows,
            sources: names,
        })
    }
}

/// Removes a `tmp_merge_*` of the same name left by a dead merge; the caller holds
/// `.merge.lock`, so it cannot be anyone's work in progress.
fn remove_abandoned_tmp(tmp: &Path) -> Result<()> {
    if !tmp.is_dir() {
        return Ok(());
    }
    match try_lock_exclusive(&tmp.join(OWNER_LOCK))? {
        Some(_owner) => remove_dir_all_if_exists(tmp).map(|_| ()),
        None => Err(VetaError::Io(format!(
            "{}: its owner is alive and does not hold .merge.lock",
            tmp.display()
        ))),
    }
}

/// Steps 2 and 3 of §6.2: writes the merged part in `tmp` and publishes it.
fn write_and_publish(
    table: &Table,
    sources: &[PartReader],
    name: PartName,
    tmp: &Path,
) -> Result<u64> {
    let rows: u64 = sources.iter().map(PartReader::rows).sum();
    let mut writer = PartWriter::create(tmp, name, table.schema(), table.primary_key(), rows)?;
    let keys = load_keys(sources, table.primary_key(), &mut writer)?;
    let order = merge_order(&keys);
    drop(keys);
    for column in 0..table.schema().len() {
        write_merged_column(sources, &order, column, &mut writer)?;
    }
    writer.finish()?;
    publish_part(table, tmp, name)?;
    Ok(rows)
}

/// Step 4 of §6.2: deletes the parts `name` covers under an exclusive `.readers.lock`. That
/// includes any left there by a merge that died before this step.
fn delete_covered(table: &Table, name: PartName) -> Result<()> {
    let _readers = lock_exclusive(&table.lock_path(READERS_LOCK))?;
    for part in list_parts(table.dir())? {
        if name.covers(&part) {
            delete_covered_part(table.dir(), part)?;
        }
    }
    sync_dir(table.dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TestDir;
    use crate::insert::insert;
    use crate::part::tests::{all_types_schema, assert_vectors_equal, random_chunk, write_part};
    use crate::part::{PartStats, STATS_BIN};
    use crate::snapshot::Snapshot;
    use crate::sort::sort_chunk;
    use crate::table::tests::sample_def;
    use crate::table::{DETACHED_DIR, TableDef};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use veta_core::rand::Rng;
    use veta_core::{Chunk, DataType, Field, Schema, Value};

    fn candidates(sizes: &[u64]) -> Vec<MergeCandidate> {
        let block = |i: usize| i as u64 + 1;
        sizes
            .iter()
            .enumerate()
            .map(|(i, &rows)| MergeCandidate {
                name: PartName::new(block(i), block(i), 0),
                rows,
                key_bytes: 8,
            })
            .collect()
    }

    fn select(sizes: &[u64], options: MergeOptions) -> Option<Range<usize>> {
        select_window(&candidates(sizes), &[], &options)
    }

    const FINAL: MergeOptions = MergeOptions {
        max_merge_rows: DEFAULT_MAX_MERGE_ROWS,
        merge_memory_mb: DEFAULT_MERGE_MEMORY_MB,
        final_merge: true,
    };

    #[test]
    fn a_score_under_1_5_is_discarded() {
        let default = MergeOptions::default();
        assert_eq!(select(&[], default), None);
        assert_eq!(select(&[100], default), None);
        assert_eq!(select(&[1000, 100], default), None);
        assert_eq!(select(&[1000, 499], default), None);
        // Exactly 1.5 qualifies.
        assert_eq!(select(&[1000, 500], default), Some(0..2));
        assert_eq!(select(&[100, 100, 100], default), Some(0..3));
    }

    #[test]
    fn the_best_score_wins_then_the_fewest_rows_then_the_lowest_blocks() {
        let default = MergeOptions::default();
        // The four together score 2.2; (100, 100) only 2.
        assert_eq!(select(&[1000, 100, 100, 1000], default), Some(0..4));
        assert_eq!(select(&[1000, 10, 10, 10], default), Some(1..4));
        // Ties at 2: (10, 10), (5, 5) and (10, 5, 5) fit 20 rows; (5, 5) has the fewest.
        let limited = MergeOptions {
            max_merge_rows: 20,
            ..default
        };
        assert_eq!(select(&[10, 10, 5, 5], limited), Some(2..4));
        // Same score and rows: the lowest blocks. At most 10 parts in a window.
        assert_eq!(select(&[7; 4], limited), Some(0..2));
        assert_eq!(select(&[7; 12], default), Some(0..10));
    }

    #[test]
    fn the_row_limit_bounds_a_window() {
        let limited = MergeOptions {
            max_merge_rows: 300,
            ..MergeOptions::default()
        };
        assert_eq!(select(&[1000, 100, 100, 1000], limited), Some(1..3));
        assert_eq!(select(&[200, 200], limited), None);
        assert_eq!(select(&[150, 150], limited), Some(0..2));
    }

    #[test]
    fn the_memory_limit_bounds_a_window() {
        // 10 bytes per row; 100 000 rows are about 0.95 MiB and 200 000 about 1.9 MiB.
        let sizes = [100_000, 100_000];
        let limit = |merge_memory_mb, final_merge| MergeOptions {
            merge_memory_mb,
            final_merge,
            ..MergeOptions::default()
        };
        assert_eq!(select(&sizes, limit(1, false)), None);
        assert_eq!(select(&sizes, limit(2, false)), Some(0..2));
        assert_eq!(select(&sizes, limit(1, true)), None);
        assert_eq!(select(&sizes, limit(2, true)), Some(0..2));
        // Right at the limit: 1 MiB holds 104 857 rows of 10 bytes.
        assert_eq!(select(&[52_429, 52_429], limit(1, false)), None);
        assert_eq!(select(&[52_429, 52_428], limit(1, false)), Some(0..2));
        // A string key is estimated from the part: wider keys, less fits.
        let mut wide = candidates(&sizes);
        wide[0].key_bytes = 100;
        assert_eq!(select_window(&wide, &[], &limit(2, false)), None);
    }

    #[test]
    fn final_ignores_the_score_and_the_row_limit() {
        assert_eq!(select(&[1000, 100], FINAL), Some(0..2));
        let small_rows = MergeOptions {
            max_merge_rows: 1,
            ..FINAL
        };
        assert_eq!(select(&[1000, 100, 100, 1000], small_rows), Some(0..4));
        assert_eq!(select(&[1000], FINAL), None);
    }

    #[test]
    fn a_window_does_not_span_a_block_in_flight() {
        let mut parts = candidates(&[100, 100, 100, 100]);
        parts[2].name = PartName::new(4, 4, 0);
        parts[3].name = PartName::new(5, 5, 0);
        let default = MergeOptions::default();
        assert_eq!(select_window(&parts, &[3], &default), Some(0..2));
        assert_eq!(select_window(&parts, &[3], &FINAL), Some(0..2));
        // A block outside the parts or a gap with nothing in flight stops nothing.
        assert_eq!(select_window(&parts, &[6], &default), Some(0..4));
        assert_eq!(select_window(&parts, &[], &default), Some(0..4));
        parts.truncate(3);
        assert_eq!(select_window(&parts, &[3], &default), Some(0..2));
    }

    fn keys_of(values: &[&[i64]]) -> Vec<SourceKeys> {
        values
            .iter()
            .map(|rows| SourceKeys {
                rows: rows.len() as u64,
                granules: rows
                    .chunks(GRANULE_ROWS)
                    .map(|granule| {
                        let granule: Vec<Value> =
                            granule.iter().map(|&v| Value::Int64(v)).collect();
                        vec![Vector::from_values(DataType::Int64, &granule).unwrap()]
                    })
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn merge_order_is_stable_and_crosses_granules() {
        assert_eq!(
            merge_order(&keys_of(&[&[1, 3, 3], &[2, 3], &[0, 3, 4]])),
            vec![2, 0, 1, 0, 0, 1, 2, 2]
        );
        let long: Vec<i64> = (0..3 * GRANULE_ROWS as i64).map(|v| v * 2).collect();
        let odd: Vec<i64> = long.iter().map(|v| v + 1).collect();
        let order = merge_order(&keys_of(&[&long, &odd]));
        assert_eq!(order.len(), 6 * GRANULE_ROWS);
        assert!(order.iter().enumerate().all(|(i, &s)| s as usize == i % 2));
    }

    #[test]
    fn without_key_sources_follow_one_another() {
        let keys = [
            SourceKeys {
                rows: 2,
                granules: Vec::new(),
            },
            SourceKeys {
                rows: 3,
                granules: Vec::new(),
            },
        ];
        assert_eq!(merge_order(&keys), vec![0, 0, 1, 1, 1]);
    }

    fn typed_table(db: &Path, name: &str, schema: Schema, key: &[usize]) -> Table {
        let def = TableDef {
            name: name.to_string(),
            schema,
            primary_key: key.to_vec(),
            ddl: format!("CREATE TABLE {name} (unused BIGINT)"),
        };
        Table::create(db, def).unwrap()
    }

    fn open_part(table: &Table, name: PartName) -> PartReader {
        let dir = table.dir().join(name.to_string());
        PartReader::open(&dir, table.schema(), table.primary_key()).unwrap()
    }

    fn rows_of(part: &PartReader) -> Chunk {
        let columns = (0..part.schema().len())
            .map(|c| part.read_column(c).unwrap())
            .collect();
        Chunk::new(columns).unwrap()
    }

    /// The directories of a table other than its parts.
    fn leftovers(table: &Table) -> Vec<String> {
        subdirectories(table.dir())
            .unwrap()
            .into_iter()
            .filter(|n| PartName::parse(n).is_none() && n != DETACHED_DIR)
            .collect()
    }

    /// The stats, sample, key index and rows of `merged` must be those of a part freshly
    /// written from `expected`.
    fn assert_part_holds(table: &Table, merged: &PartReader, expected: &Chunk, root: &Path) {
        let (schema, key) = (table.schema(), table.primary_key());
        assert_eq!(merged.rows(), expected.len as u64);
        for c in 0..schema.len() {
            let read = merged.read_column(c).unwrap();
            assert_vectors_equal(&read, expected.column(c), &format!("column {c}"));
        }
        let fresh = write_part(root, merged.name(), schema, key, expected, 5000);
        let fresh = PartReader::open(&fresh, schema, key).unwrap();
        let stats: &PartStats = merged.stats();
        assert_eq!(stats, fresh.stats());
        assert_eq!(merged.sample(), fresh.sample());
        assert_eq!(merged.index(), fresh.index());
        fs::remove_dir_all(fresh.dir()).unwrap();
    }

    #[test]
    fn random_parts_merge_into_their_sorted_union() {
        let db = TestDir::new("merge-random");
        let scratch = TestDir::new("merge-random-fresh");
        let schema = all_types_schema();
        // Nullable string, then nullable small integer: many NULLs and repeated keys.
        let key = [14, 2];
        for (t, sizes) in [
            &[1usize, 2][..],
            &[100, 100, 100, 1],
            &[8192, 8192],
            &[8191, 8193, 16_385, 1, 3000],
            &[20_000, 300, 20_000, 7, 9000, 8192, 1, 1, 12_000, 5],
        ]
        .iter()
        .enumerate()
        {
            let table = typed_table(db.path(), &format!("t{t}"), schema.clone(), &key);
            // The stable sort of the blocks in order is the one correct merge result.
            let mut union = Chunk::empty(&schema.data_types());
            for (i, &rows) in sizes.iter().enumerate() {
                let chunk = random_chunk(&schema, rows, (t * 100 + i) as u64);
                insert(&table, &chunk).unwrap();
                union
                    .extend(&sort_chunk(&chunk, &key).unwrap().unwrap_or(chunk))
                    .unwrap();
            }
            let expected = sort_chunk(&union, &key).unwrap().unwrap_or(union);

            let mut merger = Merger::new(table.clone(), MergeOptions::default());
            let step = merger.merge_once().unwrap();
            let MergeStep::Merged(merged) = step else {
                panic!("{sizes:?}: {step:?}");
            };
            assert_eq!(merged.rows, expected.len as u64);
            assert_eq!(merged.name, PartName::new(1, sizes.len() as u64, 1));
            assert_eq!(merged.sources.len(), sizes.len());
            assert_eq!(list_parts(table.dir()).unwrap(), vec![merged.name]);
            assert!(leftovers(&table).is_empty());
            assert!(
                !table
                    .dir()
                    .join(merged.name.to_string())
                    .join(OWNER_LOCK)
                    .exists()
            );
            let part = open_part(&table, merged.name);
            assert_part_holds(&table, &part, &expected, scratch.path());
        }
    }

    fn id_schema() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("label", DataType::Varchar, true),
        ])
    }

    fn id_batch(from: i64, rows: i64) -> Chunk {
        let rows: Vec<Vec<Value>> = (from..from + rows)
            .map(|id| {
                let label = match id % 4 {
                    0 => Value::Null,
                    n => Value::varchar("x".repeat(n as usize)),
                };
                vec![Value::Int64(id), label]
            })
            .collect();
        Chunk::from_rows(&id_schema().data_types(), &rows).unwrap()
    }

    #[test]
    fn without_key_the_merge_keeps_block_order() {
        let db = TestDir::new("merge-nokey");
        let table = typed_table(db.path(), "t", id_schema(), &[]);
        let mut expected = Chunk::empty(&id_schema().data_types());
        for n in 0..4 {
            let batch = id_batch(1000 - 100 * n, 6000);
            insert(&table, &batch).unwrap();
            expected.extend(&batch).unwrap();
        }
        let summary = Merger::new(table.clone(), MergeOptions::default())
            .run()
            .unwrap();
        assert_eq!(summary.merges, 1);
        assert_eq!(summary.active, 1);
        let merged = rows_of(&open_part(&table, PartName::new(1, 4, 1)));
        for c in 0..expected.num_columns() {
            assert_vectors_equal(merged.column(c), expected.column(c), &format!("column {c}"));
        }
    }

    #[test]
    fn final_repeats_until_one_part_and_levels_grow() {
        let db = TestDir::new("merge-final");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        for n in 0..25 {
            insert(&table, &id_batch(n * 10, 40)).unwrap();
        }
        let summary = Merger::new(table.clone(), FINAL).run().unwrap();
        // 25 → 16 → 7 → 3 → 1: windows of the small parts score highest until none is left.
        assert_eq!(summary.merges, 4);
        assert_eq!(summary.active, 1);
        let parts = list_parts(table.dir()).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!((parts[0].min_block, parts[0].max_block), (1, 25));
        assert_eq!(parts[0].level, 2);
        assert_eq!(open_part(&table, parts[0]).rows(), 1000);
        assert!(leftovers(&table).is_empty());
        // Nothing else to do.
        let again = Merger::new(table.clone(), FINAL).run().unwrap();
        assert_eq!(again.merges, 0);
    }

    #[test]
    fn without_final_merging_stops_when_no_window_qualifies() {
        let db = TestDir::new("merge-stop");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        insert(&table, &id_batch(0, 1000)).unwrap();
        insert(&table, &id_batch(0, 10)).unwrap();
        let mut merger = Merger::new(table.clone(), MergeOptions::default());
        assert_eq!(
            merger.merge_once().unwrap(),
            MergeStep::Idle {
                active: 2,
                in_flight: 0
            }
        );
        let summary = merger.run().unwrap();
        assert_eq!(summary.merges, 0);
        assert_eq!(summary.active, 2);
    }

    #[test]
    fn final_that_does_not_fit_in_memory_is_a_user_error() {
        let db = TestDir::new("merge-memory");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        for n in 0..2 {
            insert(&table, &id_batch(n, 10)).unwrap();
        }
        let options = MergeOptions {
            merge_memory_mb: 0,
            ..FINAL
        };
        let err = Merger::new(table.clone(), options).run().unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(list_parts(table.dir()).unwrap().len(), 2);
    }

    #[test]
    fn a_held_merge_lock_means_busy() {
        let db = TestDir::new("merge-busy");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        for n in 0..2 {
            insert(&table, &id_batch(n, 10)).unwrap();
        }
        let held = lock_exclusive(&table.lock_path(MERGE_LOCK)).unwrap();
        let mut merger = Merger::new(table.clone(), MergeOptions::default());
        assert_eq!(merger.try_merge_once().unwrap(), MergeStep::Busy);
        drop(held);
        assert!(matches!(
            merger.try_merge_once().unwrap(),
            MergeStep::Merged(_)
        ));
    }

    #[test]
    fn a_live_insert_stops_the_window_and_a_dead_one_does_not() {
        let db = TestDir::new("merge-in-flight");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        for n in 1..=5 {
            if n == 3 {
                continue;
            }
            let name = PartName::new(n, n, 0);
            write_part(
                table.dir(),
                name,
                table.schema(),
                &[0],
                &id_batch(0, 100),
                100,
            );
        }
        let tmp = table.dir().join("tmp_insert_3");
        fs::create_dir(&tmp).unwrap();
        let owner = lock_exclusive(&tmp.join(OWNER_LOCK)).unwrap();
        let mut merger = Merger::new(table.clone(), MergeOptions::default());
        let MergeStep::Merged(first) = merger.merge_once().unwrap() else {
            panic!("a window on each side of block 3");
        };
        assert_eq!(first.name, PartName::new(1, 2, 1));
        let MergeStep::Merged(second) = merger.merge_once().unwrap() else {
            panic!("the other side of block 3");
        };
        assert_eq!(second.name, PartName::new(4, 5, 1));
        assert_eq!(
            merger.merge_once().unwrap(),
            MergeStep::Idle {
                active: 2,
                in_flight: 1
            }
        );

        // The owner died: its block will never arrive.
        drop(owner);
        assert!(matches!(
            merger.merge_once().unwrap(),
            MergeStep::Merged(MergedPart {
                name: PartName {
                    min_block: 1,
                    max_block: 5,
                    level: 2
                },
                ..
            })
        ));
    }

    #[test]
    fn an_insert_running_during_final_is_waited_for() {
        let db = TestDir::new("merge-final-wait");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        for n in [1, 3] {
            let name = PartName::new(n, n, 0);
            write_part(
                table.dir(),
                name,
                table.schema(),
                &[0],
                &id_batch(0, 100),
                100,
            );
        }
        let tmp = table.dir().join("tmp_insert_2");
        fs::create_dir(&tmp).unwrap();
        let owner = lock_exclusive(&tmp.join(OWNER_LOCK)).unwrap();
        let summary = thread::scope(|scope| {
            let merging = scope.spawn(|| Merger::new(table.clone(), FINAL).run().unwrap());
            thread::sleep(Duration::from_millis(150));
            assert_eq!(list_parts(table.dir()).unwrap().len(), 2);
            drop(owner);
            merging.join().unwrap()
        });
        assert_eq!(summary.merges, 1);
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![PartName::new(1, 3, 1)]
        );
    }

    #[test]
    fn a_part_broken_on_open_is_detached_and_the_rest_merged() {
        let db = TestDir::new("merge-broken-open");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        for n in 0..3 {
            insert(&table, &id_batch(n * 100, 100)).unwrap();
        }
        let stats = table.dir().join("all_2_2_0").join(STATS_BIN);
        let mut bytes = fs::read(&stats).unwrap();
        bytes[3] ^= 0xff;
        fs::write(&stats, bytes).unwrap();

        let mut merger = Merger::new(table.clone(), MergeOptions::default());
        let summary = merger.run().unwrap();
        assert_eq!(summary.merges, 1);
        let detached = merger.take_detached();
        assert_eq!(detached.len(), 1);
        assert_eq!(detached[0].name, PartName::new(2, 2, 0));
        let dir = table.dir().join(DETACHED_DIR);
        assert!(dir.join("broken_all_2_2_0").is_dir());
        let merged = PartName::new(1, 3, 1);
        assert_eq!(list_parts(table.dir()).unwrap(), vec![merged]);
        assert_eq!(open_part(&table, merged).rows(), 200);
        assert!(merger.take_detached().is_empty());
    }

    #[test]
    fn a_corrupt_block_fails_the_merge_and_detaches_its_part() {
        let db = TestDir::new("merge-broken-block");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        for n in 0..3 {
            insert(&table, &id_batch(n * 100, 100)).unwrap();
        }
        let bin = table.dir().join("all_2_2_0").join("c1.bin");
        let mut bytes = fs::read(&bin).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        fs::write(&bin, bytes).unwrap();

        let mut merger = Merger::new(table.clone(), MergeOptions::default());
        let err = merger.run().unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(
            err.to_string().starts_with("corrupt part all_2_2_0: "),
            "{err}"
        );
        assert!(leftovers(&table).is_empty());
        assert!(
            table
                .dir()
                .join(DETACHED_DIR)
                .join("broken_all_2_2_0")
                .is_dir()
        );
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![PartName::new(1, 1, 0), PartName::new(3, 3, 0)]
        );
        // The next attempt merges what is left.
        assert_eq!(merger.run().unwrap().merges, 1);
        assert_eq!(open_part(&table, PartName::new(1, 3, 1)).rows(), 200);
    }

    #[test]
    fn what_a_dead_merge_left_is_cleaned_by_the_next_one() {
        let db = TestDir::new("merge-dead");
        let table = typed_table(db.path(), "t", id_schema(), &[0]);
        for n in 0..3 {
            insert(&table, &id_batch(n * 100, 100)).unwrap();
        }
        // Published but died before the cleanup: 1 and 2 are covered and still there.
        let mut covered = id_batch(0, 100);
        covered.extend(&id_batch(100, 100)).unwrap();
        write_part(
            table.dir(),
            PartName::new(1, 2, 1),
            table.schema(),
            &[0],
            &covered,
            200,
        );
        // Died while writing the merge that is coming now.
        let abandoned = table.dir().join("tmp_merge_all_1_3_2");
        fs::create_dir(&abandoned).unwrap();
        fs::write(abandoned.join(OWNER_LOCK), b"").unwrap();
        fs::write(abandoned.join("c0.bin"), b"partial").unwrap();

        let mut merger = Merger::new(table.clone(), MergeOptions::default());
        let MergeStep::Merged(merged) = merger.merge_once().unwrap() else {
            panic!("the two active parts merge");
        };
        assert_eq!(merged.name, PartName::new(1, 3, 2));
        assert_eq!(
            merged.sources,
            vec![PartName::new(1, 2, 1), PartName::new(3, 3, 0)]
        );
        assert_eq!(list_parts(table.dir()).unwrap(), vec![merged.name]);
        assert!(leftovers(&table).is_empty());
        assert_eq!(open_part(&table, merged.name).rows(), 300);
    }

    #[test]
    fn a_string_key_is_estimated_from_the_sample() {
        let db = TestDir::new("merge-key-bytes");
        let schema = Schema::new(vec![
            Field::new("s", DataType::Varchar, true),
            Field::new("n", DataType::Int32, false),
        ]);
        let table = typed_table(db.path(), "t", schema.clone(), &[1, 0]);
        let rows: Vec<Vec<Value>> = (0..100)
            .map(|i| {
                let s = if i % 2 == 0 {
                    Value::varchar("abcdef")
                } else {
                    Value::Null
                };
                vec![s, Value::Int32(i)]
            })
            .collect();
        let chunk = Chunk::from_rows(&schema.data_types(), &rows).unwrap();
        let name = insert(&table, &chunk).unwrap().unwrap();
        // Int32 (4) + mean length 3 + offset 4.
        assert_eq!(key_row_bytes(&open_part(&table, name)), 11);
    }

    /// Checks one snapshot: every id once and each part ordered by its key.
    fn check_snapshot(snapshot: &Snapshot, ids: i64) {
        let mut seen = Vec::with_capacity(ids as usize);
        for part in snapshot.parts() {
            let (a, b) = (part.read_column(0).unwrap(), part.read_column(1).unwrap());
            assert_eq!(part.read_column(2).unwrap().len(), a.len());
            for row in 0..a.len() {
                let Value::Int64(id) = a.get(row) else {
                    panic!("NULL id");
                };
                seen.push(id);
                if row > 0 {
                    let previous = (b.get(row - 1), a.get(row - 1));
                    assert!(previous <= (b.get(row), a.get(row)), "row {row}");
                }
            }
        }
        seen.sort_unstable();
        let expected: Vec<i64> = (0..ids).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_reader_never_sees_rows_too_many_or_too_few_while_merging() {
        let db = TestDir::new("merge-reader");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let mut rng = Rng::new(5);
        let (parts, part_rows) = (40i64, 1500i64);
        let mut ids: Vec<i64> = (0..parts * part_rows).collect();
        rng.shuffle(&mut ids);
        for batch in ids.chunks(part_rows as usize) {
            let rows: Vec<Vec<Value>> = batch
                .iter()
                .map(|&id| {
                    let label = match rng.below(5) {
                        0 => Value::Null,
                        n => Value::varchar("k".repeat(n as usize)),
                    };
                    vec![Value::Int64(id), label, Value::Float64(id as f64)]
                })
                .collect();
            let chunk = Chunk::from_rows(&table.schema().data_types(), &rows).unwrap();
            insert(&table, &chunk).unwrap();
        }
        let total = parts * part_rows;
        // Taken before anything is merged and read after everything is deleted.
        let early = Snapshot::take(&table, None).unwrap();

        let done = AtomicBool::new(false);
        let start = Barrier::new(2);
        let (summary, snapshots) = thread::scope(|scope| {
            let reader = scope.spawn(|| {
                start.wait();
                let mut snapshots = 0u32;
                let mut part_counts = Vec::new();
                loop {
                    // Read after the flag, so the last round sees the merge finished.
                    let finished = done.load(Ordering::SeqCst);
                    let snapshot = Snapshot::take(&table, None).unwrap();
                    assert!(snapshot.detached().is_empty());
                    assert_eq!(snapshot.rows(), total as u64);
                    check_snapshot(&snapshot, total);
                    part_counts.push(snapshot.parts().len());
                    snapshots += 1;
                    if finished {
                        break;
                    }
                }
                assert_eq!(part_counts.last(), Some(&1));
                snapshots
            });
            let merging = scope.spawn(|| {
                start.wait();
                let summary = Merger::new(table.clone(), FINAL).run();
                done.store(true, Ordering::SeqCst);
                summary.unwrap()
            });
            (merging.join().unwrap(), reader.join().unwrap())
        });
        assert!(summary.merges >= 4, "{summary:?}");
        assert!(snapshots >= 1);
        assert_eq!(list_parts(table.dir()).unwrap().len(), 1);
        assert!(leftovers(&table).is_empty());
        assert_eq!(early.parts().len(), parts as usize);
        check_snapshot(&early, total);
    }
}
