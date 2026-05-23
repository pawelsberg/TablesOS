# TablesOS

A bootable, BIOS-booted **x86-64 operating system whose entire interface is a
relational table store** — written in Rust. It boots from bare metal (or QEMU),
brings up its own two-stage bootloader, a VESA-framebuffer GUI, PS/2 keyboard +
mouse and an ATA disk driver, and presents a browser/editor for tables, rows,
schemas and foreign keys backed by a write-ahead-journalled, crash-safe store.

There is **no partition table and no filesystem** — the whole device is one raw
image: `[custom MBR | stage 2 | flat kernel | TablesOS volume]`.

## Highlights

- **Relational engine** (`tablestore`): `no_std + alloc`, no `unsafe`,
  unlimited-precision numeric/date/time values, `UNIQUE` and foreign keys, and a
  physical write-ahead journal with power-loss recovery (host-tested).
- **From-scratch boot**: a custom 512-byte MBR + stage 2 (relocation-free
  assembly, no third-party bootloader) sets a VESA linear framebuffer, enters
  long mode and jumps to a flat kernel at 2 MiB.
- **Keyboard- and mouse-driven GUI**: Table List, Table Browser, Row View, Row
  Editor, Schema Editor — with foreign-key navigation, reference-column labels,
  and an in-OS **About / Licenses** screen.
- **Disk-identity safety**: the kernel stamps a unique system GUID into the
  image and refuses to read or write any disk whose on-disk GUID differs from
  the one it booted with — so it can only ever touch the disk it booted from.

## Download & run

Grab the latest prebuilt image from the
[**Releases**](https://github.com/pawelsberg/TablesOS/releases) page
(`tablesos.img.gz`, ~4 MiB compressed; ~64 MiB once expanded).

```sh
# Verify the download, then decompress.
sha256sum -c SHA256SUMS.txt
gunzip tablesos.img.gz
```

**In QEMU** (no real hardware needed):

```sh
qemu-system-x86_64 -machine pc -m 1024M \
  -drive format=raw,file=tablesos.img,if=ide,index=0,media=disk
```

**On real hardware** — flash the *whole device* (not a partition) to a
pendrive, then boot it:

```sh
dd if=tablesos.img of=/dev/sdX bs=4M conv=fsync   # /dev/sdX = the whole disk
```

> ⚠️ `dd` overwrites the entire target device. Pick the right `/dev/sdX`.
> TablesOS boots via **legacy BIOS**, so enable "Legacy Boot" / "CSM" and
> disable Secure Boot in firmware. Once running, it only ever writes to the
> disk it booted from (enforced by the system-GUID check).

## Build from source

You need a nightly Rust toolchain (pinned by `rust-toolchain.toml`) and, for
running, QEMU. See [BUILD.md](BUILD.md) for the full toolchain notes (including
the Windows MSYS2 `as`/`objcopy` requirement).

```sh
cargo run                 # build the single-device image and boot it in QEMU
cargo run -- --no-run     # only build the flashable image (target/tablesos.img)
cargo test -p tablestore  # run the engine's unit tests on the host
```

## Documentation

| Doc | Contents |
|-----|----------|
| [SPECIFICATION.md](SPECIFICATION.md) | What TablesOS is and the rules it follows. |
| [IMPLEMENTATION.md](IMPLEMENTATION.md) | How it was realised, and the deliberate bounds. |
| [UI.md](UI.md) | Every screen and interaction. |
| [BUILD.md](BUILD.md) | Building, running and flashing. |
| [boot/layout.md](boot/layout.md) | The on-disk + MBR-header + BootInfo contract. |
| [ASSET_PROMPTS.md](ASSET_PROMPTS.md) | The embedded bitmap assets and how to regenerate them. |

## License

- **Code:** [MIT](LICENSE) © 2026 Pawel Welsberg.
- **Bundled font:** the on-screen glyph atlas (`kernel/assets/font.png`) is a
  bitmap derivative of **Cascadia Mono** (© Microsoft Corporation, Reserved Font
  Name *Cascadia Code*), under the **SIL Open Font License 1.1** — see
  [kernel/assets/FONT_LICENSE.txt](kernel/assets/FONT_LICENSE.txt).

Both license texts are embedded in the kernel and viewable in the running OS
via **Table List → `a` (About / Licenses)**.
