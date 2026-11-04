import contextlib
import io
import json
import re
import unittest
from pathlib import Path

from harness.report import EXIT_CODES, Report, result_line
from harness.tests.support import scratch_dir

RESULT_RE = re.compile(r"^RESULT (\S+) (PASS|FAIL|SKIP)(?: (.*))?$")


class ResultLineTest(unittest.TestCase):
    def test_format(self):
        self.assertEqual(result_line("fetch", "PASS", "41/41 verified"), "RESULT fetch PASS 41/41 verified")
        self.assertEqual(result_line("x", "SKIP", ""), "RESULT x SKIP")
        self.assertRegex(result_line("clickbench-verify-duckdb", "FAIL", "42/43 equal"), RESULT_RE)

    def test_summary_is_one_line(self):
        line = result_line("load-clickbench", "FAIL", "veta error:\n  boom\tbad")
        self.assertEqual(line, "RESULT load-clickbench FAIL veta error: boom bad")

    def test_rejects_bad_status_and_name(self):
        with self.assertRaises(ValueError):
            result_line("x", "OK", "")
        with self.assertRaises(ValueError):
            result_line("two words", "PASS", "")

    def test_exit_codes(self):
        self.assertEqual(EXIT_CODES, {"PASS": 0, "FAIL": 1, "SKIP": 77})


class ReportTest(unittest.TestCase):
    def finish(self, report, *args, **kwargs):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = report.finish(*args, **kwargs)
        return code, out.getvalue()

    def test_report_json_and_raw(self):
        with scratch_dir() as d:
            runs = Path(d)
            report = Report("load-clickbench", runs)
            raw = report.raw("duckdb", "q01", b"1,\xff\n")
            self.assertEqual(raw, runs / "load-clickbench" / "raw" / "duckdb" / "q01.out")
            self.assertEqual(raw.read_bytes(), b"1,\xff\n")
            report.add({"engine": "veta", "action": "loaded"})
            report.fields["scale_factor"] = 1
            code, out = self.finish(report, "PASS", "veta loaded")
            self.assertEqual(code, 0)
            self.assertEqual(out.splitlines()[-1], "RESULT load-clickbench PASS veta loaded")
            doc = json.loads((runs / "load-clickbench" / "report.json").read_text())
            self.assertEqual(doc, {"command": "load-clickbench", "status": "PASS", "summary": "veta loaded",
                                   "items": [{"engine": "veta", "action": "loaded"}], "scale_factor": 1})

    def test_each_run_replaces_the_directory(self):
        with scratch_dir() as d:
            first = Report("fetch", Path(d))
            first.raw("veta", "q01", b"x")
            second = Report("fetch", Path(d))
            self.assertFalse((second.dir / "raw").exists())
            code, _ = self.finish(second, "SKIP", "duckdb missing")
            self.assertEqual(code, 77)

    def test_explicit_exit_code_and_unknown_engine(self):
        with scratch_dir() as d:
            report = Report("load-clickbench", Path(d))
            with self.assertRaises(ValueError):
                report.raw("sqlite", "q01", b"")
            code, out = self.finish(report, "FAIL", "veta binary missing", exit_code=2)
            self.assertEqual(code, 2)
            self.assertIn("RESULT load-clickbench FAIL veta binary missing", out)

    def test_reserved_fields(self):
        with scratch_dir() as d:
            report = Report("x", Path(d))
            report.fields["status"] = "PASS"
            with self.assertRaises(ValueError):
                report.document("PASS", "")


if __name__ == "__main__":
    unittest.main()
