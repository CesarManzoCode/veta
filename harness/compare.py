"""Result comparison: the four rules of contract §15.2 and `--tamper` of §15.3.

The rule comes from the engine's query text (top-level `ORDER BY` and `LIMIT/OFFSET`).
Rows are normalized tuples (`normalize.py`); "engine" is the side under test (veta) and
"reference" the oracle. Every comparison returns a verdict, its rule and a reason.
"""

import math
from collections.abc import Callable
from dataclasses import dataclass

from harness import sqltext
from harness.engines import EngineRun
from harness.normalize import parse_output, render, row_sort_key, rows_equal

EQUAL, DIFFERENT, ERROR = "equal", "different", "error"
MULTISET, ORDERED, LIMIT_BORDER, LIMIT_UNORDERED = "multiset", "ordered", "limit-border", "limit-unordered"

# Above this many rows a multiset mismatch after sorting is reported without the
# quadratic search for a row equal within the tolerance.
GREEDY_MATCH_LIMIT = 20_000


class TamperError(ValueError):
    pass


@dataclass(frozen=True)
class Outcome:
    verdict: str
    rule: str
    reason: str


def rule_for(q: sqltext.Query) -> str:
    if q.has_order_by:
        return LIMIT_BORDER if q.has_limit else ORDERED
    return LIMIT_UNORDERED if q.has_limit else MULTISET


def tamper_value(v):
    """§15.3: integers +1, floats ×1.5 + 1, strings + "x", NULL → 0."""
    if v is None:
        return 0
    if isinstance(v, int):
        return v + 1
    if isinstance(v, float):
        return v * 1.5 + 1 if not math.isnan(v) else 0.0
    return v + "x"


def tamper(rows: list[tuple]) -> list[tuple]:
    """A copy of `rows` with the first cell of the first row changed."""
    if not rows or not rows[0]:
        raise TamperError("the reference result has no cell to tamper with")
    first = rows[0]
    return [(tamper_value(first[0]), *first[1:]), *rows[1:]]


def multiset_diff(engine: list[tuple], ref: list[tuple]) -> str | None:
    """`None` if both hold the same rows (numbers within the tolerance); else why not."""
    if len(engine) != len(ref):
        return f"row count {len(engine)} != reference {len(ref)}"
    a = sorted(engine, key=row_sort_key)
    b = sorted(ref, key=row_sort_key)
    first_bad = next((i for i, (x, y) in enumerate(zip(a, b)) if not rows_equal(x, y)), None)
    if first_bad is None:
        return None
    if len(a) > GREEDY_MATCH_LIMIT:
        return f"row {render(a[first_bad])} != reference {render(b[first_bad])} (sorted position {first_bad + 1})"
    left = b[first_bad:]
    for row in a[first_bad:]:
        j = next((j for j, other in enumerate(left) if rows_equal(row, other)), None)
        if j is None:
            return f"row {render(row)} is not in the reference"
        left.pop(j)
    return None


def sort_keys(rows: list[tuple], columns: tuple[int, ...]) -> list[tuple]:
    return [tuple(row[c] for c in columns) for row in rows]


def key_groups(keys: list[tuple]) -> list[tuple[int, int]]:
    """`[start, end)` runs of consecutive rows with equal keys."""
    groups = []
    start = 0
    for i in range(1, len(keys) + 1):
        if i == len(keys) or not rows_equal(keys[i], keys[start]):
            groups.append((start, i))
            start = i
    return groups


def ordered_diff(engine: list[tuple], ref: list[tuple], columns: tuple[int, ...],
                 relax_first: bool = False, relax_last: bool = False) -> str | None:
    """Identical key sequences and equal multisets within each key group.

    `relax_first`/`relax_last` (limit-border) let the first/last group of the window
    match by keys only: ties cut by the window may be broken either way.
    """
    if len(engine) != len(ref):
        return f"row count {len(engine)} != reference {len(ref)}"
    try:
        ke, kr = sort_keys(engine, columns), sort_keys(ref, columns)
    except IndexError:
        return f"a row lacks the ORDER BY columns {list(columns)}"
    for i, (x, y) in enumerate(zip(ke, kr)):
        if not rows_equal(x, y):
            return f"order key at row {i + 1}: {render(x)} != reference {render(y)}"
    groups = key_groups(kr)
    for n, (s, e) in enumerate(groups):
        if (relax_first and n == 0) or (relax_last and n == len(groups) - 1):
            continue
        diff = multiset_diff(engine[s:e], ref[s:e])
        if diff is not None:
            return f"rows {s + 1}..{e} with key {render(kr[s])}: {diff}"
    return None


def unordered_limit_diff(engine: list[tuple], ref_count: int, matches: list[tuple]) -> str | None:
    """Same row count, and every engine row is in the reference result without LIMIT."""
    if len(engine) != ref_count:
        return f"row count {len(engine)} != reference {ref_count}"
    for row in engine:
        if not any(rows_equal(row, m) for m in matches):
            return f"row {render(row)} is not in the reference result without LIMIT"
    return None


def sql_literal(v, dialect: str) -> str:
    """`v` as a literal; ClickHouse strings escape with backslashes, others double quotes."""
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        if math.isnan(v) or math.isinf(v):
            text = "nan" if math.isnan(v) else ("inf" if v > 0 else "-inf")
            return text if dialect == "clickhouse" else f"CAST('{text}' AS DOUBLE)"
        return repr(v)
    if dialect == "clickhouse":
        escaped = v.replace("\\", "\\\\").replace("'", "\\'").replace("\0", "\\0")
        return f"'{escaped}'"
    # A NUL byte cannot travel in argv: it is spliced in with chr(0).
    parts = ["'" + piece.replace("'", "''") + "'" for piece in v.split("\0")]
    return " || chr(0) || ".join(parts) if len(parts) > 1 else parts[0]


def row_condition(row: tuple, dialect: str) -> str:
    terms = []
    for i, v in enumerate(row, 1):
        col = f"c{i}"
        if v is None:
            terms.append(f"{col} IS NULL")
        elif isinstance(v, float) and math.isfinite(v):
            # A loose prefilter; the harness decides with the exact tolerance afterwards.
            terms.append(f"abs({col} - ({repr(v)})) <= {repr(2e-9 * abs(v) + 1e-12)}")
        else:
            terms.append(f"{col} = {sql_literal(v, dialect)}")
    return "(" + " AND ".join(terms) + ")"


def match_sql(ref_query: sqltext.Query, rows: list[tuple], dialect: str) -> str:
    """`SELECT c1, … FROM (<query without LIMIT, aliased c1…cn>) AS t WHERE (c1 = v1 AND …) OR …`."""
    inner = sqltext.without_limit_aliased(ref_query)
    width = len(ref_query.items)
    columns = ", ".join(f"c{i}" for i in range(1, width + 1))
    conditions = list(dict.fromkeys(row_condition(r, dialect) for r in rows))
    return f"SELECT {columns} FROM ({inner}) AS t WHERE {' OR '.join(conditions)}"


@dataclass
class Side:
    """One side of a comparison: how to run SQL and where its raw output goes.

    `engine` names `raw/<engine>/`; `suffix` tells apart two sides of the same engine
    (`pruning` runs veta against veta); `dialect` picks the literal syntax.
    """

    engine: str
    run: Callable[[str], EngineRun]
    dialect: str
    suffix: str = ""


@dataclass
class Comparison:
    query: str
    rule: str
    verdict: str
    reason: str
    rows_engine: int | None = None
    rows_ref: int | None = None
    engine_run: EngineRun | None = None
    ref_run: EngineRun | None = None


class QueryFailed(Exception):
    pass


def _execute(side: Side, sql: str, item: str, variant: str, raw) -> tuple[EngineRun, list[tuple]]:
    run = side.run(sql)
    if raw is not None:
        raw(side.engine, f"{item}{side.suffix}{variant}", run.stdout)
    if not run.ok:
        raise QueryFailed(f"{side.engine}{side.suffix} {variant.lstrip('.') or 'query'} exited {run.code}: "
                          f"{run.error_tail()}")
    return run, parse_output(side.engine, run.text)


def verify_query(item: str, engine: Side, ref: Side, engine_sql: str, ref_sql: str,
                 tampered: bool = False, raw=None) -> Comparison:
    """Runs one query on both sides and applies its §15.2 rule.

    `raw(engine, name, stdout)` stores each run: `<item>` for the query as written,
    `<item>.augmented` for the augmented variant and `<item>.match` for the LIMIT check.
    """
    try:
        q = sqltext.parse(engine_sql)
        qr = sqltext.parse(ref_sql)
    except sqltext.SqlTextError as e:
        return Comparison(item, MULTISET, ERROR, f"query text: {e}")
    rule = rule_for(q)
    c = Comparison(item, rule, ERROR, "")
    try:
        if rule_for(qr) != rule:
            raise QueryFailed(f"reference text has rule {rule_for(qr)}")
        c.engine_run, engine_rows = _execute(engine, engine_sql, item, "", raw)
        c.ref_run, ref_rows = _execute(ref, ref_sql, item, "", raw)
        c.rows_engine, c.rows_ref = len(engine_rows), len(ref_rows)
        match rule:
            case "multiset":
                diff = multiset_diff(engine_rows, tamper(ref_rows) if tampered else ref_rows)
            case "ordered" | "limit-border":
                diff = _ordered(c, q, qr, engine, ref, engine_rows, ref_rows, tampered, raw)
            case _:
                diff = _unordered_limit(c, qr, ref, engine_rows, ref_rows, tampered, raw)
    except (QueryFailed, TamperError, sqltext.SqlTextError) as e:
        c.verdict, c.reason = ERROR, str(e)
        return c
    c.verdict, c.reason = (EQUAL, "") if diff is None else (DIFFERENT, diff)
    if tampered:
        c.reason = (c.reason + " (reference tampered)").strip()
    return c


def _ordered(c, q, qr, engine, ref, engine_rows, ref_rows, tampered, raw) -> str | None:
    plan, plan_ref = sqltext.order_plan(q), sqltext.order_plan(qr)
    if (plan.augmented, plan.key_columns) != (plan_ref.augmented, plan_ref.key_columns):
        raise QueryFailed("engine and reference texts resolve ORDER BY to different columns")
    if plan.augmented:
        if len(engine_rows) != len(ref_rows):
            return f"original query row count {len(engine_rows)} != reference {len(ref_rows)}"
        _, engine_rows_aug = _execute(engine, plan.sql, c.query, ".augmented", raw)
        _, ref_rows_aug = _execute(ref, plan_ref.sql, c.query, ".augmented", raw)
        if len(engine_rows_aug) != len(engine_rows):
            return f"augmented query row count {len(engine_rows_aug)} != original {len(engine_rows)}"
        engine_rows, ref_rows = engine_rows_aug, ref_rows_aug
    if tampered:
        ref_rows = tamper(ref_rows)
    relax_first, relax_last = border_groups(q, c.rule, len(ref_rows))
    return ordered_diff(engine_rows, ref_rows, plan.key_columns, relax_first, relax_last)


def border_groups(q: sqltext.Query, rule: str, ref_count: int) -> tuple[bool, bool]:
    """Which edge groups of a limit-border window match by keys only.

    Only a group the window can cut has ambiguous ties: the first one when there is an
    OFFSET, the last one when the window is full. A group that is not cut is complete in
    both engines, so its multiset must match; this also keeps `--tamper` of a non-key first
    cell visible.
    """
    if rule != LIMIT_BORDER:
        return False, False
    relax_first = (q.offset or 0) > 0
    relax_last = q.limit is not None and ref_count >= q.limit
    return relax_first, relax_last


def _unordered_limit(c, qr, ref, engine_rows, ref_rows, tampered, raw) -> str | None:
    if len(engine_rows) != len(ref_rows):
        return f"row count {len(engine_rows)} != reference {len(ref_rows)}"
    matches = []
    if engine_rows:
        widths = {len(r) for r in engine_rows}
        if widths != {len(qr.items)}:
            return f"row width {sorted(widths)} != {len(qr.items)} select items"
        _, matches = _execute(ref, match_sql(qr, engine_rows, ref.dialect), c.query, ".match", raw)
    if tampered:
        matches = tamper(matches)
    return unordered_limit_diff(engine_rows, len(ref_rows), matches)
