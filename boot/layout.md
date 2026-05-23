# TablesOS single-device boot layout (no partitions, no FAT)

Per SPECIFICATION.md item 8: **no partition table, no partitions, no FAT**.
The pendrive is one raw device:

```
LBA 0            Custom MBR / stage 1 (512 B). NOTHING at 0x1BE (no part. table).
LBA 1 .. 63      Stage 2 (real→long mode loader). Reserve 63 sectors.
LBA 64 ..        Kernel image (flat binary, .bss not stored).
LBA <data_loc>   TablesOS relational volume (superblock "TBLS" at this LBA),
                 1 MiB-aligned, occupies the rest of the device.
```

## Custom MBR header (sector 0). All little-endian.

The classic partition-table area (0x1BE) is **deliberately repurposed** for the
header — there is no partition table.

| Off  | Size | Field            | Meaning                              |
|------|------|------------------|--------------------------------------|
|0x000 | 3    | `jmp short`+nop  | into stage-1 code                    |
|0x1B0 | 8    | format magic     | ASCII `TBLSBOOT`                     |
|0x1B8 | 2    | os bios version  | OS/BIOS-loader version (=1)          |
|0x1BA | 2    | reserved         |                                      |
|0x1BC | 8    | data location    | LBA where the TablesOS volume starts |
|0x1C4 | 4    | stage2 lba       |                                      |
|0x1C8 | 2    | stage2 sectors   |                                      |
|0x1CA | 2    | reserved         |                                      |
|0x1CC | 4    | kernel lba       |                                      |
|0x1D0 | 4    | kernel sectors   | file bytes / 512, rounded up         |
|0x1D4 | 4    | kernel load addr | physical (=0x00200000)               |
|0x1D8 | 4    | kernel entry     | physical (=0x00200000)               |
|0x1DC |16    | **system GUID**  | unique per image (builder-randomized); the running kernel re-reads this and must match the value captured at boot |
|0x1FE | 2    | 0x55 0xAA        | boot signature                       |

The builder writes an **already-formatted** TablesOS volume at the data
location (no runtime formatting); the kernel only ever *mounts*.

## Fixed addresses

| Addr        | Use                                            |
|-------------|------------------------------------------------|
|0x00007C00   | BIOS loads stage 1 here                         |
|0x00008000   | stage 1 loads stage 2 here                      |
|0x00010000   | real-mode disk bounce buffer (64 KiB window)    |
|0x00007000   | `BootInfo` handed to the kernel                 |
|0x00070000   | page tables (PML4/PDPT/4×PD, identity 0–4 GiB)  |
|0x00090000   | stage-2 long-mode scratch stack                 |
|0x00200000   | kernel load/run (identity-mapped, static)       |

## BootInfo (at 0x7000) — bootloader fills, kernel reads (pointer in RDI)

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
|0x28 | 1    | boot_drive     |
|0x29 | 7    | reserved       |
|0x30 |16    | sys_guid (copied from MBR 0x1DC at boot) |
