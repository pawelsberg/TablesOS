# Solved — Shutdown hangs on real hardware

**Status: SOLVED.** The GUI's **Shut down** action (Table List → `s` → confirm)
now powers the machine off on real hardware (Byd Tracer laptop) via an ACPI S5
soft-off. QEMU continues to power off cleanly through the *same* code path, so
the fix is regression-tested on every emulator run. Confirmed on hardware: the
screen goes black and the machine powers down.

This file is the historical record (same pattern as
`Byd Tracer Real Hardware.md` and `USB mouse on real hardware.md`). It keeps the
root-cause reasoning because that reasoning is exactly what stops someone
"simplifying" the power-off back into the bug — see **Invariants** below.

---

## Symptom (as it was)

On real hardware the Shut down action did nothing useful: the screen froze on
the last UI frame and the machine never powered off. In QEMU the same action
exited cleanly, so it looked fine throughout development.

## Root cause (confirmed)

The old `shutdown()` (`kernel/src/ui.rs`) wrote only the **emulator** power-off
I/O ports, then halted:

```rust
fn shutdown() -> ! {
    use x86_64::instructions::port::Port;
    unsafe {
        Port::<u16>::new(0x604).write(0x2000);  // QEMU >= 2.0
        Port::<u16>::new(0xB004).write(0x2000); // older QEMU / Bochs
        Port::<u16>::new(0x4004).write(0x3400); // virt
    }
    serial_println!("shutdown requested; halting");
    loop { x86_64::instructions::hlt(); }       // <-- real HW lands here forever
}
```

Ports `0x604` / `0xB004` / `0x4004` are devices **invented by QEMU/Bochs/virt**.
On a physical chipset nothing is wired to them, so the three writes are silently
discarded and control falls into the `hlt` loop. That loop *is* the "hang" — the
machine is alive, halted, with no way to power itself off. There was no bug in
the disk/USB/video stack; real-silicon power-off was simply never implemented
(the old doc-comment even said "real ACPI/APM … is out of scope").

Why `0x604`/`0x2000` happens to work in QEMU is the key clue: QEMU's FADT puts
the ACPI **PM1a control register** at I/O port `0x604`, and its `\_S5_` sleep
type is `0`, so `(0 << 10) | SLP_EN(0x2000)` written to `0x604` is a real ACPI S5
transition — just with the address and value hard-coded to QEMU's numbers. Real
machines use different numbers, which must be read from firmware.

## The fix (what shipped)

A small, AML-interpreter-free ACPI module — **`kernel/src/acpi.rs`** — does the
standard ACPI S5 soft-off, discovering every machine-specific number from
firmware instead of hard-coding QEMU's:

1. **Find the RSDP** — scan the EBDA (first KiB; segment at phys `0x40E`) and the
   BIOS area `0xE0000..0x100000` on 16-byte boundaries for `"RSD PTR "` with a
   valid checksum.
2. **Walk the RSDT/XSDT** (XSDT preferred when ACPI ≥ 2.0) to the **FADT**
   (signature `FACP`), collecting any **SSDT**s.
3. **Read FADT fields** — `PM1a_CNT_BLK`/`PM1b_CNT_BLK`, `SMI_CMD` +
   `ACPI_ENABLE`, and the `DSDT` pointer (64-bit `X_*` GAS variants preferred
   when present and in I/O space).
4. **Parse the DSDT (then SSDTs) for `\_S5_`** by the well-known structural match
   `NameOp _S5_ PackageOp <pkglen> <count> a b` to get `SLP_TYPa`/`SLP_TYPb`.
5. **Enable ACPI** if still in legacy mode (`SCI_EN`=0): write `ACPI_ENABLE` to
   `SMI_CMD` and poll `SCI_EN` (≤ ~3 s). No-op when already enabled / no SMI port.
6. **Write `(SLP_TYPx << 10) | SLP_EN(1<<13)`** to `PM1a_CNT` (and `PM1b_CNT` if
   present). The chipset cuts power.

`acpi::poweroff()` returns **only on failure**, with a short `&'static str`
reason for on-screen diagnostics (the laptop has no serial); on success the
machine is already off.

### `shutdown()` order (`kernel/src/ui.rs`)

1. Mask interrupts and paint a "TABLESOS — SHUTTING DOWN / Powering off…" screen
   (a frozen UI looks like a crash).
2. `crate::acpi::poweroff()` — the real fix; works on physical HW **and** QEMU.
3. **Fallback**: the three legacy emulator-port writes, in case ACPI discovery
   ever fails. No-ops on real hardware.
4. If still running: paint **"IT IS NOW SAFE TO TURN OFF YOUR COMPUTER"** plus
   the ACPI failure reason, and `hlt`.

### Safety / memory access

All ACPI tables are read straight from physical RAM — the bootloader
(`boot/stage2.s`) identity-maps `0..4 GiB` with 2 MiB pages, so phys == virt.
Every read is gated on `PHYS_LIMIT = 4 GiB` (`readable()`); a pointer at/above
4 GiB makes the code bail to the fallback instead of page-faulting. Table scans
are length-bounded and the ACPI-enable poll is capped, so the routine can never
itself hang.

## Verification

- **QEMU (`-machine pc`, 2026-06-09):** booted a copy headless, drove the monitor
  `sendkey s` / `sendkey ret`, captured serial. The process **exited 0**
  (guest-initiated power-off); serial showed the code re-deriving QEMU's magic
  numbers — `PM1a_CNT = 0x604`, `SLP_TYPa = 0`, ACPI enabled via `SMI_CMD 0xb2`:

  ```
  acpi: RSDP @ 0xf52e0 rev 0
  acpi: FADT pm1a=0x604 pm1b=0x0 smi=0xb2 en=0xf1 dsdt=0x3ffe0040
  acpi: _S5_ SLP_TYPa=0 SLP_TYPb=0
  acpi: enabling ACPI via SMI cmd 0xb2
  ```

  No "could not power off" line → power-off happened inside `poweroff()`, before
  the fallback. The RSDP was ACPI 1.0 (`rev 0`), so the **32-bit RSDT path** is
  the one QEMU exercises.
- **Real hardware (Byd Tracer laptop):** Shut down → screen black, machine powers
  off. Confirmed.

## Invariants — do NOT regress these

Each is here because reverting it silently brings the hang back:

1. **Keep the emulator-port fallback in `shutdown()`.** It is the safety net if
   ACPI discovery ever fails and is harmless on real hardware.
2. **ACPI runs FIRST, fallback SECOND.** This is deliberate so QEMU exercises the
   real ACPI path on every run (our only automated test for it). Reordering so
   the emulator ports come first makes QEMU power off via `0x604` *before* ACPI
   runs, leaving the ACPI code untested — a real-HW regression could then ship
   unnoticed. After changes, re-verify QEMU still prints the `acpi:` lines and
   exits 0.
3. **Keep every physical read gated on `PHYS_LIMIT` (4 GiB).** It matches the
   bootloader's identity map (`boot/stage2.s`, "Identity-map 0..4 GiB"). If the
   map ever changes, update `PHYS_LIMIT` in lockstep or ACPI reads can fault.
4. **Don't "simplify" `shutdown()` back to just the magic ports** — that is the
   exact bug this file documents.

### Diagnostic table (if a future board fails to power off)

`shutdown()` prints the ACPI failure reason on screen. Mapping:

- `no RSDP` / `no FADT` → firmware tables not found in the legacy regions (CSM
  should place the RSDP in `0xE0000..0xFFFFF` or the EBDA).
- `no _S5_` → the `\_S5_` package wasn't located by the structural scan (vendor
  AML shape we don't match, or an SSDT past our cap). Widen the scan / raise the
  SSDT cap, or dump the DSDT bytes around `_S5_`.
- `no PM1a port` → hardware-reduced ACPI (no PM1 block); would need the FADT
  `SLEEP_CONTROL_REG` path — out of scope (a legacy/CSM boot shouldn't hit it).
- `PM1 write ignored` → found everything and wrote PM1, but the board didn't
  power off. Likely `SLP_TYP` decoding or the ACPI-enable handshake; capture the
  `acpi:` numbers (consider surfacing `pm1a` / `SLP_TYPa` on screen next pass).
