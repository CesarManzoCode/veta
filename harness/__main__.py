"""`python3 -m harness <command> [options]` (contract §14).

Each command is a function `(args, paths, report) -> (status, summary)`; this module
turns that, or a `Skip`/`Infra` exception, into the RESULT line and the exit code.
"""

import argparse
import resource
import sys
import traceback

from harness import clickbench, durability, fetch, meta, oracles, parquet_interop, tpch
from harness.config import paths
from harness.report import EXIT_USAGE, FAIL, SKIP, Infra, Report, Skip


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="python3 -m harness", description="veta verification harness")
    commands = parser.add_subparsers(dest="command", required=True, metavar="<command>")

    p = commands.add_parser("fetch", help="download and verify every pinned binary and data file")
    p.set_defaults(func=fetch.run, report_name="fetch")

    p = commands.add_parser("load", help="load a benchmark into the three engines")
    benches = p.add_subparsers(dest="bench", required=True, metavar="<bench>")
    b = benches.add_parser("clickbench", help="hits_0..hits_9 into veta, DuckDB and ClickHouse")
    b.add_argument("--rebuild-veta", action="store_true", help="rebuild only the veta database")
    b.set_defaults(func=clickbench.load, report_name="load-clickbench")
    b = benches.add_parser("tpch", help="one scale factor into DuckDB, veta and ClickHouse")
    b.add_argument("--sf", type=int, required=True, choices=tpch.SCALE_FACTORS)
    b.add_argument("--rebuild-veta", action="store_true", help="rebuild only the veta database")
    b.set_defaults(func=tpch.load, report_name=tpch.load_report_name)

    p = commands.add_parser("clickbench-verify", help="the 43 ClickBench queries against an oracle")
    p.add_argument("--against", required=True, choices=("duckdb", "clickhouse"))
    p.add_argument("--tamper", type=clickbench.tamper_query, metavar="Q",
                   help="change the oracle's first cell of query Q; the command must then FAIL")
    p.set_defaults(func=clickbench.verify, report_name=clickbench.verify_report_name)

    p = commands.add_parser("tpch-verify", help="the 22 TPC-H queries against tpch_answers() or DuckDB")
    p.add_argument("--sf", type=int, required=True, choices=tpch.SCALE_FACTORS)
    p.set_defaults(func=tpch.verify, report_name=tpch.verify_report_name)

    p = commands.add_parser("tlp", help="random queries against the union of their partitions")
    p.add_argument("--queries", type=oracles.query_count, default=oracles.DEFAULT_QUERIES, metavar="N")
    p.add_argument("--seed", type=int, default=oracles.DEFAULT_SEED, metavar="S")
    p.set_defaults(func=oracles.tlp, report_name="tlp")

    p = commands.add_parser("optimizer-oracle", help="random queries and the 65 with and without --no-optimizer")
    p.add_argument("--queries", type=oracles.query_count, default=oracles.DEFAULT_QUERIES, metavar="N")
    p.add_argument("--seed", type=int, default=oracles.DEFAULT_SEED, metavar="S")
    p.set_defaults(func=oracles.optimizer_oracle, report_name="optimizer-oracle")

    p = commands.add_parser("join-reorder", help="TPC-H SF1 with cost and syntactic join orders")
    p.set_defaults(func=tpch.join_reorder, report_name="join-reorder")

    p = commands.add_parser("cardinality", help="error q of the join nodes of TPC-H SF1 and of the NULL suite")
    p.set_defaults(func=tpch.cardinality, report_name="cardinality")

    p = commands.add_parser("pruning", help="Q37-Q43 read fewer granules with pruning, same results")
    p.set_defaults(func=clickbench.pruning, report_name="pruning")

    p = commands.add_parser("codec-roundtrip", help="veta codec-roundtrip on random columns and on hits")
    p.set_defaults(func=clickbench.codec_roundtrip, report_name="codec-roundtrip")

    p = commands.add_parser("merge-equivalence", help="the 43 ClickBench results before and after merge --final")
    p.set_defaults(func=durability.merge_equivalence, report_name="merge-equivalence")

    p = commands.add_parser("concurrent-reader", help="the 5 fixed queries while the merger runs")
    p.add_argument("--iterations", type=durability.iteration_count,
                   default=durability.DEFAULT_READER_ITERATIONS, metavar="N")
    p.set_defaults(func=durability.concurrent_reader, report_name="concurrent-reader")

    p = commands.add_parser("crash-merge", help="merge --final killed with SIGKILL, then recover")
    p.add_argument("--iterations", type=durability.iteration_count,
                   default=durability.DEFAULT_CRASH_ITERATIONS, metavar="N")
    p.set_defaults(func=durability.crash_merge, report_name="crash-merge")

    p = commands.add_parser("crash-insert", help="load --tsv killed with SIGKILL, then recover")
    p.add_argument("--iterations", type=durability.iteration_count,
                   default=durability.DEFAULT_CRASH_ITERATIONS, metavar="N")
    p.set_defaults(func=durability.crash_insert, report_name="crash-insert")

    p = commands.add_parser("corruption", help="one byte changed in each file type of a part")
    p.set_defaults(func=durability.corruption, report_name="corruption")

    p = commands.add_parser("disk-bytes", help="du -sb of hits after merge --final against ClickHouse")
    p.set_defaults(func=clickbench.disk_bytes, report_name="disk-bytes")

    p = commands.add_parser("compressor", help="vlz against lz4 -b1 on the dump-columns files of hits")
    p.set_defaults(func=clickbench.compressor, report_name="compressor")

    for name, func, text in (
        ("parquet-read-duckdb", parquet_interop.parquet_read_duckdb, "DuckDB writes Parquet, veta reads it"),
        ("parquet-read-hits", parquet_interop.parquet_read_hits, "counts and sums of each hits_N.parquet"),
        ("parquet-write-duckdb", parquet_interop.parquet_write_duckdb, "veta exports, DuckDB reads"),
        ("parquet-write-clickhouse", parquet_interop.parquet_write_clickhouse, "veta exports, ClickHouse reads"),
        ("parquet-stats", parquet_interop.parquet_stats, "footer statistics of the exported files"),
        ("parquet-rowgroup-skip", parquet_interop.parquet_rowgroup_skip, "row groups skipped by l_shipdate"),
    ):
        p = commands.add_parser(name, help=text)
        p.set_defaults(func=func, report_name=name)

    p = commands.add_parser("size", help="non-empty lines of the engine and of the harness")
    p.set_defaults(func=meta.size, report_name="size")

    p = commands.add_parser("deps", help="only workspace crates and the Python standard library")
    p.set_defaults(func=meta.deps, report_name="deps")
    return parser


def report_name(args) -> str:
    """A fixed name, or one derived from the options (`clickbench-verify-duckdb`)."""
    return args.report_name(args) if callable(args.report_name) else args.report_name


def run_command(func, name: str, args, p) -> int:
    report = Report(name, p.runs)
    try:
        status, summary = func(args, p, report)
    except Skip as e:
        return report.finish(SKIP, str(e))
    except Infra as e:
        return report.finish(FAIL, str(e), exit_code=EXIT_USAGE)
    except Exception as e:  # noqa: BLE001 - any crash is an infrastructure error
        traceback.print_exc()
        return report.finish(FAIL, f"internal error: {type(e).__name__}: {e}", exit_code=EXIT_USAGE)
    return report.finish(status, summary)


def raise_open_files_limit() -> None:
    """Soft RLIMIT_NOFILE up to the hard one, inherited by every engine run: veta keeps one
    descriptor per column read in each part (contract §6.3) and would otherwise re-execute
    itself to raise it, which the timings would count."""
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft != hard:
        try:
            resource.setrlimit(resource.RLIMIT_NOFILE, (hard, hard))
        except (ValueError, OSError):
            pass


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    raise_open_files_limit()
    return run_command(args.func, report_name(args), args, paths())


if __name__ == "__main__":
    sys.exit(main())
