//! Lexer, parser, AST, binder, logical plan, optimizer and statistics (contract §10–§11).
//!
//! Depends only on `veta-core` (contract §2). [`binder`] unnests every subquery into
//! joins (§10.4). [`optimizer`] holds the rules of §11.2; [`cardinality`] estimates the rows
//! of every plan node from the catalog statistics (§11.5); [`explain`] prints a plan as the
//! JSON of §11.6.

pub mod ast;
pub mod binder;
pub mod cardinality;
pub mod catalog;
pub mod display;
pub mod explain;
pub mod expr;
pub mod lexer;
pub mod optimizer;
pub mod parser;
pub mod plan;

pub use ast::{Expr, Query, Statement};
pub use binder::bind_query;
pub use cardinality::{Estimator, JoinEstimate, estimate};
pub use catalog::{Catalog, CatalogTable, MemoryCatalog};
pub use explain::explain_json;
pub use expr::{AggFunc, AggregateExpr, BoundExpr, BoundExprKind, ScalarFunc};
pub use optimizer::{JoinOrder, OptimizerOptions, optimize};
pub use parser::{parse_expr, parse_statement, parse_statements};
pub use plan::{JoinKind, LogicalPlan, SortKey};
