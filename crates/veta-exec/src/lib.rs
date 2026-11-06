//! Expressions, functions, regex/LIKE, operators and the thread scheduler (contract §10.3, §12).
//!
//! Depends on `veta-core`, `veta-codec`, `veta-parquet`, `veta-storage` and `veta-sql`
//! (contract §2).
//!
//! - [`database`]: the `--db` directory and its tables, the binder's catalog.
//! - [`executor`]: runs a logical plan as pipelines of morsels over `--threads` workers.
//! - [`load`]: Parquet and TSV into a table (`veta load`).
//! - `pipeline`, `aggregate`, `join`, `sort`, `hash_table` and `group_keys`: the physical
//!   operators.

mod aggregate;
pub mod context;
pub mod database;
pub mod executor;
pub mod expr_eval;
pub mod functions;
mod group_keys;
mod hash_table;
mod join;
pub mod like;
pub mod load;
mod pipeline;
pub mod regex;
mod sort;

pub use context::{ExecOptions, ExecStats};
pub use database::Database;
pub use executor::{QueryResult, execute, explain, explain_analyze};
pub use expr_eval::{CompiledExpr, cast_vector, eval, eval_predicate};
pub use load::{LoadInput, LoadSummary, load};
pub use pipeline::keep_worker_error;
