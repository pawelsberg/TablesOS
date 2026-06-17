# TablesOS single-device boot layout (hybrid BIOS + UEFI)

The pendrive is one device, bootable both ways. The TablesOS volume itself
remains raw, unpartitioned space — the only partition-table entry describes
the EFI System Partition, which exists solely because UEFI firmware can only
start a loader from a FAT file system (SPECIFICATION.md item 8, relaxed
exactly this far):

```
LBA 0            Custom MBR: stage-1 boot code, TBLSBOOT header @0x180,
                 partition table @0x1BE (one type-0xEF entry = the ESP).
LBA 1 .. 63      Stage 2 (real→long mode loader, BIOS path). Reserve 63 sectors.
LBA 64 ..        Kernel image (flat binary, .bss not stored).
LBA <esp>        FAT16 EFI System Partition (4 MiB), 1 MiB-aligned, holding
                 exactly one file: \EFI\BOOT\BOOTX64.EFI (UEFI path).
LBA <data_loc>   TablesOS relational volume (superblock "TBLS" at this LBA),
                 1 MiB-aligned, occupies the rest of the device.
```

## Boot paths

- **BIOS**: firmware runs the MBR boot code → stage 1 reads stage 2 (one
  INT 13h extended read) → stage 2 sets a VESA mode, loads the kernel via
  unreal-mode INT 13h, builds BootInfo + identity paging, enters long mode,
  jumps to the kernel.
- **UEFI** (no Secure Boot): firmware finds the type-0xEF partition, mounts
  the FAT16 ESP and runs `\EFI\BOOT\BOOTX64.EFI` (`uefi-loader/`). The loader
  ignores the file system from then on: it scans Block I/O handles for the
  `TBLSBOOT` header, reads the kernel from its raw LBAs, picks a GOP mode,
  reserves the kernel's RAM footprint, builds the same BootInfo + identity
  page tables, calls ExitBootServices and jumps to the same kernel entry.

Either way the kernel starts at 0x1000000 (16 MiB) with a `BootInfo` pointer
in RDI and cannot tell (nor needs to know) which firmware booted it. The load
address is 16 MiB rather than 2 MiB so the kernel footprint (image + a small
`.bss` fallback heap + stack) stays clear of the low-RAM islands UEFI firmware
needs to keep alive after boot (OVMF parks ACPI NVS at 8 MiB). The kernel's
main heap is reserved separately by the bootloader from free RAM and reported
via `BootInfo` (heap_base/heap_size below) — it is not in `.bss`.

## Custom MBR header (sector 0, offset 0x180). All little-endian.

Version 2: the header moved from its v1 home at 0x1B0 down to 0x180 so the
classic partition table at 0x1BE could come back for the UEFI path.

| Off  | Size | Field            | Meaning                              |
|------|------|------------------|--------------------------------------|
|0x000 | 3    | `jmp short`+nop  | into stage-1 code                    |
|0x180 | 8    | format magic     | ASCII `TBLSBOOT`                     |
|0x188 | 4    | version          | unified product version, packed `(major<<16)\|(minor<<8)\|patch` — the *same* value stamped in the superblock + journal (`tablestore::VERSION`). The builder writes it; stage1.s only reserves the bytes |
|0x18C | 8    | data location    | LBA where the TablesOS volume starts |
|0x194 | 4    | stage2 lba       |                                      |
|0x198 | 2    | stage2 sectors   |                                      |
|0x19A | 2    | kernel mem MiB   | kernel RAM footprint incl. .bss+stack (UEFI loader reserves this at the load address) |
|0x19C | 4    | kernel lba       |                                      |
|0x1A0 | 4    | kernel sectors   | file bytes / 512, rounded up         |
|0x1A4 | 4    | kernel load addr | physical (=0x01000000)               |
|0x1A8 | 4    | kernel entry     | physical (=0x01000000)               |
|0x1AC |16    | **system GUID**  | unique per image (builder-randomized); the running kernel re-reads this and must match the value captured at boot |
|0x1BC | 2    | reserved         |                                      |
|0x1BE |64    | partition table  | entry 1 = ESP (type 0xEF); entries 2-4 zero |
|0x1FE | 2    | 0x55 0xAA        | boot signature                       |

The builder writes an **already-formatted** TablesOS volume at the data
location (no runtime formatting); the kernel only ever *mounts*.

### Version (unified)

There is **one** TablesOS version. It is declared once in the workspace
`Cargo.toml` (`[workspace.package] version`, e.g. `0.1.0`), inherited by every
crate, and stamped — packed as `(major<<16)|(minor<<8)|patch` — into **every**
on-disk version field. There are deliberately no independent format numbers: a
change to the version is assumed to change every on-disk format at once, so a
single comparison distinguishes a foreign or older image.

| Field | Where | Written by | Read / checked by |
|-------|-------|------------|-------------------|
| MBR header version | sector 0 `0x188` (u32) | builder (`tablestore::VERSION`) | uefi-loader (skips a non-matching disk); kernel Drives diagnostic (display) |
| Volume (store) format | superblock `SB_VERSION` | `tablestore::pager` (`crate::VERSION`) | — (migration hook, future) |
| Journal format | journal control header | `tablestore::journal` (`crate::VERSION`) | — (migration hook, future) |

Source of truth: `tablestore::VERSION` (packed u32) / `tablestore::VERSION_STR`
(`"v0.1.0"`). The dependency-free `uefi-loader` re-derives the same packing from
its inherited `CARGO_PKG_VERSION` and must stay byte-compatible. The Drives
diagnostic renders the MBR field with `tablestore::version_string`.

## Fixed addresses (BIOS path)

| Addr        | Use                                            |
|-------------|------------------------------------------------|
|0x00007C00   | BIOS loads stage 1 here                         |
|0x00008000   | stage 1 loads stage 2 here                      |
|0x00010000   | real-mode disk bounce buffer (64 KiB window)    |
|0x00007000   | `BootInfo` handed to the kernel                 |
|0x00070000   | page tables (PML4/PDPT/4×PD, identity 0–4 GiB)  |
|0x00090000   | stage-2 long-mode scratch stack                 |
|0x01000000   | kernel load/run (identity-mapped, static)       |

The UEFI loader allocates BootInfo and its page tables from boot-services
memory instead (below 4 GiB, outside the kernel footprint); only the kernel
load/entry address 0x1000000 is fixed.

## BootInfo — bootloader fills, kernel reads (pointer in RDI)

At 0x7000 on the BIOS path; anywhere below 4 GiB on the UEFI path.

| Off | Size | Field          |
|-----|------|----------------|
|0x00 | 4    | magic 0x5342544F (`OTBS`) |
|0x04 | 4    | fb_width       |
|0x08 | 4    | fb_height      |
|0x0C | 4    | fb_pitch (bytes per scanline) |
|0x10 | 8    | fb_addr (phys) |
|0x18 | 1    | fb_bpp (bytes/pixel) |
|0x19 | 1    | fb_format (0=RGB,1=BGR) |
|0x1A | 6    | reserved       |
|0x20 | 8    | data_location_lba |
|0x28 | 1    | boot_drive (BIOS DL; fixed 0x80 under UEFI) |
|0x29 | 7    | reserved       |
|0x30 |16    | sys_guid (copied from MBR 0x1AC at boot) |
|0x40 | 8    | rsdp_addr — ACPI RSDP physical address, or 0 (BIOS path: kernel scans EBDA/0xE0000 itself; UEFI path: from the EFI configuration table) |
|0x48 | 8    | heap_base — physical base of the kernel heap region the bootloader reserved (free, identity-mapped, below 4 GiB, clear of the kernel footprint), or 0 |
|0x50 | 8    | heap_size — bytes at heap_base, or 0. When 0/invalid the kernel uses a small built-in fallback heap. |

The kernel no longer carries its multi-hundred-MiB heap in `.bss` at the fixed
load address (which collided with firmware-reserved RAM on some laptops —
e.g. an SGIN M15 Pro reserves everything from 256 MiB up). Instead each
bootloader picks a large free block at runtime and reports it here: the UEFI
loader reserves it with `AllocatePages` (so its *placement* dodges any reserved
region), and the BIOS stage 2 finds the largest usable E820 region above the
kernel footprint. The kernel footprint is now just image + stack + a small
fallback heap, so `kernel mem MiB` (0x19A) is tiny.
