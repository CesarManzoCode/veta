"""ClickBench data: `load clickbench [--rebuild-veta]` (contract §14.3)."""

import shutil
import time
from pathlib import Path

from harness.config import HITS_PINS, HITS_ROWS, Paths
from harness.engines import Databases, EngineRun, Engines, clickbench_databases
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
