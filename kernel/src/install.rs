//! Install a fresh TablesOS image onto another drive.
//!
//! Reads the booted disk's `[custom MBR | stage2 | kernel]` prefix
//! verbatim via direct absolute-LBA ATA reads, stamps a fresh random
//! 16-byte system GUID into the MBR header (offset 0x1AC), writes
//! everything to the chosen target, then formats an empty TablesOS
//! volume at the target's data-location LBA. Verifies by re-reading
//! sector 0 (MBR magic + new GUID) and re-mounting the volume.
//!
//! The destination's identity is *not* what's in the booted MBR — by
//! design: the kernel's runtime identity gate refuses to write any disk
//! whose GUID matches the booted one, so the new image's GUID must
//! differ. The result is bootable on a machine whose BIOS will load
//! from the target drive (in QEMU: pass `-boot` order or `-drive`
//! attachment such that the new image is first).

use alloc::format;
use alloc::string::String;
use tablestore::block::SECTOR;
use tablestore::journal::{FIRST_DATA_PAGE, PAGE_SECTORS};
use tablestore::{BlockDevice, Result as TsResult, Store, StoreError};

use crate::ata;
use crate::usb::xhci::{self, UsbMscDevice};

/// Outcome of a single install attempt. Always returned (success or
/// failure) so the UI can surface per-step status.
#[derive(Clone)]
pub struct InstallReport {
    pub target_slot: u8,
    /// Sectors of boot prefix successfully copied (0 if we failed before
    /// the copy started; up to `data_lba` on success).
    pub sectors_copied: u64,
    /// Volume size, in sectors, that we asked `Store::format` to create.
    pub volume_sectors: u64,
    /// GUID we stamped into the new MBR.
    pub new_sys_guid: [u8; 16],
    /// MBR magic + GUID match after writing.
    pub verify_mbr_ok: bool,
    /// `Store::open` on the freshly formatted volume succeeded.
    pub verify_mount_ok: bool,
    pub message: String,
}

/// Adapt a `BlockDevice` so reads/writes at logical sector N go to
/// physical sector `base + N` on the underlying device. Used to point
/// `Store::format` at the volume region of a freshly written USB stick
/// while keeping the boot prefix in front of it untouched.
pub struct BaseOffsetDevice<D: BlockDevice> {
    inner: D,
    base: u64,
    sectors: u64,
}

impl<D: BlockDevice> BaseOffsetDevice<D> {
    pub fn new(inner: D, base: u64, sectors: u64) -> Self {
        Self { inner, base, sectors }
    }
}

impl<D: BlockDevice> BlockDevice for BaseOffsetDevice<D> {
    fn sector_count(&self) -> u64 {
        self.sectors
    }
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> TsResult<()> {
        if lba >= self.sectors {
            return Err(StoreError::Io);
        }
        self.inner.read_sector(self.base + lba, buf)
    }
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> TsResult<()> {
        if lba >= self.sectors {
            return Err(StoreError::Io);
        }
        self.inner.write_sector(self.base + lba, buf)
    }
    fn flush(&mut self) -> TsResult<()> {
        self.inner.flush()
    }
}

/// 16-byte system GUID mixed from the TSC via xorshift64. Not
/// cryptographic — just unique per call.
fn make_random_sys_guid() -> [u8; 16] {
    let mut x = unsafe { core::arch::x86_64::_rdtsc() };
    // Mix with a salt so the first call after boot doesn't get a
    // predictable bias from a near-zero counter.
    x ^= 0xCAFE_BABE_DEAD_BEEFu64;
    let mut g = [0u8; 16];
    for chunk in g.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    g
}

/// Refuses to install onto a target whose existing MBR signals it's the
/// booted disk, even though our identity is supposed to differ. Extra
/// belt-and-suspenders.
fn refuse_if_same_disk(usb: &mut UsbMscDevice, booted: &[u8; 16]) -> Option<String> {
    let mut buf = [0u8; SECTOR];
    if usb.read_sector(0, &mut buf).is_err() {
        return None; // Can't read; trust the caller's filtering.
    }
    if &buf[0x180..0x188] == b"TBLSBOOT" && &buf[0x1AC..0x1AC + 16] == &booted[..] {
        return Some(
            "target's existing MBR system GUID matches the booted disk — refusing".into(),
        );
    }
    None
}

/// Run the install. Always returns an `InstallReport`; partial progress
/// is captured even on failure (no exceptions).
pub fn install_to_usb(
    target_slot_id: u8,
    data_lba: u64,
    booted_sys_guid: &[u8; 16],
) -> InstallReport {
    let new_sys_guid = make_random_sys_guid();
    let mut report = InstallReport {
        target_slot: target_slot_id,
        sectors_copied: 0,
        volume_sectors: 0,
        new_sys_guid,
        verify_mbr_ok: false,
        verify_mount_ok: false,
        message: String::new(),
    };

    let mut usb = match UsbMscDevice::open(target_slot_id) {
        Ok(d) => d,
        Err(e) => {
            report.message = format!("open USB target: {:?}", e);
            return report;
        }
    };
    let usb_total = usb.sector_count();
    // The volume region (everything after the boot prefix) must hold at
    // least the superblock + journal region + one data page, i.e.
    // `FIRST_DATA_PAGE + 1` pages. Check up front so we fail fast with a
    // clear message instead of after copying the whole prefix.
    let min_volume_sectors = (FIRST_DATA_PAGE + 1) * PAGE_SECTORS;
    let need = data_lba + min_volume_sectors;
    if usb_total < need {
        report.message = format!(
            "USB target too small: {} sectors ({} MiB); need >= {} ({} MiB) — {} for boot prefix + {} for the volume's journal region",
            usb_total,
            usb_total / 2048,
            need,
            need / 2048,
            data_lba,
            min_volume_sectors,
        );
        return report;
    }
    if let Some(msg) = refuse_if_same_disk(&mut usb, booted_sys_guid) {
        report.message = msg;
        return report;
    }

    // Copy boot prefix (LBAs 0 .. data_lba) from booted disk (primary
    // IDE master) to target, stamping the new system GUID into LBA 0.
    // Batch into 64-sector (32 KiB) chunks so the whole prefix is a
    // few dozen USB transfers rather than several thousand single-sector
    // ones — much faster, and the SCSI write path frees its DMA buffer
    // after each chunk.
    // The prefix is written via the batched `xhci::msc_write_blocks`
    // rather than the per-sector `usb.write_sector`; `usb` is still used
    // afterwards for `flush` and as the `Store::format` backing device.
    const CHUNK_SECTORS: u64 = 64;
    let mut chunk = alloc::vec![0u8; CHUNK_SECTORS as usize * SECTOR];
    let mut sector = [0u8; SECTOR];
    let mut lba = 0u64;
    while lba < data_lba {
        let n = CHUNK_SECTORS.min(data_lba - lba);
        for i in 0..n {
            if !ata::read_sector_at(0x1F0, false, lba + i, &mut sector) {
                report.message = format!("read booted disk sector {} failed", lba + i);
                return report;
            }
            if lba + i == 0 {
                sector[0x1AC..0x1AC + 16].copy_from_slice(&new_sys_guid);
            }
            let off = i as usize * SECTOR;
            chunk[off..off + SECTOR].copy_from_slice(&sector);
        }
        if let Err(e) = xhci::msc_write_blocks(
            target_slot_id,
            lba as u32,
            n as u16,
            &chunk[..n as usize * SECTOR],
        ) {
            report.message = format!("write USB sectors at {} failed: {}", lba, e);
            return report;
        }
        lba += n;
        report.sectors_copied = lba;
    }
    let _ = usb.flush();

    // Format an empty TablesOS volume at the target's data-location.
    let volume_sectors = usb_total - data_lba;
    report.volume_sectors = volume_sectors;
    let base_dev = BaseOffsetDevice::new(usb, data_lba, volume_sectors);
    let store = match Store::format(base_dev) {
        Ok(s) => s,
        Err(e) => {
            report.message = format!("Store::format: {:?}", e);
            return report;
        }
    };
    drop(store); // Releases the underlying USB handle.

    // ---- Verify ----------------------------------------------------
    let mut vusb = match UsbMscDevice::open(target_slot_id) {
        Ok(d) => d,
        Err(e) => {
            report.message =
                format!("verify open USB: {:?}; copy + format did succeed", e);
            return report;
        }
    };
    let mut mbr_check = [0u8; SECTOR];
    if vusb.read_sector(0, &mut mbr_check).is_err() {
        report.message = "verify read MBR failed".into();
        return report;
    }
    report.verify_mbr_ok = &mbr_check[0x180..0x188] == b"TBLSBOOT"
        && &mbr_check[0x1AC..0x1AC + 16] == &new_sys_guid[..];
    if !report.verify_mbr_ok {
        report.message = "MBR re-read disagrees with what we wrote".into();
        return report;
    }
    let vbase = BaseOffsetDevice::new(vusb, data_lba, volume_sectors);
    match Store::open(vbase) {
        Ok(_s) => {
            report.verify_mount_ok = true;
            report.message = format!(
                "OK — wrote {} sectors of boot prefix + formatted {}-sector volume, verified",
                data_lba, volume_sectors
            );
        }
        Err(e) => {
            report.message = format!("verify Store::open: {:?}", e);
        }
    }
    report
}
