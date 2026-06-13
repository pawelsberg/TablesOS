# Boot investigation — legacy-BIOS disk read (HISTORICAL, RESOLVED)

> **⚠️ Do not apply anything from this file to the current code.**
>
> This is the *closed* forensic log of the 2026-05 Byd Tracer BIOS bring-up
> (resolved in commit `2bbd3c5 Running on Byd Tracer - real hardware`). The
> diagnostic `stage1.s` it tells you to revert was already restored on
> 2026-06-12 as part of the UEFI work.
>
> **Every sector-0 offset and every asm snippet below refers to the obsolete
> header format v1** (header @ 0x1B0, GUID @ 0x1DC, *no partition table*).
> The current on-disk format is **v2**: header @ 0x180, GUID @ 0x1AC, and a
> real partition table @ 0x1BE whose single type-0xEF entry is what makes the
> stick visible to UEFI firmware at all. Re-introducing any v1 layout or the
> "real loader" snippet at the bottom of this file **silently overwrites the
> partition table — UEFI machines then stop listing the pendrive entirely.**
> `boot/layout.md` is the only authoritative layout document.
>
> The xHCI/USB findings (BIOS→OS handoff, EP0 reset+retry, bulk-stall
> recovery) remain valid and are already in `kernel/src/usb/xhci.rs`.

Tracking why the loader hangs on one legacy-BIOS PC while booting perfectly in QEMU.

---

## Current symptom (real hardware)

With the `[DIAG-B2]` loader (stage 1 does one INT 13h read of exactly 3 sectors), the screen shows:

```
TablesOS stage1 [DIAG-B2]
->stage2
```

then a blinking cursor on a new line; hang. Stage 2's own banner (`TablesOS stage2`) never appears.

## Hardware under test

- A legacy-capable PC. **Not** the Dell XPS 15 9510 — that machine is UEFI-only (no CSM) and cannot run this MBR loader at all; established separately.
- Boots from a USB pendrive ("Generic Flash Disk", 7.6 GB) through the BIOS as drive `0x80`.

---

## Eliminated causes (100% certain — each tied to a direct observation)

1. **Not a bad flash / corrupt pendrive.** Booting the *physical stick* in QEMU (`-snapshot`, writes discarded) ran all the way to `->long mode`. The bytes on the stick are correct and readable.
2. **Not a corrupt or buggy stage 2 image.** The same image boots in QEMU through `TablesOS stage2 / a20 ok / kernel loaded / ->long mode` (captured on the 0xE9 debug port). Stage 2's code and data are correct.
3. **Not a build / toolchain problem.** The image assembles and boots fully in QEMU.
4. **Stage 1 runs on the target.** Its banner and `->stage2` print on the real PC. Therefore, on this machine: legacy/CSM boot is available, the BIOS loads the MBR at `0x7C00` in real mode, INT 10h text output works, and INT 13h returns success (no `stage1 disk error` branch taken).
5. **Not the A20 or unreal-mode changes.** Execution never reaches them — stage 2 dies before its first banner, which is before any A20/unreal code runs.
6. **The header is patched correctly.** Confirmed on hardware: `S2N=0003`.
7. **NOT the disk read. This is the big one.** `[DIAG-C]` on the real PC printed
   `RD c=00 a=00 n=0010` and a 16-byte row `FA 00 00 ... 00` with **zero `CC`
   bytes** — identical to QEMU. So a single INT 13h extended read delivers all 16
   requested sectors correctly, with stage 2's first byte (`FA` = `cli`) in place.
   Under-delivery is **disproved**. Stage 2 is loaded fully and correctly.
8. **NOT the jump into stage 2, NOT stage 2 setup, NOT the first print.** `[DIAG-D]`
   trace on the real PC printed `1 2 TablesOS stage2 3` — stage 2 executes from
   0x8000, sets up segments/stack, and its `print`/INT 10h works.
9. **NOT A20, NOT unreal mode, NOT load_kernel.** The same trace continued
   `a20 ok` then `kernel loaded`. So A20 enable succeeded, unreal mode ran, and the
   **entire kernel load completed** — all ~136 of load_kernel's 64-sector INT 13h
   reads worked on real hardware. (The earlier per-sector hang is unrelated to this
   read pattern; the inter-read copy loop spaces them enough.)
10. **The failure is the VBE `4F02` set-mode call in `vesa_set`.** The trace ended
    `kernel loaded` then `stage2 error` (= `die`), and the only `die` reachable there
    is in `vesa_set`. Refined by `[DIAG-E]` on the real PC:
    ```
    VER=0300
    MODES=0035 LFB=0035 MATCH=01
    B32=0780x05A0      (widest 32bpp mode = 1920x1440)
    ANY=0780x05A0 b=08
    ```
    `MATCH=01` means the filter *does* find a qualifying 32bpp >=1024x720 mode, so
    `best_mode` is set and the "no mode" `die` is NOT taken. The only `die` left is
    the one after `4F02`. **Conclusion (certain): the mode is listed but `4F02`
    refuses to set it.** `vesa_set` chose the largest by area — 1920x1440, a 4:3 mode
    the panel almost certainly can't do — and gave up instead of trying a smaller one.

## Confirmed observations (100% certain)

Three loader variants, three distinct behaviours on the same PC:

| stage 1 read strategy            | Result on the real PC                                   |
|----------------------------------|---------------------------------------------------------|
| one 63-sector read               | `->stage2`, then stage 2 prints one garbage glyph (☺ = byte `0x01`), then hang — stage 2 *partially* ran |
| 1 sector per read, no delay      | hang at the stage 1 banner, **before** `->stage2` — hung inside the read loop on the 2nd read |
| one 3-sector read                | `->stage2`, then stage 2 prints **nothing**, hang        |

Read together with finding #7 (the read is perfect), the earlier ☺ / blank-glyph
behaviour was **not** under-delivery. With stage 2 fully in memory, the only things
left that can differ between QEMU (works) and this PC (hangs) are the **far jump into
stage 2** and **stage 2's real-mode execution / BIOS interaction**. The per-sector
"hang at the stage 1 banner" remains a separate, real effect (rapid INT 13h reads),
but it is no longer on the critical path.

## Kernel fix: boot from USB mass storage (real hardware)

After the video fix the OS reached the kernel, which then showed `CANNOT START /
no boot disk`: `ata::Ata::probe` returns None on real UEFI/NVMe hardware (the boot
device is a USB stick on xHCI, not a legacy ATA disk), so the kernel had no disk.

Fix (kernel/src/main.rs): a `BootDisk` enum (ATA **or** USB) implementing
`BlockDevice`, and `discover_boot_disk()` that tries ATA first (QEMU), then drives
the existing xHCI MSC stack via `autopilot_usb_drives()` and selects the device
whose MBR GUID matches the booted system GUID. The identity gate is **preserved and
strengthened**: (1) only the `booted`-flagged USB drive is chosen, (2) the original
re-read-sector-0 GUID gate in `kmain` is unchanged, (3) the USB handle is opened with
`open_with_identity_gate` (re-verifies GUID before every write).

Verified in QEMU:
- ATA boot (IDE image): `boot disk: ATA primary master` → UI. No regression.
- USB-only boot (`-device usb-storage,...,bootindex=0`, no IDE — mimics the real PC):
  `no ATA disk; trying USB` → xHCI bring-up → `boot disk: USB slot 1` → GUID verified
  → `mounted store` → `starting UI`.

### Real-hardware result #1: graphics OK, then blank (xHCI stall)

On the real PC: boots fast, **sets graphics mode** (the vesa fallback works on real
silicon), then **blank screen** — not the `CANNOT START` error. Since the previous
build *did* render `CANNOT START` from `fatal()`, the framebuffer path works; a blank
screen means the kernel is now stuck in the new USB/xHCI bring-up and never reaches
`fatal()` or the UI. So the xHCI driver hangs on real silicon (the flagged unknown).

### DIAG: on-screen boot trace (no serial on the laptop)

`main.rs::boot_status()` draws each boot milestone as a new framebuffer line, and
`discover_boot_disk` now drives the xHCI pipeline step-by-step with a marker before
each call:
`disk: probing ATA` → `no ATA -> trying USB` → `xHCI: enumerating PCI` →
`bring-up` → `reset + enable slots` → `address devices` → `fetch configurations` →
`configure endpoints` → `probe mass storage` → `USB: selecting boot drive by GUID` →
`USB: boot drive opened`. Verified in QEMU (USB-only boot) all the way to `starting UI`.

**The last line left on screen on the real PC = the xHCI step that hangs.**

### Real-hardware result #2: hangs in `bring_up`

The trace stopped at `xHCI: bring-up` — i.e. inside `bring_up()`, before it returns.
Its poll loops are TSC-bounded (a timeout returns `Err` and would advance the marker),
so a true stall means a poll whose timeout never fires (TSC mis-calibration), a DMA
alloc problem, or the **scratchpad path** — which QEMU never exercises (it reports
`scratch=0`; real xHCI controllers request scratchpad buffers).

Added finer markers inside `bring_up` (xhci.rs): `bu: params slots=.. scratch=..`,
`bu: 1 halt`, `bu: 2 reset`, `bu: 3 scratchpad alloc n=.. pgsz=..`, `bu: 4 rings`,
`bu: 5 start R/S`. QEMU baseline runs all of them (scratch=0) and reaches the UI.
The last `bu:` line on the real PC pinpoints the exact sub-step — and the `scratch=`
/`n=` values tell us whether the untested scratchpad path is involved.

### Real-hardware result #3: hangs at `bu: 5 start R/S` (scratch=34)

Real PC trace: `bu: params slots=64 scratch=34` → `1 halt` → `2 reset` →
`3 scratchpad alloc n=34` → `4 rings` → `5 start R/S`, then dead. So halt/reset/
scratchpad/rings all succeed; the **controller-start step hangs hard** — it doesn't
even hit the 1s poll timeout (which would return Err and advance the marker). On x86,
DMA is cache-coherent, so cacheable DMA buffers are *not* the cause. The classic cause
of a hard stall when taking over a controller the firmware was just using is the
**missing BIOS->OS ownership handoff** (xHCI USB Legacy Support Capability): the BIOS
SMI handler still owns the controller and wedges on the OS's start.

### Fix attempt: BIOS->OS handoff + start markers

`bios_handoff()` (xhci.rs), called first thing in `bring_up`: walks the extended-
capability list (xECP from HCCPARAMS1) for cap ID 1, sets HC OS Owned, waits for HC
BIOS Owned to clear, then clears the BIOS SMI enables. Prints `bu: handoff OK` /
`TIMEOUT (BIOS kept it)` / `no legacy cap` / `no xECP`. Added finer start markers:
`bu: 5a polling HCH`, `bu: 5b running` / `bu: 5b start TIMEOUT`.

QEMU: handoff reports `no legacy cap` (firmware doesn't own it) and boot reaches the
UI — no regression. On the real PC, read:
- `bu: handoff OK` then `bu: 5b running` → **handoff was the fix**; boot should proceed.
- `bu: handoff TIMEOUT` → BIOS refuses to release; needs a different tack (e.g. clear
  ownership more forcefully, or proceed anyway after disabling SMIs).
- `bu: handoff: no legacy cap` then still stuck at `5 start R/S`/`5a` → not ownership;
  the start write/poll itself wedges (deeper MMIO/SMM issue) — localized by 5a vs 5b.

### Real-hardware result #4: handoff FIXED the start hang; now `no boot disk`

The BIOS->OS handoff worked: the controller starts, the whole xHCI pipeline runs to
completion, and the kernel reaches `fatal("no boot disk")` (graphics, "CANNOT START").
So `discover_boot_disk` ran end-to-end and returned None — no GUID-matching USB drive.
The earlier hang is gone; this is a *different*, later problem (enumeration found the
device but didn't match it, or didn't find it as mass storage at all).

`fatal()` was clearing the screen, erasing the trace, so the *why* was invisible.

### DIAG: preserve trace on error + report enumeration result

- `fatal()` now draws **below** the boot trace (no screen clear) so the markers stay.
- `discover_boot_disk` now prints, before selecting:
  `USB: addressed=N msc=M` (devices addressed / recognized as mass storage),
  `USB: drives=K` (fully probed MSC drives), and per drive
  `USB: slot S sig=<bool> booted=<bool>` (MBR boot signature present / GUID matches the
  booted system GUID). QEMU baseline: `addressed=1 msc=1 drives=1 slot 1 sig=true booted=true`.

Reading the real-PC numbers (the lines from `xHCI: reset + enable slots` to the bottom):
- `addressed=0` → ports didn't enable / no device addressed (root-port reset timing, or
  the stick is behind an internal hub this driver doesn't enumerate).
- `addressed>0 msc=0` → device(s) seen but none class 08/06/50 (hub? composite? the boot
  port routes through something).
- `msc>0 drives=0` → MSC found but READ CAPACITY / first-block read failed on real HW.
- `drives>0` with `sig=true booted=false` → the stick is there with a TablesOS MBR but its
  GUID != the booted GUID (USB read returned different bytes than BIOS INT 13h, or a parse
  offset issue). `sig=false` → MBR read returned non-boot data.

### Real-hardware result #5: `addressed=2 msc=0` (no mass-storage detected)

The real PC enumerated **2** USB devices but recognized **0** as mass storage, so no
drive, so `no boot disk`. QEMU has exactly 1 device and it matches
(`dev s1 p1 46f4:0001 d=00/00/00 cfgcc=1 setcfg=1` / `if0 08/06/50 eps=2`). So on real
hardware either the boot stick is one of the two but its MSC interface (class 08,
subclass 06, protocol 50 + a bulk in/out pair) isn't being detected, or the stick is
behind a hub and the two addressed devices are something else.

### DIAG: dump each addressed device

`discover_boot_disk` now prints, per addressed device:
`dev s<slot> p<port> <vid>:<pid> d=<class>/<sub>/<proto> cfgcc=<cc> setcfg=<cc>` and,
per interface, `  if<n> <class>/<sub>/<proto> eps=<count>`. This identifies each device
and shows whether the config descriptor was fetched (`cfgcc=1`), SET_CONFIGURATION
succeeded (`setcfg=1`), and what interface classes it exposes. Reading the real-PC dump:
- an interface `08/06/50` present but `msc=0` → probe rejected it (likely no bulk pair
  found, or `setcfg!=1`); fix `probe_mass_storage`'s conditions.
- a device with `d=09/..` (hub) → the stick is behind an internal hub the driver doesn't
  walk → needs hub enumeration.
- `cfgcc!=1` / `setcfg!=1` / `(no config descriptor)` → config fetch or SET_CONFIG fails
  on real silicon for that device.
- neither device looks like the stick (vid/pid, no 08/06/50) → the stick wasn't addressed
  (root-port reset/enable missed it, or it's behind a hub).

### Real-hardware result #6: the boot stick fails CONFIG-descriptor fetch (cc=4)

Real PC dump:
```
dev s1 p7 0461:4d22 d=00/00/00 cfgcc=1 setcfg=1   (Primax — an internal device, OK)
dev s2 p9 058f:6387 d=00/00/00 cfgcc=4 setcfg=0   (Alcor Micro flash drive = THE BOOT STICK)
```
`058f:6387` is the Alcor Micro USB flash drive — our boot stick. It IS addressed and its
**device** descriptor read fine (we have its vid:pid), but its **configuration** descriptor
fetch returns `cc=4` (USB Transaction Error), so SET_CONFIGURATION never runs (`setcfg=0`),
no interfaces are parsed, and it isn't recognized as mass storage → `msc=0`.

Key narrowing: the 18-byte device descriptor succeeds but the 9-byte config header fails —
so it is NOT transfer size or EP0 max-packet. It's "first control transfer to the stick
works, the next one fails" — an xHCI endpoint-state / recovery-timing / speed-handling
issue that real devices are sensitive to and QEMU isn't. (Note: after a `cc=4` transaction
error xHCI **halts** the endpoint, so any later transfer on it also fails until the endpoint
is reset — consistent with `setcfg=0`.)

QEMU baseline for comparison: `spd=4 mps0=9 dcc=1 ev=-1 cfgcc=1` — a SuperSpeed device whose
config fetch works, proving the SS / max-packet path is fine. So the difference is whatever
the real stick reports.

### Real-hardware result #7 (DECISIVE): `spd=3 mps0=64 dcc=1 ev=-1 cfgcc=4`

The stick's line: `dev s2 p9 spd=3 058f:6387 mps0=64 dcc=1 ev=-1 cfgcc=4 setcfg=0`.
- `spd=3` = **high-speed** (USB 2.0). `mps0=64` = correct EP0 max packet for HS.
- `dcc=1` = device descriptor (18 bytes, control-IN #1) succeeded. `ev=-1` = no Evaluate
  Context (correct — only LS/FS need it). `cfgcc=4` = config descriptor (control-IN #2)
  USB Transaction Error.

So EP0 is set up correctly (mps0=64 right, dcc=1), no max-packet/eval involvement. The
**first** control transfer to the stick works and the **second** fails — specific to a
high-speed device on real silicon (QEMU's model is SuperSpeed and never hits it).

Note from `make_slot_resources`: EP0 context is built with **CErr=3**, so the controller
already hardware-retries the transaction 3× before reporting `cc=4` — i.e. the error is
*persistent across HW retries*, not a single glitch. After `cc=4` xHCI leaves EP0 **halted**,
which is why `setcfg=0` (nothing else can run on EP0 until it's reset).

Leading hypotheses for the HS "second transfer fails" (to pursue next):
1. EP0 transfer-ring / Dequeue-Cycle-State desync between transfer #1 (done in
   `address_enabled_slots`) and #2 (in `fetch_configurations`, which **clones**
   `SlotResources` and writes it back) — a cycle-bit or TR-dequeue mismatch the real
   controller enforces but QEMU tolerates.
2. Missing inter-transfer / post-address recovery **delay** that a cheap HS Alcor drive needs.
3. Needs the spec **Reset Endpoint + Set TR Dequeue Pointer** recovery after `cc=4`, then retry.

Read confirmed (`address_enabled_slots`): after Address Device the code goes **straight**
into `get_device_descriptor` with **no delay**, stores the ring at `tr_enqueue=3`;
`fetch_configurations` clones that and continues — cycle/DCS bookkeeping is consistent and
works in QEMU. No existing endpoint-reset/stall recovery anywhere (the GET_MAX_LUN stall is
just faked to `max_lun=0`).

### Fix attempt #1: post-SET_ADDRESS recovery delay (xhci.rs)

Root-cause hypothesis: the missing USB 2.0 §9.2.6.3 recovery interval. Added
`time::delay_ms(20)` after Address Device (before the first descriptor fetch) and
`time::delay_ms(5)` before the config-descriptor fetch. Rationale: a cheap HS Alcor drive
can ACK the first request issued during its recovery window but then transaction-error on
the next; honoring the recovery interval should let both succeed. Low risk, spec-correct,
~25 ms added. QEMU USB-only boot still reaches `starting UI` (no regression).

Result on the real PC: **still `cfgcc=4 setcfg=0`** (stick was slot 1 this time). So the
recovery interval was NOT the cause — timing is ruled out. (The delays are left in; harmless
and spec-correct.)

### Fix attempt #2: EP0 Reset-Endpoint + Set-TR-Dequeue + retry (xhci.rs)

`control_transfer` is now a retry wrapper around `control_transfer_once`: on any completion
code other than Success(1)/Short(13) it issues **Reset Endpoint (TRB 14)** on EP0 (DCI 1),
**Set TR Dequeue Pointer (TRB 16)** to the current ring enqueue position, waits ~2 ms, and
retries (up to 2 retries). This is the spec-mandated recovery for a halted endpoint, which
the driver previously lacked entirely. Added helpers `reset_endpoint` and `set_tr_dequeue`.
Recovery only runs on error, so QEMU (transfers succeed) is unaffected — verified it still
reaches `starting UI`, no `reset EP0` lines.

Expected on the real PC:
- stick line `... cfgcc=1 ... setcfg=1` → `drives=1`, `booted=true`, → UI. **Done.**
- still `cfgcc=4` → the failure is *deterministic* for this specific request (reset+retry
  can't fix a device that simply refuses this transfer), pointing at a device quirk with the
  **9-byte short config-header read**. Next fix would be to request the full configuration in
  one larger control-IN (e.g. wLength=255) instead of the 9-byte header, truncating to
  `wTotalLength` before parsing.

### DIAG: dump speed + EP0 max-packet + per-step completion codes

The dev line now reads `dev s<slot> p<port> spd=<1..5> <vid>:<pid> mps0=<bMaxPacketSize0>
dcc=<device-desc cc> ev=<eval-context cc or -1> cfgcc=<config-desc cc> setcfg=<cc>`.
(spd: 1=full 2=low 3=high 4=super 5=super+.) The stick's `spd` and `mps0`, vs QEMU's
`spd=4 mps0=9`, will say whether it's a high-speed-specific path, an Evaluate-Context issue,
or a post-transaction-error endpoint-halt that needs a Reset Endpoint + retry.

## IT BOOTS on real hardware (after fix attempt #2)

The EP0 Reset-Endpoint + retry recovered the Alcor stick's config-descriptor fetch: it
enumerates, mounts, and the **TablesOS UI runs on the real PC**. Full chain working:
stage1 read → stage2 video fallback → kernel USB boot-disk discovery → xHCI BIOS handoff →
EP0 transaction-error recovery.

### Real-hardware result #8: reads OK, WRITES fail (`I/O ERROR` on create-table)

Mount (reads) works; creating a table (writes) shows `I/O ERROR`. Cause: `msc_command`
(USB Bulk-Only Transport) had **no stall recovery** — same gap the control path had. A bulk
stall/transaction-error returns Err and leaves the bulk endpoint **halted**, breaking every
later command. Reads happen not to trip it; writes (data-OUT) and especially `flush`→
SYNCHRONIZE CACHE (which cheap sticks routinely **stall**) do.

### Fix attempt #3: bulk-endpoint recovery + best-effort flush + error surfacing (xhci.rs, ui.rs)

- `reset_bulk_endpoint` (Reset Endpoint + Set TR Dequeue, generalized `set_tr_dequeue_raw`).
- `msc_command`: on a CBW/data/CSW stall, reset the endpoint; after a data-stage stall still
  read the CSW (BOT recovery); retry the CSW once.
- `msc_write_sector`: retry the WRITE(10) once (the first stall is cleared by the reset).
- `flush`: SYNCHRONIZE CACHE is now best-effort — a failure is recorded and ignored (USB
  sticks are write-through; SYNC CACHE is optional and commonly unsupported).
- Diagnostics: `record_msc_err` / `take_last_msc_err`; the UI's generic "I/O error" now reads
  `I/O error: <detail>` (e.g. the exact failing command/stage), since the laptop has no serial.

All recovery runs only on error, so QEMU (transfers succeed) is unaffected — still boots,
mounts, `starting UI`. The write success-path is byte-identical.

Expected on the real PC: **create-table succeeds** (whole OS works). If it still errors, the
status bar now names the failing op (`I/O error: WRITE(10) ...` vs `... SYNCHRONIZE CACHE ...`
vs `... data stage ...`), which pinpoints the exact next fix.

## Bootloader build: `[DIAG-F]` — candidate fix (set-mode fallback)

`vesa_set` now tries matching modes **largest-first and falls back on a set
failure**: it keeps an area ceiling, sets the largest matching mode below it, and if
`4F02` fails it lowers the ceiling to that mode's area and retries the next-largest,
until one actually sets or none remain. Each attempt prints `V wxh`; a failed set
adds `F`. Stage 1 is still the `[DIAG-D]` read/dump/jump; stage 2 keeps the `1 2 3`
markers. QEMU baseline: `V 0A00x0640 ->long mode` (largest mode sets first try).

Expected on the real PC after `kernel loaded`:
- `V 0780x05A0 F` (1920x1440 refused) then `V <smaller> ` that **sets** → screen goes
  graphics and the OS GUI appears = **fixed**.
- If every attempt shows `F` and it ends in `stage2 error`, then *no* 32bpp LFB mode
  is settable on this GPU — next step would be to drop the 32bpp requirement to the
  depth in `ANY` (24/16bpp) and teach the kernel framebuffer code that depth, or set a
  specific mode number without the LFB flag.

Report the `V ...` line(s) you see.

## Earlier diagnostic: `[DIAG-E]` — enumerate VBE modes

Stage 1 is the `[DIAG-D]` read/dump that then jumps to stage 2. Stage 2 keeps the
`1`/`2`/`3` execution markers, runs `a20_enable` + `load_kernel` (both confirmed
good), then calls `vesa_diag` (instead of `vesa_set`), which walks the whole VBE
mode list and prints a summary, then halts. It reports:

```
VER=xxxx                 VbeVersion (BCD, e.g. 0300 = VBE 3.0); "no VBE" if 4F00 fails
MODES=xxxx LFB=xxxx MATCH=xx
    MODES = total modes in the list
    LFB   = how many are linear-framebuffer graphics modes
    MATCH = 01 if any LFB mode is 32bpp AND >=1024x720 (what vesa_set demands), else 00
B32=WWWWxHHHH            widest 32bpp LFB mode found (0000x0000 = none at all)
ANY=WWWWxHHHH b=BB       widest LFB mode of any depth, with its bpp (e.g. b=18 = 24bpp)
```

(All values are hex. `b=20` = 32bpp, `b=18` = 24bpp, `b=10` = 16bpp.)

### Healthy baseline (QEMU)

```
VER=0300
MODES=005D LFB=004B MATCH=01
B32=0A00x0640
ANY=0F00x0870 b=10
```

### What to capture on the real PC

The four VBE lines (`VER`, `MODES/LFB/MATCH`, `B32`, `ANY`). Interpretation:
- `MATCH=00` with `B32=0000x0000` → the GPU offers **no** 32bpp LFB mode; `vesa_set`
  must be taught to accept the depth in `ANY` (likely 24bpp). Kernel framebuffer
  drawing then needs to handle that depth.
- `MATCH=00` but `B32` non-zero → 32bpp modes exist but all below 1024x720; lower the
  resolution floor (and/or accept the largest available).
- `no VBE` → the BIOS has no VBE at all in this mode; needs a different approach.

## Earlier diagnostic: `[DIAG-D]` — trace stage 2 execution

`stage1.s` keeps the proven read + on-screen dump, then **jumps into stage 2**.
`stage2.s` now emits raw INT 10h markers at entry:

- `1` = stage 2 executing (very first instruction, before any setup)
- `2` = segments + stack set, interrupts enabled
- then it runs the normal `print` of `TablesOS stage2`
- `3` = returned from that first `print`
- then `a20 ok`, `kernel loaded`, ... (existing markers)

Expected healthy line after `DIAG END`: `12` then `TablesOS stage2` then `3` then `a20 ok` ...

Where it stops on the real PC pinpoints the fault:
- nothing after `DIAG END` → the `ljmp 0x0000,0x8000` doesn't transfer here (jump/CPU-state issue)
- `1` only → hangs in segment/stack setup
- `12` then stops → the `print` routine hangs on this BIOS (INT 10h / stack)
- `12` + `TablesOS stage2` + `3` then stops → first print works; fault is in `a20_enable`
- reaches `a20 ok` / `kernel loaded` → fault is later (paging / long-mode / kernel)

---

## Diagnostic build `[DIAG-C]` — what is on screen now

`boot/stage1.s` is temporarily an instrumented build that **does not jump to stage 2**. It pre-fills the read buffer with the sentinel byte `0xCC`, issues **one** INT 13h extended read of 16 sectors (LBA 1..16) to `0x8000`, then prints and halts:

```
TablesOS stage1 [DIAG-C]
DL=xx              drive number the BIOS passed in
EXT cf=xx          INT 13h extensions present? 00 = yes (CF clear), 01 = no
S2N=xxxx           stage2 sector count from the patched header (expect 0003)
RD c=x a=xx n=xxxx read result: c = carry (0 ok / 1 err), a = AH status,
                   n = sectors the BIOS CLAIMS it transferred (DAP +2)
xx xx xx ... (16)  first byte of each of the 16 sectors read into 0x8000+
DIAG END
```

Reading the 16-byte row: **`CC` = that sector was NOT written by the read; any other value = delivered.** The first byte should be `FA` (stage 2's `cli`) if sector 0 arrived. The position where the row turns (back) into `CC` is the real per-call delivery limit.

### Healthy baseline (QEMU, for comparison)

```
DL=80
EXT cf=00
S2N=0003
RD c=00 a=00 n=0010
FA 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
DIAG END
```

`n=0010` (16) and zero `CC` bytes = full delivery.

### What to capture on the real PC

Write down the `RD ...` line and the 16-byte row. Interpretation:
- `n` far less than `0010`, and `CC` filling most of the row → BIOS under-delivers; the value/cutoff tells us the max sectors per call.
- `n=0010` but the row is mostly `CC` → BIOS reports a transfer it didn't perform (count field is a lie).
- `c=01` / non-zero `a=` → the read actually failed (we'd then chase the status code, not under-delivery).

---

## (HISTORICAL — already done; do NOT re-apply) To restore the real loader after fixing

**This step was completed on 2026-06-12.** The snippet below uses the v1
header offsets (`H_S2_SECS` @ 0x1C8 etc.) and predates the partition table at
0x1BE; pasting it into today's `stage1.s` corrupts the UEFI boot path. Kept
only as a record of what the investigation-era loader looked like.

Revert `boot/stage1.s` to the loader (it currently lives only in the working tree, uncommitted). The working read routine — one INT 13h read of exactly the header sector count — was:

```asm
    mov     ax, word ptr [H_S2_SECS]       # stage2 size in sectors
    test    ax, ax
    jnz     .have_cnt
    mov     ax, STAGE2_SECTORS             # fallback if header is unpatched
.have_cnt:
    mov     cx, 5                          # retry budget
.read_try:
    mov     si, S1 + (dap - _start)
    mov     word ptr [si+0], 0x0010        # DAP size=16, reserved=0
    mov     word ptr [si+2], ax            # sectors = stage2 size
    mov     word ptr [si+4], 0x0000        # buffer offset
    mov     word ptr [si+6], STAGE2_SEG    # dest segment
    mov     dword ptr [si+8], STAGE2_LBA   # start LBA
    mov     dword ptr [si+12], 0
    push    ax
    mov     ah, 0x42
    mov     dl, byte ptr [S1 + (drive - _start)]
    push    ds
    push    es
    push    cx
    int     0x13
    pop     cx
    pop     es
    pop     ds
    pop     ax
    jnc     .read_ok
    xor     ah, ah                         # reset disk controller
    mov     dl, byte ptr [S1 + (drive - _start)]
    push    ax
    push    ds
    push    es
    push    cx
    int     0x13
    pop     cx
    pop     es
    pop     ds
    pop     ax
    loop    .read_try
    jmp     disk_err
.read_ok:
```

The eventual real fix probably needs a **delay between reads** (the rapid-read hang) combined with a **safe per-call sector count** (the under-delivery limit), once `[DIAG-C]` tells us both numbers. The same fix must then be applied to stage 2's `load_kernel` (8685 sectors, currently 64-sector chunks).
