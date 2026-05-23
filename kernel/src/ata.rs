//! ATA PIO driver for the store disk.
//!
//! TablesOS is single-device: the disk we booted from = primary IDE **master**
//! (under QEMU `-machine pc` and on legacy SATA-in-IDE-mode hardware). The
//! TablesOS volume begins at the MBR's data-location LBA; this driver hides
//! that base offset so the engine sees a volume starting at sector 0 and can
//! never reach the boot/kernel region in front of it. LBA28 single-sector
//! PIO with status polling — simple and correct; no DMA, no interrupts.
//! LBA48 (volumes > 128 GiB) is a documented seam.
//!
//! Driving a real USB-mass-storage pendrive instead needs a USB host stack;
//! that swaps in here behind the same [`BlockDevice`] impl with zero changes
//! above (see IMPLEMENTATION.md / BUILD.md).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use tablestore::block::SECTOR;
use tablestore::{BlockDevice, Result, StoreError};
use x86_64::instructions::port::{Port, PortReadOnly, PortWriteOnly};

const BASE: u16 = 0x1F0;
const CTRL: u16 = 0x3F6;
// Single device: the disk we booted from = primary IDE **master**.
const DRIVE_MASTER_LBA: u8 = 0xE0;

const ST_BSY: u8 = 0x80;
const ST_DRDY: u8 = 0x40;
const ST_DRQ: u8 = 0x08;
const ST_ERR: u8 = 0x01;

pub struct Ata {
    data: Port<u16>,
    err: PortReadOnly<u8>,
    seccount: PortWriteOnly<u8>,
    lba0: PortWriteOnly<u8>,
    lba1: PortWriteOnly<u8>,
    lba2: PortWriteOnly<u8>,
    drive: PortWriteOnly<u8>,
    status: PortReadOnly<u8>,
    command: PortWriteOnly<u8>,
    ctrl: PortWriteOnly<u8>,
    /// First device LBA of the TablesOS volume (boot/kernel live before it).
    base: u64,
    /// Volume size in sectors (whole disk minus `base`).
    sectors: u64,
}

impl Ata {
    fn raw() -> Self {
        Ata {
            data: Port::new(BASE),
            err: PortReadOnly::new(BASE + 1),
            seccount: PortWriteOnly::new(BASE + 2),
            lba0: PortWriteOnly::new(BASE + 3),
            lba1: PortWriteOnly::new(BASE + 4),
            lba2: PortWriteOnly::new(BASE + 5),
            drive: PortWriteOnly::new(BASE + 6),
            status: PortReadOnly::new(BASE + 7),
            command: PortWriteOnly::new(BASE + 7),
            ctrl: PortWriteOnly::new(CTRL),
            base: 0,
            sectors: 0,
        }
    }

    fn poll(&mut self) -> Result<()> {
        unsafe {
            for _ in 0..4 {
                let _ = self.status.read(); // 400ns delay
            }
            let mut spins = 0u32;
            loop {
                let s = self.status.read();
                if s & ST_ERR != 0 {
                    let _ = self.err.read();
                    return Err(StoreError::Io);
                }
                if s & ST_BSY == 0 && s & ST_DRQ != 0 {
                    return Ok(());
                }
                if s & ST_BSY == 0 && s & ST_DRDY != 0 {
                    return Ok(());
                }
                spins += 1;
                if spins > 10_000_000 {
                    return Err(StoreError::Io);
                }
            }
        }
    }

    /// `lba` here is an absolute device LBA (caller already added `base`).
    fn select(&mut self, lba: u64, count: u8) {
        unsafe {
            self.drive
                .write(DRIVE_MASTER_LBA | ((lba >> 24) & 0x0F) as u8);
            for _ in 0..4 {
                let _ = self.status.read();
            }
            self.seccount.write(count);
            self.lba0.write(lba as u8);
            self.lba1.write((lba >> 8) as u8);
            self.lba2.write((lba >> 16) as u8);
        }
    }

    /// Probe the primary master with IDENTIFY. `base` is the device LBA where
    /// the TablesOS volume begins (from BootInfo); the engine then sees a
    /// volume starting at sector 0 and can never reach the boot/kernel region.
    pub fn probe(base: u64) -> Option<Ata> {
        let mut a = Ata::raw();
        a.base = base;
        unsafe {
            a.ctrl.write(0x00); // enable (no nIEN games; we poll)
            a.drive.write(DRIVE_MASTER_LBA);
            for _ in 0..4 {
                let _ = a.status.read();
            }
            a.seccount.write(0);
            a.lba0.write(0);
            a.lba1.write(0);
            a.lba2.write(0);
            a.command.write(0xEC); // IDENTIFY
            if a.status.read() == 0 {
                return None; // no device
            }
        }
        if a.poll().is_err() {
            return None;
        }
        let mut id = [0u16; 256];
        for w in id.iter_mut() {
            *w = unsafe { a.data.read() };
        }
        // LBA28 total sectors live in words 60..=61.
        let total = id[60] as u64 | ((id[61] as u64) << 16);
        a.sectors = total.saturating_sub(a.base); // volume = disk - boot region
        Some(a)
    }

    /// Read the MBR (absolute LBA 0, *ignoring* the volume base offset) for
    /// the system-GUID identity check. Read-only.
    pub fn read_boot_sector(&mut self, buf: &mut [u8]) -> Result<()> {
        if buf.len() != SECTOR {
            return Err(StoreError::Io);
        }
        self.select(0, 1);
        unsafe {
            self.command.write(0x20); // READ SECTORS
        }
        self.poll()?;
        for chunk in buf.chunks_exact_mut(2) {
            let w = unsafe { self.data.read() };
            chunk[0] = w as u8;
            chunk[1] = (w >> 8) as u8;
        }
        Ok(())
    }
}

impl BlockDevice for Ata {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        if buf.len() != SECTOR || lba >= self.sectors {
            return Err(StoreError::Io);
        }
        self.select(self.base + lba, 1);
        unsafe {
            self.command.write(0x20); // READ SECTORS
        }
        self.poll()?;
        for chunk in buf.chunks_exact_mut(2) {
            let w = unsafe { self.data.read() };
            chunk[0] = w as u8;
            chunk[1] = (w >> 8) as u8;
        }
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> Result<()> {
        if buf.len() != SECTOR || lba >= self.sectors {
            return Err(StoreError::Io);
        }
        self.select(self.base + lba, 1);
        unsafe {
            self.command.write(0x30); // WRITE SECTORS
        }
        self.poll()?;
        for chunk in buf.chunks_exact(2) {
            let w = chunk[0] as u16 | ((chunk[1] as u16) << 8);
            unsafe { self.data.write(w) };
        }
        self.flush()
    }

    fn flush(&mut self) -> Result<()> {
        unsafe {
            self.command.write(0xE7); // CACHE FLUSH
        }
        // Wait for BSY to clear.
        unsafe {
            let mut spins = 0u32;
            loop {
                let s = self.status.read();
                if s & ST_BSY == 0 {
                    if s & ST_ERR != 0 {
                        return Err(StoreError::Io);
                    }
                    return Ok(());
                }
                spins += 1;
                if spins > 10_000_000 {
                    return Err(StoreError::Io);
                }
            }
        }
    }
}

// =====================================================================
// Read-only diagnostic enumeration of all four legacy-IDE slots.
//
// This is intentionally separate from the bound `Ata` driver above. It
// pokes the IDE ports directly to query IDENTIFY and read sector 0 of
// each present device; it never issues a write command. The booted-disk
// identity gate is unaffected — diagnostic reads on other slots do not
// touch the booted volume, and the bound driver re-writes the drive
// register on its next operation.
//
// Used by the "Drives" screen on the Table List.
// =====================================================================

const SECONDARY_BASE: u16 = 0x170;
const SECONDARY_CTRL: u16 = 0x376;
const SLOT_POLL_LIMIT: u32 = 500_000;

#[derive(Clone)]
pub struct DriveInfo {
    /// Human label for this drive's location. Static strings for IDE
    /// (`"primary master"`, …); dynamic `"USB slot N (port M)"`-style
    /// strings for USB devices surfaced by the xHCI driver.
    pub slot: String,
    pub present: bool,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub lba28_sectors: u64,
    pub lba48_sectors: u64,
    pub boot_sig_ok: bool,
    pub mbr: MbrInfo,
    /// True if this slot's on-disk system GUID equals the one captured at
    /// boot — i.e. it really is the disk TablesOS is running from.
    pub booted: bool,
}

#[derive(Clone)]
pub enum MbrInfo {
    /// `TBLSBOOT` magic at MBR offset 0x1B0 → our own custom MBR.
    TablesOs {
        version: u16,
        data_loc_lba: u64,
        stage2_lba: u32,
        stage2_sectors: u16,
        kernel_lba: u32,
        kernel_sectors: u32,
        kernel_load: u32,
        kernel_entry: u32,
        sys_guid: [u8; 16],
    },
    /// Classic 4-entry MBR partition table at 0x1BE (only non-empty entries).
    Partitioned { parts: Vec<Partition> },
    /// Boot signature missing, sector all zeros.
    Blank,
    /// Boot signature missing or unrecognised data layout.
    Unknown,
    /// Sector 0 could not be read.
    Unreadable,
}

#[derive(Clone, Copy)]
pub struct Partition {
    pub bootable: bool,
    pub type_byte: u8,
    pub start_lba: u32,
    pub sectors: u32,
}

/// Probe all four legacy-IDE slots in fixed order. Slots without a device
/// come back with `present = false` and empty strings.
pub fn enumerate_drives(booted_sys_guid: &[u8; 16]) -> Vec<DriveInfo> {
    let slots = [
        (BASE, CTRL, false, "primary master"),
        (BASE, CTRL, true, "primary slave"),
        (SECONDARY_BASE, SECONDARY_CTRL, false, "secondary master"),
        (SECONDARY_BASE, SECONDARY_CTRL, true, "secondary slave"),
    ];
    let mut out = Vec::with_capacity(4);
    for &(base, ctrl, slave, name) in &slots {
        out.push(probe_one(base, ctrl, slave, name, booted_sys_guid));
    }
    out
}

fn probe_one(
    base: u16,
    ctrl: u16,
    slave: bool,
    name: &'static str,
    booted_sys_guid: &[u8; 16],
) -> DriveInfo {
    let absent = || DriveInfo {
        slot: name.to_string(),
        present: false,
        model: String::new(),
        serial: String::new(),
        firmware: String::new(),
        lba28_sectors: 0,
        lba48_sectors: 0,
        boot_sig_ok: false,
        mbr: MbrInfo::Unknown,
        booted: false,
    };
    let Some(id) = identify_slot(base, ctrl, slave) else {
        return absent();
    };
    let model = read_id_string(&id, 27, 20);
    let serial = read_id_string(&id, 10, 10);
    let firmware = read_id_string(&id, 23, 4);
    let lba28 = id[60] as u64 | ((id[61] as u64) << 16);
    let lba48 = id[100] as u64
        | ((id[101] as u64) << 16)
        | ((id[102] as u64) << 32)
        | ((id[103] as u64) << 48);
    let mut sec0 = [0u8; SECTOR];
    let (mbr, boot_sig_ok) = match read_sector_at(base, slave, 0, &mut sec0) {
        true => {
            let sig = sec0[510] == 0x55 && sec0[511] == 0xAA;
            (parse_mbr(&sec0), sig)
        }
        false => (MbrInfo::Unreadable, false),
    };
    let booted = match &mbr {
        MbrInfo::TablesOs { sys_guid, .. } => sys_guid == booted_sys_guid,
        _ => false,
    };
    DriveInfo {
        slot: name.to_string(),
        present: true,
        model,
        serial,
        firmware,
        lba28_sectors: lba28,
        lba48_sectors: lba48,
        boot_sig_ok,
        mbr,
        booted,
    }
}

fn identify_slot(base: u16, ctrl: u16, slave: bool) -> Option<[u16; 256]> {
    let mut data: Port<u16> = Port::new(base);
    let mut err: PortReadOnly<u8> = PortReadOnly::new(base + 1);
    let mut seccount: PortWriteOnly<u8> = PortWriteOnly::new(base + 2);
    let mut lba0: PortWriteOnly<u8> = PortWriteOnly::new(base + 3);
    let mut lba1: PortWriteOnly<u8> = PortWriteOnly::new(base + 4);
    let mut lba2: PortWriteOnly<u8> = PortWriteOnly::new(base + 5);
    let mut drive: PortWriteOnly<u8> = PortWriteOnly::new(base + 6);
    let mut status: PortReadOnly<u8> = PortReadOnly::new(base + 7);
    let mut command: PortWriteOnly<u8> = PortWriteOnly::new(base + 7);
    let mut ctrl_p: PortWriteOnly<u8> = PortWriteOnly::new(ctrl);
    unsafe {
        ctrl_p.write(0x00);
        drive.write(if slave { 0xB0 } else { 0xA0 });
        for _ in 0..4 {
            let _ = status.read();
        }
        // Floating bus on a totally empty channel reads 0xFF.
        if status.read() == 0xFF {
            return None;
        }
        seccount.write(0);
        lba0.write(0);
        lba1.write(0);
        lba2.write(0);
        command.write(0xEC); // IDENTIFY
        if status.read() == 0 {
            return None; // no device at this slot
        }
        let mut spins = 0u32;
        loop {
            let s = status.read();
            if s == 0xFF {
                return None;
            }
            if s & ST_ERR != 0 {
                let _ = err.read();
                return None;
            }
            if s & ST_BSY == 0 && s & ST_DRQ != 0 {
                break;
            }
            spins += 1;
            if spins > SLOT_POLL_LIMIT {
                return None;
            }
        }
        let mut id = [0u16; 256];
        for w in id.iter_mut() {
            *w = data.read();
        }
        Some(id)
    }
}

/// Read one sector at an absolute LBA from any IDE slot. Used by the
/// diagnostic enumeration (for sector 0 of every disk) and by the
/// install-to-USB flow (to copy the booted disk's boot prefix verbatim).
pub fn read_sector_at(
    base: u16,
    slave: bool,
    lba: u64,
    buf: &mut [u8; SECTOR],
) -> bool {
    let mut data: Port<u16> = Port::new(base);
    let mut err: PortReadOnly<u8> = PortReadOnly::new(base + 1);
    let mut seccount: PortWriteOnly<u8> = PortWriteOnly::new(base + 2);
    let mut lba0: PortWriteOnly<u8> = PortWriteOnly::new(base + 3);
    let mut lba1: PortWriteOnly<u8> = PortWriteOnly::new(base + 4);
    let mut lba2: PortWriteOnly<u8> = PortWriteOnly::new(base + 5);
    let mut drive: PortWriteOnly<u8> = PortWriteOnly::new(base + 6);
    let mut status: PortReadOnly<u8> = PortReadOnly::new(base + 7);
    let mut command: PortWriteOnly<u8> = PortWriteOnly::new(base + 7);
    let drive_byte: u8 =
        (if slave { 0xF0 } else { 0xE0 }) | ((lba >> 24) & 0x0F) as u8;
    unsafe {
        drive.write(drive_byte);
        for _ in 0..4 {
            let _ = status.read();
        }
        seccount.write(1);
        lba0.write(lba as u8);
        lba1.write((lba >> 8) as u8);
        lba2.write((lba >> 16) as u8);
        command.write(0x20); // READ SECTORS (LBA28)
        let mut spins = 0u32;
        loop {
            let s = status.read();
            if s == 0xFF {
                return false;
            }
            if s & ST_ERR != 0 {
                let _ = err.read();
                return false;
            }
            if s & ST_BSY == 0 && s & ST_DRQ != 0 {
                break;
            }
            spins += 1;
            if spins > SLOT_POLL_LIMIT {
                return false;
            }
        }
        for chunk in buf.chunks_exact_mut(2) {
            let w = data.read();
            chunk[0] = w as u8;
            chunk[1] = (w >> 8) as u8;
        }
    }
    true
}

/// IDENTIFY stores strings as 16-bit words with the high byte first. Decode
/// `n_words` words starting at `start`, treating each as ASCII, trim trailing
/// spaces.
fn read_id_string(id: &[u16; 256], start: usize, n_words: usize) -> String {
    let mut s = String::new();
    for &w in &id[start..start + n_words] {
        for b in [(w >> 8) as u8, w as u8] {
            if (b' '..=b'~').contains(&b) {
                s.push(b as char);
            }
        }
    }
    while s.ends_with(' ') {
        s.pop();
    }
    s
}

/// Classify the first 512 bytes of a disk: TablesOS magic at 0x1B0,
/// classic 4-entry MBR partition table at 0x1BE, blank, or unknown.
/// Accepts any slice ≥ 512 bytes so the USB-MSC path can reuse it on
/// whatever READ(10) returned for LBA 0.
pub fn parse_mbr(s: &[u8]) -> MbrInfo {
    if s.len() < 512 {
        return MbrInfo::Unreadable;
    }
    let boot_sig = s[510] == 0x55 && s[511] == 0xAA;
    if &s[0x1B0..0x1B8] == b"TBLSBOOT" && boot_sig {
        let version = u16::from_le_bytes([s[0x1B8], s[0x1B9]]);
        let data_loc_lba = u64::from_le_bytes(s[0x1BC..0x1C4].try_into().unwrap());
        let stage2_lba = u32::from_le_bytes(s[0x1C4..0x1C8].try_into().unwrap());
        let stage2_sectors = u16::from_le_bytes([s[0x1C8], s[0x1C9]]);
        let kernel_lba = u32::from_le_bytes(s[0x1CC..0x1D0].try_into().unwrap());
        let kernel_sectors = u32::from_le_bytes(s[0x1D0..0x1D4].try_into().unwrap());
        let kernel_load = u32::from_le_bytes(s[0x1D4..0x1D8].try_into().unwrap());
        let kernel_entry = u32::from_le_bytes(s[0x1D8..0x1DC].try_into().unwrap());
        let mut sys_guid = [0u8; 16];
        sys_guid.copy_from_slice(&s[0x1DC..0x1EC]);
        return MbrInfo::TablesOs {
            version,
            data_loc_lba,
            stage2_lba,
            stage2_sectors,
            kernel_lba,
            kernel_sectors,
            kernel_load,
            kernel_entry,
            sys_guid,
        };
    }
    if !boot_sig {
        if s.iter().all(|&b| b == 0) {
            return MbrInfo::Blank;
        }
        return MbrInfo::Unknown;
    }
    let mut parts = Vec::new();
    for i in 0..4 {
        let off = 0x1BE + i * 16;
        let type_byte = s[off + 4];
        if type_byte == 0 {
            continue;
        }
        let bootable = s[off] == 0x80;
        let start_lba = u32::from_le_bytes(s[off + 8..off + 12].try_into().unwrap());
        let sectors = u32::from_le_bytes(s[off + 12..off + 16].try_into().unwrap());
        parts.push(Partition {
            bootable,
            type_byte,
            start_lba,
            sectors,
        });
    }
    if parts.is_empty() {
        MbrInfo::Unknown
    } else {
        MbrInfo::Partitioned { parts }
    }
}

pub fn partition_type_name(t: u8) -> &'static str {
    match t {
        0x01 => "FAT12",
        0x04 => "FAT16 <32M",
        0x05 => "Extended (CHS)",
        0x06 => "FAT16",
        0x07 => "NTFS/exFAT",
        0x0B => "FAT32 (CHS)",
        0x0C => "FAT32 (LBA)",
        0x0E => "FAT16 (LBA)",
        0x0F => "Extended (LBA)",
        0x11 => "Hidden FAT12",
        0x16 => "Hidden FAT16",
        0x17 => "Hidden NTFS",
        0x1B => "Hidden FAT32",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x85 => "Linux extended",
        0x8E => "Linux LVM",
        0xA5 => "FreeBSD",
        0xA8 => "macOS UFS",
        0xAF => "macOS HFS+",
        0xEE => "GPT protective",
        0xEF => "EFI System",
        0xFD => "Linux RAID",
        _ => "unknown",
    }
}

/// Decimal-ish human size for a sector count. No floats (avoids the soft-FP
/// dependency on `x86_64-unknown-none`).
pub fn human_size_sectors(sectors: u64) -> String {
    let bytes = sectors.saturating_mul(SECTOR as u64);
    if bytes >= 1u64 << 30 {
        let n = bytes.saturating_mul(100) >> 30;
        format!("{}.{:02} GiB", n / 100, n % 100)
    } else if bytes >= 1u64 << 20 {
        let n = bytes.saturating_mul(100) >> 20;
        format!("{}.{:02} MiB", n / 100, n % 100)
    } else if bytes >= 1u64 << 10 {
        format!("{} KiB", bytes >> 10)
    } else {
        format!("{} B", bytes)
    }
}

/// Hex form of a 16-byte GUID, spaced into two 8-byte halves for readability.
pub fn fmt_guid(g: &[u8; 16]) -> String {
    let mut s = String::with_capacity(33);
    for (i, b) in g.iter().enumerate() {
        if i == 8 {
            s.push(' ');
        }
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{:02X}", b));
    }
    s
}
