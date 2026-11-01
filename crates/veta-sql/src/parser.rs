//! Recursive-descent parser for the SQL subset of contract §10.
//!
//! Expression precedence, loosest first: `OR` < `AND` < `NOT` < comparison, `LIKE`, `IN`,
//! `BETWEEN`, `IS` < `+ -` < `* / %` < unary `-`. Anything outside §10 is a
//! `VetaError::User` starting with `unsupported:`; syntax errors carry line and column.

use veta_core::datetime::{self, DatePart, TruncUnit};
use veta_core::{DataType, Result, VetaError, decimal};

use crate::ast::*;
use crate::lexer::{Token, TokenKind, error_at, tokenize};

/// Words that end an expression or a source and so cannot be bare aliases.
const RESERVED: &[&str] = &[
    "ALL",
    "AND",
    "ANTI",
    "ARRAY",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CROSS",
    "DESC",
    "DISTINCT",
    "ELSE",
    "END",
    "ENGINE",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FETCH",
    "FINAL",
    "FOR",
    "FORMAT",
    "FROM",
    "FULL",
    "GLOBAL",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INNER",
    "INTERSECT",
    "INTERVAL",
    "IS",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLS",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PREWHERE",
    "QUALIFY",
    "RIGHT",
    "SAMPLE",
    "SELECT",
    "SEMI",
    "SETTINGS",
    "THEN",
    "TRUE",
    "UNION",
    "USING",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

pub(crate) fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(word))
}

/// Parses one or more statements separated by `;`; empty statements are skipped.
pub fn parse_statements(sql: &str) -> Result<Vec<Statement>> {
    let mut parser = Parser::new(sql)?;
    let mut statements = Vec::new();
    loop {
        while parser.eat(&TokenKind::Semicolon) {}
        if parser.at_eof() {
            return Ok(statements);
        }
        statements.push(parser.statement()?);
        if !parser.at_eof() {
            parser.expect(&TokenKind::Semicolon, "';' or end of input")?;
        }
    }
}

/// Parses exactly one statement (a trailing `;` is allowed).
pub fn parse_statement(sql: &str) -> Result<Statement> {
    let mut statements = parse_statements(sql)?;
    match statements.len() {
        1 => Ok(statements.remove(0)),
        0 => Err(VetaError::user("syntax error: empty statement")),
        n => Err(VetaError::user(format!(
            "syntax error: expected one statement, found {n}"
        ))),
    }
}

/// Parses a standalone expression (used by tests and by callers that receive SQL
/// fragments).
pub fn parse_expr(sql: &str) -> Result<Expr> {
    let mut parser = Parser::new(sql)?;
    let expr = parser.expr()?;
    if !parser.at_eof() {
        return Err(parser.unexpected("end of expression"));
    }
    Ok(expr)
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Result<Self> {
        Ok(Parser {
            src,
            tokens: tokenize(src)?,
            pos: 0,
        })
    }

    // ---- token helpers ----

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_at(&self, offset: usize) -> &Token {
        let last = self.tokens.len() - 1;
        &self.tokens[(self.pos + offset).min(last)]
    }

    fn at_eof(&self) -> bool {
        self.peek().kind == TokenKind::Eof
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        if token.kind != TokenKind::Eof {
            self.pos += 1;
        }
        token
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if &self.peek().kind == kind {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<Token> {
        if &self.peek().kind == kind {
            Ok(self.advance())
        } else {
            Err(self.unexpected(what))
        }
    }

    fn unexpected(&self, what: &str) -> VetaError {
        let t = self.peek();
        error_at(
            t.line,
            t.column,
            format!("expected {what}, found {}", t.kind.describe()),
        )
    }

    fn error_here(&self, msg: impl std::fmt::Display) -> VetaError {
        let t = self.peek();
        error_at(t.line, t.column, msg)
    }

    fn unsupported_here(&self, what: impl std::fmt::Display) -> VetaError {
        let t = self.peek();
        error_at(t.line, t.column, format!("unsupported: {what}"))
    }

    fn word_at(&self, offset: usize, kw: &str) -> bool {
        matches!(&self.peek_at(offset).kind, TokenKind::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    fn at_kw(&self, kw: &str) -> bool {
        self.word_at(0, kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.unexpected(kw))
        }
    }

    /// An identifier: a non-reserved word or a quoted identifier.
    fn ident(&mut self, what: &str) -> Result<String> {
        match &self.peek().kind {
            TokenKind::Word(w) if !is_reserved(w) => {
                let w = w.clone();
                self.advance();
                Ok(w)
            }
            TokenKind::QuotedIdent(w) => {
                let w = w.clone();
                self.advance();
                Ok(w)
            }
            _ => Err(self.unexpected(what)),
        }
    }

    fn at_ident(&self) -> bool {
        match &self.peek().kind {
            TokenKind::Word(w) => !is_reserved(w),
            TokenKind::QuotedIdent(_) => true,
            _ => false,
        }
    }

    /// `[AS] alias`, where the alias is optional only when `AS` is absent.
    fn opt_alias(&mut self) -> Result<Option<String>> {
        if self.eat_kw("AS") {
            return Ok(Some(self.ident("alias")?));
        }
        if self.at_ident() {
            return Ok(Some(self.ident("alias")?));
        }
        Ok(None)
    }

    fn ident_list(&mut self) -> Result<Vec<String>> {
        self.expect(&TokenKind::LParen, "'('")?;
        let mut names = vec![self.ident("column name")?];
        while self.eat(&TokenKind::Comma) {
            names.push(self.ident("column name")?);
        }
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(names)
    }

    fn unsigned_integer(&mut self, what: &str) -> Result<u64> {
        match &self.peek().kind {
            TokenKind::Integer(n) => {
                let value = n
                    .parse::<u64>()
                    .map_err(|_| self.error_here(format!("{what} out of range: {n}")))?;
                self.advance();
                Ok(value)
            }
            _ => Err(self.unexpected(what)),
        }
    }

    fn string_literal(&mut self, what: &str) -> Result<String> {
        match &self.peek().kind {
            TokenKind::String(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(self.unexpected(what)),
        }
    }

    // ---- statements ----

    fn statement(&mut self) -> Result<Statement> {
        if self.at_kw("CREATE") {
            return Ok(Statement::CreateTable(self.create_table()?));
        }
        if self.eat_kw("EXPLAIN") {
            let analyze = self.eat_kw("ANALYZE");
            if !(self.at_kw("SELECT") || self.at_kw("WITH")) {
                return Err(self.unsupported_here("EXPLAIN of anything but a query"));
            }
            return Ok(Statement::Explain {
                analyze,
                query: Box::new(self.query()?),
            });
        }
        if self.at_kw("SELECT") || self.at_kw("WITH") {
            return Ok(Statement::Query(Box::new(self.query()?)));
        }
        match &self.peek().kind {
            TokenKind::Word(w) => {
                Err(self.unsupported_here(format!("statement {}", w.to_ascii_uppercase())))
            }
            _ => Err(self.unexpected("statement")),
        }
    }

    fn create_table(&mut self) -> Result<CreateTable> {
        self.expect_kw("CREATE")?;
        let or_replace = if self.eat_kw("OR") {
            self.expect_kw("REPLACE")?;
            true
        } else {
            false
        };
        if !self.at_kw("TABLE") {
            return Err(self.unsupported_here("CREATE of anything but TABLE"));
        }
        self.advance();
        if self.at_kw("IF") {
            return Err(self.unsupported_here("CREATE TABLE IF NOT EXISTS"));
        }
        let name = self.ident("table name")?;
        if self.peek().kind == TokenKind::Dot {
            return Err(self.unsupported_here("qualified table name"));
        }
        self.expect(&TokenKind::LParen, "'('")?;
        let mut columns = Vec::new();
        let mut primary_key: Option<Vec<String>> = None;
        loop {
            if self.at_kw("PRIMARY") {
                self.advance();
                self.expect_kw("KEY")?;
                if primary_key.is_some() {
                    return Err(self.error_here("duplicate PRIMARY KEY"));
                }
                primary_key = Some(self.ident_list()?);
            } else {
                columns.push(self.column_def()?);
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "',' or ')'")?;

        let mut engine = None;
        let mut order_by: Option<Vec<String>> = None;
        let mut settings = Vec::new();
        loop {
            if self.eat_kw("ENGINE") {
                self.expect(&TokenKind::Eq, "'='")?;
                engine = Some(self.ident("engine name")?);
                if self.peek().kind == TokenKind::LParen {
                    self.advance();
                    self.expect(&TokenKind::RParen, "')'")?;
                }
            } else if self.at_kw("ORDER") {
                self.advance();
                self.expect_kw("BY")?;
                order_by = Some(if self.peek().kind == TokenKind::LParen {
                    self.ident_list()?
                } else {
                    vec![self.ident("column name")?]
                });
            } else if self.eat_kw("SETTINGS") {
                loop {
                    let key = self.ident("setting name")?;
                    self.expect(&TokenKind::Eq, "'='")?;
                    let value = self.setting_literal()?;
                    settings.push((key, value));
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            } else if self.at_kw("PRIMARY") || self.at_kw("PARTITION") || self.at_kw("TTL") {
                let w = match &self.peek().kind {
                    TokenKind::Word(w) => w.to_ascii_uppercase(),
                    _ => String::new(),
                };
                return Err(self.unsupported_here(format!("{w} clause after the column list")));
            } else {
                break;
            }
        }

        let primary_key = match (primary_key, order_by) {
            (Some(pk), Some(ob)) => {
                let same = pk.len() == ob.len()
                    && pk.iter().zip(&ob).all(|(a, b)| a.eq_ignore_ascii_case(b));
                if !same {
                    return Err(VetaError::unsupported(
                        "PRIMARY KEY and ORDER BY with different columns",
                    ));
                }
                pk
            }
            (Some(pk), None) => pk,
            (None, Some(ob)) => ob,
            (None, None) => Vec::new(),
        };
        Ok(CreateTable {
            or_replace,
            name,
            columns,
            primary_key,
            engine,
            settings,
        })
    }

    fn column_def(&mut self) -> Result<ColumnDef> {
        let name = self.ident("column name")?;
        let data_type = self.data_type()?;
        let nullable = if self.at_kw("NOT") && self.word_at(1, "NULL") {
            self.advance();
            self.advance();
            false
        } else {
            self.eat_kw("NULL");
            true
        };
        if !matches!(self.peek().kind, TokenKind::Comma | TokenKind::RParen) {
            return Err(match &self.peek().kind {
                TokenKind::Word(w) => {
                    self.unsupported_here(format!("column option {}", w.to_ascii_uppercase()))
                }
                _ => self.unexpected("',' or ')'"),
            });
        }
        Ok(ColumnDef {
            name,
            data_type,
            nullable,
        })
    }

    /// A type name of §4: one word (or `DOUBLE PRECISION`) and an optional `(n[, m])`.
    fn data_type(&mut self) -> Result<DataType> {
        let start = self.peek().start;
        let (line, column) = (self.peek().line, self.peek().column);
        match &self.peek().kind {
            TokenKind::Word(_) => {
                self.advance();
            }
            _ => return Err(self.unexpected("type name")),
        }
        if self.at_kw("PRECISION") {
            self.advance();
        }
        if self.peek().kind == TokenKind::LParen {
            self.advance();
            self.unsigned_integer("type argument")?;
            if self.eat(&TokenKind::Comma) {
                self.unsigned_integer("type argument")?;
            }
            self.expect(&TokenKind::RParen, "')'")?;
        }
        let end = self.tokens[self.pos - 1].end;
        let text = &self.src[start..end];
        DataType::parse(text).map_err(|e| match e {
            VetaError::User(msg) => error_at(line, column, msg),
            other => other,
        })
    }

    fn setting_literal(&mut self) -> Result<Literal> {
        let negative = self.eat(&TokenKind::Minus);
        let token = self.advance();
        let lit = match &token.kind {
            TokenKind::Integer(n) => integer_literal(n, negative, &token)?,
            TokenKind::Decimal(n) => decimal_literal(n, negative, &token)?,
            TokenKind::String(s) if !negative => Literal::String(s.clone()),
            TokenKind::Word(w) if !negative && w.eq_ignore_ascii_case("TRUE") => {
                Literal::Boolean(true)
            }
            TokenKind::Word(w) if !negative && w.eq_ignore_ascii_case("FALSE") => {
                Literal::Boolean(false)
            }
            other => {
                return Err(error_at(
                    token.line,
                    token.column,
                    format!("expected setting value, found {}", other.describe()),
                ));
            }
        };
        Ok(lit)
    }

    // ---- queries ----

    fn query(&mut self) -> Result<Query> {
        let mut ctes = Vec::new();
        if self.eat_kw("WITH") {
            if self.at_kw("RECURSIVE") {
                return Err(self.unsupported_here("WITH RECURSIVE"));
            }
            loop {
                let name = self.ident("CTE name")?;
                if self.peek().kind == TokenKind::LParen {
                    return Err(self.unsupported_here("column list in WITH"));
                }
                self.expect_kw("AS")?;
                self.expect(&TokenKind::LParen, "'('")?;
                let query = self.query()?;
                self.expect(&TokenKind::RParen, "')'")?;
                ctes.push(Cte { name, query });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let body = self.select()?;
        for set_op in ["UNION", "EXCEPT", "INTERSECT"] {
            if self.at_kw(set_op) {
                return Err(self.unsupported_here(set_op));
            }
        }
        let mut order_by = Vec::new();
        if self.at_kw("ORDER") {
            self.advance();
            self.expect_kw("BY")?;
            loop {
                order_by.push(self.order_by_item()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = None;
        if self.eat_kw("LIMIT") {
            limit = Some(self.unsigned_integer("LIMIT count")?);
            if self.peek().kind == TokenKind::Comma {
                return Err(self.unsupported_here("LIMIT offset, count"));
            }
        }
        if self.eat_kw("OFFSET") {
            offset = Some(self.unsigned_integer("OFFSET count")?);
        }
        if self.at_kw("LIMIT") && limit.is_none() {
            return Err(self.unsupported_here("LIMIT after OFFSET"));
        }
        Ok(Query {
            ctes,
            body,
            order_by,
            limit,
            offset,
        })
    }

    fn order_by_item(&mut self) -> Result<OrderByItem> {
        let expr = self.expr()?;
        let desc = if self.eat_kw("DESC") {
            true
        } else {
            self.eat_kw("ASC");
            false
        };
        let nulls_first = if self.eat_kw("NULLS") {
            if self.eat_kw("FIRST") {
                Some(true)
            } else if self.eat_kw("LAST") {
                Some(false)
            } else {
                return Err(self.unexpected("FIRST or LAST"));
            }
        } else {
            None
        };
        Ok(OrderByItem {
            expr,
            desc,
            nulls_first,
        })
    }

    fn select(&mut self) -> Result<Select> {
        if self.peek().kind == TokenKind::LParen {
            return Err(self.unsupported_here("parenthesized query body"));
        }
        self.expect_kw("SELECT")?;
        let distinct = self.eat_kw("DISTINCT");
        if self.at_kw("ALL") {
            return Err(self.unsupported_here("SELECT ALL"));
        }
        let mut items = Vec::new();
        loop {
            items.push(self.select_item()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let mut from = Vec::new();
        if self.eat_kw("FROM") {
            loop {
                from.push(self.from_item()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let where_ = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.at_kw("GROUP") {
            self.advance();
            self.expect_kw("BY")?;
            if self.at_kw("ALL") || self.at_kw("ROLLUP") || self.at_kw("CUBE") {
                return Err(self.unsupported_here("GROUP BY ALL/ROLLUP/CUBE"));
            }
            loop {
                group_by.push(self.expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let having = if self.eat_kw("HAVING") {
            Some(self.expr()?)
        } else {
            None
        };
        for clause in ["WINDOW", "QUALIFY", "PREWHERE", "SAMPLE", "FINAL"] {
            if self.at_kw(clause) {
                return Err(self.unsupported_here(clause));
            }
        }
        Ok(Select {
            distinct,
            items,
            from,
            where_,
            group_by,
            having,
        })
    }

    fn select_item(&mut self) -> Result<SelectItem> {
        if self.peek().kind == TokenKind::Star {
            self.advance();
            if self.at_kw("EXCLUDE") || self.at_kw("REPLACE") || self.at_kw("EXCEPT") {
                return Err(self.unsupported_here("* modifiers"));
            }
            return Ok(SelectItem::Wildcard);
        }
        if self.at_ident()
            && self.peek_at(1).kind == TokenKind::Dot
            && self.peek_at(2).kind == TokenKind::Star
        {
            return Err(self.unsupported_here("qualified wildcard"));
        }
        let start = self.peek().start;
        let expr = self.expr()?;
        let end = self.tokens[self.pos - 1].end;
        let text = self.src[start..end].to_string();
        let alias = self.opt_alias()?;
        Ok(SelectItem::Expr { expr, alias, text })
    }

    fn from_item(&mut self) -> Result<TableRef> {
        let mut left = self.table_source()?;
        loop {
            let kind = if self.at_kw("JOIN") {
                self.advance();
                JoinKind::Inner
            } else if self.at_kw("INNER") {
                self.advance();
                self.expect_kw("JOIN")?;
                JoinKind::Inner
            } else if self.at_kw("LEFT") {
                self.advance();
                self.eat_kw("OUTER");
                if self.at_kw("SEMI") || self.at_kw("ANTI") {
                    return Err(self.unsupported_here("LEFT SEMI/ANTI JOIN"));
                }
                self.expect_kw("JOIN")?;
                JoinKind::Left
            } else if self.at_kw("CROSS") {
                self.advance();
                self.expect_kw("JOIN")?;
                JoinKind::Cross
            } else if [
                "RIGHT", "FULL", "NATURAL", "ANTI", "SEMI", "GLOBAL", "ARRAY",
            ]
            .iter()
            .any(|kw| self.at_kw(kw))
            {
                return Err(self.unsupported_here("join type"));
            } else {
                return Ok(left);
            };
            let right = self.table_source()?;
            let on = if kind == JoinKind::Cross {
                None
            } else if self.eat_kw("ON") {
                Some(self.expr()?)
            } else if self.at_kw("USING") {
                return Err(self.unsupported_here("JOIN … USING"));
            } else {
                return Err(self.unexpected("ON"));
            };
            left = TableRef::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                on,
            };
        }
    }

    fn table_source(&mut self) -> Result<TableRef> {
        if self.peek().kind == TokenKind::LParen {
            if !(self.word_at(1, "SELECT") || self.word_at(1, "WITH")) {
                return Err(self.unsupported_here("parenthesized FROM item"));
            }
            self.advance();
            let query = self.query()?;
            self.expect(&TokenKind::RParen, "')'")?;
            self.eat_kw("AS");
            let alias = self.ident("subquery alias")?;
            let column_aliases = if self.peek().kind == TokenKind::LParen {
                self.ident_list()?
            } else {
                Vec::new()
            };
            return Ok(TableRef::Subquery {
                query: Box::new(query),
                alias,
                column_aliases,
            });
        }
        let name = self.ident("table name")?;
        if self.peek().kind == TokenKind::Dot {
            return Err(self.unsupported_here("qualified table name"));
        }
        if self.peek().kind == TokenKind::LParen {
            if !name.eq_ignore_ascii_case("read_parquet") {
                return Err(self.unsupported_here(format!("table function {name}")));
            }
            self.advance();
            let path = self.string_literal("Parquet path string")?;
            if self.peek().kind == TokenKind::Comma {
                return Err(self.unsupported_here("read_parquet options"));
            }
            self.expect(&TokenKind::RParen, "')'")?;
            let alias = self.opt_alias()?;
            return Ok(TableRef::Function {
                name: "read_parquet".into(),
                args: vec![Expr::Literal(Literal::String(path))],
                alias,
            });
        }
        let alias = self.opt_alias()?;
        if self.peek().kind == TokenKind::LParen && alias.is_some() {
            return Err(self.unsupported_here("column aliases on a table"));
        }
        Ok(TableRef::Table { name, alias })
    }

    // ---- expressions ----

    fn expr(&mut self) -> Result<Expr> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut left = self.and_expr()?;
        while self.eat_kw("OR") {
            let right = self.and_expr()?;
            left = binary(BinaryOp::Or, left, right);
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut left = self.not_expr()?;
        while self.eat_kw("AND") {
            let right = self.not_expr()?;
            left = binary(BinaryOp::And, left, right);
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.at_kw("NOT") {
            self.advance();
            if self.at_kw("EXISTS") {
                return self.exists(true);
            }
            let expr = self.not_expr()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr),
            });
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr> {
        let mut left = self.additive()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Eq => Some(BinaryOp::Eq),
                TokenKind::NotEq => Some(BinaryOp::NotEq),
                TokenKind::Lt => Some(BinaryOp::Lt),
                TokenKind::LtEq => Some(BinaryOp::LtEq),
                TokenKind::Gt => Some(BinaryOp::Gt),
                TokenKind::GtEq => Some(BinaryOp::GtEq),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                if self.at_kw("ANY") || self.at_kw("ALL") || self.at_kw("SOME") {
                    return Err(self.unsupported_here("quantified comparison"));
                }
                let right = self.additive()?;
                left = binary(op, left, right);
                continue;
            }
            if self.at_kw("IS") {
                self.advance();
                let negated = self.eat_kw("NOT");
                if !self.eat_kw("NULL") {
                    return Err(if self.at_kw("DISTINCT") {
                        self.unsupported_here("IS [NOT] DISTINCT FROM")
                    } else if self.at_kw("TRUE") || self.at_kw("FALSE") {
                        self.unsupported_here("IS [NOT] TRUE/FALSE")
                    } else {
                        self.unexpected("NULL")
                    });
                }
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                };
                continue;
            }
            let negated = self.at_kw("NOT")
                && (self.word_at(1, "LIKE")
                    || self.word_at(1, "IN")
                    || self.word_at(1, "BETWEEN")
                    || self.word_at(1, "ILIKE"));
            if negated {
                self.advance();
            }
            if self.eat_kw("LIKE") {
                let pattern = self.additive()?;
                if self.at_kw("ESCAPE") {
                    return Err(self.unsupported_here("LIKE … ESCAPE"));
                }
                left = Expr::Like {
                    expr: Box::new(left),
                    pattern: Box::new(pattern),
                    negated,
                };
            } else if self.at_kw("ILIKE") {
                return Err(self.unsupported_here("ILIKE"));
            } else if self.eat_kw("BETWEEN") {
                if self.at_kw("SYMMETRIC") || self.at_kw("ASYMMETRIC") {
                    return Err(self.unsupported_here("BETWEEN SYMMETRIC"));
                }
                let low = self.additive()?;
                self.expect_kw("AND")?;
                let high = self.additive()?;
                left = Expr::Between {
                    expr: Box::new(left),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated,
                };
            } else if self.eat_kw("IN") {
                self.expect(&TokenKind::LParen, "'(' after IN")?;
                if self.at_kw("SELECT") || self.at_kw("WITH") {
                    let query = self.query()?;
                    self.expect(&TokenKind::RParen, "')'")?;
                    left = Expr::InSubquery {
                        expr: Box::new(left),
                        query: Box::new(query),
                        negated,
                    };
                } else {
                    let mut list = vec![self.expr()?];
                    while self.eat(&TokenKind::Comma) {
                        list.push(self.expr()?);
                    }
                    self.expect(&TokenKind::RParen, "',' or ')'")?;
                    left = Expr::InList {
                        expr: Box::new(left),
                        list,
                        negated,
                    };
                }
            } else {
                return Ok(left);
            }
        }
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.multiplicative()?;
            left = binary(op, left, right);
        }
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Mod,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.unary()?;
            left = binary(op, left, right);
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat(&TokenKind::Plus) {
            return self.unary();
        }
        if self.peek().kind == TokenKind::Minus {
            self.advance();
            // A minus directly before a number is part of the literal, so that
            // -9223372036854775808 is a valid BIGINT.
            let token = self.peek().clone();
            match &token.kind {
                TokenKind::Integer(n) => {
                    self.advance();
                    return Ok(Expr::Literal(integer_literal(n, true, &token)?));
                }
                TokenKind::Decimal(n) => {
                    self.advance();
                    return Ok(Expr::Literal(decimal_literal(n, true, &token)?));
                }
                _ => {}
            }
            let expr = self.unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(expr),
            });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        let token = self.peek().clone();
        match &token.kind {
            TokenKind::Integer(n) => {
                self.advance();
                Ok(Expr::Literal(integer_literal(n, false, &token)?))
            }
            TokenKind::Decimal(n) => {
                self.advance();
                Ok(Expr::Literal(decimal_literal(n, false, &token)?))
            }
            TokenKind::String(s) => {
                self.advance();
                Ok(Expr::Literal(Literal::String(s.clone())))
            }
            TokenKind::LParen => {
                self.advance();
                if self.at_kw("SELECT") || self.at_kw("WITH") {
                    let query = self.query()?;
                    self.expect(&TokenKind::RParen, "')'")?;
                    return Ok(Expr::ScalarSubquery(Box::new(query)));
                }
                let expr = self.expr()?;
                if self.peek().kind == TokenKind::Comma {
                    return Err(self.unsupported_here("row value"));
                }
                self.expect(&TokenKind::RParen, "')'")?;
                Ok(expr)
            }
            TokenKind::QuotedIdent(_) => self.column_ref(),
            TokenKind::Word(w) => {
                let upper = w.to_ascii_uppercase();
                let next_is_paren = self.peek_at(1).kind == TokenKind::LParen;
                let next_is_string = matches!(self.peek_at(1).kind, TokenKind::String(_));
                match upper.as_str() {
                    "NULL" => {
                        self.advance();
                        Ok(Expr::Literal(Literal::Null))
                    }
                    "TRUE" => {
                        self.advance();
                        Ok(Expr::Literal(Literal::Boolean(true)))
                    }
                    "FALSE" => {
                        self.advance();
                        Ok(Expr::Literal(Literal::Boolean(false)))
                    }
                    "DATE" | "TIMESTAMP" if next_is_string => self.typed_literal(&upper),
                    "INTERVAL" => Err(self.unsupported_here("INTERVAL")),
                    "CASE" => self.case_expr(),
                    "CAST" if next_is_paren => self.cast_expr(),
                    "EXISTS" => self.exists(false),
                    "EXTRACT" if next_is_paren => self.extract_expr(),
                    "SUBSTRING" if next_is_paren => self.substring_expr(),
                    "DATE_TRUNC" if next_is_paren => self.date_trunc_expr(),
                    _ if is_reserved(w) => Err(self.unexpected("expression")),
                    _ if next_is_paren => self.function_call(),
                    _ => self.column_ref(),
                }
            }
            _ => Err(self.unexpected("expression")),
        }
    }

    fn column_ref(&mut self) -> Result<Expr> {
        let first = self.ident("column name")?;
        if !self.eat(&TokenKind::Dot) {
            return Ok(Expr::Column {
                table: None,
                name: first,
            });
        }
        if self.peek().kind == TokenKind::Star {
            return Err(self.unsupported_here("qualified wildcard"));
        }
        let name = self.ident("column name")?;
        if self.peek().kind == TokenKind::Dot {
            return Err(self.unsupported_here("column reference with more than two parts"));
        }
        Ok(Expr::Column {
            table: Some(first),
            name,
        })
    }

    fn typed_literal(&mut self, kind: &str) -> Result<Expr> {
        self.advance();
        let token = self.advance();
        let TokenKind::String(text) = &token.kind else {
            return Err(error_at(
                token.line,
                token.column,
                "expected string literal",
            ));
        };
        let located = |e: VetaError| match e {
            VetaError::User(msg) => error_at(token.line, token.column, msg),
            other => other,
        };
        let lit = if kind == "DATE" {
            Literal::Date(datetime::parse_date(text.as_bytes()).map_err(located)?)
        } else {
            Literal::Timestamp(datetime::parse_timestamp(text.as_bytes()).map_err(located)?)
        };
        Ok(Expr::Literal(lit))
    }

    fn case_expr(&mut self) -> Result<Expr> {
        self.advance();
        if !self.at_kw("WHEN") {
            return Err(self.unsupported_here("simple CASE (CASE operand WHEN …)"));
        }
        let mut branches = Vec::new();
        while self.eat_kw("WHEN") {
            let cond = self.expr()?;
            self.expect_kw("THEN")?;
            let value = self.expr()?;
            branches.push((cond, value));
        }
        let else_ = if self.eat_kw("ELSE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw("END")?;
        Ok(Expr::Case { branches, else_ })
    }

    fn cast_expr(&mut self) -> Result<Expr> {
        self.advance();
        self.expect(&TokenKind::LParen, "'('")?;
        let expr = self.expr()?;
        self.expect_kw("AS")?;
        let data_type = self.data_type()?;
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            data_type,
        })
    }

    fn exists(&mut self, negated: bool) -> Result<Expr> {
        self.expect_kw("EXISTS")?;
        self.expect(&TokenKind::LParen, "'(' after EXISTS")?;
        if !(self.at_kw("SELECT") || self.at_kw("WITH")) {
            return Err(self.unexpected("subquery"));
        }
        let query = self.query()?;
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(Expr::Exists {
            query: Box::new(query),
            negated,
        })
    }

    fn extract_expr(&mut self) -> Result<Expr> {
        self.advance();
        self.expect(&TokenKind::LParen, "'('")?;
        let token = self.peek().clone();
        let part = match &token.kind {
            TokenKind::Word(w) | TokenKind::String(w) => DatePart::parse(w).map_err(|_| {
                error_at(
                    token.line,
                    token.column,
                    format!("unsupported: extract({w} FROM …)"),
                )
            })?,
            _ => return Err(self.unexpected("date part")),
        };
        self.advance();
        self.expect_kw("FROM")?;
        let expr = self.expr()?;
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(Expr::Extract {
            part,
            expr: Box::new(expr),
        })
    }

    fn substring_expr(&mut self) -> Result<Expr> {
        self.advance();
        self.expect(&TokenKind::LParen, "'('")?;
        let expr = self.expr()?;
        let (from, length) = if self.eat_kw("FROM") {
            let from = self.expr()?;
            let length = if self.eat_kw("FOR") {
                Some(self.expr()?)
            } else {
                None
            };
            (from, length)
        } else {
            self.expect(&TokenKind::Comma, "',' or FROM")?;
            let from = self.expr()?;
            let length = if self.eat(&TokenKind::Comma) {
                Some(self.expr()?)
            } else {
                None
            };
            (from, length)
        };
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(Expr::Substring {
            expr: Box::new(expr),
            from: Box::new(from),
            length: length.map(Box::new),
        })
    }

    fn date_trunc_expr(&mut self) -> Result<Expr> {
        self.advance();
        self.expect(&TokenKind::LParen, "'('")?;
        let token = self.peek().clone();
        let unit_name = self.string_literal("DATE_TRUNC unit string")?;
        let unit = TruncUnit::parse(&unit_name).map_err(|e| match e {
            VetaError::User(msg) => error_at(token.line, token.column, msg),
            other => other,
        })?;
        self.expect(&TokenKind::Comma, "','")?;
        let expr = self.expr()?;
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(Expr::DateTrunc {
            unit,
            expr: Box::new(expr),
        })
    }

    fn function_call(&mut self) -> Result<Expr> {
        let token = self.advance();
        let TokenKind::Word(name) = &token.kind else {
            return Err(error_at(token.line, token.column, "expected function name"));
        };
        let func = Function::from_name(name).ok_or_else(|| {
            error_at(
                token.line,
                token.column,
                format!("unsupported: function {name}"),
            )
        })?;
        self.expect(&TokenKind::LParen, "'('")?;
        if func == Function::Count && self.peek().kind == TokenKind::Star {
            self.advance();
            self.expect(&TokenKind::RParen, "')'")?;
            return Ok(Expr::CountStar);
        }
        let distinct = self.eat_kw("DISTINCT");
        if distinct && func != Function::Count {
            return Err(error_at(
                token.line,
                token.column,
                format!("unsupported: {}(DISTINCT …)", func.name()),
            ));
        }
        if self.at_kw("ALL") {
            return Err(self.unsupported_here("aggregate ALL"));
        }
        let mut args = Vec::new();
        if self.peek().kind != TokenKind::RParen {
            args.push(self.expr()?);
            while self.eat(&TokenKind::Comma) {
                args.push(self.expr()?);
            }
        }
        if self.at_kw("ORDER") {
            return Err(self.unsupported_here("ORDER BY inside a function call"));
        }
        self.expect(&TokenKind::RParen, "')'")?;
        let (min, max) = func.arity();
        if args.len() < min || args.len() > max {
            return Err(error_at(
                token.line,
                token.column,
                format!(
                    "{} expects {} argument(s), got {}",
                    func.name(),
                    if min == max {
                        min.to_string()
                    } else {
                        format!("{min} to {max}")
                    },
                    args.len()
                ),
            ));
        }
        if self.at_kw("OVER") || self.at_kw("FILTER") {
            return Err(self.unsupported_here("window functions and FILTER"));
        }
        Ok(Expr::Function {
            func,
            distinct,
            args,
        })
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn integer_literal(digits: &str, negative: bool, token: &Token) -> Result<Literal> {
    let text = if negative {
        format!("-{digits}")
    } else {
        digits.to_string()
    };
    text.parse::<i64>().map(Literal::Integer).map_err(|_| {
        error_at(
            token.line,
            token.column,
            format!("integer literal out of BIGINT range: {text}"),
        )
    })
}

fn decimal_literal(digits: &str, negative: bool, token: &Token) -> Result<Literal> {
    let text = if negative {
        format!("-{digits}")
    } else {
        digits.to_string()
    };
    decimal::parse_literal(&text)
        .map(Literal::Decimal)
        .map_err(|e| match e {
            VetaError::User(msg) => error_at(token.line, token.column, msg),
            other => other,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(sql: &str) -> Query {
        match parse_statement(sql).unwrap() {
            Statement::Query(q) => *q,
            other => panic!("expected query, got {other:?}"),
        }
    }

    fn select(sql: &str) -> Select {
        query(sql).body
    }

    fn expr(sql: &str) -> Expr {
        parse_expr(sql).unwrap()
    }

    fn col(name: &str) -> Expr {
        Expr::Column {
            table: None,
            name: name.into(),
        }
    }

    fn qcol(table: &str, name: &str) -> Expr {
        Expr::Column {
            table: Some(table.into()),
            name: name.into(),
        }
    }

    fn int(v: i64) -> Expr {
        Expr::Literal(Literal::Integer(v))
    }

    fn string(s: &str) -> Expr {
        Expr::Literal(Literal::String(s.into()))
    }

    fn bin(op: BinaryOp, l: Expr, r: Expr) -> Expr {
        binary(op, l, r)
    }

    fn user_error(sql: &str) -> String {
        match parse_statements(sql) {
            Err(VetaError::User(m)) => m,
            other => panic!("expected user error for {sql:?}, got {other:?}"),
        }
    }

    fn item_expr(item: &SelectItem) -> &Expr {
        match item {
            SelectItem::Expr { expr, .. } => expr,
            SelectItem::Wildcard => panic!("wildcard"),
        }
    }

    // ---- literals ----

    #[test]
    fn literals() {
        assert_eq!(expr("42"), int(42));
        assert_eq!(expr("-9223372036854775808"), int(i64::MIN));
        assert!(parse_expr("9223372036854775808").is_err());
        assert_eq!(
            expr("0.05"),
            Expr::Literal(Literal::Decimal(decimal::DecimalLiteral {
                value: 5,
                precision: 3,
                scale: 2
            }))
        );
        assert_eq!(
            expr("-1.50"),
            Expr::Literal(Literal::Decimal(decimal::DecimalLiteral {
                value: -150,
                precision: 3,
                scale: 2
            }))
        );
        assert_eq!(expr("'it''s'"), string("it's"));
        assert_eq!(expr("NULL"), Expr::Literal(Literal::Null));
        assert_eq!(expr("true"), Expr::Literal(Literal::Boolean(true)));
        assert_eq!(expr("FALSE"), Expr::Literal(Literal::Boolean(false)));
    }

    #[test]
    fn date_and_timestamp_literals() {
        assert_eq!(
            expr("date '1995-09-01'"),
            Expr::Literal(Literal::Date(datetime::parse_date(b"1995-09-01").unwrap()))
        );
        assert_eq!(
            expr("TIMESTAMP '2013-07-01 10:00:00'"),
            Expr::Literal(Literal::Timestamp(
                datetime::parse_timestamp(b"2013-07-01 10:00:00").unwrap()
            ))
        );
        assert!(user_error("SELECT DATE '1995-13-01'").contains("line 1, column 13"));
    }

    #[test]
    fn columns_qualified_and_quoted() {
        assert_eq!(expr("n1.n_name"), qcol("n1", "n_name"));
        assert_eq!(expr("\"Weird Name\""), col("Weird Name"));
        assert_eq!(expr("t.\"x\""), qcol("t", "x"));
        assert!(user_error("SELECT a.b.c FROM t").contains("unsupported"));
    }

    // ---- operators and precedence ----

    #[test]
    fn arithmetic_precedence() {
        assert_eq!(
            expr("a + b * c - d % 2 / e"),
            bin(
                BinaryOp::Sub,
                bin(
                    BinaryOp::Add,
                    col("a"),
                    bin(BinaryOp::Mul, col("b"), col("c"))
                ),
                bin(
                    BinaryOp::Div,
                    bin(BinaryOp::Mod, col("d"), int(2)),
                    col("e")
                )
            )
        );
        assert_eq!(
            expr("-a * b"),
            bin(
                BinaryOp::Mul,
                Expr::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(col("a"))
                },
                col("b")
            )
        );
        assert_eq!(
            expr("x * (1 - y)"),
            bin(
                BinaryOp::Mul,
                col("x"),
                bin(BinaryOp::Sub, int(1), col("y"))
            )
        );
    }

    #[test]
    fn logical_precedence() {
        // OR < AND < NOT < comparison.
        assert_eq!(
            expr("a = 1 OR NOT b < 2 AND c <> 3"),
            bin(
                BinaryOp::Or,
                bin(BinaryOp::Eq, col("a"), int(1)),
                bin(
                    BinaryOp::And,
                    Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(bin(BinaryOp::Lt, col("b"), int(2)))
                    },
                    bin(BinaryOp::NotEq, col("c"), int(3))
                )
            )
        );
        assert_eq!(expr("a != b"), bin(BinaryOp::NotEq, col("a"), col("b")));
        assert_eq!(
            expr("a + 1 >= b * 2"),
            bin(
                BinaryOp::GtEq,
                bin(BinaryOp::Add, col("a"), int(1)),
                bin(BinaryOp::Mul, col("b"), int(2))
            )
        );
        for (sql, op) in [
            ("a < b", BinaryOp::Lt),
            ("a <= b", BinaryOp::LtEq),
            ("a > b", BinaryOp::Gt),
            ("a <> b", BinaryOp::NotEq),
        ] {
            assert_eq!(expr(sql), bin(op, col("a"), col("b")));
        }
    }

    #[test]
    fn between_binds_its_own_and() {
        assert_eq!(
            expr("x BETWEEN 0.05 AND 0.07 AND y < 24"),
            bin(
                BinaryOp::And,
                Expr::Between {
                    expr: Box::new(col("x")),
                    low: Box::new(expr("0.05")),
                    high: Box::new(expr("0.07")),
                    negated: false
                },
                bin(BinaryOp::Lt, col("y"), int(24))
            )
        );
        assert!(matches!(
            expr("x NOT BETWEEN 1 AND 1 + 10"),
            Expr::Between { negated: true, .. }
        ));
    }

    #[test]
    fn is_null() {
        assert_eq!(
            expr("a + 1 IS NULL"),
            Expr::IsNull {
                expr: Box::new(bin(BinaryOp::Add, col("a"), int(1))),
                negated: false
            }
        );
        assert_eq!(
            expr("a IS NOT NULL"),
            Expr::IsNull {
                expr: Box::new(col("a")),
                negated: true
            }
        );
        assert_eq!(
            expr("NOT a IS NULL"),
            Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(Expr::IsNull {
                    expr: Box::new(col("a")),
                    negated: false
                })
            }
        );
    }

    #[test]
    fn like_and_not_like() {
        assert_eq!(
            expr("URL LIKE '%google%'"),
            Expr::Like {
                expr: Box::new(col("URL")),
                pattern: Box::new(string("%google%")),
                negated: false
            }
        );
        assert!(matches!(
            expr("p_type NOT LIKE 'MEDIUM POLISHED%'"),
            Expr::Like { negated: true, .. }
        ));
        assert!(user_error("SELECT a FROM t WHERE a LIKE 'x' ESCAPE '!'").contains("unsupported"));
    }

    #[test]
    fn in_list() {
        assert_eq!(
            expr("TraficSourceID IN (-1, 6)"),
            Expr::InList {
                expr: Box::new(col("TraficSourceID")),
                list: vec![int(-1), int(6)],
                negated: false
            }
        );
        assert!(matches!(
            expr("a NOT IN ('x', 'y')"),
            Expr::InList { negated: true, .. }
        ));
    }

    #[test]
    fn in_subquery() {
        match expr("ps_suppkey NOT IN (SELECT s_suppkey FROM supplier)") {
            Expr::InSubquery {
                expr,
                query,
                negated: true,
            } => {
                assert_eq!(*expr, col("ps_suppkey"));
                assert_eq!(query.body.items.len(), 1);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            expr("o IN (SELECT k FROM l GROUP BY k HAVING sum(q) > 300)"),
            Expr::InSubquery { negated: false, .. }
        ));
    }

    #[test]
    fn exists_and_not_exists() {
        assert!(matches!(
            expr("EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey)"),
            Expr::Exists { negated: false, .. }
        ));
        assert!(matches!(
            expr("NOT EXISTS (SELECT * FROM orders)"),
            Expr::Exists { negated: true, .. }
        ));
        assert!(matches!(
            expr("a = 1 AND NOT EXISTS (SELECT * FROM t)"),
            Expr::Binary {
                op: BinaryOp::And,
                ..
            }
        ));
    }

    #[test]
    fn scalar_subquery() {
        match expr("ps_supplycost = (SELECT min(ps_supplycost) FROM partsupp)") {
            Expr::Binary {
                op: BinaryOp::Eq,
                right,
                ..
            } => assert!(matches!(*right, Expr::ScalarSubquery(_))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn case_when() {
        assert_eq!(
            expr("CASE WHEN a = 0 THEN b ELSE '' END"),
            Expr::Case {
                branches: vec![(bin(BinaryOp::Eq, col("a"), int(0)), col("b"))],
                else_: Some(Box::new(string("")))
            }
        );
        assert_eq!(
            expr("case when a then 1 when b then 2 end"),
            Expr::Case {
                branches: vec![(col("a"), int(1)), (col("b"), int(2))],
                else_: None
            }
        );
        assert!(user_error("SELECT CASE a WHEN 1 THEN 2 END").contains("unsupported"));
    }

    #[test]
    fn cast() {
        assert_eq!(
            expr("CAST('1998-09-02' AS date)"),
            Expr::Cast {
                expr: Box::new(string("1998-09-02")),
                data_type: DataType::Date
            }
        );
        assert_eq!(
            expr("cast(x AS DECIMAL(15, 2))"),
            Expr::Cast {
                expr: Box::new(col("x")),
                data_type: DataType::Decimal {
                    precision: 15,
                    scale: 2
                }
            }
        );
        assert_eq!(
            expr("CAST(x AS double precision)"),
            Expr::Cast {
                expr: Box::new(col("x")),
                data_type: DataType::Float64
            }
        );
    }

    #[test]
    fn extract() {
        assert_eq!(
            expr("extract(year FROM l_shipdate)"),
            Expr::Extract {
                part: DatePart::Year,
                expr: Box::new(col("l_shipdate"))
            }
        );
        assert_eq!(
            expr("EXTRACT(MINUTE FROM EventTime)"),
            Expr::Extract {
                part: DatePart::Minute,
                expr: Box::new(col("EventTime"))
            }
        );
        assert!(user_error("SELECT extract(week FROM d)").contains("unsupported"));
    }

    #[test]
    fn substring_both_forms() {
        let standard = Expr::Substring {
            expr: Box::new(col("c_phone")),
            from: Box::new(int(1)),
            length: Some(Box::new(int(2))),
        };
        assert_eq!(expr("substring(c_phone FROM 1 FOR 2)"), standard);
        assert_eq!(expr("SUBSTRING(c_phone, 1, 2)"), standard);
        assert_eq!(
            expr("substring(s FROM 3)"),
            Expr::Substring {
                expr: Box::new(col("s")),
                from: Box::new(int(3)),
                length: None
            }
        );
        assert_eq!(expr("substring(s, 3)"), expr("substring(s FROM 3)"));
    }

    #[test]
    fn scalar_functions() {
        assert_eq!(
            expr("length(URL)"),
            Expr::Function {
                func: Function::Length,
                distinct: false,
                args: vec![col("URL")]
            }
        );
        assert!(matches!(
            expr("STRLEN(URL)"),
            Expr::Function {
                func: Function::Strlen,
                ..
            }
        ));
        assert_eq!(
            expr(r"REGEXP_REPLACE(Referer, '^https?://(?:www\.)?([^/]+)/.*$', '\1')"),
            Expr::Function {
                func: Function::RegexpReplace,
                distinct: false,
                args: vec![
                    col("Referer"),
                    string(r"^https?://(?:www\.)?([^/]+)/.*$"),
                    string(r"\1")
                ]
            }
        );
        assert_eq!(
            expr("DATE_TRUNC('minute', EventTime)"),
            Expr::DateTrunc {
                unit: TruncUnit::Minute,
                expr: Box::new(col("EventTime"))
            }
        );
        assert!(user_error("SELECT DATE_TRUNC('week', t)").contains("unsupported"));
        assert!(user_error("SELECT upper(x)").contains("unsupported: function upper"));
        assert!(user_error("SELECT length(a, b)").contains("expects 1"));
    }

    #[test]
    fn aggregates() {
        assert_eq!(expr("COUNT(*)"), Expr::CountStar);
        assert_eq!(
            expr("count(DISTINCT UserID)"),
            Expr::Function {
                func: Function::Count,
                distinct: true,
                args: vec![col("UserID")]
            }
        );
        for (name, func) in [
            ("SUM", Function::Sum),
            ("avg", Function::Avg),
            ("Min", Function::Min),
            ("MAX", Function::Max),
        ] {
            assert_eq!(
                expr(&format!("{name}(x + 1)")),
                Expr::Function {
                    func,
                    distinct: false,
                    args: vec![bin(BinaryOp::Add, col("x"), int(1))]
                }
            );
        }
        assert!(user_error("SELECT SUM(DISTINCT x) FROM t").contains("unsupported"));
    }

    // ---- queries ----

    #[test]
    fn select_items_and_aliases() {
        let s = select("SELECT DISTINCT *, a AS x, b y, sum(c)   FROM t");
        assert!(s.distinct);
        assert_eq!(s.items[0], SelectItem::Wildcard);
        assert_eq!(
            s.items[1],
            SelectItem::Expr {
                expr: col("a"),
                alias: Some("x".into()),
                text: "a".into()
            }
        );
        assert_eq!(
            s.items[2],
            SelectItem::Expr {
                expr: col("b"),
                alias: Some("y".into()),
                text: "b".into()
            }
        );
        match &s.items[3] {
            SelectItem::Expr { alias, text, .. } => {
                assert_eq!(alias, &None);
                assert_eq!(text, "sum(c)");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn select_without_from() {
        let s = select("SELECT 1");
        assert!(s.from.is_empty());
        assert_eq!(item_expr(&s.items[0]), &int(1));
    }

    #[test]
    fn from_table_with_alias() {
        let s = select("SELECT 1 FROM nation n1, nation AS n2, lineitem");
        assert_eq!(
            s.from,
            vec![
                TableRef::Table {
                    name: "nation".into(),
                    alias: Some("n1".into())
                },
                TableRef::Table {
                    name: "nation".into(),
                    alias: Some("n2".into())
                },
                TableRef::Table {
                    name: "lineitem".into(),
                    alias: None
                },
            ]
        );
    }

    #[test]
    fn from_subquery_with_column_aliases() {
        let s = select(
            "SELECT c_count FROM (SELECT c_custkey, count(o_orderkey) FROM customer \
             GROUP BY c_custkey) AS c_orders (c_custkey, c_count)",
        );
        match &s.from[0] {
            TableRef::Subquery {
                alias,
                column_aliases,
                query,
            } => {
                assert_eq!(alias, "c_orders");
                assert_eq!(
                    column_aliases,
                    &vec!["c_custkey".to_string(), "c_count".into()]
                );
                assert_eq!(query.body.group_by, vec![col("c_custkey")]);
            }
            other => panic!("{other:?}"),
        }
        let s = select("SELECT x FROM (SELECT 1 AS x) sub");
        assert!(
            matches!(&s.from[0], TableRef::Subquery { alias, column_aliases, .. }
            if alias == "sub" && column_aliases.is_empty())
        );
        assert!(parse_statement("SELECT x FROM (SELECT 1 AS x)").is_err());
    }

    #[test]
    fn from_read_parquet() {
        let s = select("SELECT * FROM read_parquet('/tmp/x.parquet') AS p");
        assert_eq!(
            s.from[0],
            TableRef::Function {
                name: "read_parquet".into(),
                args: vec![string("/tmp/x.parquet")],
                alias: Some("p".into())
            }
        );
        let s = select("SELECT * FROM READ_PARQUET('a.parquet')");
        assert!(matches!(&s.from[0], TableRef::Function { alias: None, .. }));
        assert!(user_error("SELECT * FROM generate_series(1, 3)").contains("unsupported"));
    }

    #[test]
    fn joins() {
        let s = select(
            "SELECT 1 FROM a JOIN b ON a.x = b.x INNER JOIN c ON c.y = b.y \
             LEFT OUTER JOIN d ON d.z = c.z AND d.w <> 1 LEFT JOIN e ON 1 = 1 CROSS JOIN f",
        );
        assert_eq!(s.from.len(), 1);
        let mut kinds = Vec::new();
        let mut node = &s.from[0];
        while let TableRef::Join {
            left,
            right,
            kind,
            on,
        } = node
        {
            kinds.push(*kind);
            assert_eq!(on.is_none(), *kind == JoinKind::Cross);
            assert!(matches!(**right, TableRef::Table { .. }));
            node = &**left;
        }
        assert_eq!(
            kinds,
            vec![
                JoinKind::Cross,
                JoinKind::Left,
                JoinKind::Left,
                JoinKind::Inner,
                JoinKind::Inner
            ]
        );
        assert!(user_error("SELECT 1 FROM a RIGHT JOIN b ON 1 = 1").contains("unsupported"));
        assert!(user_error("SELECT 1 FROM a FULL OUTER JOIN b ON 1 = 1").contains("unsupported"));
        assert!(user_error("SELECT 1 FROM a JOIN b").contains("expected ON"));
    }

    #[test]
    fn with_ctes() {
        let q = query(
            "WITH revenue AS (SELECT l_suppkey AS supplier_no FROM lineitem), \
             r2 AS (SELECT 1) SELECT * FROM revenue",
        );
        assert_eq!(q.ctes.len(), 2);
        assert_eq!(q.ctes[0].name, "revenue");
        assert_eq!(q.ctes[1].name, "r2");
        assert!(matches!(&q.body.from[0], TableRef::Table { name, .. } if name == "revenue"));
    }

    #[test]
    fn where_group_by_having() {
        let s = select(
            "SELECT CounterID, COUNT(*) FROM hits WHERE URL <> '' \
             GROUP BY CounterID HAVING COUNT(*) > 100000",
        );
        assert_eq!(s.where_, Some(bin(BinaryOp::NotEq, col("URL"), string(""))));
        assert_eq!(s.group_by, vec![col("CounterID")]);
        assert_eq!(
            s.having,
            Some(bin(BinaryOp::Gt, Expr::CountStar, int(100000)))
        );
    }

    #[test]
    fn group_by_positional_and_alias() {
        let s = select("SELECT 1, URL, COUNT(*) AS c FROM hits GROUP BY 1, URL");
        assert_eq!(s.group_by, vec![int(1), col("URL")]);
        let s = select("SELECT extract(minute FROM EventTime) AS m, COUNT(*) FROM hits GROUP BY m");
        assert_eq!(s.group_by, vec![col("m")]);
    }

    #[test]
    fn order_by_directions_and_nulls() {
        let q = query("SELECT a, b FROM t ORDER BY a DESC NULLS LAST, b, 2 ASC NULLS FIRST");
        assert_eq!(
            q.order_by,
            vec![
                OrderByItem {
                    expr: col("a"),
                    desc: true,
                    nulls_first: Some(false)
                },
                OrderByItem {
                    expr: col("b"),
                    desc: false,
                    nulls_first: None
                },
                OrderByItem {
                    expr: int(2),
                    desc: false,
                    nulls_first: Some(true)
                },
            ]
        );
    }

    #[test]
    fn limit_and_offset() {
        let q = query("SELECT URL FROM hits ORDER BY c DESC LIMIT 10 OFFSET 1000");
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.offset, Some(1000));
        let q = query("SELECT 1 OFFSET 5");
        assert_eq!((q.limit, q.offset), (None, Some(5)));
        assert!(user_error("SELECT 1 LIMIT 5, 10").contains("unsupported"));
        assert!(parse_statement("SELECT 1 LIMIT -1").is_err());
    }

    #[test]
    fn subquery_with_order_and_limit() {
        let s = select("SELECT x FROM (SELECT x FROM t ORDER BY x LIMIT 3) s");
        match &s.from[0] {
            TableRef::Subquery { query, .. } => assert_eq!(query.limit, Some(3)),
            other => panic!("{other:?}"),
        }
    }

    // ---- statements ----

    #[test]
    fn explain() {
        assert!(matches!(
            parse_statement("EXPLAIN SELECT 1").unwrap(),
            Statement::Explain { analyze: false, .. }
        ));
        assert!(matches!(
            parse_statement("explain analyze WITH a AS (SELECT 1) SELECT * FROM a").unwrap(),
            Statement::Explain { analyze: true, .. }
        ));
        assert!(user_error("EXPLAIN CREATE TABLE t (a INT)").contains("unsupported"));
    }

    #[test]
    fn create_or_replace_table_clickhouse_style() {
        let sql = "CREATE OR REPLACE TABLE hits\n(\n    WatchID BIGINT NOT NULL,\n    \
                   Title TEXT NOT NULL,\n    EventDate Date NOT NULL,\n    Note VARCHAR(10),\n    \
                   Price DECIMAL(15,2) NULL,\n    \
                   PRIMARY KEY (CounterID, EventDate)\n)\nENGINE = MergeTree\n\
                   SETTINGS a = 1, -- trailing comment\n    b = ''; -- another comment\n";
        let statements = parse_statements(sql).unwrap();
        assert_eq!(statements.len(), 1);
        let Statement::CreateTable(ct) = &statements[0] else {
            panic!("{:?}", statements[0]);
        };
        assert!(ct.or_replace);
        assert_eq!(ct.name, "hits");
        assert_eq!(
            ct.columns,
            vec![
                ColumnDef {
                    name: "WatchID".into(),
                    data_type: DataType::Int64,
                    nullable: false
                },
                ColumnDef {
                    name: "Title".into(),
                    data_type: DataType::Varchar,
                    nullable: false
                },
                ColumnDef {
                    name: "EventDate".into(),
                    data_type: DataType::Date,
                    nullable: false
                },
                ColumnDef {
                    name: "Note".into(),
                    data_type: DataType::Varchar,
                    nullable: true
                },
                ColumnDef {
                    name: "Price".into(),
                    data_type: DataType::Decimal {
                        precision: 15,
                        scale: 2
                    },
                    nullable: true
                },
            ]
        );
        assert_eq!(
            ct.primary_key,
            vec!["CounterID".to_string(), "EventDate".into()]
        );
        assert_eq!(ct.engine.as_deref(), Some("MergeTree"));
        assert_eq!(
            ct.settings,
            vec![
                ("a".to_string(), Literal::Integer(1)),
                ("b".to_string(), Literal::String(String::new())),
            ]
        );
    }

    #[test]
    fn create_table_order_by_and_plain() {
        let Statement::CreateTable(ct) =
            parse_statement("create table t (a INTEGER, b DOUBLE) ORDER BY (b, a)").unwrap()
        else {
            panic!()
        };
        assert!(!ct.or_replace);
        assert_eq!(ct.primary_key, vec!["b".to_string(), "a".into()]);
        assert!(ct.columns.iter().all(|c| c.nullable));
        let Statement::CreateTable(ct) =
            parse_statement("CREATE TABLE t (a INT) ENGINE = MergeTree ORDER BY a").unwrap()
        else {
            panic!()
        };
        assert_eq!(ct.primary_key, vec!["a".to_string()]);
        assert!(
            user_error("CREATE TABLE t (a INT, PRIMARY KEY (a)) ORDER BY (b)")
                .contains("unsupported")
        );
        assert!(user_error("CREATE TABLE t (a INT128)").contains("unsupported"));
        assert!(user_error("CREATE TABLE t (a INT DEFAULT 1)").contains("unsupported"));
    }

    #[test]
    fn multiple_statements() {
        let statements =
            parse_statements(";SELECT 1; SELECT 2;;\nCREATE TABLE t (a INT);").unwrap();
        assert_eq!(statements.len(), 3);
        assert!(parse_statement("SELECT 1; SELECT 2").is_err());
        assert!(parse_statements("SELECT 1 SELECT 2").is_err());
        assert!(parse_statements("").unwrap().is_empty());
    }

    #[test]
    fn errors_have_line_and_column() {
        let msg = user_error("SELECT a,\n  FROM t");
        assert!(msg.contains("line 2, column 3"), "{msg}");
        let msg = user_error("SELECT (a + 1 FROM t");
        assert!(msg.contains("line 1, column 15"), "{msg}");
        assert!(msg.contains("expected ')'"), "{msg}");
    }

    #[test]
    fn unsupported_constructs() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "SELECT 1 UNION SELECT 2",
            "SELECT a FROM t WHERE a ILIKE 'x'",
            "SELECT INTERVAL '1' DAY",
            "SELECT t.* FROM t",
            "SELECT a FROM t WHERE a = ANY (SELECT 1)",
            "SELECT a FROM t WHERE a IS DISTINCT FROM b",
            "SELECT a FROM (t)",
        ] {
            let msg = user_error(sql);
            assert!(msg.contains("unsupported"), "{sql}: {msg}");
        }
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            query("select A from T where B is not null order by A desc limit 1"),
            query("SELECT A FROM T WHERE B IS NOT NULL ORDER BY A DESC LIMIT 1")
        );
    }

    // ---- reference workloads ----

    fn data_dir() -> std::path::PathBuf {
        match std::env::var_os("VETA_DATA") {
            Some(dir) => std::path::PathBuf::from(dir),
            None => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.veta-data"),
        }
    }

    /// Parses the pinned ClickBench queries and DDL and the 22 TPC-H queries when they
    /// have been fetched; does nothing otherwise.
    #[test]
    fn reference_workloads_parse() {
        let data = data_dir();
        let queries = data.join("clickbench/queries-clickhouse.sql");
        let create = data.join("clickbench/create-clickhouse.sql");
        let tpch: Vec<_> = (1..=22)
            .map(|i| data.join(format!("tpch/queries/q{i:02}.sql")))
            .collect();
        if !queries.exists() || !create.exists() || !tpch.iter().all(|p| p.exists()) {
            return;
        }

        let text = std::fs::read_to_string(&queries).unwrap();
        let statements = parse_statements(&text).unwrap();
        assert_eq!(statements.len(), 43);
        assert!(statements.iter().all(|s| matches!(s, Statement::Query(_))));

        let text = std::fs::read_to_string(&create).unwrap();
        let statements = parse_statements(&text).unwrap();
        assert_eq!(statements.len(), 1);
        let Statement::CreateTable(ct) = &statements[0] else {
            panic!("expected CREATE TABLE");
        };
        assert_eq!(ct.name, "hits");
        assert_eq!(ct.columns.len(), 105);
        assert_eq!(ct.primary_key.len(), 5);

        for path in &tpch {
            let text = std::fs::read_to_string(path).unwrap();
            match parse_statement(&text) {
                Ok(Statement::Query(_)) => {}
                other => panic!("{}: {other:?}", path.display()),
            }
        }
    }
}
