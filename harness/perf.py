"""Performance, memory and threads (§16, §17): `perf clickbench|tpch`, `rss` and `threads`."""

import csv
import io
import math
from pathlib import Path

from harness import clickbench, tpch
from harness.clickbench import query_id
from harness.config import Paths
from harness.engines import EngineRun, Engines, clickbench_databases, tpch_databases, veta_sql_command
from harness.report import FAIL, PASS, Infra, Report

RUNS = 3
BENCHES = ("clickbench", "tpch")
PERF_SF = 10
REFERENCES = ("clickhouse", "duckdb")
CSV_ENGINES = ("veta", "clickhouse", "duckdb")
CSV_HEADER = ("query", "engine", "run1", "run2", "run3", "best")
# A hung engine process must not stall the whole command; far above any expected query time.
QUERY_TIMEOUT = 1800

# §16
RATIO_OFFSET = 0.01
MAX_RATIO = 15.0
MAX_GEOMEAN = 4.0
RSS_CAP_KB = 10_485_760
TIME_BIN = "/usr/bin/time"
TIME_ARGS = ("-f", "%M")
THREADS_LOW, THREADS_HIGH = 1, 12
MIN_SPEEDUP = 3.0


# Arithmetic of §16

def best_of(seconds: list[float | None]) -> float | None:
    """The best of the `RUNS` runs; `None` unless every run finished with a time."""
    if len(seconds) < RUNS or any(s is None for s in seconds):
        return None
    return min(seconds)


def ratio(t_veta: float, t_ref: float) -> float:
    """`r = (t_veta + 0.01) / (t_ref + 0.01)`; defined even for times of 0."""
    return (t_veta + RATIO_OFFSET) / (t_ref + RATIO_OFFSET)


def geomean(values: list[float]) -> float | None:
    if not values or any(v <= 0 for v in values):
        return None
    return math.exp(sum(math.log(v) for v in values) / len(values))


def ratios(best: dict[str, dict[str, float | None]], reference: str) -> dict:
    """`{"per_query": {q: r}, "geomean": g}` against one reference; `r` and `g` are `None`
    when a time is missing."""
    per_query = {}
    for qid, times in best.items():
        t_veta, t_ref = times.get("veta"), times.get(reference)
        per_query[qid] = None if t_veta is None or t_ref is None else ratio(t_veta, t_ref)
    values = list(per_query.values())
    g = geomean(values) if values and None not in values else None
    return {"per_query": per_query, "geomean": g}


def ratios_pass(r: dict) -> bool:
    """Every query `r <= 15` and geomean `<= 4` (§16)."""
    values = list(r["per_query"].values())
    if not values or None in values or r["geomean"] is None:
        return False
    return max(values) <= MAX_RATIO and r["geomean"] <= MAX_GEOMEAN


def perf_verdict(all_ratios: dict[str, dict]) -> str:
    """PASS only if the thresholds hold against ClickHouse and against DuckDB separately."""
    return PASS if all(ref in all_ratios and ratios_pass(all_ratios[ref]) for ref in REFERENCES) else FAIL


def format_seconds(t: float | None) -> str:
    return "" if t is None else f"{t:.6f}"


def times_csv(runs: dict[str, dict[str, list[float | None]]]) -> str:
    """`query,engine,run1,run2,run3,best`: one row per query and engine, in seconds; a run
    that failed leaves its cell, and the best, empty."""
    out = io.StringIO()
    writer = csv.writer(out, lineterminator="\n")
    writer.writerow(CSV_HEADER)
    for qid, by_engine in runs.items():
        for engine in CSV_ENGINES:
            if engine not in by_engine:
                continue
            seconds = list(by_engine[engine]) + [None] * (RUNS - len(by_engine[engine]))
            writer.writerow([qid, engine, *map(format_seconds, seconds[:RUNS]), format_seconds(best_of(seconds))])
    return out.getvalue()


def parse_max_rss(stderr: bytes) -> int | None:
    """KB of `/usr/bin/time -f %M`: the last non-empty stderr line, after the command's own
    stderr (and GNU time's `Command exited with non-zero status N`)."""
    lines = [line.strip() for line in stderr.split(b"\n") if line.strip()]
    if not lines or not lines[-1].isdigit():
        return None
    return int(lines[-1])


def rss_verdict(items: list[dict], expected: int) -> str:
    """Each of the `expected` queries ran and stayed at or under 10 GiB."""
    if len(items) != expected:
        return FAIL
    ok = all(i["exit_code"] == 0 and i["max_rss_kb"] is not None and i["max_rss_kb"] <= RSS_CAP_KB for i in items)
    return PASS if ok else FAIL


def speedup(t1: float | None, t12: float | None) -> float | None:
    return t1 / t12 if t1 is not None and t12 else None


def threads_verdict(factor: float | None) -> str:
    return PASS if factor is not None and factor >= MIN_SPEEDUP else FAIL


def total(values: list[float | None]) -> float | None:
    return sum(values) if values and None not in values else None


def show(value: float | None, digits: int = 3) -> str:
    return "n/a" if value is None else f"{value:.{digits}f}"


# Running

def repeat(report: Report, engine: str, item: str, run, sql: str) -> tuple[list[float | None], list[int]]:
    """`RUNS` executions of `sql`; the raw stdout of each goes to `raw/<engine>/<item>[.runN].out`.
    A run that does not exit with 0 has no time."""
    seconds, codes = [], []
    for n in range(1, RUNS + 1):
        result: EngineRun = run(sql)
        report.raw(engine, item if n == 1 else f"{item}.run{n}", result.stdout)
        codes.append(result.code)
        seconds.append(result.seconds if result.ok else None)
        if not result.ok:
            report.progress(f"{item} {engine} run {n} exited {result.code}: {result.error_tail()}")
    return seconds, codes


def clickbench_workload(engines: Engines, p: Paths) -> list[tuple[str, dict[str, tuple]]]:
    """`(qid, {engine: (runner, sql)})` for the 43 queries: veta runs the ClickHouse text
    unchanged, each oracle its own text with the rewrites of §15.4."""
    dbs = clickbench_databases(p)
    clickbench.require_loaded(engines, dbs, "veta", *REFERENCES)
    clickhouse_file, duckdb_file = clickbench.query_files(p)
    veta_texts = clickbench.read_queries(clickhouse_file)
    duckdb_texts = clickbench.read_queries(duckdb_file)
    ch_texts = clickbench.oracle_queries("clickhouse", veta_texts, duckdb_texts)
    dd_texts = clickbench.oracle_queries("duckdb", veta_texts, duckdb_texts)
    runners = {
        "veta": lambda sql: engines.veta_sql(dbs.veta, sql),
        "clickhouse": lambda sql: engines.clickhouse(sql, dbs.clickhouse_database, dbs.clickhouse_path),
        "duckdb": lambda sql: engines.duckdb(dbs.duckdb, sql),
    }
    texts = {"veta": veta_texts, "clickhouse": ch_texts, "duckdb": dd_texts}
    return [
        (query_id(n), {e: (runners[e], texts[e][n - 1]) for e in CSV_ENGINES})
        for n in range(1, clickbench.QUERY_COUNT + 1)
    ]


def tpch_workload(engines: Engines, p: Paths) -> list[tuple[str, dict[str, tuple]]]:
    """The 22 TPC-H SF10 queries, the same text in every engine."""
    dbs = tpch_databases(p, PERF_SF)
    for engine in CSV_ENGINES:
        engines.require(engine)
    tpch.require_loaded(engines, dbs, PERF_SF, "veta", "duckdb")
    if not dbs.clickhouse_path.exists():
        raise Infra(f"clickhouse: {dbs.clickhouse_path} not found; run python3 -m harness load tpch --sf {PERF_SF}")
    runners = {
        "veta": lambda sql: engines.veta_sql(dbs.veta, sql),
        "clickhouse": lambda sql: engines.clickhouse(sql, dbs.clickhouse_database, dbs.clickhouse_path),
        "duckdb": lambda sql: engines.duckdb(dbs.duckdb, sql),
    }
    return [(qid, {e: (runners[e], sql) for e in CSV_ENGINES}) for qid, sql in tpch.read_queries(p).items()]


def perf_report_name(args) -> str:
    return f"perf-{args.bench}"


def perf(args, p: Paths, report: Report):
    """Each query 3 times in each engine; `times.csv` and the ratios against each reference."""
    engines = Engines(p, timeout=QUERY_TIMEOUT)
    workload = clickbench_workload(engines, p) if args.bench == "clickbench" else tpch_workload(engines, p)
    runs: dict[str, dict[str, list[float | None]]] = {}
    best: dict[str, dict[str, float | None]] = {}
    for qid, by_engine in workload:
        runs[qid], best[qid] = {}, {}
        codes = {}
        for engine, (runner, sql) in by_engine.items():
            seconds, codes[engine] = repeat(report, engine, qid, runner, sql)
            runs[qid][engine] = seconds
            best[qid][engine] = best_of(seconds)
        r = {ref: ratio(best[qid]["veta"], best[qid][ref])
             if best[qid]["veta"] is not None and best[qid][ref] is not None else None for ref in REFERENCES}
        report.add({"query": qid, "runs": runs[qid], "best": best[qid], "exit_codes": codes, "ratios": r})
        report.progress(f"{qid} " + " ".join(f"{e} {show(best[qid][e])}" for e in CSV_ENGINES)
                        + " " + " ".join(f"r_{ref} {show(r[ref], 2)}" for ref in REFERENCES))
    (report.dir / "times.csv").write_text(times_csv(runs), encoding="utf-8")
    all_ratios = {ref: ratios(best, ref) for ref in REFERENCES}
    report.fields.update(bench=args.bench, ratios=all_ratios)
    if args.bench == "tpch":
        report.fields["scale_factor"] = PERF_SF
    parts = []
    for ref in REFERENCES:
        values = list(all_ratios[ref]["per_query"].values())
        worst = max(values) if values and None not in values else None
        parts.append(f"vs {ref}: geomean {show(all_ratios[ref]['geomean'], 2)}, worst {show(worst, 2)}")
    return perf_verdict(all_ratios), "; ".join(parts)


def time_command(argv: list[str]) -> list[str]:
    return [TIME_BIN, *TIME_ARGS, *argv]


def rss(args, p: Paths, report: Report):
    """The 43 ClickBench queries and the 22 of TPC-H SF10 in veta under `/usr/bin/time -f %M`."""
    engines = Engines(p, timeout=QUERY_TIMEOUT)
    cb = clickbench_databases(p)
    sf10 = tpch_databases(p, PERF_SF)
    clickbench.require_loaded(engines, cb, "veta")
    tpch.require_loaded(engines, sf10, PERF_SF, "veta")
    if not Path(TIME_BIN).is_file():
        raise Infra(f"{TIME_BIN} missing (GNU time)")
    work = [(f"clickbench-{query_id(n)}", cb.veta, sql)
            for n, sql in enumerate(clickbench.read_queries(clickbench.query_files(p)[0]), 1)]
    work += [(f"tpch-{qid}", sf10.veta, sql) for qid, sql in tpch.read_queries(p).items()]
    for item, db, sql in work:
        run = engines._run("veta", time_command(veta_sql_command(p.veta_bin, db, sql)))
        report.raw("veta", item, run.stdout)
        kb = parse_max_rss(run.stderr)
        report.add({"query": item, "max_rss_kb": kb, "exit_code": run.code, "seconds": run.seconds,
                    "reason": "" if run.ok else run.error_tail()})
        report.progress(f"{item} {kb} KB exit {run.code}")
    measured = [i["max_rss_kb"] for i in report.items if i["max_rss_kb"] is not None]
    worst = max(measured) if measured else None
    failed = [i["query"] for i in report.items if i["exit_code"] != 0 or i["max_rss_kb"] is None]
    report.fields.update(max_rss_kb=worst, cap_kb=RSS_CAP_KB)
    summary = f"{len(report.items) - len(failed)}/{len(work)} ran, max {worst} KB (cap {RSS_CAP_KB} KB)"
    if failed:
        summary += f"; failed: {', '.join(failed)}"
    return rss_verdict(report.items, len(work)), summary


def threads(args, p: Paths, report: Report):
    """The sum of the best-of-3 ClickBench times with `--threads 1` and with `--threads 12`."""
    engines = Engines(p, timeout=QUERY_TIMEOUT)
    dbs = clickbench_databases(p)
    clickbench.require_loaded(engines, dbs, "veta")
    texts = clickbench.read_queries(clickbench.query_files(p)[0])
    for n, sql in enumerate(texts, 1):
        qid = query_id(n)
        item = {"query": qid}
        for count in (THREADS_LOW, THREADS_HIGH):
            extra = ("--threads", str(count))
            seconds, codes = repeat(report, "veta", f"{qid}.threads{count}",
                                    lambda s, extra=extra: engines.veta_sql(dbs.veta, s, extra=extra), sql)
            item[f"t{count}_runs"] = seconds
            item[f"t{count}_best"] = best_of(seconds)
            item[f"t{count}_exit_codes"] = codes
        report.add(item)
        report.progress(f"{qid} t1 {show(item['t1_best'])} t12 {show(item['t12_best'])}")
    t1 = total([i[f"t{THREADS_LOW}_best"] for i in report.items])
    t12 = total([i[f"t{THREADS_HIGH}_best"] for i in report.items])
    factor = speedup(t1, t12)
    report.fields.update(t1=t1, t12=t12, factor=factor)
    return threads_verdict(factor), f"t1 {show(t1)} s, t12 {show(t12)} s, factor {show(factor, 2)}"

