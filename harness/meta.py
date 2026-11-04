"""`size` and `deps` (contract §17): the harness is smaller than the engine, and neither
depends on anything outside the workspace and the standard library."""

import ast
import sys
import tomllib
from pathlib import Path

from harness.config import Paths
from harness.report import FAIL, PASS, Report

DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def nonblank_lines(path: Path) -> int:
    with open(path, encoding="utf-8", errors="surrogateescape") as f:
        return sum(1 for line in f if line.strip() != "")


def count_lines(root: Path, pattern: str) -> int:
    if not root.is_dir():
        return 0
    return sum(nonblank_lines(f) for f in sorted(root.rglob(pattern)) if f.is_file())


def line_counts(repo: Path) -> tuple[int, int]:
    """Non-empty lines of `crates/**/*.rs` and of `harness/**/*.py`."""
    return count_lines(repo / "crates", "*.rs"), count_lines(repo / "harness", "*.py")


def size(args, p: Paths, report: Report):
    engine, harness = line_counts(p.repo)
    print(f"engine_lines={engine} harness_lines={harness}", flush=True)
    report.fields.update(engine_lines=engine, harness_lines=harness)
    return (PASS if harness < engine else FAIL), f"engine_lines={engine} harness_lines={harness}"


# deps


def is_workspace_name(name: str) -> bool:
    return name.startswith("veta-")


def lock_violations(lock: Path) -> list[str]:
    """`Cargo.lock` may hold only `veta-*` packages, none with a `source`."""
    if not lock.is_file():
        return [f"{lock.name} missing"]
    try:
        data = tomllib.loads(lock.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as e:
        return [f"{lock.name}: {e}"]
    out = []
    for pkg in data.get("package", []):
        name = pkg.get("name", "?")
        if not is_workspace_name(name):
            out.append(f"{lock.name}: package {name} is not a veta-* crate")
        if "source" in pkg:
            out.append(f"{lock.name}: package {name} has source {pkg['source']}")
    return out


def dependency_tables(manifest: dict):
    """Every `(table name, {dep: spec})` of a manifest, including `[target.*]` and workspace ones."""
    for key in DEP_TABLES:
        if isinstance(manifest.get(key), dict):
            yield key, manifest[key]
    for target, spec in manifest.get("target", {}).items():
        for key in DEP_TABLES:
            if isinstance(spec.get(key), dict):
                yield f"target.{target}.{key}", spec[key]
    workspace = manifest.get("workspace", {})
    if isinstance(workspace.get("dependencies"), dict):
        yield "workspace.dependencies", workspace["dependencies"]


def dependency_allowed(name: str, spec) -> bool:
    """Only workspace crates by path (or inherited with `workspace = true`)."""
    if not isinstance(spec, dict):
        return False
    if not is_workspace_name(spec.get("package", name)):
        return False
    if "git" in spec or "registry" in spec:
        return False
    return "path" in spec or spec.get("workspace") is True


def manifest_violations(repo: Path) -> list[str]:
    manifests = [repo / "Cargo.toml", *sorted((repo / "crates").glob("*/Cargo.toml"))]
    out = []
    for path in manifests:
        rel = path.relative_to(repo)
        if not path.is_file():
            out.append(f"{rel} missing")
            continue
        try:
            manifest = tomllib.loads(path.read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError as e:
            out.append(f"{rel}: {e}")
            continue
        for table, deps in dependency_tables(manifest):
            for name, spec in deps.items():
                if not dependency_allowed(name, spec):
                    out.append(f"{rel}: [{table}] {name} is not a workspace crate")
    return out


def imported_modules(source: str) -> set[str]:
    """Top-level names of every absolute import in `source`."""
    names = set()
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, ast.Import):
            names.update(alias.name.split(".")[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
            names.add(node.module.split(".")[0])
    return names


def import_violations(harness_dir: Path) -> list[str]:
    """The harness imports only the standard library and its own package."""
    allowed = set(sys.stdlib_module_names) | {"harness"}
    out = []
    for path in sorted(harness_dir.rglob("*.py")):
        rel = path.relative_to(harness_dir.parent)
        try:
            modules = imported_modules(path.read_text(encoding="utf-8"))
        except (SyntaxError, UnicodeDecodeError) as e:
            out.append(f"{rel}: cannot parse: {e}")
            continue
        out.extend(f"{rel}: imports {m}" for m in sorted(modules - allowed))
    return out


def deps(args, p: Paths, report: Report):
    checks = (
        ("cargo-lock", lock_violations(p.repo / "Cargo.lock")),
        ("manifests", manifest_violations(p.repo)),
        ("harness-imports", import_violations(p.repo / "harness")),
    )
    total = 0
    for name, violations in checks:
        total += len(violations)
        for v in violations:
            report.progress(v)
        report.add({"check": name, "violations": violations, "verdict": "ok" if not violations else "fail"})
    return (PASS if total == 0 else FAIL), f"{total} violations"
