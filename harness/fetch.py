"""`fetch`: downloads and verifies every pin of appendix A (contract §14.3).

Downloads go to `<dest>.part` and are renamed only after their hash verifies, so a file at
its final path is always a verified one; anything that already verifies is not fetched
again. Archives live in `$VETA_DATA/scratch/fetch/` and only the pinned member is written.
"""

import hashlib
import os
import shutil
import tarfile
import time
import urllib.request
from pathlib import Path

from harness.config import ALL_PINS, Paths, Pin
from harness.engines import Engines
from harness.report import FAIL, PASS, Report

CHUNK = 1 << 20
ATTEMPTS = 3
TIMEOUT_S = 60


class FetchError(Exception):
    pass


def file_hash(path: Path, algorithm: str = "sha256") -> str:
    h = hashlib.new(algorithm)
    with open(path, "rb") as f:
        while chunk := f.read(CHUNK):
            h.update(chunk)
    return h.hexdigest()


def verifies(path: Path, expected: str, algorithm: str = "sha256") -> bool:
    return path.is_file() and file_hash(path, algorithm) == expected.lower()


def _copy_hashing(src, part: Path, algorithm: str) -> str:
    h = hashlib.new(algorithm)
    with open(part, "wb") as out:
        while chunk := src.read(CHUNK):
            h.update(chunk)
            out.write(chunk)
        out.flush()
        os.fsync(out.fileno())
    return h.hexdigest()


def download(url: str, dest: Path, expected: str, algorithm: str = "sha256", log=print) -> None:
    """Downloads `url` into `dest` through `dest.part`; renames only when the hash matches."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = dest.with_name(dest.name + ".part")
    last = None
    for attempt in range(1, ATTEMPTS + 1):
        try:
            with urllib.request.urlopen(url, timeout=TIMEOUT_S) as response:
                got = _copy_hashing(response, part, algorithm)
        except OSError as e:
            last = f"{url}: {e}"
            part.unlink(missing_ok=True)
            log(f"attempt {attempt}/{ATTEMPTS} failed: {last}")
            if attempt < ATTEMPTS:
                time.sleep(2 * attempt)
            continue
        if got != expected.lower():
            part.unlink(missing_ok=True)
            raise FetchError(f"{url}: {algorithm} {got} != pinned {expected}")
        os.replace(part, dest)
        return
    raise FetchError(last or url)


def extract_member(archive: Path, member: str, dest: Path, expected_sha256: str, executable: bool) -> None:
    """Writes only `member` of `archive` to `dest` (through `.part`) and checks its sha256."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = dest.with_name(dest.name + ".part")
    with tarfile.open(archive, "r:*") as tar:
        try:
            info = tar.getmember(member)
        except KeyError:
            raise FetchError(f"{archive.name}: member {member} not found") from None
        if not info.isfile():
            raise FetchError(f"{archive.name}: member {member} is not a regular file")
        src = tar.extractfile(info)
        got = _copy_hashing(src, part, "sha256")
    if got != expected_sha256.lower():
        part.unlink(missing_ok=True)
        raise FetchError(f"{member}: sha256 {got} != pinned {expected_sha256}")
    if executable:
        part.chmod(0o755)
    os.replace(part, dest)


def install_extension(pin: Pin, p: Paths) -> None:
    """`INSTALL <name>` into `$VETA_DATA/duckdb-ext` with the pinned DuckDB, then verify."""
    engines = Engines(p, timeout=600)
    if not engines.available("duckdb"):
        raise FetchError(f"INSTALL {pin.install} needs {p.duckdb}")
    dest = p.data / pin.dest
    dest.unlink(missing_ok=True)
    p.duckdb_ext.mkdir(parents=True, exist_ok=True)
    run = engines.duckdb(None, f"INSTALL {pin.install};")
    if not run.ok:
        raise FetchError(f"INSTALL {pin.install} exited {run.code}: {run.error_tail()}")
    if not dest.is_file():
        raise FetchError(f"INSTALL {pin.install} did not create {dest}")
    got = file_hash(dest)
    if got != pin.sha256:
        dest.unlink()
        raise FetchError(f"{pin.dest}: sha256 {got} != pinned {pin.sha256}")


def fetch_pin(pin: Pin, p: Paths, log=print) -> str:
    """Makes `pin` present and verified; returns `verified` (already there) or `fetched`."""
    dest = p.data / pin.dest
    if verifies(dest, pin.sha256):
        return "verified"
    if pin.install is not None:
        install_extension(pin, p)
    elif pin.member is not None:
        algorithm, expected = pin.archive_hash
        archive = p.scratch / "fetch" / Path(pin.url).name
        if not verifies(archive, expected, algorithm):
            log(f"downloading {pin.url}")
            download(pin.url, archive, expected, algorithm, log)
        extract_member(archive, pin.member, dest, pin.sha256, pin.executable)
        archive.unlink()
    else:
        log(f"downloading {pin.url}")
        download(pin.url, dest, pin.sha256, "sha256", log)
        if pin.executable:
            dest.chmod(0o755)
    return "fetched"


def run(args, p: Paths, report: Report):
    pins = ALL_PINS
    failed = 0
    for pin in pins:
        try:
            action = fetch_pin(pin, p, report.progress)
            report.progress(f"{pin.dest}: {action}")
            report.add({"file": str(p.data / pin.dest), "sha256": pin.sha256, "action": action, "verdict": "ok"})
        except (FetchError, OSError, tarfile.TarError) as e:
            failed += 1
            report.progress(f"{pin.dest}: FAILED {e}")
            report.add({"file": str(p.data / pin.dest), "sha256": pin.sha256, "action": "error",
                        "verdict": "error", "reason": str(e)})
    scratch = p.scratch / "fetch"
    if scratch.is_dir() and not any(scratch.iterdir()):
        shutil.rmtree(scratch)
    ok = len(pins) - failed
    return (PASS if failed == 0 else FAIL), f"{ok}/{len(pins)} verified"
