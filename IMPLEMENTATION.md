# TablesOS — Implementation Decisions

Governing directives (from this file), honoured throughout:

- **BIOS and UEFI, both with our own loaders** (no third-party crate, no
  Secure Boot). BIOS: a custom 512-byte MBR (stage 1) loads stage 2, which
  sets a VESA linear-framebuffer mode, loads the flat kernel to 16 MiB via
  unreal-mode INT 13h, builds page tables + a `BootInfo`, enters long mode
  and jumps to the kernel. UEFI: our `uefi-loader` crate
  (`\EFI\BOOT\BOOTX64.EFI` on a small FAT16 ESP) does the equivalent with
  GOP + Block I/O + ExitBootServices and jumps to the **same** kernel with
  the same `BootInfo`. On UEFI firmware: disable Secure Boot (the loader is
  unsigned); CSM is no longer needed.
- **No general-purpose partitioning (SPEC item 8, minimally relaxed for
  UEFI)** — one device: `[MBR | stage2 | kernel | FAT16 ESP | TablesOS
  volume]`. The custom MBR header (at 0x180) carries the system id, OS-loader
  format version and the volume's data-location LBA. The classic partition
  table at 0x1BE holds exactly one entry — the type-0xEF EFI System Partition
  that UEFI firmware requires to find the boot loader; the TablesOS volume
  itself stays raw, unpartitioned space and nothing at runtime reads or
  writes the ESP. See `boot/layout.md`.
- **Use Rust** — engine and kernel are Rust (engine `#![forbid(unsafe_code)]`);
  the two boot stages are relocation-free GNU-`as` assembly (no ELF
  assembler/linker is available, so the load base is baked in as a constant
  and `objcopy -O binary` produces the flat sectors — no link step).
- **Use QEMU** — `cargo run` assembles the stages, flattens the kernel ELF,
  lays out the **single** image and boots it as the only disk. The same image
  `dd`s to a whole pendrive.

The rest records how `SPECIFICATION.md` / `UI.md` were realised and — honestly
— where reality is bounded.

## Architecture

| Layer | Crate | Notes |
|---|---|---|
| Relational engine | `tablestore` | `no_std + alloc`, no `unsafe`, host-unit-tested (`cargo test -p tablestore`). |
| Kernel / drivers / GUI | `kernel` | `no_std`, single boot entry for both firmwares, VBE/GOP framebuffer, PS/2 + USB-HID input, ATA, xHCI/USB-MSC, GUI. |
| UEFI loader | `uefi-loader` | `no_std`, dependency-free PE binary; GOP mode, raw-LBA kernel load, identity paging, ExitBootServices. |
| Image builder | workspace root | Assembles stages 1+2, compiles the UEFI loader, builds the FAT16 ESP, lays out one hybrid BIOS+UEFI disk image. |

The engine reaches storage only through `BlockDevice`, so identical
relational/journalling code runs in the kernel and under host tests.

## Data & values

- **Unlimited integers / decimals / years**: stored as their decimal digits.
  The engine performs *no arithmetic* on stored values (the spec has no
  aggregates), so only parse / compare / format / `mod small` are needed —
  making "unlimited" trivially correct and genuinely unbounded.
- **Dates**: proleptic Gregorian, astronomical year numbering (year 0 exists),
  unlimited signed year, Gregorian leap rule enforced.
- **Times**: `[00:00:00, 24:00:00)`, unlimited sub-second digits.
- **Zoned types**: a fixed offset in minutes. Equality/ordering is
  **literal/component-wise**, not instant-based (`12:00+00:00` ≠
  `13:00+01:00`). Instant equality would need cross-day date arithmetic on
  unlimited years, which the no-arithmetic design and the spec do not require.
  Recorded so this is a decision, not an accident.
- `UNIQUE` permits multiple `NULL`s (only non-NULL values must be distinct).
- **Reference columns**: each table carries an ordered `ref_cols` name list
  (`tablestore/src/schema.rs`) used to render a row as a compact label
  (`Table::reference_label`, e.g. `id:1 name:John`). The list is **appended
  last** in the schema encoding so a pre-feature on-disk image (which ends
  right after the FKs) decodes to an empty list — backward compatible without a
  version bump. `Table::ref_col_indices` resolves the effective columns: the
  configured names if set, else the `UNIQUE` columns, else the first column.
  Drop/rename of a column maintain `ref_cols` in the same transaction (drop
  removes it, rename follows it), so the label set never dangles.
- Foreign keys: source/target types must match, target must be `UNIQUE`,
  adding a key validates existing rows, and delete/drop is **RESTRICT** with a
  surfaced reason — never a silent destructive change.

## Storage, durability, SSD-safety

- 4 KiB pages over 512 B sectors: superblock (page 0), journal region, then
  catalog / per-table schema / per-row blob chains / free list.
- **Format versions are independent of the release version.** Three on-disk /
  wire formats each carry their own version (all `1` today): the OS/loader
  version in the MBR header (`boot/stage1.s`, surfaced by the Drives
  diagnostic), the volume/superblock `SB_VERSION` (`pager.rs`), and the journal
  record version (`journal.rs`). None tracks the crates' `0.1.0` product
  version; each is bumped only when its own layout changes (see
  `boot/layout.md`).
- A value/row is a **blob chain** of linked pages, so a single value's size is
  bounded only by free space — *except* by the journal limit below.
- **Write-ahead journal**, physical page-image redo. Per transaction: stage
  images + control `EMPTY` → flush → flip control to `COMMITTED` → flush
  (**atomic commit point**) → install at home → flush → control `EMPTY`.
  Recovery idempotently replays a `COMMITTED` journal, so power loss costs
  **at most the last uncommitted transaction**. CRC-32 rejects a torn journal.
  Unit-tested incl. a simulated power cut (`MemBlockDevice::cut_after`).
- **SSD-safe**: large aligned writes, no in-place rewrite outside a journalled
  transaction, frees deferred to commit, and the ATA driver issues
  `CACHE FLUSH`. No read-modify-write storm, no per-byte metadata hot-spot.

## Known, deliberate bounds (where "unlimited" meets a constant)

1. **Per-transaction write-set ≤ ≈2044 pages (≈8 MiB).** Each engine op is one
   transaction, so a single inserted/updated row plus the structural pages it
   touches must fit the journal region — the standard WAL-segment trade-off,
   set by one constant in `journal.rs`. Schema changes rewrite a table's rows
   in one transaction and are bounded the same way.
2. **ATA LBA28** → kernel store volumes ≤ 128 GiB (LBA48 is a localised change
   in `ata.rs`); the engine itself addresses up to 2⁶⁴ pages.
3. **USB pendrive vs ATA.** It is now a true **single device**: BIOS boots
   the disk, and the kernel drives that *same* disk (primary IDE **master**)
   via **ATA PIO**. The TablesOS volume lives at the MBR's data-location LBA;
   the ATA driver hides that base offset, so the engine sees a volume starting
   at sector 0 and can never reach the boot/kernel region in front of it.
   Correct for the QEMU IDE disk and legacy SATA-in-IDE-mode hardware. The
   bare-metal USB stack exists for **both** controller generations behind the
   same `BlockDevice` seam: xHCI (`usb/xhci.rs`, full stack incl. HID mouse
   and install-to-USB) and EHCI (`usb/ehci.rs`, boot disk + HID keyboard/mouse
   for pre-xHCI machines). EHCI does hub enumeration (chipset Rate-Matching
   Hubs) and split transactions for the Full/Low-Speed kbd/mouse behind that
   HS hub; HID is polled from the UI loop via GET_REPORT into the same input
   queue as PS/2 (the EHCI BIOS→OS handoff disables the firmware's SMM
   keyboard emulation, so the kernel must drive HID itself). Shared BOT/SCSI
   wire formats live in `usb/bot.rs`. The Drives screen and install-to-USB
   remain xHCI-only.

   **No runtime formatting + disk-identity gate (accidental-overwrite
   safety).** The builder writes an *already-formatted* volume into the
   image and stamps a unique random 16-byte **system GUID** into the MBR
   (offset 0x1AC). At boot, the bootloader copies that GUID into `BootInfo`; the
   kernel re-reads sector 0 from the ATA device and **refuses to proceed
   unless the on-disk GUID equals the one captured in memory at boot**. There
   is *no* `Store::format` path in the kernel at all — a failed mount or a
   GUID mismatch is fatal and performs **zero writes**. Net: the OS can only
   ever read/write the exact disk it was booted from; a different/foreign
   disk in the IDE slot is detected and left untouched (verified: a two-disk
   test where the kernel's ATA target ≠ the booted disk halts with
   "disk identity mismatch" and the other disk is byte-identical afterwards).

   **Identity is checked once at boot.** The ATA driver targets a fixed IDE
   slot and the disk there cannot change at runtime, so a single check
   suffices. The **future USB-mass-storage driver** does not get that for
   free: a surprise-remove + re-attach can land a different device at the
   same logical slot. **Constraint on that driver**: re-read MBR sector 0
   and re-verify the captured `sys_guid` after every device reset / port
   re-enumeration, before letting the next write through; on mismatch fail
   closed (refuse all further I/O) just like the boot-time gate. This keeps
   the "writes only ever touch the booted disk" invariant under hot-swap.
4. **Font**: the UTF-8 *pipeline* is complete (decode to scalar values, per
   code-point glyph lookup); the built-in glyph set is ASCII (public-domain
   `font8x8`) plus the arrows/dashes/ellipsis the UI uses. Non-ASCII renders as
   a replacement box without breaking layout; wider coverage is added purely as
   glyph data. An **optional higher-resolution glyph atlas** (`kernel/assets/font.png`)
   replaces the 8×8 face when present — see *Bitmap assets* below; absent it,
   `µ` and `×` (used in a couple of UI strings) currently show as the box.
5. **Keyboard**: US scancode set 1. UTF-8 is stored/compared/displayed fully;
   on-screen *entry* is limited to what the keymap produces.
6. **Shutdown** uses QEMU/Bochs ACPI poweroff I/O ports, then halts; general
   ACPI/APM poweroff on arbitrary hardware is out of scope.
7. **Framebuffer resolution.** Our stage 2 enumerates VBE modes and picks the
   **highest-area** 32-bpp linear mode that is ≥ 1024×720 (it observed
   2560×1600 under QEMU). The 1280×720 cap of the old third-party bootloader
   is gone. If no acceptable mode exists, stage 2 halts (boot fails) per the
   spec; the kernel re-checks and also hard-fails on a bad/absent mode.
8. **No graphics mode / no disk** → boot fails with a message (serial, and
   on-screen when possible), as `UI.md` requires; the UI never runs degraded.

## Input (PS/2 + USB-HID mouse)

Two pointer/key sources feed **one** cursor and **one** event queue (`ps2.rs`):

- **PS/2** (8042): IRQ1/IRQ12 → ports 0x60/0x64. Serves QEMU and any genuine
  PS/2 device (a laptop's internal keyboard is usually wired to the EC as
  i8042, so the keyboard keeps working even when the pointer is USB).
- **USB-HID boot mouse** (`usb/xhci::probe_hid_mouse`/`setup_mouse`/`pump_mouse`):
  the same xHCI enumerate/configure machinery as USB-MSC, plus a boot-protocol
  mouse interface (class 0x03 / subclass 0x01 / protocol 0x02). `SET_PROTOCOL(0)`
  gives a fixed 3-byte report `[buttons, dX, dY]` (no report-descriptor parsing);
  one interrupt-IN TRB is kept armed and polled cooperatively from the UI loop,
  feeding `ps2::feed_mouse_delta` so the UI drains it like any PS/2 event.

**Why USB-HID exists at all (and must not be removed):** on real hardware the
USB boot disk forces a BIOS→OS xHCI ownership handoff that switches off the
BIOS's SMM PS/2 emulation of USB pointers — so without our own HID driver the
mouse is dead. Owning xHCI for the boot disk and keeping BIOS HID emulation are
mutually exclusive. Full root-cause + the **regression invariants** (don't
re-enable BIOS emulation; never poll USB from an IRQ — the heap-free-IRQ rule;
keep the periodic-endpoint Interval in `configure_endpoints`; one shared cursor;
the UI idle path can't `hlt` forever when a USB mouse is present) are in
`solved-issues/USB mouse on real hardware.md`. A `usb-mouse` is attached to the
QEMU xHCI bus (`src/main.rs`) so this path is exercised on every `cargo run`.
A USB-HID **keyboard** is a cheap follow-on (protocol 0x01) but is not yet
implemented; PS/2 still serves the keyboard everywhere we've run.

## Verified

Built and run end-to-end (QEMU, BIOS, **single device, custom MBR, no
partitions/FAT**): `cargo test -p tablestore` = 17/17 (incl. simulated
power-loss recovery). The one image boots custom-MBR → stage2 → VESA
(2560×1600) → long mode → flat kernel at 2 MiB → GDT/IDT/PS-2/ATA →
**verifies the MBR system GUID** → **mounts** (never formats) the
already-formatted volume at its base LBA → GUI; creating a table via the
keyboard and **rebooting the same image** shows the table still present
(journalled volume persists, base-offset keeps it clear of the boot/kernel
region). The identity gate was verified both ways: matching GUID → mounts;
booted-disk ≠ ATA-target disk → halts with no writes, foreign disk
byte-identical. Bring-up fixes: reset `SS/DS/ES` after our GDT (stale selectors →
#GP); `add_column` forbids `NOT NULL` only when rows exist; relocation-free
flat asm + GNU `objcopy` for stages (llvm-objcopy mis-sizes COFF), llvm-objcopy
for the ELF kernel; VBE pixel-format from `RedFieldPosition` (mode-info 0x20).

## Rendering

The framebuffer is **double-buffered**: all drawing goes to an off-screen
scene buffer; a full repaint is one contiguous copy to the hardware
framebuffer (no clear-then-draw flicker). The mouse pointer is composited on
top using the clean scene as restore source, so pointer motion only touches a
small rectangle and never repaints the scene or re-reads the store. The
embedded font carries every code point the UI renders (ASCII plus
`— … ← ↑ → ↓ ⮡`); nothing shows as the replacement box.

Build note: Cargo builds the `kernel` artifact dependency with the **dev**
profile even under `--release` (a `-Z bindeps` quirk). Per-package
`opt-level` overrides in the root `Cargo.toml` keep the kernel/engine
optimised in every profile, so the image stays ~2.5 MB and the BIOS
bootloader loads it quickly (an unoptimised kernel is multi-MB and stalls at
"loading kernel...").

## Bitmap assets (PNG)

Optional bitmaps live in `kernel/assets/` and are **embedded at build time**
(`kernel/build.rs` `include_bytes!`-es whatever is present, missing files
compile to `None`) and **decoded at boot** by the `pngdec` crate
(`no_std + alloc`, no `unsafe`, host-tested `cargo test -p pngdec` = 5/5;
DEFLATE inflate + PNG filters → RGBA, color types 0/2/3/6 at bit-depth 8,
non-interlaced). Embedding the *compressed* PNG and decoding to RAM keeps the
kernel image small — the size budget is real (the BIOS loader stalls on a
multi-MB kernel), so per-view art must stay tightly-paletted (the asset cap is
~900 KB total). There is **no filesystem**, so build-time embed is the only
source; in-kernel decode is what makes "the OS reads PNGs" true.

- **Backgrounds** — `bg_<view>.png` (or a shared `bg.png` tinted per view) are
  bilinear-stretched to fill the framebuffer behind the HUD, replacing the
  procedural gradient+starfield for that view. Any view without an asset falls
  back to procedural, so the UI never depends on assets being present. The
  composited background is **cached** (scene-sized) and keyed by view, so a
  repaint is a memcpy rather than a full-screen recompute — a 2560×1600
  bilinear compose measured ~360 ms under QEMU/TCG vs ~8 ms for the cached
  copy, which is what keeps keystroke repaints snappy. The cache plus the
  scene buffer is why the heap is 64 MiB (`allocator.rs`).
- **Font atlas** — `font.png` is a 16×7 grid of equal cells (white-on-black
  coverage); glyphs are area-averaged to the on-screen cell size, tinted with
  the existing per-run gradient, and alpha-blended over the background. Blank
  in-range cells fall back to the 8×8 face per glyph.

**Font licensing (SIL OFL 1.1 compliance).** The shipped `font.png` is a
bitmap **Modified Version** of **Cascadia Mono** (Copyright © 2019 – Present,
Microsoft Corporation, with Reserved Font Name *Cascadia Code*; SIL Open Font
License 1.1). OFL clause 2 requires the copyright notice and the full license
to accompany *every* redistributed copy in a user-viewable form, so they are
not left as a loose source file: `kernel/assets/FONT_LICENSE.txt` (the verbatim
notice + full OFL text) is `include_str!`-embedded into the kernel
(`assets.rs::FONT_LICENSE`), hence travels inside the flat kernel and the
bootable image; it is rendered in-OS on the **About / Licenses** screen
(Table List → `a`) and the attribution is also logged to serial at boot. The
Reserved Font Name is respected (clause 3): the derivative is named the
"TablesOS glyph atlas" and is never presented to users as "Cascadia" — the name
appears only as upstream acknowledgement (clause 4). To swap the atlas, render
only from an OFL/permissive face and ship its license (see `ASSET_PROMPTS.md`
§2.3); never commit one rendered from a proprietary font (Consolas, Lucida
Console, …).

The graphical-AI prompts and the exact asset contract (filenames, dimensions,
formats, palette, charset/cell layout, size caps) are in `ASSET_PROMPTS.md`.
Verified end-to-end in QEMU: `bg_list.png` + the Cascadia-Mono-rendered
`font.png` decoded in-kernel and rendered on the 2560×1600 framebuffer; with
`assets/` empty the build is green and the procedural look is used.

## GUI

Keyboard-driven (every action has a key) with a working mouse pointer that can
click list/grid rows and modal options. All `UI.md` screens are implemented
and colour-coded: Table List, Table Browser (cell cursor; insert/update/delete;
schema; FK cells show the referenced row's reference label), Row View (full
untruncated values, FK fields annotated with the target's reference label, FK
navigation, "referenced by", opening a *filtered* browser), Row Editor (per
field, `[ ] NULL` toggle, type shown, validation on commit with inline errors),
Schema Editor (add/drop/rename column, reorder, toggle UNIQUE, add/drop FK, set
reference columns, drop table).

- **Reference columns editor.** The Schema Editor's `R` opens a picker
  (`Screen::RefCols` in `ui.rs`) listing every column with its rank in the
  label (`[1]`, `[2]`, …); `Enter`/`Space` toggles membership, `,`/`.` reorder
  the selected column within the label, `c` clears to the automatic default.
  Each change commits immediately through `Store::set_reference_columns` (one
  transaction, like the other schema edits), and a live "Label order" line
  previews the result. The resolved labels then appear in the Browser FK
  cells, the Row View FK fields, and the Row View relationships list — all
  driven by `Table::reference_label`. Destructive actions
use a centred confirm modal defaulting to cancel; errors appear on the status
bar / inline and never block reading the data.

- **Column rename** (`Store::rename_column`) is one transaction: the column
  definition and every foreign key that names it — the source FK in this table
  and any `to_col` pointing here from this or another table — are rewritten
  together, so references never dangle. Rows are untouched (cells are
  positional). Rejected on an empty or colliding new name.
- **Structured field builder.** In the Row Editor any field can be entered
  part-by-part: `→` opens a builder (`FieldBuilder`/`PartId` in `ui.rs`) with
  one labelled field per component. The parts are type-driven — a number's
  sign/digits (plus integer-part/fraction for `decimal`), a string's single
  text field, or the calendar/clock components for the date/time types (year,
  month, day, hour, minute, second, sub-second, and the zone offset's
  sign/hours/minutes for the zoned types). It composes the canonical string,
  validates through the same `Value::parse`, shows a live preview, and writes
  the result back into the field — so the builder never bypasses the engine's
  validation. For the date/time types only, `[n]` fills the **current**
  date/time from the legacy CMOS RTC (`kernel/src/rtc.rs` — a polled, NMI-safe
  read of ports 0x70/0x71, BCD/12-hour aware, with a stable double-read; we
  never set the clock or enable its IRQ). That wall clock is presented with a
  zero zone offset and is editable before commit, so a clock kept in local time
  rather than UTC is a starting point, never a silent error.

## Build / run

See `BUILD.md`. `cargo test -p tablestore` verifies the engine, including
crash recovery, on the host with no emulator.
