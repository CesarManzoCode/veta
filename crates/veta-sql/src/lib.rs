//! Lexer, parser, AST, binder, logical plan, optimizer and statistics (contract §10–§11).
//!
//! Depends only on `veta-core` (contract §2). [`optimizer`] holds rules 1, 2, 4, 5 and 6
//! of §11.2; join reordering and statistics arrive in K2.

pub mod ast;
pub mod binder;
pub mod catalog;
pub mod display;
pub mod expr;
pub mod lexer;
pub mod optimizer;
pub mod parser;
pub mod plan;

pub use ast::{Expr, Query, Statement};
pub use binder::bind_query;
pub use catalog::{Catalog, CatalogTable, MemoryCatalog};
pub use expr::{AggFunc, AggregateExpr, BoundExpr, BoundExprKind, ScalarFunc};
pub use optimizer::{OptimizerOptions, optimize};
pub use parser::{parse_expr, parse_statement, parse_statements};
pub use plan::{JoinKind, LogicalPlan, SortKey};
