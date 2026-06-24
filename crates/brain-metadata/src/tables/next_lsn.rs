//! `next_lsn` table: singleton holding the next WAL LSN to allocate.
//!
//! Singleton convention: `()` key with `t.get(&())` /
//! `t.insert(&(), &value)`.
//!
//! ## What lives here
//!
//! - [`NEXT_LSN_TABLE`] — singleton `() → u64`, the next LSN to hand
//!   out for a WAL record.
//!
//! ## What does NOT live here
//!
//! - **LSN allocation logic** (read, hand out, advance, persist) —
//!   the `MetadataSink` impl composes this table with the WAL.
//! - **Initial value on missing** (fresh shard vs replayed-from-WAL) —
//!   the recovery code seeds this row from a WAL scan during the
//!   open-or-recover handshake. Storage stays decision-free; callers
//!   pick their default via
//!   `.get(&()).unwrap_or_default()` or by inserting an explicit
//!   initial value.

use redb::TableDefinition;

/// The `next_lsn` table. Singleton: `()` key, `u64` value.
pub const NEXT_LSN_TABLE: TableDefinition<'static, (), u64> = TableDefinition::new("next_lsn");
