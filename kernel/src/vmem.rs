//! Make the linear framebuffer **write-combining** — minimal, safe version.
//!
//! Under UEFI the firmware MTRRs leave the GOP framebuffer **UC** (uncacheable),
//! so [`framebuffer::Display::present`]'s full-screen copy crawls (~80 ms per
//! present at high resolution on real hardware — confirmed on a DELL via the
//! on-screen FB readout). A PAT type of WC overrides a UC MTRR (Intel SDM,
//! "Effective Memory Type"), turning the copy into write-combining bursts.
//!
//! A previous attempt also did `CR0.CD=1` + `wbinvd` + a `CR3` rewrite and hung
//! the DELL right after ExitBootServices. None of that is needed: UC memory is
//! never cached, so switching UC→WC has no stale cache lines to flush, and PAT
//! slot 1 is unused until we retag the framebuffer (so changing it can't alias
//! existing mappings). This version therefore only:
//!   1. writes IA32_PAT so slot 1 = WC (others keep reset defaults → RAM stays WB),
//!   2. retags the framebuffer's page-table entries to select slot 1,
//!   3. flushes the TLB with a plain CR3 reload (raw asm — no crate helper).
//!
//! `dbg_band` paints a coloured stripe straight to the (still-UC) framebuffer
//! before/after the risky step, so any hang on other firmware is localised by
//! whichever stripe stays on screen. On success the UI overwrites them at once.

use x86_64::registers::model_specific::Msr;

const PHYS_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const PTE_P: u64 = 1;
const PTE_PWT: u64 = 1 << 3; // PAT index bit 0
const PTE_PCD: u64 = 1 << 4; // PAT index bit 1
const PTE_PS: u64 = 1 << 7;
const PDE_PAT: u64 = 1 << 12; // PAT index bit 2 on a large page
const IA32_PAT: u32 = 0x277;

/// PAT slot 1 = WC; all other slots keep their power-on defaults so normal RAM
/// (slot 0) stays WB. slots: 0=WB 1=WC 2=UC- 3=UC 4=WB 5=WT 6=UC- 7=UC.
fn pat_value() -> u64 {
    0x06 | (0x01 << 8) | (0x07 << 16) | (0x00 << 24)
        | (0x06 << 32) | (0x04 << 40) | (0x07 << 48) | (0x00 << 56)
}

/// Paint `rows` scanlines of solid `byte` at vertical offset `row0` directly to
/// the framebuffer (diagnostic; bypasses `Display`).
fn dbg_band(fb_addr: u64, pitch: usize, height: usize, row0: usize, rows: usize, byte: u8) {
    if fb_addr == 0 || pitch == 0 {
        return;
    }
    let end = (row0 + rows).min(height);
    for y in row0..end {
        let line = (fb_addr as usize + y * pitch) as *mut u8;
        for x in 0..pitch {
            unsafe { core::ptr::write_volatile(line.add(x), byte) };
        }
    }
}

/// Retag `[base, base+len)` to PAT slot 1 (WC), then flush the TLB. Safe to call
/// once at boot before anything draws. `pitch`/`height` are only used for the
/// diagnostic stripes.
pub fn enable_framebuffer_wc(fb_addr: u64, len: usize, pitch: usize, height: usize) {
    if fb_addr == 0 || len == 0 {
        return;
    }
    // Stripe 1 (top): reached vmem at all.
    dbg_band(fb_addr, pitch, height, 0, 4, 0x20);
    unsafe {
        // 1. Install the PAT (slot 1 = WC). Nothing maps slot 1 yet, so this
        //    changes no live translation and needs no flush of its own.
        Msr::new(IA32_PAT).write(pat_value());
    }
    // Stripe 2: PAT written OK.
    dbg_band(fb_addr, pitch, height, 6, 4, 0x60);
    unsafe {
        // 2. Point the framebuffer's mappings at slot 1.
        retag_range(fb_addr, len);
        // 3. Flush the TLB (plain CR3 reload — read then write back unchanged).
        core::arch::asm!(
            "mov {t}, cr3",
            "mov cr3, {t}",
            t = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
    // Stripe 3: retag + flush done — WC is live.
    dbg_band(fb_addr, pitch, height, 12, 4, 0xA0);
}

/// Walk the active page tables over `[base, base+len)` and switch every leaf
/// that backs it to PAT slot 1 (set PWT, clear PCD and the large-page PAT bit).
/// Handles 2 MiB (both loaders), 1 GiB, and 4 KiB pages. Page-table pages are
/// identity-mapped below 4 GiB, so phys == virt here.
unsafe fn retag_range(base: u64, len: usize) {
    const SZ_1G: u64 = 1 << 30;
    const SZ_2M: u64 = 2 << 20;
    const SZ_4K: u64 = 4 << 10;

    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    let pml4 = (cr3 & PHYS_MASK) as *mut u64;
    let end = base + len as u64;
    let mut addr = base & !(SZ_4K - 1);
    while addr < end {
        let pml4e = entry(pml4, (addr >> 39) & 0x1FF);
        if *pml4e & PTE_P == 0 {
            addr += SZ_4K;
            continue;
        }
        let pdpt = (*pml4e & PHYS_MASK) as *mut u64;
        let pdpte = entry(pdpt, (addr >> 30) & 0x1FF);
        if *pdpte & PTE_P == 0 {
            addr += SZ_4K;
            continue;
        }
        if *pdpte & PTE_PS != 0 {
            set_wc_large(pdpte);
            addr = (addr & !(SZ_1G - 1)) + SZ_1G;
            continue;
        }
        let pd = (*pdpte & PHYS_MASK) as *mut u64;
        let pde = entry(pd, (addr >> 21) & 0x1FF);
        if *pde & PTE_P == 0 {
            addr += SZ_4K;
            continue;
        }
        if *pde & PTE_PS != 0 {
            set_wc_large(pde);
            addr = (addr & !(SZ_2M - 1)) + SZ_2M;
            continue;
        }
        let pt = (*pde & PHYS_MASK) as *mut u64;
        let pte = entry(pt, (addr >> 12) & 0x1FF);
        if *pte & PTE_P != 0 {
            *pte = (*pte & !(PTE_PCD | PTE_PS)) | PTE_PWT;
        }
        addr += SZ_4K;
    }
}

/// Select PAT slot 1 on a large-page entry: set PWT, clear PCD and bit 12.
#[inline]
unsafe fn set_wc_large(e: *mut u64) {
    *e = (*e & !(PTE_PCD | PDE_PAT)) | PTE_PWT;
}

#[inline]
unsafe fn entry(table: *mut u64, idx: u64) -> *mut u64 {
    table.add(idx as usize)
}
