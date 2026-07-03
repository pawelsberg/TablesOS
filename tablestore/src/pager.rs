//! Copy-on-write 4 KiB pager: the durability and wear-levelling core.
//!
//! TablesOS is single-user and every engine operation is exactly one
//! transaction, so a single in-flight transaction is all that is ever needed.
//!
//! ## Why copy-on-write
//!
//! Earlier versions journalled page images and then wrote each page back to a
//! **fixed home location**, so a repeatedly-updated row, the catalog chain and
//! the free list became permanent flash hot spots, and write-levelling only
//! spanned a small region at the front of the volume. As of v0.4.0 the pager is
//! shadow-paged: a logical page never has a fixed physical location. Every
//! commit writes each changed page to a **fresh physical page** chosen by a
//! cursor that sweeps the whole device, rebuilds the logical→physical map
//! ([`Pager::cow_build`]) up to a new root, and flips a single **anchor** record
//! to publish it. Consequences:
//!
//! * No physical page is written more often than the global average → no hot
//!   spots (only a small [`N_ANCHOR`]-slot anchor ring remains, each slot written
//!   1/N of commits and strided across the whole device).
//! * One physical write per changed page per commit (the old journal wrote each
//!   page twice).
//! * Stronger crash safety: new data lands on free pages, so the previous
//!   version is untouched until the anchor flip; there is no torn-home-page
//!   window. Power loss costs at most the last uncommitted transaction.
//!
//! ## On-disk physical layout
//!
//! The device is a flat array of physical pages (`ppn`). There is no fixed
//! superblock or journal region. Mount finds the newest state by scanning the
//! **anchor ring**: [`N_ANCHOR`] single-sector records evenly strided across the
//! device; each commit writes the next slot (`generation % N_ANCHOR`), and mount
//! picks the highest `generation` with a valid CRC. The anchor names the
//! physical root of the **PMAP**, a fanout-512 radix tree mapping each logical
//! page (`lpn`) to its current physical page.

use crate::block::{BlockDevice, SECTOR};
use crate::journal::{self, crc32, FIRST_DATA_PAGE, PAGE, PAGE_SECTORS};
use crate::{Result, StoreError};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

/// Entries per PMAP node (one `u64` physical pointer each).
const F: usize = PAGE / 8; // 512
/// Anchor-ring slots. Each commit writes one; they are strided across the whole
/// device so the only fixed-location writes are spread over N erase blocks.
const N_ANCHOR: u64 = 64;
/// Anchor record magic ("TBLA").
const ANCHOR_MAGIC: u32 = u32::from_le_bytes([b'T', b'B', b'L', b'A']);
/// Write-once volume-superblock magic ("TBLV"), at physical sector 0.
///
/// The anchor ring and the allocator are laid out relative to the volume's
/// `total_pages`. That figure is fixed when the volume is `format`ted, but the
/// runtime medium can be **larger** — e.g. a fixed-size image flashed onto a
/// bigger drive — so it must not be re-derived from `dev.sector_count()` at
/// mount (doing so strides the ring across the wrong sectors and mount then
/// reads stale/garbage pages → "varint eof"). This single immutable record,
/// written once by [`Pager::format`] and never again, pins the geometry so every
/// later mount reconstructs the exact same layout regardless of medium size.
const SUPER_MAGIC: u32 = u32::from_le_bytes([b'T', b'B', b'L', b'V']);

/// A heap page image. Heap-allocated to keep large arrays off the kernel stack.
pub type Page = Vec<u8>;

pub fn zeroed_page() -> Page {
    vec![0u8; PAGE]
}

fn get_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn get_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// The logical volume header. Every field is a **logical** quantity — page
/// numbers here (`catalog_head`, `free_head`) are `lpn`s the engine dereferences
/// through the pager, never physical locations. Persisted inside the anchor
/// record, not in any fixed page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub total_pages: u64,
    pub hwm: u64,          // next never-used logical page
    pub free_head: u64,    // head of logical free-page list, 0 = none
    pub catalog_head: u64, // head of catalog chain, 0 = no tables
    pub generation: u64,
}

/// Decoded anchor record (one 512-B sector).
struct AnchorRec {
    generation: u64,
    depth: u32,
    root: u64,
    cursor: u64,
    hwm: u64,
    free_head: u64,
    catalog_head: u64,
}

/// Upper bound on cached clean pages (`CACHE_MAX * PAGE` = 32 MiB). Browsing a
/// table re-reads it on every keystroke; without a cache that is thousands of
/// polled reads per key and the GUI appears to freeze.
const CACHE_MAX: usize = 8192;

pub struct Pager<D: BlockDevice> {
    dev: D,
    pub sb: Superblock,
    /// Physical page of the PMAP radix root (0 = empty map, nothing written yet).
    pmap_root: u64,
    /// Height of the PMAP radix tree (number of node levels; 1 = root is a leaf).
    pmap_depth: u32,
    /// Next physical page to consider for allocation; sweeps `[0,total_pages)`
    /// and wraps. Persisted in the anchor so rotation continues across reboots.
    write_cursor: u64,
    /// Physical pages reserved for the anchor ring (never handed to the
    /// allocator). Derived from `total_pages`.
    anchor_pages: BTreeSet<u64>,
    /// Physical pages currently referenced by the committed PMAP (data leaves +
    /// every tree node). Rebuilt at mount; maintained incrementally on commit.
    live: BTreeSet<u64>,
    /// Staged logical writes for the active transaction (keyed by `lpn`); also a
    /// read overlay so the transaction sees its own writes.
    dirty: BTreeMap<u64, Page>,
    /// Read-through cache of committed (clean) logical pages, keyed by `lpn`.
    cache: BTreeMap<u64, Page>,
    /// Cache of PMAP node images (keyed by physical `ppn`); valid only for the
    /// current committed tree, so cleared on every commit/rollback.
    node_cache: BTreeMap<u64, Page>,
}

impl<D: BlockDevice> Pager<D> {
    // ---- anchor-ring geometry ---------------------------------------------

    /// Physical page holding anchor `slot`, evenly strided across the pages
    /// **after** the write-once superblock (page 0). Derived only from the
    /// volume's stored `total_pages`, so format and every later mount agree no
    /// matter how big the underlying medium is.
    fn anchor_page(slot: u64, total_pages: u64) -> u64 {
        1 + ((slot as u128 * (total_pages - 1) as u128) / N_ANCHOR as u128) as u64
    }

    /// Device sector the anchor for `slot` is written to (its page's first
    /// sector).
    fn anchor_sector(slot: u64, total_pages: u64) -> u64 {
        Self::anchor_page(slot, total_pages) * PAGE_SECTORS
    }

    /// The set of physical pages reserved from the data allocator: the
    /// superblock (page 0) plus every page the anchor ring occupies.
    fn anchor_pages_for(total_pages: u64) -> BTreeSet<u64> {
        let mut s: BTreeSet<u64> = (0..N_ANCHOR)
            .map(|slot| Self::anchor_page(slot, total_pages))
            .collect();
        s.insert(0);
        s
    }

    // ---- write-once volume superblock (sector 0) --------------------------

    /// Write the immutable geometry superblock to sector 0 and flush.
    fn write_super(dev: &mut D, total_pages: u64) -> Result<()> {
        let mut buf = [0u8; SECTOR];
        put_u32(&mut buf, 0, SUPER_MAGIC);
        put_u32(&mut buf, 4, crate::VERSION);
        put_u64(&mut buf, 12, total_pages);
        let crc = crc32(&buf[12..20]);
        put_u32(&mut buf, 8, crc);
        dev.write_sector(0, &buf)?;
        dev.flush()
    }

    /// Read the volume's stored `total_pages` from the sector-0 superblock.
    fn read_super(dev: &mut D) -> Result<u64> {
        let mut buf = [0u8; SECTOR];
        dev.read_sector(0, &mut buf)?;
        if get_u32(&buf, 0) != SUPER_MAGIC {
            return Err(StoreError::Corrupt("not a TablesOS volume"));
        }
        if crc32(&buf[12..20]) != get_u32(&buf, 8) {
            return Err(StoreError::Corrupt("volume superblock crc"));
        }
        Ok(get_u64(&buf, 12))
    }

    /// PMAP tree height so that `F^depth >= total_pages` (every `lpn` is
    /// addressable). `format` guarantees `total_pages > FIRST_DATA_PAGE`, so the
    /// depth is always ≥ 2 in practice.
    fn pmap_depth_for(total_pages: u64) -> u32 {
        let f = F as u64;
        let mut depth = 1u32;
        let mut cap = f;
        while cap < total_pages {
            cap = cap.saturating_mul(f);
            depth += 1;
        }
        depth
    }

    // ---- open / create ----------------------------------------------------

    /// Lay down a fresh, empty volume on `dev`.
    pub fn format(mut dev: D) -> Result<Pager<D>> {
        let total_pages = dev.sector_count() / PAGE_SECTORS;
        if total_pages <= FIRST_DATA_PAGE + 1 {
            return Err(StoreError::OutOfSpace);
        }
        let depth = Self::pmap_depth_for(total_pages);
        let anchor_pages = Self::anchor_pages_for(total_pages);
        // Pin the geometry: record the format-time `total_pages` in the
        // write-once sector-0 superblock so mount never re-derives it from the
        // medium size (which may be larger than this volume).
        Self::write_super(&mut dev, total_pages)?;
        // Invalidate every anchor slot first, so a stale higher-generation
        // anchor from a previous format on this medium can't win at mount.
        let zero = [0u8; SECTOR];
        for slot in 0..N_ANCHOR {
            dev.write_sector(Self::anchor_sector(slot, total_pages), &zero)?;
        }
        let mut pager = Pager {
            dev,
            sb: Superblock {
                total_pages,
                hwm: FIRST_DATA_PAGE,
                free_head: 0,
                catalog_head: 0,
                generation: 1,
            },
            pmap_root: 0,
            pmap_depth: depth,
            write_cursor: 0,
            anchor_pages,
            live: BTreeSet::new(),
            dirty: BTreeMap::new(),
            cache: BTreeMap::new(),
            node_cache: BTreeMap::new(),
        };
        // Publish the empty volume (generation 1, empty PMAP).
        pager.write_anchor(1, 0, 0)?;
        Ok(pager)
    }

    /// Open an existing volume: scan the anchor ring for the newest committed
    /// root, then rebuild the live-set from the PMAP.
    pub fn mount(mut dev: D) -> Result<Pager<D>> {
        // Geometry comes from the volume's own superblock, not the medium size:
        // a fixed-size image may sit on a larger drive, and re-deriving the ring
        // stride from `sector_count()` would read the anchors at the wrong
        // sectors (the v0.4.0-dev "varint eof" corruption).
        let total_pages = Self::read_super(&mut dev)?;
        if total_pages == 0 {
            return Err(StoreError::Corrupt("empty device"));
        }
        if dev.sector_count() < total_pages * PAGE_SECTORS {
            return Err(StoreError::Corrupt("medium smaller than volume"));
        }
        let rec = Self::read_best_anchor(&mut dev, total_pages)?;
        let mut pager = Pager {
            dev,
            sb: Superblock {
                total_pages,
                hwm: rec.hwm,
                free_head: rec.free_head,
                catalog_head: rec.catalog_head,
                generation: rec.generation,
            },
            pmap_root: rec.root,
            pmap_depth: rec.depth,
            write_cursor: rec.cursor,
            anchor_pages: Self::anchor_pages_for(total_pages),
            live: BTreeSet::new(),
            dirty: BTreeMap::new(),
            cache: BTreeMap::new(),
            node_cache: BTreeMap::new(),
        };
        pager.rebuild_live()?;
        Ok(pager)
    }

    /// Scan all anchor slots and return the valid record with the highest
    /// generation (the newest committed state).
    fn read_best_anchor(dev: &mut D, total_pages: u64) -> Result<AnchorRec> {
        let mut best: Option<AnchorRec> = None;
        let mut buf = [0u8; SECTOR];
        for slot in 0..N_ANCHOR {
            dev.read_sector(Self::anchor_sector(slot, total_pages), &mut buf)?;
            if get_u32(&buf, 0) != ANCHOR_MAGIC {
                continue;
            }
            if crc32(&buf[12..72]) != get_u32(&buf, 8) {
                continue; // torn / stale slot
            }
            let rec = AnchorRec {
                depth: get_u32(&buf, 12),
                generation: get_u64(&buf, 16),
                root: get_u64(&buf, 24),
                cursor: get_u64(&buf, 32),
                hwm: get_u64(&buf, 40),
                free_head: get_u64(&buf, 48),
                catalog_head: get_u64(&buf, 56),
            };
            if best.as_ref().map_or(true, |b| rec.generation > b.generation) {
                best = Some(rec);
            }
        }
        best.ok_or(StoreError::Corrupt("not a TablesOS volume"))
    }

    /// Write the anchor record for `generation` to its ring slot and flush. This
    /// single-sector write is the atomic commit point.
    fn write_anchor(&mut self, generation: u64, root: u64, cursor: u64) -> Result<()> {
        let mut buf = [0u8; SECTOR];
        put_u32(&mut buf, 0, ANCHOR_MAGIC);
        put_u32(&mut buf, 4, crate::VERSION);
        put_u32(&mut buf, 12, self.pmap_depth);
        put_u64(&mut buf, 16, generation);
        put_u64(&mut buf, 24, root);
        put_u64(&mut buf, 32, cursor);
        put_u64(&mut buf, 40, self.sb.hwm);
        put_u64(&mut buf, 48, self.sb.free_head);
        put_u64(&mut buf, 56, self.sb.catalog_head);
        put_u64(&mut buf, 64, self.sb.total_pages);
        let crc = crc32(&buf[12..72]);
        put_u32(&mut buf, 8, crc);
        let sector = Self::anchor_sector(generation % N_ANCHOR, self.sb.total_pages);
        self.dev.write_sector(sector, &buf)?;
        self.dev.flush()
    }

    // ---- PMAP traversal ----------------------------------------------------

    /// Read a PMAP node image by physical page, optionally via `node_cache`.
    fn node_image(&mut self, ppn: u64, use_cache: bool) -> Result<Page> {
        if use_cache {
            if let Some(n) = self.node_cache.get(&ppn) {
                return Ok(n.clone());
            }
        }
        let mut b = zeroed_page();
        journal::read_page(&mut self.dev, ppn, &mut b)?;
        if use_cache {
            self.node_cache.insert(ppn, b.clone());
        }
        Ok(b)
    }

    /// Translate `lpn` to its current physical page, or `None` if unmapped
    /// (never written — reads as zeros).
    fn walk(&mut self, lpn: u64, use_cache: bool) -> Result<Option<u64>> {
        let f = F as u64;
        let mut node_ppn = self.pmap_root;
        let mut height = self.pmap_depth;
        while height > 1 {
            if node_ppn == 0 {
                return Ok(None);
            }
            let span = f.pow(height - 1);
            let idx = ((lpn / span) % f) as usize;
            let node = self.node_image(node_ppn, use_cache)?;
            node_ppn = get_u64(&node, idx * 8);
            height -= 1;
        }
        if node_ppn == 0 {
            return Ok(None);
        }
        let leaf = self.node_image(node_ppn, use_cache)?;
        let d = get_u64(&leaf, (lpn % f) as usize * 8);
        Ok(if d == 0 { None } else { Some(d) })
    }

    fn translate(&mut self, lpn: u64) -> Result<Option<u64>> {
        self.walk(lpn, true)
    }

    /// Walk every PMAP node, recording all referenced physical pages (data
    /// leaves and tree nodes) into `live`. Cost ∝ live pages (= actual data),
    /// not device size.
    fn rebuild_live(&mut self) -> Result<()> {
        self.live.clear();
        self.collect_live(self.pmap_depth, self.pmap_root)
    }

    fn collect_live(&mut self, height: u32, node_ppn: u64) -> Result<()> {
        if node_ppn == 0 {
            return Ok(());
        }
        self.live.insert(node_ppn);
        let node = self.node_image(node_ppn, false)?;
        if height == 1 {
            for i in 0..F {
                let d = get_u64(&node, i * 8);
                if d != 0 {
                    self.live.insert(d);
                }
            }
        } else {
            for i in 0..F {
                let c = get_u64(&node, i * 8);
                if c != 0 {
                    self.collect_live(height - 1, c)?;
                }
            }
        }
        Ok(())
    }

    // ---- physical allocation ----------------------------------------------

    /// Allocate a fresh physical page: advance the write cursor across the whole
    /// device (wrapping, skipping anchor pages, currently-live pages, and pages
    /// already taken this transaction). Sweeping spreads wear evenly; returns
    /// [`StoreError::OutOfSpace`] only when the device is physically full.
    fn alloc_ppn(&mut self, txn_alloc: &mut BTreeSet<u64>) -> Result<u64> {
        let total = self.sb.total_pages;
        let mut scanned = 0u64;
        while scanned < total {
            let p = self.write_cursor % total;
            self.write_cursor = (self.write_cursor + 1) % total;
            scanned += 1;
            if self.anchor_pages.contains(&p) || self.live.contains(&p) || txn_alloc.contains(&p) {
                continue;
            }
            txn_alloc.insert(p);
            return Ok(p);
        }
        Err(StoreError::OutOfSpace)
    }

    /// Rebuild the PMAP subtree of the given `height` rooted at `old_ppn`,
    /// applying the sorted `changes` (`lpn` → new data `ppn`) that fall under it.
    /// Every node on a changed path is written to a fresh physical page (copy on
    /// write); unchanged children keep their existing `ppn`. Replaced node pages
    /// are recorded in `freed`. Returns the new subtree root `ppn`.
    fn cow_build(
        &mut self,
        height: u32,
        old_ppn: u64,
        base_lpn: u64,
        changes: &[(u64, u64)],
        txn_alloc: &mut BTreeSet<u64>,
        freed: &mut BTreeSet<u64>,
    ) -> Result<u64> {
        let f = F as u64;
        let mut node = if old_ppn != 0 {
            self.node_image(old_ppn, true)?
        } else {
            zeroed_page()
        };
        if height == 1 {
            for &(lpn, dppn) in changes {
                put_u64(&mut node, ((lpn - base_lpn) % f) as usize * 8, dppn);
            }
        } else {
            let span = f.pow(height - 1);
            let mut i = 0;
            while i < changes.len() {
                let child_idx = ((changes[i].0 - base_lpn) / span) as usize;
                let mut j = i + 1;
                while j < changes.len() && ((changes[j].0 - base_lpn) / span) as usize == child_idx {
                    j += 1;
                }
                let old_child = get_u64(&node, child_idx * 8);
                let child_base = base_lpn + child_idx as u64 * span;
                let new_child =
                    self.cow_build(height - 1, old_child, child_base, &changes[i..j], txn_alloc, freed)?;
                put_u64(&mut node, child_idx * 8, new_child);
                i = j;
            }
        }
        if old_ppn != 0 {
            freed.insert(old_ppn);
        }
        let new_ppn = self.alloc_ppn(txn_alloc)?;
        journal::write_page(&mut self.dev, new_ppn, &node)?;
        Ok(new_ppn)
    }

    // ---- public API --------------------------------------------------------

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.dev
    }

    /// The current logical superblock (copy).
    pub fn superblock(&self) -> Superblock {
        self.sb
    }

    /// Change the recorded volume size. Only meaningful for a same-size version
    /// re-stamp; a genuine resize changes the anchor-ring geometry and must be
    /// done by rebuilding into a freshly-`format`ted volume (see the kernel
    /// relocate path), not in place.
    pub fn set_total_pages(&mut self, total_pages: u64) {
        self.sb.total_pages = total_pages;
    }

    /// Force a one-record commit of the anchor alone, even when no pages
    /// changed. Re-stamps the on-disk version (the anchor carries
    /// [`crate::VERSION`]); used by the data-less version "top up".
    pub fn rewrite_superblock(&mut self) -> Result<()> {
        let generation = self.sb.generation + 1;
        self.write_anchor(generation, self.pmap_root, self.write_cursor)?;
        self.sb.generation = generation;
        Ok(())
    }

    /// Read a logical page, honouring uncommitted writes, then the clean cache,
    /// then the device (via the PMAP).
    pub fn read_page(&mut self, lpn: u64) -> Result<Page> {
        if let Some(p) = self.dirty.get(&lpn) {
            return Ok(p.clone());
        }
        if let Some(p) = self.cache.get(&lpn) {
            return Ok(p.clone());
        }
        if lpn >= self.sb.total_pages {
            return Err(StoreError::Corrupt("page out of range"));
        }
        let img = match self.translate(lpn)? {
            Some(ppn) => {
                let mut b = zeroed_page();
                journal::read_page(&mut self.dev, ppn, &mut b)?;
                b
            }
            None => zeroed_page(),
        };
        if self.cache.len() >= CACHE_MAX {
            self.cache.clear();
        }
        self.cache.insert(lpn, img.clone());
        Ok(img)
    }

    /// Read a logical page straight from the device, bypassing both the active
    /// transaction's staged writes and every cache (including PMAP node caches).
    /// Used by the corruption diagnostics to distinguish a deterministic bad
    /// device read from a stomped in-RAM cache.
    pub fn reread_uncached(&mut self, lpn: u64) -> Result<Page> {
        if lpn >= self.sb.total_pages {
            return Err(StoreError::Corrupt("page out of range"));
        }
        match self.walk(lpn, false)? {
            Some(ppn) => {
                let mut b = zeroed_page();
                journal::read_page(&mut self.dev, ppn, &mut b)?;
                Ok(b)
            }
            None => Ok(zeroed_page()),
        }
    }

    /// Stage a full page image into the active transaction.
    pub fn write_page(&mut self, lpn: u64, data: Page) {
        debug_assert!(data.len() == PAGE);
        self.dirty.insert(lpn, data);
    }

    /// Allocate a logical data page: reuse a freed `lpn` if any, else extend the
    /// high-water mark. The returned page is staged as all-zeros.
    pub fn alloc_page(&mut self) -> Result<u64> {
        let lpn = if self.sb.free_head != 0 {
            let p = self.sb.free_head;
            let img = self.read_page(p)?;
            self.sb.free_head = u64::from_le_bytes(img[0..8].try_into().unwrap());
            p
        } else if self.sb.hwm < self.sb.total_pages {
            let p = self.sb.hwm;
            self.sb.hwm += 1;
            p
        } else {
            return Err(StoreError::OutOfSpace);
        };
        debug_assert!(lpn != 0);
        self.write_page(lpn, zeroed_page());
        Ok(lpn)
    }

    /// Return a logical page to the free list (effective at commit).
    pub fn free_page(&mut self, lpn: u64) {
        let mut img = zeroed_page();
        img[0..8].copy_from_slice(&self.sb.free_head.to_le_bytes());
        self.write_page(lpn, img);
        self.sb.free_head = lpn;
    }

    /// Commit the active transaction atomically (copy-on-write shadow paging):
    /// 1. write each dirty page to a fresh physical page,
    /// 2. rebuild the PMAP up to a new root (fresh nodes),
    /// 3. flush, then
    /// 4. write the new anchor record — the atomic commit point — and flush.
    ///
    /// Power loss before step 4 leaves the previous anchor (a different ring
    /// slot) and all old pages intact; after step 4 every new page was already
    /// durable from step 3.
    pub fn commit(&mut self) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let generation = self.sb.generation + 1;
        let dirty = core::mem::take(&mut self.dirty);
        let mut txn_alloc: BTreeSet<u64> = BTreeSet::new();
        let mut freed: BTreeSet<u64> = BTreeSet::new();
        // 1. Stage each new page image at a fresh physical page. The old
        //    physical page (if any) is queued to be freed *after* commit, so it
        //    stays intact and un-allocatable until the anchor flips.
        let mut changes: Vec<(u64, u64)> = Vec::with_capacity(dirty.len());
        for (&lpn, img) in dirty.iter() {
            if let Some(old) = self.translate(lpn)? {
                freed.insert(old);
            }
            let ppn = self.alloc_ppn(&mut txn_alloc)?;
            journal::write_page(&mut self.dev, ppn, img)?;
            changes.push((lpn, ppn));
        }
        // `changes` is sorted by lpn (BTreeMap iterates in key order).
        // 2. Copy-on-write the PMAP up to a new root.
        let new_root = self.cow_build(
            self.pmap_depth,
            self.pmap_root,
            0,
            &changes,
            &mut txn_alloc,
            &mut freed,
        )?;
        // 3. All new data + map pages durable.
        self.dev.flush()?;
        // 4. Publish — the atomic commit point.
        self.write_anchor(generation, new_root, self.write_cursor)?;
        // 5. Adopt the new version in RAM.
        self.sb.generation = generation;
        self.pmap_root = new_root;
        for p in &freed {
            self.live.remove(p);
        }
        self.live.append(&mut txn_alloc);
        self.node_cache.clear();
        for (lpn, img) in dirty {
            if self.cache.len() >= CACHE_MAX {
                self.cache.clear();
            }
            self.cache.insert(lpn, img);
        }
        Ok(())
    }

    /// Discard the active transaction, restoring the logical header from the
    /// last committed anchor (the engine may have mutated `sb.catalog_head` etc.
    /// before an aborted commit). The live-set is untouched because a failed
    /// commit never reaches step 5.
    pub fn rollback(&mut self) -> Result<()> {
        self.dirty.clear();
        self.node_cache.clear();
        let rec = Self::read_best_anchor(&mut self.dev, self.sb.total_pages)?;
        self.sb.hwm = rec.hwm;
        self.sb.free_head = rec.free_head;
        self.sb.catalog_head = rec.catalog_head;
        self.sb.generation = rec.generation;
        self.pmap_root = rec.root;
        self.pmap_depth = rec.depth;
        self.write_cursor = rec.cursor;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemBlockDevice;

    fn dev(mb: u64) -> MemBlockDevice {
        MemBlockDevice::new(mb * 1024 * 1024 / crate::block::SECTOR as u64)
    }

    #[test]
    fn format_mount_roundtrip() {
        let d = dev(16);
        let snap = {
            let mut pg = Pager::format(d).unwrap();
            let p = pg.alloc_page().unwrap();
            let mut img = zeroed_page();
            img[0..5].copy_from_slice(b"hello");
            pg.write_page(p, img);
            pg.commit().unwrap();
            assert_eq!(p, FIRST_DATA_PAGE);
            pg.device_mut().snapshot()
        };
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_eq!(pg.sb.hwm, FIRST_DATA_PAGE + 1);
        assert_eq!(&pg.read_page(FIRST_DATA_PAGE).unwrap()[0..5], b"hello");
    }

    #[test]
    fn power_cut_before_commit_point_loses_txn() {
        let mut pg = Pager::format(dev(16)).unwrap();
        let p = pg.alloc_page().unwrap();
        pg.commit().unwrap();
        let base = pg.device_mut().snapshot();

        // New txn; cut power on the very first write (well before the anchor).
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(base)).unwrap();
        let mut img = zeroed_page();
        img[0..3].copy_from_slice(b"new");
        pg.write_page(p, img);
        pg.device_mut().cut_after = Some(1);
        let _ = pg.commit(); // fails mid-way
        let snap = pg.device_mut().snapshot();

        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_ne!(&pg.read_page(p).unwrap()[0..3], b"new"); // change vanished
    }

    #[test]
    fn clean_commit_is_durable_and_remount_is_noop() {
        let mut pg = Pager::format(dev(16)).unwrap();
        let p = pg.alloc_page().unwrap();
        let mut img = zeroed_page();
        img[0..4].copy_from_slice(b"keep");
        pg.write_page(p, img);
        pg.commit().unwrap();
        let snap = pg.device_mut().snapshot();

        let snap2 = {
            let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
            assert_eq!(&pg.read_page(p).unwrap()[0..4], b"keep");
            pg.device_mut().snapshot()
        };
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap2)).unwrap();
        assert_eq!(&pg.read_page(p).unwrap()[0..4], b"keep");
    }

    /// The goal: repeatedly rewriting the *same* logical page must spread the
    /// physical writes across the whole device — no physical page is a hot spot.
    #[test]
    fn repeated_writes_have_no_physical_hot_spot() {
        let mut pg = Pager::format(dev(16)).unwrap();
        let p = pg.alloc_page().unwrap();
        pg.commit().unwrap();

        // Track which physical page backs `p` after each of many commits.
        let mut seen = BTreeSet::new();
        let rounds = 300u64;
        for i in 0..rounds {
            let mut img = zeroed_page();
            img[0..8].copy_from_slice(&i.to_le_bytes());
            pg.write_page(p, img);
            pg.commit().unwrap();
            let ppn = pg.translate(p).unwrap().unwrap();
            seen.insert(ppn);
        }
        // The home page must have rotated across many distinct physical pages,
        // not stuck on one (the old in-place hot spot).
        assert!(
            seen.len() as u64 > rounds / 2,
            "home page did not rotate: only {} distinct physical pages over {} commits",
            seen.len(),
            rounds
        );
        // And the latest value is intact.
        assert_eq!(
            &pg.read_page(p).unwrap()[0..8],
            &(rounds - 1).to_le_bytes()[..]
        );
    }

    /// A volume formatted at one size, then mounted on a *larger* medium (e.g.
    /// a fixed-size image flashed onto a bigger drive), must still mount and read
    /// its data. Regression for the anchor-ring/allocator geometry depending on
    /// the runtime `sector_count()`.
    #[test]
    fn mounts_on_a_larger_medium_than_it_was_formatted_on() {
        let mut pg = Pager::format(dev(16)).unwrap();
        let p = pg.alloc_page().unwrap();
        let mut img = zeroed_page();
        img[0..5].copy_from_slice(b"hello");
        pg.write_page(p, img);
        // Do enough commits that the anchor ring has rotated through many slots.
        pg.commit().unwrap();
        for i in 0..100u64 {
            let mut img = zeroed_page();
            img[0..8].copy_from_slice(&i.to_le_bytes());
            pg.write_page(p, img);
            pg.commit().unwrap();
        }
        let mut snap = pg.device_mut().snapshot();
        // Simulate flashing this image onto a larger drive of an arbitrary size
        // (a real medium is not a clean multiple of the image size, so no anchor
        // slot but the size-independent ones would line up).
        snap.resize(snap.len() + 9001 * SECTOR, 0);

        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_eq!(&pg.read_page(p).unwrap()[0..8], &99u64.to_le_bytes()[..]);
    }

    /// Anchor slots rotate; the newest generation always wins at mount.
    #[test]
    fn anchor_ring_rotates_and_newest_wins() {
        let mut pg = Pager::format(dev(16)).unwrap();
        let p = pg.alloc_page().unwrap();
        let mut slots = BTreeSet::new();
        for i in 0..10u64 {
            let mut img = zeroed_page();
            img[0] = i as u8;
            pg.write_page(p, img);
            pg.commit().unwrap();
            slots.insert(pg.sb.generation % N_ANCHOR);
        }
        assert!(slots.len() >= 5, "anchor did not rotate: {:?}", slots);
        let snap = pg.device_mut().snapshot();
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_eq!(pg.read_page(p).unwrap()[0], 9);
    }

    /// After many alloc/free/commit cycles, a remount never treats a live
    /// physical page as free (the live-set rebuild is exact).
    #[test]
    fn live_set_rebuild_is_exact() {
        let mut pg = Pager::format(dev(16)).unwrap();
        let mut keep = Vec::new();
        for r in 0..20u64 {
            let a = pg.alloc_page().unwrap();
            let mut img = zeroed_page();
            img[0..8].copy_from_slice(&r.to_le_bytes());
            pg.write_page(a, img);
            let b = pg.alloc_page().unwrap();
            pg.write_page(b, zeroed_page());
            pg.commit().unwrap();
            pg.free_page(b);
            pg.commit().unwrap();
            keep.push((a, r));
        }
        let snap = pg.device_mut().snapshot();
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        // Every kept page must still read back its value after a remount that
        // rebuilt the live-set and continued allocating.
        for &(a, r) in &keep {
            assert_eq!(&pg.read_page(a).unwrap()[0..8], &r.to_le_bytes()[..]);
        }
        // Allocate more; the new pages must not clobber any live page.
        for _ in 0..30 {
            let n = pg.alloc_page().unwrap();
            pg.write_page(n, vec![0xEE; PAGE]);
            pg.commit().unwrap();
        }
        for &(a, r) in &keep {
            assert_eq!(&pg.read_page(a).unwrap()[0..8], &r.to_le_bytes()[..]);
        }
    }

    /// Sweep the power-cut point across a commit and assert the page is always
    /// fully-old or fully-new after a remount — never a torn mix.
    #[test]
    fn commit_is_atomic_under_power_cut_at_every_point() {
        let mut base = Pager::format(dev(16)).unwrap();
        let p = base.alloc_page().unwrap();
        base.write_page(p, vec![0xAA; PAGE]);
        base.commit().unwrap();
        let snap0 = base.device_mut().snapshot();

        for cut in 1..40u64 {
            let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap0.clone())).unwrap();
            pg.write_page(p, vec![0xBB; PAGE]);
            pg.device_mut().cut_after = Some(cut);
            let _ = pg.commit();
            let snap = pg.device_mut().snapshot();

            let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
            let home = pg.read_page(p).unwrap();
            assert!(
                home.iter().all(|&b| b == 0xAA) || home.iter().all(|&b| b == 0xBB),
                "torn page at cut {cut}: first byte {}",
                home[0]
            );
        }
    }
}
