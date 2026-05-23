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
use tablestore::{BlockDevice, Store};

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

    // One device: the disk we booted from (primary IDE master). The TablesOS
    // volume begins at `data_lba`; the ATA driver hides that base offset so
    // the engine still sees a volume starting at sector 0 and can never touch
    // the boot/kernel region in front of it.
    let mut disk = match ata::Ata::probe(info.data_lba) {
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
    let store: Store<ata::Ata> = match Store::open(disk) {
        Ok(s) => {
            serial_println!("mounted store");
            s
        }
        Err(e) => {
            serial_println!("mount failed: {:?} (no runtime formatting)", e);
            fatal("store volume invalid");
        }
    };

    serial_println!("starting UI");
    ui::run(store, info.sys_guid, info.data_lba)
}

/// Report a fatal boot condition on screen + serial and stop. Never returns;
/// crucially, it performs no disk writes.
fn fatal(msg: &str) -> ! {
    serial_println!("FATAL: {msg}");
    framebuffer::with(|d| {
        d.backdrop(framebuffer::C_BG_TOP, framebuffer::C_BG_ROW);
        d.starfield(120);
        d.draw_text_glow(40, 40, "TABLESOS — CANNOT START", framebuffer::C_ERR, framebuffer::C_GLOW, font::Font::Display);
        d.draw_text(40, 40 + 28, msg, framebuffer::C_FG, font::Font::Body);
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
