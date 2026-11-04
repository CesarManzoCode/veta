"""Shared test helpers: temporary directories under `$VETA_DATA/scratch/tests`, never `/tmp`."""

import tempfile

from harness.config import paths


def scratch_dir() -> tempfile.TemporaryDirectory:
    root = paths().scratch / "tests"
    root.mkdir(parents=True, exist_ok=True)
    return tempfile.TemporaryDirectory(dir=root)
