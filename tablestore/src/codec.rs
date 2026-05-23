//! Canonical byte encoding for values, rows and schema.
//!
//! Values are encoded as `tag + length-prefixed canonical text`. Because every
//! type's [`Value::display`](crate::value::Value::display) round-trips through
//! [`Value::parse`](crate::value::Value::parse), this is lossless and keeps
//! the format trivially auditable — no bespoke binary layout per type.

use crate::types::Type;
use crate::value::Value;
use crate::{Result, StoreError};
use alloc::string::String;
use alloc::vec::Vec;

/// Append an unsigned LEB128 varint.
pub fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

/// Read an unsigned LEB128 varint, advancing `pos`.
pub fn get_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos).ok_or(StoreError::Corrupt("varint eof"))?;
        *pos += 1;
        if shift >= 64 {
            return Err(StoreError::Corrupt("varint overflow"));
        }
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok(result)
}

pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_uvarint(out, b.len() as u64);
    out.extend_from_slice(b);
}

pub fn get_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let len = get_uvarint(buf, pos)? as usize;
    let end = pos
        .checked_add(len)
        .filter(|e| *e <= buf.len())
        .ok_or(StoreError::Corrupt("bytes eof"))?;
    let s = &buf[*pos..end];
    *pos = end;
    Ok(s)
}

pub fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

pub fn get_str(buf: &[u8], pos: &mut usize) -> Result<String> {
    let b = get_bytes(buf, pos)?;
    core::str::from_utf8(b)
        .map(String::from)
        .map_err(|_| StoreError::Corrupt("bad utf8"))
}

/// Encode one optional cell: `0x00` = NULL, otherwise `0x01 tag str`.
pub fn put_cell(out: &mut Vec<u8>, cell: &Option<Value>) {
    match cell {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            out.push(v.type_of().tag());
            put_str(out, &v.display());
        }
    }
}

pub fn get_cell(buf: &[u8], pos: &mut usize) -> Result<Option<Value>> {
    let present = *buf.get(*pos).ok_or(StoreError::Corrupt("cell eof"))?;
    *pos += 1;
    if present == 0 {
        return Ok(None);
    }
    let tag = *buf.get(*pos).ok_or(StoreError::Corrupt("cell tag eof"))?;
    *pos += 1;
    let ty = Type::from_tag(tag).ok_or(StoreError::Corrupt("bad type tag"))?;
    let s = get_str(buf, pos)?;
    let v = Value::parse(ty, &s).map_err(|_| StoreError::Corrupt("undecodable value"))?;
    Ok(Some(v))
}

/// A row is its column count followed by that many cells.
pub fn encode_row(cells: &[Option<Value>]) -> Vec<u8> {
    let mut out = Vec::new();
    put_uvarint(&mut out, cells.len() as u64);
    for c in cells {
        put_cell(&mut out, c);
    }
    out
}

pub fn decode_row(buf: &[u8]) -> Result<Vec<Option<Value>>> {
    let mut pos = 0;
    let n = get_uvarint(buf, &mut pos)? as usize;
    let mut cells = Vec::with_capacity(n);
    for _ in 0..n {
        cells.push(get_cell(buf, &mut pos)?);
    }
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_uvarint(&mut b, v);
            let mut p = 0;
            assert_eq!(get_uvarint(&b, &mut p).unwrap(), v);
            assert_eq!(p, b.len());
        }
    }

    #[test]
    fn row_roundtrip_with_null_and_unicode() {
        let row = alloc::vec![
            Some(Value::parse(Type::Integer, "-999999999999999999999").unwrap()),
            None,
            Some(Value::Str(String::from("héllo \u{1F600} \"quoted\""))),
            Some(Value::parse(Type::DateTimeTz, "2024-02-29T23:59:59.5+02:30").unwrap()),
        ];
        let enc = encode_row(&row);
        assert_eq!(decode_row(&enc).unwrap(), row);
    }
}
