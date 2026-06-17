//! "Top up version" — upgrade an existing TablesOS USB volume to the running
//! version **in place, keeping its data**.
//!
//! Unlike [`crate::install`] (which overwrites a target with a *fresh* image and
//! a brand-new identity), this:
//!
//! 1. Confirms the target is a TablesOS volume of a *known, not-newer* version
//!    (downgrades are refused) and is **not** the disk we booted from.
//! 2. Mounts the target's volume, runs the engine's data migration ladder
//!    ([`tablestore::migrate`]) from the target's version up to ours, and reads
//!    the live page high-water mark.
//! 3. Relocates the live volume pages to the data location the *new* boot prefix
//!    expects (kernels of different versions differ in size, so the volume's
//!    start LBA can move) and re-stamps the superblock with the current version.
//! 4. Replaces the boot prefix (custom MBR + stage2 + kernel + ESP) with the
//!    running version's, **preserving the target's existing system GUID** so it
//!    stays its own disk.
//! 5. Verifies by re-reading the MBR and re-mounting the volume.
//!
//! The result is byte-equivalent to a fresh install of the current version with
//! the old data volume carried across to the new layout.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use tablestore::block::SECTOR;
use tablestore::journal::PAGE_SECTORS;
use tablestore::{migrate, BlockDevice, Store};

use crate::ata;
use crate::install::BaseOffsetDevice;
use crate::usb::xhci::{self, UsbMscDevice};

const H_MAGIC: usize = 0x180; // "TBLSBOOT"
const H_VERSION: usize = 0x188; // u32 packed product version
const H_DATA_LBA: usize = 0x18C; // u64 volume start LBA
const H_SYS_GUID: usize = 0x1AC; // 16-byte system GUID

/// Sectors copied per batched USB transfer when relocating the volume.
const CHUNK_SECTORS: u64 = 64;

/// Outcome of one upgrade attempt. Always returned so the UI can show
/// per-step progress whether or not it succeeded.
#[derive(Clone)]
pub struct UpgradeReport {
    pub target_slot: u8,
    /// Version found on the target before the upgrade.
    pub from_version: u32,
    /// Version we brought it up to (always the running [`tablestore::VERSION`]).
    pub to_version: u32,
    /// The target's system GUID — preserved across the upgrade.
    pub sys_guid: [u8; 16],
    pub old_data_lba: u64,
    pub new_data_lba: u64,
    /// Live pages relocated (the volume high-water mark; 0 if the data location
    /// was unchanged and nothing had to move).
    pub pages_relocated: u64,
    /// Data-migration steps the engine applied (0 when already current format).
    pub migration_steps: u32,
    /// Tables visible after re-mounting (sanity signal that data survived).
    pub tables_after: u64,
    pub verify_mbr_ok: bool,
    pub verify_mount_ok: bool,
    pub message: String,
}

impl UpgradeReport {
    fn new(target_slot: u8) -> Self {
        Self {
            target_slot,
            from_version: 0,
            to_version: tablestore::VERSION,
            sys_guid: [0u8; 16],
            old_data_lba: 0,
            new_data_lba: 0,
            pages_relocated: 0,
            migration_steps: 0,
            tables_after: 0,
            verify_mbr_ok: false,
            verify_mount_ok: false,
            message: String::new(),
        }
    }
    fn fail(mut self, msg: String) -> Self {
        self.message = msg;
        self
    }
}

/// Top up the volume on `target_slot_id` to the running version, keeping its
/// data. `booted_data_lba` is the running image's data location (the layout the
/// new boot prefix expects); `booted_sys_guid` is the running disk's identity
/// (used only to refuse upgrading the booted disk itself).
pub fn upgrade_usb(
    target_slot_id: u8,
    booted_data_lba: u64,
    booted_sys_guid: &[u8; 16],
) -> UpgradeReport {
    let mut report = UpgradeReport::new(target_slot_id);

    // ---- Open + identify the target -----------------------------------
    let mut usb = match UsbMscDevice::open(target_slot_id) {
        Ok(d) => d,
        Err(e) => return report.fail(format!("open USB target: {:?}", e)),
    };
    let usb_total = usb.sector_count();

    let mut mbr = [0u8; SECTOR];
    if usb.read_sector(0, &mut mbr).is_err() {
        return report.fail("could not read target MBR (sector 0)".into());
    }
    if &mbr[H_MAGIC..H_MAGIC + 8] != b"TBLSBOOT" {
        return report.fail("target is not a TablesOS disk (no TBLSBOOT header)".into());
    }
    let from_version = u32::from_le_bytes(mbr[H_VERSION..H_VERSION + 4].try_into().unwrap());
    let old_data_lba = u64::from_le_bytes(mbr[H_DATA_LBA..H_DATA_LBA + 8].try_into().unwrap());
    let mut sys_guid = [0u8; 16];
    sys_guid.copy_from_slice(&mbr[H_SYS_GUID..H_SYS_GUID + 16]);
    report.from_version = from_version;
    report.old_data_lba = old_data_lba;
    report.new_data_lba = booted_data_lba;
    report.sys_guid = sys_guid;

    // ---- Safety / policy gates ----------------------------------------
    if &sys_guid == booted_sys_guid {
        return report.fail("that is the disk TablesOS is running from — cannot upgrade itself".into());
    }
    if from_version > tablestore::VERSION {
        return report.fail(format!(
            "target is {} which is newer than this OS {} — downgrades are not allowed",
            tablestore::version_string(from_version),
            tablestore::VERSION_STR,
        ));
    }
    if !migrate::is_known(from_version) {
        return report.fail(format!(
            "target version {} is not a known release — cannot migrate",
            tablestore::version_string(from_version),
        ));
    }
    if old_data_lba == 0 || old_data_lba >= usb_total {
        return report.fail(format!("target MBR has an implausible data LBA {}", old_data_lba));
    }

    // ---- Mount, migrate data, learn the live size ---------------------
    let old_vol_sectors = usb_total - old_data_lba;
    let new_vol_sectors = usb_total - booted_data_lba;
    let new_total_pages = new_vol_sectors / PAGE_SECTORS;

    let (hwm, steps) = {
        let base = BaseOffsetDevice::new(usb, old_data_lba, old_vol_sectors);
        let mut store = match Store::open(base) {
            Ok(s) => s,
            Err(e) => return report.fail(format!("mount target volume: {:?}", e)),
        };
        let steps = match migrate::migrate_data(&mut store, from_version, tablestore::VERSION) {
            Ok(n) => n,
            Err(e) => return report.fail(format!("data migration: {:?}", e)),
        };
        (store.hwm(), steps)
        // `store` drops here, releasing the USB handle; subsequent device I/O
        // goes through the slot-keyed batched helpers and a fresh re-open.
    };
    report.migration_steps = steps;

    if hwm > new_total_pages {
        return report.fail(format!(
            "target too small for the new layout: {} live pages but only {} fit after the larger boot prefix",
            hwm, new_total_pages
        ));
    }

    // ---- Relocate the live volume pages (0..hwm) ----------------------
    // Only the pages at/below the high-water mark hold live content; free
    // space beyond it need not move. If the data location is unchanged there
    // is nothing to relocate.
    if booted_data_lba != old_data_lba {
        let n_sectors = hwm * PAGE_SECTORS;
        // Buffer the whole live region in RAM, then write it at the new base.
        // This sidesteps any source/destination overlap between the two
        // locations on the device. The live region is bounded by `hwm`
        // (journal + catalog + rows), comfortably within the kernel heap.
        let mut buf = vec![0u8; n_sectors as usize * SECTOR];
        let mut off = 0u64;
        while off < n_sectors {
            let n = CHUNK_SECTORS.min(n_sectors - off);
            let b = off as usize * SECTOR;
            if let Err(e) = xhci::msc_read_blocks_slot(
                target_slot_id,
                (old_data_lba + off) as u32,
                n as u16,
                &mut buf[b..b + n as usize * SECTOR],
            ) {
                return report.fail(format!("read old volume at +{}: {}", off, e));
            }
            off += n;
        }
        let mut off = 0u64;
        while off < n_sectors {
            let n = CHUNK_SECTORS.min(n_sectors - off);
            let b = off as usize * SECTOR;
            if let Err(e) = xhci::msc_write_blocks(
                target_slot_id,
                (booted_data_lba + off) as u32,
                n as u16,
                &buf[b..b + n as usize * SECTOR],
            ) {
                return report.fail(format!("write relocated volume at +{}: {}", off, e));
            }
            off += n;
        }
        report.pages_relocated = hwm;
    }

    // ---- Re-stamp the superblock (sets new size + current version) ----
    {
        let usb2 = match UsbMscDevice::open(target_slot_id) {
            Ok(d) => d,
            Err(e) => return report.fail(format!("re-open USB after relocate: {:?}", e)),
        };
        let base = BaseOffsetDevice::new(usb2, booted_data_lba, new_vol_sectors);
        let mut store = match Store::open(base) {
            Ok(s) => s,
            Err(e) => return report.fail(format!("mount relocated volume: {:?}", e)),
        };
        if let Err(e) = store.finalize_upgrade(new_total_pages) {
            return report.fail(format!("finalize (re-stamp superblock): {:?}", e));
        }
        let _ = store.device_mut().flush();
    }

    // ---- Write the new boot prefix, preserving the target's GUID ------
    // Copy LBAs 0..booted_data_lba from the booted disk (primary IDE master),
    // stamping the target's *existing* system GUID back into LBA 0 so the
    // upgraded stick keeps its identity. The new MBR already carries this
    // version and a data LBA of `booted_data_lba`, so it is self-consistent
    // with the relocated volume.
    let mut chunk = vec![0u8; CHUNK_SECTORS as usize * SECTOR];
    let mut sector = [0u8; SECTOR];
    let mut lba = 0u64;
    while lba < booted_data_lba {
        let n = CHUNK_SECTORS.min(booted_data_lba - lba);
        for i in 0..n {
            if !ata::read_sector_at(0x1F0, false, lba + i, &mut sector) {
                return report.fail(format!("read booted disk sector {} failed", lba + i));
            }
            if lba + i == 0 {
                sector[H_SYS_GUID..H_SYS_GUID + 16].copy_from_slice(&sys_guid);
            }
            let o = i as usize * SECTOR;
            chunk[o..o + SECTOR].copy_from_slice(&sector);
        }
        if let Err(e) = xhci::msc_write_blocks(
            target_slot_id,
            lba as u32,
            n as u16,
            &chunk[..n as usize * SECTOR],
        ) {
            return report.fail(format!("write boot prefix at {}: {}", lba, e));
        }
        lba += n;
    }

    // ---- Verify -------------------------------------------------------
    let mut vusb = match UsbMscDevice::open(target_slot_id) {
        Ok(d) => d,
        Err(e) => return report.fail(format!("verify re-open USB: {:?}; data was upgraded", e)),
    };
    let mut check = [0u8; SECTOR];
    if vusb.read_sector(0, &mut check).is_err() {
        return report.fail("verify read MBR failed".into());
    }
    report.verify_mbr_ok = &check[H_MAGIC..H_MAGIC + 8] == b"TBLSBOOT"
        && u32::from_le_bytes(check[H_VERSION..H_VERSION + 4].try_into().unwrap())
            == tablestore::VERSION
        && &check[H_SYS_GUID..H_SYS_GUID + 16] == &sys_guid[..];
    if !report.verify_mbr_ok {
        return report.fail("MBR re-read disagrees with the new version/GUID we wrote".into());
    }
    let base = BaseOffsetDevice::new(vusb, booted_data_lba, new_vol_sectors);
    match Store::open(base) {
        Ok(mut s) => {
            report.tables_after = s.list_tables().map(|t| t.len() as u64).unwrap_or(0);
            report.verify_mount_ok = true;
            report.message = format!(
                "OK — upgraded {} → {}, {} table(s) preserved{}",
                tablestore::version_string(from_version),
                tablestore::VERSION_STR,
                report.tables_after,
                if report.pages_relocated > 0 {
                    format!(", relocated {} live pages", report.pages_relocated)
                } else {
                    String::new()
                },
            );
        }
        Err(e) => report.message = format!("verify re-mount failed: {:?}", e),
    }
    report
}
