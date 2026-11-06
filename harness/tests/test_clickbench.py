import argparse
import unittest
from pathlib import Path

from harness import clickbench
from harness.config import Paths
from harness.engines import EngineRun
from harness.report import FAIL, PASS, Infra
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


class DiskBytesTest(unittest.TestCase):
    def test_parse_du(self):
        self.assertEqual(clickbench.parse_du(b"1474779179\t/data/db/veta-clickbench/hits\n"), 1474779179)
        self.assertIsNone(clickbench.parse_du(b""))
        self.assertIsNone(clickbench.parse_du(b"du: cannot access\n"))

    def test_clickhouse_bytes_sql(self):
        self.assertEqual(clickbench.clickhouse_bytes_sql("default"),
                         "SELECT sum(bytes_on_disk) FROM system.parts WHERE database = 'default' "
                         "AND table = 'hits' AND active")

    def test_factor_and_verdict(self):
        self.assertEqual(clickbench.disk_factor(110, 100), 1.1)
        self.assertIsNone(clickbench.disk_factor(110, 0))
        self.assertIsNone(clickbench.disk_factor(None, 100))
        self.assertEqual(clickbench.disk_bytes_verdict(1, 1.10), PASS)
        self.assertEqual(clickbench.disk_bytes_verdict(1, 1.1001), FAIL)
        self.assertEqual(clickbench.disk_bytes_verdict(2, 0.9), FAIL)
        self.assertEqual(clickbench.disk_bytes_verdict(1, None), FAIL)


class CompressorTest(unittest.TestCase):
    def test_parse_lz4_short_line(self):
        text = "-1       6543210 (1.529)  712.3 MB/s  3521.8 MB/s  /data/scratch/compressor/columns/URL.raw\n"
        self.assertEqual(clickbench.parse_lz4_bench(text),
                         {"compressed_bytes": 6543210, "compress_mb_s": 712.3, "decompress_mb_s": 3521.8})

    def test_parse_lz4_keeps_the_last_result(self):
        text = (" 1#URL.raw   :  10000000 ->   7000000 (1.429),  500.0 MB/s, 2000.0 MB/s\r"
                " 1#URL.raw   :  10000000 ->   6543210 (1.528),  712.3 MB/s, 3521.8 MB/s\n")
        self.assertEqual(clickbench.parse_lz4_bench(text),
                         {"compressed_bytes": 6543210, "compress_mb_s": 712.3, "decompress_mb_s": 3521.8})
        self.assertIsNone(clickbench.parse_lz4_bench("lz4: no such file\n"))

    def test_parse_codec_bench(self):
        text = ('{"file":"/c/a.raw","raw_bytes":100,"compressed_bytes":40,"compress_mb_s":300.5,'
                '"decompress_mb_s":1500.25}\n'
                '{"file":"/c/b.raw","raw_bytes":50,"compressed_bytes":50,"compress_mb_s":1,"decompress_mb_s":2}\n')
        records = clickbench.parse_codec_bench(text)
        self.assertEqual(sorted(records), ["/c/a.raw", "/c/b.raw"])
        self.assertEqual(records["/c/a.raw"]["decompress_mb_s"], 1500.25)
        with self.assertRaises(ValueError):
            clickbench.parse_codec_bench('{"file":"/c/a.raw","raw_bytes":1}\n')
        with self.assertRaises(ValueError):
            clickbench.parse_codec_bench("not json\n")

    def test_aggregates(self):
        self.assertEqual(clickbench.aggregate_ratio([100, 300], [50, 150]), 2.0)
        self.assertIsNone(clickbench.aggregate_ratio([], []))
        # 100 B at 100 MB/s and 300 B at 300 MB/s take 1 + 1 units: 400 / 2 = 200.
        self.assertEqual(clickbench.aggregate_speed([100, 300], [100.0, 300.0]), 200.0)
        self.assertAlmostEqual(clickbench.aggregate_speed([100, 100], [100.0, 300.0]), 150.0)
        self.assertIsNone(clickbench.aggregate_speed([100], [0.0]))
        self.assertIsNone(clickbench.aggregate_speed([100], []))

    def test_stats_and_verdict(self):
        items = [
            {"raw_bytes": 100, "vlz_bytes": 40, "lz4_bytes": 50, "vlz_decompress_mb_s": 1000.0,
             "lz4_decompress_mb_s": 3000.0},
            {"raw_bytes": 300, "vlz_bytes": 160, "lz4_bytes": 150, "vlz_decompress_mb_s": 1000.0,
             "lz4_decompress_mb_s": 3000.0},
        ]
        stats = clickbench.compressor_stats(items)
        self.assertEqual(stats, {"ratio_vlz": 2.0, "ratio_lz4": 2.0, "dspeed_vlz": 1000.0, "dspeed_lz4": 3000.0})
        self.assertEqual(clickbench.compressor_verdict(stats), PASS)
        self.assertEqual(clickbench.compressor_verdict({**stats, "ratio_vlz": 1.99}), FAIL)
        self.assertEqual(clickbench.compressor_verdict({**stats, "dspeed_vlz": 989.0}), FAIL)
        self.assertEqual(clickbench.compressor_verdict({**stats, "ratio_lz4": None}), FAIL)


if __name__ == "__main__":
    unittest.main()
