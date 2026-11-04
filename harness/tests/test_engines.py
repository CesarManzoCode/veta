import unittest
from pathlib import Path

from harness import engines
from harness.config import Paths
from harness.report import Infra, Skip

P = Paths(repo=Path("/repo"), data=Path("/data"), veta_bin=Path("/repo/target/release/veta"))


class CommandTest(unittest.TestCase):
    def test_veta_sql(self):
        self.assertEqual(
            engines.veta_sql_command(P.veta_bin, Path("/data/db/veta-clickbench"), "SELECT 1"),
            ["/repo/target/release/veta", "sql", "--db", "/data/db/veta-clickbench", "--format", "tsv", "SELECT 1"],
        )
        argv = engines.veta_sql_command(P.veta_bin, Path("/d"), "SELECT 1", stats=True, extra=["--no-prune"])
        self.assertEqual(argv[-3:], ["--stats", "--no-prune", "SELECT 1"])

    def test_veta_load(self):
        argv = engines.veta_load_command(P.veta_bin, Path("/d"), "hits", parquet=[Path("/a.parquet"), Path("/b.parquet")])
        self.assertEqual(argv, ["/repo/target/release/veta", "load", "--db", "/d", "--table", "hits",
                                "--parquet", "/a.parquet", "/b.parquet"])
        argv = engines.veta_load_command(P.veta_bin, Path("/d"), "t", tsv=["/x.tsv"], batch_rows=1000)
        self.assertEqual(argv[6:], ["--tsv", "/x.tsv", "--batch-rows", "1000"])
        with self.assertRaises(ValueError):
            engines.veta_load_command(P.veta_bin, Path("/d"), "t")

    def test_duckdb(self):
        argv = engines.duckdb_command(P.duckdb, Path("/data/db/duckdb-clickbench.duckdb"), P.duckdb_ext, "SELECT 1")
        self.assertEqual(argv, [
            "/data/bin/duckdb", "-no-agent", "-csv", "-noheader", "-nullvalue", "\\N",
            "/data/db/duckdb-clickbench.duckdb",
            "-c", "SET extension_directories=['/data/duckdb-ext']",
            "-c", ".timer on",
            "-c", "SELECT 1",
        ])
        in_memory = engines.duckdb_command(P.duckdb, None, Path("/it's"), "INSTALL tpch;")
        self.assertEqual(in_memory[6:8], ["-c", "SET extension_directories=['/it''s']"])

    def test_duckdb_env_turns_agent_mode_off(self):
        env = engines.duckdb_env({"AI_AGENT": "1", "PATH": "/bin"})
        self.assertEqual(env, {"AI_AGENT": "1", "PATH": "/bin", "DUCKDB_AGENT_MODE": "0"})

    def test_clickhouse(self):
        argv = engines.clickhouse_command(P.clickhouse, Path("/data/db/clickhouse"), "SELECT 1")
        self.assertEqual(argv, [
            "/data/bin/clickhouse", "local", "--path", "/data/db/clickhouse",
            "--session_timezone", "UTC", "--join_use_nulls", "1",
            "--format", "TSV", "--time", "-q", "SELECT 1",
        ])
        argv = engines.clickhouse_command(P.clickhouse, Path("/p"), "SELECT 1", database="tpch_sf10")
        self.assertEqual(argv[8:10], ["--database", "tpch_sf10"])

    def test_databases(self):
        cb = engines.clickbench_databases(P)
        self.assertEqual(cb.veta, Path("/data/db/veta-clickbench"))
        self.assertEqual(cb.duckdb, Path("/data/db/duckdb-clickbench.duckdb"))
        self.assertEqual((cb.clickhouse_path, cb.clickhouse_database), (Path("/data/db/clickhouse"), "default"))
        t = engines.tpch_databases(P, 10)
        self.assertEqual(t.veta, Path("/data/db/veta-tpch-sf10"))
        self.assertEqual(t.duckdb, Path("/data/tpch/sf10.duckdb"))
        self.assertEqual(t.clickhouse_database, "tpch_sf10")
        with self.assertRaises(ValueError):
            engines.tpch_databases(P, 3)


class ParseTest(unittest.TestCase):
    def test_duckdb_timer(self):
        out = b"42,\\N\nRun Time (s): real 0.123 user 0.400000 sys 0.010000\n"
        body, seconds = engines.strip_duckdb_timer(out)
        self.assertEqual(body, b"42,\\N\n")
        self.assertAlmostEqual(seconds, 0.123)

    def test_duckdb_timer_sums_statements_and_may_be_absent(self):
        out = b"Run Time (s): real 1.500 user 1 sys 0\nRun Time (s): real 0.25 user 0 sys 0\n"
        body, seconds = engines.strip_duckdb_timer(out)
        self.assertEqual(body, b"\n")
        self.assertAlmostEqual(seconds, 1.75)
        self.assertEqual(engines.strip_duckdb_timer(b"1\n"), (b"1\n", None))

    def test_clickhouse_time_is_last_stderr_line(self):
        self.assertAlmostEqual(engines.clickhouse_time(b"Warning: something\n0.047\n"), 0.047)
        self.assertAlmostEqual(engines.clickhouse_time(b"1.25\n\n  \n"), 1.25)
        self.assertIsNone(engines.clickhouse_time(b""))
        self.assertIsNone(engines.clickhouse_time(b"Code: 60. DB::Exception: no table\n"))

    def test_rows_split_only_on_newline(self):
        text = engines.decode(b"a\rb\x1cc\tx\nsecond\x85\n")
        self.assertEqual(engines.split_rows(text), ["a\rb\x1cc\tx", "second\x85"])
        self.assertEqual(engines.split_rows(""), [])
        self.assertEqual(engines.split_rows("\n"), [""])

    def test_decode_keeps_invalid_bytes(self):
        raw = b"caf\xc3\xa9 \xff\xfe"
        text = engines.decode(raw)
        self.assertTrue(text.startswith("café "))
        self.assertEqual(text.encode("utf-8", errors="surrogateescape"), raw)


class RequireTest(unittest.TestCase):
    def test_missing_binaries(self):
        e = engines.Engines(Paths(repo=Path("/nonexistent"), data=Path("/nonexistent/data"),
                                  veta_bin=Path("/nonexistent/veta")))
        self.assertFalse(e.available("duckdb"))
        with self.assertRaises(Infra):
            e.require("veta")
        with self.assertRaises(Skip):
            e.require("duckdb")
        with self.assertRaises(Skip):
            e.require("clickhouse")
        with self.assertRaises(ValueError):
            e.binary("sqlite")


if __name__ == "__main__":
    unittest.main()
