import contextlib
import io
import resource
import unittest

from harness.__main__ import build_parser, raise_open_files_limit, report_name


class ParserTest(unittest.TestCase):
    def test_commands(self):
        parser = build_parser()
        args = parser.parse_args(["fetch"])
        self.assertEqual(args.report_name, "fetch")
        args = parser.parse_args(["load", "clickbench", "--rebuild-veta"])
        self.assertEqual((args.report_name, args.rebuild_veta), ("load-clickbench", True))
        self.assertFalse(parser.parse_args(["load", "clickbench"]).rebuild_veta)

    def test_verification_commands(self):
        parser = build_parser()
        args = parser.parse_args(["clickbench-verify", "--against", "duckdb"])
        self.assertEqual((report_name(args), args.tamper), ("clickbench-verify-duckdb", None))
        args = parser.parse_args(["clickbench-verify", "--against", "clickhouse", "--tamper", "7"])
        self.assertEqual((report_name(args), args.tamper), ("clickbench-verify-clickhouse", "q07"))
        for name in ("pruning", "codec-roundtrip", "size", "deps"):
            self.assertEqual(report_name(parser.parse_args([name])), name)

    def test_bad_tamper_and_oracle_are_usage_errors(self):
        for argv in (["clickbench-verify", "--against", "duckdb", "--tamper", "44"],
                     ["clickbench-verify", "--against", "sqlite"],
                     ["clickbench-verify"]):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as cm:
                build_parser().parse_args(argv)
            self.assertEqual(cm.exception.code, 2)

    def test_usage_error_exits_2(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as cm:
            build_parser().parse_args(["load"])
        self.assertEqual(cm.exception.code, 2)


class OpenFilesLimitTest(unittest.TestCase):
    def test_soft_limit_rises_to_hard(self):
        before = resource.getrlimit(resource.RLIMIT_NOFILE)
        self.addCleanup(resource.setrlimit, resource.RLIMIT_NOFILE, before)
        hard = before[1]
        low = 64 if hard == resource.RLIM_INFINITY else min(64, hard)
        resource.setrlimit(resource.RLIMIT_NOFILE, (low, hard))
        raise_open_files_limit()
        self.assertEqual(resource.getrlimit(resource.RLIMIT_NOFILE), (hard, hard))


if __name__ == "__main__":
    unittest.main()
