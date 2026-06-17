//! The only contract between the engine and the hardware.
//!
//! The kernel implements [`BlockDevice`] with an ATA PIO driver; host tests
//! use [`MemBlockDevice`], which can also simulate a power cut.

use crate::{Result, StoreError};
use alloc::vec;
use alloc::vec::Vec;

/// Logical sector size. ATA and the QEMU disk use 512-byte sectors.
pub const SECTOR: usize = 512;

pub trait BlockDevice {
    /// Number of addressable sectors.
    fn sector_count(&self) -> u64;

    /// Read one sector into `buf` (`buf.len() == SECTOR`).
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<()>;

    /// Read `buf.len() / SECTOR` consecutive sectors starting at `lba`, in as
    /// few device transactions as possible. The default loops [`read_sector`];
    /// drivers that can move many sectors per request (USB mass storage) should
    /// override it with a single multi-block transfer. Besides being far fewer
    /// round-trips, a single transfer is a workaround for controllers that
    /// mishandle a long run of tiny single-sector bulk reads. `buf.len()` must
    /// be a whole multiple of `SECTOR`.
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        debug_assert!(buf.len() % SECTOR == 0);
        for (i, chunk) in buf.chunks_mut(SECTOR).enumerate() {
            self.read_sector(lba + i as u64, chunk)?;
        }
        Ok(())
    }

    /// Write one sector from `buf` (`buf.len() == SECTOR`).
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<()>;

    /// Block until every prior write is durable on the medium. This is the
    /// barrier the journal relies on; an SSD-safe implementation also lets the
    /// device coalesce, see IMPLEMENTATION.md.
    fn flush(&mut self) -> Result<()>;
}

/// RAM-backed device for host tests. `power_cut` makes every subsequent write
/// silently vanish, modelling the pendrive being yanked.
pub struct MemBlockDevice {
    data: Vec<u8>,
    sectors: u64,
    powered: bool,
    /// Counts successful sector writes; tests use it to cut power deterministically.
    pub writes: u64,
    /// If set, the Nth write (1-based) and everything after it is dropped.
    pub cut_after: Option<u64>,
}

impl MemBlockDevice {
    pub fn new(sectors: u64) -> Self {
        MemBlockDevice {
            data: vec![0u8; sectors as usize * SECTOR],
            sectors,
            powered: true,
            writes: 0,
            cut_after: None,
        }
    }

    /// Take a byte-exact snapshot of the medium (what survives a power cut).
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.clone()
    }

    /// Re-attach a previously snapshotted medium, power restored.
    pub fn from_snapshot(data: Vec<u8>) -> Self {
        let sectors = (data.len() / SECTOR) as u64;
        MemBlockDevice {
            data,
            sectors,
            powered: true,
            writes: 0,
            cut_after: None,
        }
    }
}

impl BlockDevice for MemBlockDevice {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        if buf.len() != SECTOR || lba >= self.sectors {
            return Err(StoreError::Io);
        }
        let off = lba as usize * SECTOR;
        buf.copy_from_slice(&self.data[off..off + SECTOR]);
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<()> {
        if buf.len() != SECTOR || lba >= self.sectors {
            return Err(StoreError::Io);
        }
        if !self.powered {
            return Ok(()); // Power gone: silently lost, just like real life.
        }
        let off = lba as usize * SECTOR;
        self.data[off..off + SECTOR].copy_from_slice(buf);
        self.writes += 1;
        if Some(self.writes) == self.cut_after {
            self.powered = false;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.powered {
            Ok(())
        } else {
            Err(StoreError::Io)
        }
    }
}
