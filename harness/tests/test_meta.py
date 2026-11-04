import contextlib
import io
import unittest
from pathlib import Path

from harness import meta
from harness.config import Paths
from harness.report import FAIL, PASS, Report
from harness.tests.support import scratch_dir

LOCK_OK = """version = 4

[[package]]
name = "veta-cli"
version = "0.1.0"
dependencies = ["veta-core"]

[[package]]
name = "veta-core"
version = "0.1.0"
"""

WORKSPACE = """[workspace]
members = ["crates/*"]
resolver = "3"
"""

CRATE_OK = """[package]
name = "veta-cli"

[dependencies]
veta-core = { path = "../veta-core" }

[dev-dependencies]
veta-sql = { workspace = true }
"""


def write(root: Path, rel: str, text: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def fake_paths(root: Path) -> Paths:
    return Paths(repo=root, data=root / ".veta-data", veta_bin=root / "veta")


def run(func, root: Path):
    report = Report(func.__name__, root / "runs")
    with contextlib.redirect_stdout(io.StringIO()) as out:
        status, summary = func(None, fake_paths(root), report)
    return status, summary, report, out.getvalue()


class SizeTest(unittest.TestCase):
    def test_counts_non_empty_lines(self):
        with scratch_dir() as d:
            root = Path(d)
            write(root, "crates/veta-core/src/lib.rs", "fn a() {}\n\n   \nfn b() {}\n")
            write(root, "crates/veta-core/tests/t.rs", "fn c() {}\n")
            write(root, "crates/veta-core/README.md", "not counted\n")
            write(root, "harness/x.py", "import os\n\n")
            write(root, "harness/tests/test_x.py", "pass\n")
            self.assertEqual(meta.line_counts(root), (3, 2))
            status, summary, report, out = run(meta.size, root)
            self.assertEqual(status, PASS)
            self.assertEqual(summary, "engine_lines=3 harness_lines=2")
            self.assertIn("engine_lines=3 harness_lines=2", out)
            self.assertEqual(report.fields, {"engine_lines": 3, "harness_lines": 2})

    def test_fails_when_harness_is_not_smaller(self):
        with scratch_dir() as d:
            root = Path(d)
            write(root, "crates/veta-core/src/lib.rs", "fn a() {}\n")
            write(root, "harness/x.py", "a = 1\n")
            self.assertEqual(run(meta.size, root)[0], FAIL)


class DepsTest(unittest.TestCase):
    def tree(self, root: Path, lock=LOCK_OK, crate=CRATE_OK, module="import json\nfrom harness import report\n"):
        write(root, "Cargo.toml", WORKSPACE)
        write(root, "Cargo.lock", lock)
        write(root, "crates/veta-cli/Cargo.toml", crate)
        write(root, "harness/__init__.py", "")
        write(root, "harness/x.py", module)

    def test_clean_tree_passes(self):
        with scratch_dir() as d:
            root = Path(d)
            self.tree(root, module="import json, os.path\nfrom . import y\nfrom harness.report import PASS\n")
            status, summary, report, _ = run(meta.deps, root)
            self.assertEqual((status, summary), (PASS, "0 violations"))
            self.assertEqual([i["check"] for i in report.items], ["cargo-lock", "manifests", "harness-imports"])

    def test_external_lock_package(self):
        with scratch_dir() as d:
            root = Path(d)
            lock = LOCK_OK + '\n[[package]]\nname = "libc"\nversion = "0.2.0"\n' \
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
            self.tree(root, lock=lock)
            violations = meta.lock_violations(root / "Cargo.lock")
            self.assertEqual(len(violations), 2)
            self.assertEqual(run(meta.deps, root)[0], FAIL)

    def test_veta_named_package_with_source(self):
        with scratch_dir() as d:
            root = Path(d)
            self.tree(root, lock=LOCK_OK + '\n[[package]]\nname = "veta-x"\nsource = "git+https://x"\n')
            self.assertEqual(len(meta.lock_violations(root / "Cargo.lock")), 1)

    def test_missing_lock(self):
        with scratch_dir() as d:
            root = Path(d)
            self.tree(root)
            (root / "Cargo.lock").unlink()
            self.assertEqual(meta.lock_violations(root / "Cargo.lock"), ["Cargo.lock missing"])

    def test_external_manifest_dependencies(self):
        for crate in (
            CRATE_OK + 'serde = "1"\n',
            CRATE_OK + '\n[build-dependencies]\ncc = { version = "1" }\n',
            CRATE_OK + '\n[target.\'cfg(unix)\'.dependencies]\nlibc = "0.2"\n',
            CRATE_OK.replace('{ path = "../veta-core" }', '{ git = "https://x", path = "../veta-core" }'),
            CRATE_OK.replace('veta-core = { path = "../veta-core" }', 'veta-core = "0.1"'),
        ):
            with scratch_dir() as d:
                root = Path(d)
                self.tree(root, crate=crate)
                self.assertEqual(len(meta.manifest_violations(root)), 1, crate)

    def test_workspace_dependencies(self):
        with scratch_dir() as d:
            root = Path(d)
            self.tree(root)
            write(root, "Cargo.toml", WORKSPACE + '\n[workspace.dependencies]\nanyhow = "1"\n')
            self.assertEqual(meta.manifest_violations(root), ["Cargo.toml: [workspace.dependencies] anyhow is not a workspace crate"])

    def test_harness_imports(self):
        with scratch_dir() as d:
            root = Path(d)
            self.tree(root, module="import json\nimport numpy as np\nfrom requests.adapters import X\n"
                                   "def f():\n    import yaml\n")
            violations = meta.import_violations(root / "harness")
            self.assertEqual(violations, ["harness/x.py: imports numpy", "harness/x.py: imports requests",
                                          "harness/x.py: imports yaml"])

    def test_unparsable_module(self):
        with scratch_dir() as d:
            root = Path(d)
            self.tree(root, module="def (:\n")
            self.assertEqual(len(meta.import_violations(root / "harness")), 1)


if __name__ == "__main__":
    unittest.main()
