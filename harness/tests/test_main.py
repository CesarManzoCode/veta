import contextlib
import io
import unittest

from harness.__main__ import build_parser


class ParserTest(unittest.TestCase):
    def test_commands(self):
        parser = build_parser()
        args = parser.parse_args(["fetch"])
        self.assertEqual(args.report_name, "fetch")
        args = parser.parse_args(["load", "clickbench", "--rebuild-veta"])
        self.assertEqual((args.report_name, args.rebuild_veta), ("load-clickbench", True))
        self.assertFalse(parser.parse_args(["load", "clickbench"]).rebuild_veta)

    def test_usage_error_exits_2(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as cm:
            build_parser().parse_args(["load"])
        self.assertEqual(cm.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
