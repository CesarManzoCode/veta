import contextlib
import io
import tomllib
import unittest
from pathlib import Path

from harness import mutants
from harness.__main__ import build_parser, report_name
from harness.config import REPO, Paths
from harness.report import FAIL, PASS, SKIP, Report
from harness.tests.support import scratch_dir

CONTRACT_MUTANTS = {
    "mutant-prune-extra-granule": "veta-storage",
    "mutant-prune-wrong-bound": "veta-storage",
    "mutant-pushdown-outer": "veta-sql",
    "mutant-reorder-outer": "veta-sql",
    "mutant-estimate-ignore-null": "veta-sql",
    "mutant-codec-drop-last-block": "veta-codec",
}


def manifest(crate: str) -> dict:
    return tomllib.loads((REPO / "crates" / crate / "Cargo.toml").read_text(encoding="utf-8"))


def quiet(func, *args):
    with contextlib.redirect_stdout(io.StringIO()) as out:
        result = func(*args)
    return result, out.getvalue()


class FeaturesTest(unittest.TestCase):
    def test_catalog_matches_the_contract(self):
        self.assertEqual({mutants.feature(m): c for m, c in mutants.MUTANTS}, CONTRACT_MUTANTS)

    def test_each_feature_lives_in_its_crate_and_is_off_by_default(self):
        for feature, crate in CONTRACT_MUTANTS.items():
            features = manifest(crate).get("features", {})
            self.assertEqual(features.get(feature), [], f"{crate}: {feature}")
            self.assertNotIn(feature, features.get("default", []))

    def test_veta_cli_reexports_each_feature(self):
        features = manifest("veta-cli").get("features", {})
        for feature, crate in CONTRACT_MUTANTS.items():
            self.assertEqual(features.get(feature), [f"{crate}/{feature}"], feature)
        self.assertNotIn("default", features)

    def test_each_feature_is_planted_in_the_source_of_its_crate(self):
        for feature, crate in CONTRACT_MUTANTS.items():
            sources = (REPO / "crates" / crate / "src").rglob("*.rs")
            text = "".join(f.read_text(encoding="utf-8") for f in sources)
            self.assertIn(f'#[cfg(feature = "{feature}")]', text, feature)


class ParseTest(unittest.TestCase):
    def test_last_result_line(self):
        out = "x: progress\nRESULT tlp FAIL 3 discrepancies\nRESULT tlp PASS 0 discrepancies\n"
        self.assertEqual(mutants.parse_result(out), ("tlp", "PASS"))
        self.assertEqual(mutants.parse_result("RESULT size SKIP\n"), ("size", "SKIP"))
        self.assertIsNone(mutants.parse_result("tlp| RESULT tlp PASS ok\nnothing\n"))

    def test_outcome_falls_back_to_the_exit_code(self):
        self.assertEqual(mutants.outcome("rss", 77, "crash\n"),
                         {"command": "rss", "status": SKIP, "exit_code": 77})
        self.assertEqual(mutants.outcome("rss", -9, ""),
                         {"command": "rss", "status": FAIL, "exit_code": -9})
        self.assertEqual(mutants.outcome("x", 2, "RESULT tpch-verify-sf1 FAIL no data\n"),
                         {"command": "tpch-verify-sf1", "status": FAIL, "exit_code": 2})


class MutantsTest(unittest.TestCase):
    def run_simulated(self, exit_codes):
        """`exit_codes[(mutant, command)]`, 0 when absent."""
        with scratch_dir() as d:
            report = Report("mutants", Path(d) / "runs")
            built = []

            def build(m):
                built.append(m)
                return Path(d) / m / "veta"

            def run_command(m, binary, name, argv):
                self.assertEqual(binary, Path(d) / m / "veta")
                return exit_codes.get((m, name), 0)

            infra, out = quiet(mutants.run_mutants, build, run_command, report)
            self.assertEqual(built, [m for m, _ in mutants.MUTANTS])
            return report.items, infra, out

    def test_all_killed_passes(self):
        codes = {(m, "tlp"): 1 for m, _ in mutants.MUTANTS}
        codes[("codec-drop-last-block", "codec-roundtrip")] = 1
        items, infra, out = self.run_simulated(codes)
        self.assertEqual(infra, [])
        self.assertEqual(mutants.mutant_verdict(items), (PASS, "6/6 killed"))
        self.assertIn("MUTANT mutant-codec-drop-last-block KILLED by codec-roundtrip,tlp", out)
        self.assertEqual(items[0], {"mutant": "mutant-prune-extra-granule", "killed": True, "by": ["tlp"]})

    def test_one_survivor_fails(self):
        codes = {(m, "pruning"): 1 for m, _ in mutants.MUTANTS if m != "reorder-outer"}
        # SKIP and PASS do not kill.
        codes[("reorder-outer", "cardinality")] = 77
        items, infra, out = self.run_simulated(codes)
        status, summary = mutants.mutant_verdict(items)
        self.assertEqual(status, FAIL)
        self.assertEqual(summary, "5/6 killed; survived: mutant-reorder-outer")
        self.assertIn("MUTANT mutant-reorder-outer SURVIVED", out)
        survivor = [i for i in items if i["mutant"] == "mutant-reorder-outer"][0]
        self.assertEqual(survivor, {"mutant": "mutant-reorder-outer", "killed": False, "by": []})

    def test_exit_2_is_reported_as_infrastructure(self):
        codes = {(m, "tlp"): 1 for m, _ in mutants.MUTANTS}
        codes[("pushdown-outer", "pruning")] = 2
        items, infra, _ = self.run_simulated(codes)
        self.assertEqual(infra, ["pushdown-outer:pruning"])
        self.assertTrue(all(i["killed"] for i in items))

    def test_commands_follow_the_contract_order_and_parse(self):
        names = [n for n, _ in mutants.MUTANT_COMMANDS]
        self.assertEqual(names, ["codec-roundtrip", "pruning", "clickbench-verify-duckdb",
                                 "tpch-verify-sf1", "tlp", "optimizer-oracle", "cardinality"])
        parser = build_parser()
        for name, argv in mutants.MUTANT_COMMANDS:
            self.assertEqual(report_name(parser.parse_args(argv)), name)

    def test_skip_without_duckdb(self):
        with scratch_dir() as d:
            root = Path(d)
            p = Paths(repo=root, data=root / "data", veta_bin=root / "veta")
            report = Report("mutants", p.runs)
            with self.assertRaises(mutants.Skip):
                mutants.mutants(None, p, report)

    def test_runs_keeper_restores_the_previous_reports(self):
        with scratch_dir() as d:
            root = Path(d)
            p = Paths(repo=root, data=root / "data", veta_bin=root / "veta")
            (p.runs / "tlp").mkdir(parents=True)
            (p.runs / "tlp" / "report.json").write_text("baseline")
            report = Report("mutants", p.runs)
            with mutants.RunsKeeper(p, report, ["tlp", "pruning"]) as keeper:
                self.assertFalse((p.runs / "tlp").exists())
                (p.runs / "tlp").mkdir()
                (p.runs / "tlp" / "report.json").write_text("mutated")
                keeper.file("pushdown-outer", "tlp")
                (p.runs / "pruning").mkdir()
            self.assertEqual((p.runs / "tlp" / "report.json").read_text(), "baseline")
            self.assertEqual((report.dir / "pushdown-outer" / "tlp" / "report.json").read_text(), "mutated")
            self.assertFalse((p.runs / "pruning").exists())


class AllTest(unittest.TestCase):
    def run_simulated(self, results):
        """`results[name] = (exit code, output)`; PASS with exit 0 when absent."""
        with scratch_dir() as d:
            report = Report("all", Path(d) / "runs")
            seen = []

            def run_command(name, argv):
                seen.append(name)
                return results.get(name, (0, f"progress\nRESULT {name} PASS ok\n"))

            quiet(mutants.run_all, run_command, report)
            self.assertEqual(seen, [n for n, _ in mutants.ALL_COMMANDS])
            return report.items

    def test_every_pass_passes(self):
        items = self.run_simulated({})
        self.assertEqual(mutants.all_verdict(items), (PASS, "30/30 PASS or SKIP (0 SKIP)"))
        self.assertEqual(items[0], {"command": "clickbench-verify-duckdb", "status": PASS, "exit_code": 0})

    def test_a_skip_passes(self):
        items = self.run_simulated({"perf-tpch": (77, "RESULT perf-tpch SKIP clickhouse missing\n")})
        self.assertEqual(mutants.all_verdict(items), (PASS, "30/30 PASS or SKIP (1 SKIP)"))

    def test_a_fail_fails(self):
        items = self.run_simulated({"tlp": (1, "RESULT tlp FAIL 2 discrepancies\n")})
        status, summary = mutants.all_verdict(items)
        self.assertEqual(status, FAIL)
        self.assertEqual(summary, "29/30 PASS or SKIP (0 SKIP); failed: tlp")
        self.assertIn({"command": "tlp", "status": FAIL, "exit_code": 1}, items)

    def test_an_infrastructure_error_fails(self):
        items = self.run_simulated({"size": (2, "RESULT size FAIL internal error\n")})
        self.assertEqual(mutants.all_verdict(items)[0], FAIL)

    def test_commands_cover_section_17_except_all(self):
        names = [n for n, _ in mutants.ALL_COMMANDS]
        self.assertEqual(len(names), len(set(names)))
        self.assertNotIn("all", names)
        self.assertEqual(names[-3:], ["mutants", "size", "deps"])
        parser = build_parser()
        for name, argv in mutants.ALL_COMMANDS:
            self.assertEqual(report_name(parser.parse_args(argv)), name)


if __name__ == "__main__":
    unittest.main()
