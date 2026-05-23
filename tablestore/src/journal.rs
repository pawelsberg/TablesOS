//! Physical (page-image) write-ahead log.
//!
//! A transaction's complete set of new page images is written into a fixed
//! journal region, then a single durable flip of the control header's `state`
//! to `COMMITTED` is the atomic commit point. Recovery re-copies the journaled
//! images to their home pages — an idempotent redo. Consequently a power loss
//! costs *at most the last uncommitted transaction* (the spec's guarantee).
//!
//! Layout (4 KiB pages, 512 B sectors):
//!
//! ```text
//! page 0            superblock
//! pages 1..=4       journal control (32 B header in page 1 + entry array)
//! pages 5..2048     journal data (one full page image per entry)
//! pages 2049..      catalog / schema / heap / free list
//! ```
//!
//! The journal bounds a single transaction (hence one row + its overflow
//! chain) to [`JCAP`] pages. See IMPLEMENTATION.md for why this is the one
//! place "unlimited" becomes "bounded by a build-time constant".

use crate::block::{BlockDevice, SECTOR};
use crate::{Result, StoreError};
use alloc::vec;
use alloc::vec::Vec;

pub const PAGE: usize = 4096;
pub const PAGE_SECTORS: u64 = (PAGE / SECTOR) as u64;

const JCTRL_PAGES: u64 = 4;
const HEADER_LEN: usize = 32;
/// Entries in control page 1 (after the header) + the three following pages.
pub const JCAP: usize = ((PAGE - HEADER_LEN) / 8) + 3 * (PAGE / 8); // 508 + 1536
const JDATA_START_PAGE: u64 = 1 + JCTRL_PAGES; // 5
pub const JDATA_PAGES: u64 = JCAP as u64; // 2044
/// First page available for catalog / schema / heap / free list.
pub const FIRST_DATA_PAGE: u64 = JDATA_START_PAGE + JDATA_PAGES; // 2049

const MAGIC: u32 = 0x4a_4c_42_54; // "TBLJ" LE
const STATE_EMPTY: u32 = 0;
const STATE_COMMITTED: u32 = 1;

/// Read one 4 KiB page (8 sectors) from the device.
pub fn read_page(dev: &mut dyn BlockDevice, page: u64, out: &mut [u8]) -> Result<()> {
    debug_assert!(out.len() == PAGE);
    let mut sec = [0u8; SECTOR];
    for i in 0..PAGE_SECTORS {
        dev.read_sector(page * PAGE_SECTORS + i, &mut sec)?;
        let o = i as usize * SECTOR;
        out[o..o + SECTOR].copy_from_slice(&sec);
    }
    Ok(())
}

/// Write one 4 KiB page (8 sectors). Sectors go out in order so a torn write
/// damages the tail, not the control header in sector 0.
pub fn write_page(dev: &mut dyn BlockDevice, page: u64, data: &[u8]) -> Result<()> {
    debug_assert!(data.len() == PAGE);
    for i in 0..PAGE_SECTORS {
        let o = i as usize * SECTOR;
        dev.write_sector(page * PAGE_SECTORS + i, &data[o..o + SECTOR])?;
    }
    Ok(())
}

/// Bitwise CRC-32 (IEEE). No lookup table → no `no_std` static needed; inputs
/// here are only the small control region, so speed is irrelevant.
pub fn crc32(bytes: &[u8]) -> u32 {
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

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

/// Render the four control pages for a given entry list and state. The CRC
/// covers `txid`, `count` and the entries — never `state` — so the commit
/// flip rewrites only one well-defined sector.
fn render_ctrl(entries: &[u64], txid: u64, state: u32) -> Vec<u8> {
    let mut buf = vec![0u8; JCTRL_PAGES as usize * PAGE];
    // Entries are packed contiguously starting right after the header.
    for (i, e) in entries.iter().enumerate() {
        put_u64(&mut buf, HEADER_LEN + i * 8, *e);
    }
    // CRC over txid|count|entries.
    let mut crc_in = Vec::with_capacity(16 + entries.len() * 8);
    crc_in.extend_from_slice(&txid.to_le_bytes());
    crc_in.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for e in entries {
        crc_in.extend_from_slice(&e.to_le_bytes());
    }
    let crc = crc32(&crc_in);

    put_u32(&mut buf, 0, MAGIC);
    put_u32(&mut buf, 4, 1); // version
    put_u32(&mut buf, 8, state);
    put_u32(&mut buf, 12, crc);
    put_u64(&mut buf, 16, txid);
    put_u64(&mut buf, 24, entries.len() as u64);
    buf
}

struct Ctrl {
    state: u32,
    count: usize,
    entries: Vec<u64>,
}

fn read_ctrl(dev: &mut dyn BlockDevice) -> Result<Option<Ctrl>> {
    let mut buf = vec![0u8; JCTRL_PAGES as usize * PAGE];
    for p in 0..JCTRL_PAGES {
        let mut pg = [0u8; PAGE];
        read_page(dev, 1 + p, &mut pg)?;
        let o = p as usize * PAGE;
        buf[o..o + PAGE].copy_from_slice(&pg);
    }
    if get_u32(&buf, 0) != MAGIC {
        return Ok(None); // never initialised
    }
    let state = get_u32(&buf, 8);
    let stored_crc = get_u32(&buf, 12);
    let txid = get_u64(&buf, 16);
    let count = get_u64(&buf, 24) as usize;
    if count > JCAP {
        return Ok(None);
    }
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        entries.push(get_u64(&buf, HEADER_LEN + i * 8));
    }
    let mut crc_in = Vec::with_capacity(16 + count * 8);
    crc_in.extend_from_slice(&txid.to_le_bytes());
    crc_in.extend_from_slice(&(count as u64).to_le_bytes());
    for e in &entries {
        crc_in.extend_from_slice(&e.to_le_bytes());
    }
    if crc32(&crc_in) != stored_crc {
        return Ok(None); // torn body write → treat as no committed txn
    }
    Ok(Some(Ctrl {
        state,
        count,
        entries,
    }))
}

fn write_ctrl(dev: &mut dyn BlockDevice, buf: &[u8]) -> Result<()> {
    for p in 0..JCTRL_PAGES {
        let o = p as usize * PAGE;
        write_page(dev, 1 + p, &buf[o..o + PAGE])?;
    }
    Ok(())
}

/// Initialise an empty journal (called by `Pager::format`).
pub fn init(dev: &mut dyn BlockDevice) -> Result<()> {
    let buf = render_ctrl(&[], 0, STATE_EMPTY);
    write_ctrl(dev, &buf)?;
    dev.flush()
}

/// Replay a committed-but-not-checkpointed transaction. Returns whether a redo
/// actually happened. Idempotent: safe to crash during the redo and re-run.
pub fn recover(dev: &mut dyn BlockDevice) -> Result<bool> {
    let ctrl = match read_ctrl(dev)? {
        Some(c) => c,
        None => return Ok(false),
    };
    if ctrl.state != STATE_COMMITTED {
        return Ok(false);
    }
    let mut page = [0u8; PAGE];
    for (i, target) in ctrl.entries.iter().enumerate() {
        read_page(dev, JDATA_START_PAGE + i as u64, &mut page)?;
        write_page(dev, *target, &page)?;
    }
    dev.flush()?;
    let empty = render_ctrl(&ctrl.entries, 0, STATE_EMPTY);
    write_ctrl(dev, &empty)?;
    dev.flush()?;
    let _ = ctrl.count; // (kept for readability of the structure)
    Ok(true)
}

/// Commit `(target_page, image)` pairs atomically. Protocol:
/// 1. images → journal data pages, control written `EMPTY`, **flush**
/// 2. control flipped to `COMMITTED`, **flush**  ← durable commit point
/// 3. images → home pages, **flush**
/// 4. control back to `EMPTY`, **flush**
pub fn commit(dev: &mut dyn BlockDevice, txid: u64, dirty: &[(u64, Vec<u8>)]) -> Result<()> {
    if dirty.len() > JCAP {
        return Err(StoreError::OutOfSpace);
    }
    let entries: Vec<u64> = dirty.iter().map(|(p, _)| *p).collect();

    // 1. Stage images, then the EMPTY control body.
    for (i, (_, img)) in dirty.iter().enumerate() {
        write_page(dev, JDATA_START_PAGE + i as u64, img)?;
    }
    write_ctrl(dev, &render_ctrl(&entries, txid, STATE_EMPTY))?;
    dev.flush()?;

    // 2. Commit point.
    write_ctrl(dev, &render_ctrl(&entries, txid, STATE_COMMITTED))?;
    dev.flush()?;

    // 3. Install at home.
    for (target, img) in dirty {
        write_page(dev, *target, img)?;
    }
    dev.flush()?;

    // 4. Release the journal.
    write_ctrl(dev, &render_ctrl(&entries, txid, STATE_EMPTY))?;
    dev.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemBlockDevice;

    #[test]
    fn recover_replays_committed_then_is_idempotent() {
        let mut dev = MemBlockDevice::new(16 * 1024 * 1024 / crate::block::SECTOR as u64);
        init(&mut dev).unwrap();
        let target = FIRST_DATA_PAGE + 7;

        // Simulate a crash *after* the commit point but *before* the home
        // install: stage the image + entries, flip state to COMMITTED, stop.
        let mut img = vec![0u8; PAGE];
        img[0..5].copy_from_slice(b"after");
        write_page(&mut dev, JDATA_START_PAGE, &img).unwrap();
        write_ctrl(&mut dev, &render_ctrl(&[target], 1, STATE_EMPTY)).unwrap();
        dev.flush().unwrap();
        write_ctrl(&mut dev, &render_ctrl(&[target], 1, STATE_COMMITTED)).unwrap();
        dev.flush().unwrap();

        // Home page still holds the *old* (zero) content at this instant.
        let mut home = vec![0u8; PAGE];
        read_page(&mut dev, target, &mut home).unwrap();
        assert_ne!(&home[0..5], b"after");

        // Recovery must redo the write, then a second recovery is a no-op.
        assert!(recover(&mut dev).unwrap());
        read_page(&mut dev, target, &mut home).unwrap();
        assert_eq!(&home[0..5], b"after");
        assert!(!recover(&mut dev).unwrap());
        read_page(&mut dev, target, &mut home).unwrap();
        assert_eq!(&home[0..5], b"after");
    }
}
