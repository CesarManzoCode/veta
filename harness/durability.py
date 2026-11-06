"""Durability (contract §6, §7, §17): `merge-equivalence`, `concurrent-reader`, `crash-merge`,
`crash-insert` and `corruption`, every one on databases under `$VETA_DATA/scratch/<name>/`.

The veta side under test always runs against a database that something else changes (a merge,
a kill, a flipped byte); the reference is another database that nothing touches. Each command
keeps one item per query, iteration or file type with its raw observations, and the verdicts
are pure functions of them.
"""

import argparse
import math
import random
import re
import shutil
import signal
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from harness import clickbench, compare, gen
from harness.clickbench import TABLE, LoadError
from harness.config import HITS_PINS, Paths
from harness.engines import EngineRun, Engines, split_rows
from harness.report import FAIL, PASS, Infra, Report
from harness.tpch import step

# §16: fewer iterations than these never pass.
MIN_READER_ITERATIONS = 200
MIN_CRASH_ITERATIONS = 100
DEFAULT_READER_ITERATIONS = MIN_READER_ITERATIONS
DEFAULT_CRASH_ITERATIONS = MIN_CRASH_ITERATIONS
MIN_ALIVE_FRACTION = 0.8
# Kill instants and corrupted bytes come from fixed seeds, so a run can be repeated.
CRASH_SEED = 1
CORRUPTION_SEED = 1

MERGE_EQUIVALENCE_BATCH_ROWS = 100_000
MERGE_EQUIVALENCE_PARTS = 100
READER_BATCH_ROWS = 20_000
CRASH_MERGE_BATCHES = 20
# Q2, Q8, Q16, Q28 and Q31 of ClickBench (§17).
FIXED_QUERIES = (2, 8, 16, 28, 31)

INSERT_TABLE = "crash"
INSERT_CREATE = (
    f"CREATE TABLE {INSERT_TABLE} (batch_id BIGINT, i BIGINT, payload VARCHAR, PRIMARY KEY (batch_id, i))"
)
INSERT_BATCH_ROWS = 200_000
INSERT_COUNT_SQL = f"SELECT batch_id, count(*) FROM {INSERT_TABLE} GROUP BY batch_id"

CORRUPT_TABLE = "corrupt"
# (name, type); column N of the part files is the N-th one here.
CORRUPT_COLUMNS = (("k", "BIGINT NOT NULL"), ("v", "BIGINT"), ("s", "VARCHAR"))
CORRUPT_KEY = (0,)
CORRUPT_CREATE = (
    f"CREATE TABLE {CORRUPT_TABLE} ("
    + ", ".join(f"{n} {t}" for n, t in CORRUPT_COLUMNS)
    + f", PRIMARY KEY ({', '.join(CORRUPT_COLUMNS[c][0] for c in CORRUPT_KEY)}))"
)
CORRUPT_BATCHES = 4
CORRUPT_BATCH_ROWS = 20_000
# §17.1 names; `bin` and `mrk` stand for `c<N>.bin` and `c<N>.mrk`.
FILE_TYPES = ("part.txt", "primary.idx", "bin", "mrk", "stats.bin", "sample.bin", "checksums.txt")
COLUMN_FILE_TYPES = ("bin", "mrk")
EXIT_CORRUPT = 3
TRIAL_PASS, TRIAL_FAIL = "pass", "fail"

PART_RE = re.compile(r"all_(\d+)_(\d+)_(\d+)")


def iteration_count(value: str) -> int:
    """`--iterations`: a positive integer."""
    try:
        n = int(value)
    except ValueError:
        raise argparse.ArgumentTypeError(f"not an integer: {value}") from None
    if n <= 0:
        raise argparse.ArgumentTypeError(f"not a positive integer: {value}")
    return n


# Observations


def parse_parts(stdout: bytes) -> list[str]:
    """Names of the active parts in the output of `veta parts` (`name, rows, level, bytes`)."""
    return [row.split("\t")[0] for row in split_rows(stdout.decode("utf-8", errors="surrogateescape")) if row]


def part_blocks(name: str) -> tuple[int, int, int]:
    """`(min_block, max_block, level)` of `all_<min>_<max>_<level>`."""
    m = PART_RE.fullmatch(name)
    if not m:
        raise ValueError(f"not a part name: {name}")
    return int(m.group(1)), int(m.group(2)), int(m.group(3))


def tmp_entries(table_dir: Path) -> list[str]:
    """Every `tmp_*` left in a table directory; after `recover` there must be none (§6.5)."""
    if not table_dir.is_dir():
        return []
    return sorted(e.name for e in table_dir.iterdir() if e.name.startswith("tmp_"))


def detached_entries(table_dir: Path) -> list[str]:
    d = table_dir / "detached"
    return sorted(e.name for e in d.iterdir()) if d.is_dir() else []


def is_detached(table_dir: Path, part: str) -> bool:
    return any(name.startswith(f"broken_{part}") for name in detached_entries(table_dir))


def batch_counts(stdout: bytes) -> dict[int, int]:
    """`batch_id → rows` from `SELECT batch_id, count(*) … GROUP BY batch_id`."""
    counts = {}
    for row in split_rows(stdout.decode("utf-8", errors="surrogateescape")):
        batch, count = row.split("\t")
        counts[int(batch)] = int(count)
    return counts


def kill_delay(rng: random.Random, duration: float) -> float:
    """A uniform instant in `[0, T]`."""
    return rng.uniform(0.0, duration)


@dataclass
class Killed:
    """A process given SIGKILL after `delay` seconds; `alive` if it was still running then."""

    delay: float
    alive: bool
    code: int
    stdout: bytes
    stderr: bytes


def run_and_kill(argv: list[str], delay: float) -> Killed:
    proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        out, err = proc.communicate(timeout=delay)
        return Killed(delay, False, proc.returncode, out, err)
    except subprocess.TimeoutExpired:
        alive = proc.poll() is None
        if alive:
            proc.send_signal(signal.SIGKILL)
        out, err = proc.communicate()
        return Killed(delay, alive, proc.returncode, out, err)


def timed(argv: list[str]) -> tuple[float, subprocess.CompletedProcess]:
    start = time.perf_counter()
    done = subprocess.run(argv, capture_output=True)
    return time.perf_counter() - start, done


def alive_fraction(items: list[dict]) -> float:
    return sum(1 for i in items if i.get("alive")) / len(items) if items else 0.0


# Verdicts


def merge_equivalence_verdict(items: list[dict], parts_before: int, parts_after: int) -> tuple[str, str]:
    """All 43 equal, from 100 parts down to one active part."""
    equal = sum(1 for i in items if i.get("verdict") == compare.EQUAL)
    summary = f"{equal}/{clickbench.QUERY_COUNT} equal, parts {parts_before} -> {parts_after}"
    ok = (equal == clickbench.QUERY_COUNT == len(items) and parts_after == 1
          and parts_before == MERGE_EQUIVALENCE_PARTS)
    return (PASS if ok else FAIL), summary


def concurrent_verdict(iterations: int, mismatches: int, errors: int, parts_start: int | None,
                       parts_end: int | None, minimum: int = MIN_READER_ITERATIONS) -> tuple[str, str]:
    """No mismatch or error in ≥ `minimum` iterations, and the active parts went down."""
    summary = (f"{iterations} iterations, {mismatches} mismatches, {errors} errors, "
               f"parts {parts_start} -> {parts_end}")
    dropped = parts_start is not None and parts_end is not None and parts_end < parts_start
    ok = iterations >= minimum and mismatches == 0 and errors == 0 and dropped
    return (PASS if ok else FAIL), summary


def crash_verdict(iterations: int, failed: int, fraction: float, partial: int | None = None,
                  minimum: int = MIN_CRASH_ITERATIONS) -> tuple[str, str]:
    """No failed iteration in ≥ `minimum`, ≥ 80 % of cuts on a live process and, for
    `crash-insert`, no batch left with a row count other than its own."""
    summary = f"{iterations} iterations, {failed} failed, alive {fraction:.2f}"
    if partial is not None:
        summary += f", {partial} partial batches"
    ok = (iterations >= minimum and failed == 0 and fraction >= MIN_ALIVE_FRACTION
          and (partial is None or partial == 0))
    return (PASS if ok else FAIL), summary


def recovery_failures(recover_code: int, tmp_left: list[str], check_code: int) -> list[str]:
    """What §6.5 and §6.4 require after a cut: recover and check exit 0, no `tmp_*`."""
    reasons = []
    if recover_code != 0:
        reasons.append(f"recover exited {recover_code}")
    if tmp_left:
        reasons.append("tmp left: " + ", ".join(tmp_left))
    if check_code != 0:
        reasons.append(f"check exited {check_code}")
    return reasons


def killed_failures(cut: Killed, name: str) -> list[str]:
    """A process that ended before its cut must have succeeded."""
    return [] if cut.alive or cut.code == 0 else [f"{name} ended on its own with exit {cut.code}"]


def crash_merge_failures(cut: Killed, recover_code: int, tmp_left: list[str], check_code: int,
                         verdicts: dict[str, str]) -> list[str]:
    reasons = killed_failures(cut, "merge") + recovery_failures(recover_code, tmp_left, check_code)
    reasons += [f"{q} {v}" for q, v in verdicts.items() if v != compare.EQUAL]
    return reasons


def crash_insert_failures(cut: Killed, recover_code: int, tmp_left: list[str], check_code: int,
                          count_code: int, counts: dict[int, int], committed: set[int],
                          batch_id: int, rows: int = INSERT_BATCH_ROWS) -> tuple[list[str], set[int]]:
    """Reasons for failure and the `batch_id`s with a row count other than `rows`.

    Every batch present has all its rows; every batch committed before is still there; the one
    of this iteration is there if its `load` ended with 0.
    """
    reasons = killed_failures(cut, "load") + recovery_failures(recover_code, tmp_left, check_code)
    if count_code != 0:
        reasons.append(f"count query exited {count_code}")
        return reasons, set()
    partial = {b for b, n in counts.items() if n != rows}
    reasons += [f"batch {b} has {counts[b]} rows" for b in sorted(partial)]
    lost = sorted(committed - counts.keys())
    if lost:
        reasons.append("batches lost: " + ", ".join(map(str, lost)))
    unknown = sorted(counts.keys() - committed - {batch_id})
    if unknown:
        reasons.append("unknown batches: " + ", ".join(map(str, unknown)))
    if not cut.alive and cut.code == 0 and batch_id not in counts:
        reasons.append(f"batch {batch_id} missing after a load that exited 0")
    return reasons, partial


def corruption_verdict(first_code: int, first_equal: bool, next_code: int, next_equal: bool,
                       detached: bool) -> tuple[str, str]:
    """§17: the first query exits 3 or gives the result without the part; the next one gives that
    result; the part is in `detached/`."""
    reasons = []
    if first_code != EXIT_CORRUPT and not (first_code == 0 and first_equal):
        reasons.append(f"first query exited {first_code}" if first_code != 0
                       else "first query differs from the result without the part")
    if next_code != 0:
        reasons.append(f"next query exited {next_code}")
    elif not next_equal:
        reasons.append("next query differs from the result without the part")
    if not detached:
        reasons.append("part not in detached/")
    return (TRIAL_FAIL if reasons else TRIAL_PASS), "; ".join(reasons)


def corruption_command_verdict(items: list[dict]) -> tuple[str, str]:
    passed = {i["file_type"] for i in items if i.get("verdict") == TRIAL_PASS and i.get("detached") is True}
    missing = [t for t in FILE_TYPES if t not in passed]
    summary = f"{len(passed)}/{len(FILE_TYPES)} file types detected"
    if missing:
        summary += "; failed: " + ", ".join(missing)
    return (PASS if not missing else FAIL), summary


# Corruption: which column, which file, which byte


def affected_column(file_type: str, rng: random.Random, columns: int = len(CORRUPT_COLUMNS),
                    key: tuple[int, ...] = CORRUPT_KEY) -> int:
    """The column the trial's query reads: the file's own for `c<N>.bin`/`c<N>.mrk`, the first
    key column for `primary.idx`; any other file belongs to the whole part, so any column."""
    if file_type not in FILE_TYPES:
        raise ValueError(f"unknown file type: {file_type}")
    if file_type == "primary.idx":
        return key[0]
    return rng.randrange(columns)


def part_file(file_type: str, column: int) -> str:
    if file_type in COLUMN_FILE_TYPES:
        return f"c{column}.{file_type}"
    if file_type not in FILE_TYPES:
        raise ValueError(f"unknown file type: {file_type}")
    return file_type


def corrupt_byte(size: int, rng: random.Random) -> tuple[int, int]:
    """A uniform offset inside a file of `size` bytes and a non-zero XOR mask for that byte."""
    if size <= 0:
        raise ValueError("cannot corrupt an empty file")
    return rng.randrange(size), rng.randrange(1, 256)


def flip_byte(path: Path, rng: random.Random) -> tuple[int, int, int]:
    """Changes one byte of `path` in place; returns `(offset, old, new)`."""
    data = bytearray(path.read_bytes())
    offset, mask = corrupt_byte(len(data), rng)
    old = data[offset]
    data[offset] = old ^ mask
    with open(path, "r+b") as f:
        f.seek(offset)
        f.write(bytes([data[offset]]))
    return offset, old, data[offset]


def column_query(column: int) -> str:
    """A query that reads every block of `column`."""
    name, sql_type = CORRUPT_COLUMNS[column]
    value = f"length({name})" if sql_type.startswith("VARCHAR") else name
    return f"SELECT count(*), sum({value}), count({name}) FROM {CORRUPT_TABLE}"


def corruption_batches(seed: int = CORRUPTION_SEED) -> list[list[tuple]]:
    """4 batches with ascending keys, 10 % NULL in `v` and strings of varied length in `s`."""
    rng = random.Random(f"{seed}:corruption")
    batches = []
    for b in range(CORRUPT_BATCHES):
        rows = []
        for i in range(CORRUPT_BATCH_ROWS):
            v = None if rng.random() < 0.1 else rng.randrange(-10**12, 10**12)
            s = "".join(rng.choice("abcdefgh") for _ in range(rng.randrange(0, 24)))
            rows.append((b * CORRUPT_BATCH_ROWS + i, v, s))
        batches.append(rows)
    return batches


# crash-insert data


def insert_batch_text(batch_id: int, rows: int = INSERT_BATCH_ROWS) -> str:
    return "".join(f"{batch_id}\t{i}\tpayload-{batch_id}-{i:06d}-{(i * 2654435761) % 4294967296:08x}\n"
                   for i in range(rows))


# Shared steps


def fresh_scratch(p: Paths, report: Report) -> Path:
    scratch = p.scratch / report.name
    if scratch.exists():
        shutil.rmtree(scratch)
    scratch.mkdir(parents=True)
    return scratch


def copy_db(source: Path, dest: Path) -> Path:
    if dest.exists():
        shutil.rmtree(dest)
    shutil.copytree(source, dest)
    return dest


def require_hits(engines: Engines, p: Paths) -> tuple[str, list[Path]]:
    engines.require("veta")
    create = p.clickbench / "create-clickhouse.sql"
    files = clickbench.hits_files(p)
    missing = [f for f in [create, clickbench.query_files(p)[0], *files] if not f.is_file()]
    if missing:
        raise Infra(f"ClickBench data missing ({missing[0]}); run python3 -m harness fetch")
    return create.read_text(encoding="utf-8"), files


def load_hits(engines: Engines, report: Report, db: Path, create: str, files: list[Path],
              batch_rows: int, item: str) -> None:
    try:
        step(report, f"{item}.create", engines.veta_sql(db, create))
        step(report, f"{item}.load", engines.veta_load(db, TABLE, parquet=files, batch_rows=batch_rows))
    except LoadError as e:
        raise Infra(str(e)) from e


def active_parts(engines: Engines, db: Path, table: str) -> list[str] | None:
    run = engines.veta("parts", "--db", db, "--table", table)
    return parse_parts(run.stdout) if run.ok else None


def require_parts(engines: Engines, db: Path, table: str, expected: int) -> list[str]:
    parts = active_parts(engines, db, table)
    if parts is None or len(parts) != expected:
        raise Infra(f"{db}: {table} has {None if parts is None else len(parts)} active parts, expected {expected}")
    return parts


def parquet_rows(engines: Engines, scratch: Path, file: Path) -> int:
    """Rows of a Parquet file, from its metadata through `read_parquet`."""
    run = engines.veta_sql(scratch, f"SELECT count(*) FROM read_parquet({clickbench.sql_string(str(file))})")
    rows = split_rows(run.text)
    if not run.ok or len(rows) != 1 or not rows[0].isdigit():
        raise Infra(f"cannot count the rows of {file}: {run.error_tail()}")
    return int(rows[0])


def veta_side(engines: Engines, db: Path, suffix: str = "") -> compare.Side:
    return compare.Side("veta", lambda sql: engines.veta_sql(db, sql), "veta", suffix)


def memo_side(side: compare.Side) -> compare.Side:
    """The same side, running each distinct text once: for a reference nothing changes."""
    cache: dict[str, EngineRun] = {}

    def run(sql: str) -> EngineRun:
        if sql not in cache:
            cache[sql] = side.run(sql)
        return cache[sql]

    return compare.Side(side.engine, run, side.dialect, side.suffix)


def fixed_queries(p: Paths) -> list[tuple[str, str]]:
    texts = clickbench.read_queries(clickbench.query_files(p)[0])
    return [(clickbench.query_id(n), texts[n - 1]) for n in FIXED_QUERIES]


def compare_fixed(queries: list[tuple[str, str]], engine: compare.Side, ref: compare.Side, prefix: str,
                  raw) -> dict[str, compare.Comparison]:
    return {qid: compare.verify_query(f"{prefix}{qid}", engine, ref, sql, sql, raw=raw) for qid, sql in queries}


def store(report: Report, item: str, out: bytes) -> None:
    report.raw("veta", item, out)


# merge-equivalence


def merge_equivalence(args, p: Paths, report: Report):
    """`hits` in batches of 100 000 rows: 43 results before and after `merge --final`."""
    engines = Engines(p)
    create, files = require_hits(engines, p)
    scratch = fresh_scratch(p, report)
    before, after = scratch / "before", scratch / "after"
    report.progress(f"loading hits in batches of {MERGE_EQUIVALENCE_BATCH_ROWS} rows")
    load_hits(engines, report, before, create, files, MERGE_EQUIVALENCE_BATCH_ROWS, "before")
    parts_before = len(require_parts(engines, before, TABLE, MERGE_EQUIVALENCE_PARTS))
    copy_db(before, after)
    report.progress(f"{parts_before} parts; merge --final")
    merge = engines.veta("merge", "--db", after, "--table", TABLE, "--final")
    store(report, "merge", merge.stdout)
    if not merge.ok:
        raise Infra(f"merge --final exited {merge.code}: {merge.error_tail()}")
    parts = active_parts(engines, after, TABLE)
    parts_after = len(parts) if parts is not None else -1
    report.fields.update(parts_before=parts_before, parts_after=parts_after)
    texts = clickbench.read_queries(clickbench.query_files(p)[0])
    engine = veta_side(engines, after, ".after")
    ref = veta_side(engines, before, ".before")
    for n, sql in enumerate(texts, 1):
        qid = clickbench.query_id(n)
        c = compare.verify_query(qid, engine, ref, sql, sql, raw=report.raw)
        report.add({
            "query": qid,
            "rule": c.rule,
            "verdict": c.verdict,
            "rows": {"after": c.rows_engine, "before": c.rows_ref},
            "reason": c.reason,
        })
        report.progress(f"{qid} {c.rule} {c.verdict} {c.reason}".rstrip())
    return merge_equivalence_verdict(report.items, parts_before, parts_after)


# concurrent-reader


def concurrent_reader(args, p: Paths, report: Report):
    """`hits_0` in batches of 20 000 rows, `merger` running, N iterations of the 5 queries."""
    engines = Engines(p)
    create, files = require_hits(engines, p)
    queries = fixed_queries(p)
    scratch = fresh_scratch(p, report)
    live, reference = scratch / "live", scratch / "reference"
    hits_0 = files[:1]
    rows = parquet_rows(engines, scratch, hits_0[0])
    report.progress(f"loading hits_0 ({rows} rows) in batches of {READER_BATCH_ROWS}")
    load_hits(engines, report, reference, create, hits_0, READER_BATCH_ROWS, "load")
    require_parts(engines, reference, TABLE, math.ceil(rows / READER_BATCH_ROWS))
    copy_db(reference, live)
    ref = memo_side(veta_side(engines, reference, ".reference"))
    engine = veta_side(engines, live)
    # The reference results come before the merger starts.
    for qid, sql in queries:
        compare.verify_query(qid, ref, ref, sql, sql, raw=report.raw)
    start = active_parts(engines, live, TABLE)
    parts_start = len(start) if start is not None else None
    merger_log = report.dir / "merger.stderr"
    mismatches = errors = 0
    with open(merger_log, "wb") as log:
        merger = subprocess.Popen([str(p.veta_bin), "merger", "--db", str(live)],
                                  stdout=subprocess.DEVNULL, stderr=log)
        try:
            for n in range(1, args.iterations + 1):
                comparisons = compare_fixed(queries, engine, ref, f"i{n:03d}.", report.raw)
                bad = {q: c for q, c in comparisons.items() if c.verdict != compare.EQUAL}
                item_mismatches = sum(1 for c in bad.values() if c.verdict == compare.DIFFERENT)
                item_errors = sum(1 for c in bad.values() if c.verdict == compare.ERROR)
                mismatches += item_mismatches
                errors += item_errors
                report.add({
                    "iteration": n,
                    "verdicts": {q: c.verdict for q, c in comparisons.items()},
                    "rows": {q: c.rows_engine for q, c in comparisons.items()},
                    "mismatches": item_mismatches,
                    "errors": item_errors,
                    "reasons": {q: c.reason for q, c in bad.items()},
                    "merger_running": merger.poll() is None,
                })
                if n % 20 == 0 or bad:
                    report.progress(f"iteration {n}: {mismatches} mismatches, {errors} errors")
            end = active_parts(engines, live, TABLE)
        finally:
            merger_code = merger.poll()
            if merger_code is None:
                merger.send_signal(signal.SIGKILL)
                merger.wait()
    parts_end = len(end) if end is not None else None
    recovered = engines.veta("recover", "--db", live)
    store(report, "recover", recovered.stdout)
    report.fields.update(iterations=args.iterations, mismatches=mismatches, errors=errors,
                         parts_start=parts_start, parts_end=parts_end, merger_exit=merger_code)
    return concurrent_verdict(args.iterations, mismatches, errors, parts_start, parts_end)


# crash-merge


def recover_and_check(engines: Engines, report: Report, db: Path, table: str, prefix: str) -> tuple[int, list[str], int]:
    recovered = engines.veta("recover", "--db", db, "--table", table)
    store(report, f"{prefix}recover", recovered.stdout)
    tmp_left = tmp_entries(db / table)
    checked = engines.veta("check", "--db", db, "--table", table)
    store(report, f"{prefix}check", checked.stdout)
    return recovered.code, tmp_left, checked.code


def crash_merge(args, p: Paths, report: Report):
    """`merge --final` on a pristine copy of `hits_0` in 20 batches, killed in `[0, T]`."""
    engines = Engines(p)
    create, files = require_hits(engines, p)
    queries = fixed_queries(p)
    scratch = fresh_scratch(p, report)
    pristine, work = scratch / "pristine", scratch / "work"
    hits_0 = files[:1]
    rows = parquet_rows(engines, scratch, hits_0[0])
    batch_rows = math.ceil(rows / CRASH_MERGE_BATCHES)
    report.progress(f"loading hits_0 ({rows} rows) in {CRASH_MERGE_BATCHES} batches")
    load_hits(engines, report, pristine, create, hits_0, batch_rows, "load")
    require_parts(engines, pristine, TABLE, CRASH_MERGE_BATCHES)
    ref = memo_side(veta_side(engines, pristine, ".reference"))
    engine = veta_side(engines, work)
    merge_argv = [str(p.veta_bin), "merge", "--db", str(work), "--table", TABLE, "--final"]
    rng = random.Random(CRASH_SEED)
    duration = None
    failed = 0
    for n in range(1, args.iterations + 1):
        prefix = f"i{n:03d}."
        item: dict = {"iteration": n}
        if duration is None:
            copy_db(pristine, work)
            duration, done = timed(merge_argv)
            store(report, f"{prefix}calibration", done.stdout)
            if done.returncode != 0:
                raise Infra(f"merge --final exited {done.returncode} on the pristine copy")
            item["calibration_s"] = round(duration, 3)
            report.fields["merge_seconds"] = round(duration, 3)
            report.progress(f"a full merge takes {duration:.2f} s")
        copy_db(pristine, work)
        cut = run_and_kill(merge_argv, kill_delay(rng, duration))
        store(report, f"{prefix}merge", cut.stdout)
        recover_code, tmp_left, check_code = recover_and_check(engines, report, work, TABLE, prefix)
        parts = active_parts(engines, work, TABLE)
        comparisons = compare_fixed(queries, engine, ref, prefix, report.raw)
        verdicts = {q: c.verdict for q, c in comparisons.items()}
        reasons = crash_merge_failures(cut, recover_code, tmp_left, check_code, verdicts)
        reasons += [f"{q}: {c.reason}" for q, c in comparisons.items() if c.verdict != compare.EQUAL and c.reason]
        failed += bool(reasons)
        report.add(item | {
            "delay_s": round(cut.delay, 4),
            "alive": cut.alive,
            "merge_exit_code": cut.code,
            "recover_exit_code": recover_code,
            "tmp_found": tmp_left,
            "check_exit_code": check_code,
            "parts_after": len(parts) if parts is not None else None,
            "verdicts": verdicts,
            "verdict": "fail" if reasons else "pass",
            "reasons": reasons,
        })
        report.progress(f"iteration {n}: cut at {cut.delay:.3f} s, alive {cut.alive}, "
                        f"{'FAIL ' + '; '.join(reasons) if reasons else 'ok'}")
    fraction = alive_fraction(report.items)
    report.fields.update(iterations=args.iterations, failed=failed, alive_fraction=round(fraction, 4),
                         seed=CRASH_SEED)
    return crash_verdict(args.iterations, failed, fraction)


# crash-insert


def crash_insert(args, p: Paths, report: Report):
    """A `load --tsv` of 200 000 rows per iteration killed in `[0, T]`; no batch half there."""
    engines = Engines(p)
    engines.require("veta")
    scratch = fresh_scratch(p, report)
    db, calibration = scratch / "db", scratch / "calibration"
    batch_file = scratch / "batch.tsv"
    try:
        step(report, "create", engines.veta_sql(db, INSERT_CREATE))
        step(report, "calibration.create", engines.veta_sql(calibration, INSERT_CREATE))
    except LoadError as e:
        raise Infra(str(e)) from e
    rng = random.Random(CRASH_SEED)
    duration = None
    committed: set[int] = set()
    partial_ids: set[int] = set()
    failed = 0
    for n in range(1, args.iterations + 1):
        prefix = f"i{n:03d}."
        item: dict = {"iteration": n, "batch_id": n}
        batch_file.write_text(insert_batch_text(n), encoding="utf-8")
        load_argv = [str(p.veta_bin), "load", "--db", str(db), "--table", INSERT_TABLE, "--tsv", str(batch_file)]
        if duration is None:
            calibration_argv = [*load_argv[:2], "--db", str(calibration), *load_argv[4:]]
            duration, done = timed(calibration_argv)
            store(report, f"{prefix}calibration", done.stdout)
            if done.returncode != 0:
                raise Infra(f"load --tsv exited {done.returncode} on the calibration table")
            item["calibration_s"] = round(duration, 3)
            report.fields["insert_seconds"] = round(duration, 3)
            report.progress(f"a full load takes {duration:.2f} s")
        cut = run_and_kill(load_argv, kill_delay(rng, duration))
        store(report, f"{prefix}load", cut.stdout)
        recover_code, tmp_left, check_code = recover_and_check(engines, report, db, INSERT_TABLE, prefix)
        count = engines.veta_sql(db, INSERT_COUNT_SQL)
        store(report, f"{prefix}count", count.stdout)
        counts = batch_counts(count.stdout) if count.ok else {}
        reasons, partial = crash_insert_failures(cut, recover_code, tmp_left, check_code, count.code,
                                                 counts, committed, n)
        partial_ids |= partial
        committed = {b for b, c in counts.items() if c == INSERT_BATCH_ROWS} if count.ok else committed
        failed += bool(reasons)
        report.add(item | {
            "delay_s": round(cut.delay, 4),
            "alive": cut.alive,
            "load_exit_code": cut.code,
            "recover_exit_code": recover_code,
            "tmp_found": tmp_left,
            "check_exit_code": check_code,
            "present": n in counts,
            "counts": {str(b): c for b, c in sorted(counts.items())},
            "verdict": "fail" if reasons else "pass",
            "reasons": reasons,
        })
        report.progress(f"iteration {n}: cut at {cut.delay:.3f} s, alive {cut.alive}, "
                        f"batch {'present' if n in counts else 'absent'}, "
                        f"{'FAIL ' + '; '.join(reasons) if reasons else 'ok'}")
    batch_file.unlink(missing_ok=True)
    fraction = alive_fraction(report.items)
    report.fields.update(iterations=args.iterations, failed=failed, alive_fraction=round(fraction, 4),
                         partial_batches=len(partial_ids), seed=CRASH_SEED)
    return crash_verdict(args.iterations, failed, fraction, len(partial_ids))


# corruption


def build_corrupt_db(engines: Engines, report: Report, db: Path, batches: list[Path], item: str) -> None:
    try:
        step(report, f"{item}.create", engines.veta_sql(db, CORRUPT_CREATE))
        step(report, f"{item}.load", engines.veta_load(db, CORRUPT_TABLE, tsv=batches))
    except LoadError as e:
        raise Infra(str(e)) from e


def first_run_side(first: EngineRun, side: compare.Side) -> compare.Side:
    """`side`, except that the text of `first` returns `first` instead of running again."""
    sql = first.argv[-1]
    return compare.Side(side.engine, lambda s: first if s == sql else side.run(s), side.dialect, side.suffix)


def corruption(args, p: Paths, report: Report):
    """One byte changed in each file type of one part of a 4-part table (§6.4, §17)."""
    engines = Engines(p)
    engines.require("veta")
    scratch = fresh_scratch(p, report)
    rng = random.Random(CORRUPTION_SEED)
    batch_files = []
    for b, rows in enumerate(corruption_batches()):
        f = scratch / f"batch-{b}.tsv"
        f.write_text(gen.tsv_text(rows), encoding="utf-8")
        batch_files.append(f)
    pristine = scratch / "pristine"
    build_corrupt_db(engines, report, pristine, batch_files, "pristine")
    parts = sorted(require_parts(engines, pristine, CORRUPT_TABLE, CORRUPT_BATCHES), key=part_blocks)
    target = rng.randrange(CORRUPT_BATCHES)
    target_part = parts[target]
    reference = scratch / "reference"
    build_corrupt_db(engines, report, reference, [f for b, f in enumerate(batch_files) if b != target], "reference")
    ref = memo_side(veta_side(engines, reference, ".reference"))
    report.fields.update(part=target_part, seed=CORRUPTION_SEED)
    report.progress(f"corrupting part {target_part} (batch {target})")
    for file_type in FILE_TYPES:
        column = affected_column(file_type, rng)
        name = part_file(file_type, column)
        db = copy_db(pristine, scratch / f"trial-{file_type}")
        table_dir = db / CORRUPT_TABLE
        offset, old, new = flip_byte(table_dir / target_part / name, rng)
        sql = column_query(column)
        first = engines.veta_sql(db, sql)
        store(report, f"{file_type}.first", first.stdout)
        first_equal = False
        first_reason = ""
        if first.code == 0:
            c = compare.verify_query(f"{file_type}.first", first_run_side(first, veta_side(engines, db)), ref,
                                     sql, sql, raw=report.raw)
            first_equal, first_reason = c.verdict == compare.EQUAL, c.reason
        second = compare.verify_query(f"{file_type}.next", veta_side(engines, db), ref, sql, sql, raw=report.raw)
        next_code = second.engine_run.code if second.engine_run is not None else None
        detached = is_detached(table_dir, target_part)
        verdict, reason = corruption_verdict(first.code, first_equal, next_code if next_code is not None else -1,
                                             second.verdict == compare.EQUAL, detached)
        report.add({
            "file_type": file_type,
            "file": name,
            "part": target_part,
            "column": CORRUPT_COLUMNS[column][0],
            "offset": offset,
            "old_byte": old,
            "new_byte": new,
            "sql": sql,
            "exit_code": first.code,
            "first_stderr": first.error_tail(),
            "first_equal": first_equal,
            "next_exit_code": next_code,
            "next_equal": second.verdict == compare.EQUAL,
            "detached": detached,
            "detached_entries": detached_entries(table_dir),
            "verdict": verdict,
            "reason": "; ".join(r for r in (reason, first_reason, second.reason) if r),
        })
        report.progress(f"{file_type} ({name} byte {offset}): exit {first.code}, next {next_code}, "
                        f"detached {detached}, {verdict} {reason}".rstrip())
    return corruption_command_verdict(report.items)

