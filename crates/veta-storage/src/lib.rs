//! Part format, tables, insertion, snapshots and recovery (contract §5–§7).
//!
//! Depends on `veta-core` and `veta-codec` (contract §2); it does not parse SQL, so a
//! [`TableDef`] is built by whoever parses `CREATE TABLE`.
//!
//! - [`block`]: blocks of §5.3; [`part`]: parts of §5.2 ([`PartWriter`], [`PartReader`]).
//! - [`table`]: table directories; [`locks`]: `flock` helpers.
//! - [`insert`]: atomic insertion of §6.1; [`snapshot`]: reader snapshot of §6.3;
//!   [`recover`]: recovery of §6.5 and detaching of broken parts (§6.4).
//! - [`sort`]: ordering of a batch by the primary key with typed comparators.

pub mod block;
mod colstats;
mod fsutil;
pub mod insert;
pub mod locks;
pub mod part;
pub mod recover;
pub mod snapshot;
pub mod sort;
pub mod table;

pub use insert::insert;
pub use part::{GRANULE_ROWS, PartMeta, PartName, PartReader, PartStats, PartWriter};
pub use recover::{DetachedPart, RecoveryReport, detach_part, recover};
pub use snapshot::Snapshot;
pub use table::{Table, TableDef};
