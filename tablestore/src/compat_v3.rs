//! **Frozen** read-only reader for the v0.3.0 (and bit-compatible v0.1.0/v0.2.0)
//! on-disk format — the journalled, in-place "home page" layout that the
//! copy-on-write pager ([`crate::pager`]) replaced in v0.4.0.
//!
//! This module exists for exactly one purpose: the migration step
//! ([`crate::migrate`]) opens an old volume through here and copies its logical
//! pages into a freshly-formatted CoW volume. It is therefore **byte-frozen** —
//! it must keep reading volumes written by the retired format forever, so it is
//! deliberately self-contained (its own constants + CRC) and does not share code
//! with the evolving [`crate::journal`]/[`crate::pager`] modules.
//!
//! It is read-only with one exception: [`V3Volume::mount`] replays a
//! committed-but-not-checkpointed journal into the home pages first (an
//! idempotent redo), so a volume that lost power mid-checkpoint still migrates
//! its last committed transaction. That redo writes to the *old* volume, which is
//! being superseded anyway.

use crate::block::{BlockDevice, SECTOR};
use crate::{Result, StoreError};
use alloc::vec;
use alloc::vec::Vec;

const PAGE: usize = 4096;
const PAGE_SECTORS: u64 = (PAGE / SECTOR) as u64;

// ---- journal control geometry (frozen v0.3.0) ------------------------------
const JCTRL_PAGES: u64 = 4;
const JCTRL_SECTORS: usize = (JCTRL_PAGES * PAGE_SECTORS) as usize; // 32
const HEADER_LEN: usize = 32;
const JCAP: usize = ((PAGE - HEADER_LEN) / 8) + 3 * (PAGE / 8); // 2044
const JDATA_START_PAGE: u64 = 1 + JCTRL_PAGES; // 5
const JDATA_PAGES: u64 = JCAP as u64; // 2044
const JCTRL_FIRST_SECTOR: u64 = PAGE_SECTORS; // page 1

const J_MAGIC: u32 = 0x4a_4c_42_54; // "TBLJ" LE
const STATE_COMMITTED: u32 = 1;

// ---- superblock geometry (frozen v0.3.0) -----------------------------------
const SB_MAGIC: u32 = 0x53_4c_42_54; // "TBLS" LE
const SB_SLOTS: usize = (PAGE / SECTOR) as usize; // 8

/// Bitwise CRC-32 (IEEE) — identical to the retired `journal::crc32`.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn read_page(dev: &mut dyn BlockDevice, page: u64, out: &mut [u8]) -> Result<()> {
    debug_assert!(out.len() == PAGE);
    dev.read_blocks(page * PAGE_SECTORS, out)
}

fn write_page(dev: &mut dyn BlockDevice, page: u64, data: &[u8]) -> Result<()> {
    debug_assert!(data.len() == PAGE);
    for i in 0..PAGE_SECTORS {
        let o = i as usize * SECTOR;
        dev.write_sector(page * PAGE_SECTORS + i, &data[o..o + SECTOR])?;
    }
    Ok(())
}

struct Ctrl {
    state: u32,
    txid: u64,
    entries: Vec<u64>,
    jdata_base: u64,
    ring_start: usize,
}

/// Newest valid control header in the 32-sector ring (highest txid), or `None`.
fn scan_ctrl(dev: &mut dyn BlockDevice) -> Result<Option<Ctrl>> {
    let ring_bytes = JCTRL_SECTORS * SECTOR;
    let mut ring = vec![0u8; ring_bytes];
    for p in 0..JCTRL_PAGES {
        let mut pg = [0u8; PAGE];
        read_page(dev, 1 + p, &mut pg)?;
        let o = p as usize * PAGE;
        ring[o..o + PAGE].copy_from_slice(&pg);
    }
    let at = |idx: usize| ring[idx % ring_bytes];
    let mut best: Option<Ctrl> = None;
    for h in 0..JCTRL_SECTORS {
        let off = h * SECTOR;
        if get_u32(&ring, off) != J_MAGIC {
            continue;
        }
        let state = get_u32(&ring, off + 8);
        let stored_crc = get_u32(&ring, off + 12);
        let txid = get_u64(&ring, off + 16);
        let count = get_u32(&ring, off + 24) as usize;
        let jdata_base = get_u32(&ring, off + 28) as u64;
        if count > JCAP {
            continue;
        }
        let mut entries = Vec::with_capacity(count);
        let mut crc_in = Vec::with_capacity(16 + count * 8);
        crc_in.extend_from_slice(&txid.to_le_bytes());
        crc_in.extend_from_slice(&(count as u32).to_le_bytes());
        crc_in.extend_from_slice(&(jdata_base as u32).to_le_bytes());
        for i in 0..count {
            let base = off + HEADER_LEN + i * 8;
            let mut e = [0u8; 8];
            for (b, slot) in e.iter_mut().enumerate() {
                *slot = at(base + b);
            }
            crc_in.extend_from_slice(&e);
            entries.push(u64::from_le_bytes(e));
        }
        if crc32(&crc_in) != stored_crc {
            continue;
        }
        if best.as_ref().map_or(true, |b| txid > b.txid) {
            best = Some(Ctrl {
                state,
                txid,
                entries,
                jdata_base,
                ring_start: h,
            });
        }
    }
    Ok(best)
}

fn render_ctrl_release(entries: &[u64], txid: u64, jdata_base: u64) -> Vec<u8> {
    let mut buf = vec![0u8; JCTRL_PAGES as usize * PAGE];
    for (i, e) in entries.iter().enumerate() {
        buf[HEADER_LEN + i * 8..HEADER_LEN + i * 8 + 8].copy_from_slice(&e.to_le_bytes());
    }
    let mut crc_in = Vec::with_capacity(16 + entries.len() * 8);
    crc_in.extend_from_slice(&txid.to_le_bytes());
    crc_in.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    crc_in.extend_from_slice(&(jdata_base as u32).to_le_bytes());
    for e in entries {
        crc_in.extend_from_slice(&e.to_le_bytes());
    }
    let crc = crc32(&crc_in);
    buf[0..4].copy_from_slice(&J_MAGIC.to_le_bytes());
    // version + state(=EMPTY=0) left as-is; release only needs valid magic/crc.
    buf[8..12].copy_from_slice(&0u32.to_le_bytes()); // STATE_EMPTY
    buf[12..16].copy_from_slice(&crc.to_le_bytes());
    buf[16..24].copy_from_slice(&txid.to_le_bytes());
    buf[24..28].copy_from_slice(&(entries.len() as u32).to_le_bytes());
    buf[28..32].copy_from_slice(&(jdata_base as u32).to_le_bytes());
    buf
}

/// Replay a committed-but-not-checkpointed transaction into home pages, then
/// release it. Idempotent. Mirrors the retired `journal::recover`.
fn recover(dev: &mut dyn BlockDevice) -> Result<()> {
    let ctrl = match scan_ctrl(dev)? {
        Some(c) => c,
        None => return Ok(()),
    };
    if ctrl.state != STATE_COMMITTED {
        return Ok(());
    }
    let mut page = [0u8; PAGE];
    for (i, target) in ctrl.entries.iter().enumerate() {
        let src = JDATA_START_PAGE + (ctrl.jdata_base + i as u64) % JDATA_PAGES;
        read_page(dev, src, &mut page)?;
        write_page(dev, *target, &page)?;
    }
    dev.flush()?;
    let released = render_ctrl_release(&ctrl.entries, ctrl.txid, ctrl.jdata_base);
    let used = (HEADER_LEN + ctrl.entries.len() * 8).div_ceil(SECTOR);
    for s in 0..used {
        let phys = (ctrl.ring_start + s) % JCTRL_SECTORS;
        let o = s * SECTOR;
        dev.write_sector(JCTRL_FIRST_SECTOR + phys as u64, &released[o..o + SECTOR])?;
    }
    dev.flush()
}

/// Decode the newest superblock from a page-0 image (highest generation slot).
fn scan_sb(page: &[u8]) -> Result<V3Superblock> {
    let mut best: Option<V3Superblock> = None;
    for s in 0..SB_SLOTS {
        let off = s * SECTOR;
        let p = &page[off..off + SECTOR];
        if get_u32(p, 0) != SB_MAGIC {
            continue;
        }
        let crc = get_u32(p, 12);
        if crc32(&p[16..56]) != crc {
            continue;
        }
        let sb = V3Superblock {
            version: get_u32(p, 4),
            total_pages: get_u64(p, 16),
            hwm: get_u64(p, 24),
            free_head: get_u64(p, 32),
            catalog_head: get_u64(p, 40),
            generation: get_u64(p, 48),
        };
        if best.map_or(true, |b| sb.generation > b.generation) {
            best = Some(sb);
        }
    }
    best.ok_or(StoreError::Corrupt("not a TablesOS volume"))
}

#[derive(Debug, Clone, Copy)]
pub struct V3Superblock {
    pub version: u32,
    pub total_pages: u64,
    pub hwm: u64,
    pub free_head: u64,
    pub catalog_head: u64,
    pub generation: u64,
}

/// A mounted v0.3.0 volume, opened read-only for migration.
pub struct V3Volume {
    pub sb: V3Superblock,
}

impl V3Volume {
    /// Open an old-format volume: replay any committed journal, then read the
    /// newest superblock. The device must outlive the reads done via
    /// [`V3Volume::read_page`].
    pub fn mount(dev: &mut dyn BlockDevice) -> Result<V3Volume> {
        recover(dev)?;
        let mut p = vec![0u8; PAGE];
        read_page(dev, 0, &mut p)?;
        let sb = scan_sb(&p)?;
        Ok(V3Volume { sb })
    }

    /// Read logical page `lpn` (== physical page in the v0.3.0 format) as a
    /// fresh `PAGE`-sized image.
    pub fn read_page(&self, dev: &mut dyn BlockDevice, lpn: u64) -> Result<Vec<u8>> {
        if lpn >= self.sb.total_pages {
            return Err(StoreError::Corrupt("v3 page out of range"));
        }
        let mut buf = vec![0u8; PAGE];
        read_page(dev, lpn, &mut buf)?;
        Ok(buf)
    }
}
