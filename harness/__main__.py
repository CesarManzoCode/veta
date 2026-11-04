"""`python3 -m harness <command> [options]` (contract §14).

Each command is a function `(args, paths, report) -> (status, summary)`; this module
turns that, or a `Skip`/`Infra` exception, into the RESULT line and the exit code.
"""

import argparse
import sys
import traceback

from harness import clickbench, fetch
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
    return parser


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
    return run_command(args.func, args.report_name, args, paths())


if __name__ == "__main__":
    sys.exit(main())
