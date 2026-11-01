//! Prints the AST back as SQL (used by EXPLAIN, contract §11).
//!
//! Every compound expression gets explicit parentheses, strings double their `'`, dates and
//! timestamps print as `CAST('…' AS DATE|TIMESTAMP)` and identifiers are quoted only when
//! they would not re-lex as the same plain word. The output re-parses to an expression that
//! prints the same text.

use std::fmt::{self, Display, Formatter, Write};

use veta_core::datetime::{self, TruncUnit};
use veta_core::decimal;

use crate::ast::*;
use crate::parser::is_reserved;

/// Writes `name` as an identifier, quoting it with `"` when needed.
pub fn write_ident(f: &mut impl Write, name: &str) -> fmt::Result {
    let plain = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        && !is_reserved(name)
        && !name.eq_ignore_ascii_case("DATE")
        && !name.eq_ignore_ascii_case("TIMESTAMP");
    if plain {
        f.write_str(name)
    } else {
        f.write_char('"')?;
        f.write_str(&name.replace('"', "\"\""))?;
        f.write_char('"')
    }
}

/// Writes `s` as a `'…'` literal with `''` for each quote.
pub fn write_string(f: &mut impl Write, s: &str) -> fmt::Result {
    f.write_char('\'')?;
    f.write_str(&s.replace('\'', "''"))?;
    f.write_char('\'')
}

fn trunc_unit_name(unit: TruncUnit) -> &'static str {
    match unit {
        TruncUnit::Minute => "minute",
        TruncUnit::Hour => "hour",
        TruncUnit::Day => "day",
        TruncUnit::Month => "month",
        TruncUnit::Year => "year",
    }
}

fn write_list<T: Display>(f: &mut Formatter<'_>, items: &[T]) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

fn not(negated: bool) -> &'static str {
    if negated { "NOT " } else { "" }
}

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Null => f.write_str("NULL"),
            Literal::Boolean(true) => f.write_str("TRUE"),
            Literal::Boolean(false) => f.write_str("FALSE"),
            Literal::Integer(v) => write!(f, "{v}"),
            Literal::Decimal(d) => {
                f.write_str(&decimal::format(d.value, d.scale))?;
                // A scale-0 decimal keeps its point so that it re-reads as a decimal.
                if d.scale == 0 {
                    f.write_char('.')
                } else {
                    Ok(())
                }
            }
            Literal::String(s) => write_string(f, s),
            Literal::Date(days) => write!(f, "CAST('{}' AS DATE)", datetime::format_date(*days)),
            Literal::Timestamp(seconds) => write!(
                f,
                "CAST('{}' AS TIMESTAMP)",
                datetime::format_timestamp(*seconds)
            ),
        }
    }
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(lit) => write!(f, "{lit}"),
            Expr::Column { table, name } => {
                if let Some(table) = table {
                    write_ident(f, table)?;
                    f.write_char('.')?;
                }
                write_ident(f, name)
            }
            Expr::Unary {
                op: UnaryOp::Neg,
                expr,
            } => write!(f, "(- {expr})"),
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => write!(f, "(NOT {expr})"),
            Expr::Binary { op, left, right } => write!(f, "({left} {} {right})", op.symbol()),
            Expr::IsNull { expr, negated } => write!(f, "({expr} IS {}NULL)", not(*negated)),
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => write!(f, "({expr} {}BETWEEN {low} AND {high})", not(*negated)),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                write!(f, "({expr} {}IN (", not(*negated))?;
                write_list(f, list)?;
                f.write_str("))")
            }
            Expr::InSubquery {
                expr,
                query,
                negated,
            } => write!(f, "({expr} {}IN ({query}))", not(*negated)),
            Expr::Exists { query, negated } => write!(f, "({}EXISTS ({query}))", not(*negated)),
            Expr::ScalarSubquery(query) => write!(f, "({query})"),
            Expr::Like {
                expr,
                pattern,
                negated,
            } => write!(f, "({expr} {}LIKE {pattern})", not(*negated)),
            Expr::Case { branches, else_ } => {
                f.write_str("CASE")?;
                for (cond, value) in branches {
                    write!(f, " WHEN {cond} THEN {value}")?;
                }
                if let Some(e) = else_ {
                    write!(f, " ELSE {e}")?;
                }
                f.write_str(" END")
            }
            Expr::Cast { expr, data_type } => {
                write!(f, "CAST({expr} AS {})", data_type.sql_name())
            }
            Expr::Extract { part, expr } => write!(f, "extract({} FROM {expr})", part.name()),
            Expr::Substring { expr, from, length } => {
                write!(f, "substring({expr} FROM {from}")?;
                if let Some(length) = length {
                    write!(f, " FOR {length}")?;
                }
                f.write_char(')')
            }
            Expr::DateTrunc { unit, expr } => {
                write!(f, "date_trunc('{}', {expr})", trunc_unit_name(*unit))
            }
            Expr::CountStar => f.write_str("count(*)"),
            Expr::Function {
                func,
                distinct,
                args,
            } => {
                write!(f, "{}(", func.name())?;
                if *distinct {
                    f.write_str("DISTINCT ")?;
                }
                write_list(f, args)?;
                f.write_char(')')
            }
        }
    }
}

impl Display for SelectItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Wildcard => f.write_char('*'),
            SelectItem::Expr { expr, alias, .. } => {
                write!(f, "{expr}")?;
                if let Some(alias) = alias {
                    f.write_str(" AS ")?;
                    write_ident(f, alias)?;
                }
                Ok(())
            }
        }
    }
}

fn write_alias(f: &mut Formatter<'_>, alias: &Option<String>) -> fmt::Result {
    if let Some(alias) = alias {
        f.write_str(" AS ")?;
        write_ident(f, alias)?;
    }
    Ok(())
}

impl Display for TableRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            TableRef::Table { name, alias } => {
                write_ident(f, name)?;
                write_alias(f, alias)
            }
            TableRef::Subquery {
                query,
                alias,
                column_aliases,
            } => {
                write!(f, "({query}) AS ")?;
                write_ident(f, alias)?;
                if !column_aliases.is_empty() {
                    f.write_str(" (")?;
                    for (i, c) in column_aliases.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write_ident(f, c)?;
                    }
                    f.write_char(')')?;
                }
                Ok(())
            }
            TableRef::Function { name, args, alias } => {
                write!(f, "{name}(")?;
                write_list(f, args)?;
                f.write_char(')')?;
                write_alias(f, alias)
            }
            TableRef::Join {
                left,
                right,
                kind,
                on,
            } => {
                let kw = match kind {
                    JoinKind::Inner => "INNER JOIN",
                    JoinKind::Left => "LEFT JOIN",
                    JoinKind::Cross => "CROSS JOIN",
                };
                write!(f, "{left} {kw} {right}")?;
                if let Some(on) = on {
                    write!(f, " ON {on}")?;
                }
                Ok(())
            }
        }
    }
}

impl Display for OrderByItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if self.desc {
            f.write_str(" DESC")?;
        }
        match self.nulls_first {
            Some(true) => f.write_str(" NULLS FIRST"),
            Some(false) => f.write_str(" NULLS LAST"),
            None => Ok(()),
        }
    }
}

impl Display for Select {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("SELECT ")?;
        if self.distinct {
            f.write_str("DISTINCT ")?;
        }
        write_list(f, &self.items)?;
        if !self.from.is_empty() {
            f.write_str(" FROM ")?;
            write_list(f, &self.from)?;
        }
        if let Some(w) = &self.where_ {
            write!(f, " WHERE {w}")?;
        }
        if !self.group_by.is_empty() {
            f.write_str(" GROUP BY ")?;
            write_list(f, &self.group_by)?;
        }
        if let Some(h) = &self.having {
            write!(f, " HAVING {h}")?;
        }
        Ok(())
    }
}

impl Display for Query {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if !self.ctes.is_empty() {
            f.write_str("WITH ")?;
            for (i, cte) in self.ctes.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_ident(f, &cte.name)?;
                write!(f, " AS ({})", cte.query)?;
            }
            f.write_char(' ')?;
        }
        write!(f, "{}", self.body)?;
        if !self.order_by.is_empty() {
            f.write_str(" ORDER BY ")?;
            write_list(f, &self.order_by)?;
        }
        if let Some(limit) = self.limit {
            write!(f, " LIMIT {limit}")?;
        }
        if let Some(offset) = self.offset {
            write!(f, " OFFSET {offset}")?;
        }
        Ok(())
    }
}

impl Display for CreateTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE ")?;
        if self.or_replace {
            f.write_str("OR REPLACE ")?;
        }
        f.write_str("TABLE ")?;
        write_ident(f, &self.name)?;
        f.write_str(" (")?;
        for (i, c) in self.columns.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write_ident(f, &c.name)?;
            write!(f, " {}", c.data_type.sql_name())?;
            if !c.nullable {
                f.write_str(" NOT NULL")?;
            }
        }
        if !self.primary_key.is_empty() {
            f.write_str(", PRIMARY KEY (")?;
            for (i, k) in self.primary_key.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_ident(f, k)?;
            }
            f.write_char(')')?;
        }
        f.write_char(')')?;
        if let Some(engine) = &self.engine {
            f.write_str(" ENGINE = ")?;
            write_ident(f, engine)?;
        }
        if !self.settings.is_empty() {
            f.write_str(" SETTINGS ")?;
            for (i, (k, v)) in self.settings.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_ident(f, k)?;
                write!(f, " = {v}")?;
            }
        }
        Ok(())
    }
}

impl Display for Statement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Statement::CreateTable(ct) => write!(f, "{ct}"),
            Statement::Query(q) => write!(f, "{q}"),
            Statement::Explain { analyze, query } => {
                f.write_str("EXPLAIN ")?;
                if *analyze {
                    f.write_str("ANALYZE ")?;
                }
                write!(f, "{query}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::parser::{parse_expr, parse_statement, parse_statements};

    fn show(sql: &str) -> String {
        parse_expr(sql).unwrap().to_string()
    }

    /// Printing, re-parsing and printing again gives the same text.
    fn assert_stable(sql: &str) {
        let once = show(sql);
        let twice = parse_expr(&once).unwrap().to_string();
        assert_eq!(once, twice, "from {sql}");
    }

    #[test]
    fn expressions_print_with_explicit_parentheses() {
        assert_eq!(show("a + b * c"), "(a + (b * c))");
        assert_eq!(
            show("o.o_orderkey = l.l_orderkey"),
            "(o.o_orderkey = l.l_orderkey)"
        );
        assert_eq!(show("a != b"), "(a <> b)");
        assert_eq!(show("NOT a OR b AND c"), "((NOT a) OR (b AND c))");
        assert_eq!(show("- - x"), "(- (- x))");
        assert_eq!(show("-1 - -2.50"), "(-1 - -2.50)");
    }

    #[test]
    fn literals_print_as_sql() {
        assert_eq!(show("'it''s'"), "'it''s'");
        assert_eq!(show("DATE '1995-03-15'"), "CAST('1995-03-15' AS DATE)");
        assert_eq!(
            show("TIMESTAMP '2013-07-01 10:00:00'"),
            "CAST('2013-07-01 10:00:00' AS TIMESTAMP)"
        );
        assert_eq!(
            show("CAST('1995-03-15' AS date)"),
            "CAST('1995-03-15' AS DATE)"
        );
        assert_eq!(show("0.0001000000"), "0.0001000000");
        assert_eq!(show("7."), "7.");
        assert_eq!(show("NULL"), "NULL");
        assert_eq!(show("true"), "TRUE");
    }

    #[test]
    fn predicates_print_as_sql() {
        assert_eq!(show("a IS NOT NULL"), "(a IS NOT NULL)");
        assert_eq!(show("a NOT BETWEEN 1 AND 2"), "(a NOT BETWEEN 1 AND 2)");
        assert_eq!(show("a IN (1, -1)"), "(a IN (1, -1))");
        assert_eq!(show("a NOT LIKE '%x%'"), "(a NOT LIKE '%x%')");
        assert_eq!(
            show("a NOT IN (SELECT b FROM t WHERE c > 1)"),
            "(a NOT IN (SELECT b FROM t WHERE (c > 1)))"
        );
        assert_eq!(
            show("NOT EXISTS (SELECT * FROM t)"),
            "(NOT EXISTS (SELECT * FROM t))"
        );
        assert_eq!(
            show("x < (SELECT 0.2 * avg(q) FROM l)"),
            "(x < (SELECT (0.2 * avg(q)) FROM l))"
        );
    }

    #[test]
    fn functions_print_as_sql() {
        assert_eq!(
            show("CASE WHEN a = 0 THEN b ELSE '' END"),
            "CASE WHEN (a = 0) THEN b ELSE '' END"
        );
        assert_eq!(show("extract(YEAR FROM d)"), "extract(year FROM d)");
        assert_eq!(show("substring(s, 1, 2)"), "substring(s FROM 1 FOR 2)");
        assert_eq!(show("SUBSTRING(s FROM 3)"), "substring(s FROM 3)");
        assert_eq!(
            show("DATE_TRUNC('minute', EventTime)"),
            "date_trunc('minute', EventTime)"
        );
        assert_eq!(show("COUNT(*)"), "count(*)");
        assert_eq!(show("COUNT(DISTINCT u)"), "count(DISTINCT u)");
        assert_eq!(show("STRLEN(u)"), "strlen(u)");
        assert_eq!(
            show(r"REGEXP_REPLACE(r, '^(a)$', '\1')"),
            r"regexp_replace(r, '^(a)$', '\1')"
        );
        assert_eq!(show("CAST(x AS NUMERIC(15,2))"), "CAST(x AS DECIMAL(15,2))");
    }

    #[test]
    fn identifiers_are_quoted_when_needed() {
        assert_eq!(show("\"weird name\""), "\"weird name\"");
        assert_eq!(show("\"select\""), "\"select\"");
        assert_eq!(show("\"a\"\"b\""), "\"a\"\"b\"");
        assert_eq!(show("plain_Name1"), "plain_Name1");
    }

    #[test]
    fn printing_is_stable() {
        for sql in [
            "a + b * c - d / e % f",
            "-x * -3",
            "NOT (a = 1 OR b IS NULL) AND c NOT IN (1, 2)",
            "x BETWEEN DATE '1995-01-01' AND CAST('1996-12-31' AS date)",
            "CASE WHEN a THEN 1 WHEN b THEN 2 END",
            "substring(c_phone FROM 1 FOR 2) IN ('13', '31')",
            "EXISTS (SELECT * FROM l WHERE l.k = o.k AND l.s <> o.s)",
            "\"Mixed Case\" LIKE 'x''y'",
            "s = (WITH r AS (SELECT 1 AS m) SELECT max(m) FROM r)",
        ] {
            assert_stable(sql);
        }
    }

    #[test]
    fn queries_and_statements_print_and_reparse() {
        for sql in [
            "SELECT DISTINCT a AS x, count(*) FROM t AS u LEFT JOIN v ON (u.k = v.k) \
             CROSS JOIN w WHERE (a > 1) GROUP BY a, 2 HAVING (count(*) > 1) \
             ORDER BY x DESC NULLS LAST, 2 NULLS FIRST LIMIT 10 OFFSET 5",
            "WITH r AS (SELECT 1 AS m) SELECT * FROM r, (SELECT 2) AS s (n) \
             INNER JOIN read_parquet('/tmp/a.parquet') AS p ON (s.n = p.n)",
            "EXPLAIN ANALYZE SELECT 1",
            "CREATE OR REPLACE TABLE t (a BIGINT NOT NULL, b VARCHAR, PRIMARY KEY (a)) \
             ENGINE = MergeTree SETTINGS x = 1, y = ''",
        ] {
            let once = parse_statement(sql).unwrap().to_string();
            assert_eq!(once, sql);
            let twice = parse_statement(&once).unwrap().to_string();
            assert_eq!(once, twice);
        }
    }

    #[test]
    fn reference_queries_print_stably() {
        let tpch = r"
            SELECT c_count, count(*) AS custdist
            FROM (SELECT c_custkey, count(o_orderkey) FROM customer
                  LEFT OUTER JOIN orders ON c_custkey = o_custkey
                  AND o_comment NOT LIKE '%special%requests%'
                  GROUP BY c_custkey) AS c_orders (c_custkey, c_count)
            GROUP BY c_count ORDER BY custdist DESC, c_count DESC;";
        for statement in parse_statements(tpch).unwrap() {
            let once = statement.to_string();
            let twice = parse_statement(&once).unwrap().to_string();
            assert_eq!(once, twice);
        }
    }
}
