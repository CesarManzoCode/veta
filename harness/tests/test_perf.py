import csv
import io
import math
import unittest

from harness import perf
from harness.report import FAIL, PASS


class RatioTest(unittest.TestCase):
    def test_ratio_with_offset(self):
        self.assertAlmostEqual(perf.ratio(0.99, 0.09), 10.0)
        self.assertAlmostEqual(perf.ratio(0.0, 0.0), 1.0)
        self.assertAlmostEqual(perf.ratio(0.09, 0.0), 10.0)
        self.assertAlmostEqual(perf.ratio(0.0, 0.99), 0.01)

    def test_best_of(self):
        self.assertEqual(perf.best_of([0.3, 0.1, 0.2]), 0.1)
        self.assertEqual(perf.best_of([0.0, 0.1, 0.2]), 0.0)
        self.assertIsNone(perf.best_of([0.3, None, 0.2]))
        self.assertIsNone(perf.best_of([0.3, 0.2]))

    def test_geomean(self):
        self.assertAlmostEqual(perf.geomean([2.0, 8.0]), 4.0)
        self.assertAlmostEqual(perf.geomean([1.0, 1.0, 1.0]), 1.0)
        self.assertIsNone(perf.geomean([]))

    def test_ratios_per_query_and_geomean(self):
        best = {
            "q01": {"veta": 0.0, "clickhouse": 0.0, "duckdb": 0.03},
            "q02": {"veta": 0.39, "clickhouse": 0.09, "duckdb": 0.39},
        }
        ch = perf.ratios(best, "clickhouse")
        self.assertAlmostEqual(ch["per_query"]["q01"], 1.0)
        self.assertAlmostEqual(ch["per_query"]["q02"], 4.0)
        self.assertAlmostEqual(ch["geomean"], 2.0)
        dd = perf.ratios(best, "duckdb")
        self.assertAlmostEqual(dd["per_query"]["q01"], 0.25)
        self.assertAlmostEqual(dd["per_query"]["q02"], 1.0)
        self.assertAlmostEqual(dd["geomean"], 0.5)

    def test_missing_time(self):
        best = {"q01": {"veta": None, "clickhouse": 0.1, "duckdb": 0.1},
                "q02": {"veta": 0.1, "clickhouse": 0.1, "duckdb": 0.1}}
        r = perf.ratios(best, "clickhouse")
        self.assertIsNone(r["per_query"]["q01"])
        self.assertIsNone(r["geomean"])
        self.assertFalse(perf.ratios_pass(r))


class VerdictTest(unittest.TestCase):
    @staticmethod
    def refs(per_query, geomean):
        r = {"per_query": per_query, "geomean": geomean}
        return {"clickhouse": r, "duckdb": dict(r)}

    def test_ratio_border(self):
        self.assertEqual(perf.perf_verdict(self.refs({"q01": 15.0, "q02": 1.0}, 3.9)), PASS)
        self.assertEqual(perf.perf_verdict(self.refs({"q01": math.nextafter(15.0, 16), "q02": 1.0}, 3.9)), FAIL)

    def test_geomean_border(self):
        self.assertEqual(perf.perf_verdict(self.refs({"q01": 4.0}, 4.0)), PASS)
        self.assertEqual(perf.perf_verdict(self.refs({"q01": 4.0}, math.nextafter(4.0, 5))), FAIL)

    def test_each_reference_separately(self):
        good = {"per_query": {"q01": 2.0}, "geomean": 2.0}
        bad = {"per_query": {"q01": 16.0}, "geomean": 2.0}
        self.assertEqual(perf.perf_verdict({"clickhouse": good, "duckdb": bad}), FAIL)
        self.assertEqual(perf.perf_verdict({"clickhouse": bad, "duckdb": good}), FAIL)
        self.assertEqual(perf.perf_verdict({"clickhouse": good}), FAIL)

    def test_rss_border(self):
        def items(kb, code=0):
            return [{"query": "q", "max_rss_kb": kb, "exit_code": code}]

        self.assertEqual(perf.rss_verdict(items(10_485_760), 1), PASS)
        self.assertEqual(perf.rss_verdict(items(10_485_761), 1), FAIL)
        self.assertEqual(perf.rss_verdict(items(1000, code=4), 1), FAIL)
        self.assertEqual(perf.rss_verdict(items(None), 1), FAIL)
        self.assertEqual(perf.rss_verdict(items(1000), 65), FAIL)

    def test_threads_border(self):
        self.assertEqual(perf.threads_verdict(perf.speedup(30.0, 10.0)), PASS)
        self.assertEqual(perf.threads_verdict(math.nextafter(3.0, 2)), FAIL)
        self.assertEqual(perf.threads_verdict(perf.speedup(30.0, None)), FAIL)
        self.assertEqual(perf.threads_verdict(perf.speedup(30.0, 0.0)), FAIL)

    def test_total(self):
        self.assertAlmostEqual(perf.total([1.0, 2.5]), 3.5)
        self.assertIsNone(perf.total([1.0, None]))
        self.assertIsNone(perf.total([]))


class TimesCsvTest(unittest.TestCase):
    def test_format(self):
        runs = {
            "q01": {"veta": [0.5, 0.25, 0.75], "duckdb": [0.1, 0.2, 0.3], "clickhouse": [0.0, 0.0, 0.0]},
            "q02": {"veta": [1.0, None, 1.5], "duckdb": [0.1, 0.2, 0.3], "clickhouse": [0.4, 0.5, 0.6]},
        }
        text = perf.times_csv(runs)
        lines = text.split("\n")
        self.assertEqual(lines[0], "query,engine,run1,run2,run3,best")
        self.assertEqual(lines[1], "q01,veta,0.500000,0.250000,0.750000,0.250000")
        self.assertEqual(lines[2], "q01,clickhouse,0.000000,0.000000,0.000000,0.000000")
        self.assertEqual(lines[3], "q01,duckdb,0.100000,0.200000,0.300000,0.100000")
        self.assertEqual(lines[4], "q02,veta,1.000000,,1.500000,")
        self.assertTrue(text.endswith("\n"))
        rows = list(csv.DictReader(io.StringIO(text)))
        self.assertEqual(len(rows), 6)
        self.assertEqual({r["engine"] for r in rows}, {"veta", "clickhouse", "duckdb"})
        self.assertEqual(float(rows[0]["best"]), 0.25)


class MaxRssTest(unittest.TestCase):
    def test_last_line(self):
        self.assertEqual(perf.parse_max_rss(b"123456\n"), 123456)
        self.assertEqual(perf.parse_max_rss(b"warning: something\n  2048 \n\n"), 2048)
        self.assertEqual(perf.parse_max_rss(b"error: memory limit\nCommand exited with non-zero status 4\n9876\n"), 9876)

    def test_unparsable(self):
        self.assertIsNone(perf.parse_max_rss(b""))
        self.assertIsNone(perf.parse_max_rss(b"12 KB\n"))
        self.assertIsNone(perf.parse_max_rss(b"/usr/bin/time: cannot run veta\n"))

    def test_time_command(self):
        self.assertEqual(perf.time_command(["veta", "sql"]), ["/usr/bin/time", "-f", "%M", "veta", "sql"])


if __name__ == "__main__":
    unittest.main()
