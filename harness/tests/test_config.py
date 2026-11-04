import re
import unittest
from pathlib import Path

from harness import config

HEX64 = re.compile(r"^[0-9a-f]{64}$")
HEX128 = re.compile(r"^[0-9a-f]{128}$")


class PinsTest(unittest.TestCase):
    def test_every_sha256_is_64_hex(self):
        for pin in config.ALL_PINS:
            self.assertRegex(pin.sha256, HEX64, pin.dest)

    def test_archive_hashes(self):
        algo, digest = config.DUCKDB_PIN.archive_hash
        self.assertEqual(algo, "sha256")
        self.assertRegex(digest, HEX64)
        algo, digest = config.CLICKHOUSE_PIN.archive_hash
        self.assertEqual(algo, "sha512")
        self.assertRegex(digest, HEX128)

    def test_ten_hits_files(self):
        self.assertEqual(len(config.HITS_PINS), 10)
        self.assertEqual([p.dest for p in config.HITS_PINS],
                         [f"clickbench/hits_{n}.parquet" for n in range(10)])
        self.assertEqual(len(set(p.sha256 for p in config.HITS_PINS)), 10)
        for n, pin in enumerate(config.HITS_PINS):
            self.assertTrue(pin.url.endswith(f"/hits_{n}.parquet"))

    def test_twenty_two_tpch_queries(self):
        self.assertEqual(len(config.TPCH_QUERY_PINS), 22)
        self.assertEqual(sorted(config.TPCH_QUERY_SHA256), [f"q{n:02d}" for n in range(1, 23)])
        for pin in config.TPCH_QUERY_PINS:
            name = Path(pin.dest).stem
            self.assertTrue(pin.url.endswith(f"/queries/{name}.sql"))
            self.assertIn(config.DUCKDB_COMMIT, pin.url)

    def test_each_pin_has_exactly_one_source(self):
        dests = [p.dest for p in config.ALL_PINS]
        self.assertEqual(len(dests), len(set(dests)))
        for pin in config.ALL_PINS:
            self.assertEqual((pin.url is None), (pin.install is not None), pin.dest)
            self.assertEqual(pin.member is None, pin.archive_hash is None, pin.dest)
            self.assertFalse(Path(pin.dest).is_absolute())
            if pin.url:
                self.assertTrue(pin.url.startswith("https://"), pin.url)

    def test_duckdb_comes_before_the_extension(self):
        order = list(config.ALL_PINS)
        self.assertLess(order.index(config.DUCKDB_PIN), order.index(config.TPCH_EXTENSION_PIN))

    def test_paths_from_environment(self):
        p = config.paths({"VETA_DATA": "/data/v", "VETA_BIN": "/opt/veta"})
        self.assertEqual(p.data, Path("/data/v"))
        self.assertEqual(p.veta_bin, Path("/opt/veta"))
        self.assertEqual(p.duckdb, Path("/data/v/bin/duckdb"))
        self.assertEqual(p.clickhouse, Path("/data/v/bin/clickhouse"))
        self.assertEqual(p.scratch, Path("/data/v/scratch"))
        self.assertEqual(p.runs, Path("/data/v/runs"))

    def test_default_paths_live_in_the_repo(self):
        p = config.paths({})
        self.assertEqual(p.data, config.REPO / ".veta-data")
        self.assertEqual(p.veta_bin, config.REPO / "target" / "release" / "veta")


if __name__ == "__main__":
    unittest.main()
