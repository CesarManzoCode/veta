"""ClickBench: `load clickbench` (§14.3), `clickbench-verify` (§15), `pruning` and
`codec-roundtrip` over `hits` (§17)."""

import argparse
import re
import shutil
import time
from pathlib import Path

from harness import compare
from harness.config import HITS_PINS, HITS_ROWS, Paths
from harness.engines import Databases, EngineRun, Engines, clickbench_databases, veta_stats
from harness.report import FAIL, PASS, SKIP, Infra, Report, Skip

TABLE = "hits"


class LoadError(Exception):
    pass


def sql_string(text: str) -> str:
    """A single-quoted SQL literal valid in DuckDB, ClickHouse and veta."""
    return "'" + text.replace("'", "''") + "'"


def hits_files(p: Paths) -> list[Path]:
    return [p.data / pin.dest for pin in HITS_PINS]


def duckdb_insert_sql(files: list[Path]) -> str:
    listing = ", ".join(sql_string(str(f)) for f in files)
    return (
        f"INSERT INTO {TABLE} SELECT * REPLACE ("
        "make_date(EventDate) AS EventDate, "
        "epoch_ms(EventTime * 1000) AS EventTime, "
        "epoch_ms(ClientEventTime * 1000) AS ClientEventTime, "
        "epoch_ms(LocalEventTime * 1000) AS LocalEventTime) "
        f"FROM read_parquet([{listing}], binary_as_string=True);"
    )


def clickhouse_insert_sql(clickbench_dir: Path) -> str:
    pattern = f"{clickbench_dir}/hits_{{0..{len(HITS_PINS) - 1}}}.parquet"
    return f"INSERT INTO {TABLE} SELECT * FROM file({sql_string(pattern)}, Parquet)"


def clickhouse_state_sql(database: str) -> str:
    """Row count and active parts of `hits`; loaded means all rows in one part."""
    return (
        f"SELECT (SELECT count() FROM {database}.{TABLE}), "
        f"(SELECT count() FROM system.parts WHERE database = {sql_string(database)} "
        f"AND table = {sql_string(TABLE)} AND active)"
    )


def first_row(run: EngineRun) -> list[str] | None:
    rows = run.rows
    if not run.ok or not rows:
        return None
    return rows[0].split("\t") if run.engine != "duckdb" else rows[0].split(",")


class Loader:
    """Loads `hits` into one engine, keeping each step's raw stdout."""

    def __init__(self, engines: Engines, report: Report, dbs: Databases):
        self.engines = engines
        self.report = report
        self.dbs = dbs
        self.p = engines.paths

    def step(self, item: str, run: EngineRun) -> EngineRun:
        self.report.raw(run.engine, item, run.stdout)
        if not run.ok:
            raise LoadError(f"{run.engine} {item} exited {run.code}: {run.error_tail()}")
        return run

    # veta

    def veta_count(self) -> int | None:
        if not self.dbs.veta.exists():
            return None
        row = first_row(self.engines.veta_sql(self.dbs.veta, f"SELECT count(*) FROM {TABLE}"))
        return int(row[0]) if row and row[0].isdigit() else None

    def veta(self, rebuild: bool) -> str:
        if not rebuild and self.veta_count() == HITS_ROWS:
            return "kept"
        if self.dbs.veta.exists():
            shutil.rmtree(self.dbs.veta)
        self.dbs.veta.parent.mkdir(parents=True, exist_ok=True)
        create = (self.p.clickbench / "create-clickhouse.sql").read_text()
        self.step("create", self.engines.veta_sql(self.dbs.veta, create))
        self.step("load", self.engines.veta_load(self.dbs.veta, TABLE, parquet=hits_files(self.p)))
        return "loaded"

    # DuckDB

    def duckdb_count(self) -> int | None:
        if not self.dbs.duckdb.exists():
            return None
        row = first_row(self.engines.duckdb(self.dbs.duckdb, f"SELECT count(*) FROM {TABLE};"))
        return int(row[0]) if row and row[0].isdigit() else None

    def duckdb(self) -> str:
        if self.duckdb_count() == HITS_ROWS:
            return "kept"
        for path in (self.dbs.duckdb, self.dbs.duckdb.with_name(self.dbs.duckdb.name + ".wal")):
            path.unlink(missing_ok=True)
        self.dbs.duckdb.parent.mkdir(parents=True, exist_ok=True)
        create = (self.p.clickbench / "create-duckdb.sql").read_text()
        self.step("create", self.engines.duckdb(self.dbs.duckdb, create))
        self.step("insert", self.engines.duckdb(self.dbs.duckdb, duckdb_insert_sql(hits_files(self.p))))
        return "loaded"

    # ClickHouse

    def clickhouse_state(self) -> tuple[int, int] | None:
        db = self.dbs.clickhouse_database
        run = self.engines.clickhouse(clickhouse_state_sql(db), db, self.dbs.clickhouse_path)
        row = first_row(run)
        if not row or len(row) != 2 or not all(c.isdigit() for c in row):
            return None
        return int(row[0]), int(row[1])

    def clickhouse(self) -> str:
        if self.clickhouse_state() == (HITS_ROWS, 1):
            return "kept"
        db, path = self.dbs.clickhouse_database, self.dbs.clickhouse_path
        create = (self.p.clickbench / "create-clickhouse.sql").read_text()
        self.step("create", self.engines.clickhouse(create, db, path))
        self.step("insert", self.engines.clickhouse(clickhouse_insert_sql(self.p.clickbench), db, path))
        self.step("optimize", self.engines.clickhouse(f"OPTIMIZE TABLE {TABLE} FINAL", db, path))
        return "loaded"

    def verify(self, engine: str) -> None:
        """After loading, every engine must hold exactly the 10 M rows."""
        match engine:
            case "veta":
                ok = self.veta_count() == HITS_ROWS
            case "duckdb":
                ok = self.duckdb_count() == HITS_ROWS
            case _:
                ok = self.clickhouse_state() == (HITS_ROWS, 1)
        if not ok:
            raise LoadError(f"{engine}: {TABLE} does not hold {HITS_ROWS} rows after loading")


def load(args, p: Paths, report: Report):
    engines = Engines(p)
    engines.require("veta")
    missing = [f for f in [p.clickbench / "create-clickhouse.sql", p.clickbench / "create-duckdb.sql",
                           *hits_files(p)] if not f.is_file()]
    if missing:
        raise Infra(f"ClickBench data missing ({missing[0]}); run python3 -m harness fetch")
    loader = Loader(engines, report, clickbench_databases(p))
    actions = {}
    for engine in ("veta", "duckdb", "clickhouse"):
        item = {"engine": engine}
        start = time.perf_counter()
        try:
            engines.require(engine)
            report.progress(f"{engine}: loading")
            match engine:
                case "veta":
                    action = loader.veta(args.rebuild_veta)
                case "duckdb":
                    action = loader.duckdb()
                case _:
                    action = loader.clickhouse()
            if action == "loaded":
                loader.verify(engine)
            item.update(action=action, rows=HITS_ROWS, verdict="ok")
        except Skip as e:
            item.update(action="skipped", verdict="skip", reason=str(e))
        except (LoadError, OSError) as e:
            item.update(action="error", verdict="error", reason=str(e))
        item["wall_s"] = round(time.perf_counter() - start, 3)
        actions[engine] = item["action"]
        report.progress(f"{engine}: {item['action']} {item.get('reason', '')}".rstrip())
        report.add(item)
    summary = ", ".join(f"{e} {a}" for e, a in actions.items())
    if "error" in actions.values():
        return FAIL, summary
    if "skipped" in actions.values():
        return SKIP, summary
    return PASS, summary


# clickbench-verify

QUERY_COUNT = 43
PRUNING_QUERIES = range(37, 44)

# §15.4, the only rewrites allowed; veta always runs the ClickHouse text unchanged.
CLICKHOUSE_Q4 = "SELECT AVG(toInt128(UserID)) FROM hits;"
Q29_PATTERN_START = "REGEXP_REPLACE(Referer, '"


def query_id(n: int) -> str:
    return f"q{n:02d}"


def tamper_query(value: str) -> str:
    """`--tamper` accepts `5`, `q5`, `Q05`…; returns the item name `q05`."""
    m = re.fullmatch(r"[qQ]?(\d+)", value.strip())
    if not m or not 1 <= int(m.group(1)) <= QUERY_COUNT:
        raise argparse.ArgumentTypeError(f"not a ClickBench query (1..{QUERY_COUNT}): {value}")
    return query_id(int(m.group(1)))


def read_queries(path: Path) -> list[str]:
    """The 43 queries of a pinned `queries-*.sql`, one per non-empty line."""
    lines = [line.strip() for line in path.read_text(encoding="utf-8").split("\n") if line.strip()]
    if len(lines) != QUERY_COUNT:
        raise Infra(f"{path}: {len(lines)} queries, expected {QUERY_COUNT}")
    return lines


def duckdb_q29(text: str) -> str:
    """Q29 for DuckDB: the pattern starts with `(?s)` so `.` also matches `\\n`."""
    if text.count(Q29_PATTERN_START) != 1:
        raise Infra("ClickBench Q29 does not have the expected REGEXP_REPLACE pattern")
    return text.replace(Q29_PATTERN_START, Q29_PATTERN_START + "(?s)", 1)


def oracle_queries(against: str, clickhouse_texts: list[str], duckdb_texts: list[str]) -> list[str]:
    if against == "clickhouse":
        texts = list(clickhouse_texts)
        texts[3] = CLICKHOUSE_Q4
    else:
        texts = list(duckdb_texts)
        texts[28] = duckdb_q29(texts[28])
    return texts


def require_loaded(engines: Engines, dbs: Databases, *engine_names: str) -> None:
    """Missing binaries are SKIP (oracles) or 2 (veta); missing loaded data is 2."""
    for name in engine_names:
        engines.require(name)
    missing = {
        "veta": dbs.veta,
        "duckdb": dbs.duckdb,
        "clickhouse": dbs.clickhouse_path,
    }
    for name in engine_names:
        if not missing[name].exists():
            raise Infra(f"{name}: {missing[name]} not found; run python3 -m harness load clickbench")


def query_files(p: Paths) -> tuple[Path, Path]:
    files = (p.clickbench / "queries-clickhouse.sql", p.clickbench / "queries-duckdb.sql")
    for f in files:
        if not f.is_file():
            raise Infra(f"{f} missing; run python3 -m harness fetch")
    return files


def oracle_side(engines: Engines, dbs: Databases, against: str) -> compare.Side:
    if against == "duckdb":
        return compare.Side("duckdb", lambda sql: engines.duckdb(dbs.duckdb, sql), "duckdb")
    return compare.Side(
        "clickhouse",
        lambda sql: engines.clickhouse(sql, dbs.clickhouse_database, dbs.clickhouse_path),
        "clickhouse",
    )


def verify_report_name(args) -> str:
    return f"clickbench-verify-{args.against}"


def verify(args, p: Paths, report: Report):
    """`clickbench-verify --against duckdb|clickhouse [--tamper Q]`: `43/43 equal` to pass."""
    engines = Engines(p)
    dbs = clickbench_databases(p)
    require_loaded(engines, dbs, "veta", args.against)
    clickhouse_file, duckdb_file = query_files(p)
    veta_texts = read_queries(clickhouse_file)
    ref_texts = oracle_queries(args.against, veta_texts, read_queries(duckdb_file))
    veta = compare.Side("veta", lambda sql: engines.veta_sql(dbs.veta, sql), "veta")
    ref = oracle_side(engines, dbs, args.against)
    report.fields["against"] = args.against
    if args.tamper:
        report.fields["tamper"] = args.tamper
    equal = 0
    for n, (veta_sql, ref_sql) in enumerate(zip(veta_texts, ref_texts), 1):
        qid = query_id(n)
        c = compare.verify_query(qid, veta, ref, veta_sql, ref_sql, tampered=args.tamper == qid, raw=report.raw)
        equal += c.verdict == compare.EQUAL
        report.add({
            "query": qid,
            "rule": c.rule,
            "verdict": c.verdict,
            "rows": {"veta": c.rows_engine, args.against: c.rows_ref},
            "reason": c.reason,
        })
        report.progress(f"{qid} {c.rule} {c.verdict} {c.reason}".rstrip())
    return (PASS if equal == QUERY_COUNT else FAIL), f"{equal}/{QUERY_COUNT} equal"


def pruning(args, p: Paths, report: Report):
    """Q37–Q43 with and without pruning: fewer granules read and the same results."""
    engines = Engines(p)
    dbs = clickbench_databases(p)
    require_loaded(engines, dbs, "veta")
    texts = read_queries(query_files(p)[0])
    prune = compare.Side("veta", lambda sql: engines.veta_sql(dbs.veta, sql, stats=True), "veta")
    noprune = compare.Side(
        "veta", lambda sql: engines.veta_sql(dbs.veta, sql, stats=True, extra=["--no-prune"]), "veta",
        suffix=".noprune",
    )
    good = 0
    for n in PRUNING_QUERIES:
        qid = query_id(n)
        sql = texts[n - 1]
        c = compare.verify_query(qid, prune, noprune, sql, sql, raw=report.raw)
        read_prune = granules_read(c.engine_run)
        read_noprune = granules_read(c.ref_run)
        verdict, reason = c.verdict, c.reason
        if verdict == compare.EQUAL:
            if read_prune is None or read_noprune is None:
                verdict, reason = compare.ERROR, "no VETA_STATS granules_read on stderr"
            elif read_prune >= read_noprune:
                verdict = compare.DIFFERENT
                reason = f"granules_read with pruning {read_prune} >= without {read_noprune}"
        good += verdict == compare.EQUAL
        report.add({
            "query": qid,
            "granules_read_prune": read_prune,
            "granules_read_noprune": read_noprune,
            "verdict": verdict,
            "rule": c.rule,
            "reason": reason,
        })
        report.progress(f"{qid} granules {read_prune} vs {read_noprune} {verdict} {reason}".rstrip())
    total = len(PRUNING_QUERIES)
    return (PASS if good == total else FAIL), f"{good}/{total} pruned and equal"


def granules_read(run: EngineRun | None) -> int | None:
    stats = veta_stats(run.stderr) if run is not None else None
    value = stats.get("granules_read") if stats else None
    return value if isinstance(value, int) else None


# codec-roundtrip

ROUNDTRIP_RE = re.compile(rb"codec-roundtrip: (\d+) cases, (\d+) failures")


def roundtrip_counts(stdout: bytes) -> tuple[int, int] | None:
    found = ROUNDTRIP_RE.findall(stdout)
    return (int(found[-1][0]), int(found[-1][1])) if found else None


def codec_roundtrip(args, p: Paths, report: Report):
    """`veta codec-roundtrip --random 2000 --seed 1` and over `hits`: both must exit 0."""
    engines = Engines(p)
    dbs = clickbench_databases(p)
    require_loaded(engines, dbs, "veta")
    cases = (
        ("random", ("codec-roundtrip", "--random", 2000, "--seed", 1)),
        ("hits", ("codec-roundtrip", "--db", dbs.veta, "--table", TABLE)),
    )
    total_cases = total_failures = 0
    bad = []
    for name, argv in cases:
        report.progress(f"{name}: running")
        run = engines.veta(*argv)
        report.raw("veta", name, run.stdout)
        counts = roundtrip_counts(run.stdout)
        ok = run.ok and counts is not None and counts[1] == 0
        if counts is not None:
            total_cases += counts[0]
            total_failures += counts[1]
        if not ok:
            bad.append(name)
        report.add({
            "case": name,
            "exit_code": run.code,
            "cases": counts[0] if counts else None,
            "failures": counts[1] if counts else None,
            "verdict": "ok" if ok else "fail",
            "reason": "" if ok else run.error_tail(),
        })
        report.progress(f"{name}: exit {run.code}, {counts}")
    summary = f"{total_cases} cases, {total_failures} failures"
    if bad:
        summary += f"; failed: {', '.join(bad)}"
    return (PASS if not bad else FAIL), summary
