//! Expressions, functions, regex/LIKE, operators and the thread scheduler (contract §10.3, §12).
//!
//! Depends on `veta-core`, `veta-codec`, `veta-parquet`, `veta-storage` and `veta-sql`
//! (contract §2). Operators and the scheduler arrive in later sprints.

pub mod expr_eval;
pub mod functions;
pub mod like;
pub mod regex;

pub use expr_eval::{CompiledExpr, cast_vector, eval, eval_predicate};
