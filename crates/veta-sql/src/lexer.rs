//! SQL lexer (contract §10): case-insensitive keywords, plain and double-quoted
//! identifiers, integer and decimal literals, `'…'` strings with `''`, operators, `--` and
//! `/* */` comments, and line/column positions for error messages.
//!
//! Keywords are not a separate token kind: every unquoted word is a [`TokenKind::Word`]
//! and the parser matches keywords case-insensitively, so a non-reserved keyword can still
//! name a column.

use veta_core::{Result, VetaError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// Unquoted word, as written (keyword or identifier).
    Word(String),
    /// `"…"` identifier with `""` unescaped.
    QuotedIdent(String),
    /// Digits only, as written.
    Integer(String),
    /// `digits.digits`, `digits.` or `.digits`, as written.
    Decimal(String),
    /// `'…'` with `''` unescaped; backslashes are kept as written.
    String(String),
    Comma,
    LParen,
    RParen,
    Semicolon,
    Dot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    /// `<>` or `!=`.
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Eof,
}

impl TokenKind {
    /// Short description for error messages.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Word(w) => format!("'{w}'"),
            TokenKind::QuotedIdent(w) => format!("\"{w}\""),
            TokenKind::Integer(n) | TokenKind::Decimal(n) => format!("number {n}"),
            TokenKind::String(_) => "string literal".into(),
            TokenKind::Comma => "','".into(),
            TokenKind::LParen => "'('".into(),
            TokenKind::RParen => "')'".into(),
            TokenKind::Semicolon => "';'".into(),
            TokenKind::Dot => "'.'".into(),
            TokenKind::Plus => "'+'".into(),
            TokenKind::Minus => "'-'".into(),
            TokenKind::Star => "'*'".into(),
            TokenKind::Slash => "'/'".into(),
            TokenKind::Percent => "'%'".into(),
            TokenKind::Eq => "'='".into(),
            TokenKind::NotEq => "'<>'".into(),
            TokenKind::Lt => "'<'".into(),
            TokenKind::LtEq => "'<='".into(),
            TokenKind::Gt => "'>'".into(),
            TokenKind::GtEq => "'>='".into(),
            TokenKind::Eof => "end of input".into(),
        }
    }
}

/// A token with its 1-based line and column (in characters) and its byte span in the
/// source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub line: u32,
    pub column: u32,
    pub start: usize,
    pub end: usize,
}

/// Formats a user error located at `line:column`.
pub fn error_at(line: u32, column: u32, msg: impl std::fmt::Display) -> VetaError {
    VetaError::user(format!(
        "syntax error at line {line}, column {column}: {msg}"
    ))
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    column: u32,
}

impl<'a> Lexer<'a> {
    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    /// Advances one character (not byte), keeping line and column.
    fn bump(&mut self) {
        let Some(c) = self.src[self.pos..].chars().next() else {
            return;
        };
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
    }

    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            match (self.peek(0), self.peek(1)) {
                (Some(b), _) if b.is_ascii_whitespace() => self.bump(),
                (Some(b'-'), Some(b'-')) => {
                    while let Some(b) = self.peek(0) {
                        if b == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    let (line, column) = (self.line, self.column);
                    self.bump();
                    self.bump();
                    loop {
                        match (self.peek(0), self.peek(1)) {
                            (Some(b'*'), Some(b'/')) => {
                                self.bump();
                                self.bump();
                                break;
                            }
                            (Some(_), _) => self.bump(),
                            (None, _) => {
                                return Err(error_at(line, column, "unterminated comment"));
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// Reads a `quote`-delimited run where a doubled quote stands for one.
    fn quoted(&mut self, quote: u8, what: &str) -> Result<String> {
        let (line, column) = (self.line, self.column);
        self.bump();
        let mut out = String::new();
        let mut run_start = self.pos;
        loop {
            match self.peek(0) {
                None => return Err(error_at(line, column, format!("unterminated {what}"))),
                Some(b) if b == quote => {
                    out.push_str(&self.src[run_start..self.pos]);
                    self.bump();
                    if self.peek(0) == Some(quote) {
                        out.push(quote as char);
                        self.bump();
                        run_start = self.pos;
                    } else {
                        return Ok(out);
                    }
                }
                Some(_) => self.bump(),
            }
        }
    }

    fn number(&mut self) -> Result<TokenKind> {
        let (line, column) = (self.line, self.column);
        let start = self.pos;
        while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
            self.bump();
        }
        let mut decimal = false;
        if self.peek(0) == Some(b'.') {
            decimal = true;
            self.bump();
            while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
                self.bump();
            }
        }
        if self
            .peek(0)
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        {
            if matches!(self.peek(0), Some(b'e' | b'E')) {
                return Err(error_at(
                    line,
                    column,
                    "unsupported: exponent in numeric literal",
                ));
            }
            return Err(error_at(line, column, "invalid numeric literal"));
        }
        let text = self.src[start..self.pos].to_string();
        Ok(if decimal {
            TokenKind::Decimal(text)
        } else {
            TokenKind::Integer(text)
        })
    }

    fn next_token(&mut self) -> Result<Token> {
        self.skip_trivia()?;
        let (line, column, start) = (self.line, self.column, self.pos);
        let Some(b) = self.peek(0) else {
            return Ok(Token {
                kind: TokenKind::Eof,
                line,
                column,
                start,
                end: start,
            });
        };
        let single = |lx: &mut Self, kind: TokenKind| {
            lx.bump();
            kind
        };
        let kind = match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                while self
                    .peek(0)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
                {
                    self.bump();
                }
                TokenKind::Word(self.src[start..self.pos].to_string())
            }
            b'"' => {
                let name = self.quoted(b'"', "quoted identifier")?;
                if name.is_empty() {
                    return Err(error_at(line, column, "empty quoted identifier"));
                }
                TokenKind::QuotedIdent(name)
            }
            b'\'' => TokenKind::String(self.quoted(b'\'', "string literal")?),
            b'0'..=b'9' => self.number()?,
            b'.' if self.peek(1).is_some_and(|b| b.is_ascii_digit()) => self.number()?,
            b',' => single(self, TokenKind::Comma),
            b'(' => single(self, TokenKind::LParen),
            b')' => single(self, TokenKind::RParen),
            b';' => single(self, TokenKind::Semicolon),
            b'.' => single(self, TokenKind::Dot),
            b'+' => single(self, TokenKind::Plus),
            b'-' => single(self, TokenKind::Minus),
            b'*' => single(self, TokenKind::Star),
            b'/' => single(self, TokenKind::Slash),
            b'%' => single(self, TokenKind::Percent),
            b'=' => {
                self.bump();
                if self.peek(0) == Some(b'=') {
                    return Err(error_at(line, column, "unsupported: operator =="));
                }
                TokenKind::Eq
            }
            b'<' => {
                self.bump();
                match self.peek(0) {
                    Some(b'>') => single(self, TokenKind::NotEq),
                    Some(b'=') => single(self, TokenKind::LtEq),
                    _ => TokenKind::Lt,
                }
            }
            b'>' => {
                self.bump();
                match self.peek(0) {
                    Some(b'=') => single(self, TokenKind::GtEq),
                    _ => TokenKind::Gt,
                }
            }
            b'!' if self.peek(1) == Some(b'=') => {
                self.bump();
                single(self, TokenKind::NotEq)
            }
            _ => {
                let c = self.src[self.pos..].chars().next().unwrap_or('?');
                return Err(error_at(
                    line,
                    column,
                    format!("unsupported: character {c:?}"),
                ));
            }
        };
        Ok(Token {
            kind,
            line,
            column,
            start,
            end: self.pos,
        })
    }
}

/// Splits `src` into tokens; the last one is always [`TokenKind::Eof`].
pub fn tokenize(src: &str) -> Result<Vec<Token>> {
    let mut lexer = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        line: 1,
        column: 1,
    };
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token()?;
        let eof = token.kind == TokenKind::Eof;
        tokens.push(token);
        if eof {
            return Ok(tokens);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        tokenize(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn word(w: &str) -> TokenKind {
        TokenKind::Word(w.into())
    }

    #[test]
    fn words_numbers_and_strings() {
        assert_eq!(
            kinds("SeLeCt a1, 42, 0.05, .5, 7. 'it''s' \"Mixed \"\"q\"\"\""),
            vec![
                word("SeLeCt"),
                word("a1"),
                TokenKind::Comma,
                TokenKind::Integer("42".into()),
                TokenKind::Comma,
                TokenKind::Decimal("0.05".into()),
                TokenKind::Comma,
                TokenKind::Decimal(".5".into()),
                TokenKind::Comma,
                TokenKind::Decimal("7.".into()),
                TokenKind::String("it's".into()),
                TokenKind::QuotedIdent("Mixed \"q\"".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn backslashes_in_strings_are_literal() {
        assert_eq!(
            kinds(r"'^https?://(?:www\.)?([^/]+)/.*$' '\1'"),
            vec![
                TokenKind::String(r"^https?://(?:www\.)?([^/]+)/.*$".into()),
                TokenKind::String(r"\1".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds("= <> != < <= > >= + - * / % ( ) ; ."),
            vec![
                TokenKind::Eq,
                TokenKind::NotEq,
                TokenKind::NotEq,
                TokenKind::Lt,
                TokenKind::LtEq,
                TokenKind::Gt,
                TokenKind::GtEq,
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::Semicolon,
                TokenKind::Dot,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(
            kinds("a -- line comment\n/* block\n comment */ b--tail"),
            vec![word("a"), word("b"), TokenKind::Eof]
        );
        assert_eq!(kinds("1-2"), {
            vec![
                TokenKind::Integer("1".into()),
                TokenKind::Minus,
                TokenKind::Integer("2".into()),
                TokenKind::Eof,
            ]
        });
    }

    #[test]
    fn positions_are_line_and_column() {
        let tokens = tokenize("SELECT\n  x,\n\ty").unwrap();
        let pos: Vec<(u32, u32)> = tokens.iter().map(|t| (t.line, t.column)).collect();
        assert_eq!(pos, vec![(1, 1), (2, 3), (2, 4), (3, 2), (3, 3)]);
        assert_eq!(tokens[1].start, 9);
        assert_eq!(tokens[1].end, 10);
    }

    #[test]
    fn errors_carry_positions() {
        let msg = |src: &str| match tokenize(src) {
            Err(VetaError::User(m)) => m,
            other => panic!("expected user error, got {other:?}"),
        };
        assert!(msg("SELECT 'abc").contains("line 1, column 8: unterminated string"));
        assert!(msg("a\n /* x").contains("line 2, column 2: unterminated comment"));
        assert!(msg("a ? b").contains("line 1, column 3"));
        assert!(msg("1e5").contains("unsupported"));
        assert!(msg("\"\"").contains("empty quoted identifier"));
    }
}
