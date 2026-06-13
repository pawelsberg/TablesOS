# Building and running TablesOS

## Prerequisites

- A **nightly** Rust toolchain. `rust-toolchain.toml` pins it and lists the
  required components (`rust-src`, `llvm-tools-preview`) and the bare-metal
  targets `x86_64-unknown-none` (kernel) and `x86_64-unknown-uefi` (UEFI
  loader). `rustup` installs them automatically on first build.
- [QEMU](https://www.qemu.org/) (`qemu-system-x86_64`) to run it without real
  hardware. Optional if you only want the flashable image.

## Layout

| Crate          | What it is                                                          |
|----------------|---------------------------------------------------------------------|
| `tablestore`   | The relational engine: types, bignum, schema, paging, journal. `no_std + alloc`, host-testable. |
| `kernel`       | The OS itself: boot entry, framebuffer, PS/2 input, ATA + USB disk, GUI. `no_std`. |
| `uefi-loader`  | Our UEFI boot loader (`BOOTX64.EFI`): GOP mode, raw-LBA kernel load, identity paging, ExitBootServices, kernel handoff. `no_std`, no dependencies. |
| (workspace root) | Image builder/runner. Assembles our custom MBR + stage 2, compiles the UEFI loader, flattens the kernel ELF, lays out one hybrid device image, boots it in QEMU. |

## Commands

```sh
# Build the single-device image and boot it in QEMU (legacy BIOS).
cargo run

# Same image, booted through UEFI firmware (OVMF/EDK2, ships with QEMU).
cargo run -- --uefi

# Only build the flashable image (no QEMU).
cargo run -- --no-run

# Build an image pre-loaded with a test dataset, then boot it. Add --no-run to
# just write the seeded image. See "Test data" below.
cargo run -- --seed
cargo run -- --seed --no-run

# Run the engine's unit tests on the host (no emulator needed).
cargo test -p tablestore
```

## Test data (`--seed`)

`--seed` fills the volume with a representative, foreign-key-linked dataset
before it is laid into the image, so the GUI comes up populated:

| Table       | Rows | Columns                                                                                | Reference label      |
|-------------|------|----------------------------------------------------------------------------------------|----------------------|
| `addresses` | 100  | `id` (unique), `name`                                                                  | `name`               |
| `people`    | 200  | `id` (unique), `firstname`, `surname`, `address_id` → `addresses.id`                   | `firstname` `surname`|
| `distances` | 20   | `id` (unique), `from_address_id` / `to_address_id` → `addresses.id`, `distance_miles`  | `distance_miles`     |
| `notes`     | 500  | `id` (unique), `note`, `timestamp`                                                     | (default)            |

The **reference label** is the ordered set of columns shown wherever a row
appears as a reference (FK cells in the Browser, FK fields and relationships in
Row View) — so e.g. `people.address_id` renders as `name:High Road` rather than
the raw id.

Seeding happens at build time (in `src/seed.rs`, via the same `tablestore`
engine the kernel runs), so it survives the image rebuild that every `cargo
run` performs. A plain `cargo run` builds an **empty** volume; pass `--seed`
whenever you want the data. The dataset logic lives in
[`src/seed.rs`](src/seed.rs) — edit there to change counts or columns.

The bootable image is written to `target/tablesos.img`. It is **one hybrid
device** — `[custom MBR | stage 2 | kernel | FAT16 ESP | TablesOS volume]` —
bootable by both BIOS and UEFI firmware. The only partition-table entry is the
EFI System Partition the UEFI spec requires; the TablesOS volume stays raw
(SPECIFICATION.md item 8 / `boot/layout.md`). Flash the whole thing to a
pendrive:

```sh
dd if=target/tablesos.img of=/dev/sdX bs=4M conv=fsync
```

`of=/dev/sdX` must be the **whole device**, not a partition. TablesOS owns the
entire pendrive: a small boot/kernel prefix, then the table-store volume for
the rest of the device.

## Windows (windows-gnu host) note

The builder shells out to the **GNU `as` + `objcopy`** (to assemble the boot
stages → flat binary) and **`llvm-objcopy`** (to flatten the kernel ELF;
shipped with the Rust `llvm-tools` component). `as`/`objcopy` come from MSYS2
MinGW. Put them on `PATH` (or set the `AS` / `OBJCOPY` env vars):

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;C:\msys64\mingw64\bin;$env:Path"
cargo run
```

QEMU is auto-discovered (or set the `QEMU` env var). The kernel links with
`rust-lld` (ELF) using `kernel/linker.ld`; the boot stages need no linker
(relocation-free flat asm). With the `windows-gnu` toolchain no Visual Studio
is required.

## Boot firmware

TablesOS boots via **legacy BIOS** (custom MBR + stage 2) and via **UEFI**
(`\EFI\BOOT\BOOTX64.EFI` on the image's FAT16 ESP — see `boot/layout.md`).
Secure Boot is **not** supported: the loader is unsigned, so disable Secure
Boot in firmware setup before booting from the pendrive. CSM/"Legacy Boot" is
no longer required on UEFI-only machines.

## Storage note

The store is exposed through `tablestore::BlockDevice`, always on the **same
disk the machine booted from**, found in this order:

1. **ATA PIO** (primary IDE master) — QEMU's IDE disk, legacy SATA-in-IDE mode.
2. **xHCI + USB mass storage** — real pendrives on USB 3.x controllers
   (modern machines, BIOS or UEFI firmware).
3. **EHCI + USB mass storage** — pre-xHCI machines (≈2008-2012) whose ports
   are wired to the chipset USB 2.0 controller; includes standard hub
   enumeration because those chipsets put a Rate-Matching Hub in front of
   every port. High-Speed devices only (no split transactions).

The TablesOS volume begins at the data-location LBA recorded in the custom
MBR; each driver adds that base offset transparently, so the engine sees a
volume at sector 0 and cannot touch the boot/kernel prefix, and every USB
write is identity-gated to the booted disk's system GUID and issued with FUA
for durability.
