//! TablesOS relational engine.
//!
//! This crate is the heart of the system and is deliberately free of any
//! hardware or OS knowledge: it talks to storage only through the
//! [`BlockDevice`] trait. That keeps it `no_std + alloc` for the kernel while
//! remaining fully unit-testable on the host (`cargo test -p tablestore`),
//! including simulated power-loss recovery.
//!
//! Layering, bottom to top:
//!
//! * [`bignum`]  — unlimited-magnitude unsigned/signed integers (parse,
//!   compare, format; no arithmetic is needed by the spec).
//! * [`value`] / [`types`] — the typed value domain (integers, decimals,
//!   strings, dates/times with optional fixed zone offsets), all unlimited.
//! * [`codec`]   — canonical byte encoding of values and rows.
//! * [`schema`]  — tables, columns, nullability, `UNIQUE`, foreign keys.
//! * [`block`]   — the `BlockDevice` abstraction + an in-memory device.
//! * [`pager`]   — 4 KiB pages, free-list allocator, the on-disk superblock.
//! * [`journal`] — physical (page-image) write-ahead log + crash recovery.
//! * [`store`]   — the relational operations the GUI drives.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod bignum;
pub mod block;
pub mod codec;
pub mod journal;
pub mod pager;
pub mod schema;
pub mod store;
pub mod types;
pub mod value;

pub use block::{BlockDevice, MemBlockDevice};
pub use schema::{Column, ForeignKey, Table};
pub use store::Store;
pub use types::Type;
pub use value::Value;

use alloc::string::String;

/// Every fallible engine operation funnels through this. The GUI turns these
/// into the inline / status-bar messages the UI spec calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The backing device reported an I/O failure.
    Io,
    /// On-disk structure is corrupt or not a TablesOS volume.
    Corrupt(&'static str),
    /// The volume is full (no free pages and the device cannot grow).
    OutOfSpace,
    /// A value did not parse as its column's declared type.
    Parse(String),
    /// A `NOT NULL` column was given `NULL`.
    NullViolation { column: String },
    /// A `UNIQUE` column would hold a duplicate.
    UniqueViolation { column: String },
    /// A foreign key value has no matching row in the referenced column.
    ForeignKeyViolation { fk: String },
    /// A schema change was rejected (with the human-readable reason).
    SchemaRejected(String),
    /// Named table / column / fk does not exist.
    NotFound(String),
    /// A name collides with an existing one.
    Duplicate(String),
}

pub type Result<T> = core::result::Result<T, StoreError>;
