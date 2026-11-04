//! Order and top-N (contract §12): sorted runs per thread and a k-way merge, and a bounded
//! heap per thread merged at the end.
//!
//! Sort keys are normalized to bytes whose `memcmp` order is the requested order, so the
//! comparison has no per-type dispatch. Each key column contributes a marker byte (1 for a
//! value; NULL sorts by the marker, 0 for `NULLS FIRST` and 2 for `NULLS LAST`, and has no
//! value bytes) and the value: integers big-endian with the sign bit flipped, floats mapped
//! to the order of `cmp_f64` (`-0.0 = 0.0`, every NaN equal and above `+inf`), strings with
//! `0x00` escaped as `0x00 0xFF` and terminated with `0x00 0x00`, which keeps prefixes first.
//! `DESC` inverts the value bytes, not the marker. The encoding is prefix-free, so comparing
//! whole keys compares column by column.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use veta_core::memory::Reservation;
use veta_core::{Bitmap, Chunk, DataType, Result, Value, Vector, VectorData, VetaError};
use veta_sql::SortKey;

use crate::context::{ExecContext, new_reservation};
use crate::group_keys::KeyBuf;
use crate::pipeline::{
    Batch, CHUNK_ROWS, Evaluator, Materialized, Pipeline, chunk_bytes, parallel_tasks,
};

/// A sorted run stops growing at this many bytes, well below the 4 GiB of a string vector.
const RUN_BYTES: u64 = 1 << 30;

#[derive(Debug)]
pub(crate) struct CompiledKey {
    eval: Evaluator,
    desc: bool,
    nulls_first: bool,
}

pub(crate) fn compile_keys(keys: &[SortKey]) -> Result<Vec<CompiledKey>> {
    keys.iter()
        .map(|key| -> Result<CompiledKey> {
            Ok(CompiledKey {
                eval: Evaluator::new(&key.expr)?,
                desc: key.desc,
                nulls_first: key.nulls_first,
            })
        })
        .collect()
}

/// `f64` mapped to `u64` with the order of `cmp_f64`.
#[inline]
fn float_order(f: f64) -> u64 {
    if f.is_nan() {
        return u64::MAX;
    }
    let bits = if f == 0.0 { 0 } else { f.to_bits() };
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    }
}

fn write_fixed<T: Copy, const N: usize>(
    values: &[T],
    v: &Vector,
    key: &CompiledKey,
    buf: &mut KeyBuf,
    f: impl Fn(T) -> [u8; N],
) {
    let null_marker = if key.nulls_first { 0 } else { 2 };
    for (i, &value) in values.iter().enumerate() {
        let cursor = &mut buf.cursors[i];
        if v.is_valid(i) {
            buf.bytes[*cursor] = 1;
            *cursor += 1;
            let mut bytes = f(value);
            if key.desc {
                for b in &mut bytes {
                    *b = !*b;
                }
            }
            buf.bytes[*cursor..*cursor + N].copy_from_slice(&bytes);
            *cursor += N;
        } else {
            buf.bytes[*cursor] = null_marker;
            *cursor += 1;
        }
    }
}

fn write_strings(offsets: &[u32], bytes: &[u8], v: &Vector, key: &CompiledKey, buf: &mut KeyBuf) {
    let null_marker = if key.nulls_first { 0 } else { 2 };
    let invert: u8 = if key.desc { 0xFF } else { 0 };
    for i in 0..v.len() {
        let mut cursor = buf.cursors[i];
        if v.is_valid(i) {
            buf.bytes[cursor] = 1;
            cursor += 1;
            for &b in &bytes[offsets[i] as usize..offsets[i + 1] as usize] {
                if b == 0 {
                    buf.bytes[cursor] = invert;
                    buf.bytes[cursor + 1] = 0xFF ^ invert;
                    cursor += 2;
                } else {
                    buf.bytes[cursor] = b ^ invert;
                    cursor += 1;
                }
            }
            buf.bytes[cursor] = invert;
            buf.bytes[cursor + 1] = invert;
            cursor += 2;
        } else {
            buf.bytes[cursor] = null_marker;
            cursor += 1;
        }
        buf.cursors[i] = cursor;
    }
}

/// Encodes the sort keys of every selected row of `batch` into `buf`.
pub(crate) fn encode_keys(keys: &[CompiledKey], batch: &Batch, buf: &mut KeyBuf) -> Result<()> {
    let n = batch.rows();
    let vectors = keys
        .iter()
        .map(|key| key.eval.vector(batch))
        .collect::<Result<Vec<_>>>()?;
    buf.begin(n);
    for v in &vectors {
        match &v.data {
            VectorData::Varchar { offsets, bytes } => {
                for i in 0..n {
                    let size = if v.is_valid(i) {
                        let value = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
                        value.len() + value.iter().filter(|&&b| b == 0).count() + 3
                    } else {
                        1
                    };
                    buf.add_size(i, size);
                }
            }
            data => {
                let width = data.data_type().fixed_width().unwrap_or(0);
                for i in 0..n {
                    buf.add_size(i, if v.is_valid(i) { 1 + width } else { 1 });
                }
            }
        }
    }
    buf.allocate();
    for (key, vector) in keys.iter().zip(&vectors) {
        let v: &Vector = vector;
        match &v.data {
            VectorData::Boolean(x) => write_fixed(x, v, key, buf, |a: bool| [a as u8]),
            VectorData::Int16(x) => write_fixed(x, v, key, buf, |a: i16| {
                ((a as u16) ^ (1 << 15)).to_be_bytes()
            }),
            VectorData::Int32(x) | VectorData::Date(x) => write_fixed(x, v, key, buf, |a: i32| {
                ((a as u32) ^ (1 << 31)).to_be_bytes()
            }),
            VectorData::Int64(x) | VectorData::Timestamp(x) => {
                write_fixed(x, v, key, buf, |a: i64| {
                    ((a as u64) ^ (1 << 63)).to_be_bytes()
                })
            }
            VectorData::Decimal64 { values, .. } => write_fixed(values, v, key, buf, |a: i64| {
                ((a as u64) ^ (1 << 63)).to_be_bytes()
            }),
            VectorData::Int128(x) => write_fixed(x, v, key, buf, |a: i128| {
                ((a as u128) ^ (1 << 127)).to_be_bytes()
            }),
            VectorData::Decimal128 { values, .. } => write_fixed(values, v, key, buf, |a: i128| {
                ((a as u128) ^ (1 << 127)).to_be_bytes()
            }),
            VectorData::Float64(x) => {
                write_fixed(x, v, key, buf, |a: f64| float_order(a).to_be_bytes())
            }
            VectorData::Varchar { offsets, bytes } => write_strings(offsets, bytes, v, key, buf),
        }
    }
    Ok(())
}

/// Rows and encoded keys of one run; `perm` lists the rows in key order once sorted.
#[derive(Debug, Default)]
struct Run {
    chunk: Option<Chunk>,
    key_bytes: Vec<u8>,
    key_ends: Vec<usize>,
    perm: Vec<u32>,
}

impl Run {
    fn key(&self, row: u32) -> &[u8] {
        let row = row as usize;
        let start = if row == 0 { 0 } else { self.key_ends[row - 1] };
        &self.key_bytes[start..self.key_ends[row]]
    }

    fn append(&mut self, chunk: Chunk, keys: &KeyBuf) -> Result<()> {
        let base = self.key_bytes.len();
        self.key_bytes.extend_from_slice(&keys.bytes);
        self.key_ends
            .extend(keys.offsets[1..].iter().map(|&end| base + end));
        match &mut self.chunk {
            Some(rows) => rows.extend(&chunk),
            None => {
                self.chunk = Some(chunk);
                Ok(())
            }
        }
    }

    /// The rows in key order; ties keep the order of arrival.
    fn sorted_rows(&self) -> Vec<u32> {
        let mut perm: Vec<u32> = (0..self.key_ends.len() as u32).collect();
        perm.sort_by(|&a, &b| self.key(a).cmp(self.key(b)));
        perm
    }
}

/// A position in a run during the merge.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct MergeHead<'a> {
    key: &'a [u8],
    run: u32,
    pos: u32,
}

/// One output column from rows `(source, row)` of several vectors of the same type.
fn take_column(sources: &[&Vector], picks: &[(u32, u32)]) -> Result<Vector> {
    let mismatch = || VetaError::internal("sorted runs have different column types");
    macro_rules! take_fixed {
        ($variant:ident) => {{
            let slices = sources
                .iter()
                .map(|s| match &s.data {
                    VectorData::$variant(x) => Ok(x.as_slice()),
                    _ => Err(mismatch()),
                })
                .collect::<Result<Vec<_>>>()?;
            VectorData::$variant(
                picks
                    .iter()
                    .map(|&(s, r)| slices[s as usize][r as usize])
                    .collect(),
            )
        }};
    }
    let first = sources
        .first()
        .ok_or_else(|| VetaError::internal("no sorted run to take rows from"))?;
    let data = match &first.data {
        VectorData::Boolean(_) => take_fixed!(Boolean),
        VectorData::Int16(_) => take_fixed!(Int16),
        VectorData::Int32(_) => take_fixed!(Int32),
        VectorData::Int64(_) => take_fixed!(Int64),
        VectorData::Int128(_) => take_fixed!(Int128),
        VectorData::Float64(_) => take_fixed!(Float64),
        VectorData::Date(_) => take_fixed!(Date),
        VectorData::Timestamp(_) => take_fixed!(Timestamp),
        VectorData::Decimal64 {
            precision, scale, ..
        } => {
            let slices = sources
                .iter()
                .map(|s| match &s.data {
                    VectorData::Decimal64 { values, .. } => Ok(values.as_slice()),
                    _ => Err(mismatch()),
                })
                .collect::<Result<Vec<_>>>()?;
            VectorData::Decimal64 {
                values: picks
                    .iter()
                    .map(|&(s, r)| slices[s as usize][r as usize])
                    .collect(),
                precision: *precision,
                scale: *scale,
            }
        }
        VectorData::Decimal128 {
            precision, scale, ..
        } => {
            let slices = sources
                .iter()
                .map(|s| match &s.data {
                    VectorData::Decimal128 { values, .. } => Ok(values.as_slice()),
                    _ => Err(mismatch()),
                })
                .collect::<Result<Vec<_>>>()?;
            VectorData::Decimal128 {
                values: picks
                    .iter()
                    .map(|&(s, r)| slices[s as usize][r as usize])
                    .collect(),
                precision: *precision,
                scale: *scale,
            }
        }
        VectorData::Varchar { .. } => {
            let parts = sources
                .iter()
                .map(|s| match &s.data {
                    VectorData::Varchar { offsets, bytes } => {
                        Ok((offsets.as_slice(), bytes.as_slice()))
                    }
                    _ => Err(mismatch()),
                })
                .collect::<Result<Vec<_>>>()?;
            let mut offsets = Vec::with_capacity(picks.len() + 1);
            let mut bytes = Vec::new();
            offsets.push(0u32);
            for &(s, r) in picks {
                let (from, data) = parts[s as usize];
                let r = r as usize;
                bytes.extend_from_slice(&data[from[r] as usize..from[r + 1] as usize]);
                offsets.push(
                    u32::try_from(bytes.len())
                        .map_err(|_| VetaError::internal("string vector exceeds 4 GiB"))?,
                );
            }
            VectorData::Varchar { offsets, bytes }
        }
    };
    let validity = if sources.iter().any(|s| s.validity.is_some()) {
        let valid: Vec<bool> = picks
            .iter()
            .map(|&(s, r)| sources[s as usize].is_valid(r as usize))
            .collect();
        Some(Bitmap::from_bools(&valid))
    } else {
        None
    };
    Vector::new(data, validity)
}

fn take_rows(runs: &[&Chunk], picks: &[(u32, u32)]) -> Result<Chunk> {
    let columns = runs.first().map_or(0, |c| c.num_columns());
    let mut out = Vec::with_capacity(columns);
    for c in 0..columns {
        let sources: Vec<&Vector> = runs.iter().map(|chunk| chunk.column(c)).collect();
        out.push(take_column(&sources, picks)?);
    }
    Chunk::with_len(out, picks.len())
}

/// Merges the sorted runs into chunks of up to [`CHUNK_ROWS`] rows.
fn merge_runs(runs: &[Run]) -> Result<Vec<Chunk>> {
    let live: Vec<&Run> = runs.iter().filter(|r| r.chunk.is_some()).collect();
    let chunks: Vec<&Chunk> = live.iter().filter_map(|r| r.chunk.as_ref()).collect();
    let mut heap = BinaryHeap::with_capacity(live.len());
    for (r, run) in live.iter().enumerate() {
        if let Some(&row) = run.perm.first() {
            heap.push(Reverse(MergeHead {
                key: run.key(row),
                run: r as u32,
                pos: 0,
            }));
        }
    }
    let mut out = Vec::new();
    let mut picks = Vec::with_capacity(CHUNK_ROWS);
    while let Some(Reverse(head)) = heap.pop() {
        let run = live[head.run as usize];
        picks.push((head.run, run.perm[head.pos as usize]));
        if picks.len() == CHUNK_ROWS {
            out.push(take_rows(&chunks, &picks)?);
            picks.clear();
        }
        let next = head.pos + 1;
        if let Some(&row) = run.perm.get(next as usize) {
            heap.push(Reverse(MergeHead {
                key: run.key(row),
                run: head.run,
                pos: next,
            }));
        }
    }
    if !picks.is_empty() {
        out.push(take_rows(&chunks, &picks)?);
    }
    Ok(out)
}

/// Full sort: each worker builds runs, they are sorted in parallel and merged.
pub(crate) fn sort(
    pipeline: Pipeline,
    keys: &[SortKey],
    ctx: &ExecContext<'_>,
) -> Result<Materialized> {
    struct Local {
        runs: Vec<Run>,
        current: Run,
        current_bytes: u64,
        buf: KeyBuf,
        mem: Reservation<'static>,
    }
    let compiled = compile_keys(keys)?;
    let locals = pipeline.run_workers(
        ctx,
        || {
            Ok(Local {
                runs: Vec::new(),
                current: Run::default(),
                current_bytes: 0,
                buf: KeyBuf::default(),
                mem: new_reservation()?,
            })
        },
        |local, _m, batch| {
            encode_keys(&compiled, &batch, &mut local.buf)?;
            let chunk = batch.into_chunk();
            let bytes = chunk_bytes(&chunk) + local.buf.bytes.len() as u64 + chunk.len as u64 * 12;
            local.mem.grow(bytes)?;
            local.current.append(chunk, &local.buf)?;
            local.current_bytes += bytes;
            if local.current_bytes >= RUN_BYTES {
                let run = std::mem::take(&mut local.current);
                local.runs.push(run);
                local.current_bytes = 0;
            }
            Ok(())
        },
    )?;
    drop(pipeline);
    let mut runs = Vec::new();
    let mut hold = Vec::with_capacity(locals.len() + 1);
    for mut local in locals {
        runs.append(&mut local.runs);
        runs.push(local.current);
        hold.push(local.mem);
    }
    let perms = parallel_tasks(ctx.threads, runs.len(), |r| Ok(runs[r].sorted_rows()))?;
    for (run, perm) in runs.iter_mut().zip(perms) {
        run.perm = perm;
    }
    let chunks = merge_runs(&runs)?;
    drop(runs);
    drop(hold);
    let mut mem = new_reservation()?;
    for chunk in &chunks {
        mem.grow(chunk_bytes(chunk))?;
    }
    Ok(Materialized {
        chunks,
        hold: vec![mem],
    })
}

/// A row kept by a top-N heap, ordered by its key alone.
#[derive(Debug)]
struct TopEntry {
    key: Vec<u8>,
    row: Vec<Value>,
    bytes: u64,
}

impl PartialEq for TopEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for TopEntry {}

impl PartialOrd for TopEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

/// Bytes a row kept as values takes.
fn row_bytes(row: &[Value]) -> u64 {
    let strings: usize = row
        .iter()
        .map(|v| match v {
            Value::Varchar(bytes) => bytes.len(),
            _ => 0,
        })
        .sum();
    (size_of_val(row) + strings) as u64
}

/// Top-N: each worker keeps the best `offset + limit` rows in a max-heap whose root is the
/// first row to drop; the heaps are merged and the window `offset..offset + limit` returned.
pub(crate) fn top_n(
    pipeline: Pipeline,
    keys: &[SortKey],
    limit: u64,
    offset: u64,
    types: &[DataType],
    ctx: &ExecContext<'_>,
) -> Result<Materialized> {
    struct Local {
        heap: BinaryHeap<TopEntry>,
        buf: KeyBuf,
        mem: Reservation<'static>,
    }
    let capacity = usize::try_from(offset.saturating_add(limit)).unwrap_or(usize::MAX);
    if capacity == 0 {
        return Ok(Materialized {
            chunks: Vec::new(),
            hold: Vec::new(),
        });
    }
    let compiled = compile_keys(keys)?;
    let locals = pipeline.run_workers(
        ctx,
        || {
            Ok(Local {
                heap: BinaryHeap::new(),
                buf: KeyBuf::default(),
                mem: new_reservation()?,
            })
        },
        |local, _m, batch| {
            encode_keys(&compiled, &batch, &mut local.buf)?;
            for i in 0..batch.rows() {
                let key = local.buf.key(i);
                if local.heap.len() >= capacity {
                    let better = local
                        .heap
                        .peek()
                        .is_some_and(|worst| key < worst.key.as_slice());
                    if !better {
                        continue;
                    }
                    if let Some(worst) = local.heap.pop() {
                        local.mem.shrink(worst.bytes);
                    }
                }
                let row_index = batch.selection.as_ref().map_or(i, |s| s[i] as usize);
                let row = batch.chunk.row(row_index);
                let bytes = row_bytes(&row) + key.len() as u64 + 64;
                local.mem.grow(bytes)?;
                local.heap.push(TopEntry {
                    key: key.to_vec(),
                    row,
                    bytes,
                });
            }
            Ok(())
        },
    )?;
    let mut all: Vec<TopEntry> = Vec::new();
    let mut hold = Vec::with_capacity(locals.len());
    for local in locals {
        all.extend(local.heap.into_vec());
        hold.push(local.mem);
    }
    all.sort();
    let rows: Vec<Vec<Value>> = all
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(|entry| entry.row)
        .collect();
    let mut chunks = Vec::new();
    for rows in rows.chunks(CHUNK_ROWS) {
        chunks.push(Chunk::from_rows(types, rows)?);
    }
    Ok(Materialized { chunks, hold })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecStats;
    use crate::database::Database;
    use crate::pipeline::{Source, collect};
    use veta_core::rand::Rng;
    use veta_sql::BoundExpr;

    fn sort_key(index: usize, data_type: DataType, desc: bool, nulls_first: bool) -> SortKey {
        SortKey {
            expr: BoundExpr::column(index, "c", data_type, true),
            desc,
            nulls_first,
        }
    }

    fn batch_of(vectors: Vec<Vector>) -> Batch {
        Batch {
            chunk: Chunk::new(vectors).unwrap(),
            selection: None,
        }
    }

    /// Sorted order of one column per its encoded keys, compared with the value order.
    fn check_order(values: Vec<Value>, data_type: DataType) {
        for (desc, nulls_first) in [(false, false), (false, true), (true, false), (true, true)] {
            let vector = Vector::from_values(data_type, &values).unwrap();
            let keys = compile_keys(&[sort_key(0, data_type, desc, nulls_first)]).unwrap();
            let mut buf = KeyBuf::default();
            encode_keys(&keys, &batch_of(vec![vector]), &mut buf).unwrap();
            let rank = |v: &Value| v.is_null() == nulls_first;
            let expected = |a: &Value, b: &Value| -> Ordering {
                if a.is_null() || b.is_null() {
                    return rank(b).cmp(&rank(a));
                }
                if desc { b.cmp(a) } else { a.cmp(b) }
            };
            for i in 0..values.len() {
                for j in 0..values.len() {
                    assert_eq!(
                        buf.key(i).cmp(buf.key(j)),
                        expected(&values[i], &values[j]),
                        "{:?} vs {:?}, desc {desc}, nulls_first {nulls_first}",
                        values[i],
                        values[j]
                    );
                }
            }
        }
    }

    #[test]
    fn encoded_integers_and_dates_sort_like_values() {
        check_order(
            [i64::MIN, -5, -1, 0, 1, 7, i64::MAX]
                .iter()
                .map(|&v| Value::Int64(v))
                .chain([Value::Null])
                .collect(),
            DataType::Int64,
        );
        check_order(
            vec![
                Value::Int16(-300),
                Value::Int16(-1),
                Value::Null,
                Value::Int16(0),
                Value::Int16(300),
            ],
            DataType::Int16,
        );
        check_order(
            vec![
                Value::Date(-10),
                Value::Date(0),
                Value::Date(9),
                Value::Null,
            ],
            DataType::Date,
        );
    }

    #[test]
    fn encoded_floats_follow_the_total_order() {
        check_order(
            [
                f64::NEG_INFINITY,
                -2.5,
                -0.0,
                0.0,
                1e-300,
                3.0,
                f64::INFINITY,
                f64::NAN,
            ]
            .iter()
            .map(|&f| Value::Float64(f))
            .chain([Value::Null])
            .collect(),
            DataType::Float64,
        );
    }

    #[test]
    fn encoded_strings_keep_prefixes_first_and_escape_zero_bytes() {
        check_order(
            [
                &b""[..],
                b"\0",
                b"\0\0",
                b"\0a",
                b"a",
                b"a\0",
                b"a\0b",
                b"ab",
                b"b",
            ]
            .iter()
            .map(|s| Value::varchar(s))
            .chain([Value::Null])
            .collect(),
            DataType::Varchar,
        );
    }

    #[test]
    fn composite_keys_compare_column_by_column() {
        let a = Vector::from_values(
            DataType::Varchar,
            &[
                Value::varchar("a"),
                Value::varchar("a"),
                Value::varchar("ab"),
            ],
        )
        .unwrap();
        let b = Vector::from_data(VectorData::Int32(vec![2, 1, 0]));
        let keys = compile_keys(&[
            sort_key(0, DataType::Varchar, false, false),
            sort_key(1, DataType::Int32, true, false),
        ])
        .unwrap();
        let mut buf = KeyBuf::default();
        encode_keys(&keys, &batch_of(vec![a, b]), &mut buf).unwrap();
        assert!(buf.key(0) < buf.key(1), "same a, b descending");
        assert!(buf.key(1) < buf.key(2), "a before ab");
    }

    fn pipeline_of_random_runs(seed: u64) -> (Pipeline, Vec<i64>) {
        let mut rng = Rng::new(seed);
        let mut all = Vec::new();
        let mut chunks = Vec::new();
        for _ in 0..50 {
            let values: Vec<i64> = (0..300).map(|_| rng.range_i64(-1000, 1000)).collect();
            all.extend_from_slice(&values);
            let doubled: Vec<i64> = values.iter().map(|v| v * 2).collect();
            chunks.push(
                Chunk::new(vec![
                    Vector::from_data(VectorData::Int64(values)),
                    Vector::from_data(VectorData::Int64(doubled)),
                ])
                .unwrap(),
            );
        }
        (Pipeline::new(Source::chunks(chunks)), all)
    }

    fn first_two(chunks: &[Chunk]) -> Vec<(Value, Value)> {
        chunks
            .iter()
            .flat_map(|c| (0..c.len).map(move |i| (c.column(0).get(i), c.column(1).get(i))))
            .collect()
    }

    #[test]
    fn sort_merges_runs_of_many_threads() {
        for threads in [1, 8] {
            let db = Database::open("unused");
            let stats = ExecStats::default();
            let ctx = ExecContext {
                db: &db,
                threads,
                stats: &stats,
            };
            let (pipeline, mut all) = pipeline_of_random_runs(5);
            let out = sort(pipeline, &[sort_key(0, DataType::Int64, true, false)], &ctx).unwrap();
            all.sort_unstable_by(|a, b| b.cmp(a));
            let rows = first_two(&out.chunks);
            assert_eq!(rows.len(), all.len());
            for (row, &v) in rows.iter().zip(&all) {
                assert_eq!(row, &(Value::Int64(v), Value::Int64(v * 2)));
            }
        }
    }

    #[test]
    fn top_n_returns_the_window_of_the_full_sort() {
        let types = [DataType::Int64, DataType::Int64];
        for threads in [1, 8] {
            let db = Database::open("unused");
            let stats = ExecStats::default();
            let ctx = ExecContext {
                db: &db,
                threads,
                stats: &stats,
            };
            let (pipeline, mut all) = pipeline_of_random_runs(9);
            let out = top_n(
                pipeline,
                &[sort_key(0, DataType::Int64, false, false)],
                25,
                10,
                &types,
                &ctx,
            )
            .unwrap();
            all.sort_unstable();
            let rows = first_two(&out.chunks);
            assert_eq!(rows.len(), 25);
            for (row, &v) in rows.iter().zip(&all[10..35]) {
                assert_eq!(row.0, Value::Int64(v));
            }

            let (pipeline, _) = pipeline_of_random_runs(9);
            let empty = top_n(
                pipeline,
                &[sort_key(0, DataType::Int64, false, false)],
                0,
                0,
                &types,
                &ctx,
            )
            .unwrap();
            assert!(empty.chunks.is_empty());
        }
    }

    #[test]
    fn take_rows_mixes_runs_with_and_without_nulls() {
        let a = Chunk::new(vec![
            Vector::from_values(DataType::Varchar, &[Value::varchar("x"), Value::Null]).unwrap(),
        ])
        .unwrap();
        let b = Chunk::new(vec![
            Vector::from_values(DataType::Varchar, &[Value::varchar("y")]).unwrap(),
        ])
        .unwrap();
        let out = take_rows(&[&a, &b], &[(1, 0), (0, 1), (0, 0)]).unwrap();
        assert_eq!(out.column(0).get(0), Value::varchar("y"));
        assert_eq!(out.column(0).get(1), Value::Null);
        assert_eq!(out.column(0).get(2), Value::varchar("x"));
    }

    #[test]
    fn collect_of_an_empty_sort_is_empty() {
        let db = Database::open("unused");
        let stats = ExecStats::default();
        let ctx = ExecContext {
            db: &db,
            threads: 4,
            stats: &stats,
        };
        let out = sort(
            Pipeline::new(Source::chunks(Vec::new())),
            &[sort_key(0, DataType::Int64, false, false)],
            &ctx,
        )
        .unwrap();
        assert!(out.chunks.is_empty());
        let out = collect(Pipeline::new(Source::chunks(Vec::new())), &ctx, None).unwrap();
        assert!(out.chunks.is_empty());
    }
}
