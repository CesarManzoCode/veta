"""`mutants` (contract §18) and `all` (contract §17): both drive other harness commands as
subprocesses and judge them by their exit code and their RESULT line.

Each subprocess streams its output with a `<command>| ` prefix, so the only line starting
with `RESULT` is the driver's own; its untouched output also goes to
`runs/<driver>/logs/`. Decisions within the contract, labelled as design:

- A mutant is killed by a command that exits 1 (FAIL). Exit 0 (PASS) or 77 (SKIP) does not
  kill it; exit 2 (usage or infrastructure) means the verdict cannot be trusted, so
  `mutants` ends as an infrastructure error after recording every item.
- The commands run with the mutant replace their `runs/<command>/`; `mutants` moves those
  directories to `runs/mutants/<mutant>/<command>/` and restores the ones that were there
  before it started, so a run of `all` keeps the reports of the unmutated binary.
- `mutants` is SKIP (77) without `$VETA_DATA/bin/duckdb`: `clickbench-verify --against
  duckdb` and `cardinality` need it, and without them a survivor would mean nothing.
"""

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

from harness.config import Paths
from harness.report import EXIT_CODES, EXIT_USAGE, FAIL, PASS, SKIP, Infra, Report, Skip

# The catalog of §18: feature name without `mutant-`, and the crate that holds it.
MUTANTS = (
    ("prune-extra-granule", "veta-storage"),
    ("prune-wrong-bound", "veta-storage"),
    ("pushdown-outer", "veta-sql"),
    ("reorder-outer", "veta-sql"),
    ("estimate-ignore-null", "veta-sql"),
    ("codec-drop-last-block", "veta-codec"),
)

# The commands of §18 in their order: (name of the RESULT line, arguments).
MUTANT_COMMANDS = (
    ("codec-roundtrip", ["codec-roundtrip"]),
    ("pruning", ["pruning"]),
    ("clickbench-verify-duckdb", ["clickbench-verify", "--against", "duckdb"]),
    ("tpch-verify-sf1", ["tpch-verify", "--sf", "1"]),
    ("tlp", ["tlp"]),
    ("optimizer-oracle", ["optimizer-oracle"]),
    ("cardinality", ["cardinality"]),
)

# Every command of the table of §17 but `all`, in its order.
ALL_COMMANDS = (
    ("clickbench-verify-duckdb", ["clickbench-verify", "--against", "duckdb"]),
    ("clickbench-verify-clickhouse", ["clickbench-verify", "--against", "clickhouse"]),
    ("tpch-verify-sf1", ["tpch-verify", "--sf", "1"]),
    ("tpch-verify-sf10", ["tpch-verify", "--sf", "10"]),
    ("perf-clickbench", ["perf", "clickbench"]),
    ("perf-tpch", ["perf", "tpch"]),
    ("rss", ["rss"]),
    ("threads", ["threads"]),
    ("tlp", ["tlp"]),
    ("optimizer-oracle", ["optimizer-oracle"]),
    ("join-reorder", ["join-reorder"]),
    ("cardinality", ["cardinality"]),
    ("pruning", ["pruning"]),
    ("codec-roundtrip", ["codec-roundtrip"]),
    ("disk-bytes", ["disk-bytes"]),
    ("compressor", ["compressor"]),
    ("merge-equivalence", ["merge-equivalence"]),
    ("concurrent-reader", ["concurrent-reader"]),
    ("crash-merge", ["crash-merge"]),
    ("crash-insert", ["crash-insert"]),
    ("corruption", ["corruption"]),
    ("parquet-read-duckdb", ["parquet-read-duckdb"]),
    ("parquet-read-hits", ["parquet-read-hits"]),
    ("parquet-write-duckdb", ["parquet-write-duckdb"]),
    ("parquet-write-clickhouse", ["parquet-write-clickhouse"]),
    ("parquet-stats", ["parquet-stats"]),
    ("parquet-rowgroup-skip", ["parquet-rowgroup-skip"]),
    ("mutants", ["mutants"]),
    ("size", ["size"]),
    ("deps", ["deps"]),
)

RESULT_RE = re.compile(r"^RESULT (\S+) (PASS|FAIL|SKIP)(?: .*)?$")
STATUS_OF_EXIT = {code: status for status, code in EXIT_CODES.items()}


def feature(mutant: str) -> str:
    return f"mutant-{mutant}"


def parse_result(output: str) -> tuple[str, str] | None:
    """`(name, status)` of the last RESULT line of `output`, if any."""
    found = None
    for line in output.split("\n"):
        m = RESULT_RE.match(line.rstrip("\r"))
        if m:
            found = (m.group(1), m.group(2))
    return found


def run_harness(argv: list[str], env: dict, repo: Path, log: Path, prefix: str) -> tuple[int, str]:
    """Runs `python3 -m harness <argv>`, streaming its output prefixed; returns the exit code
    and the whole output, which also goes to `log`."""
    log.parent.mkdir(parents=True, exist_ok=True)
    chunks = []
    with open(log, "wb") as f:
        proc = subprocess.Popen(
            [sys.executable, "-m", "harness", *argv],
            cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        for line in proc.stdout:
            f.write(line)
            text = line.decode("utf-8", errors="surrogateescape")
            chunks.append(text)
            print(f"{prefix}| {text.rstrip()}".encode("utf-8", errors="replace").decode("utf-8"), flush=True)
        code = proc.wait()
    return code, "".join(chunks)


def outcome(name: str, code: int, output: str) -> dict:
    """The item of one command run: its RESULT name and status, falling back to the expected
    name and the status of its exit code when the command printed no RESULT line."""
    parsed = parse_result(output)
    command, status = parsed if parsed else (name, STATUS_OF_EXIT.get(code, FAIL))
    return {"command": command, "status": status, "exit_code": code}


# mutants


def kills(code: int) -> bool:
    return code == EXIT_CODES[FAIL]


def mutant_verdict(items: list[dict]) -> tuple[str, str]:
    """PASS if every mutant is killed."""
    survivors = [i["mutant"] for i in items if not i["killed"]]
    summary = f"{len(items) - len(survivors)}/{len(items)} killed"
    if survivors:
        summary += f"; survived: {', '.join(survivors)}"
    return (PASS if items and not survivors else FAIL), summary


def mutant_line(item: dict) -> str:
    if item["killed"]:
        return f"MUTANT {item['mutant']} KILLED by {','.join(item['by'])}"
    return f"MUTANT {item['mutant']} SURVIVED"


def run_mutants(build, run_command, report: Report) -> list[str]:
    """Builds and judges each mutant. `build(mutant) -> Path` returns the mutant binary;
    `run_command(mutant, binary, name, argv) -> exit code`. Returns the commands that ended
    with exit 2, as `<mutant>:<command>`."""
    infra = []
    for mutant, _crate in MUTANTS:
        report.progress(f"building {feature(mutant)}")
        binary = build(mutant)
        by = []
        for name, argv in MUTANT_COMMANDS:
            code = run_command(mutant, binary, name, argv)
            report.progress(f"{feature(mutant)} {name}: exit {code}")
            if kills(code):
                by.append(name)
            elif code == EXIT_USAGE:
                infra.append(f"{mutant}:{name}")
        item = report.add({"mutant": mutant, "killed": bool(by), "by": by})
        print(mutant_line(item), flush=True)
    return infra


def cargo_build(p: Paths, mutant: str) -> Path:
    target = p.repo / "target" / "mutants" / mutant
    cmd = ["cargo", "build", "--release", "-p", "veta-cli", "--features", feature(mutant),
           "--target-dir", str(target)]
    try:
        done = subprocess.run(cmd, cwd=p.repo)
    except FileNotFoundError as e:
        raise Infra(f"cargo missing: {e}") from e
    binary = target / "release" / "veta"
    if done.returncode != 0 or not binary.is_file():
        raise Infra(f"build of {feature(mutant)} failed (exit {done.returncode})")
    return binary


class RunsKeeper:
    """Keeps the `runs/<command>/` that existed before `mutants` (moved to scratch) and files
    each mutated run under the report directory."""

    def __init__(self, p: Paths, report: Report, names):
        self.runs = p.runs
        self.report_dir = report.dir
        self.names = list(dict.fromkeys(names))
        self.backup = p.scratch / "mutants" / "baseline"

    def __enter__(self):
        if self.backup.exists():
            shutil.rmtree(self.backup)
        self.backup.mkdir(parents=True)
        for name in self.names:
            if (self.runs / name).exists():
                shutil.move(self.runs / name, self.backup / name)
        return self

    def file(self, mutant: str, name: str) -> None:
        src = self.runs / name
        if src.exists():
            dst = self.report_dir / mutant / name
            if dst.exists():
                shutil.rmtree(dst)
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.move(src, dst)

    def __exit__(self, *exc):
        for name in self.names:
            if (self.runs / name).exists():
                shutil.rmtree(self.runs / name)
            if (self.backup / name).exists():
                shutil.move(self.backup / name, self.runs / name)
        shutil.rmtree(self.backup, ignore_errors=True)
        return False


def mutants(args, p: Paths, report: Report):
    if not p.duckdb.is_file():
        raise Skip(f"duckdb missing: {p.duckdb} (python3 -m harness fetch)")

    with RunsKeeper(p, report, [name for name, _ in MUTANT_COMMANDS]) as keeper:
        def run_command(mutant, binary, name, argv):
            env = {**os.environ, "VETA_BIN": str(binary), "VETA_DATA": str(p.data)}
            log = report.dir / mutant / "logs" / f"{name}.log"
            code, _ = run_harness(argv, env, p.repo, log, f"{mutant} {name}")
            keeper.file(mutant, name)
            return code

        infra = run_mutants(lambda m: cargo_build(p, m), run_command, report)
    status, summary = mutant_verdict(report.items)
    if infra:
        raise Infra(f"{summary}; infrastructure errors in {', '.join(infra)}")
    return status, summary


# all


def all_verdict(items: list[dict]) -> tuple[str, str]:
    """PASS if every command is PASS or SKIP (77)."""
    ok = [i for i in items if i["status"] in (PASS, SKIP) and i["exit_code"] in (0, 77)]
    failed = [i["command"] for i in items if i not in ok]
    skipped = sum(1 for i in ok if i["status"] == SKIP)
    summary = f"{len(ok)}/{len(items)} PASS or SKIP ({skipped} SKIP)"
    if failed:
        summary += f"; failed: {', '.join(failed)}"
    return (PASS if items and not failed else FAIL), summary


def run_all(run_command, report: Report) -> None:
    """`run_command(name, argv) -> (exit code, output)` for every command of §17 in order."""
    for name, argv in ALL_COMMANDS:
        report.progress(f"running {name}")
        code, output = run_command(name, argv)
        item = report.add(outcome(name, code, output))
        report.progress(f"{item['command']} {item['status']} (exit {code})")


def all_commands(args, p: Paths, report: Report):
    env = {**os.environ, "VETA_BIN": str(p.veta_bin), "VETA_DATA": str(p.data)}

    def run_command(name, argv):
        return run_harness(argv, env, p.repo, report.dir / "logs" / f"{name}.log", name)

    run_all(run_command, report)
    return all_verdict(report.items)
