import math
import unittest

from harness import normalize as n


class CellTest(unittest.TestCase):
    def test_scalars(self):
        self.assertIsNone(n.normalize_tsv_cell("\\N"))
        self.assertEqual(n.normalize_tsv_cell("true"), 1)
        self.assertEqual(n.normalize_tsv_cell("false"), 0)
        self.assertEqual(n.normalize_tsv_cell("-42"), -42)
        self.assertIsInstance(n.normalize_tsv_cell("1508"), int)
        self.assertIsInstance(n.normalize_tsv_cell("1508.0"), float)
        self.assertEqual(n.normalize_tsv_cell("1e3"), 1000.0)
        self.assertEqual(n.normalize_tsv_cell("-inf"), -math.inf)
        self.assertTrue(math.isnan(n.normalize_tsv_cell("nan")))

    def test_timestamps_lose_a_null_fraction(self):
        self.assertEqual(n.normalize_tsv_cell("2013-07-15 10:00:00"), "2013-07-15 10:00:00")
        self.assertEqual(n.normalize_csv_cell("2013-07-15 10:00:00.000"), "2013-07-15 10:00:00")
        self.assertEqual(n.normalize_csv_cell("2013-07-15 10:00:00.250"), "2013-07-15 10:00:00.250")

    def test_tsv_escapes(self):
        self.assertEqual(n.unescape_tsv("a\\tb\\nc\\\\d\\fe\\'f\\0g\\bh\\ri"), "a\tb\nc\\d\fe'f\0g\bh\ri")
        self.assertEqual(n.unescape_tsv("keep \\x as is"), "keep \\x as is")
        # An escaped backslash before N is a string, not NULL.
        self.assertEqual(n.normalize_tsv_cell("\\\\N"), "\\N")

    def test_csv_is_not_unescaped(self):
        self.assertEqual(n.normalize_csv_cell("a\\tb"), "a\\tb")

    def test_parse_outputs(self):
        self.assertEqual(n.parse_tsv("1\tx\\ty\n\\N\t2.5\n"), [(1, "x\ty"), (None, 2.5)])
        self.assertEqual(n.parse_csv('1,"a,b"\n2,"line\nbreak"\n'), [(1, "a,b"), (2, "line\nbreak")])
        self.assertEqual(n.parse_output("duckdb", "1,true\n"), [(1, 1)])
        self.assertEqual(n.parse_output("clickhouse", "1\ttrue\n"), [(1, 1)])
        self.assertEqual(n.parse_tsv(""), [])

    def test_non_utf8_bytes_survive(self):
        text = b"caf\xe9\n".decode("utf-8", errors="surrogateescape")
        (row,) = n.parse_tsv(text)
        self.assertEqual(row[0].encode("utf-8", errors="surrogateescape"), b"caf\xe9")


class EqualityTest(unittest.TestCase):
    def test_integers_are_exact(self):
        self.assertTrue(n.cells_equal(10**30, 10**30))
        self.assertFalse(n.cells_equal(10**30, 10**30 + 1))

    def test_tolerance(self):
        self.assertTrue(n.cells_equal(1508, 1508.0))
        self.assertTrue(n.cells_equal(11798257.208, 11798257.2080))
        self.assertTrue(n.cells_equal(1.0, 1.0 + 5e-10))
        self.assertFalse(n.cells_equal(1.0, 1.0 + 1e-8))
        self.assertTrue(n.cells_equal(0.0, 1e-13))
        self.assertFalse(n.cells_equal(0.0, 1e-11))

    def test_special_values(self):
        self.assertTrue(n.cells_equal(math.inf, math.inf))
        self.assertFalse(n.cells_equal(math.inf, -math.inf))
        self.assertTrue(n.cells_equal(math.nan, math.nan))
        self.assertFalse(n.cells_equal(math.nan, 1.0))
        self.assertTrue(n.cells_equal(None, None))
        self.assertFalse(n.cells_equal(None, 0))
        self.assertFalse(n.cells_equal("1", 1))

    def test_rows(self):
        self.assertTrue(n.rows_equal((1, "a", None), (1.0, "a", None)))
        self.assertFalse(n.rows_equal((1, "a"), (1, "a", None)))

    def test_sort_key_mixes_types(self):
        rows = [("b",), (None,), (2.5,), (1,), (math.nan,)]
        self.assertEqual(sorted(rows, key=n.row_sort_key)[:3], [(None,), (1,), (2.5,)])


if __name__ == "__main__":
    unittest.main()
