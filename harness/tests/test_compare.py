import unittest

from harness import compare
from harness.engines import EngineRun
from harness.normalize import parse_csv, parse_tsv


def run(engine: str, out: bytes, code: int = 0) -> EngineRun:
    return EngineRun(engine, [], code, out, b"" if code == 0 else b"boom\n", 0.0, out)


class FakeEngine:
    """Answers each SQL text from a table; records what it was asked."""

    def __init__(self, engine: str, answers: dict[str, bytes]):
        self.engine = engine
        self.answers = answers
        self.asked: list[str] = []

    def __call__(self, sql: str) -> EngineRun:
        self.asked.append(sql)
        for prefix, out in self.answers.items():
            if sql.startswith(prefix):
                return run(self.engine, out)
        return run(self.engine, b"", code=1)


def sides(veta: dict, ref: dict, ref_engine: str = "clickhouse"):
    v, r = FakeEngine("veta", veta), FakeEngine(ref_engine, ref)
    return compare.Side("veta", v, "veta"), compare.Side(ref_engine, r, ref_engine), v, r


class MultisetTest(unittest.TestCase):
    def test_false_positive(self):
        self.assertIsNotNone(compare.multiset_diff([(1, "a"), (2, "b")], [(1, "a"), (2, "c")]))
        self.assertIsNotNone(compare.multiset_diff([(1,), (1,)], [(1,)]))
        self.assertIsNotNone(compare.multiset_diff([(1,), (1,), (2,)], [(1,), (2,), (2,)]))
        self.assertIsNotNone(compare.multiset_diff([(1.0,)], [(1.0 + 1e-6,)]))

    def test_benign(self):
        self.assertIsNone(compare.multiset_diff([(2, "b"), (1, "a")], [(1, "a"), (2, "b")]))
        self.assertIsNone(compare.multiset_diff(parse_tsv("1508\n"), parse_csv("1508.0\n")))
        self.assertIsNone(compare.multiset_diff(parse_tsv("11798257.208\n"), parse_tsv("11798257.2080\n")))
        self.assertIsNone(compare.multiset_diff(parse_tsv("a\\fb\n"), parse_csv("a\fb\n")))
        self.assertIsNone(compare.multiset_diff(parse_tsv("true\t\\N\n"), parse_csv("1,\\N\n")))
        self.assertIsNone(compare.multiset_diff([], []))

    def test_tolerance_across_sort_positions(self):
        # Sorting pairs the rows by their numbers; they only match within the tolerance across pairs.
        a = [(1.0 + 1e-12, "x"), (1.0, "y")]
        b = [(1.0, "x"), (1.0 + 1e-12, "y")]
        self.assertIsNone(compare.multiset_diff(a, b))


class OrderedTest(unittest.TestCase):
    def test_false_positive(self):
        ref = [("a", 3), ("b", 2), ("c", 1)]
        # Same multiset, wrong order.
        self.assertIsNotNone(compare.ordered_diff([("b", 2), ("a", 3), ("c", 1)], ref, (1,)))
        # A different value inside a group.
        self.assertIsNotNone(compare.ordered_diff([("a", 3), ("x", 2), ("c", 1)], ref, (1,)))
        self.assertIsNotNone(compare.ordered_diff(ref[:2], ref, (1,)))

    def test_benign(self):
        ref = [("a", 3), ("b", 2), ("c", 2), ("d", 1)]
        # Ties may come in any order inside their group.
        self.assertIsNone(compare.ordered_diff([("a", 3), ("c", 2), ("b", 2), ("d", 1)], ref, (1,)))
        self.assertIsNone(compare.ordered_diff(parse_tsv("x\t11798257.208\n"), parse_tsv("x\t11798257.2080\n"),
                                               (1,)))
        self.assertIsNone(compare.ordered_diff(parse_tsv("1508\t1\n"), parse_csv("1508.0,1\n"), (1,)))

    def test_inner_group_must_still_match_without_border(self):
        ref = [("a", 3), ("b", 2), ("c", 1)]
        self.assertIsNotNone(compare.ordered_diff([("z", 3), ("b", 2), ("c", 1)], ref, (1,)))


class LimitBorderTest(unittest.TestCase):
    def test_border_groups(self):
        from harness import sqltext

        q = sqltext.parse("SELECT a, COUNT(*) AS c FROM t GROUP BY a ORDER BY c DESC LIMIT 10 OFFSET 1000")
        self.assertEqual(compare.border_groups(q, compare.LIMIT_BORDER, 10), (True, True))
        self.assertEqual(compare.border_groups(q, compare.LIMIT_BORDER, 4), (True, False))
        q = sqltext.parse("SELECT a, COUNT(*) AS c FROM t GROUP BY a ORDER BY c DESC LIMIT 10")
        self.assertEqual(compare.border_groups(q, compare.LIMIT_BORDER, 10), (False, True))
        self.assertEqual(compare.border_groups(q, compare.ORDERED, 10), (False, False))

    def test_tie_at_the_border(self):
        # The window cut a group of ties: either row is a valid last row (and first row).
        ref = [("p", 9), ("q", 5), ("x", 1)]
        self.assertIsNone(compare.ordered_diff([("r", 9), ("q", 5), ("y", 1)], ref, (1,), True, True))
        self.assertIsNotNone(compare.ordered_diff([("r", 9), ("q", 5), ("y", 1)], ref, (1,)))

    def test_false_positive(self):
        ref = [("p", 9), ("q", 5), ("x", 1)]
        # The inner group must match exactly.
        self.assertIsNotNone(compare.ordered_diff([("p", 9), ("z", 5), ("x", 1)], ref, (1,), True, True))
        # Border keys must match.
        self.assertIsNotNone(compare.ordered_diff([("p", 9), ("q", 5), ("x", 2)], ref, (1,), True, True))


class UnorderedLimitTest(unittest.TestCase):
    def test_benign(self):
        # Different rows than the reference's own LIMIT, all present without LIMIT.
        matches = [(1, "a", 3), (2, "b", 1)]
        self.assertIsNone(compare.unordered_limit_diff([(2, "b", 1.0), (1, "a", 3)], 2, matches))

    def test_false_positive(self):
        matches = [(1, "a", 3), (2, "b", 1)]
        self.assertIsNotNone(compare.unordered_limit_diff([(1, "a", 3), (2, "b", 2)], 2, matches))
        self.assertIsNotNone(compare.unordered_limit_diff([(1, "a", 3)], 2, matches))

    def test_match_sql(self):
        from harness import sqltext

        q = sqltext.parse("SELECT u, p, COUNT(*) FROM t GROUP BY u, p LIMIT 10;")
        sql = compare.match_sql(q, [(1, "it's", 3), (1, "it's", 3), (None, "a\\b", 2)], "clickhouse")
        self.assertEqual(
            sql,
            "SELECT c1, c2, c3 FROM (SELECT u AS c1, p AS c2, COUNT(*) AS c3 FROM t GROUP BY u, p) AS t "
            "WHERE (c1 = 1 AND c2 = 'it\\'s' AND c3 = 3) OR (c1 IS NULL AND c2 = 'a\\\\b' AND c3 = 2)",
        )
        self.assertIn("c2 = 'it''s'", compare.match_sql(q, [(1, "it's", 3)], "duckdb"))
        self.assertEqual(compare.sql_literal("a\0b", "duckdb"), "'a' || chr(0) || 'b'")
        self.assertIn("abs(c3 - (2.5))", compare.match_sql(q, [(1, "x", 2.5)], "duckdb"))


class TamperTest(unittest.TestCase):
    def test_values(self):
        self.assertEqual(compare.tamper([(5, "a"), (6, "b")]), [(6, "a"), (6, "b")])
        self.assertEqual(compare.tamper([(2.0,)]), [(4.0,)])
        self.assertEqual(compare.tamper([("s",)]), [("sx",)])
        self.assertEqual(compare.tamper([(None,)]), [(0,)])
        with self.assertRaises(compare.TamperError):
            compare.tamper([])


class VerifyQueryTest(unittest.TestCase):
    def test_multiset_and_tamper(self):
        sql = "SELECT a, b FROM t;"
        veta, ref, _, _ = sides({"SELECT a": b"1\tx\n2\ty\n"}, {"SELECT a": b"2\ty\n1\tx\n"})
        raw = []
        c = compare.verify_query("q01", veta, ref, sql, sql, raw=lambda *a: raw.append(a[:2]))
        self.assertEqual((c.rule, c.verdict, c.rows_engine, c.rows_ref), ("multiset", "equal", 2, 2))
        self.assertEqual(raw, [("veta", "q01"), ("clickhouse", "q01")])
        c = compare.verify_query("q01", veta, ref, sql, sql, tampered=True)
        self.assertEqual(c.verdict, "different")
        self.assertIn("tampered", c.reason)

    def test_engine_error(self):
        veta, ref, _, _ = sides({}, {"SELECT": b"1\n"})
        c = compare.verify_query("q02", veta, ref, "SELECT 1", "SELECT 1")
        self.assertEqual(c.verdict, "error")
        self.assertIn("exited 1", c.reason)

    def test_augmented(self):
        sql = "SELECT p FROM t ORDER BY ts LIMIT 2;"
        veta, ref, v, r = sides(
            {"SELECT p FROM": b"a\nb\n", "SELECT p, ts": b"a\t1\nb\t2\n"},
            {"SELECT p FROM": b"a\nb\n", "SELECT p, ts": b"a\t1\nb\t2\n"},
        )
        c = compare.verify_query("q25", veta, ref, sql, sql)
        self.assertEqual((c.rule, c.verdict), ("limit-border", "equal"))
        self.assertEqual(v.asked[-1], "SELECT p, ts FROM t ORDER BY ts LIMIT 2")
        c = compare.verify_query("q25", veta, ref, sql, sql, tampered=True)
        self.assertEqual(c.verdict, "different")

    def test_augmented_checks_original_row_count(self):
        sql = "SELECT p FROM t ORDER BY ts LIMIT 2;"
        veta, ref, _, _ = sides(
            {"SELECT p FROM": b"a\n", "SELECT p, ts": b"a\t1\nb\t2\n"},
            {"SELECT p FROM": b"a\nb\n", "SELECT p, ts": b"a\t1\nb\t2\n"},
        )
        self.assertEqual(compare.verify_query("q25", veta, ref, sql, sql).verdict, "different")

    def test_unordered_limit(self):
        sql = "SELECT u, p, COUNT(*) FROM t GROUP BY u, p LIMIT 2;"
        veta, ref, _, r = sides(
            {"SELECT u": b"7\tz\t1\n1\ta\t3\n"},
            {"SELECT u": b"1\ta\t3\n2\tb\t1\n", "SELECT c1": b"1,a,3\n7,z,1\n"},
            ref_engine="duckdb",
        )
        c = compare.verify_query("q18", veta, ref, sql, sql)
        self.assertEqual((c.rule, c.verdict), ("limit-unordered", "equal"), c.reason)
        self.assertTrue(r.asked[-1].startswith("SELECT c1, c2, c3 FROM (SELECT u AS c1"))
        c = compare.verify_query("q18", veta, ref, sql, sql, tampered=True)
        self.assertEqual(c.verdict, "different")

    def test_unordered_limit_missing_row(self):
        sql = "SELECT u, p, COUNT(*) FROM t GROUP BY u, p LIMIT 2;"
        veta, ref, _, _ = sides(
            {"SELECT u": b"7\tz\t1\n1\ta\t3\n"},
            {"SELECT u": b"1\ta\t3\n2\tb\t1\n", "SELECT c1": b"1\ta\t3\n"},
        )
        self.assertEqual(compare.verify_query("q18", veta, ref, sql, sql).verdict, "different")


if __name__ == "__main__":
    unittest.main()
