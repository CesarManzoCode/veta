import unittest

from harness import compare, sqltext
from harness.clickbench import QUERY_COUNT, read_queries
from harness.config import paths


class ClauseTest(unittest.TestCase):
    def test_plain_aggregate(self):
        q = sqltext.parse("SELECT COUNT(*) FROM t;")
        self.assertEqual([i.expr for i in q.items], ["COUNT(*)"])
        self.assertFalse(q.has_order_by)
        self.assertFalse(q.has_limit)

    def test_order_by_count_desc(self):
        q = sqltext.parse("SELECT a, COUNT(*) FROM t WHERE a <> 0 GROUP BY a ORDER BY COUNT(*) DESC;")
        self.assertEqual([(k.expr, k.descending) for k in q.order_by], [("COUNT(*)", True)])
        self.assertEqual(sqltext.order_plan(q).key_columns, (1,))
        self.assertFalse(sqltext.order_plan(q).augmented)

    def test_alias_and_limit(self):
        q = sqltext.parse("SELECT r, COUNT(DISTINCT u) AS u FROM t GROUP BY r ORDER BY u DESC LIMIT 10;")
        self.assertEqual(q.items[1].alias, "u")
        self.assertEqual(q.items[1].expr, "COUNT(DISTINCT u)")
        self.assertEqual((q.limit, q.offset), (10, None))
        self.assertEqual(sqltext.order_plan(q).key_columns, (1,))

    def test_limit_offset(self):
        q = sqltext.parse("SELECT u, COUNT(*) AS v FROM t GROUP BY u ORDER BY v DESC LIMIT 10 OFFSET 1000;")
        self.assertEqual((q.limit, q.offset), (10, 1000))
        self.assertEqual(q.order_by[0].expr, "v")
        q = sqltext.parse("SELECT a FROM t ORDER BY a LIMIT 1000, 10")
        self.assertEqual((q.limit, q.offset), (10, 1000))
        q = sqltext.parse("SELECT a FROM t ORDER BY a OFFSET 5")
        self.assertEqual((q.limit, q.offset), (None, 5))
        self.assertTrue(q.has_limit)

    def test_ordinal(self):
        q = sqltext.parse("SELECT 1, u, COUNT(*) AS c FROM t GROUP BY 1, u ORDER BY 2 ASC, c DESC LIMIT 10")
        self.assertEqual([(k.expr, k.descending) for k in q.order_by], [("2", False), ("c", True)])
        self.assertEqual(sqltext.order_plan(q).key_columns, (1, 2))

    def test_order_by_expression_text(self):
        q = sqltext.parse("SELECT DATE_TRUNC('minute', ts) AS m, COUNT(*) AS v FROM t "
                          "GROUP BY DATE_TRUNC('minute', ts) ORDER BY DATE_TRUNC('minute',  ts) LIMIT 10 OFFSET 1000;")
        self.assertEqual(sqltext.order_plan(q).key_columns, (0,))

    def test_strings_and_parentheses_are_opaque(self):
        q = sqltext.parse("SELECT REGEXP_REPLACE(r, '^https?://(?:www\\.)?([^/]+)/.*$', '\\1') AS k, "
                          "'a, b LIMIT 3' AS s, CASE WHEN (x = 0 AND y = 0) THEN r ELSE '' END AS src, "
                          "f(a, (b, c)) FROM t WHERE x IN (-1, 6) GROUP BY k HAVING COUNT(*) > 100000 "
                          "ORDER BY l DESC LIMIT 25;")
        self.assertEqual([i.alias for i in q.items], ["k", "s", "src", None])
        self.assertEqual(q.items[3].expr, "f(a, (b, c))")
        self.assertEqual(q.limit, 25)

    def test_subquery_limit_is_not_top_level(self):
        q = sqltext.parse("SELECT a FROM (SELECT a FROM t ORDER BY a LIMIT 3) AS s -- LIMIT 9\n")
        self.assertFalse(q.has_order_by)
        self.assertFalse(q.has_limit)

    def test_rules(self):
        cases = {
            "SELECT a FROM t": compare.MULTISET,
            "SELECT a FROM t ORDER BY a": compare.ORDERED,
            "SELECT a FROM t ORDER BY a LIMIT 3": compare.LIMIT_BORDER,
            "SELECT a, b, COUNT(*) FROM t GROUP BY a, b LIMIT 10": compare.LIMIT_UNORDERED,
        }
        for sql, rule in cases.items():
            self.assertEqual(compare.rule_for(sqltext.parse(sql)), rule, sql)

    def test_augmented_variant(self):
        q = sqltext.parse("SELECT p FROM t WHERE p <> '' ORDER BY ts LIMIT 10;")
        plan = sqltext.order_plan(q)
        self.assertTrue(plan.augmented)
        self.assertEqual(plan.sql, "SELECT p, ts FROM t WHERE p <> '' ORDER BY ts LIMIT 10")
        self.assertEqual(plan.key_columns, (-1,))
        self.assertEqual(plan.original_columns, 1)

        q = sqltext.parse("SELECT p FROM t WHERE p <> '' ORDER BY ts, p LIMIT 10;")
        plan = sqltext.order_plan(q)
        self.assertEqual(plan.sql, "SELECT p, ts FROM t WHERE p <> '' ORDER BY ts, p LIMIT 10")
        self.assertEqual(plan.key_columns, (-1, 0))

    def test_augmented_star(self):
        plan = sqltext.order_plan(sqltext.parse("SELECT * FROM t WHERE u LIKE '%g%' ORDER BY ts LIMIT 10"))
        self.assertEqual(plan.sql, "SELECT *, ts FROM t WHERE u LIKE '%g%' ORDER BY ts LIMIT 10")
        self.assertEqual(plan.key_columns, (-1,))
        self.assertIsNone(plan.original_columns)

    def test_without_limit_aliased(self):
        q = sqltext.parse("SELECT u, p, COUNT(*) FROM t GROUP BY u, p LIMIT 10;")
        self.assertEqual(sqltext.without_limit_aliased(q),
                         "SELECT u AS c1, p AS c2, COUNT(*) AS c3 FROM t GROUP BY u, p")
        with self.assertRaises(sqltext.SqlTextError):
            sqltext.without_limit_aliased(sqltext.parse("SELECT * FROM t LIMIT 1"))

    def test_top_level_with(self):
        sql = ("WITH r AS (SELECT a, sum(b) AS total FROM t ORDER BY a LIMIT 5), s AS (SELECT 1 AS x)\n"
               "SELECT a, total AS revenue FROM r ORDER BY revenue DESC LIMIT 3;")
        q = sqltext.parse(sql)
        self.assertEqual([item.alias for item in q.items], [None, "revenue"])
        self.assertEqual((q.limit, [k.expr for k in q.order_by]), (3, ["revenue"]))
        self.assertEqual(sqltext.order_plan(q).key_columns, (1,))
        self.assertTrue(sqltext.without_limit_aliased(q).startswith("WITH r AS ("))
        self.assertTrue(sqltext.without_limit_aliased(q).endswith("ORDER BY revenue DESC"))

    def test_with_without_a_select(self):
        with self.assertRaises(sqltext.SqlTextError):
            sqltext.parse("WITH r AS (SELECT 1)")

    def test_errors(self):
        for sql in ("SELECT (a FROM t", "SELECT 'a FROM t", "INSERT INTO t VALUES (1)",
                    "SELECT a FROM t UNION ALL SELECT b FROM u", "SELECT a FROM t LIMIT x"):
            with self.assertRaises(sqltext.SqlTextError, msg=sql):
                sqltext.parse(sql)


QUERIES = paths().clickbench / "queries-clickhouse.sql"


@unittest.skipUnless(QUERIES.is_file(), "pinned ClickBench queries not fetched")
class ClickBenchFormsTest(unittest.TestCase):
    """The 43 pinned queries (not versioned, §19): rules and ORDER BY resolution."""

    def test_all_queries(self):
        texts = read_queries(QUERIES)
        self.assertEqual(len(texts), QUERY_COUNT)
        rules = {}
        augmented = set()
        for n, text in enumerate(texts, 1):
            q = sqltext.parse(text)
            rules[n] = compare.rule_for(q)
            if q.has_order_by and sqltext.order_plan(q).augmented:
                augmented.add(n)
        self.assertEqual({n for n, r in rules.items() if r == compare.MULTISET}, {1, 2, 3, 4, 5, 6, 7, 20, 21, 30})
        self.assertEqual({n for n, r in rules.items() if r == compare.ORDERED}, {8})
        self.assertEqual({n for n, r in rules.items() if r == compare.LIMIT_UNORDERED}, {18})
        self.assertEqual(augmented, {24, 25, 27})
        self.assertEqual(sqltext.parse(texts[38]).offset, 1000)


if __name__ == "__main__":
    unittest.main()
