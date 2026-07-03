//! Page I/O primitives shared by the storage layer.
//!
//! Historically this module was a physical (page-image) write-ahead log. As of
//! v0.4.0 durability is provided by the **copy-on-write** pager
//! ([`crate::pager`]) — every commit writes changed pages to fresh physical
//! pages and flips a single atomic anchor record, so there is no journal region
//! and no in-place home-page rewrites (which removes the last flash hot spots).
//!
//! What remains here are the small, format-agnostic helpers the pager (and the
//! kernel) still build on: the 4 KiB page geometry, the per-transaction page
//! bound, a table-free CRC-32, and whole-page read/write.

use crate::block::{BlockDevice, SECTOR};
use crate::Result;

/// Page size in bytes (8 × 512-B sectors).
pub const PAGE: usize = 4096;
/// Sectors per page.
pub const PAGE_SECTORS: u64 = (PAGE / SECTOR) as u64;

/// Maximum number of pages a single transaction may dirty. This bounds the
/// engine's "unlimited" values (a giant row + its overflow chain) to a
/// build-time constant, and — under copy-on-write — bounds how many fresh
/// physical pages one commit must allocate before the old ones are released
/// (see [`crate::pager`] capacity reasoning). The value is the historical
/// journal capacity, kept stable so the bound is unchanged.
pub const JCAP: usize = ((PAGE - 32) / 8) + 3 * (PAGE / 8); // 2044

/// First allocatable **logical** page number. Logical page 0 is reserved as the
/// engine's null pointer; logical pages `1..FIRST_DATA_PAGE` are reserved so
/// that a v0.3.0 volume (whose data pages started here) migrates 1:1 by logical
/// page number into the copy-on-write layout. Under CoW these numbers are purely
/// logical handles — they no longer correspond to fixed physical locations.
pub const FIRST_DATA_PAGE: u64 = 2049;

/// Read one 4 KiB page (8 sectors) from the device.
pub fn read_page(dev: &mut dyn BlockDevice, page: u64, out: &mut [u8]) -> Result<()> {
    debug_assert!(out.len() == PAGE);
    // One multi-sector request where the driver supports it (USB): fewer
    // round-trips, and avoids the long run of tiny single-sector bulk reads
    // that some xHCI controllers mishandle (returning phantom data).
    dev.read_blocks(page * PAGE_SECTORS, out)
}

/// Write one 4 KiB page (8 sectors). Sectors go out in order.
pub fn write_page(dev: &mut dyn BlockDevice, page: u64, data: &[u8]) -> Result<()> {
    debug_assert!(data.len() == PAGE);
    for i in 0..PAGE_SECTORS {
        let o = i as usize * SECTOR;
        dev.write_sector(page * PAGE_SECTORS + i, &data[o..o + SECTOR])?;
    }
    Ok(())
}

/// Bitwise CRC-32 (IEEE). No lookup table → no `no_std` static needed; inputs
/// here are only small control/superblock records, so speed is irrelevant.
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
