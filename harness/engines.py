"""Adapters for veta, DuckDB and ClickHouse with the exact invocation of contract §14.4."""

import json
import os
import re
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from harness.config import Paths
from harness.report import Infra, Skip

TIMER_RE = re.compile(rb"^Run Time \(s\): real (\d+(?:\.\d+)?)\b")


def decode(data: bytes) -> str:
    """Engine output as text; bytes that are not UTF-8 survive as surrogates."""
    return data.decode("utf-8", errors="surrogateescape")


def split_rows(text: str) -> list[str]:
    """Rows split only on `\\n` (`splitlines` also cuts on `\\r`, `\\x1c`…)."""
    rows = text.split("\n")
    if rows and rows[-1] == "":
        rows.pop()
    return rows


def strip_duckdb_timer(stdout: bytes) -> tuple[bytes, float | None]:
    """Removes the `Run Time (s): real X …` lines; returns the rest and the summed X."""
    kept = []
    total = None
    for line in stdout.split(b"\n"):
        m = TIMER_RE.match(line)
        if m:
            total = (total or 0.0) + float(m.group(1))
        else:
            kept.append(line)
    return b"\n".join(kept), total


def clickhouse_time(stderr: bytes) -> float | None:
    """The `--time` value: the last non-empty line of stderr, in seconds."""
    lines = [line.strip() for line in stderr.split(b"\n") if line.strip()]
    if not lines:
        return None
    try:
        return float(lines[-1])
    except ValueError:
        return None


def veta_stats(stderr: bytes) -> dict | None:
    """The last `VETA_STATS {…}` line that `--stats` writes to stderr (contract §13)."""
    found = None
    for line in stderr.split(b"\n"):
        if line.startswith(b"VETA_STATS "):
            try:
                found = json.loads(line[len(b"VETA_STATS "):])
            except ValueError:
                continue
    return found if isinstance(found, dict) else None


def veta_sql_command(veta_bin: Path, db: Path, sql: str, stats: bool = False, extra=()) -> list[str]:
    argv = [str(veta_bin), "sql", "--db", str(db), "--format", "tsv"]
    if stats:
        argv.append("--stats")
    argv.extend(extra)
    argv.append(sql)
    return argv


def veta_load_command(veta_bin: Path, db: Path, table: str, parquet=(), tsv=(), batch_rows=None) -> list[str]:
    if bool(parquet) == bool(tsv):
        raise ValueError("load takes either parquet or tsv files")
    argv = [str(veta_bin), "load", "--db", str(db), "--table", table]
    if parquet:
        argv += ["--parquet", *map(str, parquet)]
    else:
        argv += ["--tsv", *map(str, tsv)]
    if batch_rows is not None:
        argv += ["--batch-rows", str(batch_rows)]
    return argv


def duckdb_env(base=None) -> dict:
    env = dict(os.environ if base is None else base)
    env["DUCKDB_AGENT_MODE"] = "0"
    return env


def duckdb_command(duckdb_bin: Path, db_file: Path | None, ext_dir: Path, sql: str) -> list[str]:
    """`db_file=None` runs on an in-memory database (used by `fetch` for `INSTALL`)."""
    argv = [str(duckdb_bin), "-no-agent", "-csv", "-noheader", "-nullvalue", "\\N"]
    if db_file is not None:
        argv.append(str(db_file))
    ext = str(ext_dir).replace("'", "''")
    argv += ["-c", f"SET extension_directories=['{ext}']", "-c", ".timer on", "-c", sql]
    return argv


def clickhouse_command(clickhouse_bin: Path, path: Path, sql: str, database: str | None = None) -> list[str]:
    argv = [
        str(clickhouse_bin), "local", "--path", str(path),
        "--session_timezone", "UTC", "--join_use_nulls", "1",
    ]
    if database is not None:
        argv += ["--database", database]
    argv += ["--format", "TSV", "--time", "-q", sql]
    return argv


@dataclass
class EngineRun:
    """One engine process. `stdout` is untouched; `body` drops DuckDB's timer lines."""

    engine: str
    argv: list[str]
    code: int
    stdout: bytes
    stderr: bytes
    seconds: float | None
    body: bytes

    @property
    def ok(self) -> bool:
        return self.code == 0

    @property
    def text(self) -> str:
        return decode(self.body)

    @property
    def rows(self) -> list[str]:
        return split_rows(self.text)

    def error_tail(self, lines: int = 5) -> str:
        tail = split_rows(decode(self.stderr))[-lines:]
        return " | ".join(t.strip() for t in tail if t.strip())


@dataclass(frozen=True)
class Databases:
    """Where each engine keeps one benchmark (contract §14.1)."""

    veta: Path
    duckdb: Path
    clickhouse_path: Path
    clickhouse_database: str | None


def clickbench_databases(p: Paths) -> Databases:
    return Databases(
        veta=p.db / "veta-clickbench",
        duckdb=p.db / "duckdb-clickbench.duckdb",
        clickhouse_path=p.db / "clickhouse",
        clickhouse_database="default",
    )


def tpch_databases(p: Paths, sf: int) -> Databases:
    if sf not in (1, 10):
        raise ValueError(f"unsupported scale factor: {sf}")
    return Databases(
        veta=p.db / f"veta-tpch-sf{sf}",
        duckdb=p.tpch / f"sf{sf}.duckdb",
        clickhouse_path=p.db / "clickhouse",
        clickhouse_database=f"tpch_sf{sf}",
    )


def is_executable(path: Path) -> bool:
    return path.is_file() and os.access(path, os.X_OK)


class Engines:
    """Runs the three engines from the configured paths."""

    def __init__(self, paths: Paths, timeout: float | None = None):
        self.paths = paths
        self.timeout = timeout

    def available(self, engine: str) -> bool:
        return is_executable(self.binary(engine))

    def binary(self, engine: str) -> Path:
        match engine:
            case "veta":
                return self.paths.veta_bin
            case "duckdb":
                return self.paths.duckdb
            case "clickhouse":
                return self.paths.clickhouse
        raise ValueError(f"unknown engine: {engine}")

    def require(self, engine: str) -> None:
        """Missing veta is an infrastructure error (2); a missing oracle is a SKIP (77)."""
        if self.available(engine):
            return
        if engine == "veta":
            raise Infra(f"veta binary missing: {self.paths.veta_bin} (cargo build --release -p veta-cli)")
        raise Skip(f"{engine} missing: {self.binary(engine)} (python3 -m harness fetch)")

    def _run(self, engine: str, argv: list[str], env=None) -> EngineRun:
        start = time.perf_counter()
        try:
            p = subprocess.run(argv, capture_output=True, env=env, timeout=self.timeout)
            code, out, err = p.returncode, p.stdout, p.stderr
        except subprocess.TimeoutExpired as e:
            code, out, err = 124, e.stdout or b"", (e.stderr or b"") + f"\ntimeout after {self.timeout}s".encode()
        wall = time.perf_counter() - start
        match engine:
            case "veta":
                body, seconds = out, wall
            case "duckdb":
                body, seconds = strip_duckdb_timer(out)
            case _:
                body, seconds = out, clickhouse_time(err)
        return EngineRun(engine, argv, code, out, err, seconds, body)

    def veta(self, *args) -> EngineRun:
        """Any other `veta` subcommand, e.g. `veta("codec-roundtrip", "--random", 2000)`."""
        return self._run("veta", [str(self.paths.veta_bin), *map(str, args)])

    def veta_sql(self, db: Path, sql: str, stats: bool = False, extra=()) -> EngineRun:
        return self._run("veta", veta_sql_command(self.paths.veta_bin, db, sql, stats, extra))

    def veta_load(self, db: Path, table: str, parquet=(), tsv=(), batch_rows=None) -> EngineRun:
        return self._run("veta", veta_load_command(self.paths.veta_bin, db, table, parquet, tsv, batch_rows))

    def duckdb(self, db_file: Path | None, sql: str) -> EngineRun:
        argv = duckdb_command(self.paths.duckdb, db_file, self.paths.duckdb_ext, sql)
        return self._run("duckdb", argv, env=duckdb_env())

    def clickhouse(self, sql: str, database: str | None = None, path: Path | None = None) -> EngineRun:
        path = path or self.paths.db / "clickhouse"
        path.mkdir(parents=True, exist_ok=True)
        return self._run("clickhouse", clickhouse_command(self.paths.clickhouse, path, sql, database))
