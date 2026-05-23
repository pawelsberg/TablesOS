//! The type system from the specification.

use alloc::string::String;

/// Logical column types. The GUI's type picker is exactly [`Type::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Type {
    Integer,
    UnsignedInteger,
    Decimal,
    String,
    Date,
    DateTz,
    Time,
    DateTime,
    DateTimeTz,
}

impl Type {
    /// Fixed, ordered picker list (matches the spec's type table).
    pub const ALL: [Type; 9] = [
        Type::Integer,
        Type::UnsignedInteger,
        Type::Decimal,
        Type::String,
        Type::Date,
        Type::DateTz,
        Type::Time,
        Type::DateTime,
        Type::DateTimeTz,
    ];

    /// Canonical name as written in the spec and shown in the UI.
    pub fn name(self) -> &'static str {
        match self {
            Type::Integer => "integer",
            Type::UnsignedInteger => "unsigned integer",
            Type::Decimal => "decimal",
            Type::String => "string",
            Type::Date => "date",
            Type::DateTz => "date tz",
            Type::Time => "time",
            Type::DateTime => "date time",
            Type::DateTimeTz => "date time tz",
        }
    }

    pub fn from_name(s: &str) -> Option<Type> {
        Type::ALL.into_iter().find(|t| t.name() == s)
    }

    /// One-byte tag used by the on-disk codec. Stable across versions.
    pub fn tag(self) -> u8 {
        match self {
            Type::Integer => 1,
            Type::UnsignedInteger => 2,
            Type::Decimal => 3,
            Type::String => 4,
            Type::Date => 5,
            Type::DateTz => 6,
            Type::Time => 7,
            Type::DateTime => 8,
            Type::DateTimeTz => 9,
        }
    }

    pub fn from_tag(tag: u8) -> Option<Type> {
        Type::ALL.into_iter().find(|t| t.tag() == tag)
    }

    /// Short hint shown next to the field in the Row Editor.
    pub fn input_hint(self) -> &'static str {
        match self {
            Type::Integer => "e.g. -1234567890123456789",
            Type::UnsignedInteger => "e.g. 1234567890123456789",
            Type::Decimal => "e.g. -3.14159265358979",
            Type::String => "any UTF-8 text",
            Type::Date => "YYYY-MM-DD (year may be signed)",
            Type::DateTz => "YYYY-MM-DD+HH:MM",
            Type::Time => "HH:MM:SS[.fraction]",
            Type::DateTime => "YYYY-MM-DDTHH:MM:SS[.fraction]",
            Type::DateTimeTz => "YYYY-MM-DDTHH:MM:SS[.fraction]+HH:MM",
        }
    }
}

/// Parse failures carry a short message that the Row Editor shows inline next
/// to the offending field.
pub(crate) fn perr(msg: impl Into<String>) -> crate::StoreError {
    crate::StoreError::Parse(msg.into())
}
