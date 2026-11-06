"""Parquet interoperability (contract §9, §17): `parquet-read-duckdb`, `parquet-read-hits`,
`parquet-write-duckdb`, `parquet-write-clickhouse`, `parquet-stats` and `parquet-rowgroup-skip`.

Reading: DuckDB writes, veta reads, and both `SELECT *` must be the same multiset. Writing:
`veta export` writes `hits` and the 8 SF1 tables, and each oracle compares the file with its
own table by `EXCEPT ALL` in both directions. Every file lives under `$VETA_DATA/scratch/<name>/`
and each command writes its own; the SQL builders and parsers are pure functions.
"""

import itertools
from dataclasses import dataclass
from pathlib import Path

from harness import compare, tpch
from harness.clickbench import TABLE as HITS
from harness.clickbench import hits_files, sql_string
from harness.config import PARQUET_TESTING_PINS, Paths
from harness.durability import fresh_scratch
from harness.engines import EngineRun, Engines, clickbench_databases, split_rows, tpch_databases, veta_stats
from harness.normalize import cells_equal, normalize_tsv_cell, parse_output, render, unescape_tsv
from harness.report import FAIL, PASS, Infra, Report

EQUAL, DIFFERENT, ERROR = compare.EQUAL, compare.DIFFERENT, compare.ERROR


class CheckError(Exception):
    """One file or query could not be checked; its item becomes an error."""


def ident(name: str) -> str:
    """A double-quoted identifier, valid in veta, DuckDB and ClickHouse."""
    return '"' + name.replace('"', '""') + '"'


def duckdb_source(file: Path, row_numbers: bool = False) -> str:
    """`read_parquet` in DuckDB; byte arrays without annotation read as `VARCHAR`, like veta."""
    options = ", binary_as_string=true" + (", file_row_number=true" if row_numbers else "")
    return f"read_parquet({sql_string(str(file))}{options})"


def veta_source(file: Path) -> str:
    return f"read_parquet({sql_string(str(file))})"


def clickhouse_source(file: Path) -> str:
    return f"file({sql_string(str(file))}, Parquet)"


def single_row(run: EngineRun, what: str) -> tuple:
    """The only row of a successful run, normalized (§15.1)."""
    if not run.ok:
        raise CheckError(f"{what}: {run.engine} exited {run.code}: {run.error_tail()}")
    rows = parse_output(run.engine, run.text)
    if len(rows) != 1:
        raise CheckError(f"{what}: {run.engine} printed {len(rows)} rows, expected 1")
    return rows[0]


def query_db(scratch: Path) -> Path:
    """A veta database directory for `read_parquet` queries, which touch no table."""
    db = scratch / "veta-db"
    db.mkdir(parents=True, exist_ok=True)
    return db


# parquet-meta and parquet_metadata()


@dataclass(frozen=True)
class Chunk:
    """One column chunk: what the footer says, as `parquet-meta` or `parquet_metadata()` print it."""

    row_group: int
    column: str
    physical_type: str
    encodings: tuple[str, ...]
    compression: str
    min: object
    max: object
    null_count: int | None
    rows: int | None = None


META_FIELDS = 8


def split_encodings(text: str) -> tuple[str, ...]:
    """`PLAIN,RLE_DICTIONARY`, `PLAIN, RLE` or `[PLAIN, RLE]` as a tuple of names."""
    text = text.strip().removeprefix("[").removesuffix("]")
    return tuple(e.strip() for e in text.split(",") if e.strip())


def null_count_cell(value, where: str) -> int | None:
    if value is not None and not isinstance(value, int):
        raise ValueError(f"{where}: null_count is not an integer: {render(value)}")
    return value


def parse_parquet_meta(text: str) -> list[Chunk]:
    """The TSV of `veta parquet-meta` (§13): `row_group, column, physical type, encodings,
    compression, min, max, null_count`, values in the format of §13.1."""
    chunks = []
    for n, row in enumerate(split_rows(text), 1):
        cells = row.split("\t")
        if len(cells) != META_FIELDS:
            raise ValueError(f"parquet-meta row {n} has {len(cells)} cells, expected {META_FIELDS}")
        group, column, physical, encodings, compression, lo, hi, nulls = cells
        if not group.isdigit():
            raise ValueError(f"parquet-meta row {n}: row group is not an integer: {group!r}")
        chunks.append(Chunk(
            row_group=int(group),
            column=unescape_tsv(column),
            physical_type=unescape_tsv(physical),
            encodings=split_encodings(unescape_tsv(encodings)),
            compression=unescape_tsv(compression),
            min=normalize_tsv_cell(lo),
            max=normalize_tsv_cell(hi),
            null_count=null_count_cell(normalize_tsv_cell(nulls), f"parquet-meta row {n}"),
        ))
    return chunks


DUCKDB_META_COLUMNS = (
    "row_group_id", "path_in_schema", "type", "encodings", "compression",
    "stats_min_value", "stats_max_value", "stats_null_count", "row_group_num_rows",
)


def duckdb_metadata_sql(file: Path) -> str:
    return (f"SELECT {', '.join(DUCKDB_META_COLUMNS)} FROM parquet_metadata({sql_string(str(file))}) "
            "ORDER BY row_group_id, column_id;")


def parse_duckdb_metadata(text: str) -> list[Chunk]:
    """The CSV of `duckdb_metadata_sql`, one chunk per row."""
    chunks = []
    for n, row in enumerate(parse_output("duckdb", text), 1):
        if len(row) != len(DUCKDB_META_COLUMNS):
            raise ValueError(f"parquet_metadata row {n} has {len(row)} cells, expected {len(DUCKDB_META_COLUMNS)}")
        group, column, physical, encodings, compression, lo, hi, nulls, rows = row
        if not isinstance(group, int) or not isinstance(rows, int):
            raise ValueError(f"parquet_metadata row {n}: row group or row count is not an integer")
        chunks.append(Chunk(
            row_group=group,
            column=str(column),
            physical_type=str(physical),
            encodings=split_encodings("" if encodings is None else str(encodings)),
            compression="" if compression is None else str(compression),
            min=lo,
            max=hi,
            null_count=null_count_cell(nulls, f"parquet_metadata row {n}"),
            rows=rows,
        ))
    return chunks


def columns_of(chunks: list[Chunk]) -> list[str]:
    """Column names in file order, from the first row group."""
    return [c.column for c in chunks if c.row_group == 0]


def row_group_rows(chunks: list[Chunk]) -> list[int]:
    """Rows of each row group, in order, from `parquet_metadata()` chunks."""
    rows = {}
    for c in chunks:
        rows.setdefault(c.row_group, c.rows)
    if sorted(rows) != list(range(len(rows))):
        raise ValueError(f"row groups are not 0..{len(rows) - 1}: {sorted(rows)}")
    return [rows[g] for g in range(len(rows))]


def veta_meta(engines: Engines, report: Report, file: Path, item: str) -> list[Chunk]:
    run = engines.veta("parquet-meta", "--file", file)
    report.raw("veta", f"{item}.meta", run.stdout)
    if not run.ok:
        raise CheckError(f"veta parquet-meta exited {run.code}: {run.error_tail()}")
    try:
        return parse_parquet_meta(run.text)
    except ValueError as e:
        raise CheckError(str(e)) from e


def duckdb_meta(engines: Engines, report: Report, file: Path, item: str) -> list[Chunk]:
    run = engines.duckdb(None, duckdb_metadata_sql(file))
    report.raw("duckdb", f"{item}.metadata", run.stdout)
    if not run.ok:
        raise CheckError(f"duckdb parquet_metadata exited {run.code}: {run.error_tail()}")
    try:
        return parse_duckdb_metadata(run.text)
    except ValueError as e:
        raise CheckError(str(e)) from e


def file_item(file: Path, table: str, verdict: str, reason: str = "", **extra) -> dict:
    return {"file": str(file.resolve()), "table": table, "verdict": verdict, "reason": reason, **extra}


def equal_summary(items: list[dict]) -> tuple[int, str]:
    equal = sum(i["verdict"] == EQUAL for i in items)
    return equal, f"{equal}/{len(items)} equal"


# parquet-read-duckdb

# (name, COPY options) for each configuration of §17; `DICTIONARY_SIZE_LIMIT 1` drops the
# dictionary (RESEARCH §3).
READ_CONFIGS = (
    ("v1", ()),
    ("v1-nodict", (("DICTIONARY_SIZE_LIMIT", "1"),)),
    ("v2", (("PARQUET_VERSION", "v2"),)),
    ("v2-nodict", (("PARQUET_VERSION", "v2"), ("DICTIONARY_SIZE_LIMIT", "1"))),
)
READ_COMPRESSIONS = ("uncompressed", "snappy")
# Several row groups and pages per file, small enough to compare whole files in memory.
READ_ROW_GROUP_SIZE = 10_240
HITS_SAMPLE_ROWS = 20_000
LINEITEM_SAMPLE_MAX_ORDERKEY = 40_000

REQUIRED_ENCODINGS = (
    "PLAIN", "PLAIN_DICTIONARY", "RLE_DICTIONARY", "RLE", "DELTA_BINARY_PACKED",
    "DELTA_LENGTH_BYTE_ARRAY", "DELTA_BYTE_ARRAY", "BYTE_STREAM_SPLIT",
)
REQUIRED_COMPRESSIONS = ("UNCOMPRESSED", "SNAPPY")

HITS_SAMPLE_SQL = f"SELECT * FROM {HITS} ORDER BY WatchID LIMIT {HITS_SAMPLE_ROWS}"
# Derived nullable columns with NULL (definition levels in RLE) and DOUBLEs (BYTE_STREAM_SPLIT).
LINEITEM_SAMPLE_SQL = (
    "SELECT *, "
    "CAST(l_extendedprice AS DOUBLE) AS l_extendedprice_double, "
    "CASE WHEN l_linenumber % 3 = 0 THEN NULL ELSE l_quantity END AS l_quantity_nullable, "
    "CASE WHEN l_orderkey % 5 = 0 THEN NULL ELSE l_partkey END AS l_partkey_nullable, "
    "CASE WHEN l_linenumber % 2 = 0 THEN NULL ELSE l_comment END AS l_comment_nullable, "
    "CASE WHEN l_linenumber = 1 THEN NULL ELSE l_shipdate END AS l_shipdate_nullable, "
    "CASE WHEN l_linenumber = 2 THEN NULL ELSE CAST(l_discount AS DOUBLE) END AS l_discount_double_nullable "
    f"FROM lineitem WHERE l_orderkey <= {LINEITEM_SAMPLE_MAX_ORDERKEY}"
)


def copy_options(config: tuple, compression: str, row_group_size: int = READ_ROW_GROUP_SIZE) -> list[tuple[str, str]]:
    return [("FORMAT", "parquet"), ("COMPRESSION", compression), ("ROW_GROUP_SIZE", str(row_group_size)), *config]


def duckdb_copy_sql(select: str, file: Path, options: list[tuple[str, str]]) -> str:
    """`COPY (<select>) TO '<file>' (FORMAT parquet, …)`."""
    listing = ", ".join(f"{k} {v}" for k, v in options)
    return f"COPY ({select}) TO {sql_string(str(file))} ({listing});"


def covered(chunks: list[Chunk]) -> tuple[set[str], set[str]]:
    """Encodings and compressions of the chunks. DuckDB lists only the data encoding, so RLE
    counts as covered when a chunk has NULLs: its definition levels are RLE/bit-packed (§17)."""
    encodings = {e for c in chunks for e in c.encodings}
    if any((c.null_count or 0) > 0 for c in chunks):
        encodings.add("RLE")
    return encodings, {c.compression for c in chunks if c.compression}


def read_duckdb_verdict(items: list[dict], encodings: set[str], compressions: set[str]) -> tuple[str, str]:
    equal, summary = equal_summary(items)
    missing = [e for e in REQUIRED_ENCODINGS if e not in encodings]
    missing += [c for c in REQUIRED_COMPRESSIONS if c not in compressions]
    covered_count = len(REQUIRED_ENCODINGS) - sum(e in missing for e in REQUIRED_ENCODINGS)
    summary += f", encodings {covered_count}/{len(REQUIRED_ENCODINGS)}"
    if missing:
        summary += f", missing {', '.join(missing)}"
    good = bool(items) and equal == len(items) and not missing
    return (PASS if good else FAIL), summary


def write_duckdb_files(engines: Engines, report: Report, dest: Path, p: Paths) -> list[tuple[str, Path, str | None]]:
    """The 16 DuckDB files: `(table, path, error)`; `error` is set if the COPY failed."""
    sources = (
        (HITS, clickbench_databases(p).duckdb, HITS_SAMPLE_SQL),
        ("lineitem", tpch_databases(p, 1).duckdb, LINEITEM_SAMPLE_SQL),
    )
    files = []
    for (table, db, select), (config, options), compression in itertools.product(
            sources, READ_CONFIGS, READ_COMPRESSIONS):
        file = dest / f"{table}-{config}-{compression}.parquet"
        run = engines.duckdb(db, duckdb_copy_sql(select, file, copy_options(options, compression)))
        report.raw("duckdb", f"{file.stem}.copy", run.stdout)
        error = None if run.ok else f"duckdb COPY exited {run.code}: {run.error_tail()}"
        files.append((table, file, error))
        report.progress(f"{file.name}: {'written' if error is None else error}")
    return files


def parquet_read_duckdb(args, p: Paths, report: Report):
    """DuckDB writes each configuration, veta reads it: `SELECT *` equal as multisets."""
    engines = Engines(p)
    engines.require("veta")
    engines.require("duckdb")
    sources = [clickbench_databases(p).duckdb, tpch_databases(p, 1).duckdb]
    for db in sources:
        if not db.exists():
            raise Infra(f"duckdb: {db} not found; run python3 -m harness load clickbench and load tpch --sf 1")
    testing = [p.data / pin.dest for pin in PARQUET_TESTING_PINS]
    for f in testing:
        if not f.is_file():
            raise Infra(f"{f} missing; run python3 -m harness fetch")
    scratch = fresh_scratch(p, report)
    reader = query_db(scratch)
    (scratch / "files").mkdir()
    files = write_duckdb_files(engines, report, scratch / "files", p)
    files += [("parquet-testing", f, None) for f in testing]
    all_chunks = []
    for table, file, error in files:
        item = file.stem
        encodings, compressions, nulls = [], [], 0
        verdict, reason, rows = ERROR, error or "", {}
        if error is None:
            try:
                chunks = duckdb_meta(engines, report, file, item)
                all_chunks += chunks
                found, codecs = covered(chunks)
                encodings, compressions = sorted(found), sorted(codecs)
                nulls = sum((c.null_count or 0) > 0 for c in chunks)
                veta = compare.Side("veta", lambda sql: engines.veta_sql(reader, sql), "veta")
                duck = compare.Side("duckdb", lambda sql: engines.duckdb(None, sql), "duckdb")
                c = compare.verify_query(item, veta, duck, f"SELECT * FROM {veta_source(file)}",
                                         f"SELECT * FROM {duckdb_source(file)};", raw=report.raw)
                verdict, reason, rows = c.verdict, c.reason, {"veta": c.rows_engine, "duckdb": c.rows_ref}
            except CheckError as e:
                reason = str(e)
        report.add(file_item(file, table, verdict, reason, encodings=encodings, compressions=compressions,
                             chunks_with_nulls=nulls, rows=rows))
        report.progress(f"{item} {verdict} {' '.join(encodings)} {reason}".rstrip())
    encodings, compressions = covered(all_chunks)
    report.fields.update(encodings_covered=sorted(encodings), compressions_covered=sorted(compressions))
    return read_duckdb_verdict(report.items, encodings, compressions)


# parquet-read-hits

NUMERIC_TYPES = ("INT32", "INT64", "FLOAT", "DOUBLE")
STRING_TYPES = ("BYTE_ARRAY",)


def hits_columns(chunks: list[Chunk]) -> list[tuple[str, str]]:
    """`(column, "numeric"|"string")` of the first row group, in file order; other physical
    types have no sum to compare and are left out."""
    columns = []
    for c in chunks:
        if c.row_group != 0:
            continue
        if c.physical_type in NUMERIC_TYPES:
            columns.append((c.column, "numeric"))
        elif c.physical_type in STRING_TYPES:
            columns.append((c.column, "string"))
    return columns


def read_hits_sql(columns: list[tuple[str, str]], source: str, dialect: str) -> str:
    """`count(*)`, `sum` of each numeric column and the byte length sum of each string one.
    DuckDB's `length` counts characters, so it uses `strlen` (bytes), as veta's `length` does."""
    length = "strlen" if dialect == "duckdb" else "length"
    exprs = ["count(*)"]
    for name, kind in columns:
        exprs.append(f"sum({ident(name)})" if kind == "numeric" else f"sum({length}({ident(name)}))")
    return f"SELECT {', '.join(exprs)} FROM {source}"


def aggregate_labels(columns: list[tuple[str, str]]) -> list[str]:
    return ["count(*)"] + [f"sum({n})" if k == "numeric" else f"sum(length({n}))" for n, k in columns]


def cell_differences(labels: list[str], veta: tuple, ref: tuple) -> list[str]:
    """`label: veta X != ref Y` for each cell that differs (§15.1 equality)."""
    if len(veta) != len(labels) or len(ref) != len(labels):
        return [f"width veta {len(veta)} / reference {len(ref)} != {len(labels)} aggregates"]
    return [f"{label}: veta {render(a)} != {render(b)}"
            for label, a, b in zip(labels, veta, ref) if not cells_equal(a, b)]


def parquet_read_hits(args, p: Paths, report: Report):
    """Per `hits_N.parquet`: `count(*)`, sums and string lengths in veta equal to DuckDB."""
    engines = Engines(p)
    engines.require("veta")
    engines.require("duckdb")
    files = hits_files(p)
    for f in files:
        if not f.is_file():
            raise Infra(f"{f} missing; run python3 -m harness fetch")
    db = query_db(fresh_scratch(p, report))
    for file in files:
        item = file.stem
        verdict, reason, columns = ERROR, "", []
        try:
            columns = hits_columns(veta_meta(engines, report, file, item))
            if not columns:
                raise CheckError("no numeric or string column in the footer")
            veta = engines.veta_sql(db, read_hits_sql(columns, veta_source(file), "veta"))
            duck = engines.duckdb(None, read_hits_sql(columns, duckdb_source(file), "duckdb") + ";")
            report.raw("veta", item, veta.stdout)
            report.raw("duckdb", item, duck.stdout)
            diffs = cell_differences(aggregate_labels(columns), single_row(veta, "veta"), single_row(duck, "duckdb"))
            verdict = DIFFERENT if diffs else EQUAL
            reason = "; ".join(diffs[:10]) + (f"; {len(diffs) - 10} more" if len(diffs) > 10 else "")
        except CheckError as e:
            reason = str(e)
        report.add(file_item(file, HITS, verdict, reason, aggregates=len(columns) + 1 if columns else 0))
        report.progress(f"{item} {verdict} {reason}".rstrip())
    return (PASS if equal_summary(report.items)[0] == len(files) else FAIL), equal_summary(report.items)[1]


# veta export: the files of parquet-write-*, parquet-stats


@dataclass(frozen=True)
class Export:
    """One `veta export` of a whole table (§17)."""

    table: str
    bench: str  # "clickbench" or "tpch"
    encoding: str
    compression: str

    @property
    def name(self) -> str:
        return f"{self.table}-{self.encoding}-{self.compression}"


HITS_EXPORTS = (("dict", "snappy"), ("delta", "none"))
TPCH_ENCODINGS = ("plain", "dict", "delta", "delta-byte-array")
TPCH_COMPRESSIONS = ("none", "snappy")


def write_exports() -> list[Export]:
    """`hits` with dict+snappy and delta+none; each SF1 table with the 4 encodings × 2 compressions."""
    exports = [Export(HITS, "clickbench", e, c) for e, c in HITS_EXPORTS]
    exports += [Export(t, "tpch", e, c) for t in tpch.TABLES for e in TPCH_ENCODINGS for c in TPCH_COMPRESSIONS]
    return exports


def export_argv(db: Path, sql: str, out: Path, encoding: str | None = None, compression: str | None = None,
                row_group_rows: int | None = None) -> list:
    argv = ["export", "--db", db, "--sql", sql, "--out", out]
    if encoding is not None:
        argv += ["--encoding", encoding]
    if compression is not None:
        argv += ["--compression", compression]
    if row_group_rows is not None:
        argv += ["--row-group-rows", row_group_rows]
    return argv


def veta_databases(p: Paths) -> dict[str, Path]:
    return {"clickbench": clickbench_databases(p).veta, "tpch": tpch_databases(p, 1).veta}


def require_veta_sources(engines: Engines, p: Paths) -> dict[str, Path]:
    engines.require("veta")
    dbs = veta_databases(p)
    for bench, db in dbs.items():
        if not db.exists():
            load = "load clickbench" if bench == "clickbench" else "load tpch --sf 1"
            raise Infra(f"veta: {db} not found; run python3 -m harness {load}")
    return dbs


def export_all(engines: Engines, report: Report, dbs: dict[str, Path], dest: Path) -> list[tuple[Export, Path, str | None]]:
    """Runs every export of `write_exports()`: `(export, file, error)`."""
    dest.mkdir(parents=True, exist_ok=True)
    done = []
    for export in write_exports():
        file = dest / f"{export.name}.parquet"
        run = engines.veta(*export_argv(dbs[export.bench], f"SELECT * FROM {export.table}", file,
                                        export.encoding, export.compression))
        report.raw("veta", f"{export.name}.export", run.stdout)
        error = None if run.ok and file.is_file() else f"veta export exited {run.code}: {run.error_tail()}"
        done.append((export, file, error))
        report.progress(f"{file.name}: {'exported' if error is None else error}")
    return done


# EXCEPT ALL against the oracle's table


def match_columns(oracle: list[tuple[str, str]], file_columns: list[str]) -> list[tuple[str, str, str]]:
    """`(file column, oracle column, oracle type)` in the oracle's order, matched by name
    without case; the file must hold exactly the oracle's columns."""
    by_lower = {}
    for name in file_columns:
        if name.lower() in by_lower:
            raise CheckError(f"the file has two columns named {name.lower()!r}")
        by_lower[name.lower()] = name
    matched = []
    for name, type_ in oracle:
        found = by_lower.pop(name.lower(), None)
        if found is None:
            raise CheckError(f"the file has no column {name!r}")
        matched.append((found, name, type_))
    if by_lower:
        raise CheckError(f"the file has extra columns: {', '.join(sorted(by_lower.values()))}")
    return matched


def file_select(columns: list[tuple[str, str, str]], source: str) -> str:
    """Each file column cast to the oracle's type, e.g. a `Timestamp` in MILLIS to `TIMESTAMP`."""
    exprs = ", ".join(f"CAST({ident(f)} AS {t}) AS {ident(o)}" for f, o, t in columns)
    return f"SELECT {exprs} FROM {source}"


def table_select(columns: list[tuple[str, str, str]], table: str) -> str:
    return f"SELECT {', '.join(ident(o) for _, o, _ in columns)} FROM {table}"


def except_all_sql(left: str, right: str) -> str:
    """Rows of `left` missing from `right` and of `right` missing from `left`, as one row."""
    return (f"SELECT (SELECT count(*) FROM ({left} EXCEPT ALL {right}) AS only_file), "
            f"(SELECT count(*) FROM ({right} EXCEPT ALL {left}) AS only_table)")


def except_counts(row: tuple) -> tuple[int, int]:
    if len(row) != 2 or not all(isinstance(v, int) for v in row):
        raise CheckError(f"EXCEPT ALL counts are not two integers: {row!r}")
    return row[0], row[1]


def except_verdict(only_file: int, only_table: int) -> tuple[str, str]:
    if only_file == 0 and only_table == 0:
        return EQUAL, ""
    return DIFFERENT, f"{only_file} rows only in the file, {only_table} only in the table"


def duckdb_columns_sql(table: str) -> str:
    return ("SELECT column_name, data_type FROM information_schema.columns "
            f"WHERE table_name = {sql_string(table)} ORDER BY ordinal_position")


def clickhouse_columns_sql(database: str, table: str) -> str:
    return (f"SELECT name, type FROM system.columns WHERE database = {sql_string(database)} "
            f"AND table = {sql_string(table)} ORDER BY position")


def clickhouse_describe_sql(file: Path) -> str:
    return f"DESCRIBE TABLE {clickhouse_source(file)}"


def parse_columns(run: EngineRun, what: str) -> list[tuple[str, str]]:
    """`(name, type)` rows of a schema query; DESCRIBE prints more cells after these two."""
    if not run.ok:
        raise CheckError(f"{what}: {run.engine} exited {run.code}: {run.error_tail()}")
    if run.engine == "duckdb":
        rows = [tuple(str(c) for c in row) for row in parse_output("duckdb", run.text)]
    else:
        rows = [tuple(unescape_tsv(c) for c in row.split("\t")) for row in split_rows(run.text)]
    if not rows or any(len(r) < 2 for r in rows):
        raise CheckError(f"{what}: no columns")
    return [(r[0], r[1]) for r in rows]


class Oracle:
    """How one oracle reads a file and where its copy of each table lives."""

    def __init__(self, engines: Engines, p: Paths, engine: str):
        self.engines = engines
        self.engine = engine
        self.p = p
        self.schemas: dict[str, list[tuple[str, str]]] = {}

    def location(self, export: Export):
        dbs = clickbench_databases(self.p) if export.bench == "clickbench" else tpch_databases(self.p, 1)
        return dbs.duckdb if self.engine == "duckdb" else dbs.clickhouse_database

    def run(self, export: Export, sql: str, on_table: bool = True) -> EngineRun:
        if self.engine == "duckdb":
            return self.engines.duckdb(self.location(export) if on_table else None, sql + ";")
        return self.engines.clickhouse(sql, self.location(export) if on_table else None)

    def table_columns(self, export: Export) -> list[tuple[str, str]]:
        key = f"{export.bench}.{export.table}"
        if key not in self.schemas:
            if self.engine == "duckdb":
                run = self.run(export, duckdb_columns_sql(export.table))
            else:
                run = self.run(export, clickhouse_columns_sql(self.location(export), export.table))
            self.schemas[key] = parse_columns(run, f"columns of {export.table}")
        return self.schemas[key]

    def file_columns(self, export: Export, file: Path) -> list[str]:
        """DuckDB matches names without case itself; ClickHouse needs the file's spelling."""
        if self.engine == "duckdb":
            return [name for name, _ in self.table_columns(export)]
        return [name for name, _ in parse_columns(self.run(export, clickhouse_describe_sql(file), False),
                                                   f"DESCRIBE {file.name}")]

    def source(self, file: Path) -> str:
        return duckdb_source(file) if self.engine == "duckdb" else clickhouse_source(file)

    def check_sql(self, export: Export, file: Path) -> str:
        columns = match_columns(self.table_columns(export), self.file_columns(export, file))
        return except_all_sql(file_select(columns, self.source(file)), table_select(columns, export.table))


def require_oracle_tables(engines: Engines, p: Paths, engine: str) -> None:
    engines.require(engine)
    if engine == "duckdb":
        for db in (clickbench_databases(p).duckdb, tpch_databases(p, 1).duckdb):
            if not db.exists():
                raise Infra(f"duckdb: {db} not found; run python3 -m harness load clickbench and load tpch --sf 1")
    elif not (p.db / "clickhouse").exists():
        raise Infra(f"clickhouse: {p.db / 'clickhouse'} not found; run python3 -m harness load clickbench")


def parquet_write(engine: str, p: Paths, report: Report):
    """veta exports, the oracle reads: `EXCEPT ALL` in both directions = 0 rows per file."""
    engines = Engines(p)
    dbs = require_veta_sources(engines, p)
    require_oracle_tables(engines, p, engine)
    scratch = fresh_scratch(p, report)
    oracle = Oracle(engines, p, engine)
    for export, file, error in export_all(engines, report, dbs, scratch / "files"):
        verdict, reason, counts = ERROR, error or "", (None, None)
        if error is None:
            try:
                run = oracle.run(export, oracle.check_sql(export, file))
                report.raw(engine, export.name, run.stdout)
                counts = except_counts(single_row(run, "EXCEPT ALL"))
                verdict, reason = except_verdict(*counts)
            except CheckError as e:
                reason = str(e)
        report.add(file_item(file, export.table, verdict, reason, encoding=export.encoding,
                             compression=export.compression, only_in_file=counts[0], only_in_table=counts[1]))
        report.progress(f"{export.name} {verdict} {reason}".rstrip())
    equal, summary = equal_summary(report.items)
    return (PASS if report.items and equal == len(report.items) else FAIL), summary


def parquet_write_duckdb(args, p: Paths, report: Report):
    return parquet_write("duckdb", p, report)


def parquet_write_clickhouse(args, p: Paths, report: Report):
    return parquet_write("clickhouse", p, report)


# parquet-stats

ROW_GROUP_ALIAS = "veta_row_group"


def row_group_case(rows: list[int]) -> str:
    """The row group of `file_row_number` given the rows of each group, as a `CASE`."""
    if len(rows) <= 1:
        return "0"
    bounds = list(itertools.accumulate(rows))[:-1]
    whens = " ".join(f"WHEN file_row_number < {b} THEN {g}" for g, b in enumerate(bounds))
    return f"CASE {whens} ELSE {len(rows) - 1} END"


def row_group_stats_sql(file: Path, columns: list[str], rows: list[int]) -> str:
    """`min`, `max` and NULL count of each column per row group, computed by DuckDB over the rows."""
    aggs = ", ".join(f"min({c}), max({c}), count(*) - count({c})" for c in map(ident, columns))
    return (f"SELECT {ROW_GROUP_ALIAS}, {aggs} FROM (SELECT *, {row_group_case(rows)} AS {ROW_GROUP_ALIAS} "
            f"FROM {duckdb_source(file, row_numbers=True)}) AS t GROUP BY {ROW_GROUP_ALIAS} "
            f"ORDER BY {ROW_GROUP_ALIAS};")


def parse_row_group_stats(text: str, columns: list[str]) -> dict[tuple[int, str], tuple]:
    """`(row group, column) → (min, max, null_count)` from the output of `row_group_stats_sql`."""
    stats = {}
    for row in parse_output("duckdb", text):
        if len(row) != 1 + 3 * len(columns) or not isinstance(row[0], int):
            raise ValueError(f"row group stats row has {len(row)} cells, expected {1 + 3 * len(columns)}")
        for n, column in enumerate(columns):
            stats[(row[0], column)] = tuple(row[1 + 3 * n: 4 + 3 * n])
    return stats


def chunk_stats(chunks: list[Chunk]) -> dict[tuple[int, str], tuple]:
    return {(c.row_group, c.column): (c.min, c.max, c.null_count) for c in chunks}


def stats_differences(veta: dict, footer: dict, computed: dict) -> list[str]:
    """Every chunk where `parquet-meta`, `parquet_metadata()` and the rows disagree."""
    diffs = []
    keys = sorted(set(veta) | set(footer) | set(computed))
    names = ("min", "max", "null_count")
    for key in keys:
        group, column = key
        sides = {"veta": veta.get(key), "parquet_metadata": footer.get(key), "rows": computed.get(key)}
        missing = [name for name, value in sides.items() if value is None]
        if missing:
            diffs.append(f"rg {group} {column}: missing in {', '.join(missing)}")
            continue
        for i, name in enumerate(names):
            values = [v[i] for v in sides.values()]
            if not (cells_equal(values[0], values[1]) and cells_equal(values[0], values[2])):
                shown = ", ".join(f"{s} {render(v)}" for s, v in zip(sides, values))
                diffs.append(f"rg {group} {column} {name}: {shown}")
    return diffs


def check_stats(engines: Engines, report: Report, file: Path, item: str) -> tuple[int, list[str]]:
    """`(chunks, differences)` of one file."""
    veta = veta_meta(engines, report, file, item)
    footer = duckdb_meta(engines, report, file, item)
    try:
        rows = row_group_rows(footer)
    except ValueError as e:
        raise CheckError(str(e)) from e
    columns = columns_of(footer)
    run = engines.duckdb(None, row_group_stats_sql(file, columns, rows))
    report.raw("duckdb", f"{item}.rowgroups", run.stdout)
    if not run.ok:
        raise CheckError(f"duckdb row group stats exited {run.code}: {run.error_tail()}")
    try:
        computed = parse_row_group_stats(run.text, columns)
    except ValueError as e:
        raise CheckError(str(e)) from e
    return len(footer), stats_differences(chunk_stats(veta), chunk_stats(footer), computed)


def parquet_stats(args, p: Paths, report: Report):
    """For each chunk of the exported files: `parquet-meta` = `parquet_metadata()` = the rows."""
    engines = Engines(p)
    dbs = require_veta_sources(engines, p)
    engines.require("duckdb")
    scratch = fresh_scratch(p, report)
    for export, file, error in export_all(engines, report, dbs, scratch / "files"):
        verdict, reason, chunks = ERROR, error or "", None
        if error is None:
            try:
                chunks, diffs = check_stats(engines, report, file, export.name)
                verdict = DIFFERENT if diffs else EQUAL
                reason = "; ".join(diffs[:10]) + (f"; {len(diffs) - 10} more" if len(diffs) > 10 else "")
            except CheckError as e:
                reason = str(e)
        report.add(file_item(file, export.table, verdict, reason, encoding=export.encoding,
                             compression=export.compression, chunks=chunks))
        report.progress(f"{export.name} {verdict} {reason}".rstrip())
    equal, summary = equal_summary(report.items)
    return (PASS if report.items and equal == len(report.items) else FAIL), summary


# parquet-rowgroup-skip

SKIP_ROW_GROUP_ROWS = 100_000
SKIP_EXPORT_SQL = "SELECT * FROM lineitem ORDER BY l_shipdate"
# Selective filters on `l_shipdate`, which the sorted export makes prunable.
SKIP_FILTERS = (
    ("count(*)", "l_shipdate = DATE '1995-06-17'"),
    ("sum(l_extendedprice), min(l_orderkey)", "l_shipdate < DATE '1992-02-01'"),
    ("l_returnflag, count(*)", "l_shipdate > DATE '1998-11-01' GROUP BY l_returnflag"),
    ("sum(l_quantity), max(l_partkey)", "l_shipdate BETWEEN DATE '1994-01-01' AND DATE '1994-01-07'"),
    ("count(*), sum(l_discount)",
     "l_shipdate >= DATE '1996-12-30' AND l_shipdate < DATE '1997-01-02' AND l_discount > 0.05"),
)


def skip_queries(file: Path) -> list[tuple[str, str]]:
    source = veta_source(file)
    return [(f"s{n}", f"SELECT {select} FROM {source} WHERE {where}") for n, (select, where) in enumerate(SKIP_FILTERS, 1)]


def row_groups(run: EngineRun | None) -> tuple[int | None, int | None]:
    stats = veta_stats(run.stderr) if run is not None else None
    if not stats:
        return None, None
    read, total = stats.get("row_groups_read"), stats.get("row_groups_total")
    return (read if isinstance(read, int) else None), (total if isinstance(total, int) else None)


def skip_verdict(verdict: str, reason: str, read: int | None, total: int | None) -> tuple[str, str]:
    """Equal results, and fewer row groups read than the file has."""
    if verdict != EQUAL:
        return verdict, reason
    if read is None or total is None:
        return ERROR, "no VETA_STATS row_groups_read/row_groups_total on stderr"
    if read >= total:
        return DIFFERENT, f"row_groups_read {read} >= row_groups_total {total}"
    return EQUAL, ""


def parquet_rowgroup_skip(args, p: Paths, report: Report):
    """`lineitem` SF1 sorted by `l_shipdate` in groups of 100 000 rows: 5 selective queries skip
    row groups and return what `--no-prune` returns."""
    engines = Engines(p)
    db = require_veta_sources(engines, p)["tpch"]
    scratch = fresh_scratch(p, report)
    file = scratch / "lineitem-by-shipdate.parquet"
    run = engines.veta(*export_argv(db, SKIP_EXPORT_SQL, file, row_group_rows=SKIP_ROW_GROUP_ROWS))
    report.raw("veta", "export", run.stdout)
    if not run.ok or not file.is_file():
        raise Infra(f"veta export exited {run.code}: {run.error_tail()}")
    reader = query_db(scratch)
    prune = compare.Side("veta", lambda sql: engines.veta_sql(reader, sql, stats=True), "veta")
    noprune = compare.Side("veta", lambda sql: engines.veta_sql(reader, sql, stats=True, extra=["--no-prune"]),
                           "veta", suffix=".noprune")
    good = 0
    for qid, sql in skip_queries(file):
        c = compare.verify_query(qid, prune, noprune, sql, sql, raw=report.raw)
        read, total = row_groups(c.engine_run)
        verdict, reason = skip_verdict(c.verdict, c.reason, read, total)
        good += verdict == EQUAL
        report.add({"query": qid, "row_groups_read": read, "row_groups_total": total, "verdict": verdict,
                    "rule": c.rule, "reason": reason, "sql": sql})
        report.progress(f"{qid} row groups {read}/{total} {verdict} {reason}".rstrip())
    return (PASS if good == len(SKIP_FILTERS) else FAIL), f"{good}/{len(SKIP_FILTERS)} skipped and equal"
