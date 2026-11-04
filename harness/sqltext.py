"""Top-level clauses of a query and the variants of contract §15.2.

The scanner only looks at depth 0, outside strings (`'…'` with `''` and `\\'`), quoted
identifiers (`"…"`, `` `…` ``) and comments, so a `LIMIT` or a comma inside a subquery or a
function call is never taken for the query's own.
"""

import re
from dataclasses import dataclass, field

WORD_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
NUMBER_RE = re.compile(r"\d+")
CLAUSES = ("SELECT", "FROM", "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET",
           "UNION", "INTERSECT", "EXCEPT", "SETTINGS", "FORMAT", "WINDOW", "QUALIFY")


class SqlTextError(ValueError):
    pass


@dataclass(frozen=True)
class Token:
    """A top-level word (upper-cased in `word`) or comma at `start` of the text."""

    word: str
    start: int
    end: int


def _skip_quoted(text: str, i: int, quote: str) -> int:
    """Index after the quoted span opening at `i`."""
    j = i + 1
    while j < len(text):
        c = text[j]
        if c == "\\" and quote == "'":
            j += 2
            continue
        if c == quote:
            if j + 1 < len(text) and text[j + 1] == quote:
                j += 2
                continue
            return j + 1
        j += 1
    raise SqlTextError(f"unterminated {quote} at offset {i}")


def top_level_tokens(text: str) -> list[Token]:
    """Words and commas at parenthesis depth 0."""
    tokens = []
    depth = 0
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c in "'\"`":
            i = _skip_quoted(text, i, c)
        elif text.startswith("--", i):
            nl = text.find("\n", i)
            i = n if nl < 0 else nl + 1
        elif text.startswith("/*", i):
            end = text.find("*/", i + 2)
            if end < 0:
                raise SqlTextError("unterminated comment")
            i = end + 2
        elif c == "(":
            depth += 1
            i += 1
        elif c == ")":
            depth -= 1
            if depth < 0:
                raise SqlTextError(f"unbalanced ) at offset {i}")
            i += 1
        elif c == ",":
            if depth == 0:
                tokens.append(Token(",", i, i + 1))
            i += 1
        elif (m := WORD_RE.match(text, i) or NUMBER_RE.match(text, i)) is not None:
            if depth == 0:
                tokens.append(Token(m.group(0).upper(), i, m.end()))
            i = m.end()
        else:
            i += 1
    if depth != 0:
        raise SqlTextError("unbalanced parentheses")
    return tokens


def split_commas(text: str) -> list[str]:
    """`text` cut at its top-level commas, each piece stripped."""
    pieces = []
    start = 0
    for tok in top_level_tokens(text):
        if tok.word == ",":
            pieces.append(text[start:tok.start].strip())
            start = tok.end
    pieces.append(text[start:].strip())
    return [p for p in pieces if p]


def collapse(text: str) -> str:
    """Whitespace-insensitive form used to compare expressions by their text."""
    return " ".join(text.split())


@dataclass(frozen=True)
class SelectItem:
    text: str
    expr: str
    alias: str | None


@dataclass(frozen=True)
class OrderKey:
    text: str
    expr: str
    descending: bool


@dataclass
class Query:
    """One SELECT split into its top-level parts; offsets index `text`."""

    text: str
    select_start: int
    select_end: int
    items: list[SelectItem]
    order_start: int | None = None
    order_by: list[OrderKey] = field(default_factory=list)
    limit: int | None = None
    offset: int | None = None
    limit_start: int | None = None

    @property
    def has_order_by(self) -> bool:
        return bool(self.order_by)

    @property
    def has_limit(self) -> bool:
        return self.limit is not None or self.offset is not None

    @property
    def body_end(self) -> int:
        """End of the query before `ORDER BY`, `LIMIT` and `OFFSET`."""
        cuts = [c for c in (self.order_start, self.limit_start) if c is not None]
        return min(cuts) if cuts else len(self.text)


def strip_statement(sql: str) -> str:
    text = sql.strip()
    while text.endswith(";"):
        text = text[:-1].rstrip()
    return text


def parse_select_item(text: str) -> SelectItem:
    words = [t for t in top_level_tokens(text) if t.word != ","]
    if len(words) >= 2 and words[-2].word == "AS" and words[-1].end == len(text):
        alias = text[words[-1].start:words[-1].end]
        return SelectItem(text, text[:words[-2].start].strip(), alias)
    quoted = re.search(r"\s+AS\s+(\"[^\"]+\"|`[^`]+`)$", text, re.IGNORECASE)
    if quoted:
        return SelectItem(text, text[:quoted.start()].strip(), quoted.group(1)[1:-1])
    return SelectItem(text, text, None)


def parse_order_key(text: str) -> OrderKey:
    words = [t for t in top_level_tokens(text) if t.word != ","]
    end = len(text)
    # NULLS FIRST|LAST is accepted and does not change the direction.
    if len(words) >= 2 and words[-2].word == "NULLS" and words[-1].word in ("FIRST", "LAST"):
        end = words[-2].start
        words = words[:-2]
    descending = False
    if words and words[-1].word in ("ASC", "DESC") and words[-1].end <= end:
        descending = words[-1].word == "DESC"
        end = words[-1].start
    return OrderKey(text, text[:end].strip(), descending)


def _int_at(tok: Token | None) -> int:
    if tok is None:
        raise SqlTextError("expected an integer")
    if not tok.word.isdigit():
        raise SqlTextError(f"expected an integer at offset {tok.start}, got {tok.word}")
    return int(tok.word)


def parse(sql: str) -> Query:
    """Top-level SELECT list, ORDER BY keys with directions, LIMIT and OFFSET of `sql`."""
    text = strip_statement(sql)
    tokens = top_level_tokens(text)
    words = [t for t in tokens if t.word != ","]
    if not words or words[0].word not in ("SELECT", "WITH"):
        raise SqlTextError("not a SELECT")
    if words[0].word == "WITH":
        raise SqlTextError("top-level WITH is not supported by the comparison")
    if any(t.word in ("UNION", "INTERSECT", "EXCEPT") for t in words):
        raise SqlTextError("top-level set operation is not supported by the comparison")
    select_kw = words[0]
    select_start = select_kw.end
    clause_starts = [t for t in words[1:] if t.word in CLAUSES]
    select_end = clause_starts[0].start if clause_starts else len(text)
    select_text = text[select_start:select_end]
    for modifier in ("DISTINCT", "ALL"):
        m = re.match(rf"\s*{modifier}\b", select_text, re.IGNORECASE)
        if m:
            select_start += m.end()
            select_text = text[select_start:select_end]
    items = [parse_select_item(p) for p in split_commas(select_text)]
    if not items:
        raise SqlTextError("empty select list")
    q = Query(text=text, select_start=select_start, select_end=select_end, items=items)

    i = 0
    while i < len(words):
        w = words[i]
        if w.word == "ORDER" and i + 1 < len(words) and words[i + 1].word == "BY":
            q.order_start = w.start
            following = [t for t in words[i + 2:] if t.word in CLAUSES]
            end = following[0].start if following else len(text)
            q.order_by = [parse_order_key(p) for p in split_commas(text[words[i + 1].end:end])]
            i += 2
            continue
        if w.word == "LIMIT":
            if q.limit_start is None:
                q.limit_start = w.start
            rest = [t for t in tokens if t.start > w.start]
            first = rest[0] if rest else None
            if first is None:
                raise SqlTextError("LIMIT without a count")
            value = _int_at(first)
            if len(rest) >= 2 and rest[1].word == ",":
                q.offset, q.limit = value, _int_at(rest[2] if len(rest) > 2 else None)
            else:
                q.limit = value
        elif w.word == "OFFSET":
            if q.limit_start is None:
                q.limit_start = w.start
            rest = [t for t in words if t.start > w.start]
            if not rest:
                raise SqlTextError("OFFSET without a count")
            q.offset = _int_at(rest[0])
        i += 1
    return q


def resolve_key(q: Query, key: OrderKey) -> int | None:
    """Output column of an ORDER BY key: by alias, ordinal or identical text; else None."""
    expr = key.expr
    if expr.isdigit():
        n = int(expr)
        if not 1 <= n <= len(q.items):
            raise SqlTextError(f"ORDER BY ordinal {n} out of range")
        if any(item.expr == "*" or item.expr.endswith(".*") for item in q.items[:n]):
            return None
        return n - 1
    if any(item.expr == "*" or item.expr.endswith(".*") for item in q.items):
        return None
    for idx, item in enumerate(q.items):
        if item.alias is not None and item.alias == expr:
            return idx
    for idx, item in enumerate(q.items):
        if item.alias is not None and item.alias.lower() == expr.lower():
            return idx
    target = collapse(expr)
    for idx, item in enumerate(q.items):
        if collapse(item.expr) == target:
            return idx
    return None


@dataclass(frozen=True)
class Plan:
    """What to run for the ordered rules: `sql` (original or augmented) and its key columns.

    `augmented` means some keys were appended to the select list; `original_columns` is
    then the width of the original output (`None` with `*`, whose width is unknown).
    """

    sql: str
    key_columns: tuple[int, ...]
    augmented: bool
    original_columns: int | None


def order_plan(q: Query) -> Plan:
    """Augmented variant (§15.2) when an ORDER BY key is not in the select list."""
    resolved = [resolve_key(q, k) for k in q.order_by]
    if all(r is not None for r in resolved):
        return Plan(q.text, tuple(resolved), False, len(q.items))
    has_star = any(item.expr == "*" or item.expr.endswith(".*") for item in q.items)
    width = None if has_star else len(q.items)
    extra = []
    columns = []
    for key, r in zip(q.order_by, resolved):
        if r is not None:
            columns.append(r)
            continue
        if key.expr.isdigit():
            raise SqlTextError(f"ORDER BY ordinal {key.expr} over * cannot be augmented")
        columns.append(("extra", len(extra)))
        extra.append(key.expr)
    # The k appended keys are the row's last k columns: the j-th (0-based) is at index j - k.
    key_columns = tuple(c if isinstance(c, int) else c[1] - len(extra) for c in columns)
    sql = q.text[:q.select_end].rstrip() + ", " + ", ".join(extra) + " " + q.text[q.select_end:].lstrip()
    return Plan(sql.rstrip(), key_columns, True, width)


def without_limit_aliased(q: Query) -> str:
    """The query without LIMIT/OFFSET and with its select list as `expr AS c1, …, expr AS cn`."""
    if any(item.expr == "*" or item.expr.endswith(".*") for item in q.items):
        raise SqlTextError("cannot alias the columns of *")
    select = ", ".join(f"{item.expr} AS c{i}" for i, item in enumerate(q.items, 1))
    head = q.text[:q.select_start]
    middle = q.text[q.select_end:q.limit_start if q.limit_start is not None else len(q.text)]
    return f"{head.rstrip()} {select} {middle.strip()}".rstrip()
