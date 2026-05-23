# Building and running TablesOS

## Prerequisites

- A **nightly** Rust toolchain. `rust-toolchain.toml` pins it and lists the
  required components (`rust-src`, `llvm-tools-preview`) and the bare-metal
  target `x86_64-unknown-none`. `rustup` installs them automatically on first
  build.
- [QEMU](https://www.qemu.org/) (`qemu-system-x86_64`) to run it without real
  hardware. Optional if you only want the flashable image.

## Layout

| Crate          | What it is                                                          |
|----------------|---------------------------------------------------------------------|
| `tablestore`   | The relational engine: types, bignum, schema, paging, journal. `no_std + alloc`, host-testable. |
| `kernel`       | The OS itself: BIOS entry, VESA framebuffer, PS/2 input, ATA disk, GUI. `no_std`. |
| (workspace root) | Image builder/runner. Assembles our custom MBR + stage 2, flattens the kernel ELF, lays out one raw device image, boots it in QEMU. |

## Commands

```sh
# Build the single-device image and boot it in QEMU.
cargo run

# Only build the flashable image (no QEMU).
cargo run -- --no-run

# Run the engine's unit tests on the host (no emulator needed).
cargo test -p tablestore
```

The bootable image is written to `target/tablesos.img`. It is **one raw
device** — `[custom MBR | stage 2 | kernel | TablesOS volume]`, no partition
table, no FAT (SPECIFICATION.md item 8 / `boot/layout.md`). Flash the whole
thing to a pendrive:

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

TablesOS boots via **legacy BIOS**, not UEFI (per `IMPLEMENTATION.md`). On a
modern machine, enable "Legacy Boot" / "CSM" and disable Secure Boot in
firmware setup, then boot from the pendrive.

## Storage note

The store is exposed through `tablestore::BlockDevice`. The kernel implements
it with an **ATA PIO** driver on the **same disk it booted from** (primary IDE
master). The TablesOS volume begins at the data-location LBA recorded in the
custom MBR; the driver adds that base offset transparently, so the engine sees
a volume at sector 0 and cannot touch the boot/kernel prefix. This covers the
QEMU IDE disk and legacy SATA-in-IDE-mode hardware. A USB mass-storage stack
(xHCI/EHCI + USB + Bulk-Only Transport) for booting from a USB pendrive on bare
metal is the one documented seam: it drops in behind the same `BlockDevice`
impl with no engine or GUI changes.
