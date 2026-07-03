//! "Top up version" — upgrade an existing TablesOS USB volume to the running
//! version **in place, keeping its data**.
//!
//! Unlike [`crate::install`] (which overwrites a target with a *fresh* image and
//! a brand-new identity), this:
//!
//! 1. Confirms the target is a TablesOS volume of a *known, not-newer* version
//!    (downgrades are refused) and is **not** the disk we booted from.
//! 2. Reads the target volume's live logical pages into RAM through the reader
//!    matching its on-disk format ([`tablestore::compat_v3`] for the retired
//!    journalled format, the copy-on-write [`tablestore::pager`] for v0.4.0+).
//! 3. Rebuilds a *fresh* copy-on-write volume at the data location the new boot
//!    prefix expects (kernels of different versions differ in size, so the
//!    volume's start LBA can move), writing each logical page under its original
//!    number. This both relocates and — for a pre-CoW source — converts the
//!    format, and stamps the current version into every anchor it writes.
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
use alloc::vec::Vec;
use tablestore::block::SECTOR;
use tablestore::compat_v3::V3Volume;
use tablestore::journal::{FIRST_DATA_PAGE, PAGE_SECTORS};
use tablestore::pager::Pager;
use tablestore::{migrate, BlockDevice, Store};

use crate::install::{self, BaseOffsetDevice};
use crate::usb::xhci::{self, UsbMscDevice};

/// On-disk version at/above which the volume uses the copy-on-write format
/// (v0.4.0). Anything below it is the retired journalled format and is read
/// through [`V3Volume`].
const COW_FORMAT_VERSION: u32 = 4 << 8; // packed v0.4.0

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

/// Read the old volume's live logical pages `[FIRST_DATA_PAGE, hwm)` into RAM,
/// plus its logical header pointers, choosing the reader by on-disk format
/// version. Consumes the source device (a USB stick or the SD card) so the
/// target region can then be re-opened for writing.
///
/// Returns `(pages, hwm, catalog_head, free_head)` where `pages[i]` is logical
/// page `FIRST_DATA_PAGE + i`.
fn read_old_logical<D: BlockDevice>(
    dev: D,
    old_data_lba: u64,
    old_vol_sectors: u64,
    from_version: u32,
) -> core::result::Result<(Vec<Vec<u8>>, u64, u64, u64), String> {
    let mut base = BaseOffsetDevice::new(dev, old_data_lba, old_vol_sectors);
    if from_version >= COW_FORMAT_VERSION {
        // Already copy-on-write: read through the current pager.
        let mut pager =
            Pager::mount(base).map_err(|e| format!("mount target volume: {:?}", e))?;
        let sb = pager.superblock();
        let mut pages = Vec::with_capacity(sb.hwm.saturating_sub(FIRST_DATA_PAGE) as usize);
        for lpn in FIRST_DATA_PAGE..sb.hwm {
            pages.push(
                pager
                    .read_page(lpn)
                    .map_err(|e| format!("read old page {}: {:?}", lpn, e))?,
            );
        }
        Ok((pages, sb.hwm, sb.catalog_head, sb.free_head))
    } else {
        // Retired journalled format: read through the frozen compat reader.
        let v3 = V3Volume::mount(&mut base)
            .map_err(|e| format!("mount target volume (pre-CoW): {:?}", e))?;
        let mut pages = Vec::with_capacity(v3.sb.hwm.saturating_sub(FIRST_DATA_PAGE) as usize);
        for lpn in FIRST_DATA_PAGE..v3.sb.hwm {
            pages.push(
                v3.read_page(&mut base, lpn)
                    .map_err(|e| format!("read old page {}: {:?}", lpn, e))?,
            );
        }
        Ok((pages, v3.sb.hwm, v3.sb.catalog_head, v3.sb.free_head))
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

    // ---- Read the old volume's live logical pages into RAM ------------
    // The copy-on-write format (v0.4.0+) scatters live pages across the whole
    // volume, so relocation can no longer copy a physical prefix. Instead we
    // read the old volume *logically* (through the reader matching its format)
    // into RAM, then rebuild a fresh CoW volume at the new location. This both
    // moves and, for a pre-CoW source, converts the format in one pass. RAM use
    // is bounded by the live page count (hwm), not the device size.
    let old_vol_sectors = usb_total - old_data_lba;
    let new_vol_sectors = usb_total - booted_data_lba;
    let new_total_pages = new_vol_sectors / PAGE_SECTORS;

    let (pages, hwm, catalog_head, free_head) =
        match read_old_logical(usb, old_data_lba, old_vol_sectors, from_version) {
            Ok(v) => v,
            Err(e) => return report.fail(e),
        };
    // Number of released versions between the target's and ours (cosmetic).
    report.migration_steps = migrate::KNOWN_VERSIONS
        .iter()
        .filter(|&&kv| kv > from_version && kv <= tablestore::VERSION)
        .count() as u32;

    if hwm > new_total_pages || new_total_pages <= FIRST_DATA_PAGE + 1 {
        return report.fail(format!(
            "target too small for the new layout: {} live pages, only {} pages available after the boot prefix",
            hwm, new_total_pages
        ));
    }

    // ---- Rebuild a fresh CoW volume at the new data location ----------
    // All old data is now in RAM, so writing the target region (which may
    // overlap the old volume) is safe. The new volume is stamped with the
    // current version by every anchor it writes.
    {
        let usb2 = match UsbMscDevice::open(target_slot_id) {
            Ok(d) => d,
            Err(e) => return report.fail(format!("re-open USB to write volume: {:?}", e)),
        };
        let base = BaseOffsetDevice::new(usb2, booted_data_lba, new_vol_sectors);
        let mut newp = match Pager::format(base) {
            Ok(p) => p,
            Err(e) => return report.fail(format!("format new volume: {:?}", e)),
        };
        if let Err(e) = migrate::rebuild_into(&mut newp, hwm, catalog_head, free_head, |lpn| {
            Ok(pages[(lpn - FIRST_DATA_PAGE) as usize].clone())
        }) {
            return report.fail(format!("rebuild volume: {:?}", e));
        }
        let _ = newp.device_mut().flush();
        report.pages_relocated = hwm.saturating_sub(FIRST_DATA_PAGE);
    }

    // ---- Write the new boot prefix, preserving the target's GUID ------
    // Copy LBAs 0..booted_data_lba from the disk we booted from, stamping the
    // target's *existing* system GUID back into LBA 0 so the upgraded stick keeps
    // its identity. The new MBR already carries this version and a data LBA of
    // `booted_data_lba`, so it is self-consistent with the relocated volume.
    let mut booted = match install::BootedReader::open(booted_sys_guid) {
        Some(b) => b,
        None => return report.fail("could not reach the booted disk to copy the boot prefix".into()),
    };
    let mut chunk = vec![0u8; CHUNK_SECTORS as usize * SECTOR];
    let mut lba = 0u64;
    while lba < booted_data_lba {
        let n = CHUNK_SECTORS.min(booted_data_lba - lba);
        if !booted.read(lba, n, &mut chunk[..n as usize * SECTOR]) {
            return report.fail(format!("read booted disk sectors at {} failed", lba));
        }
        if lba == 0 {
            chunk[H_SYS_GUID..H_SYS_GUID + 16].copy_from_slice(&sys_guid);
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
