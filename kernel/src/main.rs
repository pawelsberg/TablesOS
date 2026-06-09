//! TablesOS kernel entry point.
//!
//! Boot path: our own custom BIOS bootloader (no partitions, no FAT — see
//! SPECIFICATION.md item 8 and boot/layout.md) loads this flat kernel at
//! 0x200000, sets a VESA mode, enters long mode and jumps to `_start` with a
//! pointer to [`BootInfo`] in RDI. `_start` sets up the stack, zeroes BSS,
//! and calls [`kmain`], which brings up the system and runs the GUI.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![allow(clippy::too_many_arguments)]

extern crate alloc;

mod allocator;
mod assets;
mod ata;
mod font;
mod framebuffer;
mod gdt;
mod install;
mod interrupts;
mod pci;
mod ps2;
mod rtc;
mod serial;
mod time;
mod ui;
mod usb;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};
use tablestore::{BlockDevice, Result as TsResult, Store, StoreError};

/// Handed over by the bootloader at a fixed address; layout mirrors
/// `boot/layout.md`. `#[repr(C)]` so the field offsets match the assembler.
#[repr(C)]
pub struct BootInfo {
    pub magic: u32,       // 0x00  'OTBS' = 0x5342544F
    pub fb_width: u32,    // 0x04
    pub fb_height: u32,   // 0x08
    pub fb_pitch: u32,    // 0x0C  bytes per scanline
    pub fb_addr: u64,     // 0x10  physical (identity-mapped)
    pub fb_bpp: u8,       // 0x18  bytes per pixel
    pub fb_fmt: u8,       // 0x19  0 = RGB, 1 = BGR
    _r: [u8; 6],          // 0x1A
    pub data_lba: u64,    // 0x20  LBA where the TablesOS volume starts
    pub boot_drive: u8,   // 0x28
    _r2: [u8; 7],         // 0x29
    /// The unique system GUID copied out of the MBR (offset 0x1DC) at boot.
    /// The kernel re-reads sector 0 and refuses to run unless this still
    /// matches — so it can never read, write or format any disk other than
    /// the exact one it was booted from.
    pub sys_guid: [u8; 16], // 0x30
}

/// Offset of the 16-byte system GUID inside the custom MBR.
const SYS_GUID_OFF: usize = 0x1DC;

const BOOT_MAGIC: u32 = 0x5342_544F;

// The very first bytes of the image (`.text._start`, forced first by the
// linker script) so the entry point == load address 0x200000.
core::arch::global_asm!(
    r#"
.section .text._start,"ax"
.global _start
_start:
    cli
    cld
    lea     rsp, [rip + __boot_stack_top]
    mov     r12, rdi                 # save BootInfo pointer
    lea     rdi, [rip + __bss_start]
    lea     rcx, [rip + __bss_end]
    sub     rcx, rdi
    xor     eax, eax
    rep     stosb                    # zero .bss (heap + stack region)
    mov     rdi, r12
    call    kmain
1:  hlt
    jmp     1b
"#
);

#[no_mangle]
extern "C" fn kmain(info: *const BootInfo) -> ! {
    let info: &BootInfo = unsafe { &*info };

    serial::init();
    serial_println!("\nTablesOS booting (custom MBR, single device)");

    allocator::init();
    gdt::init();
    interrupts::init();
    time::init();
    serial_println!(
        "TSC calibrated: {} ticks/µs (≈ {} MHz)",
        time::tsc_per_us(),
        time::tsc_per_us()
    );

    if info.magic != BOOT_MAGIC {
        serial_println!("FATAL: bad BootInfo magic {:#x}", info.magic);
        halt();
    }
    serial_println!(
        "framebuffer {}x{} pitch={} {}bpp {}",
        info.fb_width,
        info.fb_height,
        info.fb_pitch,
        info.fb_bpp,
        if info.fb_fmt == 1 { "BGR" } else { "RGB" }
    );

    let fb = framebuffer::FbInfo {
        width: info.fb_width as usize,
        height: info.fb_height as usize,
        pitch: info.fb_pitch as usize,
        bpp: info.fb_bpp as usize,
        bgr: info.fb_fmt == 1,
    };
    if fb.width < 1024 || fb.height < 720 || (fb.bpp != 3 && fb.bpp != 4) || info.fb_addr == 0 {
        serial_println!("FATAL: no acceptable graphics mode");
        halt();
    }
    // The framebuffer is identity-mapped by the bootloader's page tables.
    let len = fb.pitch * fb.height;
    let buffer =
        unsafe { core::slice::from_raw_parts_mut(info.fb_addr as *mut u8, len) };
    framebuffer::init(buffer, fb);
    // Decode and install any embedded bitmaps (backgrounds + font atlas). Safe
    // no-op when no assets are bundled; the UI then uses the procedural look.
    assets::install();
    ps2::set_bounds(fb.width, fb.height);
    ps2::init_mouse();

    // The disk we booted from — ATA (QEMU IDE) or USB mass storage (a real
    // pendrive). The TablesOS volume begins at `data_lba`; `BootDisk` hides that
    // base offset so the engine sees a volume starting at sector 0 and can never
    // touch the boot/kernel region in front of it.
    let mut disk = match discover_boot_disk(info.data_lba, &info.sys_guid) {
        Some(d) => d,
        None => fatal("no boot disk"),
    };

    // Identity gate. Re-read sector 0 (the MBR, absolute LBA 0) and require
    // its system GUID to still equal the one captured in memory at boot. If
    // it differs this is *not* the disk we booted from, so we touch nothing:
    // no volume read, no write, and there is no formatting path at all.
    let mut mbr = [0u8; 512];
    if disk.read_boot_sector(&mut mbr).is_err() {
        fatal("cannot read MBR");
    }
    if mbr[SYS_GUID_OFF..SYS_GUID_OFF + 16] != info.sys_guid[..] {
        serial_println!("system GUID mismatch — refusing to use this disk");
        fatal("disk identity mismatch (not the booted device)");
    }
    serial_println!(
        "system GUID verified; volume base LBA {}, {} sectors",
        info.data_lba,
        disk.sector_count()
    );

    // The image always ships an initialised volume; the OS only ever mounts.
    let store: Store<BootDisk> = match Store::open(disk) {
        Ok(s) => {
            serial_println!("mounted store");
            s
        }
        Err(e) => {
            serial_println!("mount failed: {:?} (no runtime formatting)", e);
            fatal("store volume invalid");
        }
    };

    // Bind a USB-HID boot mouse if one is present. On real hardware the pointer
    // is USB and the BIOS's PS/2 emulation for it was switched off by our xHCI
    // ownership handoff (taken to reach the USB boot disk), so without this the
    // mouse is dead. The PS/2 path still serves QEMU and any genuine PS/2 mouse.
    // See solved-issues/USB mouse on real hardware.md.
    // (DIAG: the on-screen line is a temporary boot marker.)
    if usb::xhci::setup_mouse() {
        serial_println!("USB-HID boot mouse ready");
        boot_status("input: USB mouse ready");
    } else {
        serial_println!("no USB-HID mouse found (PS/2 only)");
    }

    serial_println!("starting UI");
    ui::run(store, info.sys_guid, info.data_lba)
}

/// DIAG: on-screen boot trace. The laptop has no serial console, so this draws
/// each milestone as a new line on the framebuffer (and mirrors to serial for
/// QEMU). The last line left visible when boot stalls pinpoints the hang.
/// Temporary — remove with the rest of the diagnostics once USB boot is solid.
static BOOT_Y: AtomicUsize = AtomicUsize::new(30);
pub fn boot_status(msg: &str) {
    serial_println!("{}", msg);
    let y = BOOT_Y.fetch_add(18, Ordering::Relaxed);
    framebuffer::with(|d| {
        d.draw_text(40, y, msg, framebuffer::C_FG, font::Font::Body);
        d.blit();
    });
}

/// The disk we booted from, reached either over legacy ATA PIO (QEMU's IDE
/// master) or over USB mass storage (a real pendrive, via the xHCI driver).
/// Either way the TablesOS volume begins at `data_lba`; this hides that base
/// offset so the engine sees a volume starting at sector 0 and can never touch
/// the boot/kernel region in front of it. `read_boot_sector` bypasses the
/// offset to reach the absolute MBR for the system-GUID identity gate.
enum BootDisk {
    Ata(ata::Ata),
    Usb {
        dev: usb::xhci::UsbMscDevice,
        base: u64,
        sectors: u64,
    },
}

impl BootDisk {
    /// Read absolute LBA 0 (the MBR), ignoring the volume base offset.
    fn read_boot_sector(&mut self, buf: &mut [u8]) -> TsResult<()> {
        match self {
            BootDisk::Ata(a) => a.read_boot_sector(buf),
            BootDisk::Usb { dev, .. } => dev.read_sector(0, buf),
        }
    }
}

impl BlockDevice for BootDisk {
    fn sector_count(&self) -> u64 {
        match self {
            BootDisk::Ata(a) => a.sector_count(),
            BootDisk::Usb { sectors, .. } => *sectors,
        }
    }
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> TsResult<()> {
        match self {
            BootDisk::Ata(a) => a.read_sector(lba, buf),
            BootDisk::Usb { dev, base, sectors } => {
                if lba >= *sectors {
                    return Err(StoreError::Io);
                }
                dev.read_sector(*base + lba, buf)
            }
        }
    }
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> TsResult<()> {
        match self {
            BootDisk::Ata(a) => a.write_sector(lba, buf),
            BootDisk::Usb { dev, base, sectors } => {
                if lba >= *sectors {
                    return Err(StoreError::Io);
                }
                dev.write_sector(*base + lba, buf)
            }
        }
    }
    fn flush(&mut self) -> TsResult<()> {
        match self {
            BootDisk::Ata(a) => a.flush(),
            BootDisk::Usb { dev, .. } => dev.flush(),
        }
    }
}

/// Find the disk we booted from. Tries legacy ATA first (how QEMU's IDE image
/// is reached), then USB mass storage (a real pendrive). In both cases the disk
/// is only accepted when its MBR system GUID matches the one captured at boot,
/// so the kernel can never latch onto the wrong device. The caller still runs
/// the identity gate afterwards (belt and suspenders).
fn discover_boot_disk(data_lba: u64, sys_guid: &[u8; 16]) -> Option<BootDisk> {
    // Legacy ATA PIO (primary master). On real UEFI/NVMe hardware there is no
    // such device and `probe` returns None; on QEMU it is the boot image.
    boot_status("disk: probing ATA");
    if let Some(mut a) = ata::Ata::probe(data_lba) {
        let mut mbr = [0u8; 512];
        if a.read_boot_sector(&mut mbr).is_ok()
            && mbr[SYS_GUID_OFF..SYS_GUID_OFF + 16] == sys_guid[..]
        {
            boot_status("disk: ATA primary master (GUID ok)");
            return Some(BootDisk::Ata(a));
        }
        boot_status("disk: ATA present, wrong GUID -> trying USB");
    } else {
        boot_status("disk: no ATA -> trying USB");
    }

    // USB mass storage (real pendrive). Drive the xHCI pipeline explicitly with
    // an on-screen marker before each step, so a stall on real silicon is
    // visible. Each step is idempotent and swallows its own error; only the
    // drive whose MBR GUID matches the booted system GUID is ever selected.
    boot_status("xHCI: enumerating PCI");
    let devices = pci::enumerate();
    let mut saw_xhci = false;
    for dev in devices
        .iter()
        .filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.prog_if == 0x30)
    {
        saw_xhci = true;
        let info = match usb::xhci::inspect(dev) {
            Some(i) => i,
            None => continue,
        };
        boot_status("xHCI: bring-up");
        let _ = usb::xhci::bring_up(dev, &info);
        boot_status("xHCI: reset + enable slots");
        let _ = usb::xhci::reset_and_enable_slots(dev, &info);
        boot_status("xHCI: address devices");
        let _ = usb::xhci::address_enabled_slots(dev, &info);
        boot_status("xHCI: fetch configurations");
        let _ = usb::xhci::fetch_configurations(dev, &info);
        boot_status("xHCI: configure endpoints");
        let _ = usb::xhci::configure_endpoints(dev, &info);
        boot_status("xHCI: probe mass storage");
        let _ = usb::xhci::probe_mass_storage(dev, &info);
    }
    if !saw_xhci {
        boot_status("xHCI: no controller found in PCI");
    }

    // Report what enumeration produced before selecting, so a "no boot disk"
    // failure shows whether devices were addressed, probed as mass storage, and
    // how each drive's MBR signature / GUID compared.
    let n_addr = usb::xhci::current_addressed().len();
    let n_msc = usb::xhci::current_msc().len();
    boot_status(&alloc::format!("USB: addressed={} msc={}", n_addr, n_msc));
    // DIAG: dump each addressed device so we can see whether the boot stick is
    // present and why it is not recognized as mass storage (class 08/06/50).
    for ad in usb::xhci::current_addressed() {
        let (vid, pid, mps0) = match &ad.descriptor {
            Some(d) => (d.id_vendor, d.id_product, d.max_packet_size_ep0),
            None => (0, 0, 0),
        };
        let ev = ad.eval_context_cc.map(|c| c as i32).unwrap_or(-1);
        // spd: 1=full 2=low 3=high 4=super 5=super+. dcc=device-desc cc (worked),
        // cfgcc=config-desc cc (the failing one). mps0=bMaxPacketSize0.
        boot_status(&alloc::format!(
            "dev s{} p{} spd={} {:04x}:{:04x} mps0={} dcc={} ev={} cfgcc={} setcfg={}",
            ad.slot_id, ad.port, ad.speed, vid, pid, mps0,
            ad.descriptor_completion_code, ev,
            ad.config_completion_code, ad.set_config_cc
        ));
        match &ad.config {
            Some(cfg) => {
                for ifd in &cfg.interfaces {
                    boot_status(&alloc::format!(
                        "  if{} {:02x}/{:02x}/{:02x} eps={}",
                        ifd.number, ifd.class, ifd.subclass, ifd.protocol,
                        ifd.endpoints.len()
                    ));
                }
            }
            None => boot_status("  (no config descriptor)"),
        }
    }
    let drives = usb::xhci::usb_drives_with_slots(sys_guid);
    boot_status(&alloc::format!("USB: drives={}", drives.len()));
    for (slot, di) in &drives {
        boot_status(&alloc::format!(
            "USB: slot {} sig={} booted={}",
            slot, di.boot_sig_ok, di.booted
        ));
        if !di.booted {
            continue;
        }
        if let Ok(dev) = usb::xhci::UsbMscDevice::open_with_identity_gate(*slot, *sys_guid) {
            let total = dev.sector_count();
            if total <= data_lba {
                continue;
            }
            boot_status("USB: boot drive opened");
            return Some(BootDisk::Usb {
                dev,
                base: data_lba,
                sectors: total - data_lba,
            });
        }
    }
    boot_status("USB: no matching boot drive");
    None
}

/// Report a fatal boot condition on screen + serial and stop. Never returns;
/// crucially, it performs no disk writes.
fn fatal(msg: &str) -> ! {
    serial_println!("FATAL: {msg}");
    // DIAG: draw below the boot trace instead of clearing the screen, so the
    // markers that explain the failure stay visible. (Restore the full-screen
    // error backdrop during cleanup.)
    let y = BOOT_Y.fetch_add(52, Ordering::Relaxed);
    framebuffer::with(|d| {
        d.draw_text_glow(40, y + 10, "TABLESOS — CANNOT START", framebuffer::C_ERR, framebuffer::C_GLOW, font::Font::Display);
        d.draw_text(40, y + 38, msg, framebuffer::C_FG, font::Font::Body);
        d.blit();
    });
    halt();
}

fn halt() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    serial_println!("\n*** KERNEL PANIC ***\n{}", info);
    unsafe {
        framebuffer::emergency_fill(0x60, 0x00, 0x00);
    }
    halt();
}
