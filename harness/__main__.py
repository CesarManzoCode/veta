"""`python3 -m harness <command> [options]` (contract §14).

Each command is a function `(args, paths, report) -> (status, summary)`; this module
turns that, or a `Skip`/`Infra` exception, into the RESULT line and the exit code.
"""

import argparse
import sys
import traceback

from harness import clickbench, fetch, meta
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

    p = commands.add_parser("clickbench-verify", help="the 43 ClickBench queries against an oracle")
    p.add_argument("--against", required=True, choices=("duckdb", "clickhouse"))
    p.add_argument("--tamper", type=clickbench.tamper_query, metavar="Q",
                   help="change the oracle's first cell of query Q; the command must then FAIL")
    p.set_defaults(func=clickbench.verify, report_name=clickbench.verify_report_name)

    p = commands.add_parser("pruning", help="Q37-Q43 read fewer granules with pruning, same results")
    p.set_defaults(func=clickbench.pruning, report_name="pruning")

    p = commands.add_parser("codec-roundtrip", help="veta codec-roundtrip on random columns and on hits")
    p.set_defaults(func=clickbench.codec_roundtrip, report_name="codec-roundtrip")

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


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    return run_command(args.func, report_name(args), args, paths())


if __name__ == "__main__":
    sys.exit(main())
