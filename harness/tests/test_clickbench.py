import unittest
from pathlib import Path

from harness import clickbench
from harness.config import Paths
from harness.engines import EngineRun


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


if __name__ == "__main__":
    unittest.main()
