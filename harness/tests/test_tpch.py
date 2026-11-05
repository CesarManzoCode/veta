import argparse
import csv
import io
import math
import re
import unittest
from pathlib import Path

from harness import compare, tpch
from harness.config import REPO, Paths
from harness.engines import EngineRun
from harness.report import Report
from harness.tests.support import scratch_dir

P = Paths(repo=Path("/r"), data=Path("/d"), veta_bin=Path("/v"))

Q6_ANSWER = "revenue\n123141078.2283\n"
Q2_ANSWER = (
    "s_acctbal|s_name|s_comment\n"
    "9938.53|Supplier#000005359|uriously regular, requests\n"
    "9937.84|Supplier#000005969| leading space\n"
)


def veta_run(stdout: bytes, code: int = 0, seconds: float = 0.5) -> EngineRun:
    return EngineRun("veta", [], code, stdout, b"", seconds, stdout)


def duckdb_run(stdout: bytes, code: int = 0) -> EngineRun:
    return EngineRun("duckdb", [], code, stdout, b"", 0.5, stdout)


def scan(alias: str) -> dict:
    return {"op": "Scan", "est_rows": 10.0, "actual_rows": 10, "relations": [{"alias": alias, "table": alias}],
            "predicates": [], "children": []}


def node(op: str, *children, kind: str | None = None, predicates=()) -> dict:
    out = {"op": op, "est_rows": 5.0, "actual_rows": 5, "predicates": list(predicates), "children": list(children),
           "relations": [r for c in children for r in c["relations"]]}
    if kind is not None:
        out["kind"] = kind
    return out


# The example of §11.6.
EXPLAIN_EXAMPLE = {
    "op": "HashJoin", "kind": "inner", "est_rows": 1234.5, "actual_rows": 1200,
    "relations": [{"alias": "o", "table": "orders"}, {"alias": "l", "table": "lineitem"}],
    "predicates": ["(o.o_orderkey = l.l_orderkey)", "(o.o_orderdate < CAST('1995-03-15' AS DATE))"],
    "children": [scan("o"), scan("l")],
}


class SchemaTest(unittest.TestCase):
    def setUp(self):
        self.statements = tpch.statements((REPO / "bench" / "tpch" / "create.sql").read_text(encoding="utf-8"))

    def test_eight_tables_in_loading_order(self):
        names = [re.match(r"CREATE TABLE (\w+) \(", s).group(1) for s in self.statements]
        self.assertEqual(tuple(names), tpch.TABLES)

    def test_primary_keys_of_the_contract(self):
        keys = {
            "lineitem": "l_orderkey, l_linenumber", "orders": "o_orderkey", "customer": "c_custkey",
            "part": "p_partkey", "partsupp": "ps_partkey, ps_suppkey", "supplier": "s_suppkey",
            "nation": "n_nationkey", "region": "r_regionkey",
        }
        for table, statement in zip(tpch.TABLES, self.statements):
            self.assertIn(f"PRIMARY KEY ({keys[table]})", statement)

    def test_clickhouse_adds_the_engine_to_each_table(self):
        for statement in self.statements:
            self.assertTrue(tpch.clickhouse_create(statement).endswith(") ENGINE = MergeTree"))

    def test_statements_drop_the_empty_ends(self):
        self.assertEqual(tpch.statements("\nCREATE TABLE a (x INT);\n\nCREATE TABLE b (y INT);\n"),
                         ["CREATE TABLE a (x INT)", "CREATE TABLE b (y INT)"])


class LoadSqlTest(unittest.TestCase):
    def test_duckdb_generates_within_the_memory_limit(self):
        self.assertEqual(tpch.duckdb_generate_sql(10), "SET memory_limit='4GB'; LOAD tpch; CALL dbgen(sf=10);")

    def test_export_and_insert_name_the_parquet_of_the_scale_factor(self):
        file = tpch.parquet_file(P, 1, "region")
        self.assertEqual(file, Path("/d/tpch/sf1/region.parquet"))
        self.assertEqual(tpch.duckdb_export_sql("region", file),
                         "SET memory_limit='4GB'; COPY region TO '/d/tpch/sf1/region.parquet' (FORMAT parquet);")
        self.assertEqual(tpch.clickhouse_insert_sql("region", file),
                         "INSERT INTO region SELECT * FROM file('/d/tpch/sf1/region.parquet', Parquet)")

    def test_expected_rows(self):
        sf1, sf10 = tpch.expected_rows(1), tpch.expected_rows(10)
        self.assertEqual(sorted(sf1), sorted(tpch.TABLES))
        self.assertEqual(sf1["lineitem"], 6_001_215)
        self.assertEqual(sf10["lineitem"], 59_986_052)
        self.assertEqual(sf10["orders"], 15_000_000)
        self.assertEqual((sf10["nation"], sf10["region"]), (25, 5))

    def test_single_count(self):
        self.assertEqual(tpch.single_count(veta_run(b"25\n")), 25)
        self.assertIsNone(tpch.single_count(veta_run(b"25\n", code=1)))
        self.assertIsNone(tpch.single_count(veta_run(b"")))
        self.assertIsNone(tpch.single_count(veta_run(b"1\n2\n")))
        self.assertIsNone(tpch.single_count(veta_run(b"1\t2\n")))
        self.assertIsNone(tpch.single_count(veta_run(b"x\n")))

    def test_report_names(self):
        self.assertEqual(tpch.load_report_name(argparse.Namespace(sf=10)), "load-tpch-sf10")
        self.assertEqual(tpch.verify_report_name(argparse.Namespace(sf=1)), "tpch-verify-sf1")


class AnswerTest(unittest.TestCase):
    def test_parse_answer_keeps_the_cells_verbatim(self):
        header, rows = tpch.parse_answer(Q2_ANSWER)
        self.assertEqual(header, ["s_acctbal", "s_name", "s_comment"])
        self.assertEqual(rows[0], ["9938.53", "Supplier#000005359", "uriously regular, requests"])
        self.assertEqual(rows[1][2], " leading space")

    def test_parse_answer_rejects_what_is_not_a_table(self):
        with self.assertRaises(tpch.AnswerError):
            tpch.parse_answer("")
        with self.assertRaises(tpch.AnswerError):
            tpch.parse_answer("a|b\n1\n")

    def test_answers_from_output(self):
        out = io.StringIO(newline="")
        csv.writer(out, lineterminator="\n").writerows([[n, f"c\n{n}\n"] for n in range(1, 23)])
        answers = tpch.answers_from_output(out.getvalue())
        self.assertEqual(len(answers), 22)
        self.assertEqual(answers["q07"], "c\n7\n")
        with self.assertRaises(tpch.AnswerError):
            tpch.answers_from_output("1,\"c\n1\n\"\n")

    def test_answer_run_prints_csv_and_keeps_the_answer_as_raw(self):
        run = tpch.answer_run("SELECT 1", "SELECT 1", Q2_ANSWER)
        self.assertTrue(run.ok)
        self.assertEqual(run.stdout, Q2_ANSWER.encode())
        self.assertEqual(len(run.rows), 2)
        self.assertIn('"uriously regular, requests"', run.text)

    def test_answer_run_only_holds_the_query_as_written(self):
        self.assertFalse(tpch.answer_run("SELECT 1, 2", "SELECT 1", Q2_ANSWER).ok)
        self.assertFalse(tpch.answer_run("SELECT 1", "SELECT 1", "a|b\n1\n").ok)

    def compared(self, sql: str, answer: str, stdout: bytes) -> compare.Comparison:
        veta = compare.Side("veta", lambda _: veta_run(stdout), "veta")
        return compare.verify_query("q01", veta, tpch.answer_side(sql, answer), sql, sql)

    def test_veta_against_an_answer_with_the_rules_of_the_contract(self):
        sql = "SELECT sum(x) AS revenue FROM lineitem;"
        c = self.compared(sql, Q6_ANSWER, b"123141078.2283\n")
        self.assertEqual((c.rule, c.verdict), (compare.MULTISET, compare.EQUAL))
        self.assertEqual(self.compared(sql, Q6_ANSWER, b"123141079.2283\n").verdict, compare.DIFFERENT)

    def test_an_ordered_query_with_with_is_compared(self):
        sql = "WITH r AS (SELECT a FROM t) SELECT a, b FROM r, s ORDER BY a;"
        c = self.compared(sql, "a|b\n1|x\n2|y\n", b"1\tx\n2\ty\n")
        self.assertEqual((c.rule, c.verdict), (compare.ORDERED, compare.EQUAL))
        self.assertEqual(self.compared(sql, "a|b\n1|x\n2|y\n", b"2\ty\n1\tx\n").verdict, compare.DIFFERENT)

    def test_a_query_that_needs_the_augmented_variant_is_an_error(self):
        sql = "SELECT a FROM t ORDER BY b;"
        self.assertEqual(self.compared(sql, "a\n1\n", b"1\n").verdict, compare.ERROR)


class ReorderTest(unittest.TestCase):
    def test_best_of_needs_every_run(self):
        self.assertEqual(tpch.best_of([0.3, 0.1, 0.2]), 0.1)
        self.assertIsNone(tpch.best_of([0.3, 0.1]))

    def test_totals_and_factor(self):
        items = [{"cost_best": 1.0, "syntactic_best": 2.0}, {"cost_best": 3.0, "syntactic_best": 4.0}]
        self.assertEqual(tpch.reorder_totals(items), (4.0, 6.0, 1.5))

    def test_a_missing_time_leaves_no_factor(self):
        items = [{"cost_best": 1.0, "syntactic_best": 2.0}, {"cost_best": None, "syntactic_best": 4.0}]
        self.assertEqual(tpch.reorder_totals(items), (None, None, None))
        self.assertEqual(tpch.reorder_totals([]), (None, None, None))

    def test_timed_side_notes_only_finished_runs(self):
        seconds = []
        runs = iter([veta_run(b"1\n", seconds=0.25), veta_run(b"", code=1)])
        side = tpch.timed_side(compare.Side("veta", lambda _: next(runs), "veta", ".syntactic"), seconds)
        self.assertEqual(side.suffix, ".syntactic")
        side.run("SELECT 1")
        side.run("SELECT 1")
        self.assertEqual(seconds, [0.25])


class CountSqlTest(unittest.TestCase):
    def test_count_sql_of_a_join_node(self):
        self.assertEqual(
            tpch.node_count_sql(EXPLAIN_EXAMPLE),
            "SELECT count(*) FROM orders AS o, lineitem AS l WHERE (o.o_orderkey = l.l_orderkey) "
            "AND (o.o_orderdate < CAST('1995-03-15' AS DATE))",
        )

    def test_a_cross_join_has_no_where_and_repeated_predicates_go_once(self):
        cross = node("NestedLoopJoin", scan("a"), scan("b"), kind="cross")
        self.assertEqual(tpch.node_count_sql(cross), "SELECT count(*) FROM a AS a, b AS b")
        cross["predicates"] = ["(a.x > 1)", "(a.x > 1)"]
        self.assertEqual(tpch.node_count_sql(cross), "SELECT count(*) FROM a AS a, b AS b WHERE (a.x > 1)")

    def test_measurable_joins_in_preorder(self):
        inner = node("HashJoin", scan("a"), node("Filter", scan("b")), kind="inner")
        outer = node("NestedLoopJoin", node("Project", inner), scan("c"), kind="cross")
        tree = node("TopN", node("HashAggregate", outer))
        self.assertEqual(tpch.measurable_joins(tree), [outer, inner])

    def test_a_join_over_anything_else_is_left_out(self):
        for kind in ("left", "semi", "anti", "null_aware_anti", "single"):
            below = node("HashJoin", scan("a"), scan("b"), kind=kind)
            above = node("HashJoin", below, scan("c"), kind="inner")
            self.assertEqual(tpch.measurable_joins(above), [], kind)
        aggregate = node("HashJoin", node("HashAggregate", scan("a")), scan("b"), kind="inner")
        self.assertEqual(tpch.measurable_joins(aggregate), [])
        parquet = node("HashJoin", dict(scan("a"), op="ParquetScan"), scan("b"), kind="inner")
        self.assertEqual(tpch.measurable_joins(parquet), [])

    def test_an_inner_join_below_an_outer_one_still_counts(self):
        inner = node("HashJoin", scan("a"), scan("b"), kind="inner")
        tree = node("HashJoin", inner, scan("c"), kind="left")
        self.assertEqual(tpch.measurable_joins(tree), [inner])


class StatsTest(unittest.TestCase):
    def test_q_error_is_symmetric(self):
        self.assertEqual(tpch.q_error(100, 50), 2.0)
        self.assertEqual(tpch.q_error(50, 100), 2.0)

    def test_q_error_with_values_under_one(self):
        self.assertEqual(tpch.q_error(0.25, 0), 1.0)
        self.assertEqual(tpch.q_error(0, 8), 8.0)
        self.assertEqual(tpch.q_error(8.0, 0), 8.0)

    def test_median(self):
        self.assertEqual(tpch.median([3.0, 1.0, 2.0]), 2.0)
        self.assertEqual(tpch.median([4.0, 1.0, 2.0, 3.0]), 2.5)
        self.assertIsNone(tpch.median([]))

    def test_p90_is_the_nearest_rank(self):
        self.assertEqual(tpch.percentile_90([float(n) for n in range(1, 11)]), 9.0)
        self.assertEqual(tpch.percentile_90([float(n) for n in range(11, 0, -1)]), 10.0)
        self.assertEqual(tpch.percentile_90([7.0]), 7.0)
        self.assertIsNone(tpch.percentile_90([]))

    def items(self, tpch_q=2.0, nulls_q=3.0, verdict="equal"):
        tpch_items = [{"suite": "tpch", "q": tpch_q, "verdict": verdict} for _ in range(5)]
        nulls = [{"suite": "nulls", "q": nulls_q, "verdict": "equal"} for _ in tpch.NULL_QUERIES]
        return tpch_items + nulls

    def test_stats_keep_the_suites_apart(self):
        items = self.items(tpch_q=2.0, nulls_q=3.0)
        items.append({"suite": "tpch", "q": None, "verdict": "error"})
        self.assertEqual(tpch.cardinality_stats(items), {"median_q": 2.0, "p90_q": 2.0, "max_q_nulls": 3.0})

    def passes(self, items) -> bool:
        return tpch.cardinality_passes(items, tpch.cardinality_stats(items))

    def test_thresholds(self):
        self.assertTrue(self.passes(self.items()))
        self.assertFalse(self.passes(self.items(tpch_q=2.5)))
        self.assertFalse(self.passes(self.items(nulls_q=3.5)))

    def test_a_count_that_differs_fails(self):
        self.assertFalse(self.passes(self.items(verdict="different")))

    def test_an_unmeasured_suite_fails(self):
        items = self.items()
        self.assertFalse(self.passes([i for i in items if i["suite"] == "tpch"]))
        self.assertFalse(self.passes([i for i in items if i["suite"] == "nulls"]))
        self.assertFalse(self.passes(items[:-1]))


class NullSuiteTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.na, cls.nb = tpch.null_suite_rows()

    def test_is_deterministic(self):
        self.assertEqual(tpch.null_suite_rows(), (self.na, self.nb))
        self.assertNotEqual(tpch.null_suite_rows(seed=2)[0], self.na)

    def test_sizes_of_the_contract(self):
        self.assertEqual((len(self.na), len(self.nb)), (200_000, 20_000))

    def test_ninety_percent_of_na_k_is_null(self):
        self.assertEqual(sum(k is None for k, _ in self.na), 180_000)

    def test_nb_k_is_unique_and_not_null(self):
        keys = [k for k, _ in self.nb]
        self.assertEqual(len(set(keys)), len(keys))
        self.assertNotIn(None, keys)

    def test_every_key_of_na_matches_one_row_of_nb(self):
        keys = {k for k, _ in self.nb}
        self.assertTrue(all(k in keys for k, _ in self.na if k is not None))

    def test_ten_queries_with_varied_filters(self):
        self.assertEqual(len(set(tpch.NULL_QUERIES)), 10)
        for sql in tpch.NULL_QUERIES:
            self.assertTrue(sql.startswith("SELECT count(*) FROM na JOIN nb ON na.k = nb.k"), sql)
        self.assertEqual(sum(" WHERE " not in sql for sql in tpch.NULL_QUERIES), 1)

    def test_tsv_writes_null_as_the_format_says(self):
        self.assertEqual(tpch.tsv_text([(None, 1), (2, 3)]), "\\N\t1\n2\t3\n")

    def test_duckdb_reads_the_tsv_with_tab_and_null(self):
        self.assertEqual(tpch.duckdb_copy_sql("na", Path("/s/na.tsv")),
                         "COPY na FROM '/s/na.tsv' (FORMAT csv, DELIMITER E'\\t', NULL '\\N', HEADER false);")


class FakeEngines:
    """`Engines` for one measure: fixed outputs instead of processes."""

    def __init__(self, veta: EngineRun, duckdb: EngineRun):
        self.veta, self.duck = veta, duckdb
        self.extras = []

    def veta_sql(self, db, sql, stats=False, extra=()):
        self.extras.append(tuple(extra))
        return self.veta

    def duckdb(self, db, sql):
        return self.duck


class CounterTest(unittest.TestCase):
    def measured(self, veta: EngineRun, duckdb: EngineRun) -> tuple[dict, Report, FakeEngines]:
        with scratch_dir() as d:
            report = Report("cardinality", Path(d))
            engines = FakeEngines(veta, duckdb)
            counter = tpch.Counter(engines, report, "tpch", Path("/v"), Path("/d"))
            counter.measure("q05", "q05.node1", EXPLAIN_EXAMPLE, tpch.node_count_sql(EXPLAIN_EXAMPLE))
            self.assertTrue((report.dir / "raw" / "veta" / "q05.node1.out").is_file())
            self.assertTrue((report.dir / "raw" / "duckdb" / "q05.node1.out").is_file())
            return report.items[0], report, engines

    def test_item_with_the_fields_of_the_contract(self):
        item, _, engines = self.measured(veta_run(b"2469\n"), duckdb_run(b"2469\n"))
        self.assertEqual(engines.extras, [("--no-optimizer",)])
        self.assertEqual(item["suite"], "tpch")
        self.assertEqual(item["query"], "q05")
        self.assertEqual(item["relations"], EXPLAIN_EXAMPLE["relations"])
        self.assertEqual(item["est_rows"], 1234.5)
        self.assertEqual((item["veta_count"], item["duckdb_count"]), (2469, 2469))
        self.assertTrue(math.isclose(item["q"], 2.0))
        self.assertEqual(item["verdict"], compare.EQUAL)

    def test_counts_that_differ(self):
        item, _, _ = self.measured(veta_run(b"10\n"), duckdb_run(b"11\n"))
        self.assertEqual(item["verdict"], compare.DIFFERENT)

    def test_a_failed_count_is_an_error(self):
        item, _, _ = self.measured(veta_run(b"", code=1), duckdb_run(b"11\n"))
        self.assertEqual((item["verdict"], item["q"]), (compare.ERROR, None))

    def test_explain_failures_become_error_items(self):
        for run in (veta_run(b"", code=1), veta_run(b"not json\n"), veta_run(b"[]\n")):
            with scratch_dir() as d:
                report = Report("cardinality", Path(d))
                counter = tpch.Counter(FakeEngines(run, duckdb_run(b"")), report, "nulls", Path("/v"), Path("/d"))
                self.assertIsNone(counter.explain("n01", "SELECT 1"))
                self.assertEqual((report.items[0]["suite"], report.items[0]["verdict"]), ("nulls", compare.ERROR))

    def test_explain_parses_the_tree(self):
        with scratch_dir() as d:
            report = Report("cardinality", Path(d))
            run = veta_run(b'{"op":"Scan","est_rows":1.0,"relations":[],"predicates":[],"children":[]}\n')
            engines = FakeEngines(run, duckdb_run(b""))
            tree = tpch.Counter(engines, report, "tpch", Path("/v"), Path("/d")).explain("q06", "SELECT 1")
            self.assertEqual(tree["op"], "Scan")
            self.assertEqual(engines.extras, [("--explain-analyze",)])
            self.assertEqual(report.items, [])


if __name__ == "__main__":
    unittest.main()
