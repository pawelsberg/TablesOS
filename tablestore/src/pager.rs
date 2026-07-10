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

/// Dirty pages per relocation commit during [`Pager::resize`] (bounds the RAM
/// the staged images take: 512 × 4 KiB = 2 MiB per commit).
const RESIZE_CHUNK: usize = 512;
/// Pages a shrink keeps free as transient headroom: a relocation commit holds
/// the old and new copy of up to [`RESIZE_CHUNK`] pages (plus their PMAP
/// paths) until its anchor flips.
const RESIZE_HEADROOM: u64 = RESIZE_CHUNK as u64 + 128;

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
    /// During an in-place [`Pager::resize`] only: the target geometry's
    /// reserved pages, which the allocator must additionally avoid so the
    /// migrated state is valid at either geometry. Empty otherwise.
    resize_reserve: BTreeSet<u64>,
    /// During an in-place [`Pager::resize`] only: exclusive allocation ceiling
    /// (the smaller of the old and new volume size). `None` otherwise.
    resize_limit: Option<u64>,
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
            resize_reserve: BTreeSet::new(),
            resize_limit: None,
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
            resize_reserve: BTreeSet::new(),
            resize_limit: None,
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
        let (depth, total) = (self.pmap_depth, self.sb.total_pages);
        self.write_anchor_geom(generation, depth, root, cursor, total)
    }

    /// Write an anchor record into the ring of an explicit geometry
    /// (`total_pages` picks the slot stride) and flush. [`Pager::resize`] uses
    /// this to pre-publish the migrated state at the *target* geometry before
    /// the superblock flip makes that geometry the one mount reads.
    fn write_anchor_geom(
        &mut self,
        generation: u64,
        depth: u32,
        root: u64,
        cursor: u64,
        total_pages: u64,
    ) -> Result<()> {
        let mut buf = [0u8; SECTOR];
        put_u32(&mut buf, 0, ANCHOR_MAGIC);
        put_u32(&mut buf, 4, crate::VERSION);
        put_u32(&mut buf, 12, depth);
        put_u64(&mut buf, 16, generation);
        put_u64(&mut buf, 24, root);
        put_u64(&mut buf, 32, cursor);
        put_u64(&mut buf, 40, self.sb.hwm);
        put_u64(&mut buf, 48, self.sb.free_head);
        put_u64(&mut buf, 56, self.sb.catalog_head);
        put_u64(&mut buf, 64, total_pages);
        let crc = crc32(&buf[12..72]);
        put_u32(&mut buf, 8, crc);
        let sector = Self::anchor_sector(generation % N_ANCHOR, total_pages);
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
        let limit = self.resize_limit.unwrap_or(total);
        let mut scanned = 0u64;
        while scanned < total {
            let p = self.write_cursor % total;
            self.write_cursor = (self.write_cursor + 1) % total;
            scanned += 1;
            if p >= limit
                || self.anchor_pages.contains(&p)
                || self.resize_reserve.contains(&p)
                || self.live.contains(&p)
                || txn_alloc.contains(&p)
            {
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

    /// Physical pages referenced by the committed state: data leaves, PMAP
    /// nodes and the pages holding logical free-list links (a logically freed
    /// page still occupies a physical page — [`Pager::free_page`] writes the
    /// link image into it).
    pub fn live_pages(&self) -> u64 {
        self.live.len() as u64
    }

    /// Physical pages available to future commits: the volume size less the
    /// committed live set and the reserved superblock/anchor-ring pages.
    /// Reflects the last *committed* state — pages staged in the active
    /// transaction are not charged until [`Pager::commit`]. Note that a commit
    /// transiently needs old+new copies of every changed page plus fresh PMAP
    /// nodes, so a commit can fail with [`StoreError::OutOfSpace`] before this
    /// reaches zero.
    pub fn free_pages(&self) -> u64 {
        self.sb
            .total_pages
            .saturating_sub(self.live.len() as u64)
            .saturating_sub(self.anchor_pages.len() as u64)
    }

    /// Change the recorded volume size. Only meaningful for a same-size version
    /// re-stamp (the upgrade path); a genuine size change alters the anchor-ring
    /// geometry and must go through [`Pager::resize`].
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

    // ---- in-place resize ----------------------------------------------------

    /// Pages the underlying medium can hold. The volume (`total_pages`) may be
    /// smaller — the difference is the room [`Pager::resize`] can expand into.
    pub fn device_pages(&self) -> u64 {
        self.dev.sector_count() / PAGE_SECTORS
    }

    /// Smallest volume size an in-place [`Pager::resize`] can shrink to right
    /// now: every logical page must stay addressable (`hwm` — logical pages are
    /// never renumbered), and every live physical page plus the target anchor
    /// reserve and the relocation headroom must fit.
    pub fn min_total_pages(&self) -> u64 {
        (FIRST_DATA_PAGE + 2)
            .max(self.sb.hwm)
            .max(self.live.len() as u64 + N_ANCHOR + 1 + RESIZE_HEADROOM)
    }

    /// Grow or shrink the volume **in place** to `new_total` pages, preserving
    /// all content. Requires no active transaction.
    ///
    /// The anchor ring is strided across `total_pages`, so a resize is a
    /// geometry migration, ordered so a power cut at any point leaves a volume
    /// that mounts to the current content at *either* the old or the new size:
    ///
    /// 1. Relocate (normal COW commits, at the old geometry) every live page
    ///    that the new geometry cannot keep: pages at or beyond
    ///    `min(old, new)` and pages the new anchor ring will occupy. The
    ///    allocator is constrained to pages valid at **both** geometries.
    /// 2. Grow the PMAP with wrapper root nodes if the new size needs more
    ///    depth (written to free pages — dangling until step 5).
    /// 3. Park a fresh copy of the newest old-geometry anchor in a ring slot
    ///    that does not collide with any new-geometry slot position.
    /// 4. Zero every new-geometry slot: a stale but valid anchor left at those
    ///    sectors by a *previous volume* that lived at this exact geometry
    ///    must not win after the flip. Step 3 keeps the old geometry mountable
    ///    through this.
    /// 5. Pre-publish the migrated state as one anchor record at the new
    ///    geometry (unreachable so far — mount still reads the old super).
    /// 6. Rewrite the sector-0 volume superblock with the new `total_pages` —
    ///    the atomic flip. Before it, mount uses the old ring; after it, the
    ///    new ring, and both name the same content.
    ///
    /// On any error the volume is rolled back to its old size, fully intact
    /// (relocation commits that already happened are content-neutral).
    pub fn resize(&mut self, new_total: u64) -> Result<()> {
        if !self.dirty.is_empty() {
            return Err(StoreError::Corrupt("resize during open transaction"));
        }
        let old_total = self.sb.total_pages;
        if new_total == old_total {
            return Ok(());
        }
        if new_total > self.device_pages() {
            return Err(StoreError::OutOfSpace);
        }
        if new_total < old_total && new_total < self.min_total_pages() {
            return Err(StoreError::OutOfSpace);
        }
        self.resize_reserve = Self::anchor_pages_for(new_total);
        self.resize_limit = Some(old_total.min(new_total));
        let r = self.resize_migrate(new_total);
        if r.is_err() {
            self.resize_reserve.clear();
            self.resize_limit = None;
            // Relocation commits were content-neutral; this only restores the
            // logical header/cursor after a half-staged transaction.
            let _ = self.rollback();
        }
        r
    }

    /// The fallible body of [`Pager::resize`]; constraints are already set.
    fn resize_migrate(&mut self, new_total: u64) -> Result<()> {
        let old_total = self.sb.total_pages;
        let limit = old_total.min(new_total);
        // 1. Relocate. A pass rewrites every offending page through the
        // constrained allocator; rewriting a leaf also rewrites its whole PMAP
        // path, so one victim leaf per offending *node* relocates the node.
        // Fresh pages are never offending, so pass 2 is normally empty; the
        // cap only guards against a logic bug looping forever.
        for round in 0.. {
            let victims = self.resize_victims(limit)?;
            if victims.is_empty() {
                break;
            }
            if round >= 8 {
                return Err(StoreError::Corrupt("resize relocation did not converge"));
            }
            let mut staged = 0usize;
            for &lpn in &victims {
                let img = self.read_page(lpn)?;
                self.write_page(lpn, img);
                staged += 1;
                if staged % RESIZE_CHUNK == 0 {
                    self.commit()?;
                }
            }
            self.commit()?;
        }
        // 2. Wrapper nodes if the new size needs a taller PMAP. They live on
        // free pages and stay unreferenced (harmless if we crash) until the
        // new-geometry anchor in step 5 names the new root.
        let new_depth = Self::pmap_depth_for(new_total).max(self.pmap_depth);
        let mut new_root = self.pmap_root;
        let mut wrap_alloc: BTreeSet<u64> = BTreeSet::new();
        if new_root != 0 {
            for _ in self.pmap_depth..new_depth {
                let ppn = self.alloc_ppn(&mut wrap_alloc)?;
                let mut img = zeroed_page();
                put_u64(&mut img, 0, new_root);
                journal::write_page(&mut self.dev, ppn, &img)?;
                new_root = ppn;
            }
            if !wrap_alloc.is_empty() {
                self.dev.flush()?;
            }
        }
        // 3. Park the newest old-geometry anchor where step 4 won't zero it,
        // so a power cut below still mounts to exactly this state.
        let new_positions: BTreeSet<u64> = (0..N_ANCHOR)
            .map(|slot| Self::anchor_page(slot, new_total))
            .collect();
        let mut gen = self.sb.generation + 1;
        let mut tries = 0;
        while new_positions.contains(&Self::anchor_page(gen % N_ANCHOR, old_total)) {
            gen += 1;
            tries += 1;
            if tries > 2 * N_ANCHOR {
                return Err(StoreError::Corrupt("resize anchor rings fully overlap"));
            }
        }
        self.write_anchor(gen, self.pmap_root, self.write_cursor)?;
        self.sb.generation = gen;
        // 4. Invalidate the whole target ring.
        let zero = [0u8; SECTOR];
        for slot in 0..N_ANCHOR {
            self.dev
                .write_sector(Self::anchor_sector(slot, new_total), &zero)?;
        }
        // 5. Pre-publish the migrated state at the target geometry.
        let cursor = self.write_cursor % new_total;
        self.write_anchor_geom(gen + 1, new_depth, new_root, cursor, new_total)?;
        // 6. The atomic flip.
        Self::write_super(&mut self.dev, new_total)?;
        // Adopt the new geometry in RAM.
        self.sb.total_pages = new_total;
        self.sb.generation = gen + 1;
        self.pmap_root = new_root;
        self.pmap_depth = new_depth;
        self.write_cursor = cursor;
        self.anchor_pages = Self::anchor_pages_for(new_total);
        self.live.append(&mut wrap_alloc);
        self.resize_reserve = BTreeSet::new();
        self.resize_limit = None;
        self.node_cache.clear();
        Ok(())
    }

    /// Logical pages whose rewrite would evict everything the target geometry
    /// cannot keep: leaves whose data page offends, plus one representative
    /// leaf under every offending PMAP node (its COW rewrite renews the node).
    /// Offending = at/beyond `limit` or on a `resize_reserve` page.
    fn resize_victims(&mut self, limit: u64) -> Result<BTreeSet<u64>> {
        let mut out = BTreeSet::new();
        for lpn in 0..self.sb.hwm {
            if let Some(ppn) = self.translate(lpn)? {
                if ppn >= limit || self.resize_reserve.contains(&ppn) {
                    out.insert(lpn);
                }
            }
        }
        let (depth, root) = (self.pmap_depth, self.pmap_root);
        self.node_victims(depth, root, 0, limit, &mut out)?;
        Ok(out)
    }

    fn node_victims(
        &mut self,
        height: u32,
        node_ppn: u64,
        base_lpn: u64,
        limit: u64,
        out: &mut BTreeSet<u64>,
    ) -> Result<()> {
        if node_ppn == 0 {
            return Ok(());
        }
        if node_ppn >= limit || self.resize_reserve.contains(&node_ppn) {
            if let Some(lpn) = self.first_mapped_under(height, node_ppn, base_lpn)? {
                out.insert(lpn);
            }
        }
        if height > 1 {
            let span = (F as u64).pow(height - 1);
            let node = self.node_image(node_ppn, true)?;
            for i in 0..F {
                let child = get_u64(&node, i * 8);
                if child != 0 {
                    self.node_victims(height - 1, child, base_lpn + i as u64 * span, limit, out)?;
                }
            }
        }
        Ok(())
    }

    /// First mapped `lpn` in the subtree — every allocated node has one, since
    /// entries are set on write and never cleared (a logical free keeps its
    /// page mapped to the free-list link).
    fn first_mapped_under(&mut self, height: u32, node_ppn: u64, base_lpn: u64) -> Result<Option<u64>> {
        let node = self.node_image(node_ppn, true)?;
        if height == 1 {
            for i in 0..F {
                if get_u64(&node, i * 8) != 0 {
                    return Ok(Some(base_lpn + i as u64));
                }
            }
            return Ok(None);
        }
        let span = (F as u64).pow(height - 1);
        for i in 0..F {
            let child = get_u64(&node, i * 8);
            if child != 0 {
                if let Some(l) = self.first_mapped_under(height - 1, child, base_lpn + i as u64 * span)? {
                    return Ok(Some(l));
                }
            }
        }
        Ok(None)
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

    /// Free-space accounting: `free + live + reserved == total` holds across
    /// alloc/free/commit cycles, COW rewrites don't leak pages, a logical free
    /// keeps the page physically charged (it holds the free-list link), and the
    /// figures survive a remount.
    #[test]
    fn free_space_accounting_tracks_commits() {
        let mut pg = Pager::format(dev(16)).unwrap();
        let total = pg.sb.total_pages;
        assert_eq!(pg.live_pages(), 0);
        // With nothing live, everything but the superblock/anchor reserve is free.
        let reserved = total - pg.free_pages();
        assert!(reserved > 0 && reserved <= N_ANCHOR + 1);

        // Committing 10 data pages charges 10 leaves plus their PMAP nodes.
        let before = pg.free_pages();
        let mut pages = Vec::new();
        for _ in 0..10u64 {
            let p = pg.alloc_page().unwrap();
            pg.write_page(p, vec![0xAB; PAGE]);
            pages.push(p);
        }
        pg.commit().unwrap();
        assert_eq!(pg.free_pages() + pg.live_pages() + reserved, total);
        assert!(before - pg.free_pages() >= 10);

        // COW-rewriting the same logical pages must not leak: the old physical
        // pages are freed as the new ones are charged.
        let live_settled = pg.live_pages();
        for &p in &pages {
            pg.write_page(p, vec![0xCD; PAGE]);
        }
        pg.commit().unwrap();
        assert_eq!(pg.live_pages(), live_settled);

        // A logically freed page stays physically charged (free-list link).
        pg.free_page(pages[0]);
        pg.commit().unwrap();
        assert_eq!(pg.live_pages(), live_settled);

        // Reusing it from the free list consumes no extra physical pages.
        let reused = pg.alloc_page().unwrap();
        assert_eq!(reused, pages[0]);
        pg.write_page(reused, vec![0xEF; PAGE]);
        pg.commit().unwrap();
        assert_eq!(pg.live_pages(), live_settled);
        assert_eq!(pg.free_pages() + pg.live_pages() + reserved, total);

        // Remount rebuilds the live-set to the exact same accounting.
        let free = pg.free_pages();
        let snap = pg.device_mut().snapshot();
        let pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_eq!(pg.live_pages(), live_settled);
        assert_eq!(pg.free_pages(), free);
    }

    /// Rewrite `pages` (same content) in commit batches until the allocator
    /// cursor has swept past `past`, scattering their physical homes across the
    /// device. `past` must be well below `total_pages` so the cursor can't wrap.
    fn sweep_cursor_past(pg: &mut Pager<MemBlockDevice>, pages: &[u64], past: u64) {
        assert!(past < pg.sb.total_pages);
        while pg.write_cursor < past {
            for &p in pages {
                let img = pg.read_page(p).unwrap();
                pg.write_page(p, img);
            }
            pg.commit().unwrap();
        }
    }

    fn marker(i: u64) -> Page {
        let mut img = zeroed_page();
        img[0..8].copy_from_slice(&(0xC0FF_EE00u64 + i).to_le_bytes());
        img
    }

    fn assert_markers(pg: &mut Pager<MemBlockDevice>, pages: &[u64], ctx: &str) {
        for (i, &p) in pages.iter().enumerate() {
            assert_eq!(
                &pg.read_page(p).unwrap()[0..8],
                &(0xC0FF_EE00u64 + i as u64).to_le_bytes()[..],
                "marker {i} lost ({ctx})"
            );
        }
    }

    fn accounting_holds(pg: &Pager<MemBlockDevice>) -> bool {
        pg.free_pages() + pg.live_pages() + pg.anchor_pages.len() as u64 == pg.sb.total_pages
    }

    /// Expanding into spare medium grows capacity in place: data survives, the
    /// new pages are allocatable (hwm can march past the old volume size), and
    /// it all survives a remount.
    #[test]
    fn resize_expand_preserves_data_and_grows_capacity() {
        let mut pg = Pager::format(dev(16)).unwrap(); // 4096 pages
        let mut pages = Vec::new();
        for i in 0..40u64 {
            let p = pg.alloc_page().unwrap();
            pg.write_page(p, marker(i));
            pages.push(p);
        }
        pg.commit().unwrap();
        let old_total = pg.sb.total_pages;
        let mut snap = pg.device_mut().snapshot();
        // The image lands on a bigger stick (odd size: no sector alignment).
        snap.resize(snap.len() + 8 * 1024 * 1024 + 9001 * SECTOR, 0);

        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        let free_before = pg.free_pages();
        let target = pg.device_pages();
        pg.resize(target).unwrap();
        pg.resize(target).unwrap(); // same-size resize is a no-op
        assert_eq!(pg.sb.total_pages, target);
        assert!(pg.free_pages() > free_before + (target - old_total) - 70);
        assert!(accounting_holds(&pg));
        assert_markers(&mut pg, &pages, "after expand");

        // Fill past the old capacity: the expanded logical space is real.
        let mut extra = Vec::new();
        while pg.sb.hwm <= old_total + 50 {
            let p = pg.alloc_page().unwrap();
            pg.write_page(p, marker(1000 + p));
            extra.push(p);
            if extra.len() % 256 == 0 {
                pg.commit().unwrap();
            }
        }
        pg.commit().unwrap();
        let snap = pg.device_mut().snapshot();
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_eq!(pg.sb.total_pages, target);
        assert_markers(&mut pg, &pages, "after remount");
        for &p in extra.iter().rev().take(5) {
            assert_eq!(
                &pg.read_page(p).unwrap()[0..8],
                &(0xC0FF_EE00u64 + 1000 + p).to_le_bytes()[..]
            );
        }
        assert!(accounting_holds(&pg));
    }

    /// Shrinking relocates every physical page above the new size (and off the
    /// new anchor ring), preserving all content; shrinking below the content is
    /// refused with the volume untouched; resize inside an open transaction is
    /// refused.
    #[test]
    fn resize_shrink_relocates_and_preserves_data() {
        let mut pg = Pager::format(dev(32)).unwrap(); // 8192 pages
        let mut pages = Vec::new();
        for i in 0..300u64 {
            let p = pg.alloc_page().unwrap();
            pg.write_page(p, marker(i));
            pages.push(p);
        }
        pg.commit().unwrap();
        sweep_cursor_past(&mut pg, &pages, 4500);
        assert!(
            pg.live.iter().any(|&p| p >= 4096),
            "setup failed: nothing lives above the shrink target"
        );

        // Refused mid-transaction, leaving the staged write intact.
        let img = pg.read_page(pages[0]).unwrap();
        pg.write_page(pages[0], img);
        assert!(matches!(pg.resize(4096), Err(StoreError::Corrupt(_))));
        pg.rollback().unwrap();

        // Refused below the content, volume untouched.
        assert!(matches!(
            pg.resize(pg.min_total_pages() - 1),
            Err(StoreError::OutOfSpace)
        ));
        assert_eq!(pg.sb.total_pages, 8192);
        assert_markers(&mut pg, &pages, "after refused shrink");

        pg.resize(4096).unwrap();
        assert_eq!(pg.sb.total_pages, 4096);
        assert!(pg.live.iter().next_back().map_or(true, |&p| p < 4096));
        assert!(accounting_holds(&pg));
        assert_markers(&mut pg, &pages, "after shrink");

        let snap = pg.device_mut().snapshot();
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_eq!(pg.sb.total_pages, 4096);
        assert_markers(&mut pg, &pages, "after remount");
        let p = pg.alloc_page().unwrap();
        pg.write_page(p, marker(9999));
        pg.commit().unwrap();
        assert!(accounting_holds(&pg));
    }

    /// Expanding across the fanout boundary (512² pages = 1 GiB) must grow the
    /// PMAP a level, and pages beyond the old addressable range must work.
    /// (~1 GiB RAM-backed device — the realistic \"64 MiB image flashed onto a
    /// multi-GiB stick, then expanded\" path.)
    #[test]
    fn resize_expand_across_pmap_depth_boundary() {
        let mut pg = Pager::format(dev(16)).unwrap(); // 4096 pages, depth 2
        let p = pg.alloc_page().unwrap();
        pg.write_page(p, marker(0));
        pg.commit().unwrap();
        assert_eq!(pg.pmap_depth, 2);
        let mut snap = pg.device_mut().snapshot();
        drop(pg);
        let target = (F as u64) * (F as u64) + 256; // just past depth-2 capacity
        snap.resize((target * PAGE_SECTORS) as usize * SECTOR, 0);

        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        pg.resize(target).unwrap();
        assert_eq!(pg.pmap_depth, 3);
        assert_eq!(&pg.read_page(p).unwrap()[0..8], &0xC0FF_EE00u64.to_le_bytes()[..]);

        // Address a page beyond the old depth-2 range. (Pager-level test:
        // write_page bypasses alloc_page's hwm bookkeeping on purpose.)
        let high = (F as u64) * (F as u64) + 100;
        pg.write_page(high, marker(7));
        pg.commit().unwrap();
        let snap = pg.device_mut().snapshot();
        drop(pg);
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_eq!(pg.pmap_depth, 3);
        assert_eq!(&pg.read_page(high).unwrap()[0..8], &0xC0FF_EE07u64.to_le_bytes()[..]);
        assert_eq!(&pg.read_page(p).unwrap()[0..8], &0xC0FF_EE00u64.to_le_bytes()[..]);
        assert!(accounting_holds(&pg));
    }

    /// Sweep the power-cut point across an entire shrink and assert the volume
    /// always remounts — at the old or the new size — with every page intact
    /// and still writable. The flip must be atomic exactly like a commit.
    #[test]
    fn resize_survives_power_cut_at_every_point() {
        let mut pg = Pager::format(dev(16)).unwrap(); // 4096 pages
        let mut pages = Vec::new();
        for i in 0..60u64 {
            let p = pg.alloc_page().unwrap();
            pg.write_page(p, marker(i));
            pages.push(p);
        }
        pg.commit().unwrap();
        sweep_cursor_past(&mut pg, &pages, 3000);
        let base = pg.device_mut().snapshot();
        drop(pg);

        // How many device writes does the uncut shrink take?
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(base.clone())).unwrap();
        assert!(pg.min_total_pages() <= 2560, "setup: shrink target too tight");
        pg.resize(2560).unwrap();
        assert_markers(&mut pg, &pages, "uncut shrink");
        let writes = pg.device_mut().writes;
        drop(pg);

        for cut in 1..=writes {
            let mut pg =
                Pager::mount(MemBlockDevice::from_snapshot(base.clone())).unwrap();
            pg.device_mut().cut_after = Some(cut);
            let _ = pg.resize(2560); // power dies mid-way; outcome irrelevant
            let snap = pg.device_mut().snapshot();
            drop(pg);
            let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
            let total = pg.sb.total_pages;
            assert!(
                total == 4096 || total == 2560,
                "cut {cut}: impossible volume size {total}"
            );
            assert_markers(&mut pg, &pages, &format!("cut {cut}"));
            assert!(accounting_holds(&pg), "cut {cut}: accounting broken");
            let np = pg.alloc_page().unwrap();
            pg.write_page(np, marker(500));
            pg.commit().unwrap();
        }
    }

    /// A previous, larger TablesOS volume on the same stick leaves valid
    /// high-generation anchors at exactly the slot positions the expanded
    /// geometry will use. The resize must invalidate them, or the first
    /// remount would time-travel to the dead volume's state.
    #[test]
    fn resize_ignores_stale_anchors_from_a_previous_volume() {
        // The dead volume: full 32 MiB geometry, forged huge generations in
        // ring slots whose pages lie beyond the 16 MiB image flashed next.
        let mut old = Pager::format(dev(32)).unwrap(); // 8192 pages
        let p = old.alloc_page().unwrap();
        old.write_page(p, vec![0xEE; PAGE]);
        old.commit().unwrap();
        for g in 9_000_060..9_000_064u64 {
            let (root, cur) = (old.pmap_root, old.write_cursor);
            old.write_anchor(g, root, cur).unwrap();
        }
        let mut medium = old.device_mut().snapshot();
        drop(old);

        // Flash a fresh 16 MiB image over the front of the stick.
        let mut small = Pager::format(dev(16)).unwrap(); // 4096 pages
        let q = small.alloc_page().unwrap();
        small.write_page(q, marker(42));
        small.commit().unwrap();
        let image = small.device_mut().snapshot();
        drop(small);
        medium[..image.len()].copy_from_slice(&image);
        {
            // Sanity: the forged anchors really did survive the flash.
            let survivor = Pager::<MemBlockDevice>::anchor_sector(60, 8192) as usize * SECTOR;
            assert!(survivor >= image.len());
            assert_eq!(get_u32(&medium[survivor..survivor + 4], 0), ANCHOR_MAGIC);
        }

        // Expand the flashed volume back onto the dead volume's exact geometry.
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(medium)).unwrap();
        pg.resize(8192).unwrap();
        assert_eq!(&pg.read_page(q).unwrap()[0..8], &0xC0FF_EE2Au64.to_le_bytes()[..]);
        let snap = pg.device_mut().snapshot();
        drop(pg);
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert!(pg.sb.generation < 9_000_000, "stale anchor resurrected");
        assert_eq!(&pg.read_page(q).unwrap()[0..8], &0xC0FF_EE2Au64.to_le_bytes()[..]);
        assert!(accounting_holds(&pg));
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
