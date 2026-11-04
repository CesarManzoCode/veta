"""Common command contract (contract §14.2): the RESULT line, exit codes, report.json, raw/."""

import json
import os
import shutil
from pathlib import Path

PASS, FAIL, SKIP = "PASS", "FAIL", "SKIP"
EXIT_CODES = {PASS: 0, FAIL: 1, SKIP: 77}
EXIT_USAGE = 2
ENGINES = ("veta", "duckdb", "clickhouse")


class Skip(Exception):
    """A reference binary (`duckdb`, `clickhouse`) or `lz4` is missing: exit 77."""


class Infra(Exception):
    """A usage or infrastructure error (no veta binary, data not loaded): exit 2."""


def result_line(name: str, status: str, summary: str) -> str:
    """`RESULT <name> <PASS|FAIL|SKIP> <summary>` on a single line."""
    if status not in EXIT_CODES:
        raise ValueError(f"invalid status: {status}")
    if not name or any(c.isspace() for c in name):
        raise ValueError(f"invalid command name: {name!r}")
    summary = " ".join(summary.split())
    return f"RESULT {name} {status} {summary}".rstrip()


def progress(name: str, message: str) -> None:
    print(f"{name}: {message}", flush=True)


class Report:
    """The run directory `runs/<name>/` of one command; created empty on construction."""

    def __init__(self, name: str, runs_dir: Path):
        self.name = name
        self.dir = Path(runs_dir) / name
        self.items: list[dict] = []
        self.fields: dict = {}
        if self.dir.exists():
            shutil.rmtree(self.dir)
        self.dir.mkdir(parents=True)

    def progress(self, message: str) -> None:
        progress(self.name, message)

    def raw(self, engine: str, item: str, stdout: bytes) -> Path:
        """Stores one engine run's untouched stdout as `raw/<engine>/<item>.out`."""
        if engine not in ENGINES:
            raise ValueError(f"unknown engine: {engine}")
        path = self.dir / "raw" / engine / f"{item}.out"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(stdout)
        return path

    def add(self, item: dict) -> dict:
        self.items.append(item)
        return item

    def document(self, status: str, summary: str) -> dict:
        reserved = {"command", "status", "summary", "items"} & self.fields.keys()
        if reserved:
            raise ValueError(f"reserved report fields: {sorted(reserved)}")
        return {
            "command": self.name,
            "status": status,
            "summary": summary,
            "items": self.items,
            **self.fields,
        }

    def finish(self, status: str, summary: str, exit_code: int | None = None) -> int:
        """Writes report.json, prints the RESULT line and returns the exit code."""
        line = result_line(self.name, status, summary)
        path = self.dir / "report.json"
        part = path.with_name("report.json.part")
        with open(part, "w", encoding="utf-8") as f:
            json.dump(self.document(status, " ".join(summary.split())), f, indent=2, ensure_ascii=False)
            f.write("\n")
        os.replace(part, path)
        print(line, flush=True)
        return EXIT_CODES[status] if exit_code is None else exit_code
