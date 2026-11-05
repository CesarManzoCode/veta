"""Random tables and queries of contract §17, shared by `tlp` and `optimizer-oracle`.

Everything derives from the seed: a dataset from `(seed, dataset)` and a query from
`(seed, index)`, so a query does not depend on how many were asked for and both commands
see the same workload. Queries have every output column aliased `c0, c1, …`, and those
with `LIMIT` are ordered by all their output columns, so ties only hold identical rows.
"""

import datetime
import random
from dataclasses import dataclass
from decimal import Decimal
from pathlib import Path

from harness.clickbench import sql_string

TABLES = ("t0", "t1", "t2")
COLUMNS = {
    "cbig": "BIGINT",
    "cint": "INTEGER",
    "cdec": "DECIMAL(10,2)",
    "cstr": "VARCHAR",
    "cdate": "DATE",
    "cbool": "BOOLEAN",
}
INTEGERS = ("cbig", "cint")
NUMERIC = (*INTEGERS, "cdec")
COMPARABLE = {
    "cbig": INTEGERS,
    "cint": INTEGERS,
    "cdec": ("cdec",),
    "cstr": ("cstr",),
    "cdate": ("cdate",),
    "cbool": ("cbool",),
}
PRIMARY_KEYS = ((), ("cbig",), ("cint", "cdate"), ("cstr",), ("cdate",))

MAX_ROWS = 2000
MAX_NULL_FRACTION = 0.5
EMPTY_TABLE_PROBABILITY = 0.1
# A new dataset every so many queries, so row counts and NULL fractions also vary in one run.
QUERIES_PER_DATASET = 250

# The key domains keep a three-table join near 8000 rows at the largest table sizes.
VALUE_RANGE = 500
DATE_RANGE = 1000
BASE_DATE = datetime.date(2020, 1, 1)
CENTS_RANGE = 100_000
EXTREME_PROBABILITY = 0.01
# Far from the Int64 limits: the generated `+`/`-` stay valid (an overflow is an error, §4).
EXTREME_BIG = 2**62
EXTREME_INT = 2_000_000_000
EXTREME_CENTS = 9_999_999_999
# Not letters of `true`, `false`, `inf` or `nan`, so a string never normalizes to a scalar (§15.1).
ALPHABET = "abc_%\t"
MAX_STRING = 3

TSV_ESCAPES = {"\\": "\\\\", "\t": "\\t", "\n": "\\n", "\r": "\\r", "\0": "\\0", "\b": "\\b", "\f": "\\f"}


# Data


@dataclass(frozen=True)
class Table:
    name: str
    primary_key: tuple[str, ...]
    rows: tuple[tuple, ...]

    @property
    def create_sql(self) -> str:
        columns = ", ".join(f"{name} {sql_type}" for name, sql_type in COLUMNS.items())
        key = f", PRIMARY KEY ({', '.join(self.primary_key)})" if self.primary_key else ""
        return f"CREATE TABLE {self.name} ({columns}{key})"


def random_string(rng: random.Random) -> str:
    return "".join(rng.choice(ALPHABET) for _ in range(rng.randint(0, MAX_STRING)))


def random_value(rng: random.Random, column: str):
    extreme = rng.random() < EXTREME_PROBABILITY
    match column:
        case "cbig":
            return rng.choice((-EXTREME_BIG, EXTREME_BIG)) if extreme else rng.randint(-VALUE_RANGE, VALUE_RANGE)
        case "cint":
            return rng.choice((-EXTREME_INT, EXTREME_INT)) if extreme else rng.randint(-VALUE_RANGE, VALUE_RANGE)
        case "cdec":
            cents = rng.choice((-EXTREME_CENTS, EXTREME_CENTS)) if extreme else rng.randint(-CENTS_RANGE, CENTS_RANGE)
            return Decimal(cents).scaleb(-2)
        case "cstr":
            return random_string(rng)
        case "cdate":
            return BASE_DATE + datetime.timedelta(days=rng.randrange(DATE_RANGE))
        case "cbool":
            return rng.random() < 0.5
    raise ValueError(f"unknown column: {column}")


def generate_table(rng: random.Random, name: str) -> Table:
    """0–2000 rows with a NULL fraction of 0–50 % for each column."""
    row_count = 0 if rng.random() < EMPTY_TABLE_PROBABILITY else rng.randint(1, MAX_ROWS)
    fractions = {column: rng.uniform(0, MAX_NULL_FRACTION) for column in COLUMNS}
    rows = tuple(
        tuple(None if rng.random() < fractions[c] else random_value(rng, c) for c in COLUMNS)
        for _ in range(row_count)
    )
    return Table(name, rng.choice(PRIMARY_KEYS), rows)


def generate_dataset(seed: int, dataset: int) -> tuple[Table, ...]:
    rng = random.Random(f"{seed}:data:{dataset}")
    return tuple(generate_table(rng, name) for name in TABLES)


def tsv_cell(value) -> str:
    """One value in the format of §13.1."""
    if value is None:
        return "\\N"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, Decimal):
        return f"{value:.2f}"
    if isinstance(value, datetime.date):
        return value.isoformat()
    if isinstance(value, str):
        return "".join(TSV_ESCAPES.get(c, c) for c in value)
    return str(value)


def tsv_text(rows) -> str:
    return "".join("\t".join(tsv_cell(v) for v in row) + "\n" for row in rows)


def split_batches(rng: random.Random, rows: tuple[tuple, ...]) -> list[tuple[tuple, ...]]:
    """`rows` cut into 2–4 non-empty batches, or fewer if there are not enough rows."""
    count = min(rng.randint(2, 4), len(rows))
    if count == 0:
        return []
    cuts = sorted(rng.sample(range(1, len(rows)), count - 1))
    bounds = [0, *cuts, len(rows)]
    return [rows[a:b] for a, b in zip(bounds, bounds[1:])]


def write_batches(seed: int, dataset: int, table: Table, directory: Path) -> list[Path]:
    """One TSV file per batch; `veta load` publishes each file as one part."""
    rng = random.Random(f"{seed}:batches:{dataset}:{table.name}")
    files = []
    for n, batch in enumerate(split_batches(rng, table.rows)):
        file = directory / f"{table.name}-{n}.tsv"
        file.write_text(tsv_text(batch), encoding="utf-8")
        files.append(file)
    return files


# Expressions


@dataclass(frozen=True)
class ColumnRef:
    alias: str
    name: str

    def __str__(self) -> str:
        return f"{self.alias}.{self.name}"


def columns_of(alias: str) -> list[ColumnRef]:
    return [ColumnRef(alias, name) for name in COLUMNS]


def decimal_literal(rng: random.Random) -> str:
    return f"{Decimal(rng.randint(-CENTS_RANGE, CENTS_RANGE)).scaleb(-2):.2f}"


def date_text(rng: random.Random) -> str:
    return (BASE_DATE + datetime.timedelta(days=rng.randrange(DATE_RANGE))).isoformat()


def literal(rng: random.Random, column: str) -> str:
    match column:
        case "cbig" | "cint":
            return str(rng.randint(-VALUE_RANGE, VALUE_RANGE))
        case "cdec":
            return decimal_literal(rng)
        case "cstr":
            return sql_string(random_string(rng))
        case "cdate":
            return f"DATE '{date_text(rng)}'"
        case "cbool":
            return rng.choice(("TRUE", "FALSE"))
    raise ValueError(f"unknown column: {column}")


def comparison_operator(rng: random.Random, column: str) -> str:
    if column == "cbool":
        return rng.choice(("=", "<>"))
    return rng.choice(("=", "<>", "!=", "<", "<=", ">", ">="))


def literal_comparison(rng, col, cols) -> str:
    # §4: a DATE also compares with a string literal.
    value = sql_string(date_text(rng)) if col.name == "cdate" and rng.random() < 0.2 else literal(rng, col.name)
    return f"{col} {comparison_operator(rng, col.name)} {value}"


def column_comparison(rng, col, cols) -> str:
    other = rng.choice([c for c in cols if c.name in COMPARABLE[col.name]])
    return f"{col} {comparison_operator(rng, col.name)} {other}"


def null_test(rng, col, cols) -> str:
    return f"{col} IS {rng.choice(('', 'NOT '))}NULL"


def between(rng, col, cols) -> str:
    negated = rng.choice(("", "NOT "))
    return f"{col} {negated}BETWEEN {literal(rng, col.name)} AND {literal(rng, col.name)}"


def in_list(rng, col, cols) -> str:
    values = ", ".join(literal(rng, col.name) for _ in range(rng.randint(1, 4)))
    return f"{col} {rng.choice(('', 'NOT '))}IN ({values})"


def arithmetic(rng, col, cols) -> str:
    # `*` only on INTEGER: promoted to Int64 it cannot overflow, while BIGINT holds ±2^62.
    operators = ("+", "-", "*") if col.name == "cint" else ("+", "-")
    term = f"({col} {rng.choice(operators)} {rng.randint(1, 9)})"
    modulus = rng.randint(2, 9)
    return f"{term} % {modulus} {comparison_operator(rng, col.name)} {rng.randrange(modulus)}"


def like(rng, col, cols) -> str:
    pattern = "".join(rng.choice("abc_%") for _ in range(rng.randint(1, 3)))
    return f"{col} {rng.choice(('', 'NOT '))}LIKE {sql_string(pattern)}"


def string_length(rng, col, cols) -> str:
    return f"length({col}) {comparison_operator(rng, 'cint')} {rng.randint(0, MAX_STRING)}"


def date_part(rng, col, cols) -> str:
    part, low, high = rng.choice((("year", 2020, 2022), ("month", 1, 12), ("day", 1, 31)))
    return f"extract({part} FROM {col}) {comparison_operator(rng, 'cint')} {rng.randint(low, high)}"


def bare_boolean(rng, col, cols) -> str:
    return str(col)


def atom(rng: random.Random, cols: list[ColumnRef]) -> str:
    col = rng.choice(cols)
    forms = [literal_comparison, column_comparison, null_test]
    if col.name != "cbool":
        forms += [between, in_list]
    if col.name in INTEGERS:
        forms.append(arithmetic)
    match col.name:
        case "cstr":
            forms += [like, string_length]
        case "cdate":
            forms.append(date_part)
        case "cbool":
            forms.append(bare_boolean)
    return rng.choice(forms)(rng, col, cols)


def predicate(rng: random.Random, cols: list[ColumnRef], depth: int = 2) -> str:
    if depth == 0 or rng.random() < 0.4:
        return atom(rng, cols)
    inner = predicate(rng, cols, depth - 1)
    match rng.choice(("and", "or", "not", "constant", "case")):
        case "and":
            return f"({inner} AND {predicate(rng, cols, depth - 1)})"
        case "or":
            return f"({inner} OR {predicate(rng, cols, depth - 1)})"
        case "not":
            return f"NOT ({inner})"
        case "constant":
            # Food for constant folding and boolean simplification (§11.2, rule 1).
            return rng.choice((f"({inner} AND TRUE)", f"({inner} OR FALSE)", f"({inner} AND 1 + 1 = 2)"))
        case _:
            return f"CASE WHEN {inner} THEN 1 ELSE 0 END = 1"


def output_expression(rng: random.Random, cols: list[ColumnRef]) -> str:
    col = rng.choice(cols)
    if rng.random() < 0.8:
        return str(col)
    match col.name:
        case "cint":
            return rng.choice((f"{col} + 1", f"{col} * 2", f"CAST({col} AS BIGINT)", f"{col} % 5"))
        case "cbig":
            return rng.choice((f"{col} - 1", f"{col} % 5"))
        case "cdec":
            return f"{col} + 1.50"
        case "cstr":
            return rng.choice((f"length({col})", f"substring({col}, 1, 2)"))
        case "cdate":
            return rng.choice((f"extract(year FROM {col})", f"DATE_TRUNC('month', {col})"))
    return str(col)


# FROM and WHERE


def of_names(cols: list[ColumnRef], names) -> list[ColumnRef]:
    return [c for c in cols if c.name in names]


def join_condition(rng: random.Random, left: list[ColumnRef], right: list[ColumnRef], single_join: bool) -> str:
    """An equality or a narrow band, with extra predicates on either side.

    Strings (few distinct values) and the keyless band only go in a two-table query, so the
    output stays small.
    """
    forms = ["integer", "integer", "date"] + (["string", "band"] if single_join else [])
    match rng.choice(forms):
        case "integer":
            on = f"{rng.choice(of_names(left, INTEGERS))} = {rng.choice(of_names(right, INTEGERS))}"
        case "date":
            on = f"{rng.choice(of_names(left, ('cdate',)))} = {rng.choice(of_names(right, ('cdate',)))}"
        case "string":
            on = f"{rng.choice(of_names(left, ('cstr',)))} = {rng.choice(of_names(right, ('cstr',)))}"
        case _:
            low = rng.choice(of_names(right, INTEGERS))
            on = f"{rng.choice(of_names(left, INTEGERS))} BETWEEN {low} AND {low} + 2"
    for cols, probability in ((right, 0.4), (left, 0.2), (left + right, 0.2)):
        if rng.random() < probability:
            on += f" AND {predicate(rng, cols, 1)}"
    return on


def random_from(rng: random.Random) -> tuple[str, list[ColumnRef], list[ColumnRef]]:
    """The FROM text, the visible columns and those on a side of a LEFT JOIN that adds NULLs."""
    joins = rng.choices((0, 1, 2), weights=(25, 45, 30))[0]
    text = f"{rng.choice(TABLES)} AS x0"
    cols = columns_of("x0")
    null_side = []
    for n in range(1, joins + 1):
        alias = f"x{n}"
        right = columns_of(alias)
        kind = rng.choice(("JOIN", "LEFT JOIN"))
        text += f" {kind} {rng.choice(TABLES)} AS {alias} ON {join_condition(rng, cols, right, joins == 1)}"
        if kind == "LEFT JOIN":
            null_side += right
        cols = cols + right
    return text, cols, null_side


def random_where(rng: random.Random, cols: list[ColumnRef], null_side: list[ColumnRef]) -> str | None:
    if null_side and rng.random() < 0.5:
        return predicate(rng, null_side)
    return predicate(rng, cols) if rng.random() < 0.6 else None


# Queries


@dataclass(frozen=True)
class OutputColumn:
    """`expr` as written; `aggregate` is `COUNT|SUM|MIN|MAX`, or `None` for anything else."""

    expr: str
    aggregate: str | None = None


@dataclass(frozen=True)
class OrderTerm:
    column: int
    descending: bool = False
    nulls: str | None = None

    def __str__(self) -> str:
        nulls = f" NULLS {self.nulls}" if self.nulls else ""
        return f"c{self.column} {'DESC' if self.descending else 'ASC'}{nulls}"


@dataclass(frozen=True)
class GenQuery:
    """A query in parts, so that `tlp` can rebuild it around its partitions."""

    index: int
    dataset: int
    columns: tuple[OutputColumn, ...]
    from_clause: str
    where: str | None
    partition: str
    distinct: bool = False
    aggregated: bool = False
    group_by: tuple[str, ...] = ()
    order_by: tuple[OrderTerm, ...] = ()
    limit: int | None = None
    offset: int | None = None

    def render(self, where: str | None, window: bool) -> str:
        select = ", ".join(f"{c.expr} AS c{i}" for i, c in enumerate(self.columns))
        text = f"SELECT {'DISTINCT ' if self.distinct else ''}{select} FROM {self.from_clause}"
        if where is not None:
            text += f" WHERE {where}"
        if self.group_by:
            text += f" GROUP BY {', '.join(self.group_by)}"
        if window and self.order_by:
            text += f" ORDER BY {', '.join(map(str, self.order_by))}"
        if window and self.limit is not None:
            text += f" LIMIT {self.limit}"
        if window and self.offset is not None:
            text += f" OFFSET {self.offset}"
        return text

    @property
    def sql(self) -> str:
        return self.render(self.where, window=True)

    def partition_sqls(self) -> tuple[str, str, str]:
        """The query under `p`, `NOT p` and `p IS NULL`, without its ORDER BY and LIMIT."""
        p = self.partition
        conditions = (p, f"NOT ({p})", f"({p}) IS NULL")
        if self.where is not None:
            conditions = tuple(f"({self.where}) AND ({c})" for c in conditions)
        return tuple(self.render(c, window=False) for c in conditions)


def aggregate_columns(rng: random.Random, cols: list[ColumnRef], required: bool) -> list[OutputColumn]:
    out = []
    for _ in range(rng.randint(1 if required else 0, 3)):
        match rng.choice(("COUNT", "SUM", "MIN", "MAX")):
            case "COUNT":
                inner = rng.choice(("*", str(rng.choice(cols))))
                out.append(OutputColumn(f"COUNT({inner})", "COUNT"))
            case "SUM":
                col = rng.choice(of_names(cols, NUMERIC))
                inner = f"{col} + 1" if col.name == "cint" and rng.random() < 0.2 else str(col)
                out.append(OutputColumn(f"SUM({inner})", "SUM"))
            case func:
                out.append(OutputColumn(f"{func}({rng.choice(cols)})", func))
    return out


def group_reference(rng: random.Random, columns: list[OutputColumn], i: int) -> str:
    """§10.2: a GROUP BY entry is an expression, an alias or an ordinal."""
    return rng.choice((columns[i].expr, f"c{i}", str(i + 1)))


def generate_query(seed: int, index: int) -> GenQuery:
    rng = random.Random(f"{seed}:query:{index}")
    from_clause, cols, null_side = random_from(rng)
    where = random_where(rng, cols, null_side)
    partition = predicate(rng, cols)
    distinct = aggregated = False
    group_by: tuple[str, ...] = ()
    match rng.choices(("plain", "distinct", "aggregate"), weights=(40, 20, 40))[0]:
        case "aggregate":
            aggregated = True
            keys = [OutputColumn(output_expression(rng, cols)) for _ in range(rng.choice((0, 1, 1, 2, 2, 3)))]
            columns = keys + aggregate_columns(rng, cols, required=not keys)
            rng.shuffle(columns)
            group_by = tuple(group_reference(rng, columns, i) for i, c in enumerate(columns) if c.aggregate is None)
        case shape:
            distinct = shape == "distinct"
            columns = [OutputColumn(output_expression(rng, cols)) for _ in range(rng.randint(1, 4))]
    order_by: tuple[OrderTerm, ...] = ()
    limit = offset = None
    if rng.random() < 0.35:
        positions = list(range(len(columns)))
        rng.shuffle(positions)
        order_by = tuple(
            OrderTerm(i, rng.random() < 0.5, rng.choice((None, None, "FIRST", "LAST"))) for i in positions
        )
        limit = rng.randint(1, 50)
        offset = rng.randint(1, 20) if rng.random() < 0.2 else None
    return GenQuery(
        index=index,
        dataset=index // QUERIES_PER_DATASET,
        columns=tuple(columns),
        from_clause=from_clause,
        where=where,
        partition=partition,
        distinct=distinct,
        aggregated=aggregated,
        group_by=group_by,
        order_by=order_by,
        limit=limit,
        offset=offset,
    )
