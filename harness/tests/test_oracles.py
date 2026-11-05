import argparse
import unittest
from pathlib import Path

from harness import compare, gen, oracles
from harness.config import Paths
from harness.engines import EngineRun
from harness.report import FAIL, PASS, Infra, Report
from harness.tests.support import scratch_dir

Term = gen.OrderTerm


def query(columns, **fields) -> gen.GenQuery:
    return gen.GenQuery(
        index=0, dataset=0, columns=tuple(gen.OutputColumn(e, a) for e, a in columns), from_clause="t0 AS x0",
        where=fields.pop("where", None), partition="x0.cint > 0", **fields,
    )


PLAIN = query([("x0.cint", None), ("x0.cstr", None)])
DISTINCT = query([("x0.cint", None)], distinct=True)
GLOBAL = query([("COUNT(*)", "COUNT"), ("SUM(x0.cint)", "SUM"), ("MIN(x0.cint)", "MIN"), ("MAX(x0.cint)", "MAX")],
               aggregated=True)
GROUPED = query([("x0.cstr", None), ("COUNT(x0.cint)", "COUNT"), ("SUM(x0.cdec)", "SUM")],
                aggregated=True, group_by=("c0",))
ORDERED = query([("x0.cint", None)], order_by=(Term(0),), limit=2)


def run_ok(stdout: bytes) -> EngineRun:
    return EngineRun("veta", [], 0, stdout, b"", 0.1, stdout)


def runner(q: gen.GenQuery, whole: str, p: str, not_p: str, is_null: str, failing: set[str] = frozenset()):
    """A simulated veta that answers `q` and each partition from fixed TSV text."""
    outputs = dict(zip((q.sql, *q.partition_sqls()), (whole, p, not_p, is_null)))

    def run(sql: str) -> EngineRun:
        if sql in failing:
            return EngineRun("veta", [], 1, b"", b"unsupported: something", 0.1, b"")
        return run_ok(outputs[sql].encode())

    return run


class RecomposeTest(unittest.TestCase):
    def test_plain_union_keeps_duplicates(self):
        got = oracles.recompose(PLAIN, [[(1, "a")], [(1, "a"), (None, None)], []])
        self.assertEqual(got, [(1, "a"), (1, "a"), (None, None)])

    def test_distinct_deduplicates_after_the_union(self):
        got = oracles.recompose(DISTINCT, [[(1,), (2,)], [(1,)], [(None,), (2,)]])
        self.assertEqual(got, [(1,), (2,), (None,)])

    def test_count_and_sum_add_up_and_min_max_take_the_extremes(self):
        got = oracles.recompose(GLOBAL, [[(2, 5, 1, 4)], [(3, 10, -2, 3)], [(1, 7, 9, 9)]])
        self.assertEqual(got, [(6, 22, -2, 9)])

    def test_sum_of_all_null_is_null(self):
        got = oracles.recompose(GLOBAL, [[(2, None, None, None)], [(0, None, None, None)], [(1, None, None, None)]])
        self.assertEqual(got, [(3, None, None, None)])

    def test_null_is_ignored_next_to_a_value(self):
        got = oracles.recompose(GLOBAL, [[(0, None, None, None)], [(1, 7, 7, 7)], [(0, None, None, None)]])
        self.assertEqual(got, [(1, 7, 7, 7)])

    def test_min_of_an_empty_partition_does_not_win(self):
        q = query([("MIN(x0.cstr)", "MIN")], aggregated=True)
        self.assertEqual(oracles.recompose(q, [[(None,)], [("",)], [(None,)]]), [("",)])

    def test_group_by_regroups_and_null_is_one_key(self):
        parts = [
            [("a", 1, 1.5), (None, 2, None)],
            [("a", 2, None), ("b", 0, None)],
            [(None, 1, 2.25)],
        ]
        got = oracles.recompose(GROUPED, parts)
        self.assertCountEqual(got, [("a", 3, 1.5), (None, 3, 2.25), ("b", 0, None)])

    def test_group_keys_may_be_in_any_position(self):
        q = query([("COUNT(*)", "COUNT"), ("x0.cstr", None)], aggregated=True, group_by=("c1",))
        self.assertEqual(oracles.recompose(q, [[(1, "a")], [(2, "a")], []]), [(3, "a")])

    def test_no_groups_stay_no_groups(self):
        self.assertEqual(oracles.recompose(GROUPED, [[], [], []]), [])

    def test_decimal_sums_are_exact(self):
        self.assertEqual(oracles.add_numbers(0.1, 0.2), 0.3)
        self.assertEqual(oracles.add_numbers(99999999.99, -99999999.98), 0.01)
        self.assertEqual(oracles.add_numbers(2**70, 1), 2**70 + 1)

    def test_order_and_window_apply_after_the_union(self):
        q = query([("x0.cint", None)], order_by=(Term(0),), limit=2, offset=1)
        self.assertEqual(oracles.recompose(q, [[(3,), (None,)], [(1,)], [(2,)]]), [(2,), (3,)])

    def test_nulls_go_last_in_asc_and_first_in_desc(self):
        rows = [(2,), (None,), (1,)]
        self.assertEqual(oracles.sort_rows(rows, (Term(0),)), [(1,), (2,), (None,)])
        self.assertEqual(oracles.sort_rows(rows, (Term(0, True),)), [(None,), (2,), (1,)])
        self.assertEqual(oracles.sort_rows(rows, (Term(0, False, "FIRST"),)), [(None,), (1,), (2,)])
        self.assertEqual(oracles.sort_rows(rows, (Term(0, True, "LAST"),)), [(2,), (1,), (None,)])

    def test_the_first_term_decides_and_later_ones_break_ties(self):
        rows = [(1, 1), (2, 5), (1, 2), (2, None)]
        got = oracles.sort_rows(rows, (Term(0, True), Term(1)))
        self.assertEqual(got, [(2, 5), (2, None), (1, 1), (1, 2)])


class CheckTlpTest(unittest.TestCase):
    def test_equal_union_with_null_parts(self):
        run = runner(GLOBAL, "3\t3\t1\t2\n", "2\t3\t1\t2\n", "0\t\\N\t\\N\t\\N\n", "1\t\\N\t\\N\t\\N\n")
        self.assertEqual(oracles.check_tlp(GLOBAL, "r0000", run).verdict, compare.EQUAL)

    def test_a_union_that_differs_is_detected(self):
        run = runner(PLAIN, "1\ta\n2\tb\n", "1\ta\n", "", "")
        outcome = oracles.check_tlp(PLAIN, "r0000", run)
        self.assertEqual((outcome.verdict, outcome.rule), (compare.DIFFERENT, compare.MULTISET))
        self.assertIn("row count", outcome.reason)

    def test_a_wrong_value_with_the_same_row_count_is_detected(self):
        run = runner(PLAIN, "1\ta\n2\tb\n", "1\ta\n", "2\tc\n", "")
        self.assertEqual(oracles.check_tlp(PLAIN, "r0000", run).verdict, compare.DIFFERENT)

    def test_a_wrong_aggregate_is_detected(self):
        run = runner(GLOBAL, "4\t3\t1\t2\n", "2\t3\t1\t2\n", "0\t\\N\t\\N\t\\N\n", "1\t\\N\t\\N\t\\N\n")
        self.assertEqual(oracles.check_tlp(GLOBAL, "r0000", run).verdict, compare.DIFFERENT)

    def test_a_change_of_order_alone_is_benign(self):
        run = runner(PLAIN, "2\tb\n\\N\t\\N\n1\ta\n", "1\ta\n", "2\tb\n", "\\N\t\\N\n")
        self.assertEqual(oracles.check_tlp(PLAIN, "r0000", run).verdict, compare.EQUAL)

    def test_order_counts_when_the_query_fixes_it(self):
        good = runner(ORDERED, "1\n2\n", "2\n", "1\n", "\\N\n")
        self.assertEqual(oracles.check_tlp(ORDERED, "r0000", good).verdict, compare.EQUAL)
        bad = runner(ORDERED, "2\n1\n", "2\n", "1\n", "\\N\n")
        outcome = oracles.check_tlp(ORDERED, "r0000", bad)
        self.assertEqual((outcome.verdict, outcome.rule), (compare.DIFFERENT, compare.LIMIT_BORDER))

    def test_an_engine_error_in_any_run_is_an_error(self):
        for failing in (PLAIN.sql, *PLAIN.partition_sqls()):
            run = runner(PLAIN, "1\ta\n", "1\ta\n", "", "", failing={failing})
            outcome = oracles.check_tlp(PLAIN, "r0000", run)
            self.assertEqual(outcome.verdict, compare.ERROR)
            self.assertIn("unsupported", outcome.reason)

    def test_a_row_of_the_wrong_width_is_detected(self):
        run = runner(PLAIN, "1\ta\n", "1\n", "", "")
        self.assertEqual(oracles.check_tlp(PLAIN, "r0000", run).verdict, compare.DIFFERENT)

    def test_values_that_cannot_be_recomposed_are_detected(self):
        run = runner(GLOBAL, "3\t3\t1\t2\n", "x\t3\t1\t2\n", "1\t3\t1\t2\n", "1\t3\t1\t2\n")
        self.assertEqual(oracles.check_tlp(GLOBAL, "r0000", run).verdict, compare.DIFFERENT)

    def test_each_run_is_stored_raw(self):
        stored = []
        run = runner(PLAIN, "1\ta\n", "1\ta\n", "", "")
        oracles.check_tlp(PLAIN, "r0007", run, lambda engine, item, out: stored.append((engine, item, out)))
        self.assertEqual(stored, [
            ("veta", "r0007", b"1\ta\n"),
            ("veta", "r0007.p", b"1\ta\n"),
            ("veta", "r0007.not-p", b""),
            ("veta", "r0007.p-is-null", b""),
        ])


def side(stdout: bytes, code: int = 0, suffix: str = "") -> compare.Side:
    err = b"boom" if code else b""
    return compare.Side("veta", lambda sql: EngineRun("veta", [], code, stdout, err, 0.1, stdout), "veta", suffix)


class CheckOnOffTest(unittest.TestCase):
    SQL = "SELECT a AS c0 FROM t"
    ORDERED_SQL = "SELECT a AS c0 FROM t ORDER BY c0 ASC LIMIT 5"

    def check(self, sql, on, off):
        return oracles.check_on_off("r0000", sql, on, off)

    def test_same_rows_in_another_order_are_benign(self):
        outcome = self.check(self.SQL, side(b"1\n2\n"), side(b"2\n1\n", suffix=".off"))
        self.assertEqual((outcome.verdict, outcome.rule), (compare.EQUAL, compare.MULTISET))

    def test_other_rows_differ(self):
        self.assertEqual(self.check(self.SQL, side(b"1\n"), side(b"2\n")).verdict, compare.DIFFERENT)

    def test_order_counts_when_the_query_fixes_it(self):
        outcome = self.check(self.ORDERED_SQL, side(b"1\n2\n"), side(b"2\n1\n"))
        self.assertEqual((outcome.verdict, outcome.rule), (compare.DIFFERENT, compare.LIMIT_BORDER))

    def test_an_error_on_either_side_is_an_error(self):
        self.assertEqual(self.check(self.SQL, side(b"", 1), side(b"1\n")).verdict, compare.ERROR)
        self.assertEqual(self.check(self.SQL, side(b"1\n"), side(b"", 1)).verdict, compare.ERROR)

    def test_the_off_side_runs_with_no_optimizer(self):
        calls = []

        def run_veta(db, sql, extra=()):
            calls.append((db, sql, tuple(extra)))
            return run_ok(b"")

        on, off = oracles.on_off_sides(run_veta, Path("/db"))
        on.run("S")
        off.run("S")
        self.assertEqual(calls, [(Path("/db"), "S", ()), (Path("/db"), "S", ("--no-optimizer",))])
        self.assertNotEqual(on.suffix, off.suffix)


class TallyTest(unittest.TestCase):
    def tally(self, directory: str) -> oracles.Tally:
        return oracles.Tally(Report("tlp", Path(directory)))

    def test_only_discrepancies_become_items(self):
        with scratch_dir() as d:
            tally = self.tally(d)
            tally.record("r0", "S0", compare.Outcome(compare.EQUAL, compare.MULTISET, ""))
            tally.record("r1", "S1", compare.Outcome(compare.DIFFERENT, compare.MULTISET, "why"), partition="p")
            tally.record("r2", "S2", compare.Outcome(compare.ERROR, compare.MULTISET, "boom"))
            self.assertEqual(tally.counts, {"queries": 3, "discrepancies": 2, "errors": 1})
            items = tally.report.items
            self.assertEqual([i["sql"] for i in items], ["S1", "S2"])
            self.assertEqual(items[0]["partition"], "p")
            self.assertIs(tally.report.fields["counts"], tally.counts)

    def test_verdict(self):
        with scratch_dir() as d:
            tally = self.tally(d)
            tally.counts.update(queries=1000)
            self.assertEqual(tally.verdict(1000)[0], PASS)
            self.assertEqual(tally.verdict(1000)[1], "1000 queries, 0 discrepancies, 0 errors")
            self.assertEqual(tally.verdict(1001)[0], FAIL)
            tally.counts.update(discrepancies=1)
            self.assertEqual(tally.verdict(1000)[0], FAIL)
            tally.counts.update(discrepancies=0, errors=1)
            self.assertEqual(tally.verdict(1000)[0], FAIL)


class CommandTest(unittest.TestCase):
    def test_a_missing_binary_is_an_infrastructure_error(self):
        args = argparse.Namespace(queries=1, seed=1)
        with scratch_dir() as d:
            data = Path(d)
            p = Paths(repo=data, data=data, veta_bin=data / "no-veta")
            for command in (oracles.tlp, oracles.optimizer_oracle):
                with self.assertRaises(Infra):
                    command(args, p, Report("oracle", data / "runs"))

    def test_the_oracle_needs_both_benchmarks_loaded(self):
        args = argparse.Namespace(queries=1, seed=1)
        with scratch_dir() as d:
            data = Path(d)
            binary = data / "veta"
            binary.write_text("")
            binary.chmod(0o755)
            p = Paths(repo=data, data=data, veta_bin=binary)
            with self.assertRaises(Infra):
                oracles.optimizer_oracle(args, p, Report("optimizer-oracle", data / "runs"))

    def test_the_bench_count_is_the_contract_one(self):
        self.assertEqual(oracles.BENCH_QUERIES, 65)
        self.assertEqual(oracles.MIN_RANDOM_QUERIES + oracles.BENCH_QUERIES, 1065)


if __name__ == "__main__":
    unittest.main()
