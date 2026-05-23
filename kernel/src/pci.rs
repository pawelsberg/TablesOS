//! PCI configuration-space enumeration via the legacy I/O port mechanism
//! (CONFIG_ADDRESS = 0xCF8, CONFIG_DATA = 0xCFC). Read-only.
//!
//! This is **phase 1** of the USB-stack roadmap: the kernel does not yet
//! talk to xHCI / AHCI / NVMe, but listing them at least makes the kernel's
//! blind spots visible to the user. The Drives screen surfaces this.
//!
//! All reads here are independent of the bound IDE driver in `ata.rs`. No
//! BARs are activated, no devices touched beyond config-space reads.

use alloc::vec::Vec;
use x86_64::instructions::port::{Port, PortWriteOnly};

const CONFIG_ADDR: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

#[derive(Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class: u8,
    pub header_type: u8,
    /// Six 32-bit Base Address Register slots from a Type-0 header. For
    /// 64-bit memory BARs the high half is stored in the next slot. Bridge
    /// (Type-1) headers leave these zero.
    pub bars: [u32; 6],
    pub irq_line: u8,
    pub irq_pin: u8,
}

fn read32(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    let addr: u32 = 0x8000_0000
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | (off as u32 & 0xFC);
    let mut a: PortWriteOnly<u32> = PortWriteOnly::new(CONFIG_ADDR);
    let mut d: Port<u32> = Port::new(CONFIG_DATA);
    unsafe {
        a.write(addr);
        d.read()
    }
}

fn read_one(bus: u8, slot: u8, func: u8) -> Option<PciDevice> {
    let v_d = read32(bus, slot, func, 0x00);
    let vendor = v_d as u16;
    if vendor == 0xFFFF {
        return None;
    }
    let device = (v_d >> 16) as u16;
    let cls = read32(bus, slot, func, 0x08);
    let revision = cls as u8;
    let prog_if = (cls >> 8) as u8;
    let subclass = (cls >> 16) as u8;
    let class = (cls >> 24) as u8;
    let hdr = (read32(bus, slot, func, 0x0C) >> 16) as u8;
    let mut bars = [0u32; 6];
    if hdr & 0x7F == 0 {
        for (i, b) in bars.iter_mut().enumerate() {
            *b = read32(bus, slot, func, 0x10 + (i as u8) * 4);
        }
    }
    let irq = read32(bus, slot, func, 0x3C);
    Some(PciDevice {
        bus,
        slot,
        func,
        vendor,
        device,
        revision,
        prog_if,
        subclass,
        class,
        header_type: hdr,
        bars,
        irq_line: irq as u8,
        irq_pin: (irq >> 8) as u8,
    })
}

/// Brute-force scan: 256 buses × 32 slots × 8 functions. Empty positions
/// exit on the vendor-ID check, so this is fast even on small systems
/// (typical QEMU machine: < 20 devices, < 1 ms).
pub fn enumerate() -> Vec<PciDevice> {
    let mut out = Vec::new();
    for bus in 0u16..256 {
        for slot in 0u8..32 {
            let Some(d0) = read_one(bus as u8, slot, 0) else {
                continue;
            };
            let multi = d0.header_type & 0x80 != 0;
            out.push(d0);
            if multi {
                for f in 1u8..8 {
                    if let Some(d) = read_one(bus as u8, slot, f) {
                        out.push(d);
                    }
                }
            }
        }
    }
    out
}

/// Short, human-readable name for the device's PCI class triple.
pub fn class_name(class: u8, subclass: u8, prog_if: u8) -> &'static str {
    match (class, subclass, prog_if) {
        (0x00, _, _) => "Unclassified",
        (0x01, 0x00, _) => "SCSI controller",
        (0x01, 0x01, _) => "IDE controller",
        (0x01, 0x04, _) => "RAID controller",
        (0x01, 0x05, _) => "ATA controller",
        (0x01, 0x06, 0x01) => "SATA AHCI",
        (0x01, 0x06, _) => "SATA controller",
        (0x01, 0x08, 0x02) => "NVMe controller",
        (0x01, 0x08, _) => "NVM controller",
        (0x01, _, _) => "Mass-storage controller",
        (0x02, _, _) => "Network controller",
        (0x03, _, _) => "Display controller",
        (0x04, _, _) => "Multimedia controller",
        (0x05, _, _) => "Memory controller",
        (0x06, 0x00, _) => "Host bridge",
        (0x06, 0x01, _) => "ISA bridge",
        (0x06, 0x04, _) => "PCI-to-PCI bridge",
        (0x06, _, _) => "Bridge",
        (0x07, _, _) => "Communication controller",
        (0x08, _, _) => "Generic system peripheral",
        (0x09, _, _) => "Input device",
        (0x0B, _, _) => "Processor",
        (0x0C, 0x03, 0x00) => "USB UHCI (USB 1.x)",
        (0x0C, 0x03, 0x10) => "USB OHCI (USB 1.x)",
        (0x0C, 0x03, 0x20) => "USB EHCI (USB 2.0)",
        (0x0C, 0x03, 0x30) => "USB xHCI (USB 3.x)",
        (0x0C, 0x03, 0x40) => "USB4 controller",
        (0x0C, 0x03, 0xFE) => "USB device (not host)",
        (0x0C, 0x03, _) => "USB controller",
        (0x0C, 0x05, _) => "SMBus controller",
        (0x0C, _, _) => "Serial bus controller",
        _ => "(other)",
    }
}

/// Best-effort vendor name. Tiny hand-maintained table covering the IDs
/// that actually appear under QEMU and on common hardware; everything else
/// shows up as "(vendor 0xVVVV)".
pub fn vendor_name(vendor: u16) -> &'static str {
    match vendor {
        0x1022 => "AMD",
        0x10DE => "NVIDIA",
        0x10EC => "Realtek",
        0x1234 => "QEMU std VGA",
        0x14E4 => "Broadcom",
        0x15AD => "VMware",
        0x1AF4 => "Red Hat (virtio)",
        0x1B36 => "Red Hat (QEMU)",
        0x8086 => "Intel",
        _ => "(unknown vendor)",
    }
}

pub fn is_usb_host(d: &PciDevice) -> bool {
    d.class == 0x0C && d.subclass == 0x03
}

pub fn is_mass_storage(d: &PciDevice) -> bool {
    d.class == 0x01
}

/// Decode the device's BAR pair `[i, i+1]` into a physical address. Handles
/// 32-bit and 64-bit memory BARs (xHCI normally uses 64-bit). Returns 0 if
/// the BAR is an I/O BAR, unassigned, or `i` is out of range.
pub fn bar_address(d: &PciDevice, i: usize) -> u64 {
    let Some(&b0) = d.bars.get(i) else { return 0 };
    if b0 == 0 || b0 == 0xFFFFFFFF {
        return 0;
    }
    if b0 & 1 != 0 {
        // I/O BAR (bit 0 = 1). Not relevant for MMIO controllers.
        return 0;
    }
    let kind = (b0 >> 1) & 0x3;
    let lo = (b0 & !0xF) as u64;
    if kind == 0x2 {
        // 64-bit memory BAR: combine with the next BAR slot.
        let hi = d.bars.get(i + 1).copied().unwrap_or(0) as u64;
        lo | (hi << 32)
    } else {
        lo
    }
}

/// Read/modify/write the 16-bit PCI Command register (offset 0x04).
fn modify_command(bus: u8, slot: u8, func: u8, set: u16, clear: u16) -> u16 {
    let v = read32(bus, slot, func, 0x04);
    let cmd_old = v as u16;
    let cmd_new = (cmd_old & !clear) | set;
    let merged = (v & 0xFFFF_0000) | cmd_new as u32;
    let addr: u32 = 0x8000_0000
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | 0x04;
    let mut a: PortWriteOnly<u32> = PortWriteOnly::new(CONFIG_ADDR);
    let mut d: Port<u32> = Port::new(CONFIG_DATA);
    unsafe {
        a.write(addr);
        d.write(merged);
    }
    cmd_old
}

/// Make sure Memory Space (bit 1) and Bus Master (bit 2) are enabled on the
/// device's PCI command register; these are typically set by the BIOS but
/// some configurations leave them off. Returns the previous command value.
/// Required before any MMIO access (xHCI etc.) and before DMA later.
pub fn enable_mmio_and_bus_master(d: &PciDevice) -> u16 {
    modify_command(d.bus, d.slot, d.func, 0x0006, 0x0000)
}
