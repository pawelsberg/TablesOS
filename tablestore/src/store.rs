//! The relational layer: every operation the GUI can perform, with full
//! integrity enforcement. Each public mutating method is exactly one
//! transaction — it either commits whole or rolls back leaving the store
//! structurally and referentially intact.
//!
//! On-page structures (all on pages ≥ `FIRST_DATA_PAGE`):
//!
//! * **Blob chain** — arbitrary bytes split across linked pages
//!   (`[next:8][len:8][payload]`). Used for the catalog, each table's schema,
//!   and each individual row. This is what makes values "unlimited": a row is
//!   just a blob, however long.
//! * **Index chain** — linked pages of `u64` row-head pointers
//!   (`[next:8][n:8][slots…]`); `0` is a tombstone. A table's rows are an
//!   unordered multiset addressed by [`RowId`] = (index page, slot).

use crate::block::BlockDevice;
use crate::codec::*;
use crate::journal::PAGE;
use crate::pager::{zeroed_page, Pager};
use crate::schema::{Column, ForeignKey, Table};
use crate::value::Value;
use crate::{Result, StoreError};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

const CHAIN_HDR: usize = 16;
const CHAIN_CAP: usize = PAGE - CHAIN_HDR;
const IDX_HDR: usize = 16;
const IDX_CAP: usize = (PAGE - IDX_HDR) / 8;

/// Stable handle to a row for the lifetime of that row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowId {
    pub index_page: u64,
    pub slot: u32,
}

/// One line of the Table List screen.
#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub columns: usize,
    pub rows: u64,
}

#[derive(Debug, Clone)]
struct CatEntry {
    name: String,
    schema_head: u64,
    data_head: u64,
    row_count: u64,
}

pub struct Store<D: BlockDevice> {
    pager: Pager<D>,
}

fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

impl<D: BlockDevice> Store<D> {
    /// Mount an existing TablesOS volume. **Never formats.** A missing or
    /// corrupt superblock returns `Corrupt("uninitialised")` and the caller
    /// must fail closed — the kernel has no format path (SPECIFICATION.md /
    /// IMPLEMENTATION.md item 3: "accidental-overwrite safety"). Only the
    /// host image builder is allowed to call [`Store::format`].
    pub fn open(dev: D) -> Result<Store<D>> {
        match Pager::mount(dev) {
            Ok(p) => Ok(Store { pager: p }),
            Err(StoreError::Corrupt(_)) => Err(StoreError::Corrupt("uninitialised")),
            Err(e) => Err(e),
        }
    }

    /// Unconditionally create a fresh empty store on the device.
    pub fn format(dev: D) -> Result<Store<D>> {
        Ok(Store {
            pager: Pager::format(dev)?,
        })
    }

    /// Allocator high-water mark — the next never-used **logical** page. All
    /// live content (catalog, schema, rows, free list) is addressed by logical
    /// pages below it; the upgrade path reads logical pages `0..hwm` to rebuild
    /// the volume. (Under copy-on-write these are logical handles — their
    /// physical locations are spread across the device, not a `0..hwm` prefix.)
    pub fn hwm(&self) -> u64 {
        self.pager.superblock().hwm
    }

    /// Total pages the volume currently believes it spans.
    pub fn total_pages(&self) -> u64 {
        self.pager.superblock().total_pages
    }

    /// Finish a version "top up" after the volume's bytes are in place: record
    /// the (possibly new) volume size and force-rewrite the superblock so every
    /// on-disk version field is stamped with the current product version.
    /// Fails closed if the new region cannot hold the existing live pages.
    pub fn finalize_upgrade(&mut self, new_total_pages: u64) -> Result<()> {
        if new_total_pages < self.pager.superblock().hwm {
            return Err(StoreError::OutOfSpace);
        }
        self.pager.set_total_pages(new_total_pages);
        self.pager.rewrite_superblock()
    }

    // ---- generic blob chains -------------------------------------------------

    fn read_chain(&mut self, head: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut p = head;
        while p != 0 {
            let page = self.pager.read_page(p)?;
            let next = rd_u64(&page, 0);
            let len = rd_u64(&page, 8) as usize;
            if len > CHAIN_CAP {
                return Err(self.chain_corruption_detail("chain length", head, p, &page));
            }
            out.extend_from_slice(&page[CHAIN_HDR..CHAIN_HDR + len]);
            p = next;
        }
        Ok(out)
    }

    /// Build a page-level diagnostic for a corrupt chain page and re-read the
    /// page straight from the device to test read determinism. The result is
    /// surfaced verbatim in the on-screen "corrupt store" message so a machine
    /// that fails only on certain hardware can report *what* it read wrong
    /// (zeros = unfilled read, plausible-but-wrong = stale/wrong-LBA, SAME on
    /// re-read = deterministic device read, DIFF = intermittent / stomped RAM).
    fn chain_corruption_detail(
        &mut self,
        what: &str,
        head: u64,
        page_no: u64,
        first: &[u8],
    ) -> StoreError {
        let mut s = format!(
            "{what} @page {page_no} (chain head {head}) next={:#x} len={} head=",
            rd_u64(first, 0),
            rd_u64(first, 8),
        );
        for b in first.iter().take(16) {
            s.push_str(&format!("{b:02x}"));
        }
        match self.pager.reread_uncached(page_no) {
            Ok(again) => {
                let same = again.len() == first.len() && again[..] == first[..];
                s.push_str(if same { " reread=SAME" } else { " reread=DIFF " });
                if !same {
                    for b in again.iter().take(16) {
                        s.push_str(&format!("{b:02x}"));
                    }
                }
            }
            Err(_) => s.push_str(" reread=ERR"),
        }
        StoreError::CorruptDetail(s)
    }

    fn write_chain(&mut self, bytes: &[u8]) -> Result<u64> {
        let nchunks = if bytes.is_empty() {
            1
        } else {
            (bytes.len() + CHAIN_CAP - 1) / CHAIN_CAP
        };
        let mut pages = Vec::with_capacity(nchunks);
        for _ in 0..nchunks {
            pages.push(self.pager.alloc_page()?);
        }
        for i in 0..nchunks {
            let start = i * CHAIN_CAP;
            let end = (start + CHAIN_CAP).min(bytes.len());
            let chunk = &bytes[start..end];
            let next = if i + 1 < nchunks { pages[i + 1] } else { 0 };
            let mut img = zeroed_page();
            wr_u64(&mut img, 0, next);
            wr_u64(&mut img, 8, chunk.len() as u64);
            img[CHAIN_HDR..CHAIN_HDR + chunk.len()].copy_from_slice(chunk);
            self.pager.write_page(pages[i], img);
        }
        Ok(pages[0])
    }

    fn free_chain(&mut self, head: u64) -> Result<()> {
        let mut p = head;
        while p != 0 {
            let page = self.pager.read_page(p)?;
            let next = rd_u64(&page, 0);
            self.pager.free_page(p);
            p = next;
        }
        Ok(())
    }

    // ---- catalog -------------------------------------------------------------

    fn load_catalog(&mut self) -> Result<Vec<CatEntry>> {
        let head = self.pager.sb.catalog_head;
        if head == 0 {
            return Ok(Vec::new());
        }
        let bytes = self.read_chain(head)?;
        let mut pos = 0;
        let n = get_uvarint(&bytes, &mut pos)? as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(CatEntry {
                name: get_str(&bytes, &mut pos)?,
                schema_head: get_uvarint(&bytes, &mut pos)?,
                data_head: get_uvarint(&bytes, &mut pos)?,
                row_count: get_uvarint(&bytes, &mut pos)?,
            });
        }
        Ok(v)
    }

    fn store_catalog(&mut self, cat: &[CatEntry]) -> Result<()> {
        let mut bytes = Vec::new();
        put_uvarint(&mut bytes, cat.len() as u64);
        for e in cat {
            put_str(&mut bytes, &e.name);
            put_uvarint(&mut bytes, e.schema_head);
            put_uvarint(&mut bytes, e.data_head);
            put_uvarint(&mut bytes, e.row_count);
        }
        let old = self.pager.sb.catalog_head;
        let new = self.write_chain(&bytes)?;
        self.pager.sb.catalog_head = new;
        if old != 0 {
            self.free_chain(old)?;
        }
        Ok(())
    }

    fn cat_index(cat: &[CatEntry], name: &str) -> Result<usize> {
        cat.iter()
            .position(|e| e.name == name)
            .ok_or_else(|| StoreError::NotFound(format!("table '{name}'")))
    }

    fn schema_of(&mut self, e: &CatEntry) -> Result<Table> {
        Table::decode(&self.read_chain(e.schema_head)?)
    }

    // ---- index chain ---------------------------------------------------------

    fn index_collect(&mut self, data_head: u64) -> Result<Vec<(RowId, u64)>> {
        let mut out = Vec::new();
        let mut p = data_head;
        while p != 0 {
            let page = self.pager.read_page(p)?;
            let next = rd_u64(&page, 0);
            let n = rd_u64(&page, 8) as usize;
            for s in 0..n {
                let head = rd_u64(&page, IDX_HDR + s * 8);
                if head != 0 {
                    out.push((
                        RowId {
                            index_page: p,
                            slot: s as u32,
                        },
                        head,
                    ));
                }
            }
            p = next;
        }
        Ok(out)
    }

    fn index_append(&mut self, data_head: u64, row_head: u64) -> Result<RowId> {
        // Walk to the last index page.
        let mut p = data_head;
        loop {
            let mut page = self.pager.read_page(p)?;
            let next = rd_u64(&page, 0);
            let n = rd_u64(&page, 8) as usize;
            if next == 0 {
                if n < IDX_CAP {
                    wr_u64(&mut page, IDX_HDR + n * 8, row_head);
                    wr_u64(&mut page, 8, (n + 1) as u64);
                    self.pager.write_page(p, page);
                    return Ok(RowId {
                        index_page: p,
                        slot: n as u32,
                    });
                }
                // Full: chain on a new index page.
                let np = self.pager.alloc_page()?;
                let mut newp = zeroed_page();
                wr_u64(&mut newp, 8, 1);
                wr_u64(&mut newp, IDX_HDR, row_head);
                self.pager.write_page(np, newp);
                wr_u64(&mut page, 0, np);
                self.pager.write_page(p, page);
                return Ok(RowId {
                    index_page: np,
                    slot: 0,
                });
            }
            p = next;
        }
    }

    fn index_set(&mut self, index_page: u64, slot: u32, value: u64) -> Result<()> {
        let mut page = self.pager.read_page(index_page)?;
        let n = rd_u64(&page, 8) as usize;
        if (slot as usize) >= n {
            return Err(StoreError::NotFound("row".to_string()));
        }
        wr_u64(&mut page, IDX_HDR + slot as usize * 8, value);
        self.pager.write_page(index_page, page);
        Ok(())
    }

    fn row_head_at(&mut self, id: RowId) -> Result<u64> {
        let page = self.pager.read_page(id.index_page)?;
        let n = rd_u64(&page, 8) as usize;
        if (id.slot as usize) >= n {
            return Err(StoreError::NotFound("row".to_string()));
        }
        let h = rd_u64(&page, IDX_HDR + id.slot as usize * 8);
        if h == 0 {
            return Err(StoreError::NotFound("row (deleted)".to_string()));
        }
        Ok(h)
    }

    fn decode_row_fitted(&mut self, head: u64, ncols: usize) -> Result<Vec<Option<Value>>> {
        let mut cells = decode_row(&self.read_chain(head)?)?;
        // Schema may have grown/shrunk relative to when the row was written
        // (defensive — schema edits also rewrite rows).
        cells.resize(ncols, None);
        Ok(cells)
    }

    // ---- transaction helper --------------------------------------------------

    /// Run `f`; commit on success, roll back (restoring the allocator) on any
    /// error so the store is never left half-mutated.
    fn tx<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        match f(self) {
            Ok(r) => match self.pager.commit() {
                Ok(()) => Ok(r),
                // A failed commit (e.g. write-set too large) must also discard
                // the staged pages and restore the in-memory allocator.
                Err(e) => {
                    let _ = self.pager.rollback();
                    Err(e)
                }
            },
            Err(e) => {
                let _ = self.pager.rollback();
                Err(e)
            }
        }
    }

    // ---- read API ------------------------------------------------------------

    pub fn list_tables(&mut self) -> Result<Vec<TableInfo>> {
        let cat = self.load_catalog()?;
        let mut out = Vec::with_capacity(cat.len());
        for e in &cat {
            let t = Table::decode(&self.read_chain(e.schema_head)?)?;
            out.push(TableInfo {
                name: e.name.clone(),
                columns: t.columns.len(),
                rows: e.row_count,
            });
        }
        Ok(out)
    }

    pub fn get_table(&mut self, name: &str) -> Result<Table> {
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, name)?;
        self.schema_of(&cat[i].clone())
    }

    /// Every live row, in retrieval order, with its stable id.
    pub fn scan(&mut self, name: &str) -> Result<Vec<(RowId, Vec<Option<Value>>)>> {
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, name)?;
        let e = cat[i].clone();
        let t = self.schema_of(&e)?;
        let ncols = t.columns.len();
        let rows = self.index_collect(e.data_head)?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, head) in rows {
            out.push((id, self.decode_row_fitted(head, ncols)?));
        }
        Ok(out)
    }

    pub fn get_row(&mut self, name: &str, id: RowId) -> Result<Vec<Option<Value>>> {
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, name)?;
        let ncols = self.schema_of(&cat[i].clone())?.columns.len();
        let head = self.row_head_at(id)?;
        self.decode_row_fitted(head, ncols)
    }

    // ---- integrity -----------------------------------------------------------

    fn all_values_in(&mut self, e: &CatEntry, col: usize) -> Result<Vec<Option<Value>>> {
        let mut vals = Vec::new();
        for (_, head) in self.index_collect(e.data_head)? {
            let row = decode_row(&self.read_chain(head)?)?;
            vals.push(row.get(col).cloned().flatten());
        }
        Ok(vals)
    }

    /// Validate `cells` against `t`'s schema and the rest of the store, in the
    /// order the UI expects to surface errors: type, NOT NULL, UNIQUE, FK.
    fn validate(
        &mut self,
        cat: &[CatEntry],
        ti: usize,
        t: &Table,
        cells: &[Option<Value>],
        exclude: Option<RowId>,
    ) -> Result<()> {
        if cells.len() != t.columns.len() {
            return Err(StoreError::Parse(format!(
                "row has {} cells, table has {} columns",
                cells.len(),
                t.columns.len()
            )));
        }
        for (c, cell) in t.columns.iter().zip(cells) {
            match cell {
                None if !c.nullable => {
                    return Err(StoreError::NullViolation {
                        column: c.name.clone(),
                    })
                }
                Some(v) if v.type_of() != c.ty => {
                    return Err(StoreError::Parse(format!(
                        "column '{}' expects {}",
                        c.name,
                        c.ty.name()
                    )))
                }
                _ => {}
            }
        }
        // UNIQUE — only among non-NULL values.
        for (ci, c) in t.columns.iter().enumerate() {
            if !c.unique {
                continue;
            }
            let Some(v) = &cells[ci] else { continue };
            for (id, head) in self.index_collect(cat[ti].data_head)? {
                if Some(id) == exclude {
                    continue;
                }
                let other = decode_row(&self.read_chain(head)?)?;
                if other.get(ci).cloned().flatten().as_ref() == Some(v) {
                    return Err(StoreError::UniqueViolation {
                        column: c.name.clone(),
                    });
                }
            }
        }
        // Foreign keys — referenced value must exist (non-NULL) in target.
        // When the target table is this table and the row being updated
        // (`exclude`) is scanned, the pending `cells` stand in for its stored
        // values: a self-referencing row may change its key and its reference
        // together, and is checked against the state the update will produce.
        for fk in &t.fks {
            let fi = t
                .column_index(&fk.from_col)
                .ok_or_else(|| StoreError::Corrupt("fk source column"))?;
            let Some(v) = &cells[fi] else { continue };
            let tj = Self::cat_index(cat, &fk.to_table)?;
            let tt = self.schema_of(&cat[tj].clone())?;
            let tcol = tt
                .column_index(&fk.to_col)
                .ok_or_else(|| StoreError::Corrupt("fk target column"))?;
            let mut exists = false;
            for (rid, head) in self.index_collect(cat[tj].data_head)? {
                let val = if tj == ti && Some(rid) == exclude {
                    cells.get(tcol).cloned().flatten()
                } else {
                    decode_row(&self.read_chain(head)?)?
                        .get(tcol)
                        .cloned()
                        .flatten()
                };
                if val.as_ref() == Some(v) {
                    exists = true;
                    break;
                }
            }
            if !exists {
                return Err(StoreError::ForeignKeyViolation {
                    fk: fk.name.clone(),
                });
            }
        }
        Ok(())
    }

    // ---- schema operations ---------------------------------------------------

    pub fn create_table(&mut self, name: &str) -> Result<()> {
        if name.is_empty() {
            return Err(StoreError::SchemaRejected("table name is empty".into()));
        }
        self.tx(|s| {
            let mut cat = s.load_catalog()?;
            if cat.iter().any(|e| e.name == name) {
                return Err(StoreError::Duplicate(format!("table '{name}'")));
            }
            let schema_head = s.write_chain(&Table::new(name).encode())?;
            let data_head = s.write_chain(&[])?; // empty index page
            // The freshly written empty index page must look like an index
            // page (n = 0), which an all-zero blob page already satisfies.
            cat.push(CatEntry {
                name: name.to_string(),
                schema_head,
                data_head,
                row_count: 0,
            });
            s.store_catalog(&cat)
        })
    }

    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.tx(|s| {
            let mut cat = s.load_catalog()?;
            let i = Self::cat_index(&cat, name)?;
            // Reject if another table points a foreign key at this one.
            for (j, e) in cat.iter().enumerate() {
                if j == i {
                    continue;
                }
                let ot = Table::decode(&s.read_chain(e.schema_head)?)?;
                if ot.fks.iter().any(|f| f.to_table == name) {
                    return Err(StoreError::SchemaRejected(format!(
                        "table '{name}' is referenced by a foreign key in '{}'",
                        e.name
                    )));
                }
            }
            let e = cat[i].clone();
            for (_, head) in s.index_collect(e.data_head)? {
                s.free_chain(head)?;
            }
            // Free index chain and schema chain.
            let mut p = e.data_head;
            while p != 0 {
                let pg = s.pager.read_page(p)?;
                let nx = rd_u64(&pg, 0);
                s.pager.free_page(p);
                p = nx;
            }
            s.free_chain(e.schema_head)?;
            cat.remove(i);
            s.store_catalog(&cat)
        })
    }

    /// Replace a table's schema blob and, if `rewrite`, transform every row.
    fn put_schema<F>(&mut self, name: &str, mutate: F, rewrite: Option<&dyn Fn(&mut Vec<Option<Value>>)>) -> Result<()>
    where
        F: FnOnce(&mut Table) -> Result<()>,
    {
        self.tx(|s| {
            let mut cat = s.load_catalog()?;
            let i = Self::cat_index(&cat, name)?;
            let mut t = s.schema_of(&cat[i].clone())?;
            mutate(&mut t)?;
            if let Some(map) = rewrite {
                let rows = s.index_collect(cat[i].data_head)?;
                for (id, head) in rows {
                    let mut cells = decode_row(&s.read_chain(head)?)?;
                    map(&mut cells);
                    s.free_chain(head)?;
                    let nh = s.write_chain(&encode_row(&cells))?;
                    s.index_set(id.index_page, id.slot, nh)?;
                }
            }
            let old = cat[i].schema_head;
            cat[i].schema_head = s.write_chain(&t.encode())?;
            s.free_chain(old)?;
            s.store_catalog(&cat)
        })
    }

    pub fn add_column(&mut self, table: &str, col: Column) -> Result<()> {
        // The spec's rule ("existing rows receive NULL, so the column must be
        // nullable") only bites when there *are* existing rows. On an empty
        // table a NOT NULL / UNIQUE column is fine — and is in fact the only
        // way the spec's canonical `id ... not null unique` can ever exist.
        if !col.nullable {
            let cat = self.load_catalog()?;
            let i = Self::cat_index(&cat, table)?;
            if cat[i].row_count > 0 {
                return Err(StoreError::SchemaRejected(
                    "table has rows; a new column must be nullable (existing rows receive NULL)"
                        .into(),
                ));
            }
        }
        let name = col.name.clone();
        self.put_schema(
            table,
            move |t| {
                if t.column(&name).is_some() {
                    return Err(StoreError::Duplicate(format!("column '{name}'")));
                }
                t.columns.push(col);
                Ok(())
            },
            Some(&|cells| cells.push(None)),
        )
    }

    pub fn drop_column(&mut self, table: &str, col: &str) -> Result<()> {
        // Cross-table FK targeting check needs the whole catalog.
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, table)?;
        let t = self.schema_of(&cat[i].clone())?;
        let idx = t
            .column_index(col)
            .ok_or_else(|| StoreError::NotFound(format!("column '{col}'")))?;
        if t.fks.iter().any(|f| f.from_col == col) {
            return Err(StoreError::SchemaRejected(format!(
                "column '{col}' carries a foreign key"
            )));
        }
        for e in &cat {
            let ot = self.schema_of(&e.clone())?;
            if ot
                .fks
                .iter()
                .any(|f| f.to_table == table && f.to_col == col)
            {
                return Err(StoreError::SchemaRejected(format!(
                    "column '{col}' is targeted by a foreign key in '{}'",
                    e.name
                )));
            }
        }
        let cn = col.to_string();
        self.put_schema(
            table,
            move |t| {
                t.columns.remove(idx);
                // A dropped column can no longer be part of the reference label.
                t.ref_cols.retain(|n| n != &cn);
                Ok(())
            },
            Some(&move |cells| {
                if idx < cells.len() {
                    cells.remove(idx);
                }
            }),
        )
    }

    /// Move column `col` to position `new_index` (0‑based) in `table`. Schema
    /// and every row are updated atomically: the row cells are permuted to
    /// match the new column order. No-op if the column is already there. FK
    /// constraints are unaffected (they reference columns by name).
    pub fn move_column(&mut self, table: &str, col: &str, new_index: usize) -> Result<()> {
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, table)?;
        let t = self.schema_of(&cat[i].clone())?;
        let n = t.columns.len();
        let old = t
            .column_index(col)
            .ok_or_else(|| StoreError::NotFound(format!("column '{col}'")))?;
        if new_index >= n {
            return Err(StoreError::SchemaRejected(format!(
                "new column index {new_index} out of range (0..{n})"
            )));
        }
        if new_index == old {
            return Ok(());
        }
        // perm[new_position] = old_position
        let perm: Vec<usize> = {
            let mut order: Vec<usize> = (0..n).collect();
            let v = order.remove(old);
            order.insert(new_index, v);
            order
        };
        self.put_schema(
            table,
            move |t| {
                let c = t.columns.remove(old);
                t.columns.insert(new_index, c);
                Ok(())
            },
            Some(&move |cells| {
                cells.resize(n, None);
                let original = cells.clone();
                for (new_i, old_i) in perm.iter().enumerate() {
                    cells[new_i] = original[*old_i].clone();
                }
            }),
        )
    }

    /// Rename column `old` to `new` in `table`. Rows are untouched (cells are
    /// positional), but every foreign key that names this column — its source
    /// in this table, and any `to_col` pointing here from this or another
    /// table — is rewritten in the same transaction so references stay intact.
    /// Rejected if `new` is empty or already names another column.
    pub fn rename_column(&mut self, table: &str, old: &str, new: &str) -> Result<()> {
        if new.is_empty() {
            return Err(StoreError::SchemaRejected("column name is empty".into()));
        }
        if old == new {
            return Ok(());
        }
        self.tx(|s| {
            let mut cat = s.load_catalog()?;
            let i = Self::cat_index(&cat, table)?;
            let t = s.schema_of(&cat[i].clone())?;
            if t.column(old).is_none() {
                return Err(StoreError::NotFound(format!("column '{old}'")));
            }
            if t.column(new).is_some() {
                return Err(StoreError::Duplicate(format!("column '{new}'")));
            }
            // Touch every table whose schema mentions (table, old): the column
            // itself in the owning table, and FKs in any table referencing it.
            for idx in 0..cat.len() {
                let mut ot = s.schema_of(&cat[idx].clone())?;
                let owns = cat[idx].name == table;
                let mut changed = false;
                if owns {
                    for c in &mut ot.columns {
                        if c.name == old {
                            c.name = new.to_string();
                            changed = true;
                        }
                    }
                    // A renamed column keeps its place in the reference label.
                    for rc in &mut ot.ref_cols {
                        if rc == old {
                            *rc = new.to_string();
                            changed = true;
                        }
                    }
                }
                for fk in &mut ot.fks {
                    if owns && fk.from_col == old {
                        fk.from_col = new.to_string();
                        changed = true;
                    }
                    if fk.to_table == table && fk.to_col == old {
                        fk.to_col = new.to_string();
                        changed = true;
                    }
                }
                if changed {
                    let old_head = cat[idx].schema_head;
                    cat[idx].schema_head = s.write_chain(&ot.encode())?;
                    s.free_chain(old_head)?;
                }
            }
            s.store_catalog(&cat)
        })
    }

    pub fn set_unique(&mut self, table: &str, col: &str, unique: bool) -> Result<()> {
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, table)?;
        let e = cat[i].clone();
        let t = self.schema_of(&e)?;
        let ci = t
            .column_index(col)
            .ok_or_else(|| StoreError::NotFound(format!("column '{col}'")))?;
        if !unique {
            // Cannot drop UNIQUE while a foreign key relies on it as a target.
            for ce in &cat {
                let ot = self.schema_of(&ce.clone())?;
                if ot
                    .fks
                    .iter()
                    .any(|f| f.to_table == table && f.to_col == col)
                {
                    return Err(StoreError::SchemaRejected(format!(
                        "'{col}' is a foreign-key target and must stay UNIQUE"
                    )));
                }
            }
        } else {
            // Adding UNIQUE: reject existing duplicate non-NULL values.
            let mut seen: Vec<Value> = Vec::new();
            for ov in self.all_values_in(&e, ci)? {
                if let Some(v) = ov {
                    if seen.contains(&v) {
                        return Err(StoreError::SchemaRejected(format!(
                            "column '{col}' already contains duplicate values"
                        )));
                    }
                    seen.push(v);
                }
            }
        }
        self.put_schema(
            table,
            move |t| {
                t.columns[ci].unique = unique;
                Ok(())
            },
            None,
        )
    }

    pub fn add_fk(&mut self, table: &str, from_col: &str, to_table: &str, to_col: &str) -> Result<()> {
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, table)?;
        let t = self.schema_of(&cat[i].clone())?;
        let fc = t
            .column(from_col)
            .ok_or_else(|| StoreError::NotFound(format!("column '{from_col}'")))?
            .clone();
        let j = Self::cat_index(&cat, to_table)?;
        let tt = self.schema_of(&cat[j].clone())?;
        let tc = tt
            .column(to_col)
            .ok_or_else(|| StoreError::NotFound(format!("column '{to_table}.{to_col}'")))?
            .clone();
        if !tc.unique {
            return Err(StoreError::SchemaRejected(format!(
                "foreign-key target '{to_table}.{to_col}' must be UNIQUE"
            )));
        }
        if fc.ty != tc.ty {
            return Err(StoreError::SchemaRejected(
                "foreign-key column and target must share a type".into(),
            ));
        }
        // Existing rows must already satisfy the new constraint.
        let targets = self.all_values_in(&cat[j].clone(), tt.column_index(to_col).unwrap())?;
        for ov in self.all_values_in(&cat[i].clone(), t.column_index(from_col).unwrap())? {
            if let Some(v) = ov {
                if !targets.iter().any(|x| x.as_ref() == Some(&v)) {
                    return Err(StoreError::SchemaRejected(format!(
                        "existing value {} has no match in {to_table}.{to_col}",
                        v.display()
                    )));
                }
            }
        }
        let mut base = format!("{from_col}_fk");
        let existing: Vec<String> = t.fks.iter().map(|f| f.name.clone()).collect();
        let mut n = 2;
        while existing.contains(&base) {
            base = format!("{from_col}_fk{n}");
            n += 1;
        }
        let fk = ForeignKey {
            name: base,
            from_col: from_col.to_string(),
            to_table: to_table.to_string(),
            to_col: to_col.to_string(),
        };
        self.put_schema(
            table,
            move |t| {
                t.fks.push(fk);
                Ok(())
            },
            None,
        )
    }

    pub fn drop_fk(&mut self, table: &str, fk_name: &str) -> Result<()> {
        let fk_name = fk_name.to_string();
        self.put_schema(
            table,
            move |t| {
                let before = t.fks.len();
                t.fks.retain(|f| f.name != fk_name);
                if t.fks.len() == before {
                    return Err(StoreError::NotFound(format!("foreign key '{fk_name}'")));
                }
                Ok(())
            },
            None,
        )
    }

    /// Set the ordered list of columns used to render a row of `table` as a
    /// reference label (e.g. `id:1 name:John`). Every name must be an existing
    /// column; unknown or duplicate names are rejected. An empty list restores
    /// the automatic default (UNIQUE columns, or the first column).
    pub fn set_reference_columns(&mut self, table: &str, cols: Vec<String>) -> Result<()> {
        self.put_schema(
            table,
            move |t| {
                let mut seen: Vec<String> = Vec::new();
                for c in &cols {
                    if t.column(c).is_none() {
                        return Err(StoreError::NotFound(format!("column '{c}'")));
                    }
                    if seen.contains(c) {
                        return Err(StoreError::Duplicate(format!("reference column '{c}'")));
                    }
                    seen.push(c.clone());
                }
                t.ref_cols = cols;
                Ok(())
            },
            None,
        )
    }

    /// Set (or clear, with `None`) a column's Table Browser display width, in
    /// characters. Purely presentational — stored values are untouched; a value
    /// wider than this is truncated only when rendered in the browser grid.
    pub fn set_display_width(&mut self, table: &str, col: &str, width: Option<u16>) -> Result<()> {
        let col = col.to_string();
        self.put_schema(
            table,
            move |t| {
                let ci = t
                    .column_index(&col)
                    .ok_or_else(|| StoreError::NotFound(format!("column '{col}'")))?;
                t.columns[ci].display_width = width;
                Ok(())
            },
            None,
        )
    }

    // ---- data operations -----------------------------------------------------

    pub fn insert(&mut self, table: &str, cells: Vec<Option<Value>>) -> Result<RowId> {
        self.tx(|s| {
            let mut cat = s.load_catalog()?;
            let i = Self::cat_index(&cat, table)?;
            let t = s.schema_of(&cat[i].clone())?;
            s.validate(&cat, i, &t, &cells, None)?;
            let head = s.write_chain(&encode_row(&cells))?;
            let id = s.index_append(cat[i].data_head, head)?;
            cat[i].row_count += 1;
            s.store_catalog(&cat)?;
            Ok(id)
        })
    }

    /// Update a row in place. Like [`delete`](Store::delete) this restricts on
    /// inbound references: changing a value in a foreign-key *target* column is
    /// refused while any other live row still references the old value, so an
    /// update can never leave a dangling reference.
    pub fn update(&mut self, table: &str, id: RowId, cells: Vec<Option<Value>>) -> Result<()> {
        self.tx(|s| {
            let cat = s.load_catalog()?;
            let i = Self::cat_index(&cat, table)?;
            let t = s.schema_of(&cat[i].clone())?;
            s.validate(&cat, i, &t, &cells, Some(id))?;
            let old = s.row_head_at(id)?;
            // Restrict: for every FK targeting this table, a changed key must
            // not orphan the rows that reference its old value. The row being
            // updated is skipped — its own reference is replaced by `cells`
            // and was validated above.
            let old_row = s.decode_row_fitted(old, t.columns.len())?;
            for (j, e) in cat.clone().iter().enumerate() {
                let ot = s.schema_of(&e.clone())?;
                for fk in &ot.fks {
                    if fk.to_table != table {
                        continue;
                    }
                    let tci = t.column_index(&fk.to_col).unwrap();
                    let Some(old_key) = old_row.get(tci).cloned().flatten() else {
                        continue;
                    };
                    if cells.get(tci).cloned().flatten().as_ref() == Some(&old_key) {
                        continue; // key unchanged — references stay valid
                    }
                    let fci = ot.column_index(&fk.from_col).unwrap();
                    for (rid, rh) in s.index_collect(cat[j].data_head)? {
                        if j == i && rid == id {
                            continue;
                        }
                        let orow = decode_row(&s.read_chain(rh)?)?;
                        if orow.get(fci).cloned().flatten().as_ref() == Some(&old_key) {
                            return Err(StoreError::ForeignKeyViolation {
                                fk: fk.name.clone(),
                            });
                        }
                    }
                }
            }
            let new = s.write_chain(&encode_row(&cells))?;
            s.index_set(id.index_page, id.slot, new)?;
            s.free_chain(old)?;
            Ok(())
        })
    }

    pub fn delete(&mut self, table: &str, id: RowId) -> Result<()> {
        self.tx(|s| {
            let mut cat = s.load_catalog()?;
            let i = Self::cat_index(&cat, table)?;
            let t = s.schema_of(&cat[i].clone())?;
            // Restrict: refuse if a live row anywhere references this one.
            let head = s.row_head_at(id)?;
            let row = s.decode_row_fitted(head, t.columns.len())?;
            for (j, e) in cat.clone().iter().enumerate() {
                let ot = s.schema_of(&e.clone())?;
                for fk in &ot.fks {
                    if fk.to_table != table {
                        continue;
                    }
                    let tci = t.column_index(&fk.to_col).unwrap();
                    let Some(key) = row.get(tci).cloned().flatten() else {
                        continue;
                    };
                    let fci = ot.column_index(&fk.from_col).unwrap();
                    for (rid, rh) in s.index_collect(cat[j].data_head)? {
                        if j == i && rid == id {
                            continue;
                        }
                        let orow = decode_row(&s.read_chain(rh)?)?;
                        if orow.get(fci).cloned().flatten().as_ref() == Some(&key) {
                            return Err(StoreError::ForeignKeyViolation {
                                fk: fk.name.clone(),
                            });
                        }
                    }
                }
            }
            s.free_chain(head)?;
            s.index_set(id.index_page, id.slot, 0)?;
            cat[i].row_count = cat[i].row_count.saturating_sub(1);
            s.store_catalog(&cat)
        })
    }

    // ---- relationship navigation --------------------------------------------

    /// Follow `fk` from a row: the referenced rows in the target table.
    pub fn follow_fk(
        &mut self,
        table: &str,
        id: RowId,
        fk_name: &str,
    ) -> Result<(String, Vec<(RowId, Vec<Option<Value>>)>)> {
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, table)?;
        let t = self.schema_of(&cat[i].clone())?;
        let fk = t
            .fks
            .iter()
            .find(|f| f.name == fk_name)
            .ok_or_else(|| StoreError::NotFound(format!("foreign key '{fk_name}'")))?
            .clone();
        let fci = t.column_index(&fk.from_col).unwrap();
        let head = self.row_head_at(id)?;
        let key = self
            .decode_row_fitted(head, t.columns.len())?
            .get(fci)
            .cloned()
            .flatten();
        let mut matches = Vec::new();
        if let Some(key) = key {
            let j = Self::cat_index(&cat, &fk.to_table)?;
            let tt = self.schema_of(&cat[j].clone())?;
            let tci = tt.column_index(&fk.to_col).unwrap();
            for (rid, rh) in self.index_collect(cat[j].data_head)? {
                let row = self.decode_row_fitted(rh, tt.columns.len())?;
                if row.get(tci).cloned().flatten().as_ref() == Some(&key) {
                    matches.push((rid, row));
                }
            }
        }
        Ok((fk.to_table, matches))
    }

    /// Rows, anywhere, that reference this row (the "Referenced by" list).
    pub fn referencing_rows(
        &mut self,
        table: &str,
        id: RowId,
    ) -> Result<Vec<(String, String, RowId, Vec<Option<Value>>)>> {
        let cat = self.load_catalog()?;
        let i = Self::cat_index(&cat, table)?;
        let t = self.schema_of(&cat[i].clone())?;
        let head = self.row_head_at(id)?;
        let row = self.decode_row_fitted(head, t.columns.len())?;
        let mut out = Vec::new();
        for (j, e) in cat.clone().iter().enumerate() {
            let ot = self.schema_of(&e.clone())?;
            for fk in &ot.fks {
                if fk.to_table != table {
                    continue;
                }
                let tci = t.column_index(&fk.to_col).unwrap();
                let Some(key) = row.get(tci).cloned().flatten() else {
                    continue;
                };
                let fci = ot.column_index(&fk.from_col).unwrap();
                for (rid, rh) in self.index_collect(cat[j].data_head)? {
                    let orow = self.decode_row_fitted(rh, ot.columns.len())?;
                    if orow.get(fci).cloned().flatten().as_ref() == Some(&key) {
                        out.push((e.name.clone(), fk.name.clone(), rid, orow));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Force durability (used before reporting a safe-to-remove state).
    pub fn sync(&mut self) -> Result<()> {
        self.pager.device_mut().flush()
    }

    /// Access the underlying block device (the image builder uses this to
    /// snapshot a freshly formatted in-memory volume into the disk image).
    pub fn device_mut(&mut self) -> &mut D {
        self.pager.device_mut()
    }

    /// Monotonic counter bumped on every committed mutation; unchanged by
    /// read-only operations. A caller can cache derived data (e.g. a decoded
    /// row list) tagged with this value and treat it as fresh while it holds.
    pub fn generation(&self) -> u64 {
        self.pager.sb.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemBlockDevice;
    use crate::types::Type;

    fn dev() -> MemBlockDevice {
        MemBlockDevice::new(32 * 1024 * 1024 / crate::block::SECTOR as u64)
    }
    fn col(n: &str, t: Type, nul: bool, uniq: bool) -> Column {
        Column {
            name: n.into(),
            ty: t,
            nullable: nul,
            unique: uniq,
            display_width: None,
        }
    }
    fn iv(s: &str) -> Option<Value> {
        Some(Value::parse(Type::UnsignedInteger, s).unwrap())
    }
    fn sv(s: &str) -> Option<Value> {
        Some(Value::Str(s.into()))
    }

    #[test]
    fn end_to_end_relational() {
        let mut st = Store::format(dev()).unwrap();
        st.create_table("person").unwrap();
        st.add_column("person", col("id", Type::UnsignedInteger, false, true))
            .unwrap();
        st.add_column("person", col("name", Type::String, false, false))
            .unwrap();
        st.add_column("person", col("boss", Type::UnsignedInteger, true, false))
            .unwrap();
        st.add_fk("person", "boss", "person", "id").unwrap();

        let alice = st.insert("person", alloc::vec![iv("1"), sv("Alice"), None]).unwrap();
        st.insert("person", alloc::vec![iv("2"), sv("Bob"), iv("1")])
            .unwrap();

        // UNIQUE collision on id.
        assert!(matches!(
            st.insert("person", alloc::vec![iv("1"), sv("Eve"), None]),
            Err(StoreError::UniqueViolation { .. })
        ));
        // FK violation: boss 99 doesn't exist.
        assert!(matches!(
            st.insert("person", alloc::vec![iv("3"), sv("Mallory"), iv("99")]),
            Err(StoreError::ForeignKeyViolation { .. })
        ));
        // NOT NULL.
        assert!(matches!(
            st.insert("person", alloc::vec![None, sv("X"), None]),
            Err(StoreError::NullViolation { .. })
        ));

        let info = st.list_tables().unwrap();
        assert_eq!(info[0].rows, 2);

        // Alice is referenced by Bob → delete restricted.
        assert!(matches!(
            st.delete("person", alice),
            Err(StoreError::ForeignKeyViolation { .. })
        ));

        // Navigation: who reports to Alice?
        let refs = st.referencing_rows("person", alice).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].3[1], sv("Bob"));

        // Persist + remount.
        st.sync().unwrap();
    }

    #[test]
    fn update_restricts_referenced_key_changes() {
        let mut st = Store::format(dev()).unwrap();
        st.create_table("person").unwrap();
        st.add_column("person", col("id", Type::UnsignedInteger, false, true))
            .unwrap();
        st.add_column("person", col("name", Type::String, true, false))
            .unwrap();
        st.add_column("person", col("boss", Type::UnsignedInteger, true, false))
            .unwrap();
        st.add_fk("person", "boss", "person", "id").unwrap();

        let alice = st
            .insert("person", alloc::vec![iv("1"), sv("Alice"), None])
            .unwrap();
        let bob = st
            .insert("person", alloc::vec![iv("2"), sv("Bob"), iv("1")])
            .unwrap();

        // Bob references id 1, so changing Alice's id would dangle — refused.
        assert!(matches!(
            st.update("person", alice, alloc::vec![iv("5"), sv("Alice"), None]),
            Err(StoreError::ForeignKeyViolation { .. })
        ));
        // Non-key cells of a referenced row may still change freely.
        st.update("person", alice, alloc::vec![iv("1"), sv("Alicia"), None])
            .unwrap();
        // Re-point Bob elsewhere, then Alice's key is free to change.
        st.update("person", bob, alloc::vec![iv("2"), sv("Bob"), iv("2")])
            .unwrap();
        st.update("person", alice, alloc::vec![iv("5"), sv("Alicia"), None])
            .unwrap();

        // No dangling reference may exist afterwards.
        let rows = st.scan("person").unwrap();
        let ids: alloc::vec::Vec<Value> =
            rows.iter().filter_map(|(_, r)| r[0].clone()).collect();
        for (_, r) in &rows {
            if let Some(boss) = &r[2] {
                assert!(ids.contains(boss), "dangling boss reference");
            }
        }
    }

    #[test]
    fn update_self_reference_can_move_key_and_reference_together() {
        let mut st = Store::format(dev()).unwrap();
        st.create_table("person").unwrap();
        st.add_column("person", col("id", Type::UnsignedInteger, false, true))
            .unwrap();
        st.add_column("person", col("boss", Type::UnsignedInteger, true, false))
            .unwrap();
        st.add_fk("person", "boss", "person", "id").unwrap();

        // A row becomes its own boss (validated against its stored key).
        let carol = st.insert("person", alloc::vec![iv("3"), None]).unwrap();
        st.update("person", carol, alloc::vec![iv("3"), iv("3")])
            .unwrap();
        // Key and self-reference move together; the post-update state is
        // consistent, so this must be accepted.
        st.update("person", carol, alloc::vec![iv("4"), iv("4")])
            .unwrap();
        let rows = st.scan("person").unwrap();
        assert_eq!(rows[0].1[0], iv("4"));
        assert_eq!(rows[0].1[1], iv("4"));
        // But the key may not run away from its own reference.
        assert!(matches!(
            st.update("person", carol, alloc::vec![iv("9"), iv("4")]),
            Err(StoreError::ForeignKeyViolation { .. })
        ));
    }

    #[test]
    fn survives_power_cut_mid_insert() {
        let snap = {
            let mut st = Store::format(dev()).unwrap();
            st.create_table("t").unwrap();
            st.add_column("t", col("v", Type::String, true, false)).unwrap();
            st.insert("t", alloc::vec![sv("durable")]).unwrap();
            st.pager.device_mut().snapshot()
        };
        // Reopen, start an insert, yank power before the commit point.
        let mut st = Store::open(MemBlockDevice::from_snapshot(snap)).unwrap();
        st.pager.device_mut().cut_after = Some(1);
        let _ = st.insert("t", alloc::vec![sv("lost")]);
        let after = st.pager.device_mut().snapshot();

        let mut st = Store::open(MemBlockDevice::from_snapshot(after)).unwrap();
        let rows = st.scan("t").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1[0], sv("durable"));
    }

    #[test]
    fn unlimited_value_survives_roundtrip() {
        let mut st = Store::format(dev()).unwrap();
        st.create_table("big").unwrap();
        st.add_column("big", col("n", Type::UnsignedInteger, false, false))
            .unwrap();
        let huge = "9".repeat(20_000); // far larger than one page
        st.insert("big", alloc::vec![iv(&huge)]).unwrap();
        let rows = st.scan("big").unwrap();
        assert_eq!(rows[0].1[0].as_ref().unwrap().display(), huge);
    }

    #[test]
    fn rename_column_updates_schema_and_fks() {
        let mut st = Store::format(dev()).unwrap();
        st.create_table("person").unwrap();
        st.add_column("person", col("id", Type::UnsignedInteger, false, true))
            .unwrap();
        st.add_column("person", col("boss", Type::UnsignedInteger, true, false))
            .unwrap();
        st.add_fk("person", "boss", "person", "id").unwrap();
        st.insert("person", alloc::vec![iv("1"), None]).unwrap();
        st.insert("person", alloc::vec![iv("2"), iv("1")]).unwrap();

        // Rename the FK target column 'id' -> 'pid'. Both the source FK's
        // `to_col` and the column definition must follow.
        st.rename_column("person", "id", "pid").unwrap();
        let t = st.get_table("person").unwrap();
        assert_eq!(t.columns[0].name, "pid");
        assert_eq!(t.fks[0].to_col, "pid");
        // Rename the FK source column 'boss' -> 'manager'.
        st.rename_column("person", "boss", "manager").unwrap();
        let t = st.get_table("person").unwrap();
        assert_eq!(t.columns[1].name, "manager");
        assert_eq!(t.fks[0].from_col, "manager");

        // Rows are positional, so values are unchanged and the FK still
        // resolves (Bob still reports to Alice).
        let alice = st.scan("person").unwrap()[0].0;
        let refs = st.referencing_rows("person", alice).unwrap();
        assert_eq!(refs.len(), 1);

        // Collision and unknown-column rejections.
        assert!(matches!(
            st.rename_column("person", "pid", "manager"),
            Err(StoreError::Duplicate(_))
        ));
        assert!(matches!(
            st.rename_column("person", "nope", "x"),
            Err(StoreError::NotFound(_))
        ));
        // Empty target name rejected; same-name is a no-op.
        assert!(matches!(
            st.rename_column("person", "pid", ""),
            Err(StoreError::SchemaRejected(_))
        ));
        st.rename_column("person", "pid", "pid").unwrap();
    }

    #[test]
    fn move_column_reorders_schema_and_rows() {
        let mut st = Store::format(dev()).unwrap();
        st.create_table("t").unwrap();
        st.add_column("t", col("a", Type::UnsignedInteger, true, false))
            .unwrap();
        st.add_column("t", col("b", Type::UnsignedInteger, true, false))
            .unwrap();
        st.add_column("t", col("c", Type::UnsignedInteger, true, false))
            .unwrap();
        st.insert("t", alloc::vec![iv("1"), iv("2"), iv("3")]).unwrap();

        // Move 'b' to index 0: schema becomes [b, a, c]; row becomes [2, 1, 3].
        st.move_column("t", "b", 0).unwrap();
        let schema: alloc::vec::Vec<String> = st
            .get_table("t")
            .unwrap()
            .columns
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(schema, alloc::vec!["b", "a", "c"]);
        let row = &st.scan("t").unwrap()[0].1;
        assert_eq!(row[0].as_ref().unwrap().display(), "2");
        assert_eq!(row[1].as_ref().unwrap().display(), "1");
        assert_eq!(row[2].as_ref().unwrap().display(), "3");

        // Move 'c' to index 1: [b, c, a]; row becomes [2, 3, 1].
        st.move_column("t", "c", 1).unwrap();
        let row = &st.scan("t").unwrap()[0].1;
        assert_eq!(row[0].as_ref().unwrap().display(), "2");
        assert_eq!(row[1].as_ref().unwrap().display(), "3");
        assert_eq!(row[2].as_ref().unwrap().display(), "1");

        // No-op (same index).
        st.move_column("t", "c", 1).unwrap();

        // Out-of-range rejected.
        assert!(matches!(
            st.move_column("t", "c", 99),
            Err(StoreError::SchemaRejected(_))
        ));

        // Unknown column.
        assert!(matches!(
            st.move_column("t", "nope", 0),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn reference_columns_persist_and_track_schema_edits() {
        let mut st = Store::format(dev()).unwrap();
        st.create_table("person").unwrap();
        st.add_column("person", col("id", Type::UnsignedInteger, false, true))
            .unwrap();
        st.add_column("person", col("name", Type::String, false, false))
            .unwrap();
        st.add_column("person", col("nick", Type::String, true, false))
            .unwrap();

        // Default (no config) → the UNIQUE column.
        let t = st.get_table("person").unwrap();
        assert!(t.ref_cols.is_empty());
        assert_eq!(t.ref_col_indices(), alloc::vec![0]);

        // Configure an explicit, ordered label.
        st.set_reference_columns("person", alloc::vec!["id".into(), "name".into()])
            .unwrap();
        let t = st.get_table("person").unwrap();
        assert_eq!(t.ref_cols, alloc::vec!["id".to_string(), "name".to_string()]);
        let row = alloc::vec![iv("1"), sv("Alice"), None];
        assert_eq!(t.reference_label(&row), "id:1 name:Alice");

        // Unknown / duplicate reference columns are rejected.
        assert!(matches!(
            st.set_reference_columns("person", alloc::vec!["nope".into()]),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            st.set_reference_columns("person", alloc::vec!["id".into(), "id".into()]),
            Err(StoreError::Duplicate(_))
        ));

        // Renaming a reference column follows it; the order is preserved.
        st.rename_column("person", "name", "full_name").unwrap();
        let t = st.get_table("person").unwrap();
        assert_eq!(t.ref_cols, alloc::vec!["id".to_string(), "full_name".to_string()]);

        // Dropping a reference column removes it from the label set.
        st.drop_column("person", "id").unwrap();
        let t = st.get_table("person").unwrap();
        assert_eq!(t.ref_cols, alloc::vec!["full_name".to_string()]);

        // Clearing restores the automatic default.
        st.set_reference_columns("person", alloc::vec![]).unwrap();
        assert!(st.get_table("person").unwrap().ref_cols.is_empty());
    }
}
