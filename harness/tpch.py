"""TPC-H: `load tpch` (§14.3), `tpch-verify` (§15), and `join-reorder` and `cardinality` (§17)."""

import csv
import io
import json
import math
import random
import shutil
import statistics
import time
from pathlib import Path

from harness import compare
from harness.clickbench import LoadError, first_row, query_id, sql_string
from harness.config import TPCH_EXTENSION_PIN, TPCH_QUERY_SHA256, Paths
from harness.engines import Databases, EngineRun, Engines, split_rows, tpch_databases
from harness.normalize import parse_output
from harness.report import FAIL, PASS, SKIP, Infra, Report, Skip

SCALE_FACTORS = (1, 10)
QUERY_COUNT = len(TPCH_QUERY_SHA256)
TABLES = ("lineitem", "orders", "customer", "part", "partsupp", "supplier", "nation", "region")
BATCH_ROWS = 1_000_000
DUCKDB_MEMORY = "SET memory_limit='4GB';"

# Rows per unit of scale factor; `lineitem` is not proportional (RESEARCH §3).
LINEITEM_ROWS = {1: 6_001_215, 10: 59_986_052}
SCALED_ROWS = {"orders": 1_500_000, "customer": 150_000, "part": 200_000, "partsupp": 800_000, "supplier": 10_000}
FIXED_ROWS = {"nation": 25, "region": 5}


def expected_rows(sf: int) -> dict[str, int]:
    scaled = {table: rows * sf for table, rows in SCALED_ROWS.items()}
    return {"lineitem": LINEITEM_ROWS[sf], **scaled, **FIXED_ROWS}


def statements(sql: str) -> list[str]:
    """The statements of a script without strings that hold `;`, like `bench/tpch/create.sql`."""
    return [s.strip() for s in sql.split(";") if s.strip()]


def clickhouse_create(statement: str) -> str:
    return f"{statement} ENGINE = MergeTree"


def clickhouse_database(sf: int) -> str:
    return f"tpch_sf{sf}"


def parquet_file(p: Paths, sf: int, table: str) -> Path:
    return p.tpch / f"sf{sf}" / f"{table}.parquet"


def duckdb_generate_sql(sf: int) -> str:
    return f"{DUCKDB_MEMORY} LOAD tpch; CALL dbgen(sf={sf});"


def duckdb_export_sql(table: str, file: Path) -> str:
    return f"{DUCKDB_MEMORY} COPY {table} TO {sql_string(str(file))} (FORMAT parquet);"


def clickhouse_insert_sql(table: str, file: Path) -> str:
    return f"INSERT INTO {table} SELECT * FROM file({sql_string(str(file))}, Parquet)"


def count_sql(table: str) -> str:
    return f"SELECT count(*) FROM {table};"


def single_count(run: EngineRun) -> int | None:
    """The integer of a one-cell result; `None` if the run failed or printed anything else."""
    row = first_row(run)
    if row is None or len(run.rows) != 1 or len(row) != 1:
        return None
    try:
        return int(row[0])
    except ValueError:
        return None


def step(report: Report, item: str, run: EngineRun) -> EngineRun:
    report.raw(run.engine, item, run.stdout)
    if not run.ok:
        raise LoadError(f"{run.engine} {item} exited {run.code}: {run.error_tail()}")
    return run


class Loader:
    """Loads one scale factor: DuckDB generates and exports, veta and ClickHouse read the Parquet."""

    def __init__(self, engines: Engines, report: Report, dbs: Databases, sf: int):
        self.engines = engines
        self.report = report
        self.dbs = dbs
        self.sf = sf
        self.p = engines.paths
        self.expected = expected_rows(sf)
        self.create = statements((self.p.repo / "bench" / "tpch" / "create.sql").read_text(encoding="utf-8"))

    def holds_all_rows(self, count) -> bool:
        return all(count(table) == rows for table, rows in self.expected.items())

    def require_parquet(self) -> list[Path]:
        files = [parquet_file(self.p, self.sf, table) for table in TABLES]
        if not all(f.is_file() for f in files):
            raise Skip(f"Parquet files of SF{self.sf} missing; DuckDB did not export them")
        return files

    # DuckDB

    def duckdb_count(self, table: str) -> int | None:
        return single_count(self.engines.duckdb(self.dbs.duckdb, count_sql(table)))

    def duckdb_loaded(self) -> bool:
        return self.dbs.duckdb.exists() and self.holds_all_rows(self.duckdb_count)

    def duckdb(self) -> str:
        generated = self.duckdb_loaded()
        missing = [t for t in TABLES if not parquet_file(self.p, self.sf, t).is_file()]
        if generated and not missing:
            return "kept"
        if not generated:
            if not (self.p.data / TPCH_EXTENSION_PIN.dest).is_file():
                raise Infra(f"tpch extension missing ({TPCH_EXTENSION_PIN.dest}); run python3 -m harness fetch")
            for path in (self.dbs.duckdb, self.dbs.duckdb.with_name(self.dbs.duckdb.name + ".wal")):
                path.unlink(missing_ok=True)
            self.dbs.duckdb.parent.mkdir(parents=True, exist_ok=True)
            step(self.report, "generate", self.engines.duckdb(self.dbs.duckdb, duckdb_generate_sql(self.sf)))
            missing = list(TABLES)
        for table in missing:
            file = parquet_file(self.p, self.sf, table)
            file.parent.mkdir(parents=True, exist_ok=True)
            step(self.report, f"export-{table}", self.engines.duckdb(self.dbs.duckdb, duckdb_export_sql(table, file)))
        return "loaded"

    # veta

    def veta_count(self, table: str) -> int | None:
        return single_count(self.engines.veta_sql(self.dbs.veta, count_sql(table)))

    def veta_loaded(self) -> bool:
        return self.dbs.veta.exists() and self.holds_all_rows(self.veta_count)

    def veta(self, rebuild: bool) -> str:
        if not rebuild and self.veta_loaded():
            return "kept"
        files = self.require_parquet()
        if self.dbs.veta.exists():
            shutil.rmtree(self.dbs.veta)
        self.dbs.veta.parent.mkdir(parents=True, exist_ok=True)
        for table, statement in zip(TABLES, self.create):
            step(self.report, f"create-{table}", self.engines.veta_sql(self.dbs.veta, statement))
        for table, file in zip(TABLES, files):
            run = self.engines.veta_load(self.dbs.veta, table, parquet=[file], batch_rows=BATCH_ROWS)
            step(self.report, f"load-{table}", run)
        return "loaded"

    # ClickHouse

    def clickhouse_run(self, sql: str, database: str | None) -> EngineRun:
        return self.engines.clickhouse(sql, database, self.dbs.clickhouse_path)

    def clickhouse_count(self, table: str) -> int | None:
        return single_count(self.clickhouse_run(count_sql(table), self.dbs.clickhouse_database))

    def clickhouse_loaded(self) -> bool:
        return self.holds_all_rows(self.clickhouse_count)

    def clickhouse(self) -> str:
        if self.clickhouse_loaded():
            return "kept"
        files = self.require_parquet()
        db = self.dbs.clickhouse_database
        step(self.report, "drop-database", self.clickhouse_run(f"DROP DATABASE IF EXISTS {db}", None))
        step(self.report, "create-database", self.clickhouse_run(f"CREATE DATABASE {db}", None))
        for table, statement in zip(TABLES, self.create):
            step(self.report, f"create-{table}", self.clickhouse_run(clickhouse_create(statement), db))
        for table, file in zip(TABLES, files):
            step(self.report, f"insert-{table}", self.clickhouse_run(clickhouse_insert_sql(table, file), db))
        return "loaded"

    def verify(self, engine: str) -> None:
        """After loading, every engine must hold exactly the rows of the scale factor."""
        loaded = {"veta": self.veta_loaded, "duckdb": self.duckdb_loaded, "clickhouse": self.clickhouse_loaded}
        if not loaded[engine]():
            raise LoadError(f"{engine}: the tables of SF{self.sf} do not hold the expected rows after loading")


def load_report_name(args) -> str:
    return f"load-tpch-sf{args.sf}"


def load(args, p: Paths, report: Report):
    """`load tpch --sf 1|10 [--rebuild-veta]`; only `--rebuild-veta` replaces loaded data."""
    engines = Engines(p)
    engines.require("veta")
    create = p.repo / "bench" / "tpch" / "create.sql"
    if not create.is_file():
        raise Infra(f"{create} missing")
    loader = Loader(engines, report, tpch_databases(p, args.sf), args.sf)
    total_rows = sum(loader.expected.values())
    actions = {}
    for engine in ("duckdb", "veta", "clickhouse"):
        item = {"engine": engine}
        start = time.perf_counter()
        try:
            engines.require(engine)
            report.progress(f"{engine}: loading SF{args.sf}")
            match engine:
                case "duckdb":
                    action = loader.duckdb()
                case "veta":
                    action = loader.veta(args.rebuild_veta)
                case _:
                    action = loader.clickhouse()
            if action == "loaded":
                loader.verify(engine)
            item.update(action=action, rows=total_rows, verdict="ok")
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


# Shared by the commands that read the loaded data


def require_loaded(engines: Engines, dbs: Databases, sf: int, *engine_names: str) -> None:
    """Missing binaries are SKIP (oracles) or 2 (veta); missing loaded data is 2."""
    for name in engine_names:
        engines.require(name)
    where = {"veta": dbs.veta, "duckdb": dbs.duckdb}
    for name in engine_names:
        if not where[name].exists():
            raise Infra(f"{name}: {where[name]} not found; run python3 -m harness load tpch --sf {sf}")


def read_queries(p: Paths) -> dict[str, str]:
    """`{"q01": text, …}` of the pinned files; every engine runs the text unchanged."""
    texts = {}
    for name in TPCH_QUERY_SHA256:
        path = p.tpch / "queries" / f"{name}.sql"
        if not path.is_file():
            raise Infra(f"{path} missing; run python3 -m harness fetch")
        texts[name] = path.read_text(encoding="utf-8")
    return texts


def veta_side(engines: Engines, db: Path, extra=(), suffix: str = "") -> compare.Side:
    return compare.Side("veta", lambda sql: engines.veta_sql(db, sql, extra=extra), "veta", suffix)


# tpch-verify

ANSWERS_SQL = "LOAD tpch; SELECT query_nr, answer FROM tpch_answers() WHERE scale_factor = 1 ORDER BY query_nr;"


class AnswerError(ValueError):
    pass


def parse_answer(text: str) -> tuple[list[str], list[list[str]]]:
    """Header and rows of a `tpch_answers()` cell: lines of `|`-separated values, kept verbatim."""
    lines = split_rows(text)
    if not lines:
        raise AnswerError("empty answer")
    header = lines[0].split("|")
    rows = [line.split("|") for line in lines[1:]]
    for n, row in enumerate(rows, 1):
        if len(row) != len(header):
            raise AnswerError(f"answer row {n} has {len(row)} cells, the header has {len(header)}")
    return header, rows


def answers_from_output(text: str) -> dict[str, str]:
    """`{"q01": answer, …}` from the CSV output of `ANSWERS_SQL`."""
    answers = {query_id(int(row[0])): row[1] for row in parse_output("duckdb", text)}
    if sorted(answers) != sorted(TPCH_QUERY_SHA256):
        raise AnswerError(f"{len(answers)} answers, expected {QUERY_COUNT}")
    return answers


def official_answers(engines: Engines, report: Report) -> dict[str, str]:
    run = engines.duckdb(None, ANSWERS_SQL)
    report.raw("duckdb", "tpch_answers", run.stdout)
    if not run.ok:
        raise Infra(f"tpch_answers() exited {run.code}: {run.error_tail()}")
    try:
        return answers_from_output(run.text)
    except (ValueError, IndexError) as e:
        raise Infra(f"tpch_answers(): {e}") from e


def answer_run(sql: str, query_sql: str, answer: str) -> EngineRun:
    """The official answer of `query_sql` as if DuckDB had printed it (`-csv -noheader`).

    The answer holds only the query as written, so any other text, like an augmented
    variant, fails; `compare` reports that as an error of the query.
    """
    argv = ["tpch_answers()"]
    raw = answer.encode("utf-8", errors="surrogateescape")
    if sql != query_sql:
        return EngineRun("duckdb", argv, 1, raw, b"tpch_answers() only holds the query as written", None, b"")
    try:
        _, rows = parse_answer(answer)
    except AnswerError as e:
        return EngineRun("duckdb", argv, 1, raw, str(e).encode(), None, b"")
    out = io.StringIO(newline="")
    csv.writer(out, lineterminator="\n").writerows(rows)
    return EngineRun("duckdb", argv, 0, raw, b"", None, out.getvalue().encode("utf-8", errors="surrogateescape"))


def answer_side(query_sql: str, answer: str) -> compare.Side:
    return compare.Side("duckdb", lambda sql: answer_run(sql, query_sql, answer), "duckdb")


def verify_report_name(args) -> str:
    return f"tpch-verify-sf{args.sf}"


def verify(args, p: Paths, report: Report):
    """`tpch-verify --sf 1` against `tpch_answers()`, `--sf 10` against DuckDB: `22/22 equal`."""
    engines = Engines(p)
    dbs = tpch_databases(p, args.sf)
    report.fields["scale_factor"] = args.sf
    if args.sf == 1:
        require_loaded(engines, dbs, args.sf, "veta")
        engines.require("duckdb")
        answers = official_answers(engines, report)
    else:
        require_loaded(engines, dbs, args.sf, "veta", "duckdb")
        answers = None
    veta = veta_side(engines, dbs.veta)
    duckdb = compare.Side("duckdb", lambda sql: engines.duckdb(dbs.duckdb, sql), "duckdb")
    equal = 0
    for qid, sql in read_queries(p).items():
        ref = answer_side(sql, answers[qid]) if answers is not None else duckdb
        c = compare.verify_query(qid, veta, ref, sql, sql, raw=report.raw)
        equal += c.verdict == compare.EQUAL
        report.add({
            "query": qid,
            "rule": c.rule,
            "verdict": c.verdict,
            "rows": {"veta": c.rows_engine, "duckdb": c.rows_ref},
            "reason": c.reason,
        })
        report.progress(f"{qid} {c.rule} {c.verdict} {c.reason}".rstrip())
    return (PASS if equal == QUERY_COUNT else FAIL), f"{equal}/{QUERY_COUNT} equal"


# join-reorder

REORDER_RUNS = 3
REORDER_MIN_FACTOR = 1.5
JOIN_ORDER_COST = ("--join-order", "cost")
JOIN_ORDER_SYNTACTIC = ("--join-order", "syntactic")


def best_of(seconds: list[float]) -> float | None:
    """The best of all the runs; `None` unless each of them finished."""
    return min(seconds) if len(seconds) >= REORDER_RUNS else None


def reorder_totals(items: list[dict]) -> tuple[float | None, float | None, float | None]:
    """`T(cost)`, `T(syntactic)` and their factor over the items; `None` if a time is missing."""
    cost = [i["cost_best"] for i in items]
    syntactic = [i["syntactic_best"] for i in items]
    if not items or None in cost or None in syntactic:
        return None, None, None
    total_cost, total_syntactic = sum(cost), sum(syntactic)
    return total_cost, total_syntactic, (total_syntactic / total_cost if total_cost > 0 else None)


def timed_side(side: compare.Side, seconds: list[float]) -> compare.Side:
    """`side`, noting the time of each run that exits with 0."""

    def run(sql: str) -> EngineRun:
        result = side.run(sql)
        if result.ok:
            seconds.append(result.seconds)
        return result

    return compare.Side(side.engine, run, side.dialect, side.suffix)


def join_reorder(args, p: Paths, report: Report):
    """TPC-H SF1 with each join order: equal results and `T(syntactic) / T(cost) >= 1.5`."""
    engines = Engines(p)
    dbs = tpch_databases(p, 1)
    require_loaded(engines, dbs, 1, "veta")
    equal = 0
    for qid, sql in read_queries(p).items():
        cost_seconds, syntactic_seconds = [], []
        cost = timed_side(veta_side(engines, dbs.veta, JOIN_ORDER_COST), cost_seconds)
        syntactic = timed_side(veta_side(engines, dbs.veta, JOIN_ORDER_SYNTACTIC, ".syntactic"), syntactic_seconds)
        c = compare.verify_query(qid, cost, syntactic, sql, sql, raw=report.raw)
        if c.verdict == compare.EQUAL:
            for n in range(2, REORDER_RUNS + 1):
                for side in (cost, syntactic):
                    report.raw("veta", f"{qid}{side.suffix}.run{n}", side.run(sql).stdout)
        cost_best, syntactic_best = best_of(cost_seconds), best_of(syntactic_seconds)
        verdict, reason = c.verdict, c.reason
        if verdict == compare.EQUAL and (cost_best is None or syntactic_best is None):
            verdict, reason = compare.ERROR, "a repeated run did not exit with 0"
        equal += verdict == compare.EQUAL
        report.add({
            "query": qid,
            "cost_best": cost_best,
            "syntactic_best": syntactic_best,
            "verdict": verdict,
            "rule": c.rule,
            "reason": reason,
        })
        report.progress(f"{qid} cost {cost_best} syntactic {syntactic_best} {verdict} {reason}".rstrip())
    total_cost, total_syntactic, factor = reorder_totals(report.items)
    report.fields.update(total_cost=total_cost, total_syntactic=total_syntactic, factor=factor)
    shown = "n/a" if factor is None else f"{factor:.3f}"
    good = equal == QUERY_COUNT and factor is not None and factor >= REORDER_MIN_FACTOR
    return (PASS if good else FAIL), f"{equal}/{QUERY_COUNT} equal, factor {shown}"


# cardinality

MEDIAN_Q_MAX = 2
P90_Q_MAX = 10
NULLS_Q_MAX = 3
EXPLAIN_ANALYZE = ("--explain-analyze",)
NO_OPTIMIZER = ("--no-optimizer",)
JOIN_OPS = ("HashJoin", "NestedLoopJoin")
INNER_KINDS = ("inner", "cross")
PLAIN_OPS = ("Scan", "Filter", "Project")


def is_inner_join(node: dict) -> bool:
    return node["op"] in JOIN_OPS and node.get("kind") in INNER_KINDS


def is_plain_subtree(node: dict) -> bool:
    """Only `Scan`, `Filter`, `Project` and inner joins from `node` down."""
    if node["op"] not in PLAIN_OPS and not is_inner_join(node):
        return False
    return all(is_plain_subtree(child) for child in node["children"])


def measurable_joins(node: dict) -> list[dict]:
    """The inner joins of an EXPLAIN tree, in preorder, that a plain `count(*)` can measure."""
    found = [node] if is_inner_join(node) and is_plain_subtree(node) else []
    for child in node["children"]:
        found.extend(measurable_joins(child))
    return found


def node_count_sql(node: dict) -> str:
    """`SELECT count(*) FROM <relations with aliases> WHERE <predicates>` of a join node."""
    relations = ", ".join(f"{r['table']} AS {r['alias']}" for r in node["relations"])
    predicates = list(dict.fromkeys(node["predicates"]))
    where = f" WHERE {' AND '.join(predicates)}" if predicates else ""
    return f"SELECT count(*) FROM {relations}{where}"


def q_error(estimate: float, actual: float) -> float:
    """`q = max(e/a, a/e)` with `e, a >= 1` (§16)."""
    e, a = max(float(estimate), 1.0), max(float(actual), 1.0)
    return max(e / a, a / e)


def median(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def percentile_90(values: list[float]) -> float | None:
    """By nearest rank: `sorted(q)[ceil(0.9·n) − 1]` (§17.1)."""
    return sorted(values)[math.ceil(0.9 * len(values)) - 1] if values else None


def cardinality_stats(items: list[dict]) -> dict:
    """`median_q`, `p90_q` (TPC-H) and `max_q_nulls` over the items that have a `q`."""
    tpch = [i["q"] for i in items if i["suite"] == "tpch" and i.get("q") is not None]
    nulls = [i["q"] for i in items if i["suite"] == "nulls" and i.get("q") is not None]
    return {"median_q": median(tpch), "p90_q": percentile_90(tpch), "max_q_nulls": max(nulls, default=None)}


def cardinality_passes(items: list[dict], stats: dict) -> bool:
    """Every count equal to DuckDB's, both suites measured and the thresholds of §16."""
    if any(i["verdict"] != compare.EQUAL for i in items) or None in stats.values():
        return False
    return (sum(i["suite"] == "nulls" for i in items) == len(NULL_QUERIES)
            and stats["median_q"] <= MEDIAN_Q_MAX
            and stats["p90_q"] <= P90_Q_MAX
            and stats["max_q_nulls"] <= NULLS_Q_MAX)


# The NULL suite of §17: every non-NULL `na.k` matches exactly one row of `nb`.

NULL_SUITE_SEED = 1
NA_ROWS = 200_000
NB_ROWS = 20_000
VALUE_RANGE = 1000
NULL_TABLES = {"na": "CREATE TABLE na (k BIGINT, v BIGINT)", "nb": "CREATE TABLE nb (k BIGINT, w BIGINT)"}
NULL_JOIN = "SELECT count(*) FROM na JOIN nb ON na.k = nb.k"
NULL_FILTERS = (
    "",
    "na.v < 100",
    "na.v >= 500",
    "nb.w < 100",
    "nb.w BETWEEN 200 AND 799",
    "na.v < 500 AND nb.w < 500",
    "na.k IS NOT NULL",
    "na.k <= 2000",
    "nb.k > 15000",
    "na.v % 2 = 0 AND nb.w > 250",
)
NULL_QUERIES = tuple(f"{NULL_JOIN} WHERE {f}" if f else NULL_JOIN for f in NULL_FILTERS)


def null_suite_rows(seed: int = NULL_SUITE_SEED) -> tuple[list[tuple], list[tuple]]:
    """`na`: exactly 90 % of `k` NULL, the rest uniform over the keys of `nb`; `nb`: `k` unique."""
    rng = random.Random(seed)
    with_key = set(rng.sample(range(NA_ROWS), NA_ROWS // 10))
    na = [(rng.randint(1, NB_ROWS) if i in with_key else None, rng.randrange(VALUE_RANGE)) for i in range(NA_ROWS)]
    keys = list(range(1, NB_ROWS + 1))
    rng.shuffle(keys)
    nb = [(k, rng.randrange(VALUE_RANGE)) for k in keys]
    return na, nb


def tsv_text(rows: list[tuple]) -> str:
    """Integer rows in the format of §13.1, with `\\N` for NULL."""
    return "".join("\t".join("\\N" if v is None else str(v) for v in row) + "\n" for row in rows)


def duckdb_copy_sql(table: str, file: Path) -> str:
    return f"COPY {table} FROM {sql_string(str(file))} (FORMAT csv, DELIMITER E'\\t', NULL '\\N', HEADER false);"


def build_null_suite(engines: Engines, report: Report, scratch: Path) -> tuple[Path, Path]:
    """The tables of the suite in a new veta database and a new DuckDB file under `scratch`."""
    if scratch.exists():
        shutil.rmtree(scratch)
    scratch.mkdir(parents=True)
    veta_db, duckdb_db = scratch / "veta-nulls", scratch / "nulls.duckdb"
    for (table, create), rows in zip(NULL_TABLES.items(), null_suite_rows()):
        file = scratch / f"{table}.tsv"
        file.write_text(tsv_text(rows), encoding="utf-8")
        step(report, f"create-{table}", engines.veta_sql(veta_db, create))
        step(report, f"load-{table}", engines.veta_load(veta_db, table, tsv=[file]))
        step(report, f"create-{table}", engines.duckdb(duckdb_db, f"{create};"))
        step(report, f"load-{table}", engines.duckdb(duckdb_db, duckdb_copy_sql(table, file)))
    return veta_db, duckdb_db


def explain_tree(run: EngineRun) -> dict:
    """The JSON tree of an `--explain-analyze` run; `ValueError` if it does not have one."""
    if not run.ok:
        raise ValueError(f"explain exited {run.code}: {run.error_tail()}")
    tree = json.loads(run.text)
    if not isinstance(tree, dict):
        raise ValueError("explain did not print an object")
    return tree


class Counter:
    """Measures join nodes of one suite: the estimate against both engines' `count(*)`."""

    def __init__(self, engines: Engines, report: Report, suite: str, veta_db: Path, duckdb_db: Path):
        self.engines = engines
        self.report = report
        self.suite = suite
        self.veta_db = veta_db
        self.duckdb_db = duckdb_db

    def explain(self, query: str, sql: str) -> dict | None:
        """The tree of `sql`; a failure becomes an error item, since no node can be measured."""
        run = self.engines.veta_sql(self.veta_db, sql, extra=EXPLAIN_ANALYZE)
        self.report.raw("veta", f"{query}.explain", run.stdout)
        try:
            return explain_tree(run)
        except ValueError as e:
            self.error(query, str(e))
            return None

    def error(self, query: str, reason: str) -> None:
        self.report.add({
            "suite": self.suite, "query": query, "relations": [], "est_rows": None,
            "veta_count": None, "duckdb_count": None, "q": None, "verdict": compare.ERROR, "reason": reason,
        })
        self.report.progress(f"{query} {compare.ERROR} {reason}")

    def measure(self, query: str, item: str, node: dict, sql: str) -> None:
        veta = self.engines.veta_sql(self.veta_db, sql, extra=NO_OPTIMIZER)
        duckdb = self.engines.duckdb(self.duckdb_db, sql)
        self.report.raw("veta", item, veta.stdout)
        self.report.raw("duckdb", item, duckdb.stdout)
        veta_count, duckdb_count = single_count(veta), single_count(duckdb)
        q = q_error(node["est_rows"], veta_count) if veta_count is not None else None
        if veta_count is None or duckdb_count is None:
            verdict = compare.ERROR
            reason = f"veta exited {veta.code}: {veta.error_tail()}; duckdb exited {duckdb.code}: {duckdb.error_tail()}"
        elif veta_count != duckdb_count:
            verdict, reason = compare.DIFFERENT, f"count {veta_count} != DuckDB {duckdb_count}"
        else:
            verdict, reason = compare.EQUAL, ""
        self.report.add({
            "suite": self.suite,
            "query": query,
            "relations": node["relations"],
            "est_rows": node["est_rows"],
            "veta_count": veta_count,
            "duckdb_count": duckdb_count,
            "q": q,
            "verdict": verdict,
            "reason": reason,
            "sql": sql,
        })
        self.report.progress(f"{item} est {node['est_rows']} count {veta_count} q {q} {verdict} {reason}".rstrip())


def cardinality(args, p: Paths, report: Report):
    """The error q of the join nodes of TPC-H SF1 and of the NULL suite, against §16."""
    engines = Engines(p)
    dbs = tpch_databases(p, 1)
    require_loaded(engines, dbs, 1, "veta", "duckdb")
    counter = Counter(engines, report, "tpch", dbs.veta, dbs.duckdb)
    for qid, sql in read_queries(p).items():
        tree = counter.explain(qid, sql)
        if tree is None:
            continue
        for n, node in enumerate(measurable_joins(tree), 1):
            counter.measure(qid, f"{qid}.node{n}", node, node_count_sql(node))
    try:
        veta_db, duckdb_db = build_null_suite(engines, report, p.scratch / report.name)
    except LoadError as e:
        raise Infra(f"NULL suite: {e}") from e
    counter = Counter(engines, report, "nulls", veta_db, duckdb_db)
    for n, sql in enumerate(NULL_QUERIES, 1):
        name = f"n{n:02d}"
        tree = counter.explain(name, sql)
        if tree is None:
            continue
        joins = measurable_joins(tree)
        if joins:
            counter.measure(name, name, joins[0], sql)
        else:
            counter.error(name, "the plan has no measurable join node")
    stats = cardinality_stats(report.items)
    report.fields.update(stats)
    summary = ", ".join(f"{name} {'n/a' if value is None else f'{value:.3f}'}" for name, value in stats.items())
    summary += f", {len(report.items)} nodes"
    return (PASS if cardinality_passes(report.items, stats) else FAIL), summary
