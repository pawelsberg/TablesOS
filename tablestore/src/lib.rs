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
pub mod migrate;
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

// ---- Unified product version ------------------------------------------------
//
// TablesOS has ONE version, declared once in the workspace `Cargo.toml`
// (`[workspace.package] version`) and inherited by every crate. It is stamped
// into every on-disk version field — the MBR boot header, the volume
// superblock ([`pager`]) and the journal control header ([`journal`]). There
// are deliberately no independent format numbers: a change to the version is
// assumed to change every on-disk format, so one comparison distinguishes a
// foreign or older image. The dependency-free `uefi-loader` duplicates this
// packing (it cannot depend on this crate) and must stay byte-compatible.

/// Parse a decimal string to `u32` in a `const` context — used on the
/// Cargo-provided `CARGO_PKG_VERSION_*` components.
const fn parse_dec(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut v = 0u32;
    let mut i = 0;
    while i < b.len() {
        v = v * 10 + (b[i] - b'0') as u32;
        i += 1;
    }
    v
}

/// The single TablesOS version, packed `(major << 16) | (minor << 8) | patch`.
/// This exact value is written to every on-disk version field.
pub const VERSION: u32 = (parse_dec(env!("CARGO_PKG_VERSION_MAJOR")) << 16)
    | (parse_dec(env!("CARGO_PKG_VERSION_MINOR")) << 8)
    | parse_dec(env!("CARGO_PKG_VERSION_PATCH"));

/// The human-readable product version, e.g. `"v0.1.0"`.
pub const VERSION_STR: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// Render any packed [`VERSION`]-form value back to `"vMAJOR.MINOR.PATCH"`
/// (e.g. for a version read off another disk's header or superblock).
pub fn version_string(packed: u32) -> String {
    alloc::format!(
        "v{}.{}.{}",
        (packed >> 16) & 0xFF,
        (packed >> 8) & 0xFF,
        packed & 0xFF
    )
}

/// Every fallible engine operation funnels through this. The GUI turns these
/// into the inline / status-bar messages the UI spec calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The backing device reported an I/O failure.
    Io,
    /// On-disk structure is corrupt or not a TablesOS volume.
    Corrupt(&'static str),
    /// Like [`StoreError::Corrupt`] but with a runtime-built, page-level
    /// diagnostic (page number, header fields, first bytes, and a fresh
    /// uncached re-read comparison). Used to pinpoint hardware-specific read
    /// corruption that reproduces on some machines but not others.
    CorruptDetail(String),
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
