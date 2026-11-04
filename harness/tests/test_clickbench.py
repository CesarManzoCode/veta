import argparse
import unittest
from pathlib import Path

from harness import clickbench
from harness.config import Paths
from harness.engines import EngineRun
from harness.report import Infra
from harness.tests.support import scratch_dir


class SqlTest(unittest.TestCase):
    def test_duckdb_insert(self):
        files = [Path(f"/data/clickbench/hits_{n}.parquet") for n in range(10)]
        sql = clickbench.duckdb_insert_sql(files)
        self.assertTrue(sql.startswith("INSERT INTO hits SELECT * REPLACE (make_date(EventDate) AS EventDate, "
                                       "epoch_ms(EventTime * 1000) AS EventTime, "
                                       "epoch_ms(ClientEventTime * 1000) AS ClientEventTime, "
                                       "epoch_ms(LocalEventTime * 1000) AS LocalEventTime) FROM read_parquet(["))
        self.assertTrue(sql.endswith("'/data/clickbench/hits_9.parquet'], binary_as_string=True);"))
        self.assertEqual(sql.count(".parquet'"), 10)

    def test_clickhouse_insert(self):
        self.assertEqual(
            clickbench.clickhouse_insert_sql(Path("/data/clickbench")),
            "INSERT INTO hits SELECT * FROM file('/data/clickbench/hits_{0..9}.parquet', Parquet)",
        )

    def test_quoting(self):
        self.assertEqual(clickbench.sql_string("it's"), "'it''s'")
        self.assertIn("database = 'default'", clickbench.clickhouse_state_sql("default"))

    def test_hits_files(self):
        p = Paths(repo=Path("/r"), data=Path("/d"), veta_bin=Path("/v"))
        self.assertEqual(clickbench.hits_files(p), [Path(f"/d/clickbench/hits_{n}.parquet") for n in range(10)])

    def test_first_row(self):
        def run(engine, code, out):
            return EngineRun(engine, [], code, out, b"", None, out)

        self.assertEqual(clickbench.first_row(run("clickhouse", 0, b"10000000\t1\n")), ["10000000", "1"])
        self.assertEqual(clickbench.first_row(run("duckdb", 0, b"10000000\n")), ["10000000"])
        self.assertIsNone(clickbench.first_row(run("veta", 1, b"10000000\n")))
        self.assertIsNone(clickbench.first_row(run("veta", 0, b"")))



class VerifyTest(unittest.TestCase):
    def test_tamper_query(self):
        self.assertEqual(clickbench.tamper_query("5"), "q05")
        self.assertEqual(clickbench.tamper_query("Q43"), "q43")
        self.assertEqual(clickbench.tamper_query("q07"), "q07")
        for bad in ("0", "44", "x", "q"):
            with self.assertRaises(argparse.ArgumentTypeError):
                clickbench.tamper_query(bad)

    def test_rewrites_are_only_q4_and_q29(self):
        base = [f"SELECT {n} FROM hits;" for n in range(1, 44)]
        duck = list(base)
        duck[28] = "SELECT REGEXP_REPLACE(Referer, '^https?://x', '\\1') AS k FROM hits;"
        ch = clickbench.oracle_queries("clickhouse", base, duck)
        self.assertEqual(ch[3], "SELECT AVG(toInt128(UserID)) FROM hits;")
        self.assertEqual([i for i, (a, b) in enumerate(zip(base, ch)) if a != b], [3])
        dd = clickbench.oracle_queries("duckdb", base, duck)
        self.assertEqual(dd[28], "SELECT REGEXP_REPLACE(Referer, '(?s)^https?://x', '\\1') AS k FROM hits;")
        self.assertEqual([i for i, (a, b) in enumerate(zip(duck, dd)) if a != b], [28])
        with self.assertRaises(Infra):
            clickbench.duckdb_q29("SELECT 1")

    def test_read_queries(self):
        with scratch_dir() as d:
            path = Path(d) / "queries.sql"
            path.write_text("\n".join(f"SELECT {n};" for n in range(43)) + "\n\n")
            self.assertEqual(len(clickbench.read_queries(path)), 43)
            path.write_text("SELECT 1;\n")
            with self.assertRaises(Infra):
                clickbench.read_queries(path)

    def test_report_name(self):
        self.assertEqual(clickbench.verify_report_name(argparse.Namespace(against="duckdb")),
                         "clickbench-verify-duckdb")


class PruningTest(unittest.TestCase):
    def test_granules_read(self):
        stats = b'VETA_STATS {"granules_read":12,"granules_total":99}\n'
        self.assertEqual(clickbench.granules_read(EngineRun("veta", [], 0, b"", stats, 0.0, b"")), 12)
        self.assertIsNone(clickbench.granules_read(EngineRun("veta", [], 0, b"", b"", 0.0, b"")))
        self.assertIsNone(clickbench.granules_read(None))
        self.assertEqual(list(clickbench.PRUNING_QUERIES), list(range(37, 44)))


class CodecRoundtripTest(unittest.TestCase):
    def test_counts(self):
        out = b"OK plain a\nFAIL vlz b\ncodec-roundtrip: 12 cases, 1 failures\n"
        self.assertEqual(clickbench.roundtrip_counts(out), (12, 1))
        self.assertIsNone(clickbench.roundtrip_counts(b"OK plain a\n"))


if __name__ == "__main__":
    unittest.main()
