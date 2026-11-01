//! Lexer, parser, AST, binder, logical plan, optimizer and statistics (contract §10–§11).
//!
//! Depends only on `veta-core` (contract §2). The binder, plan, optimizer and statistics
//! arrive in later sprints.

pub mod ast;
pub mod display;
pub mod lexer;
pub mod parser;

pub use ast::{Expr, Query, Statement};
pub use parser::{parse_expr, parse_statement, parse_statements};
