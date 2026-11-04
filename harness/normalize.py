"""Cell normalization and numeric equality (contract §15.1 and the tolerance of §16).

A normalized cell is `None` (NULL), an `int`, a `float` or a `str` (bytes that are not
UTF-8 survive as surrogates, so equal bytes are equal strings). Booleans become 0/1 and a
timestamp loses an all-zero fraction, so the three engines print the same values alike.
"""

import csv
import io
import math
import re

from harness.engines import split_rows

REL_TOL = 1e-9
ABS_TOL = 1e-12

INT_RE = re.compile(r"-?\d+")
FLOAT_RE = re.compile(r"[-+]?(?:(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?|inf|infinity|nan)", re.IGNORECASE)
TIMESTAMP_RE = re.compile(r"(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})(?:\.(\d*))?")

# §13.1 plus `\'`, which ClickHouse also writes.
TSV_ESCAPES = {"\\": "\\", "t": "\t", "n": "\n", "r": "\r", "0": "\0", "b": "\b", "f": "\f", "'": "'"}


def unescape_tsv(cell: str) -> str:
    """Undoes the TSV escapes; an unknown escape keeps its backslash."""
    if "\\" not in cell:
        return cell
    out = []
    i = 0
    while i < len(cell):
        c = cell[i]
        if c == "\\" and i + 1 < len(cell) and cell[i + 1] in TSV_ESCAPES:
            out.append(TSV_ESCAPES[cell[i + 1]])
            i += 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


def scalar(text: str):
    """NULL, boolean, number or timestamp from an unescaped cell; `None` if none applies.

    Returns a 1-tuple so that a NULL result is told apart from "not a scalar".
    """
    if text == "\\N":
        return (None,)
    if text == "true":
        return (1,)
    if text == "false":
        return (0,)
    if INT_RE.fullmatch(text):
        return (int(text),)
    if FLOAT_RE.fullmatch(text):
        return (float(text),)
    m = TIMESTAMP_RE.fullmatch(text)
    if m:
        fraction = m.group(2)
        if fraction is None or fraction.strip("0") == "":
            return (m.group(1),)
        return (text,)
    return None


def normalize_tsv_cell(cell: str):
    s = scalar(cell)
    return s[0] if s is not None else unescape_tsv(cell)


def normalize_csv_cell(cell: str):
    s = scalar(cell)
    return s[0] if s is not None else cell


def parse_tsv(text: str) -> list[tuple]:
    """veta and ClickHouse `TSV` output as normalized rows."""
    return [tuple(normalize_tsv_cell(c) for c in row.split("\t")) for row in split_rows(text)]


def parse_csv(text: str) -> list[tuple]:
    """DuckDB `-csv -noheader` output as normalized rows (quoted cells may hold `\\n`)."""
    reader = csv.reader(io.StringIO(text, newline=""), strict=False)
    return [tuple(normalize_csv_cell(c) for c in row) for row in reader]


def parse_output(engine: str, text: str) -> list[tuple]:
    return parse_csv(text) if engine == "duckdb" else parse_tsv(text)


def is_number(v) -> bool:
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def numbers_equal(a, b) -> bool:
    """Equal if both are identical integers, or within relative 1e-9 plus absolute 1e-12."""
    if isinstance(a, int) and isinstance(b, int):
        return a == b
    try:
        fa, fb = float(a), float(b)
    except OverflowError:
        return a == b
    if math.isnan(fa) or math.isnan(fb):
        return math.isnan(fa) and math.isnan(fb)
    if fa == fb:
        return True
    if math.isinf(fa) or math.isinf(fb):
        return False
    return abs(fa - fb) <= REL_TOL * max(abs(fa), abs(fb)) + ABS_TOL


def cells_equal(a, b) -> bool:
    if a is None or b is None:
        return a is None and b is None
    if is_number(a) and is_number(b):
        return numbers_equal(a, b)
    if is_number(a) or is_number(b):
        return False
    return a == b


def rows_equal(a: tuple, b: tuple) -> bool:
    return len(a) == len(b) and all(cells_equal(x, y) for x, y in zip(a, b))


def cell_sort_key(v):
    """A total order that keeps numbers within the tolerance next to each other."""
    if v is None:
        return (0, 0)
    if is_number(v):
        if isinstance(v, float) and math.isnan(v):
            return (2, 0)
        return (1, v)
    return (3, v)


def row_sort_key(row: tuple):
    return tuple(cell_sort_key(v) for v in row)


def render(v) -> str:
    """A normalized cell for a human-readable reason."""
    if v is None:
        return "NULL"
    text = repr(v)
    return text if len(text) <= 80 else text[:77] + "..."
