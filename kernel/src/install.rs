//! Install a fresh TablesOS image onto another drive.
//!
//! Reads the booted disk's `[custom MBR | stage2 | kernel]` prefix
//! verbatim via [`BootedReader`] (the actual boot device — IDE, xHCI or
//! EHCI USB — not just the IDE master), stamps a fresh random
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
use crate::usb::ehci;
use crate::usb::xhci::{self, UsbMscDevice};

/// A read handle to the disk the machine **booted from**, used to copy the
/// `[MBR | stage2 | kernel]` boot prefix onto an install/upgrade target.
///
/// The boot prefix lives on whichever device the kernel started from — IDE
/// (QEMU), an xHCI USB pendrive, or an EHCI USB pendrive (old machines). The
/// original code read it from the primary IDE master unconditionally, which is
/// correct only when booted from IDE; on a machine booted from USB that read
/// fails and install/upgrade aborts with "read booted disk sector … failed".
/// [`open`](BootedReader::open) resolves the actual boot device by matching the
/// booted system GUID, so the prefix copy works regardless of how we booted.
pub enum BootedReader {
    /// Legacy ATA PIO, primary IDE master (QEMU's boot disk).
    Ata,
    Usb(UsbMscDevice),
    Ehci(ehci::EhciMscDevice),
}

impl BootedReader {
    /// Open a reader for the booted disk. Prefers the USB/EHCI drive whose MBR
    /// system GUID matches `sys_guid` (a definitive identity match), and falls
    /// back to the IDE master — the QEMU boot path, where the boot disk is not a
    /// USB device. Returns `None` if no booted disk can be reached.
    pub fn open(sys_guid: &[u8; 16]) -> Option<BootedReader> {
        // xHCI: the enumerated drive flagged `booted` is the one we started from.
        for (mmio, slot, di) in xhci::usb_drives_with_slots(sys_guid) {
            if di.booted {
                if let Ok(dev) = UsbMscDevice::open_with_identity_gate(mmio, slot, *sys_guid) {
                    return Some(BootedReader::Usb(dev));
                }
            }
        }
        // EHCI (pre-xHCI machines): same identity gate inside find_boot_drive.
        if let Some((dev, _total)) = ehci::find_boot_drive(sys_guid) {
            return Some(BootedReader::Ehci(dev));
        }
        // IDE primary master: present in QEMU, absent on a USB-booting laptop.
        let mut probe = [0u8; SECTOR];
        if ata::read_sector_at(0x1F0, false, 0, &mut probe) {
            return Some(BootedReader::Ata);
        }
        None
    }

    /// Read `count` sectors starting at absolute `lba` into `buf` (must be
    /// `count * 512` bytes). Returns false on any I/O failure.
    pub fn read(&mut self, lba: u64, count: u64, buf: &mut [u8]) -> bool {
        let bytes = count as usize * SECTOR;
        if buf.len() < bytes {
            return false;
        }
        match self {
            BootedReader::Ata => {
                let mut s = [0u8; SECTOR];
                for i in 0..count {
                    if !ata::read_sector_at(0x1F0, false, lba + i, &mut s) {
                        return false;
                    }
                    let off = i as usize * SECTOR;
                    buf[off..off + SECTOR].copy_from_slice(&s);
                }
                true
            }
            BootedReader::Usb(dev) => dev.read_blocks(lba, &mut buf[..bytes]).is_ok(),
            BootedReader::Ehci(dev) => {
                let mut s = [0u8; SECTOR];
                for i in 0..count {
                    if dev.read_sector(lba + i, &mut s).is_err() {
                        return false;
                    }
                    let off = i as usize * SECTOR;
                    buf[off..off + SECTOR].copy_from_slice(&s);
                }
                true
            }
        }
    }
}

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
    // The volume region (everything after the boot prefix) must hold at least
    // the reserved low logical pages + one data page, i.e. `FIRST_DATA_PAGE + 1`
    // pages (the copy-on-write pager's `format` guard). Check up front so we
    // fail fast with a clear message instead of after copying the whole prefix.
    let min_volume_sectors = (FIRST_DATA_PAGE + 1) * PAGE_SECTORS;
    let need = data_lba + min_volume_sectors;
    if usb_total < need {
        report.message = format!(
            "USB target too small: {} sectors ({} MiB); need >= {} ({} MiB) — {} for boot prefix + {} for the volume's minimum reserved pages",
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

    // Copy boot prefix (LBAs 0 .. data_lba) from the disk we booted from to the
    // target, stamping the new system GUID into LBA 0. Batch into 64-sector
    // (32 KiB) chunks so the whole prefix is a few dozen USB transfers rather
    // than several thousand single-sector ones — much faster, and the SCSI write
    // path frees its DMA buffer after each chunk.
    // The prefix is written via the batched `xhci::msc_write_blocks`
    // rather than the per-sector `usb.write_sector`; `usb` is still used
    // afterwards for `flush` and as the `Store::format` backing device.
    let mut booted = match BootedReader::open(booted_sys_guid) {
        Some(b) => b,
        None => {
            report.message = "could not reach the booted disk to copy the boot prefix".into();
            return report;
        }
    };
    const CHUNK_SECTORS: u64 = 64;
    let mut chunk = alloc::vec![0u8; CHUNK_SECTORS as usize * SECTOR];
    let mut lba = 0u64;
    while lba < data_lba {
        let n = CHUNK_SECTORS.min(data_lba - lba);
        if !booted.read(lba, n, &mut chunk[..n as usize * SECTOR]) {
            report.message = format!("read booted disk sectors at {} failed", lba);
            return report;
        }
        if lba == 0 {
            chunk[0x1AC..0x1AC + 16].copy_from_slice(&new_sys_guid);
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
