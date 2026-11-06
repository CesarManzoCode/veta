//! Pipelines (contract §12): a source of morsels, streaming operators and a sink.
//!
//! A morsel is up to 16 contiguous granules of a part, a Parquet row group or one chunk
//! already in memory. A [`Dispatcher`] hands out morsels with an atomic counter to the
//! workers, plain `std::thread::scope` threads, so morsels are claimed in order. Filters
//! narrow a selection vector instead of copying rows; projections evaluate over it; the
//! probe of a join ([`crate::join`]) turns one batch into any number of batches. Every
//! worker keeps its own sink state, which the operator combines after the threads end.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use veta_core::memory::Reservation;
use veta_core::predicate::ScanFilter;
use veta_core::{Chunk, DataType, Result, Vector, VectorData, VetaError};
use veta_parquet::ParquetFile;
use veta_sql::{BoundExpr, BoundExprKind};
use veta_storage::prune::prune_part;
use veta_storage::{PartReader, Snapshot, Table, detach_corrupt};

use crate::context::{ExecContext, add, new_reservation};
use crate::expr_eval::CompiledExpr;
use crate::join::JoinProbe;

/// Granules per morsel of a stored table (contract §12).
pub(crate) const MORSEL_GRANULES: usize = 16;
/// Rows per chunk of the operators that materialize their output.
pub(crate) const CHUNK_ROWS: usize = 8192;

/// Hands out morsel numbers `0, 1, …` once each, until the end or a stop.
#[derive(Debug)]
pub(crate) struct Dispatcher {
    next: AtomicUsize,
    total: usize,
    stopped: AtomicBool,
}

impl Dispatcher {
    pub fn new(total: usize) -> Dispatcher {
        Dispatcher {
            next: AtomicUsize::new(0),
            total,
            stopped: AtomicBool::new(false),
        }
    }

    pub fn next(&self) -> Option<usize> {
        if self.stopped.load(Ordering::Relaxed) {
            return None;
        }
        let morsel = self.next.fetch_add(1, Ordering::Relaxed);
        (morsel < self.total).then_some(morsel)
    }

    /// No morsel is handed out after this; those already claimed are finished.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

/// Runs `work` on `workers` threads (inline if only one) and returns every state, or the
/// first error in worker order. A panic in a worker becomes an internal error.
/// Keeps in `kept` the error a run ends with when several workers fail. A corrupt part wins:
/// it was detached already, so the exit code has to say 3 (§6.4). Any other corrupt part gets
/// its line on stderr here, since only one error reaches the end of the process.
pub fn keep_worker_error(kept: &mut Option<VetaError>, err: VetaError) {
    let corrupt = matches!(err, VetaError::Corrupt { .. });
    match kept {
        Some(VetaError::Corrupt { .. }) if corrupt => eprintln!("veta: {err}"),
        Some(VetaError::Corrupt { .. }) => {}
        Some(_) if !corrupt => {}
        _ => *kept = Some(err),
    }
}

fn join_workers<S: Send>(workers: usize, work: impl Fn() -> Result<S> + Sync) -> Result<Vec<S>> {
    if workers <= 1 {
        return Ok(vec![work()?]);
    }
    let work = &work;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers).map(|_| scope.spawn(move || work())).collect();
        let mut states = Vec::with_capacity(workers);
        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(state)) => states.push(state),
                Ok(Err(err)) => keep_worker_error(&mut first_error, err),
                Err(_) => keep_worker_error(
                    &mut first_error,
                    VetaError::internal("worker thread panicked"),
                ),
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(states),
        }
    })
}

/// Runs tasks `0..tasks` on up to `threads` workers; the results come back in task order.
pub(crate) fn parallel_tasks<R: Send>(
    threads: usize,
    tasks: usize,
    f: impl Fn(usize) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    let dispatcher = Dispatcher::new(tasks);
    let workers = threads.min(tasks).max(1);
    let done = join_workers(workers, || -> Result<Vec<(usize, R)>> {
        let mut out = Vec::new();
        while let Some(task) = dispatcher.next() {
            match f(task) {
                Ok(result) => out.push((task, result)),
                Err(err) => {
                    dispatcher.stop();
                    return Err(err);
                }
            }
        }
        Ok(out)
    })?;
    let mut all: Vec<(usize, R)> = done.into_iter().flatten().collect();
    all.sort_by_key(|entry| entry.0);
    Ok(all.into_iter().map(|entry| entry.1).collect())
}

/// Rows flowing through a pipeline: a chunk and, after a filter, the rows that remain.
#[derive(Debug)]
pub(crate) struct Batch {
    pub chunk: Chunk,
    pub selection: Option<Vec<u32>>,
}

impl Batch {
    pub fn rows(&self) -> usize {
        self.selection.as_ref().map_or(self.chunk.len, Vec::len)
    }

    /// The selected rows as a chunk of their own.
    pub fn into_chunk(self) -> Chunk {
        match &self.selection {
            Some(selection) => self.chunk.gather(selection),
            None => self.chunk,
        }
    }
}

/// An expression of a sink, with a plain column read without copying when no selection
/// applies.
#[derive(Debug)]
pub(crate) struct Evaluator {
    compiled: CompiledExpr,
    column: Option<usize>,
}

fn plain_column(expr: &BoundExpr) -> Option<usize> {
    match &expr.kind {
        BoundExprKind::Column { index, .. } | BoundExprKind::AggregateRef { index, .. } => {
            Some(*index)
        }
        _ => None,
    }
}

impl Evaluator {
    pub fn new(expr: &BoundExpr) -> Result<Evaluator> {
        Ok(Evaluator {
            compiled: CompiledExpr::new(expr)?,
            column: plain_column(expr),
        })
    }

    /// One value per selected row of `batch`.
    pub fn vector<'a>(&self, batch: &'a Batch) -> Result<Cow<'a, Vector>> {
        if batch.selection.is_none()
            && let Some(column) = self.column
        {
            return Ok(Cow::Borrowed(&batch.chunk.columns[column]));
        }
        Ok(Cow::Owned(
            self.compiled
                .eval(&batch.chunk, batch.selection.as_deref())?,
        ))
    }
}

/// An output column of a projection.
#[derive(Debug)]
pub(crate) enum ProjectItem {
    /// A column no other output reads: moved when no selection applies.
    Move(usize),
    Eval(CompiledExpr),
}

/// A streaming operator.
#[derive(Debug)]
pub(crate) enum StreamOp {
    Filter(CompiledExpr),
    Project(Vec<ProjectItem>),
    /// The probe of a join whose build side is done.
    Join(Box<JoinProbe>),
    /// Counts the rows that reach it (EXPLAIN ANALYZE, §11.6).
    Count(Arc<AtomicU64>),
}

impl StreamOp {
    pub fn filter(predicate: &BoundExpr) -> Result<StreamOp> {
        Ok(StreamOp::Filter(CompiledExpr::new(predicate)?))
    }

    pub fn project(exprs: &[BoundExpr]) -> Result<StreamOp> {
        let mut uses: HashMap<usize, usize> = HashMap::new();
        for expr in exprs {
            for column in expr.columns() {
                *uses.entry(column).or_insert(0) += 1;
            }
        }
        let mut items = Vec::with_capacity(exprs.len());
        for expr in exprs {
            items.push(match plain_column(expr) {
                Some(column) if uses.get(&column) == Some(&1) => ProjectItem::Move(column),
                _ => ProjectItem::Eval(CompiledExpr::new(expr)?),
            });
        }
        Ok(StreamOp::Project(items))
    }

    fn apply(&self, batch: &mut Batch) -> Result<()> {
        match self {
            StreamOp::Filter(predicate) => {
                let selection =
                    predicate.eval_predicate(&batch.chunk, batch.selection.as_deref())?;
                batch.selection = Some(selection);
            }
            StreamOp::Project(items) => {
                let rows = batch.rows();
                let selection = batch.selection.as_deref();
                let mut columns = Vec::with_capacity(items.len());
                for item in items {
                    columns.push(match item {
                        ProjectItem::Eval(expr) => expr.eval(&batch.chunk, selection)?,
                        ProjectItem::Move(column) => match selection {
                            Some(selection) => batch.chunk.columns[*column].gather(selection),
                            None => std::mem::replace(
                                &mut batch.chunk.columns[*column],
                                Vector::with_capacity(DataType::Boolean, 0),
                            ),
                        },
                    });
                }
                batch.chunk = Chunk::with_len(columns, rows)?;
                batch.selection = None;
            }
            StreamOp::Count(counter) => add(counter, batch.rows() as u64),
            StreamOp::Join(_) => {
                return Err(VetaError::internal("a join probe does not apply in place"));
            }
        }
        Ok(())
    }
}

/// Granules `start..end` of one part.
#[derive(Debug, Clone, Copy)]
struct TableMorsel {
    part: usize,
    start: usize,
    end: usize,
}

/// The snapshot of a stored table being scanned.
#[derive(Debug)]
pub(crate) struct TableSource {
    table: Table,
    parts: Vec<PartReader>,
    columns: Vec<usize>,
    morsels: Vec<TableMorsel>,
    /// Whether a part was already counted in `parts_read`.
    touched: Vec<AtomicBool>,
}

impl TableSource {
    fn read_granule(&self, part: &PartReader, granule: usize) -> Result<Chunk> {
        let columns = self
            .columns
            .iter()
            .map(|&c| part.read_granule(c, granule))
            .collect::<Result<Vec<Vector>>>()?;
        Chunk::with_len(columns, part.granule_rows(granule))
    }
}

/// Where the rows of a pipeline come from.
#[derive(Debug)]
pub(crate) enum Source {
    Table(TableSource),
    /// A Parquet file; morsel `m` is row group `row_groups[m]`.
    Parquet {
        file: ParquetFile,
        columns: Vec<usize>,
        row_groups: Vec<usize>,
    },
    /// Chunks in memory, each one a morsel and taken only once.
    Chunks(Vec<Mutex<Option<Chunk>>>),
}

impl Source {
    /// Takes the snapshot of `table_name` keeping `columns` open (contract §6.3) and
    /// reports on stderr the parts it had to detach (§6.4). With pruning on, the morsels
    /// cover only the granules `filters` may match (§11.4): runs of contiguous selected
    /// granules, cut every [`MORSEL_GRANULES`].
    pub fn table(
        ctx: &ExecContext<'_>,
        table_name: &str,
        columns: &[usize],
        filters: &[ScanFilter],
    ) -> Result<Source> {
        let table = ctx.db.open_table(table_name)?;
        let snapshot = Snapshot::take(&table, Some(columns))?;
        for detached in snapshot.detached() {
            eprintln!("{}", detached.message());
        }
        let parts = snapshot.into_parts();
        let filters: &[ScanFilter] = if ctx.prune { filters } else { &[] };
        let mut morsels = Vec::new();
        for (p, part) in parts.iter().enumerate() {
            add(&ctx.stats.granules_total, part.granules() as u64);
            let selected = prune_part(part, filters);
            let mut i = 0;
            while i < selected.len() {
                let start = selected[i];
                let mut end = start + 1;
                i += 1;
                while i < selected.len() && selected[i] == end && end - start < MORSEL_GRANULES {
                    end += 1;
                    i += 1;
                }
                morsels.push(TableMorsel {
                    part: p,
                    start,
                    end,
                });
            }
        }
        add(&ctx.stats.parts_total, parts.len() as u64);
        let touched = parts.iter().map(|_| AtomicBool::new(false)).collect();
        Ok(Source::Table(TableSource {
            table,
            parts,
            columns: columns.to_vec(),
            morsels,
            touched,
        }))
    }

    /// Opens the Parquet file at `path`; each row group is a morsel. With pruning on, a
    /// row group is skipped when the statistics of a filtered column exclude the filter
    /// (§11.4).
    pub fn parquet(
        ctx: &ExecContext<'_>,
        path: &str,
        columns: &[usize],
        filters: &[ScanFilter],
    ) -> Result<Source> {
        let file = ParquetFile::open(path)?;
        let total = file.num_row_groups();
        add(&ctx.stats.row_groups_total, total as u64);
        let filters: &[ScanFilter] = if ctx.prune { filters } else { &[] };
        let row_groups = (0..total)
            .filter(|&rg| row_group_may_match(&file, rg, filters))
            .collect();
        Ok(Source::Parquet {
            file,
            columns: columns.to_vec(),
            row_groups,
        })
    }

    pub fn chunks(chunks: Vec<Chunk>) -> Source {
        Source::Chunks(
            chunks
                .into_iter()
                .filter(|c| c.len > 0)
                .map(|c| Mutex::new(Some(c)))
                .collect(),
        )
    }

    /// The one row without columns of a `SELECT` without `FROM`.
    pub fn single_row() -> Result<Source> {
        Ok(Source::chunks(vec![Chunk::with_len(Vec::new(), 1)?]))
    }

    fn morsels(&self) -> usize {
        match self {
            Source::Table(table) => table.morsels.len(),
            Source::Parquet { row_groups, .. } => row_groups.len(),
            Source::Chunks(chunks) => chunks.len(),
        }
    }
}

/// Whether row group `rg` may hold a row satisfying every filter: a column whose chunk
/// statistics are unknown never excludes it.
fn row_group_may_match(file: &ParquetFile, rg: usize, filters: &[ScanFilter]) -> bool {
    filters
        .iter()
        .all(|f| match file.row_group_stats(rg, f.column()) {
            Some((min, max, _nulls)) => f.may_match(&min, &max),
            None => true,
        })
}

/// Output of a pipeline break: chunks in result order and the memory they hold.
#[derive(Debug)]
pub(crate) struct Materialized {
    pub chunks: Vec<Chunk>,
    pub hold: Vec<Reservation<'static>>,
}

/// A source and the streaming operators over it.
#[derive(Debug)]
pub(crate) struct Pipeline {
    source: Source,
    ops: Vec<StreamOp>,
    dispatcher: Dispatcher,
    _hold: Vec<Reservation<'static>>,
}

impl Pipeline {
    pub fn new(source: Source) -> Pipeline {
        let dispatcher = Dispatcher::new(source.morsels());
        Pipeline {
            source,
            ops: Vec::new(),
            dispatcher,
            _hold: Vec::new(),
        }
    }

    /// A pipeline over the output of a pipeline break, which keeps its memory counted
    /// until the pipeline is dropped.
    pub fn over(materialized: Materialized) -> Pipeline {
        let mut pipeline = Pipeline::new(Source::chunks(materialized.chunks));
        pipeline._hold = materialized.hold;
        pipeline
    }

    pub fn push_op(&mut self, op: StreamOp) {
        self.ops.push(op);
    }

    /// Stops handing out morsels (the early stop of a `LIMIT`).
    pub fn stop(&self) {
        self.dispatcher.stop();
    }

    fn push_chunk(&self, chunk: Chunk, sink: &mut dyn FnMut(Batch) -> Result<()>) -> Result<()> {
        let batch = Batch {
            chunk,
            selection: None,
        };
        self.push_batch(0, batch, sink)
    }

    /// Runs `batch` through the operators from `first` on. Each batch a join probe emits
    /// goes on through the operators after it.
    fn push_batch(
        &self,
        first: usize,
        mut batch: Batch,
        sink: &mut dyn FnMut(Batch) -> Result<()>,
    ) -> Result<()> {
        for (i, op) in self.ops.iter().enumerate().skip(first) {
            if let StreamOp::Join(join) = op {
                return join.probe(batch, &mut |out| self.push_batch(i + 1, out, &mut *sink));
            }
            op.apply(&mut batch)?;
            if batch.rows() == 0 {
                return Ok(());
            }
        }
        sink(batch)
    }

    /// Runs morsel `m` through the operators; `sink` gets every non-empty batch, in order.
    fn run_morsel(
        &self,
        m: usize,
        ctx: &ExecContext<'_>,
        sink: &mut dyn FnMut(Batch) -> Result<()>,
    ) -> Result<()> {
        match &self.source {
            Source::Table(source) => {
                let morsel = source.morsels[m];
                let part = &source.parts[morsel.part];
                if !source.touched[morsel.part].swap(true, Ordering::Relaxed) {
                    add(&ctx.stats.parts_read, 1);
                }
                add(&ctx.stats.granules_read, (morsel.end - morsel.start) as u64);
                for granule in morsel.start..morsel.end {
                    let chunk = match source.read_granule(part, granule) {
                        Ok(chunk) => chunk,
                        Err(err) => {
                            // §6.4: the part is detached before the process exits. The
                            // query fails with `err` whatever the detaching does.
                            if let Err(failure) = detach_corrupt(source.table.dir(), &err) {
                                eprintln!("veta: {failure}");
                            }
                            return Err(err);
                        }
                    };
                    add(&ctx.stats.rows_read, chunk.len as u64);
                    self.push_chunk(chunk, sink)?;
                }
                Ok(())
            }
            Source::Parquet {
                file,
                columns,
                row_groups,
            } => {
                add(&ctx.stats.row_groups_read, 1);
                for chunk in file.scan(vec![row_groups[m]], columns.clone())? {
                    let chunk = chunk?;
                    add(&ctx.stats.rows_read, chunk.len as u64);
                    self.push_chunk(chunk, sink)?;
                }
                Ok(())
            }
            Source::Chunks(chunks) => {
                let chunk = chunks[m]
                    .lock()
                    .map_err(|_| VetaError::internal("chunk lock poisoned"))?
                    .take();
                match chunk {
                    Some(chunk) => self.push_chunk(chunk, sink),
                    None => Ok(()),
                }
            }
        }
    }

    /// Runs the pipeline on the workers: each one starts with `init()` and gets `consume`d
    /// with its morsel number and every batch. Returns the state of each worker; there is
    /// always at least one, even without morsels.
    pub fn run_workers<S: Send>(
        &self,
        ctx: &ExecContext<'_>,
        init: impl Fn() -> Result<S> + Sync,
        consume: impl Fn(&mut S, usize, Batch) -> Result<()> + Sync,
    ) -> Result<Vec<S>> {
        let workers = ctx.threads.min(self.source.morsels()).max(1);
        join_workers(workers, || {
            let mut state = init()?;
            while let Some(m) = self.dispatcher.next() {
                let result = self.run_morsel(m, ctx, &mut |batch| consume(&mut state, m, batch));
                if let Err(err) = result {
                    self.dispatcher.stop();
                    return Err(err);
                }
            }
            Ok(state)
        })
    }
}

/// Bytes held by a vector.
fn vector_bytes(vector: &Vector) -> u64 {
    let data = match &vector.data {
        VectorData::Varchar { offsets, bytes } => offsets.len() * 4 + bytes.len(),
        data => data.len() * data.data_type().fixed_width().unwrap_or(0),
    };
    let validity = vector.validity.as_ref().map_or(0, |b| b.len() / 8 + 1);
    (data + validity) as u64
}

/// Bytes held by a chunk, for the memory tracker (contract §12).
pub(crate) fn chunk_bytes(chunk: &Chunk) -> u64 {
    chunk.columns.iter().map(vector_bytes).sum()
}

/// Sink that keeps the rows in morsel order. With `needed`, it stops handing out morsels
/// as soon as that many rows were seen: every morsel claimed before the stop still runs to
/// its end and morsels are claimed in order, so the first `needed` rows of the result do
/// not depend on the number of threads (contract §12, the early stop of `LIMIT`).
pub(crate) fn collect(
    pipeline: Pipeline,
    ctx: &ExecContext<'_>,
    needed: Option<u64>,
) -> Result<Materialized> {
    struct Local {
        chunks: Vec<(usize, usize, Chunk)>,
        mem: Reservation<'static>,
    }
    let seen = AtomicU64::new(0);
    let locals = pipeline.run_workers(
        ctx,
        || {
            Ok(Local {
                chunks: Vec::new(),
                mem: new_reservation()?,
            })
        },
        |local, m, batch| {
            let chunk = batch.into_chunk();
            local.mem.grow(chunk_bytes(&chunk))?;
            let rows = chunk.len as u64;
            let order = local.chunks.len();
            local.chunks.push((m, order, chunk));
            if let Some(needed) = needed
                && seen.fetch_add(rows, Ordering::Relaxed) + rows >= needed
            {
                pipeline.stop();
            }
            Ok(())
        },
    )?;
    let mut hold = Vec::with_capacity(locals.len());
    let mut all = Vec::new();
    for local in locals {
        all.extend(local.chunks);
        hold.push(local.mem);
    }
    all.sort_unstable_by_key(|entry| (entry.0, entry.1));
    Ok(Materialized {
        chunks: all.into_iter().map(|entry| entry.2).collect(),
        hold,
    })
}

/// Sink that hands the rows to `sink` in morsel order as they are ready, without keeping
/// them: a worker waits before starting a morsel too far ahead of the next one to deliver,
/// so at most two morsels per worker are in memory. The morsel to deliver is always running
/// or done, and a failure or a panic wakes everyone, so no worker waits forever. `sink` runs
/// under the lock, one chunk at a time. An error of `sink` ends the run with that error.
pub(crate) fn stream(
    pipeline: Pipeline,
    ctx: &ExecContext<'_>,
    sink: &mut (dyn FnMut(Chunk) -> Result<()> + Send),
) -> Result<()> {
    struct Delivery<'s> {
        next: usize,
        ready: BTreeMap<usize, Vec<Chunk>>,
        failed: bool,
        sink: &'s mut (dyn FnMut(Chunk) -> Result<()> + Send),
    }
    /// Marks the run as failed if its worker panics.
    struct PanicGuard<'g, 's> {
        delivery: &'g Mutex<Delivery<'s>>,
        advanced: &'g Condvar,
        dispatcher: &'g Dispatcher,
    }
    impl Drop for PanicGuard<'_, '_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                self.dispatcher.stop();
                let mut delivery = self.delivery.lock().unwrap_or_else(PoisonError::into_inner);
                delivery.failed = true;
                self.advanced.notify_all();
            }
        }
    }
    fn poisoned<T>(_: PoisonError<T>) -> VetaError {
        VetaError::internal("delivery lock poisoned")
    }
    let workers = ctx.threads.min(pipeline.source.morsels()).max(1);
    let window = workers * 2;
    let delivery = Mutex::new(Delivery {
        next: 0,
        ready: BTreeMap::new(),
        failed: false,
        sink,
    });
    let advanced = Condvar::new();
    let dispatcher = &pipeline.dispatcher;
    let fail = |err: VetaError| -> VetaError {
        dispatcher.stop();
        delivery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failed = true;
        advanced.notify_all();
        err
    };
    join_workers(workers, || -> Result<()> {
        let _guard = PanicGuard {
            delivery: &delivery,
            advanced: &advanced,
            dispatcher,
        };
        while let Some(m) = dispatcher.next() {
            {
                let mut state = delivery.lock().map_err(poisoned)?;
                while !state.failed && m >= state.next + window {
                    state = advanced.wait(state).map_err(poisoned)?;
                }
                if state.failed {
                    return Ok(());
                }
            }
            let mut chunks = Vec::new();
            pipeline
                .run_morsel(m, ctx, &mut |batch| {
                    chunks.push(batch.into_chunk());
                    Ok(())
                })
                .map_err(&fail)?;
            let mut state = delivery.lock().map_err(poisoned)?;
            state.ready.insert(m, chunks);
            loop {
                let next = state.next;
                let Some(ready) = state.ready.remove(&next) else {
                    break;
                };
                for chunk in ready {
                    if let Err(err) = (state.sink)(chunk) {
                        drop(state);
                        return Err(fail(err));
                    }
                }
                state.next += 1;
            }
            advanced.notify_all();
        }
        Ok(())
    })?;
    Ok(())
}

/// Rows `offset..offset + limit` of `chunks`.
pub(crate) fn slice_chunks(chunks: Vec<Chunk>, offset: u64, limit: Option<u64>) -> Vec<Chunk> {
    let mut skip = offset;
    let mut left = limit.unwrap_or(u64::MAX);
    let mut out = Vec::new();
    for chunk in chunks {
        if left == 0 {
            break;
        }
        let len = chunk.len as u64;
        if skip >= len {
            skip -= len;
            continue;
        }
        let take = (len - skip).min(left);
        if take == len {
            out.push(chunk);
        } else {
            out.push(chunk.slice(skip as usize, take as usize));
        }
        skip = 0;
        left -= take;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecStats;

    use crate::database::Database;
    use std::collections::HashSet;
    use veta_core::Value;

    fn int_chunk(values: &[i64]) -> Chunk {
        Chunk::new(vec![Vector::from_data(VectorData::Int64(values.to_vec()))]).unwrap()
    }

    fn first_column(chunks: &[Chunk]) -> Vec<Value> {
        chunks
            .iter()
            .flat_map(|c| (0..c.len).map(move |i| c.column(0).get(i)))
            .collect()
    }

    #[test]
    fn dispatcher_hands_out_each_morsel_once_across_threads() {
        let dispatcher = Dispatcher::new(1000);
        let seen = Mutex::new(HashSet::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    while let Some(m) = dispatcher.next() {
                        assert!(seen.lock().unwrap().insert(m));
                    }
                });
            }
        });
        assert_eq!(seen.lock().unwrap().len(), 1000);
        assert_eq!(dispatcher.next(), None);

        let stopped = Dispatcher::new(10);
        assert_eq!(stopped.next(), Some(0));
        stopped.stop();
        assert_eq!(stopped.next(), None);
    }

    #[test]
    fn parallel_tasks_keep_order_and_report_errors() {
        for threads in [1, 8] {
            let out = parallel_tasks(threads, 100, |i| Ok(i * 2)).unwrap();
            assert_eq!(out, (0..100).map(|i| i * 2).collect::<Vec<_>>());
            let err = parallel_tasks(threads, 100, |i| {
                if i == 7 {
                    Err(VetaError::user("seven"))
                } else {
                    Ok(i)
                }
            })
            .unwrap_err();
            assert_eq!(err.to_string(), "seven");
            assert!(parallel_tasks(threads, 0, Ok).unwrap().is_empty());
        }
    }

    #[test]
    fn slicing_crosses_chunk_borders() {
        let chunks = || {
            vec![
                int_chunk(&[0, 1, 2]),
                int_chunk(&[3, 4]),
                int_chunk(&[5, 6]),
            ]
        };
        let values = |offset, limit| first_column(&slice_chunks(chunks(), offset, limit));
        assert_eq!(values(0, None).len(), 7);
        assert_eq!(values(2, Some(2)), vec![Value::Int64(2), Value::Int64(3)]);
        assert_eq!(values(3, Some(10)).len(), 4);
        assert!(values(7, Some(1)).is_empty());
        assert!(values(0, Some(0)).is_empty());
    }

    #[test]
    fn collect_keeps_morsel_order_with_many_threads() {
        let db = Database::open("unused");
        let stats = ExecStats::default();
        let ctx = ExecContext {
            db: &db,
            threads: 8,
            stats: &stats,
            prune: true,
            profile: None,
        };
        let chunks: Vec<Chunk> = (0..200).map(|i| int_chunk(&[i, i + 1000])).collect();
        let out = collect(Pipeline::new(Source::chunks(chunks)), &ctx, None).unwrap();
        let values = first_column(&out.chunks);
        let expected: Vec<Value> = (0..200)
            .flat_map(|i| [Value::Int64(i), Value::Int64(i + 1000)])
            .collect();
        assert_eq!(values, expected);
    }

    #[test]
    fn collect_with_a_limit_returns_the_same_prefix_for_any_thread_count() {
        let db = Database::open("unused");
        let stats = ExecStats::default();
        for threads in [1, 8] {
            let ctx = ExecContext {
                db: &db,
                threads,
                stats: &stats,
                prune: true,
                profile: None,
            };
            let chunks: Vec<Chunk> = (0..500).map(|i| int_chunk(&[i])).collect();
            let out = collect(Pipeline::new(Source::chunks(chunks)), &ctx, Some(5)).unwrap();
            let values = first_column(&slice_chunks(out.chunks, 0, Some(5)));
            assert_eq!(values, (0..5).map(Value::Int64).collect::<Vec<_>>());
        }
    }

    #[test]
    fn stream_delivers_in_morsel_order_with_many_threads() {
        let db = Database::open("unused");
        let stats = ExecStats::default();
        for threads in [1, 2, 8] {
            let ctx = ExecContext {
                db: &db,
                threads,
                stats: &stats,
                prune: true,
                profile: None,
            };
            let chunks: Vec<Chunk> = (0..500).map(|i| int_chunk(&[i, i + 1000])).collect();
            let mut seen = Vec::new();
            stream(Pipeline::new(Source::chunks(chunks)), &ctx, &mut |chunk| {
                seen.push(chunk);
                Ok(())
            })
            .unwrap();
            let expected: Vec<Value> = (0..500)
                .flat_map(|i| [Value::Int64(i), Value::Int64(i + 1000)])
                .collect();
            assert_eq!(first_column(&seen), expected);
        }
    }

    #[test]
    fn stream_with_morsels_that_emit_nothing_goes_on() {
        use veta_sql::expr::BinaryOp;
        let db = Database::open("unused");
        let stats = ExecStats::default();
        let ctx = ExecContext {
            db: &db,
            threads: 4,
            stats: &stats,
            prune: true,
            profile: None,
        };
        let predicate = BoundExpr::new(
            BoundExprKind::Binary {
                op: BinaryOp::Gt,
                left: Box::new(BoundExpr::column(0, "x", DataType::Int64, false)),
                right: Box::new(BoundExpr::literal(Value::Int64(195))),
            },
            DataType::Boolean,
            false,
        );
        let chunks: Vec<Chunk> = (0..200).map(|i| int_chunk(&[i])).collect();
        let mut pipeline = Pipeline::new(Source::chunks(chunks));
        pipeline.push_op(StreamOp::filter(&predicate).unwrap());
        let mut seen = Vec::new();
        stream(pipeline, &ctx, &mut |chunk| {
            seen.push(chunk);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            first_column(&seen),
            (196..200).map(Value::Int64).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stream_ends_with_the_error_of_its_sink() {
        let db = Database::open("unused");
        let stats = ExecStats::default();
        for threads in [1, 8] {
            let ctx = ExecContext {
                db: &db,
                threads,
                stats: &stats,
                prune: true,
                profile: None,
            };
            let chunks: Vec<Chunk> = (0..500).map(|i| int_chunk(&[i])).collect();
            let mut seen = 0;
            let err = stream(Pipeline::new(Source::chunks(chunks)), &ctx, &mut |_| {
                seen += 1;
                if seen == 10 {
                    Err(VetaError::user("full"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(err.to_string(), "full");
            assert_eq!(seen, 10);
        }
    }

    #[test]
    fn filter_and_project_work_over_a_selection() {
        use veta_sql::expr::BinaryOp;
        let db = Database::open("unused");
        let stats = ExecStats::default();
        let ctx = ExecContext {
            db: &db,
            threads: 2,
            stats: &stats,
            prune: true,
            profile: None,
        };
        let column = BoundExpr::column(0, "x", DataType::Int64, false);
        let predicate = BoundExpr::new(
            BoundExprKind::Binary {
                op: BinaryOp::Gt,
                left: Box::new(column.clone()),
                right: Box::new(BoundExpr::literal(Value::Int64(2))),
            },
            DataType::Boolean,
            false,
        );
        let mut pipeline = Pipeline::new(Source::chunks(vec![
            int_chunk(&[1, 2, 3]),
            int_chunk(&[1, 1]),
            int_chunk(&[4]),
        ]));
        pipeline.push_op(StreamOp::filter(&predicate).unwrap());
        pipeline.push_op(StreamOp::project(&[column]).unwrap());
        let out = collect(pipeline, &ctx, None).unwrap();
        assert_eq!(
            first_column(&out.chunks),
            vec![Value::Int64(3), Value::Int64(4)]
        );
    }

    #[test]
    fn a_corrupt_part_decides_how_a_run_ends() {
        let corrupt = |part: &str| VetaError::corrupt(part, "crc");
        let mut kept = None;
        keep_worker_error(&mut kept, VetaError::user("overflow"));
        keep_worker_error(&mut kept, VetaError::Io("disk".into()));
        assert_eq!(kept, Some(VetaError::user("overflow")));
        keep_worker_error(&mut kept, corrupt("all_1_1_0"));
        assert_eq!(kept, Some(corrupt("all_1_1_0")));
        keep_worker_error(&mut kept, VetaError::user("overflow"));
        keep_worker_error(&mut kept, corrupt("all_2_2_0"));
        assert_eq!(kept, Some(corrupt("all_1_1_0")));
        assert_eq!(kept.map(|err| err.exit_code()), Some(3));
    }
}
