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
/// The journal control region treated as a ring of this many 512-B sectors
/// (4 pages × 8). Each commit places its control header+entries at ring sector
/// `txid % JCTRL_SECTORS`, wrapping — so the hot header/state sectors rotate
/// across the whole region instead of always landing on sector 0 (flash
/// wear-levelling; see [`commit`] / [`scan_ctrl`]).
const JCTRL_SECTORS: usize = (JCTRL_PAGES * PAGE_SECTORS) as usize; // 32
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
    // One multi-sector request where the driver supports it (USB): fewer
    // round-trips, and avoids the long run of tiny single-sector bulk reads
    // that some xHCI controllers mishandle (returning phantom data).
    dev.read_blocks(page * PAGE_SECTORS, out)
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

/// Render the logical control buffer (header at offset 0, entries packed
/// after it) for one transaction. `jdata_base` records where this txn's page
/// images live in the rotated journal-data region so recovery can find them.
///
/// Header layout (32 B): magic, version, state, crc, txid(8), count(u32),
/// jdata_base(u32). The CRC covers `txid | count | jdata_base | entries` —
/// never `state` — so a commit-point state flip rewrites only the header
/// sector. This is **bit-compatible with v0.2.0** when `jdata_base == 0`:
/// v0.2.0 stored `count` as a u64 at offset 24, whose high half is exactly the
/// `jdata_base = 0` u32 at offset 28, and its CRC input bytes are identical —
/// so a v0.2.0 pending journal still recovers correctly here.
fn render_ctrl(entries: &[u64], txid: u64, state: u32, jdata_base: u64) -> Vec<u8> {
    let mut buf = vec![0u8; JCTRL_PAGES as usize * PAGE];
    // Entries are packed contiguously starting right after the header.
    for (i, e) in entries.iter().enumerate() {
        put_u64(&mut buf, HEADER_LEN + i * 8, *e);
    }
    // CRC over txid | count(u32) | jdata_base(u32) | entries.
    let mut crc_in = Vec::with_capacity(16 + entries.len() * 8);
    crc_in.extend_from_slice(&txid.to_le_bytes());
    crc_in.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    crc_in.extend_from_slice(&(jdata_base as u32).to_le_bytes());
    for e in entries {
        crc_in.extend_from_slice(&e.to_le_bytes());
    }
    let crc = crc32(&crc_in);

    put_u32(&mut buf, 0, MAGIC);
    put_u32(&mut buf, 4, crate::VERSION); // unified product version (crate::VERSION)
    put_u32(&mut buf, 8, state);
    put_u32(&mut buf, 12, crc);
    put_u64(&mut buf, 16, txid);
    put_u32(&mut buf, 24, entries.len() as u32);
    put_u32(&mut buf, 28, jdata_base as u32);
    buf
}

struct Ctrl {
    state: u32,
    txid: u64,
    entries: Vec<u64>,
    /// Where this txn's page images start in the rotated journal-data region.
    jdata_base: u64,
    /// Ring sector at which this control header was found (for the release).
    ring_start: usize,
}

/// Read the whole control region (32 sectors) and return the **newest valid**
/// control header found anywhere in the ring, or `None` if there is none.
///
/// Because each commit places its header at a rotating ring sector, a mount
/// may find several headers (the current one plus older, already-released
/// ones). Every candidate is CRC-validated against its own (wrapping) entry
/// bytes — a stale header whose entry sectors were partly overwritten fails
/// CRC and is skipped — and the one with the highest `txid` wins. The engine
/// only ever has one transaction in flight, so at most one header is
/// `COMMITTED`, and it is always the highest-txid one.
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
        // The 32-byte header never wraps (a sector is 512 B), so read it flat.
        if get_u32(&ring, off) != MAGIC {
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
        // Entries follow the header logically and may wrap around the ring.
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
            continue; // torn / stale header → ignore
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

fn write_ctrl(dev: &mut dyn BlockDevice, buf: &[u8]) -> Result<()> {
    for p in 0..JCTRL_PAGES {
        let o = p as usize * PAGE;
        write_page(dev, 1 + p, &buf[o..o + PAGE])?;
    }
    Ok(())
}

/// Device sector at which the journal control region begins (page 1).
const JCTRL_FIRST_SECTOR: u64 = PAGE_SECTORS; // page 1

/// How many leading sectors of the rendered control buffer actually carry
/// live data for `count` entries: the 32-byte header plus `count` 8-byte
/// entries, rounded up to whole sectors (always ≥ 1, the header sector).
/// Sectors beyond this hold zero-padding or stale entries that `read_ctrl`
/// never looks at (it reads exactly `count` entries and the CRC covers only
/// those), so they don't need rewriting.
fn ctrl_used_sectors(count: usize) -> u64 {
    let bytes = HEADER_LEN + count * 8;
    ((bytes + SECTOR - 1) / SECTOR) as u64
}

/// Write the first `sectors` logical sectors of a rendered control buffer into
/// the control ring, starting at ring sector `ring_start` and wrapping. Only
/// the sectors that change are written (wear reduction): the header+entries for
/// a small txn is one sector, and a commit-point state flip is just the header
/// sector. The `ring_start` rotation (driven by `txid`) spreads those writes
/// across all 32 control sectors instead of hammering sector 0. See [`commit`].
fn write_ctrl_ring(
    dev: &mut dyn BlockDevice,
    buf: &[u8],
    ring_start: usize,
    sectors: u64,
) -> Result<()> {
    for s in 0..sectors as usize {
        let phys = (ring_start + s) % JCTRL_SECTORS;
        let o = s * SECTOR;
        dev.write_sector(JCTRL_FIRST_SECTOR + phys as u64, &buf[o..o + SECTOR])?;
    }
    Ok(())
}

/// Ring sector at which transaction `txid`'s control header is placed.
fn ctrl_ring_start(txid: u64) -> usize {
    (txid % JCTRL_SECTORS as u64) as usize
}

/// Initialise an empty journal (called by `Pager::format`). Zeroes the whole
/// control region (so no stale `MAGIC` lingers from a previous format) and
/// lays down one clean `EMPTY` header at ring sector 0.
pub fn init(dev: &mut dyn BlockDevice) -> Result<()> {
    let zero = vec![0u8; JCTRL_PAGES as usize * PAGE];
    write_ctrl(dev, &zero)?;
    write_ctrl_ring(dev, &render_ctrl(&[], 0, STATE_EMPTY, 0), 0, 1)?;
    dev.flush()
}

/// Replay a committed-but-not-checkpointed transaction. Returns whether a redo
/// actually happened. Idempotent: safe to crash during the redo and re-run.
pub fn recover(dev: &mut dyn BlockDevice) -> Result<bool> {
    let ctrl = match scan_ctrl(dev)? {
        Some(c) => c,
        None => return Ok(false),
    };
    if ctrl.state != STATE_COMMITTED {
        return Ok(false);
    }
    // Images live at the rotated journal-data base recorded in the header.
    let mut page = [0u8; PAGE];
    for (i, target) in ctrl.entries.iter().enumerate() {
        let src = JDATA_START_PAGE + (ctrl.jdata_base + i as u64) % JDATA_PAGES;
        read_page(dev, src, &mut page)?;
        write_page(dev, *target, &page)?;
    }
    dev.flush()?;
    // Release this header in place (its ring sector), flipping it to EMPTY.
    let released = render_ctrl(&ctrl.entries, ctrl.txid, STATE_EMPTY, ctrl.jdata_base);
    write_ctrl_ring(dev, &released, ctrl.ring_start, 1)?;
    dev.flush()?;
    Ok(true)
}

/// Commit `(target_page, image)` pairs atomically. Protocol:
/// 1. images → journal data pages, control written `EMPTY`, **flush**
/// 2. control flipped to `COMMITTED`, **flush**  ← durable commit point
/// 3. images → home pages, **flush**
/// 4. control back to `EMPTY`, **flush**
///
/// For flash wear-levelling, both hot structures rotate by `txid`: the page
/// images go to journal-data pages starting at `txid % JDATA_PAGES` (wrapping),
/// and the control header+entries go to control ring sector
/// `txid % JCTRL_SECTORS` (wrapping). Recovery reads both positions back from
/// the header, so correctness (atomic commit, idempotent redo) is unchanged.
pub fn commit(dev: &mut dyn BlockDevice, txid: u64, dirty: &[(u64, Vec<u8>)]) -> Result<()> {
    if dirty.len() > JCAP {
        return Err(StoreError::OutOfSpace);
    }
    let entries: Vec<u64> = dirty.iter().map(|(p, _)| *p).collect();
    let jdata_base = txid % JDATA_PAGES;
    let ring_start = ctrl_ring_start(txid);
    // Only the control sectors that change are written: the header+entries span
    // `ctrl_used_sectors(count)` sectors for staging, and each state flip is
    // just the header sector. The state byte + CRC live in the header sector,
    // and `scan_ctrl` reads exactly `count` entries (CRC-checked), so stale tail
    // sectors are ignored.
    let used = ctrl_used_sectors(entries.len());

    // 1. Stage images at the rotated journal-data base, then the EMPTY header.
    for (i, (_, img)) in dirty.iter().enumerate() {
        let p = JDATA_START_PAGE + (jdata_base + i as u64) % JDATA_PAGES;
        write_page(dev, p, img)?;
    }
    write_ctrl_ring(dev, &render_ctrl(&entries, txid, STATE_EMPTY, jdata_base), ring_start, used)?;
    dev.flush()?;

    // 2. Commit point — flip the state byte in the header sector only.
    write_ctrl_ring(dev, &render_ctrl(&entries, txid, STATE_COMMITTED, jdata_base), ring_start, 1)?;
    dev.flush()?;

    // 3. Install at home.
    for (target, img) in dirty {
        write_page(dev, *target, img)?;
    }
    dev.flush()?;

    // 4. Release the journal — flip the state byte in the header sector only.
    write_ctrl_ring(dev, &render_ctrl(&entries, txid, STATE_EMPTY, jdata_base), ring_start, 1)?;
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
        // install: stage the image + entries at the rotated positions for this
        // txid, flip state to COMMITTED, stop.
        let txid = 1u64;
        let base = txid % JDATA_PAGES;
        let rs = ctrl_ring_start(txid);
        let mut img = vec![0u8; PAGE];
        img[0..5].copy_from_slice(b"after");
        write_page(&mut dev, JDATA_START_PAGE + base % JDATA_PAGES, &img).unwrap();
        write_ctrl_ring(
            &mut dev,
            &render_ctrl(&[target], txid, STATE_EMPTY, base),
            rs,
            ctrl_used_sectors(1),
        )
        .unwrap();
        dev.flush().unwrap();
        write_ctrl_ring(
            &mut dev,
            &render_ctrl(&[target], txid, STATE_COMMITTED, base),
            rs,
            1,
        )
        .unwrap();
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

    #[test]
    fn small_commit_rewrites_few_control_sectors() {
        // Wear-reduction regression: a one-page commit must touch only a
        // handful of control sectors, not all 4 control pages (32 sectors) ×3.
        let mut dev = MemBlockDevice::new(16 * 1024 * 1024 / crate::block::SECTOR as u64);
        init(&mut dev).unwrap();
        let target = FIRST_DATA_PAGE + 1;
        let before = dev.writes;
        commit(&mut dev, 1, &[(target, vec![7u8; PAGE])]).unwrap();
        let written = dev.writes - before;
        // 1 journal-data page (8) + 1 home page (8) = 16 data sectors; control:
        // staging(1) + commit(1) + release(1) = 3. The old all-pages path wrote
        // 3×32 = 96 control sectors alone.
        assert_eq!(written, 16 + 3, "unexpected sector write count: {written}");

        // And the data actually landed (correctness preserved).
        let mut home = vec![0u8; PAGE];
        read_page(&mut dev, target, &mut home).unwrap();
        assert_eq!(home[0], 7);
        assert!(!recover(&mut dev).unwrap()); // cleanly released
    }

    #[test]
    fn shrinking_commit_ignores_stale_tail_entries() {
        // A big commit whose entries spill past control sector 0, followed by a
        // tiny one that only rewrites sector 0, must leave a consistent control
        // region — the stale tail entries from the big commit are never read.
        let mut dev = MemBlockDevice::new(32 * 1024 * 1024 / crate::block::SECTOR as u64);
        init(&mut dev).unwrap();
        let big: alloc::vec::Vec<(u64, Vec<u8>)> = (0..100)
            .map(|i| (FIRST_DATA_PAGE + i, vec![1u8; PAGE]))
            .collect();
        assert!(ctrl_used_sectors(big.len()) > 1, "test needs multi-sector entries");
        commit(&mut dev, 1, &big).unwrap();

        commit(&mut dev, 2, &[(FIRST_DATA_PAGE, vec![2u8; PAGE])]).unwrap();
        // Clean: nothing to redo, and the small commit's home write landed.
        assert!(!recover(&mut dev).unwrap());
        let mut p = vec![0u8; PAGE];
        read_page(&mut dev, FIRST_DATA_PAGE, &mut p).unwrap();
        assert_eq!(p[0], 2);
    }

    #[test]
    fn control_writes_rotate_across_the_ring() {
        // Wear-levelling: successive commits must place their control headers on
        // different ring sectors, not all on sector 0.
        let mut dev = MemBlockDevice::new(16 * 1024 * 1024 / crate::block::SECTOR as u64);
        init(&mut dev).unwrap();
        for txid in 1..=6u64 {
            commit(&mut dev, txid, &[(FIRST_DATA_PAGE, vec![txid as u8; PAGE])]).unwrap();
        }
        let mut ring = vec![0u8; JCTRL_SECTORS * SECTOR];
        for p in 0..JCTRL_PAGES {
            let mut pg = [0u8; PAGE];
            read_page(&mut dev, 1 + p, &mut pg).unwrap();
            let o = p as usize * PAGE;
            ring[o..o + PAGE].copy_from_slice(&pg);
        }
        let magic_sectors = (0..JCTRL_SECTORS)
            .filter(|h| get_u32(&ring, h * SECTOR) == MAGIC)
            .count();
        assert!(magic_sectors >= 3, "control headers did not rotate: {magic_sectors}");
        assert!(!recover(&mut dev).unwrap()); // still clean
    }

    #[test]
    fn recovers_a_v0_2_0_style_committed_journal() {
        // Bit-compatibility: a journal written by v0.2.0 (control header at ring
        // sector 0, `count` as a u64 at offset 24 → implicit jdata_base 0, images
        // at page 5+i) must still recover under the rotating v0.3.0 reader.
        let mut dev = MemBlockDevice::new(16 * 1024 * 1024 / crate::block::SECTOR as u64);
        let target = FIRST_DATA_PAGE + 3;
        let txid = 7u64;
        let mut img = vec![0u8; PAGE];
        img[0..3].copy_from_slice(b"old");
        write_page(&mut dev, JDATA_START_PAGE, &img).unwrap(); // base 0 → page 5

        let mut ctrl = vec![0u8; JCTRL_PAGES as usize * PAGE];
        put_u64(&mut ctrl, HEADER_LEN, target); // one entry
        // CRC exactly as v0.2.0 computed it: txid | count(u64) | entries.
        let mut crc_in = Vec::new();
        crc_in.extend_from_slice(&txid.to_le_bytes());
        crc_in.extend_from_slice(&1u64.to_le_bytes());
        crc_in.extend_from_slice(&target.to_le_bytes());
        let crc = crc32(&crc_in);
        put_u32(&mut ctrl, 0, MAGIC);
        put_u32(&mut ctrl, 4, 0x0002_00); // a v0.2.0-shaped version stamp
        put_u32(&mut ctrl, 8, STATE_COMMITTED);
        put_u32(&mut ctrl, 12, crc);
        put_u64(&mut ctrl, 16, txid);
        put_u64(&mut ctrl, 24, 1); // count as u64 (offset 28 stays 0 = base 0)
        write_ctrl(&mut dev, &ctrl).unwrap();
        dev.flush().unwrap();

        assert!(recover(&mut dev).unwrap());
        let mut home = vec![0u8; PAGE];
        read_page(&mut dev, target, &mut home).unwrap();
        assert_eq!(&home[0..3], b"old");
    }

    #[test]
    fn commit_is_atomic_under_power_cut_at_every_point() {
        // Sweep the power-cut point across the whole commit and assert the home
        // page is always either fully old or fully new after recovery — never a
        // torn mix — for the rotating layout.
        let target = FIRST_DATA_PAGE + 2;
        let mut base_dev = MemBlockDevice::new(16 * 1024 * 1024 / crate::block::SECTOR as u64);
        init(&mut base_dev).unwrap();
        commit(&mut base_dev, 1, &[(target, vec![0xAA; PAGE])]).unwrap();
        let base = base_dev.snapshot();

        for cut in 1..40u64 {
            let mut dev = MemBlockDevice::from_snapshot(base.clone());
            dev.cut_after = Some(cut);
            let _ = commit(&mut dev, 2, &[(target, vec![0xBB; PAGE])]);
            let snap = dev.snapshot();

            let mut dev2 = MemBlockDevice::from_snapshot(snap);
            recover(&mut dev2).unwrap();
            let mut home = vec![0u8; PAGE];
            read_page(&mut dev2, target, &mut home).unwrap();
            assert!(
                home.iter().all(|&b| b == 0xAA) || home.iter().all(|&b| b == 0xBB),
                "torn page at cut {cut}: first byte {}",
                home[0]
            );
            assert!(!recover(&mut dev2).unwrap()); // recovery is idempotent
        }
    }
}
