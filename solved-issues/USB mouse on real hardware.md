# Solved — USB mouse dead on real hardware

**Status: SOLVED.** A USB-HID boot-protocol mouse driver on our own xHCI stack
now drives the pointer on real hardware (Byd Tracer laptop); QEMU continues to
work via both this path (a `usb-mouse` on the xHCI bus) and the legacy PS/2
mouse. Confirmed on hardware: the pointer moves and clicks.

This file is the historical record (the same pattern as
`Byd Tracer Real Hardware.md`). It keeps the root-cause reasoning because that
reasoning is exactly what stops someone "fixing" a future mouse problem by
reverting the parts that make this work — see **Invariants** below.

---

## Symptom (as it was)

| | QEMU | Real laptop |
|---|---|---|
| Boot to GUI | yes | yes (after the Byd Tracer fixes) |
| Keyboard | works | works (genuine PS/2 — wired to the EC) |
| Mouse move / click | worked | **nothing** |

The keyboard working while the mouse was dead was the key clue: the laptop
keyboard is a genuine i8042 PS/2 device, so it never depended on BIOS USB
emulation; the pointer is USB and did.

## Root cause (confirmed)

The mouse never reached the kernel because **the OS owns the xHCI controller**
(required for the USB boot disk) and the **BIOS SMM PS/2 emulation** that used
to forward a USB pointer into ports 0x60/0x64 is switched off by the ownership
handoff. The OS then had no USB-HID driver, so it produced zero pointer events.

The chain, each link tied to code:

1. **Input was PS/2-only.** IRQ1/IRQ12 → ports 0x60/0x64 → `kernel/src/ps2.rs`.
   The UI consumed input through exactly that one source.
2. **No USB-HID driver existed.** `kernel/src/usb/xhci.rs` was a mass-storage
   stack; `configure_endpoints` deliberately skipped interrupt-endpoint
   scheduling ("Interval = 0 is fine for Bulk… left for later").
3. **The boot path turns off BIOS USB legacy emulation.** `bios_handoff`
   (`xhci.rs`) clears the USBLEGCTLSTS SMI-enable bits during the BIOS→OS
   ownership handoff. That is precisely what made the BIOS SMM handler re-inject
   USB HID as PS/2. After it, the BIOS forwards nothing.
4. **Order of operations / hardware-only.** The handoff runs inside
   `discover_boot_disk`, reached only when the ATA probe fails — i.e. only on
   real hardware booting from USB. QEMU boots the IDE image and has a genuine
   emulated PS/2 mouse, which is why this never showed up in QEMU.

**Why we could not just keep the BIOS emulation:** not doing the handoff
hard-hangs the controller at start (Byd Tracer result #3/#4), and bring-up
resets + re-enumerates the controller, destroying the BIOS's SMM device state
regardless of the SMI bits. Owning xHCI for the boot disk and leaving BIOS HID
emulation alive are mutually exclusive. The only coherent answer was to own the
controller and drive the HID mouse ourselves.

## The fix (what shipped)

A USB-HID boot-protocol mouse driver built on the existing xHCI
enumerate/address/configure machinery, in `kernel/src/usb/xhci.rs`:

- **`probe_hid_mouse`** finds a configured interface with class `0x03` /
  subclass `0x01` / protocol `0x02` (HID / boot / mouse) and its interrupt-IN
  endpoint, issues `SET_PROTOCOL(boot)` (and best-effort `SET_IDLE(0)`), and
  arms the first interrupt transfer. Boot protocol means a fixed 3-byte report
  `[buttons, dX i8, dY i8]` — no HID report-descriptor parsing needed.
- **`configure_endpoints`** now fills the EP-context **Interval** (and Max ESIT
  payload) for periodic (Interrupt/Isoch) endpoints — `xhci_interval` encodes
  the spec §6.2.3.6 `2^N × 125 µs` value per speed. Without this a real
  controller never schedules the transfers. Bulk/Control still get Interval 0,
  so the MSC path is unchanged.
- **One interrupt-IN TRB is kept armed at all times.** `try_consume_event` is
  the non-blocking event-ring primitive shared by the blocking `drain_event`
  and the mouse poll; `post_normal_trb` is the shared enqueue used by both
  `bulk_transfer` and `arm_mouse`. Mouse completions arriving mid-`drain_event`
  (e.g. while an MSC transfer is in flight) are serviced inline and skipped, so
  a moving mouse can never be mistaken for the command/bulk event a drain waits
  for.
- **Polling is cooperative, from the UI loop** (`ui.rs`, `xhci::pump_mouse()`),
  never from an IRQ — xHCI transfers allocate, and an interrupt handler must not
  (the heap-free-IRQ invariant). Each report accumulates a delta + buttons;
  `poll_mouse_delta` returns the net movement, and `ps2::feed_mouse_delta`
  pushes it through `apply_motion` into the **same** cursor position and **same**
  event queue the PS/2 IRQ path uses. The UI drains it identically — no
  UI-layer changes beyond the idle-wait strategy below.
- **Idle wait.** A USB mouse delivers no IRQ to wake `hlt`, so when a USB mouse
  is present (`xhci::mouse_present()`, cached at UI start) the idle path does a
  short `time::delay_ms(8)` (~125 Hz poll) instead of sleeping until the next
  IRQ. Keyboard IRQs still enqueue and are picked up next iteration.
- **Boot wiring.** `xhci::setup_mouse()` runs once in `kmain`, after
  `discover_boot_disk`. On a USB-booted machine enumeration already ran during
  boot-disk discovery, so it only probes (re-running port reset could disturb
  the open boot disk); on an ATA-booted machine (QEMU) it runs the full
  idempotent enumerate→configure pipeline first.

## Invariants — do NOT regress these

These are the things that, if "tidied" or reverted, silently kill the mouse on
real hardware again. Each is here because it is non-obvious:

1. **Never re-enable BIOS USB legacy emulation to "get the mouse back."**
   Skipping the SMI-disable in `bios_handoff` hard-hangs the controller and
   cannot coexist with us owning xHCI for the boot disk. The mouse must come
   from our HID driver.
2. **Never poll the USB mouse from an interrupt handler.** xHCI transfers
   allocate; the shared `LockedHeap` spinlock in IRQ context caused a hard
   freeze before (see the IRQ-no-heap invariant). Poll from the UI loop only.
3. **Keep the periodic-endpoint Interval in `configure_endpoints`.** The old
   comment said Interval 0 was fine "since usb-storage has no interrupt
   endpoints" — true for MSC, fatal for HID. `xhci_interval` must stay.
4. **PS/2 and USB share one cursor and one queue.** `apply_motion` /
   `feed_mouse_delta` in `ps2.rs` are the single screen-space apply path; both
   sources go through it. Don't fork a second cursor.
5. **The UI idle path must not `hlt` indefinitely when a USB mouse is present.**
   It has no IRQ to wake it; that is what the `mouse_present()` check and the
   8 ms poll delay are for.
6. **`setup_mouse` runs after `discover_boot_disk`** and must not re-reset ports
   on a USB-booted machine (would disturb the open boot disk).

## Progress log

- **Root cause established from code** (no hardware needed): PS/2-only input +
  no HID driver + handoff disables BIOS emulation → zero pointer events on real
  hardware.
- **Driver implemented and confirmed working on the Byd Tracer laptop**: pointer
  moves and clicks. QEMU exercises the same path via a `usb-mouse` added to the
  xHCI bus in `src/main.rs`, so the HID path is regression-tested every
  `cargo run` without needing the laptop.

---

## Cleanup still owed (diagnostic scaffolding — not mouse-specific)

The mouse fix is done; these are leftover **boot/storage** diagnostics from the
Byd Tracer bring-up that should still come out (tracked here so they are not
forgotten). None of these are the mouse driver.

- **`boot/stage1.s`** — `[DIAG-C/D/F]` build: the on-screen `DL=/EXT=/S2N=/RD=`
  dump + sector row + `DIAG_SECS` test read. Restore a clean loader (the proven
  read + handoff is preserved in `Byd Tracer Real Hardware.md` ~lines 461-505).
  The dangling `INVESTIGATION.md` reference in `stage1.s:1` goes away with it.
- **`boot/stage2.s`** — remove `vesa_diag` (DIAG-E) and the `1`/`2`/`3` markers
  (DIAG-D). **Keep** the `vesa_set` largest-first fallback (a real fix).
- **`kernel/src/main.rs`** — `boot_status()` + `BOOT_Y` on-screen milestone
  trace, including the temporary `boot_status("input: USB mouse ready")` line
  added with the mouse fix; the `discover_boot_disk` step markers + device-dump
  block; restore `fatal()`'s full-screen error backdrop. Keep serial logging.
- **`kernel/src/usb/xhci.rs`** — remove the `bu: …` bring-up markers and
  `bu: handoff …` strings; keep the logic they wrapped.

### KEEP — real fixes, not diagnostics
- The **USB-HID boot mouse driver** (this fix).
- xHCI **BIOS→OS handoff** (`bios_handoff`) — minus its on-screen markers.
- stage 2 **VBE largest-first set-mode fallback**.
- **USB boot-disk discovery** (`BootDisk` + `discover_boot_disk`) — minus trace.
- **EP0 / bulk endpoint recovery** in `xhci.rs`.
- **MSC error surfacing** in the UI (`I/O error: <detail>`).
