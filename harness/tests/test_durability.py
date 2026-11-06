import argparse
import random
import unittest
from pathlib import Path

from harness import compare, durability
from harness.__main__ import build_parser, report_name
from harness.durability import Killed
from harness.engines import EngineRun
from harness.report import FAIL, PASS
from harness.tests.support import scratch_dir


def killed(alive=True, code=-9):
    return Killed(0.5, alive, code, b"", b"")


class ObservationTest(unittest.TestCase):
    def test_parse_parts(self):
        out = b"all_1_10_1\t500000\t1\t123\nall_11_11_0\t50000\t0\t9\n"
        self.assertEqual(durability.parse_parts(out), ["all_1_10_1", "all_11_11_0"])
        self.assertEqual(durability.parse_parts(b""), [])

    def test_part_blocks(self):
        self.assertEqual(durability.part_blocks("all_3_7_2"), (3, 7, 2))
        names = ["all_10_10_0", "all_2_2_0", "all_1_1_0"]
        self.assertEqual(sorted(names, key=durability.part_blocks), ["all_1_1_0", "all_2_2_0", "all_10_10_0"])
        with self.assertRaises(ValueError):
            durability.part_blocks("tmp_insert_3")

    def test_tmp_and_detached_entries(self):
        with scratch_dir() as d:
            table = Path(d) / "t"
            self.assertEqual(durability.tmp_entries(table), [])
            for name in ("all_1_1_0", "tmp_insert_2", "tmp_merge_all_1_2_1", "delete_tmp_all_1_1_0"):
                (table / name).mkdir(parents=True)
            self.assertEqual(durability.tmp_entries(table), ["tmp_insert_2", "tmp_merge_all_1_2_1"])
            self.assertFalse(durability.is_detached(table, "all_1_1_0"))
            (table / "detached" / "broken_all_1_1_0").mkdir(parents=True)
            self.assertTrue(durability.is_detached(table, "all_1_1_0"))
            self.assertFalse(durability.is_detached(table, "all_2_2_0"))

    def test_batch_counts(self):
        self.assertEqual(durability.batch_counts(b"1\t200000\n3\t17\n"), {1: 200000, 3: 17})
        self.assertEqual(durability.batch_counts(b""), {})

    def test_kill_delay_is_uniform_in_range_and_seeded(self):
        a = [durability.kill_delay(random.Random(1), 2.0) for _ in range(3)]
        self.assertEqual(a, [durability.kill_delay(random.Random(1), 2.0) for _ in range(3)])
        rng = random.Random(7)
        self.assertTrue(all(0.0 <= durability.kill_delay(rng, 2.0) <= 2.0 for _ in range(1000)))

    def test_alive_fraction(self):
        self.assertEqual(durability.alive_fraction([{"alive": True}, {"alive": False}]), 0.5)
        self.assertEqual(durability.alive_fraction([]), 0.0)

    def test_insert_batch_text(self):
        lines = durability.insert_batch_text(4, rows=3).split("\n")
        self.assertEqual(len(lines), 4)
        self.assertTrue(all(line.startswith("4\t") for line in lines[:3]))
        self.assertEqual(lines[2].split("\t")[1], "2")

    def test_iteration_count(self):
        self.assertEqual(durability.iteration_count("5"), 5)
        for bad in ("0", "-2", "x"):
            with self.assertRaises(argparse.ArgumentTypeError):
                durability.iteration_count(bad)


class VerdictTest(unittest.TestCase):
    def test_merge_equivalence(self):
        items = [{"query": f"q{n:02d}", "verdict": "equal"} for n in range(1, 44)]
        self.assertEqual(durability.merge_equivalence_verdict(items, 100, 1)[0], PASS)
        self.assertEqual(durability.merge_equivalence_verdict(items, 100, 2)[0], FAIL)
        self.assertEqual(durability.merge_equivalence_verdict(items, 99, 1)[0], FAIL)
        different = items[:5] + [{"query": "q06", "verdict": "different"}] + items[6:]
        status, summary = durability.merge_equivalence_verdict(different, 100, 1)
        self.assertEqual((status, summary.split(",")[0]), (FAIL, "42/43 equal"))
        self.assertEqual(durability.merge_equivalence_verdict(items[:42], 100, 1)[0], FAIL)

    def test_concurrent(self):
        self.assertEqual(durability.concurrent_verdict(200, 0, 0, 50, 4)[0], PASS)
        self.assertEqual(durability.concurrent_verdict(200, 1, 0, 50, 4)[0], FAIL)
        self.assertEqual(durability.concurrent_verdict(200, 0, 1, 50, 4)[0], FAIL)
        self.assertEqual(durability.concurrent_verdict(200, 0, 0, 50, 50)[0], FAIL)
        self.assertEqual(durability.concurrent_verdict(200, 0, 0, None, 4)[0], FAIL)
        self.assertEqual(durability.concurrent_verdict(199, 0, 0, 50, 4)[0], FAIL)

    def test_crash(self):
        self.assertEqual(durability.crash_verdict(100, 0, 0.93)[0], PASS)
        self.assertEqual(durability.crash_verdict(100, 1, 0.93)[0], FAIL)
        self.assertEqual(durability.crash_verdict(100, 0, 0.79)[0], FAIL)
        self.assertEqual(durability.crash_verdict(100, 0, 0.8)[0], PASS)
        self.assertEqual(durability.crash_verdict(99, 0, 0.93)[0], FAIL)
        self.assertEqual(durability.crash_verdict(100, 0, 0.9, partial=0)[0], PASS)
        self.assertEqual(durability.crash_verdict(100, 0, 0.9, partial=1)[0], FAIL)

    def test_crash_merge_iteration(self):
        ok = {"q02": "equal", "q08": "equal"}
        self.assertEqual(durability.crash_merge_failures(killed(), 0, [], 0, ok), [])
        self.assertEqual(durability.crash_merge_failures(killed(alive=False, code=0), 0, [], 0, ok), [])
        self.assertTrue(durability.crash_merge_failures(killed(), 0, ["tmp_merge_all_1_20_1"], 0, ok))
        self.assertTrue(durability.crash_merge_failures(killed(), 0, [], 3, ok))
        self.assertTrue(durability.crash_merge_failures(killed(), 2, [], 0, ok))
        self.assertTrue(durability.crash_merge_failures(killed(), 0, [], 0, dict(ok, q08="different")))
        self.assertTrue(durability.crash_merge_failures(killed(), 0, [], 0, dict(ok, q08="error")))
        self.assertTrue(durability.crash_merge_failures(killed(alive=False, code=2), 0, [], 0, ok))

    def test_crash_insert_iteration(self):
        rows = durability.INSERT_BATCH_ROWS
        ok, partial = durability.crash_insert_failures(killed(), 0, [], 0, 0, {1: rows}, {1}, 2)
        self.assertEqual((ok, partial), ([], set()))
        ok, _ = durability.crash_insert_failures(killed(), 0, [], 0, 0, {1: rows, 2: rows}, {1}, 2)
        self.assertEqual(ok, [])
        # A batch half there.
        reasons, partial = durability.crash_insert_failures(killed(), 0, [], 0, 0, {1: rows, 2: 81920}, {1}, 2)
        self.assertTrue(reasons)
        self.assertEqual(partial, {2})
        # A tmp_* left behind.
        reasons, _ = durability.crash_insert_failures(killed(), 0, ["tmp_insert_2"], 0, 0, {1: rows}, {1}, 2)
        self.assertTrue(reasons)
        # A committed batch lost, an unknown one, a failed check or count.
        self.assertTrue(durability.crash_insert_failures(killed(), 0, [], 0, 0, {}, {1}, 2)[0])
        self.assertTrue(durability.crash_insert_failures(killed(), 0, [], 0, 0, {1: rows, 9: rows}, {1}, 2)[0])
        self.assertTrue(durability.crash_insert_failures(killed(), 0, [], 3, 0, {1: rows}, {1}, 2)[0])
        self.assertTrue(durability.crash_insert_failures(killed(), 0, [], 0, 2, {}, {1}, 2)[0])
        # A load that finished must have published its batch.
        done = killed(alive=False, code=0)
        self.assertTrue(durability.crash_insert_failures(done, 0, [], 0, 0, {1: rows}, {1}, 2)[0])
        self.assertEqual(durability.crash_insert_failures(done, 0, [], 0, 0, {1: rows, 2: rows}, {1}, 2)[0], [])

    def test_corruption_trial(self):
        self.assertEqual(durability.corruption_verdict(3, False, 0, True, True)[0], "pass")
        self.assertEqual(durability.corruption_verdict(0, True, 0, True, True)[0], "pass")
        # The first query returned the rows of the corrupt part.
        self.assertEqual(durability.corruption_verdict(0, False, 0, True, True)[0], "fail")
        self.assertEqual(durability.corruption_verdict(2, False, 0, True, True)[0], "fail")
        self.assertEqual(durability.corruption_verdict(3, False, 3, False, True)[0], "fail")
        self.assertEqual(durability.corruption_verdict(3, False, 0, False, True)[0], "fail")
        self.assertEqual(durability.corruption_verdict(3, False, 0, True, False)[0], "fail")

    def test_corruption_command(self):
        items = [{"file_type": t, "detached": True, "verdict": "pass"} for t in durability.FILE_TYPES]
        self.assertEqual(durability.corruption_command_verdict(items)[0], PASS)
        self.assertEqual(durability.corruption_command_verdict(items[:-1])[0], FAIL)
        broken = [dict(items[2], verdict="fail")] + items[:2] + items[3:]
        self.assertEqual(durability.corruption_command_verdict(broken)[0], FAIL)


class CorruptionChoiceTest(unittest.TestCase):
    def test_byte_inside_the_file_and_changed(self):
        rng = random.Random(3)
        for size in (1, 2, 17, 8192):
            for _ in range(200):
                offset, mask = durability.corrupt_byte(size, rng)
                self.assertTrue(0 <= offset < size)
                self.assertTrue(1 <= mask <= 255)
        with self.assertRaises(ValueError):
            durability.corrupt_byte(0, rng)

    def test_flip_byte_changes_exactly_one_byte(self):
        with scratch_dir() as d:
            path = Path(d) / "c1.bin"
            data = bytes(range(256)) * 4
            path.write_bytes(data)
            offset, old, new = durability.flip_byte(path, random.Random(5))
            after = path.read_bytes()
            self.assertEqual(len(after), len(data))
            self.assertEqual([i for i in range(len(data)) if data[i] != after[i]], [offset])
            self.assertEqual((data[offset], after[offset]), (old, new))
            self.assertNotEqual(old, new)

    def test_affected_column(self):
        rng = random.Random(1)
        self.assertEqual(durability.affected_column("primary.idx", rng), durability.CORRUPT_KEY[0])
        self.assertEqual(durability.affected_column("primary.idx", rng, columns=5, key=(3, 1)), 3)
        for t in durability.FILE_TYPES:
            self.assertTrue(all(0 <= durability.affected_column(t, rng) < len(durability.CORRUPT_COLUMNS)
                                for _ in range(50)))
        seen = {durability.affected_column("bin", rng) for _ in range(200)}
        self.assertEqual(seen, set(range(len(durability.CORRUPT_COLUMNS))))
        with self.assertRaises(ValueError):
            durability.affected_column("c0.bin", rng)

    def test_part_file(self):
        self.assertEqual(durability.part_file("bin", 2), "c2.bin")
        self.assertEqual(durability.part_file("mrk", 0), "c0.mrk")
        for t in ("part.txt", "primary.idx", "stats.bin", "sample.bin", "checksums.txt"):
            self.assertEqual(durability.part_file(t, 1), t)
        with self.assertRaises(ValueError):
            durability.part_file("data.bin", 1)

    def test_column_query_reads_the_column(self):
        self.assertIn("sum(v)", durability.column_query(1))
        self.assertIn("sum(length(s))", durability.column_query(2))
        self.assertIn("FROM corrupt", durability.column_query(0))

    def test_batches(self):
        batches = durability.corruption_batches()
        self.assertEqual(len(batches), durability.CORRUPT_BATCHES)
        self.assertTrue(all(len(b) == durability.CORRUPT_BATCH_ROWS for b in batches))
        keys = [r[0] for b in batches for r in b]
        self.assertEqual(keys, sorted(keys))
        self.assertEqual(batches, durability.corruption_batches())
        self.assertIn("PRIMARY KEY (k)", durability.CORRUPT_CREATE)


class SideTest(unittest.TestCase):
    def test_memo_side_runs_each_text_once(self):
        calls = []

        def run(sql):
            calls.append(sql)
            return EngineRun("veta", ["veta", sql], 0, b"1\n", b"", 0.0, b"1\n")

        side = durability.memo_side(compare.Side("veta", run, "veta", ".reference"))
        side.run("SELECT 1")
        side.run("SELECT 1")
        side.run("SELECT 2")
        self.assertEqual(calls, ["SELECT 1", "SELECT 2"])
        self.assertEqual(side.suffix, ".reference")

    def test_first_run_side(self):
        first = EngineRun("veta", ["veta", "sql", "SELECT 1"], 0, b"1\n", b"", 0.0, b"1\n")
        other = []
        side = durability.first_run_side(first, compare.Side("veta", lambda s: other.append(s), "veta"))
        self.assertIs(side.run("SELECT 1"), first)
        side.run("SELECT 2")
        self.assertEqual(other, ["SELECT 2"])


class ParserTest(unittest.TestCase):
    def test_commands(self):
        parser = build_parser()
        for name in ("merge-equivalence", "corruption"):
            self.assertEqual(report_name(parser.parse_args([name])), name)
        args = parser.parse_args(["concurrent-reader"])
        self.assertEqual((report_name(args), args.iterations), ("concurrent-reader", 200))
        for name in ("crash-merge", "crash-insert"):
            args = parser.parse_args([name])
            self.assertEqual((report_name(args), args.iterations), (name, 100))
            self.assertEqual(parser.parse_args([name, "--iterations", "7"]).iterations, 7)


if __name__ == "__main__":
    unittest.main()
