//! Minimal ACPI support — *just* enough to power a real machine off (the ACPI
//! "S5 soft-off" transition). There is no AML interpreter here.
//!
//! ## Why this module exists
//!
//! The GUI's Shutdown action used to write only the QEMU/Bochs/virt emulator
//! power-off I/O ports (`0x604` / `0xB004` / `0x4004`). Those ports are
//! unconnected on a physical PC, so the writes did nothing and the kernel fell
//! straight into its `hlt` loop — the machine "just hung" (see
//! `solved-issues/Shutdown on real hardware.md`). Real hardware needs the
//! proper ACPI sequence:
//!
//! 1. Find the RSDP, follow it to the RSDT/XSDT, and locate the **FADT**.
//! 2. From the FADT read the **PM1a/PM1b control** I/O ports and the **SMI
//!    command** port + **ACPI-enable** value.
//! 3. Parse the **DSDT** (and any SSDT) for the fixed `\_S5_` package to get the
//!    **SLP_TYPa/SLP_TYPb** sleep-type values for S5.
//! 4. If the chipset is still in legacy mode, switch it to ACPI mode via the SMI
//!    command port.
//! 5. Write `SLP_TYPx | SLP_EN` to PM1a_CNT (and PM1b_CNT). The chipset cuts
//!    power.
//!
//! ## Regression safety
//!
//! This path is exercised on **every** shutdown, including in QEMU: QEMU's FADT
//! exposes PM1a_CNT at I/O `0x604` with `SLP_TYPa = 0`, i.e. exactly the
//! `0x2000` value the old hard-coded write used. So the emulator regression-
//! tests the discovery + write logic, and [`crate::ui`] keeps the legacy port
//! writes as a fall-back if discovery ever fails.

use crate::serial_println;
use crate::time;
use x86_64::instructions::port::Port;

/// The bootloader (`boot/stage2.s`) identity-maps only the first **4 GiB** of
/// physical memory (2 MiB pages). Any ACPI pointer at or above this is not
/// mapped, so dereferencing it would page-fault. Every physical read is gated
/// on this limit; an out-of-range table makes us bail (and fall back) rather
/// than crash.
const PHYS_LIMIT: u64 = 0x1_0000_0000;

/// SLP_EN — bit 13 of the PM1 control register. Writing it together with a
/// SLP_TYP value starts the sleep transition.
const SLP_EN: u16 = 1 << 13;

/// SCI_EN — bit 0 of PM1a_CNT. Set once the chipset is in ACPI (not legacy)
/// mode; we use it both to detect "already enabled" and to confirm enabling.
const SCI_EN: u16 = 1;

/// True when `[pa, pa+len)` lies wholly inside the identity-mapped window.
#[inline]
fn readable(pa: u64, len: u64) -> bool {
    pa != 0 && pa.checked_add(len).is_some_and(|end| end <= PHYS_LIMIT)
}

// Raw physical reads. Physical == virtual here (identity-mapped). ACPI tables
// are ordinary RAM but packed, so reads are unaligned-safe. Callers must have
// already checked `readable`.
#[inline]
unsafe fn rd_u8(pa: u64) -> u8 {
    core::ptr::read_unaligned(pa as *const u8)
}
#[inline]
unsafe fn rd_u16(pa: u64) -> u16 {
    core::ptr::read_unaligned(pa as *const u16)
}
#[inline]
unsafe fn rd_u32(pa: u64) -> u32 {
    core::ptr::read_unaligned(pa as *const u32)
}
#[inline]
unsafe fn rd_u64(pa: u64) -> u64 {
    core::ptr::read_unaligned(pa as *const u64)
}

/// 8-bit ACPI checksum: a valid structure's bytes sum to 0 (mod 256).
fn checksum_ok(pa: u64, len: u64) -> bool {
    if !readable(pa, len) {
        return false;
    }
    let mut sum: u8 = 0;
    let mut i = 0;
    while i < len {
        sum = sum.wrapping_add(unsafe { rd_u8(pa + i) });
        i += 1;
    }
    sum == 0
}

const RSDP_SIG: [u8; 8] = *b"RSD PTR ";

/// Locate the RSDP. Per the ACPI spec it lives in one of two BIOS regions: the
/// first KiB of the EBDA, or the BIOS read-only area `0xE0000..0x100000`, on a
/// 16-byte boundary, with a valid 20-byte checksum.
fn find_rsdp() -> Option<u64> {
    // The EBDA segment (a 16-byte paragraph) is stored at physical 0x40E.
    let ebda = (unsafe { rd_u16(0x40E) } as u64) << 4;
    if (0x400..0xA_0000).contains(&ebda) {
        if let Some(p) = scan_rsdp(ebda, ebda + 0x400) {
            return Some(p);
        }
    }
    scan_rsdp(0xE_0000, 0x10_0000)
}

/// Scan `[start, end)` on 16-byte boundaries for a checksum-valid RSDP.
fn scan_rsdp(start: u64, end: u64) -> Option<u64> {
    let mut pa = start & !0xF;
    while pa + 20 <= end {
        let sig_ok = (0..8).all(|i| unsafe { rd_u8(pa + i) } == RSDP_SIG[i as usize]);
        if sig_ok && checksum_ok(pa, 20) {
            return Some(pa);
        }
        pa += 16;
    }
    None
}

/// Call `f(signature, table_phys)` for every table listed in the RSDT/XSDT.
fn for_each_sdt(mut f: impl FnMut([u8; 4], u64)) {
    let Some(rsdp) = find_rsdp() else {
        serial_println!("acpi: no RSDP found");
        return;
    };
    let revision = unsafe { rd_u8(rsdp + 15) };
    serial_println!("acpi: RSDP @ {:#x} rev {}", rsdp, revision);

    // ACPI 2.0+ (revision >= 2) gives a 64-bit XSDT; prefer it, fall back to the
    // 32-bit RSDT if the XSDT pointer is missing or out of the mapped window.
    let (root, entry_size): (u64, u64) = if revision >= 2 {
        let xsdt = unsafe { rd_u64(rsdp + 24) };
        if readable(xsdt, 36) {
            (xsdt, 8)
        } else {
            (unsafe { rd_u32(rsdp + 16) } as u64, 4)
        }
    } else {
        (unsafe { rd_u32(rsdp + 16) } as u64, 4)
    };

    let len = if readable(root, 36) {
        (unsafe { rd_u32(root + 4) }) as u64
    } else {
        0
    };
    if len < 36 || !readable(root, len) {
        serial_println!("acpi: root SDT unreadable @ {:#x}", root);
        return;
    }

    // Defensive cap — a corrupt length must not spin us forever.
    let count = ((len - 36) / entry_size).min(1024);
    for i in 0..count {
        let ent = root + 36 + i * entry_size;
        let table = if entry_size == 8 {
            unsafe { rd_u64(ent) }
        } else {
            (unsafe { rd_u32(ent) }) as u64
        };
        if !readable(table, 36) {
            continue;
        }
        let sig = [
            unsafe { rd_u8(table) },
            unsafe { rd_u8(table + 1) },
            unsafe { rd_u8(table + 2) },
            unsafe { rd_u8(table + 3) },
        ];
        f(sig, table);
    }
}

/// The handful of FADT fields the power-off path needs.
struct Fadt {
    smi_cmd: u32,
    acpi_enable: u8,
    pm1a_cnt: u32,
    pm1b_cnt: u32,
    dsdt: u64,
}

/// Read the relevant FADT fields, preferring the 64-bit / GAS "extended" fields
/// when the table is long enough to contain them and they're populated.
fn parse_fadt(pa: u64) -> Fadt {
    let len = unsafe { rd_u32(pa + 4) } as u64;
    let mut f = Fadt {
        smi_cmd: unsafe { rd_u32(pa + 48) },
        acpi_enable: unsafe { rd_u8(pa + 52) },
        pm1a_cnt: unsafe { rd_u32(pa + 64) },
        pm1b_cnt: unsafe { rd_u32(pa + 68) },
        dsdt: unsafe { rd_u32(pa + 40) } as u64,
    };
    // X_DSDT (64-bit) at offset 140; X_PM1{a,b}_CNT_BLK (GAS) at 172 / 184.
    if len >= 148 {
        let x_dsdt = unsafe { rd_u64(pa + 140) };
        if readable(x_dsdt, 36) {
            f.dsdt = x_dsdt;
        }
    }
    if len >= 184 {
        if let Some(p) = gas_io_port(pa + 172) {
            f.pm1a_cnt = p;
        }
    }
    if len >= 196 {
        if let Some(p) = gas_io_port(pa + 184) {
            f.pm1b_cnt = p;
        }
    }
    f
}

/// Read a Generic Address Structure, returning its address only when it is a
/// non-zero **System I/O** port that fits in 16 bits — which is what a PM1
/// control block always is. Anything else (memory-mapped, zero) is rejected so
/// the caller keeps the legacy 32-bit field instead.
fn gas_io_port(pa: u64) -> Option<u32> {
    let space = unsafe { rd_u8(pa) }; // 0 = system memory, 1 = system I/O
    let addr = unsafe { rd_u64(pa + 4) };
    (space == 1 && addr != 0 && addr <= 0xFFFF).then_some(addr as u32)
}

/// SLP_TYP values for the `\_S5_` (soft-off) state, pulled from the DSDT/SSDT.
struct S5 {
    a: u8,
    b: u8,
}

/// Scan an AML table (DSDT or SSDT) for the fixed `\_S5_` package and pull out
/// SLP_TYPa / SLP_TYPb. No AML interpreter — just the well-known structural
/// match every small OS uses:
///
/// ```text
///   NameOp(0x08) ['\'] '_' 'S' '5' '_' PackageOp(0x12) <PkgLength> <count> a b
/// ```
fn find_s5(table: u64) -> Option<S5> {
    if !readable(table, 36) {
        return None;
    }
    let len = unsafe { rd_u32(table + 4) } as u64;
    if len < 36 || !readable(table, len) {
        return None;
    }
    let end = table + len;
    let mut p = table + 36; // skip the 36-byte SDT header
    while p + 4 <= end {
        let is_s5 = unsafe {
            rd_u8(p) == b'_' && rd_u8(p + 1) == b'S' && rd_u8(p + 2) == b'5' && rd_u8(p + 3) == b'_'
        };
        if is_s5 {
            // Validate it is a NameOp-introduced package (optionally root-`\`
            // prefixed) immediately followed by a PackageOp.
            let b1 = unsafe { rd_u8(p - 1) };
            let b2 = if p >= table + 2 { unsafe { rd_u8(p - 2) } } else { 0 };
            let name_ok = b1 == 0x08 || (b1 == b'\\' && b2 == 0x08);
            let pkg_op = p + 4 < end && unsafe { rd_u8(p + 4) } == 0x12;
            if name_ok && pkg_op {
                return parse_s5_package(p + 5, end);
            }
        }
        p += 1;
    }
    None
}

/// Decode the package body that follows `_S5_`'s PackageOp. `q` points at the
/// first PkgLength byte; `end` is one past the table.
fn parse_s5_package(mut q: u64, end: u64) -> Option<S5> {
    if q >= end {
        return None;
    }
    // PkgLength: the top two bits of the lead byte give how many *extra* length
    // bytes follow. Skip those + the lead byte + the NumElements byte to land on
    // the first element.
    let lead = unsafe { rd_u8(q) };
    q += (lead >> 6) as u64 + 2;

    // Each element is either a bare integer constant — ZeroOp(0x00)/OneOp(0x01)
    // encode 0/1 directly, and SLP_TYP is only ever 0..7 — or a BytePrefix
    // (0x0A) followed by the byte.
    let a = read_aml_byte(&mut q, end)?;
    let b = read_aml_byte(&mut q, end).unwrap_or(0);
    Some(S5 { a, b })
}

/// Read one small AML integer at `*q`, advancing past it. Skips an optional
/// BytePrefix (0x0A).
fn read_aml_byte(q: &mut u64, end: u64) -> Option<u8> {
    if *q >= end {
        return None;
    }
    if unsafe { rd_u8(*q) } == 0x0A {
        *q += 1;
    }
    if *q >= end {
        return None;
    }
    let v = unsafe { rd_u8(*q) };
    *q += 1;
    Some(v)
}

/// Switch the chipset from legacy to ACPI mode if it isn't already, so the PM1
/// sleep write is honoured. A no-op when already enabled, or when the firmware
/// exposes no SMI command port (already-ACPI / hardware-reduced platforms).
fn enable_acpi(f: &Fadt) {
    if f.pm1a_cnt == 0 || f.pm1a_cnt > 0xFFFF {
        return;
    }
    let mut pm1a = Port::<u16>::new(f.pm1a_cnt as u16);
    if unsafe { pm1a.read() } & SCI_EN != 0 {
        return; // already in ACPI mode
    }
    if f.smi_cmd == 0 || f.smi_cmd > 0xFFFF || f.acpi_enable == 0 {
        return; // nothing to poke / nothing to do
    }
    serial_println!("acpi: enabling ACPI via SMI cmd {:#x}", f.smi_cmd);
    unsafe { Port::<u8>::new(f.smi_cmd as u16).write(f.acpi_enable) };
    // Wait up to ~3 s for SCI_EN to come up.
    let mut tries = 0;
    while tries < 300 && unsafe { pm1a.read() } & SCI_EN == 0 {
        time::delay_ms(10);
        tries += 1;
    }
}

/// Attempt an ACPI S5 soft-off. On success the machine powers off and this
/// **never returns**; it only returns (with a short reason, for on-screen
/// diagnostics — the target laptop has no serial) when power-off was not
/// possible, so the caller can fall back.
#[must_use]
pub fn poweroff() -> &'static str {
    let mut fadt: Option<Fadt> = None;
    let mut ssdts: [u64; 8] = [0; 8];
    let mut n_ssdt = 0usize;
    for_each_sdt(|sig, addr| {
        if &sig == b"FACP" {
            fadt = Some(parse_fadt(addr));
        } else if &sig == b"SSDT" && n_ssdt < ssdts.len() {
            ssdts[n_ssdt] = addr;
            n_ssdt += 1;
        }
    });

    let Some(fadt) = fadt else {
        return "no FADT";
    };
    serial_println!(
        "acpi: FADT pm1a={:#x} pm1b={:#x} smi={:#x} en={:#x} dsdt={:#x}",
        fadt.pm1a_cnt,
        fadt.pm1b_cnt,
        fadt.smi_cmd,
        fadt.acpi_enable,
        fadt.dsdt
    );

    // SLP_TYP for S5: the DSDT defines `\_S5_` on virtually every machine; scan
    // any SSDTs too in case a vendor split it out.
    let mut s5 = find_s5(fadt.dsdt);
    let mut k = 0;
    while s5.is_none() && k < n_ssdt {
        s5 = find_s5(ssdts[k]);
        k += 1;
    }
    let Some(s5) = s5 else {
        return "no _S5_";
    };
    serial_println!("acpi: _S5_ SLP_TYPa={} SLP_TYPb={}", s5.a, s5.b);

    if fadt.pm1a_cnt == 0 || fadt.pm1a_cnt > 0xFFFF {
        return "no PM1a port"; // hardware-reduced ACPI — out of scope here
    }

    enable_acpi(&fadt);

    // The write that actually cuts the power.
    let val_a = (((s5.a as u16) & 7) << 10) | SLP_EN;
    unsafe { Port::<u16>::new(fadt.pm1a_cnt as u16).write(val_a) };
    if fadt.pm1b_cnt != 0 && fadt.pm1b_cnt <= 0xFFFF {
        let val_b = (((s5.b as u16) & 7) << 10) | SLP_EN;
        unsafe { Port::<u16>::new(fadt.pm1b_cnt as u16).write(val_b) };
    }

    // Give the chipset a beat to act. If we're still running, it didn't take.
    time::delay_ms(100);
    serial_println!("acpi: PM1 sleep write did not power off");
    "PM1 write ignored"
}
