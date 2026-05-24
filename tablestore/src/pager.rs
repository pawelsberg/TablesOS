//! 4 KiB pages on top of a [`BlockDevice`]: the superblock, a free-list
//! allocator, and a one-transaction-at-a-time write buffer that commits
//! through the [`journal`].
//!
//! TablesOS is single-user and every engine operation is exactly one
//! transaction, so a single in-flight transaction is all that is ever needed.

use crate::block::BlockDevice;
use crate::journal::{self, FIRST_DATA_PAGE, PAGE};
use crate::{Result, StoreError};
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

const SB_MAGIC: u32 = 0x53_4c_42_54; // "TBLS" LE
const SB_VERSION: u32 = 1;

/// A heap page image. Heap-allocated to keep large arrays off the kernel stack.
pub type Page = Vec<u8>;

pub fn zeroed_page() -> Page {
    vec![0u8; PAGE]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub total_pages: u64,
    pub hwm: u64,          // next never-used page
    pub free_head: u64,    // head of free-page list, 0 = none
    pub catalog_head: u64, // head of catalog chain, 0 = no tables
    pub generation: u64,
}

impl Superblock {
    fn encode(&self) -> Page {
        let mut p = zeroed_page();
        p[0..4].copy_from_slice(&SB_MAGIC.to_le_bytes());
        p[4..8].copy_from_slice(&SB_VERSION.to_le_bytes());
        p[8..12].copy_from_slice(&(PAGE as u32).to_le_bytes());
        // p[12..16] = crc, filled last
        p[16..24].copy_from_slice(&self.total_pages.to_le_bytes());
        p[24..32].copy_from_slice(&self.hwm.to_le_bytes());
        p[32..40].copy_from_slice(&self.free_head.to_le_bytes());
        p[40..48].copy_from_slice(&self.catalog_head.to_le_bytes());
        p[48..56].copy_from_slice(&self.generation.to_le_bytes());
        let crc = journal::crc32(&p[16..56]);
        p[12..16].copy_from_slice(&crc.to_le_bytes());
        p
    }

    fn decode(p: &[u8]) -> Result<Superblock> {
        if u32::from_le_bytes(p[0..4].try_into().unwrap()) != SB_MAGIC {
            return Err(StoreError::Corrupt("not a TablesOS volume"));
        }
        let crc = u32::from_le_bytes(p[12..16].try_into().unwrap());
        if journal::crc32(&p[16..56]) != crc {
            return Err(StoreError::Corrupt("superblock checksum"));
        }
        Ok(Superblock {
            total_pages: u64::from_le_bytes(p[16..24].try_into().unwrap()),
            hwm: u64::from_le_bytes(p[24..32].try_into().unwrap()),
            free_head: u64::from_le_bytes(p[32..40].try_into().unwrap()),
            catalog_head: u64::from_le_bytes(p[40..48].try_into().unwrap()),
            generation: u64::from_le_bytes(p[48..56].try_into().unwrap()),
        })
    }
}

/// Upper bound on cached clean pages (`CACHE_MAX * PAGE` = 32 MiB). Browsing a
/// table re-reads it on every keystroke; without a cache that is thousands of
/// polled PIO sector reads per key and the GUI appears to freeze. A whole-table
/// scan stays under this for any realistic table, so it lives entirely in RAM
/// after the first read; if it ever overflows the cache is simply dropped.
const CACHE_MAX: usize = 8192;

pub struct Pager<D: BlockDevice> {
    dev: D,
    pub sb: Superblock,
    /// Staged page images for the active transaction (also a read cache so the
    /// transaction sees its own writes). Keyed by page number.
    dirty: BTreeMap<u64, Page>,
    /// Read-through cache of committed (clean) pages, keyed by page number.
    /// Always holds the current on-disk content: refreshed on commit, untouched
    /// by rollback (the committed state it mirrors does not change there).
    cache: BTreeMap<u64, Page>,
}

impl<D: BlockDevice> Pager<D> {
    /// Lay down a fresh, empty volume on `dev`.
    pub fn format(mut dev: D) -> Result<Pager<D>> {
        let total_pages = dev.sector_count() / journal::PAGE_SECTORS;
        if total_pages <= FIRST_DATA_PAGE + 1 {
            return Err(StoreError::OutOfSpace);
        }
        journal::init(&mut dev)?;
        let sb = Superblock {
            total_pages,
            hwm: FIRST_DATA_PAGE,
            free_head: 0,
            catalog_head: 0,
            generation: 1,
        };
        journal::write_page(&mut dev, 0, &sb.encode())?;
        dev.flush()?;
        Ok(Pager {
            dev,
            sb,
            dirty: BTreeMap::new(),
            cache: BTreeMap::new(),
        })
    }

    /// Open an existing volume, replaying any committed-but-not-checkpointed
    /// transaction first.
    pub fn mount(mut dev: D) -> Result<Pager<D>> {
        journal::recover(&mut dev)?;
        let mut p = zeroed_page();
        journal::read_page(&mut dev, 0, &mut p)?;
        let sb = Superblock::decode(&p)?;
        Ok(Pager {
            dev,
            sb,
            dirty: BTreeMap::new(),
            cache: BTreeMap::new(),
        })
    }

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.dev
    }

    /// Read a page, honouring uncommitted writes from the active transaction,
    /// then the clean-page cache, then the disk (populating the cache).
    pub fn read_page(&mut self, page: u64) -> Result<Page> {
        if let Some(p) = self.dirty.get(&page) {
            return Ok(p.clone());
        }
        if let Some(p) = self.cache.get(&page) {
            return Ok(p.clone());
        }
        if page >= self.sb.total_pages {
            return Err(StoreError::Corrupt("page out of range"));
        }
        let mut buf = zeroed_page();
        journal::read_page(&mut self.dev, page, &mut buf)?;
        if self.cache.len() >= CACHE_MAX {
            self.cache.clear();
        }
        self.cache.insert(page, buf.clone());
        Ok(buf)
    }

    /// Stage a full page image into the active transaction.
    pub fn write_page(&mut self, page: u64, data: Page) {
        debug_assert!(data.len() == PAGE);
        self.dirty.insert(page, data);
    }

    /// Allocate a data page: reuse a freed page if any, else extend the
    /// high-water mark. The returned page is staged as all-zeros.
    pub fn alloc_page(&mut self) -> Result<u64> {
        let page = if self.sb.free_head != 0 {
            let p = self.sb.free_head;
            let img = self.read_page(p)?;
            let next = u64::from_le_bytes(img[0..8].try_into().unwrap());
            self.sb.free_head = next;
            p
        } else if self.sb.hwm < self.sb.total_pages {
            let p = self.sb.hwm;
            self.sb.hwm += 1;
            p
        } else {
            return Err(StoreError::OutOfSpace);
        };
        self.write_page(page, zeroed_page());
        Ok(page)
    }

    /// Return a page to the free list (effective at commit).
    pub fn free_page(&mut self, page: u64) {
        let mut img = zeroed_page();
        img[0..8].copy_from_slice(&self.sb.free_head.to_le_bytes());
        self.write_page(page, img);
        self.sb.free_head = page;
    }

    /// Commit the active transaction atomically. The superblock is always
    /// part of the write-set so allocator/catalog changes land with the data.
    pub fn commit(&mut self) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let txid = self.sb.generation + 1;
        let mut sb = self.sb;
        sb.generation = txid;
        let mut batch: Vec<(u64, Vec<u8>)> = Vec::with_capacity(self.dirty.len() + 1);
        batch.push((0u64, sb.encode()));
        for (p, img) in &self.dirty {
            debug_assert!(*p != 0);
            batch.push((*p, img.clone()));
        }
        journal::commit(&mut self.dev, txid, &batch)?;
        self.sb = sb;
        // The just-committed images are now the clean on-disk content: fold
        // them into the read cache (keeping it warm and correct) before the
        // staging set is dropped. Page 0 (the superblock) is never cached.
        for (p, img) in &self.dirty {
            if self.cache.len() >= CACHE_MAX {
                self.cache.clear();
            }
            self.cache.insert(*p, img.clone());
        }
        self.dirty.clear();
        Ok(())
    }

    /// Discard the active transaction.
    pub fn rollback(&mut self) -> Result<()> {
        self.dirty.clear();
        let mut p = zeroed_page();
        journal::read_page(&mut self.dev, 0, &mut p)?;
        self.sb = Superblock::decode(&p)?;
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

        // New txn; cut power on the very first write (well before commit).
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
    fn clean_commit_is_durable_and_recover_is_noop() {
        let mut pg = Pager::format(dev(16)).unwrap();
        let p = pg.alloc_page().unwrap();
        let mut img = zeroed_page();
        img[0..4].copy_from_slice(b"keep");
        pg.write_page(p, img);
        pg.commit().unwrap();
        let snap = pg.device_mut().snapshot();

        // Remount twice: recovery on a cleanly-committed volume must be a
        // no-op and never disturb the data (idempotent recovery).
        let snap2 = {
            let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
            assert_eq!(&pg.read_page(p).unwrap()[0..4], b"keep");
            pg.device_mut().snapshot()
        };
        let mut pg = Pager::mount(MemBlockDevice::from_snapshot(snap2)).unwrap();
        assert_eq!(&pg.read_page(p).unwrap()[0..4], b"keep");
    }
}
