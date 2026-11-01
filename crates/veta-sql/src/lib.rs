//! Lexer, parser, AST, binder, logical plan, optimizer and statistics (contract §10–§11).
//!
//! Depends only on `veta-core` (contract §2). The optimizer and statistics arrive in later
//! sprints.

pub mod ast;
pub mod binder;
pub mod catalog;
pub mod display;
pub mod expr;
pub mod lexer;
pub mod parser;
pub mod plan;

pub use ast::{Expr, Query, Statement};
pub use binder::bind_query;
pub use catalog::{Catalog, CatalogTable, MemoryCatalog};
pub use expr::{AggFunc, AggregateExpr, BoundExpr, BoundExprKind, ScalarFunc};
pub use parser::{parse_expr, parse_statement, parse_statements};
pub use plan::{JoinKind, LogicalPlan, SortKey};
