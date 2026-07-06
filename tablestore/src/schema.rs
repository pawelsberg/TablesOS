//! Schema model: tables, columns, foreign keys.
//!
//! A schema is small, so it is serialized and rewritten whole on every schema
//! change rather than edited in place — simpler and obviously correct.

use crate::codec::*;
use crate::types::Type;
use crate::value::Value;
use crate::{Result, StoreError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub ty: Type,
    pub nullable: bool,
    pub unique: bool,
    /// Display width (in characters) for the Table Browser column, or `None`
    /// for the default width. Purely presentational: stored values are never
    /// affected — a value wider than this is simply truncated when rendered in
    /// the browser grid.
    pub display_width: Option<u16>,
}

/// `from_col -> to_table.to_col`. `name` is auto-derived but user-visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    pub from_col: String,
    pub to_table: String,
    pub to_col: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub fks: Vec<ForeignKey>,
    /// Ordered list of column names used to render a row of this table as a
    /// compact **reference label** wherever the row appears as a reference
    /// (e.g. `id:1 name:John`). Empty means "use the automatic default" —
    /// see [`Table::ref_col_indices`].
    pub ref_cols: Vec<String>,
}

impl Table {
    pub fn new(name: impl Into<String>) -> Table {
        Table {
            name: name.into(),
            columns: Vec::new(),
            fks: Vec::new(),
            ref_cols: Vec::new(),
        }
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Column indices that form this table's reference label, in display
    /// order. Uses the configured [`ref_cols`](Table::ref_cols) when set
    /// (skipping any name that no longer maps to a column); otherwise falls
    /// back to the `UNIQUE` columns, or the first column if none is `UNIQUE`.
    pub fn ref_col_indices(&self) -> Vec<usize> {
        if !self.ref_cols.is_empty() {
            let mapped: Vec<usize> = self
                .ref_cols
                .iter()
                .filter_map(|n| self.column_index(n))
                .collect();
            if !mapped.is_empty() {
                return mapped;
            }
        }
        let uniq: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.unique)
            .map(|(i, _)| i)
            .collect();
        if !uniq.is_empty() {
            uniq
        } else if self.columns.is_empty() {
            Vec::new()
        } else {
            alloc::vec![0]
        }
    }

    /// Compact label for `row` when it is shown as a reference elsewhere, e.g.
    /// `id:1 name:John`. Columns come from [`ref_col_indices`](Table::ref_col_indices);
    /// a missing/NULL cell renders its name with `NULL`.
    pub fn reference_label(&self, row: &[Option<Value>]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for ci in self.ref_col_indices() {
            let val = match row.get(ci) {
                Some(Some(v)) => v.display(),
                _ => String::from("NULL"),
            };
            parts.push(format!("{}:{}", self.columns[ci].name, val));
        }
        parts.join(" ")
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.name);
        put_uvarint(&mut out, self.columns.len() as u64);
        for c in &self.columns {
            put_str(&mut out, &c.name);
            out.push(c.ty.tag());
            out.push(c.nullable as u8);
            out.push(c.unique as u8);
        }
        put_uvarint(&mut out, self.fks.len() as u64);
        for f in &self.fks {
            put_str(&mut out, &f.name);
            put_str(&mut out, &f.from_col);
            put_str(&mut out, &f.to_table);
            put_str(&mut out, &f.to_col);
        }
        // Reference columns are appended after the FKs so an older schema image
        // (which ends there) decodes with an empty list — see `decode`.
        put_uvarint(&mut out, self.ref_cols.len() as u64);
        for c in &self.ref_cols {
            put_str(&mut out, c);
        }
        // Display widths are appended last, as a sparse `(column index, width)`
        // list — only columns with a width set appear. An image written before
        // this field ends right after the ref-cols block and decodes with every
        // width unset.
        let widths: Vec<(usize, u16)> = self
            .columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.display_width.map(|w| (i, w)))
            .collect();
        put_uvarint(&mut out, widths.len() as u64);
        for (i, w) in widths {
            put_uvarint(&mut out, i as u64);
            put_uvarint(&mut out, w as u64);
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Table> {
        let mut pos = 0;
        let name = get_str(buf, &mut pos)?;
        let ncols = get_uvarint(buf, &mut pos)? as usize;
        let mut columns = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let name = get_str(buf, &mut pos)?;
            let tag = *buf.get(pos).ok_or(StoreError::Corrupt("schema eof"))?;
            pos += 1;
            let ty = Type::from_tag(tag).ok_or(StoreError::Corrupt("schema type"))?;
            let nullable = *buf.get(pos).ok_or(StoreError::Corrupt("schema eof"))? != 0;
            pos += 1;
            let unique = *buf.get(pos).ok_or(StoreError::Corrupt("schema eof"))? != 0;
            pos += 1;
            columns.push(Column {
                name,
                ty,
                nullable,
                unique,
                display_width: None,
            });
        }
        let nfks = get_uvarint(buf, &mut pos)? as usize;
        let mut fks = Vec::with_capacity(nfks);
        for _ in 0..nfks {
            fks.push(ForeignKey {
                name: get_str(buf, &mut pos)?,
                from_col: get_str(buf, &mut pos)?,
                to_table: get_str(buf, &mut pos)?,
                to_col: get_str(buf, &mut pos)?,
            });
        }
        // Reference columns: present only in newer images. An older schema
        // ends right after the FKs, so a clean EOF here means "none".
        let mut ref_cols = Vec::new();
        if pos < buf.len() {
            let nref = get_uvarint(buf, &mut pos)? as usize;
            ref_cols.reserve(nref);
            for _ in 0..nref {
                ref_cols.push(get_str(buf, &mut pos)?);
            }
        }
        // Display widths: a sparse `(column index, width)` list appended after
        // the ref-cols block. Absent in older images (EOF here), in which case
        // every width stays unset.
        if pos < buf.len() {
            let nw = get_uvarint(buf, &mut pos)? as usize;
            for _ in 0..nw {
                let ci = get_uvarint(buf, &mut pos)? as usize;
                let w = get_uvarint(buf, &mut pos)?;
                if let Some(c) = columns.get_mut(ci) {
                    c.display_width = Some(w as u16);
                }
            }
        }
        Ok(Table {
            name,
            columns,
            fks,
            ref_cols,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person() -> Table {
        Table {
            name: "person".into(),
            columns: alloc::vec![
                Column { name: "id".into(), ty: Type::UnsignedInteger, nullable: false, unique: true, display_width: None },
                Column { name: "name".into(), ty: Type::String, nullable: false, unique: false, display_width: None },
                Column { name: "boss".into(), ty: Type::UnsignedInteger, nullable: true, unique: false, display_width: None },
            ],
            fks: alloc::vec![ForeignKey {
                name: "boss_fk".into(),
                from_col: "boss".into(),
                to_table: "person".into(),
                to_col: "id".into(),
            }],
            ref_cols: alloc::vec!["id".into(), "name".into()],
        }
    }

    #[test]
    fn schema_roundtrip() {
        let t = person();
        assert_eq!(Table::decode(&t.encode()).unwrap(), t);
    }

    #[test]
    fn decode_pre_ref_cols_image_yields_empty() {
        // Simulate an older on-disk schema that ends right after the FKs:
        // truncate the trailing ref-col block off a freshly-encoded table.
        let mut t = person();
        t.ref_cols.clear();
        let mut bytes = t.encode();
        bytes.pop(); // drop the trailing `ref_cols.len() == 0` varint byte
        let decoded = Table::decode(&bytes).unwrap();
        assert!(decoded.ref_cols.is_empty());
        assert_eq!(decoded.columns, t.columns);
    }

    #[test]
    fn display_width_roundtrips() {
        let mut t = person();
        t.columns[1].display_width = Some(24); // name
        t.columns[2].display_width = Some(6); // boss
        let decoded = Table::decode(&t.encode()).unwrap();
        assert_eq!(decoded.columns[0].display_width, None);
        assert_eq!(decoded.columns[1].display_width, Some(24));
        assert_eq!(decoded.columns[2].display_width, Some(6));
        assert_eq!(decoded, t);
    }

    #[test]
    fn decode_pre_display_width_image_yields_none() {
        // Simulate an image written before display widths existed: it ends right
        // after the ref-cols block. Truncate the trailing (empty) width block.
        let t = person(); // no widths set → width block is a single `0` byte
        let mut bytes = t.encode();
        bytes.pop(); // drop the `display_widths.len() == 0` varint byte
        let decoded = Table::decode(&bytes).unwrap();
        assert!(decoded.columns.iter().all(|c| c.display_width.is_none()));
        assert_eq!(decoded.ref_cols, t.ref_cols);
    }

    #[test]
    fn reference_label_uses_configured_columns() {
        let t = person();
        let row = alloc::vec![
            Some(Value::parse(Type::UnsignedInteger, "1").unwrap()),
            Some(Value::Str("John".into())),
            None,
        ];
        assert_eq!(t.ref_col_indices(), alloc::vec![0, 1]);
        assert_eq!(t.reference_label(&row), "id:1 name:John");
    }

    #[test]
    fn reference_label_falls_back_to_unique_then_first() {
        // No configured ref cols → the UNIQUE column(s).
        let mut t = person();
        t.ref_cols.clear();
        assert_eq!(t.ref_col_indices(), alloc::vec![0]);

        // No UNIQUE column → the first column.
        for c in &mut t.columns {
            c.unique = false;
        }
        assert_eq!(t.ref_col_indices(), alloc::vec![0]);
        let row = alloc::vec![
            Some(Value::parse(Type::UnsignedInteger, "7").unwrap()),
            Some(Value::Str("Ada".into())),
            None,
        ];
        assert_eq!(t.reference_label(&row), "id:7");
    }
}
