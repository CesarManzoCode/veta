import datetime
import random
import unittest
from decimal import Decimal
from pathlib import Path

from harness import compare, gen, sqltext
from harness.normalize import unescape_tsv
from harness.tests.support import scratch_dir

SAMPLE = 400


def sample(seed: int = 1) -> list[gen.GenQuery]:
    return [gen.generate_query(seed, i) for i in range(SAMPLE)]


class DeterminismTest(unittest.TestCase):
    def test_same_seed_same_queries(self):
        self.assertEqual(sample(), sample())

    def test_same_seed_same_data(self):
        self.assertEqual(gen.generate_dataset(1, 0), gen.generate_dataset(1, 0))

    def test_another_seed_changes_queries_and_data(self):
        self.assertNotEqual([q.sql for q in sample(1)], [q.sql for q in sample(2)])
        self.assertNotEqual(gen.generate_dataset(1, 0), gen.generate_dataset(2, 0))

    def test_a_query_does_not_depend_on_the_others(self):
        self.assertEqual(gen.generate_query(1, 123), sample()[123])

    def test_batches_are_deterministic(self):
        table = gen.generate_dataset(1, 0)[0]
        with scratch_dir() as a, scratch_dir() as b:
            files_a = gen.write_batches(1, 0, table, Path(a))
            files_b = gen.write_batches(1, 0, table, Path(b))
            self.assertEqual([f.read_bytes() for f in files_a], [f.read_bytes() for f in files_b])


class QueryShapeTest(unittest.TestCase):
    def test_every_query_parses_with_sqltext(self):
        for q in sample():
            for sql in (q.sql, *q.partition_sqls()):
                with self.subTest(sql=sql):
                    self.assertTrue(sqltext.parse(sql).items)

    def test_every_output_column_has_its_alias(self):
        for q in sample():
            parsed = sqltext.parse(q.sql)
            self.assertEqual([i.alias for i in parsed.items], [f"c{n}" for n in range(len(q.columns))])

    def test_order_is_total_and_comes_with_limit(self):
        ordered = 0
        for q in sample():
            parsed = sqltext.parse(q.sql)
            rule = compare.rule_for(parsed)
            self.assertIn(rule, (compare.MULTISET, compare.LIMIT_BORDER))
            if rule == compare.LIMIT_BORDER:
                ordered += 1
                plan = sqltext.order_plan(parsed)
                self.assertFalse(plan.augmented)
                self.assertEqual(sorted(plan.key_columns), list(range(len(q.columns))))
                self.assertEqual(parsed.limit, q.limit)
                self.assertEqual(parsed.offset, q.offset)
        self.assertGreater(ordered, 0)

    def test_partitions_have_no_window_and_hold_the_three_predicates(self):
        for q in sample():
            p, not_p, is_null = q.partition_sqls()
            for sql in (p, not_p, is_null):
                parsed = sqltext.parse(sql)
                self.assertFalse(parsed.has_order_by or parsed.has_limit)
            self.assertIn(q.partition, p)
            self.assertIn(f"NOT ({q.partition})", not_p)
            self.assertIn(f"({q.partition}) IS NULL", is_null)
            if q.where is not None:
                self.assertIn(f"({q.where}) AND (", p)

    def test_the_forms_of_the_contract_appear(self):
        queries = sample()
        sqls = [q.sql for q in queries]
        self.assertTrue(any(" LEFT JOIN " in s for s in sqls))
        self.assertTrue(any(" JOIN " in s.replace(" LEFT JOIN ", " ") for s in sqls))
        self.assertTrue(any(q.distinct for q in queries))
        self.assertTrue(any(q.group_by for q in queries))
        self.assertTrue(any(q.aggregated and not q.group_by for q in queries))
        self.assertTrue(any(q.where is None for q in queries))
        kinds = {c.aggregate for q in queries for c in q.columns}
        self.assertEqual(kinds, {None, "COUNT", "SUM", "MIN", "MAX"})

    def test_where_reaches_the_side_that_adds_nulls(self):
        hits = 0
        for seed in range(200):
            rng = random.Random(seed)
            _, cols, null_side = gen.random_from(rng)
            where = gen.random_where(rng, cols, null_side)
            if where and null_side and any(str(c) in where for c in null_side):
                hits += 1
        self.assertGreater(hits, 0)

    def test_no_avg_and_no_sum_over_non_numeric_columns(self):
        for q in sample():
            for c in q.columns:
                self.assertNotIn("AVG", c.expr)
                if c.aggregate == "SUM":
                    self.assertTrue(any(name in c.expr for name in gen.NUMERIC), c.expr)

    def test_datasets_change_every_so_many_queries(self):
        self.assertEqual(gen.generate_query(1, gen.QUERIES_PER_DATASET - 1).dataset, 0)
        self.assertEqual(gen.generate_query(1, gen.QUERIES_PER_DATASET).dataset, 1)


class DataTest(unittest.TestCase):
    def test_three_tables_within_the_limits(self):
        for dataset in range(5):
            tables = gen.generate_dataset(1, dataset)
            self.assertEqual([t.name for t in tables], list(gen.TABLES))
            for table in tables:
                self.assertLessEqual(len(table.rows), gen.MAX_ROWS)
                for row in table.rows:
                    self.assertEqual(len(row), len(gen.COLUMNS))

    def test_values_have_the_type_of_their_column(self):
        kinds = {"cbig": int, "cint": int, "cdec": Decimal, "cstr": str, "cdate": datetime.date, "cbool": bool}
        rng = random.Random(1)
        for column, kind in kinds.items():
            for _ in range(200):
                self.assertIsInstance(gen.random_value(rng, column), kind)

    def test_strings_stay_in_the_alphabet(self):
        rng = random.Random(1)
        for _ in range(200):
            self.assertLessEqual(set(gen.random_string(rng)), set(gen.ALPHABET))

    def test_create_sql_has_all_the_types(self):
        sql = gen.Table("t0", ("cbig",), ()).create_sql
        for fragment in ("cbig BIGINT", "cint INTEGER", "cdec DECIMAL(10,2)", "cstr VARCHAR", "cdate DATE",
                         "cbool BOOLEAN", "PRIMARY KEY (cbig)"):
            self.assertIn(fragment, sql)
        self.assertNotIn("PRIMARY KEY", gen.Table("t1", (), ()).create_sql)

    def test_tsv_format(self):
        row = (None, -5, Decimal(-1250).scaleb(-2), "a\tb", datetime.date(2020, 3, 1), True)
        self.assertEqual(gen.tsv_text([row]), "\\N\t-5\t-12.50\ta\\tb\t2020-03-01\ttrue\n")
        self.assertEqual(gen.tsv_cell(False), "false")

    def test_string_escapes_round_trip(self):
        text = "\\\t\n\r\0\b\f plain"
        self.assertEqual(unescape_tsv(gen.tsv_cell(text)), text)

    def test_batches_keep_the_rows_and_none_is_empty(self):
        rng = random.Random(1)
        for size in (0, 1, 2, 3, 10, 2000):
            rows = tuple((i,) for i in range(size))
            batches = gen.split_batches(rng, rows)
            self.assertEqual(tuple(r for b in batches for r in b), rows)
            self.assertTrue(all(batches))
            if size >= 2:
                self.assertGreaterEqual(len(batches), 2)


if __name__ == "__main__":
    unittest.main()
