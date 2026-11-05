"""`tlp` and `optimizer-oracle` (contract §17) over the workload of `gen.py`.

TLP runs a query and its partitions `p`, `NOT p`, `p IS NULL`, and rebuilds the result from
the partitions in the harness, since veta has no `UNION`. The optimizer oracle compares each
query with and without `--no-optimizer` under the rules of §15.2. An engine error in any run
is a discrepancy too, also counted in `errors`.
"""

import argparse
import shutil
from collections.abc import Callable, Iterator
from decimal import Decimal
from pathlib import Path

from harness import clickbench, compare, gen, sqltext, tpch
from harness.clickbench import LoadError
from harness.config import Paths
from harness.engines import EngineRun, Engines, clickbench_databases, tpch_databases
from harness.normalize import parse_output
from harness.report import FAIL, PASS, Infra, Report

DEFAULT_QUERIES = 1000
DEFAULT_SEED = 1
# §16: fewer random queries than this never pass, however few discrepancies they show.
MIN_RANDOM_QUERIES = 1000
BENCH_QUERIES = clickbench.QUERY_COUNT + tpch.QUERY_COUNT
# A hung random query must not stop the run: it times out as an engine error. The benchmarks
# run without a limit, as in their own commands.
RANDOM_QUERY_TIMEOUT_S = 300
PROGRESS_EVERY = 100
PARTITION_NAMES = ("p", "not-p", "p-is-null")
NO_OPTIMIZER_SUFFIX = ".no-optimizer"

Runner = Callable[[str], EngineRun]
Raw = Callable[[str, str, bytes], object]


def query_count(value: str) -> int:
    """`--queries`: a positive integer."""
    try:
        n = int(value)
    except ValueError:
        raise argparse.ArgumentTypeError(f"not an integer: {value}") from None
    if n <= 0:
        raise argparse.ArgumentTypeError(f"not a positive integer: {value}")
    return n


# Recomposing the partitions


def add_numbers(a, b):
    """Exact on integers and on decimals: float addition could leave the tolerance of §16 when
    a large sum cancels out. Printed decimals survive `repr`, so the sum is the exact one."""
    if isinstance(a, int) and isinstance(b, int):
        return a + b
    return float(Decimal(repr(a)) + Decimal(repr(b)))


def merge_cells(aggregate: str, a, b):
    """Two partial results of one aggregate: NULL means that a partition had no non-NULL value."""
    if aggregate == "COUNT":
        return a + b
    if a is None or b is None:
        return b if a is None else a
    match aggregate:
        case "SUM":
            return add_numbers(a, b)
        case "MIN":
            return min(a, b)
        case "MAX":
            return max(a, b)
    raise ValueError(f"unknown aggregate: {aggregate}")


def merge_aggregates(rows: list[tuple], aggregates: tuple[str | None, ...]) -> list[tuple]:
    """Re-groups by the columns that are not aggregates, with NULL as one more key value."""
    groups: dict[tuple, tuple] = {}
    for row in rows:
        key = tuple(v for v, a in zip(row, aggregates) if a is None)
        seen = groups.get(key)
        if seen is None:
            groups[key] = row
        else:
            groups[key] = tuple(s if a is None else merge_cells(a, s, v) for s, v, a in zip(seen, row, aggregates))
    return list(groups.values())


def sort_rows(rows: list[tuple], terms: tuple[gen.OrderTerm, ...]) -> list[tuple]:
    """§4: NULL last in ASC and first in DESC, unless `NULLS FIRST|LAST` says otherwise."""
    ordered = list(rows)
    for term in reversed(terms):
        nulls_first = term.nulls == "FIRST" if term.nulls else term.descending
        values = [r for r in ordered if r[term.column] is not None]
        nulls = [r for r in ordered if r[term.column] is None]
        values.sort(key=lambda r: r[term.column], reverse=term.descending)
        ordered = nulls + values if nulls_first else values + nulls
    return ordered


def recompose(q: gen.GenQuery, partitions: list[list[tuple]]) -> list[tuple]:
    """The result of `q` from those of its partitions, which have no ORDER BY nor LIMIT."""
    rows = [row for part in partitions for row in part]
    if q.aggregated:
        rows = merge_aggregates(rows, tuple(c.aggregate for c in q.columns))
    elif q.distinct:
        rows = list(dict.fromkeys(rows))
    if q.order_by:
        rows = sort_rows(rows, q.order_by)
    if q.limit is not None:
        start = q.offset or 0
        rows = rows[start:start + q.limit]
    return rows


# Comparing


def result_diff(sql: str, whole: list[tuple], composed: list[tuple]) -> str | None:
    """The §15.2 rule of `sql`: only an order that the query does not fix may change."""
    q = sqltext.parse(sql)
    rule = compare.rule_for(q)
    if rule == compare.MULTISET:
        return compare.multiset_diff(whole, composed)
    plan = sqltext.order_plan(q)
    relax_first, relax_last = compare.border_groups(q, rule, len(composed))
    return compare.ordered_diff(whole, composed, plan.key_columns, relax_first, relax_last)


def run_rows(run: Runner, sql: str, item: str, raw: Raw | None) -> list[tuple]:
    result = run(sql)
    if raw is not None:
        raw("veta", item, result.stdout)
    if not result.ok:
        raise compare.QueryFailed(f"{item} exited {result.code}: {result.error_tail()}")
    return parse_output("veta", result.text)


def check_tlp(q: gen.GenQuery, item: str, run: Runner, raw: Raw | None = None) -> compare.Outcome:
    """One query against the union of its three partitions."""
    sql = q.sql
    rule = compare.MULTISET
    try:
        rule = compare.rule_for(sqltext.parse(sql))
        whole = run_rows(run, sql, item, raw)
        parts = [
            run_rows(run, text, f"{item}.{name}", raw)
            for name, text in zip(PARTITION_NAMES, q.partition_sqls())
        ]
        width = len(q.columns)
        for rows in (whole, *parts):
            if any(len(row) != width for row in rows):
                return compare.Outcome(compare.DIFFERENT, rule, f"a row does not have {width} cells")
        composed = recompose(q, parts)
        diff = result_diff(sql, whole, composed)
    except compare.QueryFailed as e:
        return compare.Outcome(compare.ERROR, rule, str(e))
    except sqltext.SqlTextError as e:
        return compare.Outcome(compare.ERROR, rule, f"query text: {e}")
    except (TypeError, ArithmeticError) as e:
        return compare.Outcome(compare.DIFFERENT, rule, f"partitions cannot be recomposed: {e}")
    return compare.Outcome(compare.EQUAL, rule, "") if diff is None else compare.Outcome(compare.DIFFERENT, rule, diff)


def check_on_off(item: str, sql: str, on: compare.Side, off: compare.Side, raw: Raw | None = None) -> compare.Outcome:
    c = compare.verify_query(item, on, off, sql, sql, raw=raw)
    return compare.Outcome(c.verdict, c.rule, c.reason)


def on_off_sides(run_veta: Callable[..., EngineRun], db: Path) -> tuple[compare.Side, compare.Side]:
    on = compare.Side("veta", lambda sql: run_veta(db, sql), "veta")
    off = compare.Side("veta", lambda sql: run_veta(db, sql, extra=tpch.NO_OPTIMIZER), "veta", NO_OPTIMIZER_SUFFIX)
    return on, off


# Tally and verdict


class Tally:
    """`counts` of §17.1; each item of the report is a discrepancy with its SQL."""

    def __init__(self, report: Report):
        self.report = report
        self.counts = {"queries": 0, "discrepancies": 0, "errors": 0}
        report.fields["counts"] = self.counts

    def record(self, query: str, sql: str, outcome: compare.Outcome, **extra) -> None:
        self.counts["queries"] += 1
        if outcome.verdict != compare.EQUAL:
            self.counts["discrepancies"] += 1
            self.counts["errors"] += outcome.verdict == compare.ERROR
            self.report.add({
                "query": query,
                "sql": sql,
                "rule": outcome.rule,
                "verdict": outcome.verdict,
                "reason": outcome.reason,
                **extra,
            })
            self.report.progress(f"{query} {outcome.verdict} {outcome.reason}")
        if self.counts["queries"] % PROGRESS_EVERY == 0:
            self.report.progress(f"{self.counts['queries']} queries, {self.counts['discrepancies']} discrepancies")

    def verdict(self, minimum: int) -> tuple[str, str]:
        c = self.counts
        summary = f"{c['queries']} queries, {c['discrepancies']} discrepancies, {c['errors']} errors"
        if c["queries"] < minimum:
            return FAIL, f"{summary}; fewer than the {minimum} queries required"
        return (PASS if c["discrepancies"] == 0 and c["errors"] == 0 else FAIL), summary


# The workload


def build_dataset(engines: Engines, report: Report, scratch: Path, seed: int, dataset: int) -> Path:
    """The tables of one dataset in a new veta database, each loaded in several batches."""
    directory = scratch / f"d{dataset}"
    directory.mkdir(parents=True)
    db = directory / "veta"
    try:
        for table in gen.generate_dataset(seed, dataset):
            tpch.step(report, f"d{dataset}.create-{table.name}", engines.veta_sql(db, table.create_sql))
            files = gen.write_batches(seed, dataset, table, directory)
            if files:
                tpch.step(report, f"d{dataset}.load-{table.name}", engines.veta_load(db, table.name, tsv=files))
    except LoadError as e:
        raise Infra(f"dataset {dataset}: {e}") from e
    return db


def random_workload(engines: Engines, report: Report, seed: int, count: int) -> Iterator[tuple[gen.GenQuery, Path]]:
    """Each query with the database of its dataset, built when first needed."""
    scratch = engines.paths.scratch / report.name
    if scratch.exists():
        shutil.rmtree(scratch)
    dbs: dict[int, Path] = {}
    for index in range(count):
        q = gen.generate_query(seed, index)
        if q.dataset not in dbs:
            dbs[q.dataset] = build_dataset(engines, report, scratch, seed, q.dataset)
        yield q, dbs[q.dataset]


def random_item(q: gen.GenQuery) -> str:
    return f"r{q.index:04d}"


def tlp(args, p: Paths, report: Report):
    """`tlp [--queries N] [--seed S]`: PASS with 0 discrepancies and 0 errors in ≥ 1000 queries."""
    engines = Engines(p, timeout=RANDOM_QUERY_TIMEOUT_S)
    engines.require("veta")
    report.fields["seed"] = args.seed
    tally = Tally(report)
    for q, db in random_workload(engines, report, args.seed, args.queries):
        item = random_item(q)
        outcome = check_tlp(q, item, lambda sql: engines.veta_sql(db, sql), report.raw)
        tally.record(item, q.sql, outcome, partition=q.partition, partitions=list(q.partition_sqls()))
    return tally.verdict(MIN_RANDOM_QUERIES)


def bench_queries(p: Paths) -> list[tuple[str, str, Path]]:
    """`(item, sql, database)` of the 43 + 22, in the text that veta runs unchanged."""
    engines = Engines(p)
    cb, sf1 = clickbench_databases(p), tpch_databases(p, 1)
    clickbench.require_loaded(engines, cb, "veta")
    tpch.require_loaded(engines, sf1, 1, "veta")
    texts = clickbench.read_queries(clickbench.query_files(p)[0])
    out = [(f"clickbench-{clickbench.query_id(n)}", sql, cb.veta) for n, sql in enumerate(texts, 1)]
    out += [(f"tpch-{qid}", sql, sf1.veta) for qid, sql in tpch.read_queries(p).items()]
    return out


def optimizer_oracle(args, p: Paths, report: Report):
    """`optimizer-oracle [--queries N] [--seed S]`: on and off equal in the random queries + 65."""
    benches = bench_queries(p)
    report.fields["seed"] = args.seed
    tally = Tally(report)
    engines = Engines(p, timeout=RANDOM_QUERY_TIMEOUT_S)
    for q, db in random_workload(engines, report, args.seed, args.queries):
        item = random_item(q)
        on, off = on_off_sides(engines.veta_sql, db)
        tally.record(item, q.sql, check_on_off(item, q.sql, on, off, report.raw))
    bench_engines = Engines(p)
    for item, sql, db in benches:
        on, off = on_off_sides(bench_engines.veta_sql, db)
        tally.record(item, sql, check_on_off(item, sql, on, off, report.raw))
        report.progress(f"{item} done")
    return tally.verdict(MIN_RANDOM_QUERIES + BENCH_QUERIES)
