//! Syntax tree of the SQL subset of contract §10.
//!
//! Identifiers keep the text as written; comparing them case-insensitively is the
//! binder's job (§10.1). Positional `GROUP BY`/`ORDER BY` entries stay integer literals and
//! select-list aliases stay column references: the binder resolves both (§10.2).

use veta_core::DataType;
use veta_core::datetime::{DatePart, TruncUnit};
use veta_core::decimal::DecimalLiteral;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    Query(Box<Query>),
    Explain { analyze: bool, query: Box<Query> },
}

/// `CREATE [OR REPLACE] TABLE` (§10.1). `ORDER BY` is folded into `primary_key`;
/// `ENGINE` and `SETTINGS` are kept only so that callers can see they were accepted.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub or_replace: bool,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub primary_key: Vec<String>,
    pub engine: Option<String>,
    pub settings: Vec<(String, Literal)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub ctes: Vec<Cte>,
    pub body: Select,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    pub query: Query,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    /// Comma-separated `FROM` entries; each one may be a join tree.
    pub from: Vec<TableRef>,
    pub where_: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`.
    Wildcard,
    /// `e [[AS] alias]`; `text` is the expression's source text, which names the output
    /// column when there is no alias (§10.1).
    Expr {
        expr: Expr,
        alias: Option<String>,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub desc: bool,
    /// `Some(true)` for `NULLS FIRST`, `Some(false)` for `NULLS LAST`, `None` for the
    /// default of §4.
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Table {
        name: String,
        alias: Option<String>,
    },
    Subquery {
        query: Box<Query>,
        alias: String,
        column_aliases: Vec<String>,
    },
    /// Table function; only `read_parquet('path')` is accepted (§10.2).
    Function {
        name: String,
        args: Vec<Expr>,
        alias: Option<String>,
    },
    Join {
        left: Box<TableRef>,
        right: Box<TableRef>,
        kind: JoinKind,
        /// `None` only for `CROSS JOIN`.
        on: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Boolean(bool),
    Integer(i64),
    Decimal(DecimalLiteral),
    String(String),
    /// `DATE '…'`, days since 1970-01-01.
    Date(i32),
    /// `TIMESTAMP '…'`, seconds since 1970-01-01 00:00:00 UTC.
    Timestamp(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::And => "AND",
            BinaryOp::Or => "OR",
        }
    }
}

/// Scalar and aggregate functions of §10.3 that use the plain `name(args)` call syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Function {
    Length,
    Strlen,
    RegexpReplace,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl Function {
    pub fn from_name(name: &str) -> Option<Function> {
        Some(match name.to_ascii_lowercase().as_str() {
            "length" => Function::Length,
            "strlen" => Function::Strlen,
            "regexp_replace" => Function::RegexpReplace,
            "count" => Function::Count,
            "sum" => Function::Sum,
            "avg" => Function::Avg,
            "min" => Function::Min,
            "max" => Function::Max,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Function::Length => "length",
            Function::Strlen => "strlen",
            Function::RegexpReplace => "regexp_replace",
            Function::Count => "count",
            Function::Sum => "sum",
            Function::Avg => "avg",
            Function::Min => "min",
            Function::Max => "max",
        }
    }

    pub fn is_aggregate(self) -> bool {
        matches!(
            self,
            Function::Count | Function::Sum | Function::Avg | Function::Min | Function::Max
        )
    }

    /// Accepted argument counts.
    pub fn arity(self) -> (usize, usize) {
        match self {
            Function::RegexpReplace => (3, 3),
            _ => (1, 1),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    /// `[table.]name`; also a select-list alias until the binder resolves it.
    Column {
        table: Option<String>,
        name: String,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        query: Box<Query>,
        negated: bool,
    },
    Exists {
        query: Box<Query>,
        negated: bool,
    },
    ScalarSubquery(Box<Query>),
    /// `[NOT] LIKE` with `%`, `_` and the fixed escape `\`.
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    /// Searched `CASE WHEN … THEN … [ELSE …] END`.
    Case {
        branches: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        data_type: DataType,
    },
    Extract {
        part: DatePart,
        expr: Box<Expr>,
    },
    /// `substring(s FROM a [FOR b])` and `substring(s, a[, b])`.
    Substring {
        expr: Box<Expr>,
        from: Box<Expr>,
        length: Option<Box<Expr>>,
    },
    DateTrunc {
        unit: TruncUnit,
        expr: Box<Expr>,
    },
    /// `COUNT(*)`.
    CountStar,
    Function {
        func: Function,
        distinct: bool,
        args: Vec<Expr>,
    },
}
