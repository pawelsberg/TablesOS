//! TablesOS kernel entry point.
//!
//! Boot paths (both our own — see boot/layout.md):
//! - **BIOS**: stage 1 (custom MBR) + stage 2 load this flat kernel at
//!   0x1000000, set a VESA mode, enter long mode and jump to `_start` with a
//!   pointer to [`BootInfo`] in RDI.
//! - **UEFI**: `uefi-loader` (BOOTX64.EFI on the image's FAT16 ESP) does the
//!   equivalent — GOP mode, identity paging, ExitBootServices — and jumps to
//!   the same `_start` with the same `BootInfo` contract.
//!
//! `_start` sets up the stack, zeroes BSS, and calls [`kmain`], which brings
//! up the system and runs the GUI.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![allow(clippy::too_many_arguments)]

extern crate alloc;

mod acpi;
mod allocator;
mod assets;
mod ata;
mod font;
mod framebuffer;
mod gdt;
mod install;
mod interrupts;
mod keymap;
mod pci;
mod ps2;
mod rtc;
mod serial;
mod time;
mod ui;
mod upgrade;
mod usb;
mod vmem;

use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;
use spin::Mutex;
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
    /// The unique system GUID copied out of the MBR (offset 0x1AC) at boot.
    /// The kernel re-reads sector 0 and refuses to run unless this still
    /// matches — so it can never read, write or format any disk other than
    /// the exact one it was booted from.
    pub sys_guid: [u8; 16], // 0x30
    /// Physical address of the ACPI RSDP, or 0 when the bootloader doesn't
    /// know it. The UEFI loader fills this from the EFI configuration table
    /// (UEFI firmware need not place the RSDP in the legacy BIOS scan areas);
    /// the BIOS stage 2 leaves it 0 and the kernel scans EBDA/0xE0000.
    pub rsdp_addr: u64, // 0x40
    /// Physical base of the heap region the bootloader reserved for the kernel:
    /// a free, identity-mapped, below-4-GiB block clear of the kernel footprint.
    /// 0 when the bootloader didn't provide one (the kernel then uses a small
    /// built-in fallback heap). See `allocator::init`.
    pub heap_base: u64, // 0x48
    /// Bytes available at `heap_base`, or 0.
    pub heap_size: u64, // 0x50
}

/// Offset of the 16-byte system GUID inside the custom MBR.
const SYS_GUID_OFF: usize = 0x1AC;

const BOOT_MAGIC: u32 = 0x5342_544F;

// The very first bytes of the image (`.text._start`, forced first by the
// linker script) so the entry point == load address 0x1000000.
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

    let (heap_base, heap_size) = allocator::init(info.heap_base, info.heap_size);
    if heap_base == info.heap_base && info.heap_size != 0 {
        serial_println!(
            "heap: {} MiB at {:#x} (bootloader-placed)",
            heap_size >> 20,
            heap_base
        );
    } else {
        serial_println!(
            "heap: {} MiB fallback (bootloader heap {:#x}/{} unusable)",
            heap_size >> 20,
            info.heap_base,
            info.heap_size
        );
    }
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
    // Under UEFI the RSDP rarely sits in the legacy BIOS areas; the loader
    // hands us its address so ACPI shutdown works there too.
    if info.rsdp_addr != 0 {
        acpi::set_rsdp_hint(info.rsdp_addr);
        serial_println!("ACPI RSDP hint from bootloader: {:#x}", info.rsdp_addr);
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
    // The framebuffer is identity-mapped by the bootloader's page tables but
    // left UC (uncacheable) under UEFI, so a full-screen present crawls (~80 ms
    // at high res). Retag it write-combining — minimal version: just PAT slot 1
    // + page retag + TLB flush, no CR0.CD/wbinvd/CR3-helper (the dance that hung
    // the DELL before). UC has no cached lines to flush, so this is safe; if it
    // ever hangs on other firmware, the coloured stripe left on screen localises
    // it. See `vmem`.
    let len = fb.pitch * fb.height;
    vmem::enable_framebuffer_wc(info.fb_addr, len, fb.pitch, fb.height);
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
        // Surface the driver's own reason (no serial console on real HW), e.g.
        // the EHCI/xHCI low-level error that made the read fail.
        if let Some(detail) = usb::xhci::take_last_msc_err() {
            fatal(&alloc::format!("cannot read MBR: {}", detail));
        }
        if let Some(detail) = usb::ehci::take_last_err() {
            fatal(&alloc::format!("cannot read MBR: {}", detail));
        }
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

    // Bind USB-HID boot mouse + keyboard if present. On real hardware input is
    // USB and the BIOS's PS/2 emulation for it was switched off by our xHCI
    // ownership handoff (taken to reach the USB boot disk), so without this the
    // mouse and keyboard are dead. The PS/2 path still serves QEMU and any
    // genuine PS/2 device. See solved-issues/USB mouse on real hardware.md.
    // (DIAG: the on-screen line is a temporary boot marker.)
    if usb::xhci::setup_hid() {
        serial_println!("USB-HID input ready");
        boot_status("input: USB HID ready");
    } else {
        serial_println!("no USB-HID input found (PS/2 only)");
    }

    // Hold briefly on the boot log so it can be reviewed (press any key to page
    // through it, Enter to continue); auto-continues if left untouched.
    boot_pager_finish();

    serial_println!("starting UI");
    ui::run(store, info.sys_guid, info.data_lba)
}

/// DIAG: on-screen boot trace, as a pager. The laptop has no serial console, so
/// every milestone is also drawn on the framebuffer. Lines fill the screen
/// top-to-bottom; once a page is full the screen clears and the next lines start
/// again from the top. Every line is kept, so the whole trace can be reviewed.
///
/// When boot finishes, [`boot_pager_finish`] holds the last page for a few
/// seconds ("press any key to review"); pressing any key there engages
/// navigation — PageUp/PageDown/Home/End step through the captured pages and
/// Enter continues into the UI. If no key is pressed the boot continues on its
/// own. On real hardware the keyboard is USB-HID, which isn't live until the end
/// of boot, so that end-of-boot window (which pumps USB-HID, not just PS/2) is
/// the portable place to pause. A PS/2 keyboard (QEMU) can additionally lock the
/// trace early, mid-boot, via [`boot_status`].
/// Temporary — remove with the rest of the diagnostics once USB boot is solid.
struct BootPager {
    /// Every boot line ever emitted, so navigation can reach any page.
    lines: Vec<String>,
    /// A key was pressed: auto-scroll is locked and navigation is active.
    paused: bool,
    /// Page currently shown while navigating.
    view: usize,
}

static BOOT: Mutex<BootPager> = Mutex::new(BootPager {
    lines: Vec::new(),
    paused: false,
    view: 0,
});

/// Text rows on a boot page. The bottom row is reserved for the hint line.
fn boot_rows() -> usize {
    framebuffer::with(|d| d.rows().saturating_sub(1))
        .unwrap_or(40)
        .max(1)
}

/// Total captured pages for `lines` at `per` rows each (at least one).
fn boot_pages(lines: &[String], per: usize) -> usize {
    if lines.is_empty() {
        1
    } else {
        (lines.len() - 1) / per + 1
    }
}

/// What to stamp on the reserved bottom row of a boot page.
#[derive(Clone, Copy)]
enum Hint {
    /// Navigation engaged: `page X/Y · PgUp/PgDn · Enter`.
    Nav,
    /// End-of-boot review window counting down `secs` to auto-continue.
    Countdown(u64),
    /// Fatal browse: the boot failed; page through the log to review it.
    Fatal,
}

/// Repaint one whole page of the boot log, clearing the screen first, and stamp
/// `hint` on the reserved bottom row.
fn draw_boot_page(lines: &[String], page: usize, per: usize, hint: Hint) {
    framebuffer::with(|d| {
        let (w, h) = (d.width(), d.height());
        d.fill_rect(0, 0, w, h, framebuffer::C_BG_TOP);
        let start = page * per;
        for i in 0..per {
            let li = start + i;
            if li >= lines.len() {
                break;
            }
            d.draw_text(8, i * framebuffer::CELL_H, &lines[li], framebuffer::C_FG, font::Font::Body);
        }
        let text = match hint {
            Hint::Nav => Some(alloc::format!(
                "[boot log] page {}/{}  PgUp/PgDn: navigate  Enter: continue",
                page + 1,
                boot_pages(lines, per)
            )),
            Hint::Countdown(secs) => Some(alloc::format!(
                "boot complete — press any key to review the log   (continuing in {secs}s)"
            )),
            Hint::Fatal => Some(alloc::format!(
                "CANNOT START — page {}/{}  PgUp/PgDn/Home/End: review boot log",
                page + 1,
                boot_pages(lines, per)
            )),
        };
        if let Some(t) = text {
            // The fatal hint reads in the error colour so it's unmistakable.
            let c = if matches!(hint, Hint::Fatal) {
                framebuffer::C_ERR
            } else {
                framebuffer::C_ACCENT
            };
            d.draw_text(8, per * framebuffer::CELL_H, &t, c, font::Font::Body);
        }
        d.blit();
    });
}

pub fn boot_status(msg: &str) {
    serial_println!("{}", msg);
    let per = boot_rows();
    let mut bp = BOOT.lock();
    bp.lines.push(String::from(msg));
    let idx = bp.lines.len() - 1;

    if !bp.paused {
        // Live rolling fill. Any keypress locks the visible page and switches
        // into navigation for the rest of boot.
        let mut pressed = false;
        while let Some(e) = ps2::poll() {
            if let ps2::Event::Key(_) = e {
                pressed = true;
            }
        }
        if pressed {
            bp.paused = true;
            bp.view = idx / per;
            let view = bp.view;
            draw_boot_page(&bp.lines, view, per, Hint::Nav);
            return;
        }
        let row = idx % per;
        framebuffer::with(|d| {
            if row == 0 {
                // New page: clear and re-stamp the live hint on the bottom row.
                let (w, h) = (d.width(), d.height());
                d.fill_rect(0, 0, w, h, framebuffer::C_BG_TOP);
                d.draw_text(
                    8,
                    per * framebuffer::CELL_H,
                    "[boot] press any key to pause / navigate",
                    framebuffer::C_DIM,
                    font::Font::Body,
                );
            }
            d.draw_text(8, row * framebuffer::CELL_H, &bp.lines[idx], framebuffer::C_FG, font::Font::Body);
            d.blit();
        });
    } else {
        // Locked: keep buffering, but only redraw in response to navigation.
        let max_page = idx / per;
        let mut redraw = false;
        while let Some(e) = ps2::poll() {
            if let ps2::Event::Key(k) = e {
                match k {
                    ps2::Key::PageUp => bp.view = bp.view.saturating_sub(1),
                    ps2::Key::PageDown => bp.view = (bp.view + 1).min(max_page),
                    ps2::Key::Home => bp.view = 0,
                    ps2::Key::End => bp.view = max_page,
                    _ => continue,
                }
                redraw = true;
            }
        }
        if redraw {
            let view = bp.view;
            draw_boot_page(&bp.lines, view, per, Hint::Nav);
        }
    }
}

/// End-of-boot review window. Holds the last log page for [`REVIEW_MS`] showing
/// a countdown; if any key is pressed it engages navigation (PageUp/PageDown,
/// Home/End) and waits for Enter before continuing into the UI, otherwise it
/// auto-continues when the countdown elapses. This pumps both PS/2 and USB-HID,
/// so it works on real hardware (where the keyboard is USB and only becomes live
/// at the end of boot) as well as in QEMU. A PS/2 key pressed mid-boot already
/// set `paused`, in which case navigation is engaged immediately with no
/// countdown.
fn boot_pager_finish() {
    /// How long the post-boot review window waits for a keypress before
    /// continuing into the UI on its own.
    const REVIEW_MS: u64 = 4000;

    let per = boot_rows();
    let mut engaged = BOOT.lock().paused;
    let mut elapsed = 0u64;
    let mut last_secs = u64::MAX;

    // Draw the initial page (the last one filled, unless an early lock moved the
    // view): navigation hint if already engaged, otherwise the countdown.
    {
        let bp = BOOT.lock();
        let view = if engaged { bp.view } else { boot_pages(&bp.lines, per) - 1 };
        let hint = if engaged { Hint::Nav } else { Hint::Countdown(REVIEW_MS / 1000) };
        draw_boot_page(&bp.lines, view, per, hint);
    }

    loop {
        usb::xhci::pump_hid();
        usb::ehci::pump_hid();

        let mut redraw = false;
        while let Some(e) = ps2::poll() {
            let ps2::Event::Key(k) = e else { continue };
            if !engaged {
                // First key ends the countdown and engages navigation, locked on
                // the last page.
                engaged = true;
                let mut bp = BOOT.lock();
                bp.paused = true;
                bp.view = boot_pages(&bp.lines, per) - 1;
                redraw = true;
                continue;
            }
            let mut bp = BOOT.lock();
            let max_page = boot_pages(&bp.lines, per) - 1;
            match k {
                ps2::Key::PageUp => bp.view = bp.view.saturating_sub(1),
                ps2::Key::PageDown => bp.view = (bp.view + 1).min(max_page),
                ps2::Key::Home => bp.view = 0,
                ps2::Key::End => bp.view = max_page,
                ps2::Key::Enter => return,
                _ => continue,
            }
            redraw = true;
        }

        if engaged {
            if redraw {
                let bp = BOOT.lock();
                let view = bp.view;
                draw_boot_page(&bp.lines, view, per, Hint::Nav);
            }
        } else {
            // Counting down: repaint once per second, auto-continue at zero.
            let remaining = REVIEW_MS.saturating_sub(elapsed);
            let secs = remaining.div_ceil(1000);
            if secs != last_secs {
                last_secs = secs;
                let bp = BOOT.lock();
                let view = boot_pages(&bp.lines, per) - 1;
                draw_boot_page(&bp.lines, view, per, Hint::Countdown(secs));
            }
            if remaining == 0 {
                return;
            }
        }

        // ~125 Hz poll so a USB key (no IRQ) is caught promptly and the
        // countdown stays smooth. PS/2 IRQs enqueue in the meantime regardless.
        time::delay_ms(8);
        elapsed += 8;
    }
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
    /// Pre-xHCI machines: the pendrive sits on the chipset EHCI controller.
    Ehci {
        dev: usb::ehci::EhciMscDevice,
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
            BootDisk::Ehci { dev, .. } => dev.read_sector(0, buf),
        }
    }

    /// One unverified sector write (the actual device write). `write_sector`
    /// wraps this with read-after-write verification.
    fn write_sector_raw(&mut self, lba: u64, buf: &[u8]) -> TsResult<()> {
        match self {
            BootDisk::Ata(a) => a.write_sector(lba, buf),
            BootDisk::Usb { dev, base, sectors } => {
                if lba >= *sectors {
                    return Err(StoreError::Io);
                }
                dev.write_sector(*base + lba, buf)
            }
            BootDisk::Ehci { dev, base, sectors } => {
                if lba >= *sectors {
                    return Err(StoreError::Io);
                }
                dev.write_sector(*base + lba, buf)
            }
        }
    }
}

impl BlockDevice for BootDisk {
    fn sector_count(&self) -> u64 {
        match self {
            BootDisk::Ata(a) => a.sector_count(),
            BootDisk::Usb { sectors, .. } => *sectors,
            BootDisk::Ehci { sectors, .. } => *sectors,
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
            BootDisk::Ehci { dev, base, sectors } => {
                if lba >= *sectors {
                    return Err(StoreError::Io);
                }
                dev.read_sector(*base + lba, buf)
            }
        }
    }
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> TsResult<()> {
        // USB: one multi-sector transfer (fewer round-trips + a workaround for
        // controllers that mishandle long runs of single-sector bulk reads).
        // ATA/EHCI keep the per-sector default loop (adds `base` via
        // `read_sector`); those are the QEMU and pre-xHCI paths.
        let count = (buf.len() / 512) as u64;
        match self {
            BootDisk::Usb { dev, base, sectors } => {
                if buf.len() % 512 != 0 || lba + count > *sectors {
                    return Err(StoreError::Io);
                }
                dev.read_blocks(*base + lba, buf)
            }
            _ => {
                for (i, chunk) in buf.chunks_mut(512).enumerate() {
                    self.read_sector(lba + i as u64, chunk)?;
                }
                Ok(())
            }
        }
    }
    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> TsResult<()> {
        // Read-after-write verify. Cheap USB sticks (and controllers brought up
        // through a forced BIOS handoff) can ACK a write yet leave wrong bytes on
        // the medium — silent corruption that later surfaces as "corrupt store:
        // chain length / varint eof". Read the sector back and compare; retry on
        // mismatch, and fail loudly (I/O error) rather than corrupt if it never
        // matches. Reads are trustworthy (the volume mounts), so a mismatch means
        // the *write* didn't take. ATA (QEMU) is reliable and passes first try.
        if buf.len() != 512 {
            return Err(StoreError::Io);
        }
        let mut check = [0u8; 512];
        // Skip-if-unchanged (flash-wear reduction): reading does not wear NAND,
        // so if the medium already holds exactly these bytes there is nothing to
        // gain by erasing/programming them again. The hot journal/superblock
        // sectors and re-saved-but-unchanged data pages are the common case.
        // Trust a successful, matching read; on any read error fall through and
        // write (reads being unreliable is not a reason to skip a write).
        if self.read_sector(lba, &mut check).is_ok() && check[..] == buf[..] {
            return Ok(());
        }
        for _ in 0..4 {
            self.write_sector_raw(lba, buf)?;
            self.read_sector(lba, &mut check)?;
            if check[..] == buf[..] {
                return Ok(());
            }
        }
        Err(StoreError::Io)
    }
    fn flush(&mut self) -> TsResult<()> {
        match self {
            BootDisk::Ata(a) => a.flush(),
            BootDisk::Usb { dev, .. } => dev.flush(),
            BootDisk::Ehci { dev, .. } => dev.flush(),
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
    // Real machines have several xHCI controllers (PCH + Thunderbolt …).
    // Inspect them all, then visit the ones that already show a connected
    // device first — that's where the boot stick is, and a deviceless
    // controller costs a full connect-wait timeout.
    let mut xhci_devs: alloc::vec::Vec<(pci::PciDevice, usb::xhci::XhciInfo)> = devices
        .iter()
        .filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.prog_if == 0x30)
        .filter_map(|d| usb::xhci::inspect(d).map(|i| (*d, i)))
        .collect();
    xhci_devs.sort_by_key(|(_, i)| !i.ports.iter().any(|p| p.ccs));
    let saw_xhci = !xhci_devs.is_empty();
    // Surface USB host controllers we have no driver for. On pre-xHCI-PCH
    // machines the USB2 ports are wired to EHCI permanently — a stick in one
    // of those is invisible to the xHCI no matter what; this line plus an
    // all-empty SC dump is the signature of that situation (try a USB3 port).
    for d in devices
        .iter()
        .filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.prog_if != 0x30)
    {
        boot_status(&alloc::format!(
            "USB: {} {:04x}:{:04x} present (no driver)",
            pci::class_name(d.class, d.subclass, d.prog_if),
            d.vendor,
            d.device
        ));
    }
    for (dev, info) in &xhci_devs {
        let info = info.clone();
        if !info.mmio_accessible {
            boot_status(&alloc::format!(
                "xHCI: MMIO unreachable at {:#x} (BAR relocation failed?)",
                info.mmio_base
            ));
            continue;
        }
        // Each step swallows its own error internally; print it here so a
        // real-hardware failure names the exact step instead of surfacing
        // later as a bare "addressed=0".
        boot_status("xHCI: bring-up");
        if let Err(e) = usb::xhci::bring_up(dev, &info) {
            boot_status(&alloc::format!("xHCI: bring-up FAILED: {}", e));
            continue;
        }
        boot_status("xHCI: reset + enable slots");
        match usb::xhci::reset_and_enable_slots(dev, &info) {
            Ok(en) => {
                // Raw PORTSC per port — the forensic line for "no device on
                // any port" on real hardware (PP, PLS and CCS are visible).
                let mut line = alloc::string::String::from("SC:");
                for pr in &en.ports {
                    line = alloc::format!("{} {}={:08x}", line, pr.port, pr.portsc_after);
                    if line.len() > 96 {
                        boot_status(&line);
                        line = alloc::string::String::from("SC:");
                    }
                }
                if line.len() > 3 {
                    boot_status(&line);
                }
            }
            Err(e) => boot_status(&alloc::format!("xHCI: enable slots FAILED: {}", e)),
        }
        boot_status("xHCI: address devices");
        if let Err(e) = usb::xhci::address_enabled_slots(dev, &info) {
            boot_status(&alloc::format!("xHCI: address FAILED: {}", e));
        }
        boot_status("xHCI: fetch configurations");
        if let Err(e) = usb::xhci::fetch_configurations(dev, &info) {
            boot_status(&alloc::format!("xHCI: configs FAILED: {}", e));
        }
        boot_status("xHCI: configure endpoints");
        if let Err(e) = usb::xhci::configure_endpoints(dev, &info) {
            boot_status(&alloc::format!("xHCI: endpoints FAILED: {}", e));
        }
        boot_status("xHCI: probe mass storage");
        if let Err(e) = usb::xhci::probe_mass_storage(dev, &info) {
            boot_status(&alloc::format!("xHCI: MSC probe FAILED: {}", e));
        }
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
    for (mmio, slot, di) in &drives {
        boot_status(&alloc::format!(
            "USB: slot {} sig={} booted={}",
            slot, di.boot_sig_ok, di.booted
        ));
        if !di.booted {
            continue;
        }
        // Open by *controller + slot*: slot ids are only unique per xHCI, and
        // this machine has two controllers that both number a slot 1. Keying on
        // the slot id alone resolved to whichever controller was brought up
        // first — which differs between a cold boot and a reset, the cause of
        // the intermittent `slot has no bulk IN endpoint` on cold start.
        if let Ok(mut dev) =
            usb::xhci::UsbMscDevice::open_with_identity_gate(*mmio, *slot, *sys_guid)
        {
            let total = dev.sector_count();
            if total <= data_lba {
                continue;
            }
            // Cold-boot spin-up: the stick answers INQUIRY/READ CAPACITY (all
            // `open` needs) before its medium is actually readable, so the MBR
            // identity read below fails on a cold first boot but works after a
            // reset (the stick stays powered and ready). Wait for TEST UNIT
            // READY first so a cold boot behaves like a reset.
            if !dev.wait_until_ready() {
                boot_status("USB: drive slow to become ready -> proceeding");
            }
            // The ctrl/slot tag also doubles as a build marker: an old kernel
            // (pre controller-keyed I/O) prints a plain "USB: boot drive opened".
            boot_status(&alloc::format!(
                "USB: boot drive opened (ctrl {:#x} slot {})",
                mmio, slot
            ));
            return Some(BootDisk::Usb {
                dev,
                base: data_lba,
                sectors: total - data_lba,
            });
        }
    }
    boot_status("USB: no matching boot drive");

    // Pre-xHCI machines (or sticks in EHCI-wired ports): try the USB 2.0
    // controllers. Purely additive — machines that booted above never reach
    // this point.
    boot_status("EHCI: trying USB 2.0 controllers");
    if let Some((dev, total)) = usb::ehci::find_boot_drive(sys_guid) {
        if total > data_lba {
            boot_status("EHCI: boot drive opened");
            return Some(BootDisk::Ehci {
                dev,
                base: data_lba,
                sectors: total - data_lba,
            });
        }
    }
    None
}

/// Report a fatal boot condition on screen + serial, then let the boot log be
/// **browsed** (PgUp/PgDn/Home/End) instead of just halting, so the trace that
/// explains the failure can be reviewed — e.g. the USB enumeration dump behind a
/// "no boot disk". Never returns; crucially, it performs **no disk writes** (it
/// only reads input and repaints).
fn fatal(msg: &str) -> ! {
    serial_println!("FATAL: {msg}");
    let per = boot_rows();
    {
        let mut bp = BOOT.lock();
        bp.lines.push(alloc::format!("FATAL: {msg}"));
    }
    // A "no boot disk" (and the other disk failures) happen *before* kmain's
    // normal `setup_hid`, so on real hardware the USB keyboard isn't bound yet —
    // bring it up now (best effort) so the log is actually navigable. PS/2 works
    // regardless; if no keyboard binds, the last page just stays put (no worse
    // than the old halt, and the FATAL line is right there).
    usb::xhci::setup_hid();

    let mut view = {
        let bp = BOOT.lock();
        boot_pages(&bp.lines, per) - 1 // start on the last page (the FATAL line)
    };
    {
        let bp = BOOT.lock();
        draw_boot_page(&bp.lines, view, per, Hint::Fatal);
    }
    loop {
        usb::xhci::pump_hid();
        usb::ehci::pump_hid();
        let mut redraw = false;
        while let Some(e) = ps2::poll() {
            let ps2::Event::Key(k) = e else { continue };
            let max_page = {
                let bp = BOOT.lock();
                boot_pages(&bp.lines, per) - 1
            };
            match k {
                ps2::Key::PageUp => view = view.saturating_sub(1),
                ps2::Key::PageDown => view = (view + 1).min(max_page),
                ps2::Key::Home => view = 0,
                ps2::Key::End => view = max_page,
                _ => continue,
            }
            redraw = true;
        }
        if redraw {
            let bp = BOOT.lock();
            draw_boot_page(&bp.lines, view, per, Hint::Fatal);
        }
        // ~125 Hz poll so a USB key (no IRQ) is caught promptly.
        time::delay_ms(8);
    }
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
