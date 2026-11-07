//! Hash joins and nested loop joins (contract §10.4, §11.1, §12).
//!
//! The right input builds and the left input probes. The build is a pipeline break; the
//! probe is one more streaming operator of the left pipeline, so it runs on the morsels of
//! the left input on every worker.
//!
//! Build of a hash join: the workers evaluate the right keys of each batch, drop the rows
//! with a NULL key (NULL never equals) and encode the others with their hash, as packed
//! `u64`/`u128` keys or as bytes (the layouts of [`crate::group_keys`]). The batches are
//! put back in morsel order and concatenated into one chunk, so every build row has the
//! same number whatever the thread count. The top bits of the hash split the rows into
//! [`PARTITIONS`] radix partitions, built in parallel, each one a hash table from a key to
//! its build rows in row order. A nested loop join (no key) keeps every right row.
//!
//! Every partition table is sized from the exact number of build rows it receives, twice as
//! many slots as rows, so it never grows while filled; the build chunk is allocated once
//! from the row and byte counts of the pieces. The probe hashes a string key straight from
//! its vector, without copying the batch's strings.
//!
//! Probe: the candidates of a left row are the build rows with an equal key (every build
//! row in a nested loop join; none when a left key is NULL). The pairs, at most
//! [`CHUNK_ROWS`] at a time, are filtered by the residual; a pair that passes is a match:
//!
//! - `Inner`, `Cross`: every match.
//! - `Left`: every match, then the left rows without one with NULLs on the right.
//! - `Single`: like `Left`, and a second match of a left row is an error. Without keys and
//!   without a residual the right input is the value of a scalar subquery, evaluated once:
//!   more than one right row is an error even if no left row asks for it.
//! - `Semi`, `Anti`: the left rows with and without a match, as a selection over the batch.
//! - `NullAwareAnti` (one key, no residual): every left row if the right input is empty;
//!   none if a right key is NULL; otherwise the left rows with a non-NULL key and no match.

use std::borrow::Cow;

use veta_core::hash::hash_bytes;
use veta_core::memory::Reservation;
use veta_core::{Chunk, DataType, Result, Vector, VectorData, VetaError};
use veta_sql::{BoundExpr, JoinKind};

use crate::context::{ExecContext, new_reservation};
use crate::expr_eval::CompiledExpr;
use crate::group_keys::{KeyBuf, KeyLayout, pack_keys, serialize_keys};
use crate::hash_table::{BytesTable, FixedKey, FixedTable, PARTITIONS, partition_of};
use crate::pipeline::{Batch, CHUNK_ROWS, Evaluator, Pipeline, chunk_bytes, parallel_tasks};

fn single_error() -> VetaError {
    VetaError::user("more than one row returned by a subquery used as an expression")
}

fn mismatch() -> VetaError {
    VetaError::internal("join keys do not match their hash table")
}

/// Encoded keys of the rows of a batch whose key columns are all non-NULL.
#[derive(Debug)]
enum KeyData {
    U64(Vec<u64>),
    U128(Vec<u128>),
    /// A string key as is, or a composite key.
    Bytes(KeyBuf),
}

#[derive(Debug)]
struct EncodedKeys {
    /// Row of the batch of each key; `None` when no row has a NULL key.
    rows: Option<Vec<u32>>,
    data: KeyData,
    hashes: Vec<u64>,
}

impl EncodedKeys {
    fn len(&self) -> usize {
        self.hashes.len()
    }

    #[inline]
    fn row(&self, j: usize) -> usize {
        self.rows.as_ref().map_or(j, |rows| rows[j] as usize)
    }

    fn bytes(&self) -> u64 {
        let data = match &self.data {
            KeyData::U64(keys) => keys.capacity() * 8,
            KeyData::U128(keys) => keys.capacity() * 16,
            KeyData::Bytes(buf) => buf.offsets.capacity() * 8 + buf.bytes.capacity(),
        };
        let rows = self.rows.as_ref().map_or(0, |rows| rows.capacity() * 4);
        (data + rows + self.hashes.capacity() * 8) as u64
    }
}

/// Encodes the keys of `n` rows given by one vector per key column. `layout` is that of the
/// key types with every column NOT NULL: rows with a NULL key are left out.
fn encode_keys(layout: &KeyLayout, vectors: Vec<Cow<'_, Vector>>, n: usize) -> Result<EncodedKeys> {
    let (rows, vectors) = if vectors.iter().any(|v| v.null_count() > 0) {
        let rows: Vec<u32> = (0..n)
            .filter(|&i| vectors.iter().all(|v| v.is_valid(i)))
            .map(|i| i as u32)
            .collect();
        let gathered: Vec<Cow<'_, Vector>> = vectors
            .iter()
            .map(|v| Cow::Owned(v.gather(&rows)))
            .collect();
        (Some(rows), gathered)
    } else {
        (None, vectors)
    };
    let m = rows.as_ref().map_or(n, Vec::len);
    let bytes_hashes =
        |buf: &KeyBuf| -> Vec<u64> { (0..m).map(|i| hash_bytes(buf.key(i))).collect() };
    let (data, hashes): (KeyData, Vec<u64>) = match layout {
        KeyLayout::Fixed64(columns) => {
            let mut keys: Vec<u64> = Vec::new();
            pack_keys(columns, &vectors, m, &mut keys)?;
            let hashes = keys.iter().map(|k| k.hash64()).collect();
            (KeyData::U64(keys), hashes)
        }
        KeyLayout::Fixed128(columns) => {
            let mut keys: Vec<u128> = Vec::new();
            pack_keys(columns, &vectors, m, &mut keys)?;
            let hashes = keys.iter().map(|k| k.hash64()).collect();
            (KeyData::U128(keys), hashes)
        }
        KeyLayout::Str => {
            let Some(VectorData::Varchar { offsets, bytes }) = vectors.first().map(|v| &v.data)
            else {
                return Err(mismatch());
            };
            let buf = KeyBuf {
                offsets: offsets.iter().map(|&o| o as usize).collect(),
                bytes: bytes.clone(),
                cursors: Vec::new(),
            };
            let hashes = bytes_hashes(&buf);
            (KeyData::Bytes(buf), hashes)
        }
        KeyLayout::Composite(columns) => {
            let mut buf = KeyBuf::default();
            serialize_keys(columns, &vectors, m, &mut buf)?;
            let hashes = bytes_hashes(&buf);
            (KeyData::Bytes(buf), hashes)
        }
        KeyLayout::Global => return Err(VetaError::internal("hash join without keys")),
    };
    Ok(EncodedKeys { rows, data, hashes })
}

#[derive(Debug)]
enum JoinTable {
    Fixed64(FixedTable<u64>),
    Fixed128(FixedTable<u128>),
    Bytes(BytesTable),
}

impl JoinTable {
    fn len(&self) -> usize {
        match self {
            JoinTable::Fixed64(t) => t.len(),
            JoinTable::Fixed128(t) => t.len(),
            JoinTable::Bytes(t) => t.len(),
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            JoinTable::Fixed64(t) => t.bytes(),
            JoinTable::Fixed128(t) => t.bytes(),
            JoinTable::Bytes(t) => t.bytes(),
        }
    }

    /// The id of key `j` of `keys`, whose hash is `hash`.
    #[inline]
    fn find(&self, keys: &KeyData, j: usize, hash: u64) -> Result<Option<u32>> {
        Ok(match (self, keys) {
            (JoinTable::Fixed64(t), KeyData::U64(k)) => t.find(k[j]),
            (JoinTable::Fixed128(t), KeyData::U128(k)) => t.find(k[j]),
            (JoinTable::Bytes(t), KeyData::Bytes(buf)) => t.find(buf.key(j), hash),
            _ => return Err(mismatch()),
        })
    }
}

/// One radix partition of the build side: key id `i` has the build rows
/// `rows[starts[i]..starts[i + 1]]`, in row order.
#[derive(Debug)]
struct Partition {
    table: JoinTable,
    starts: Vec<u32>,
    rows: Vec<u32>,
    _mem: Reservation<'static>,
}

impl Partition {
    #[inline]
    fn rows_of(&self, id: u32) -> &[u32] {
        let id = id as usize;
        &self.rows[self.starts[id] as usize..self.starts[id + 1] as usize]
    }
}

/// The rows that one worker built from one batch of the right input.
#[derive(Debug)]
struct BuildPiece {
    morsel: usize,
    order: usize,
    chunk: Chunk,
    /// `None` in a nested loop join.
    keys: Option<EncodedKeys>,
    /// Number of the first row of `chunk` in the build side.
    base: u32,
    mem: Reservation<'static>,
}

/// Ids of the keys of partition `p` inserted into `table`, and the build row of each.
fn insert_fixed<K: FixedKey>(
    table: &mut FixedTable<K>,
    pieces: &[BuildPiece],
    lists: &[Vec<Vec<u32>>],
    p: usize,
    keys_of: impl Fn(&KeyData) -> Option<&[K]>,
    ids: &mut Vec<u32>,
    members: &mut Vec<u32>,
) -> Result<()> {
    for (piece, lists) in pieces.iter().zip(lists) {
        let encoded = piece.keys.as_ref().ok_or_else(mismatch)?;
        let keys = keys_of(&encoded.data).ok_or_else(mismatch)?;
        for &local in &lists[p] {
            ids.push(table.find_or_insert(keys[local as usize]));
            members.push(piece.base + local);
        }
    }
    Ok(())
}

/// Builds partition `p` from the rows of each piece that `lists[piece][p]` names.
fn build_partition(
    layout: &KeyLayout,
    pieces: &[BuildPiece],
    lists: &[Vec<Vec<u32>>],
    p: usize,
) -> Result<Partition> {
    let n: usize = lists.iter().map(|l| l[p].len()).sum();
    let mut mem = new_reservation()?;
    // The ids and the members below, until the rows are grouped by key.
    mem.grow(n as u64 * 8)?;
    let mut ids = Vec::with_capacity(n);
    let mut members = Vec::with_capacity(n);
    // Twice as many slots as rows: the tables never grow while they are filled.
    let table = match layout {
        KeyLayout::Fixed64(_) => {
            let mut table = FixedTable::<u64>::with_slots(2 * n);
            insert_fixed(
                &mut table,
                pieces,
                lists,
                p,
                |d| match d {
                    KeyData::U64(k) => Some(k.as_slice()),
                    _ => None,
                },
                &mut ids,
                &mut members,
            )?;
            JoinTable::Fixed64(table)
        }
        KeyLayout::Fixed128(_) => {
            let mut table = FixedTable::<u128>::with_slots(2 * n);
            insert_fixed(
                &mut table,
                pieces,
                lists,
                p,
                |d| match d {
                    KeyData::U128(k) => Some(k.as_slice()),
                    _ => None,
                },
                &mut ids,
                &mut members,
            )?;
            JoinTable::Fixed128(table)
        }
        KeyLayout::Str | KeyLayout::Composite(_) => {
            let mut table = BytesTable::with_slots(2 * n);
            for (piece, lists) in pieces.iter().zip(lists) {
                let encoded = piece.keys.as_ref().ok_or_else(mismatch)?;
                let KeyData::Bytes(buf) = &encoded.data else {
                    return Err(mismatch());
                };
                for &local in &lists[p] {
                    let l = local as usize;
                    ids.push(table.find_or_insert(buf.key(l), encoded.hashes[l]));
                    members.push(piece.base + local);
                }
            }
            JoinTable::Bytes(table)
        }
        KeyLayout::Global => return Err(VetaError::internal("hash join without keys")),
    };
    let keys = table.len();
    let mut starts = vec![0u32; keys + 1];
    for &id in &ids {
        starts[id as usize + 1] += 1;
    }
    for i in 0..keys {
        starts[i + 1] += starts[i];
    }
    let mut cursors = starts[..keys].to_vec();
    let mut rows = vec![0u32; n];
    for (&id, &row) in ids.iter().zip(&members) {
        let cursor = &mut cursors[id as usize];
        rows[*cursor as usize] = row;
        *cursor += 1;
    }
    mem.resize(table.bytes() + (starts.capacity() + rows.capacity()) as u64 * 4)?;
    Ok(Partition {
        table,
        starts,
        rows,
        _mem: mem,
    })
}

/// The residual, compiled over the columns of `left ++ right` it reads, in that order.
#[derive(Debug)]
struct Residual {
    compiled: CompiledExpr,
    columns: Vec<usize>,
}

impl Residual {
    fn new(expr: &BoundExpr) -> Result<Residual> {
        let columns = expr.columns();
        let mut compact = expr.clone();
        compact.map_columns(&mut |c| columns.binary_search(&c).unwrap_or(usize::MAX));
        Ok(Residual {
            compiled: CompiledExpr::new(&compact)?,
            columns,
        })
    }
}

/// The selected rows of `batch`, narrowed to `rows` (positions among them) if given.
fn keep_rows(batch: Batch, rows: Option<&[u32]>) -> Chunk {
    match (rows, &batch.selection) {
        (None, _) => batch.into_chunk(),
        (Some(rows), None) => batch.chunk.gather(rows),
        (Some(rows), Some(selection)) => {
            let rows: Vec<u32> = rows.iter().map(|&r| selection[r as usize]).collect();
            batch.chunk.gather(&rows)
        }
    }
}

/// Positions `rows` of the selected rows of `batch` as rows of its chunk.
fn chunk_rows<'a>(batch: &Batch, rows: &'a [u32]) -> Cow<'a, [u32]> {
    match &batch.selection {
        Some(selection) => Cow::Owned(rows.iter().map(|&r| selection[r as usize]).collect()),
        None => Cow::Borrowed(rows),
    }
}

/// `batch` narrowed to `rows`, positions among its selected rows.
fn emit_selected(
    batch: Batch,
    rows: Vec<u32>,
    emit: &mut dyn FnMut(Batch) -> Result<()>,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    if rows.len() == batch.rows() {
        return emit(batch);
    }
    let selection = chunk_rows(&batch, &rows).into_owned();
    emit(Batch {
        chunk: batch.chunk,
        selection: Some(selection),
        dicts: batch.dicts,
        origin: batch.origin,
    })
}

/// Pairs of one probe batch waiting to be checked, and what is known of each left row.
#[derive(Debug)]
struct Pairs {
    left: Vec<u32>,
    right: Vec<u32>,
    /// Matches of each left row so far.
    matches: Vec<u32>,
}

/// A built join: the probe runs as a streaming operator of the left pipeline.
#[derive(Debug)]
pub(crate) struct JoinProbe {
    kind: JoinKind,
    left_width: usize,
    right_types: Vec<DataType>,
    /// The left keys; none in a nested loop join.
    probe_keys: Vec<Evaluator>,
    layout: KeyLayout,
    residual: Option<Residual>,
    /// The build rows: those with no NULL key in a hash join, all in a nested loop join.
    /// Without columns when neither the output nor the residual reads them.
    build: Chunk,
    partitions: Vec<Partition>,
    /// `0..build.len`: the candidates of every left row of a nested loop join.
    all_rows: Vec<u32>,
    /// Rows of the right input, NULL keys included.
    input_rows: u64,
    has_null_key: bool,
    _mem: Vec<Reservation<'static>>,
}

/// Builds the join of a left input of `left_width` columns with the right input that
/// `pipeline` produces, whose columns have the types `right_types`. `keys` are the pairs
/// `(left, right)` of equal expressions; without keys the join is a nested loop.
pub(crate) fn build(
    pipeline: Pipeline,
    kind: JoinKind,
    keys: &[(BoundExpr, BoundExpr)],
    residual: Option<&BoundExpr>,
    left_width: usize,
    right_types: Vec<DataType>,
    ctx: &ExecContext<'_>,
) -> Result<JoinProbe> {
    if kind == JoinKind::NullAwareAnti {
        if keys.len() != 1 {
            return Err(VetaError::internal(
                "null-aware anti join needs exactly one key",
            ));
        }
        if residual.is_some() {
            return Err(VetaError::unsupported(
                "null-aware anti join with a residual",
            ));
        }
    }
    for (l, r) in keys {
        if l.data_type != r.data_type {
            return Err(VetaError::internal(format!(
                "join key types differ: {} and {}",
                l.data_type, r.data_type
            )));
        }
    }
    if let Some(residual) = residual
        && residual
            .columns()
            .iter()
            .any(|&c| c >= left_width + right_types.len())
    {
        return Err(VetaError::internal(
            "join residual reads a column out of range",
        ));
    }
    let hashed = !keys.is_empty();
    let layout = if hashed {
        let columns: Vec<(DataType, bool)> =
            keys.iter().map(|(l, _)| (l.data_type, false)).collect();
        KeyLayout::new(&columns)
    } else {
        KeyLayout::Global
    };
    let build_keys = keys
        .iter()
        .map(|(_, r)| Evaluator::new(r))
        .collect::<Result<Vec<_>>>()?;
    let probe_keys = keys
        .iter()
        .map(|(l, _)| Evaluator::new(l))
        .collect::<Result<Vec<_>>>()?;
    let residual = residual.map(Residual::new).transpose()?;
    let keep_columns = residual.is_some()
        || matches!(
            kind,
            JoinKind::Inner | JoinKind::Cross | JoinKind::Left | JoinKind::Single
        );

    struct Local {
        pieces: Vec<BuildPiece>,
        input_rows: u64,
        has_null_key: bool,
    }
    let locals = pipeline.run_workers(
        ctx,
        || {
            Ok(Local {
                pieces: Vec::new(),
                input_rows: 0,
                has_null_key: false,
            })
        },
        |local, morsel, batch| {
            let n = batch.rows();
            local.input_rows += n as u64;
            let (chunk, encoded) = if hashed {
                let vectors = build_keys
                    .iter()
                    .map(|e| e.vector(&batch))
                    .collect::<Result<Vec<_>>>()?;
                let mut encoded = encode_keys(&layout, vectors, n)?;
                if encoded.rows.is_some() {
                    local.has_null_key = true;
                }
                if encoded.len() == 0 {
                    return Ok(());
                }
                let rows = encoded.rows.take();
                let chunk = if keep_columns {
                    keep_rows(batch, rows.as_deref())
                } else {
                    Chunk::with_len(Vec::new(), encoded.len())?
                };
                (chunk, Some(encoded))
            } else {
                let chunk = if keep_columns {
                    batch.into_chunk()
                } else {
                    Chunk::with_len(Vec::new(), n)?
                };
                (chunk, None)
            };
            let mut mem = new_reservation()?;
            mem.grow(chunk_bytes(&chunk) + encoded.as_ref().map_or(0, EncodedKeys::bytes))?;
            let order = local.pieces.len();
            local.pieces.push(BuildPiece {
                morsel,
                order,
                chunk,
                keys: encoded,
                base: 0,
                mem,
            });
            Ok(())
        },
    )?;
    drop(pipeline);

    let mut input_rows = 0;
    let mut has_null_key = false;
    let mut pieces = Vec::new();
    for local in locals {
        input_rows += local.input_rows;
        has_null_key |= local.has_null_key;
        pieces.extend(local.pieces);
    }
    if kind == JoinKind::Single && !hashed && residual.is_none() && input_rows > 1 {
        return Err(single_error());
    }
    pieces.sort_unstable_by_key(|piece| (piece.morsel, piece.order));
    let total: usize = pieces.iter().map(|piece| piece.chunk.len).sum();
    if total >= u32::MAX as usize {
        return Err(VetaError::unsupported(
            "join build side of 2^32 rows or more",
        ));
    }

    let mut build = if keep_columns {
        presized_chunk(&right_types, &pieces, total)
    } else {
        Chunk::with_len(Vec::new(), 0)?
    };
    let mut build_mem = new_reservation()?;
    for piece in &mut pieces {
        piece.base = build.len as u32;
        let chunk = std::mem::replace(&mut piece.chunk, Chunk::with_len(Vec::new(), 0)?);
        let bytes = chunk_bytes(&chunk);
        build_mem.grow(bytes)?;
        build.extend(&chunk)?;
        drop(chunk);
        piece.mem.shrink(bytes);
    }
    let mut mem = vec![build_mem];

    let mut partitions = Vec::new();
    let mut all_rows = Vec::new();
    if hashed {
        let lists = parallel_tasks(ctx.threads, pieces.len(), |j| {
            let encoded = pieces[j].keys.as_ref().ok_or_else(mismatch)?;
            let mut lists = vec![Vec::new(); PARTITIONS];
            for (local, &hash) in encoded.hashes.iter().enumerate() {
                lists[partition_of(hash)].push(local as u32);
            }
            Ok(lists)
        })?;
        let mut lists_mem = new_reservation()?;
        lists_mem.grow(total as u64 * 4)?;
        partitions = parallel_tasks(ctx.threads, PARTITIONS, |p| {
            build_partition(&layout, &pieces, &lists, p)
        })?;
    } else {
        let mut rows_mem = new_reservation()?;
        rows_mem.grow(total as u64 * 4)?;
        all_rows = (0..total as u32).collect();
        mem.push(rows_mem);
    }
    drop(pieces);

    Ok(JoinProbe {
        kind,
        left_width,
        right_types,
        probe_keys,
        layout,
        residual,
        build,
        partitions,
        all_rows,
        input_rows,
        has_null_key,
        _mem: mem,
    })
}

/// An empty chunk of `types` with room for the `total` rows of `pieces` and their strings.
fn presized_chunk(types: &[DataType], pieces: &[BuildPiece], total: usize) -> Chunk {
    let columns = types
        .iter()
        .enumerate()
        .map(|(c, &t)| {
            let mut data = VectorData::with_capacity(t, total);
            if let VectorData::Varchar { bytes, .. } = &mut data {
                let strings: usize = pieces
                    .iter()
                    .map(|piece| match piece.chunk.columns.get(c).map(|v| &v.data) {
                        Some(VectorData::Varchar { bytes, .. }) => bytes.len(),
                        _ => 0,
                    })
                    .sum();
                bytes.reserve(strings);
            }
            Vector::from_data(data)
        })
        .collect();
    Chunk { columns, len: 0 }
}

impl JoinProbe {
    /// Whether no left row can produce output, so the left input need not be read.
    pub fn produces_nothing(&self) -> bool {
        match self.kind {
            JoinKind::Inner | JoinKind::Cross | JoinKind::Semi => self.build.len == 0,
            JoinKind::NullAwareAnti => self.input_rows > 0 && self.has_null_key,
            JoinKind::Left | JoinKind::Anti | JoinKind::Single => false,
        }
    }

    /// Joins the rows of `batch` with the build side; `emit` gets the output, in batches
    /// of at most [`CHUNK_ROWS`] pairs.
    pub fn probe(&self, batch: Batch, emit: &mut dyn FnMut(Batch) -> Result<()>) -> Result<()> {
        if self.kind == JoinKind::NullAwareAnti && self.input_rows == 0 {
            return emit(batch);
        }
        if self.produces_nothing() {
            return Ok(());
        }
        let candidates = self.candidates(&batch)?;
        let without_pairs = self.residual.is_none()
            && matches!(
                self.kind,
                JoinKind::Semi | JoinKind::Anti | JoinKind::NullAwareAnti
            );
        if !without_pairs {
            return self.probe_pairs(batch, &candidates, emit);
        }
        let keep = |c: &Option<&[u32]>| match self.kind {
            JoinKind::Semi => c.is_some_and(|c| !c.is_empty()),
            JoinKind::Anti => c.is_none_or(|c| c.is_empty()),
            _ => c.is_some_and(|c| c.is_empty()),
        };
        let rows = (0..candidates.len())
            .filter(|&i| keep(&candidates[i]))
            .map(|i| i as u32)
            .collect();
        emit_selected(batch, rows, emit)
    }

    /// The build rows with the key of each selected row of `batch`: `None` when a key of
    /// the row is NULL.
    fn candidates(&self, batch: &Batch) -> Result<Vec<Option<&[u32]>>> {
        let n = batch.rows();
        if self.probe_keys.is_empty() {
            return Ok(vec![Some(self.all_rows.as_slice()); n]);
        }
        let vectors = self
            .probe_keys
            .iter()
            .map(|e| e.vector(batch))
            .collect::<Result<Vec<_>>>()?;
        if let (KeyLayout::Str, [v]) = (&self.layout, vectors.as_slice())
            && v.validity.is_none()
            && let VectorData::Varchar { offsets, bytes } = &v.data
        {
            let mut out = Vec::with_capacity(n);
            for w in offsets.windows(2) {
                let key = &bytes[w[0] as usize..w[1] as usize];
                let hash = hash_bytes(key);
                let partition = &self.partitions[partition_of(hash)];
                let JoinTable::Bytes(table) = &partition.table else {
                    return Err(mismatch());
                };
                out.push(Some(match table.find(key, hash) {
                    Some(id) => partition.rows_of(id),
                    None => &[][..],
                }));
            }
            return Ok(out);
        }
        let keys = encode_keys(&self.layout, vectors, n)?;
        let mut out = vec![None; n];
        for j in 0..keys.len() {
            let hash = keys.hashes[j];
            let partition = &self.partitions[partition_of(hash)];
            let rows = match partition.table.find(&keys.data, j, hash)? {
                Some(id) => partition.rows_of(id),
                None => &[][..],
            };
            out[keys.row(j)] = Some(rows);
        }
        Ok(out)
    }

    fn probe_pairs(
        &self,
        batch: Batch,
        candidates: &[Option<&[u32]>],
        emit: &mut dyn FnMut(Batch) -> Result<()>,
    ) -> Result<()> {
        let n = batch.rows();
        let first_match_is_enough = matches!(self.kind, JoinKind::Semi | JoinKind::Anti);
        let mut pairs = Pairs {
            left: Vec::with_capacity(CHUNK_ROWS),
            right: Vec::with_capacity(CHUNK_ROWS),
            matches: vec![0; n],
        };
        for (i, candidates) in candidates.iter().enumerate() {
            let Some(mut rest) = *candidates else {
                continue;
            };
            while !rest.is_empty() {
                if first_match_is_enough && pairs.matches[i] > 0 {
                    break;
                }
                let take = rest.len().min(CHUNK_ROWS - pairs.left.len());
                pairs.left.extend(std::iter::repeat_n(i as u32, take));
                pairs.right.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if pairs.left.len() == CHUNK_ROWS {
                    self.flush(&batch, &mut pairs, emit)?;
                }
            }
        }
        if !pairs.left.is_empty() {
            self.flush(&batch, &mut pairs, emit)?;
        }
        match self.kind {
            JoinKind::Inner | JoinKind::Cross => Ok(()),
            JoinKind::Left | JoinKind::Single => {
                let unmatched: Vec<u32> = (0..n)
                    .filter(|&i| pairs.matches[i] == 0)
                    .map(|i| i as u32)
                    .collect();
                for rows in unmatched.chunks(CHUNK_ROWS) {
                    let mut columns = self.gather_left(&batch, rows);
                    columns.extend(
                        self.right_types
                            .iter()
                            .map(|&t| Vector::nulls(t, rows.len())),
                    );
                    emit(Batch::new(Chunk::with_len(columns, rows.len())?))?;
                }
                Ok(())
            }
            JoinKind::Semi | JoinKind::Anti => {
                let semi = self.kind == JoinKind::Semi;
                let rows = (0..n)
                    .filter(|&i| (pairs.matches[i] > 0) == semi)
                    .map(|i| i as u32)
                    .collect();
                emit_selected(batch, rows, emit)
            }
            JoinKind::NullAwareAnti => Err(VetaError::internal(
                "null-aware anti join does not check pairs",
            )),
        }
    }

    /// Checks the waiting pairs against the residual, counts the matches and emits them
    /// for the kinds that output pairs.
    fn flush(
        &self,
        batch: &Batch,
        pairs: &mut Pairs,
        emit: &mut dyn FnMut(Batch) -> Result<()>,
    ) -> Result<()> {
        let passing = match &self.residual {
            Some(residual) => {
                let chunk = self.residual_chunk(residual, batch, &pairs.left, &pairs.right)?;
                Some(residual.compiled.eval_predicate(&chunk, None)?)
            }
            None => None,
        };
        let count = passing.as_ref().map_or(pairs.left.len(), Vec::len);
        for p in 0..count {
            let k = passing.as_ref().map_or(p, |s| s[p] as usize);
            let left = pairs.left[k] as usize;
            pairs.matches[left] += 1;
            if self.kind == JoinKind::Single && pairs.matches[left] > 1 {
                return Err(single_error());
            }
        }
        let outputs_pairs = matches!(
            self.kind,
            JoinKind::Inner | JoinKind::Cross | JoinKind::Left | JoinKind::Single
        );
        if outputs_pairs && count > 0 {
            let (left, right): (Cow<'_, [u32]>, Cow<'_, [u32]>) = match &passing {
                Some(s) => (
                    Cow::Owned(s.iter().map(|&k| pairs.left[k as usize]).collect()),
                    Cow::Owned(s.iter().map(|&k| pairs.right[k as usize]).collect()),
                ),
                None => (
                    Cow::Borrowed(pairs.left.as_slice()),
                    Cow::Borrowed(pairs.right.as_slice()),
                ),
            };
            let mut columns = self.gather_left(batch, &left);
            columns.extend(self.build.columns.iter().map(|c| c.gather(&right)));
            emit(Batch::new(Chunk::with_len(columns, count)?))?;
        }
        pairs.left.clear();
        pairs.right.clear();
        Ok(())
    }

    /// Every column of the selected rows `rows` of `batch`.
    fn gather_left(&self, batch: &Batch, rows: &[u32]) -> Vec<Vector> {
        let rows = chunk_rows(batch, rows);
        batch
            .chunk
            .columns
            .iter()
            .map(|c| c.gather(&rows))
            .collect()
    }

    /// The columns the residual reads, one row per pair.
    fn residual_chunk(
        &self,
        residual: &Residual,
        batch: &Batch,
        left: &[u32],
        right: &[u32],
    ) -> Result<Chunk> {
        let left_rows = chunk_rows(batch, left);
        let columns = residual
            .columns
            .iter()
            .map(|&c| {
                if c < self.left_width {
                    batch.chunk.columns[c].gather(&left_rows)
                } else {
                    self.build.columns[c - self.left_width].gather(right)
                }
            })
            .collect();
        Chunk::with_len(columns, left.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecStats;
    use crate::database::Database;
    use crate::pipeline::{Source, StreamOp, collect};
    use veta_core::Value;
    use veta_core::rand::Rng;
    use veta_sql::BoundExprKind;
    use veta_sql::expr::BinaryOp;

    /// Columns of both inputs: `k BIGINT, s VARCHAR, v BIGINT`, all nullable.
    const TYPES: [DataType; 3] = [DataType::Int64, DataType::Varchar, DataType::Int64];
    const WIDTH: usize = 3;

    type Rows = Vec<Vec<Value>>;
    /// The residual of the naive join over a left and a right row.
    type Check = fn(&[Value], &[Value]) -> bool;

    fn col(index: usize, side: usize) -> BoundExpr {
        let local = index % WIDTH;
        BoundExpr::column(index, format!("c{side}_{local}"), TYPES[local], true)
    }

    /// `left.v <> right.v`, the shape of the residual of TPC-H Q21.
    fn not_equal_v() -> (BoundExpr, Check) {
        let expr = BoundExpr::new(
            BoundExprKind::Binary {
                op: BinaryOp::NotEq,
                left: Box::new(col(2, 0)),
                right: Box::new(col(WIDTH + 2, 1)),
            },
            DataType::Boolean,
            true,
        );
        let check: Check = |l, r| !l[2].is_null() && !r[2].is_null() && l[2] != r[2];
        (expr, check)
    }

    fn random_rows(rng: &mut Rng, n: usize, keys: i64, null_p: f64) -> Rows {
        (0..n)
            .map(|_| {
                let k = if rng.next_bool(null_p) {
                    Value::Null
                } else {
                    Value::Int64(rng.range_i64(0, keys - 1))
                };
                let s = if rng.next_bool(null_p) {
                    Value::Null
                } else {
                    Value::varchar(format!("s{}", rng.below(6)))
                };
                let v = if rng.next_bool(null_p) {
                    Value::Null
                } else {
                    Value::Int64(rng.range_i64(-3, 3))
                };
                vec![k, s, v]
            })
            .collect()
    }

    /// `rows` cut into chunks of random sizes, each one a morsel.
    fn chunks(rows: &[Vec<Value>], seed: u64) -> Vec<Chunk> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::new();
        let mut start = 0;
        while start < rows.len() {
            let len = (rng.range_i64(1, 120) as usize).min(rows.len() - start);
            out.push(Chunk::from_rows(&TYPES, &rows[start..start + len]).unwrap());
            start += len;
        }
        out
    }

    fn sorted(mut rows: Rows) -> Rows {
        rows.sort();
        rows
    }

    fn run_join(
        left: &[Vec<Value>],
        right: &[Vec<Value>],
        kind: JoinKind,
        keys: &[(usize, usize)],
        residual: Option<&BoundExpr>,
        threads: usize,
    ) -> Result<Rows> {
        let db = Database::open("unused");
        let stats = ExecStats::default();
        let ctx = ExecContext {
            db: &db,
            threads,
            stats: &stats,
            prune: true,
            profile: None,
        };
        let key_exprs: Vec<(BoundExpr, BoundExpr)> =
            keys.iter().map(|&(l, r)| (col(l, 0), col(r, 1))).collect();
        let right_pipeline = Pipeline::new(Source::chunks(chunks(right, 1)));
        let probe = build(
            right_pipeline,
            kind,
            &key_exprs,
            residual,
            WIDTH,
            TYPES.to_vec(),
            &ctx,
        )?;
        if probe.produces_nothing() {
            return Ok(Vec::new());
        }
        let mut left_pipeline = Pipeline::new(Source::chunks(chunks(left, 2)));
        left_pipeline.push_op(StreamOp::Join(Box::new(probe)));
        let out = collect(left_pipeline, &ctx, None)?;
        Ok(sorted(
            out.chunks
                .iter()
                .flat_map(|c| (0..c.len).map(move |i| c.row(i)))
                .collect(),
        ))
    }

    /// The join by its definition: every left row against every right row.
    fn naive(
        left: &[Vec<Value>],
        right: &[Vec<Value>],
        kind: JoinKind,
        keys: &[(usize, usize)],
        residual: Option<Check>,
    ) -> Result<Rows> {
        let pair = |l: &[Value], r: &[Value]| {
            keys.iter()
                .all(|&(a, b)| !l[a].is_null() && !r[b].is_null() && l[a] == r[b])
                && residual.is_none_or(|f| f(l, r))
        };
        let nulls = vec![Value::Null; WIDTH];
        let mut out = Vec::new();
        if kind == JoinKind::Single && keys.is_empty() && residual.is_none() && right.len() > 1 {
            return Err(single_error());
        }
        for l in left {
            let l = l.as_slice();
            let matches: Vec<&[Value]> = right
                .iter()
                .map(Vec::as_slice)
                .filter(|&r| pair(l, r))
                .collect();
            match kind {
                JoinKind::Inner | JoinKind::Cross => {
                    out.extend(matches.iter().map(|&r| [l, r].concat()));
                }
                JoinKind::Left | JoinKind::Single => {
                    if kind == JoinKind::Single && matches.len() > 1 {
                        return Err(single_error());
                    }
                    if matches.is_empty() {
                        out.push([l, nulls.as_slice()].concat());
                    } else {
                        out.extend(matches.iter().map(|&r| [l, r].concat()));
                    }
                }
                JoinKind::Semi if !matches.is_empty() => out.push(l.to_vec()),
                JoinKind::Anti if matches.is_empty() => out.push(l.to_vec()),
                JoinKind::NullAwareAnti => {
                    let (a, b) = keys[0];
                    let passes = right.is_empty()
                        || (!right.iter().any(|r| r[b].is_null())
                            && !l[a].is_null()
                            && matches.is_empty());
                    if passes {
                        out.push(l.to_vec());
                    }
                }
                _ => {}
            }
        }
        Ok(sorted(out))
    }

    /// Checks the hash or nested loop join against [`naive`] (rows as a multiset, or the
    /// same error) with 1 and 8 threads.
    fn check(
        left: &[Vec<Value>],
        right: &[Vec<Value>],
        kind: JoinKind,
        keys: &[(usize, usize)],
        residual: Option<(&BoundExpr, Check)>,
    ) {
        let expected = naive(left, right, kind, keys, residual.map(|r| r.1));
        let mut results = Vec::new();
        for threads in [1, 8] {
            let got = run_join(left, right, kind, keys, residual.map(|r| r.0), threads);
            let what = format!(
                "{kind:?} keys {keys:?} residual {} threads {threads}",
                residual.is_some()
            );
            if let (Ok(got), Ok(expected)) = (&got, &expected) {
                assert_eq!(got.len(), expected.len(), "{what}");
            }
            assert_eq!(got, expected, "{what}");
            results.push(got);
        }
        assert_eq!(results[0], results[1]);
    }

    /// Key sets of every layout: packed `u64`, string, composite and packed `u128`.
    const KEY_SETS: [&[(usize, usize)]; 4] =
        [&[(0, 0)], &[(1, 1)], &[(0, 0), (1, 1)], &[(0, 0), (2, 2)]];

    #[test]
    fn hash_joins_match_a_naive_nested_loop_with_null_and_repeated_keys() {
        let mut rng = Rng::new(42);
        let left = random_rows(&mut rng, 600, 60, 0.15);
        let right = random_rows(&mut rng, 400, 60, 0.15);
        let (residual, residual_check) = not_equal_v();
        for keys in KEY_SETS {
            for kind in [
                JoinKind::Inner,
                JoinKind::Left,
                JoinKind::Semi,
                JoinKind::Anti,
            ] {
                check(&left, &right, kind, keys, None);
                check(&left, &right, kind, keys, Some((&residual, residual_check)));
            }
        }
    }

    #[test]
    fn string_keys_without_nulls_probe_straight_from_the_vector() {
        // No NULL on the left: the probe hashes the strings of the batch in place.
        let mut rng = Rng::new(43);
        let left = random_rows(&mut rng, 700, 30, 0.0);
        let right = random_rows(&mut rng, 300, 30, 0.1);
        let (residual, residual_check) = not_equal_v();
        for kind in [
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Semi,
            JoinKind::Anti,
            JoinKind::Single,
        ] {
            let right_rows: &[Vec<Value>] = if kind == JoinKind::Single {
                // One right row per key, so that `Single` has no duplicate match.
                &right[..1]
            } else {
                &right
            };
            check(&left, right_rows, kind, &[(1, 1)], None);
            check(
                &left,
                right_rows,
                kind,
                &[(1, 1)],
                Some((&residual, residual_check)),
            );
        }
    }

    #[test]
    fn joins_with_an_empty_side() {
        let mut rng = Rng::new(7);
        let rows = random_rows(&mut rng, 300, 20, 0.2);
        let (residual, residual_check) = not_equal_v();
        for kind in [
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Semi,
            JoinKind::Anti,
            JoinKind::NullAwareAnti,
            JoinKind::Single,
        ] {
            check(&rows, &[], kind, &[(0, 0)], None);
            check(&[], &rows, kind, &[(0, 0)], None);
        }
        for kind in [JoinKind::Cross, JoinKind::Left, JoinKind::Anti] {
            check(&rows, &[], kind, &[], Some((&residual, residual_check)));
        }
    }

    #[test]
    fn null_aware_anti_join_follows_not_in() {
        let mut rng = Rng::new(3);
        let left = random_rows(&mut rng, 500, 40, 0.2);
        let with_nulls = random_rows(&mut rng, 100, 40, 0.1);
        assert!(with_nulls.iter().any(|r| r[0].is_null()));
        let without_nulls: Rows = with_nulls
            .iter()
            .filter(|r| !r[0].is_null())
            .cloned()
            .collect();
        check(&left, &with_nulls, JoinKind::NullAwareAnti, &[(0, 0)], None);
        check(
            &left,
            &without_nulls,
            JoinKind::NullAwareAnti,
            &[(0, 0)],
            None,
        );
        check(&left, &[], JoinKind::NullAwareAnti, &[(0, 0)], None);
        // With a NULL on the right nothing passes; with an empty right everything does.
        assert!(
            run_join(
                &left,
                &with_nulls,
                JoinKind::NullAwareAnti,
                &[(0, 0)],
                None,
                8
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            run_join(&left, &[], JoinKind::NullAwareAnti, &[(0, 0)], None, 8)
                .unwrap()
                .len(),
            left.len()
        );
        let got = run_join(
            &left,
            &without_nulls,
            JoinKind::NullAwareAnti,
            &[(0, 0)],
            None,
            1,
        )
        .unwrap();
        assert!(got.iter().all(|r| !r[0].is_null()));
    }

    #[test]
    fn single_join_takes_the_only_match_and_rejects_two() {
        let mut rng = Rng::new(11);
        let left = random_rows(&mut rng, 500, 60, 0.15);
        // Every non-NULL key once on the right, as after the grouping of a scalar subquery.
        let mut right: Rows = (0..60)
            .filter(|k| k % 3 != 0)
            .map(|k| {
                vec![
                    Value::Int64(k),
                    Value::varchar(format!("s{k}")),
                    Value::Int64(k % 4),
                ]
            })
            .collect();
        right.push(vec![Value::Null, Value::Null, Value::Int64(1)]);
        right.push(vec![Value::Null, Value::Null, Value::Int64(2)]);
        let (residual, residual_check) = not_equal_v();
        check(&left, &right, JoinKind::Single, &[(0, 0)], None);
        check(
            &left,
            &right,
            JoinKind::Single,
            &[(0, 0)],
            Some((&residual, residual_check)),
        );

        let mut twice = right.clone();
        twice.push(vec![
            Value::Int64(1),
            Value::varchar("again"),
            Value::Int64(9),
        ]);
        let left_with_one = vec![vec![Value::Int64(1), Value::Null, Value::Int64(0)]];
        for threads in [1, 8] {
            let err = run_join(
                &left_with_one,
                &twice,
                JoinKind::Single,
                &[(0, 0)],
                None,
                threads,
            )
            .unwrap_err();
            assert_eq!(err.exit_code(), 1);
            assert_eq!(err, single_error());
        }
        // A left row without a match never sees the duplicate.
        let left_elsewhere = vec![vec![Value::Int64(2), Value::Null, Value::Int64(0)]];
        assert_eq!(
            run_join(
                &left_elsewhere,
                &twice,
                JoinKind::Single,
                &[(0, 0)],
                None,
                8
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn single_join_without_keys_is_a_scalar_value_evaluated_once() {
        let mut rng = Rng::new(5);
        let left = random_rows(&mut rng, 200, 10, 0.2);
        let one = random_rows(&mut rng, 1, 10, 0.0);
        let two = random_rows(&mut rng, 2, 10, 0.0);
        check(&left, &one, JoinKind::Single, &[], None);
        check(&left, &[], JoinKind::Single, &[], None);
        for threads in [1, 8] {
            assert_eq!(
                run_join(&left, &two, JoinKind::Single, &[], None, threads).unwrap_err(),
                single_error()
            );
            assert_eq!(
                run_join(&[], &two, JoinKind::Single, &[], None, threads).unwrap_err(),
                single_error()
            );
        }
    }

    #[test]
    fn nested_loop_joins_without_keys() {
        let mut rng = Rng::new(9);
        let left = random_rows(&mut rng, 150, 10, 0.2);
        let right = random_rows(&mut rng, 90, 10, 0.2);
        let (residual, residual_check) = not_equal_v();
        check(&left, &right, JoinKind::Cross, &[], None);
        for kind in [
            JoinKind::Cross,
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Semi,
            JoinKind::Anti,
            JoinKind::Single,
        ] {
            if kind != JoinKind::Single {
                check(&left, &right, kind, &[], None);
            }
            check(&left, &right, kind, &[], Some((&residual, residual_check)));
        }
    }

    #[test]
    fn one_probe_batch_with_more_pairs_than_a_chunk() {
        // 200 × 300 pairs of the same key: every probe batch of more than 27 rows flushes
        // its pairs more than once, and the flushes cut the candidates of a left row.
        let left: Rows = (0..200)
            .map(|i| vec![Value::Int64(1), Value::varchar("x"), Value::Int64(i % 3)])
            .collect();
        let right: Rows = (0..300)
            .map(|i| vec![Value::Int64(1), Value::varchar("x"), Value::Int64(i % 5)])
            .collect();
        assert!(30 * right.len() > CHUNK_ROWS);
        let (residual, residual_check) = not_equal_v();
        for kind in [
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Semi,
            JoinKind::Anti,
        ] {
            check(&left, &right, kind, &[(0, 0)], None);
            check(
                &left,
                &right,
                kind,
                &[(0, 0)],
                Some((&residual, residual_check)),
            );
        }
        assert_eq!(
            run_join(&left, &right, JoinKind::Inner, &[(0, 0)], None, 8)
                .unwrap()
                .len(),
            60_000
        );
        assert_eq!(
            run_join(&left, &right, JoinKind::Single, &[(0, 0)], None, 8).unwrap_err(),
            single_error()
        );
    }

    #[test]
    fn residual_alone_can_reject_every_pair() {
        // Q21: the only other supplier of the order is the same one, so `<>` rejects it.
        let left = vec![vec![Value::Int64(7), Value::Null, Value::Int64(1)]];
        let right = vec![
            vec![Value::Int64(7), Value::Null, Value::Int64(1)],
            vec![Value::Int64(7), Value::Null, Value::Null],
        ];
        let (residual, residual_check) = not_equal_v();
        for kind in [
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Semi,
            JoinKind::Anti,
        ] {
            check(
                &left,
                &right,
                kind,
                &[(0, 0)],
                Some((&residual, residual_check)),
            );
        }
        assert_eq!(
            run_join(&left, &right, JoinKind::Anti, &[(0, 0)], Some(&residual), 1).unwrap(),
            left
        );
    }

    #[test]
    fn build_accounts_its_memory() {
        let mut rng = Rng::new(1);
        let right = random_rows(&mut rng, 2000, 100, 0.1);
        let db = Database::open("unused");
        let stats = ExecStats::default();
        let ctx = ExecContext {
            db: &db,
            threads: 4,
            stats: &stats,
            prune: true,
            profile: None,
        };
        let keys = vec![(col(0, 0), col(0, 1))];
        let probe = build(
            Pipeline::new(Source::chunks(chunks(&right, 3))),
            JoinKind::Inner,
            &keys,
            None,
            WIDTH,
            TYPES.to_vec(),
            &ctx,
        )
        .unwrap();
        let held: u64 = probe._mem.iter().map(Reservation::size).sum::<u64>()
            + probe.partitions.iter().map(|p| p._mem.size()).sum::<u64>();
        assert!(held >= chunk_bytes(&probe.build));
        assert!(
            probe
                .partitions
                .iter()
                .all(|p| p._mem.size() >= p.table.bytes())
        );
        assert_eq!(probe.partitions.len(), PARTITIONS);
        let rows: usize = probe.partitions.iter().map(|p| p.rows.len()).sum();
        assert_eq!(rows, probe.build.len);
        assert_eq!(
            probe.build.len,
            right.iter().filter(|r| !r[0].is_null()).count()
        );
    }

    #[test]
    fn mismatched_key_types_are_an_internal_error() {
        let db = Database::open("unused");
        let stats = ExecStats::default();
        let ctx = ExecContext {
            db: &db,
            threads: 1,
            stats: &stats,
            prune: true,
            profile: None,
        };
        let keys = vec![(col(0, 0), col(1, 1))];
        let err = build(
            Pipeline::new(Source::chunks(Vec::new())),
            JoinKind::Inner,
            &keys,
            None,
            WIDTH,
            TYPES.to_vec(),
            &ctx,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
