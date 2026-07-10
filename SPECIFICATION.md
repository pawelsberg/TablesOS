# TablesOS — System Specification

## Overview

TablesOS is an operating system. It boots directly from a USB
flash drive ("pendrive"), takes full control of the machine, and presents the
user with one thing: a relational store of **tables** and the **relationships**
between them.

There is no general-purpose file system, no user processes, no shell scripting.
The entire device is one custom-formatted volume.

Characteristics:

1. **Boot from any pendrive**, regardless of size, on commodity x86-64 hardware
   — both legacy BIOS and UEFI firmware (no Secure Boot).
2. **All capacity is data capacity.**
3. **Unlimited values.** Integers, decimals, strings and dates have no built-in
   width limit; a single value may take arbitrarily big space.
4. **Relational integrity.** Foreign keys connect tables and are enforced.
5. **Crash safety.** The pendrive may be removed at any instant; the store is
   never left structurally inconsistent.
6. **SSD safe** system uses SSD in a safe manner - taking into account nature of
   flash memory. Preventing physical damage of the medium.
7. **Graphical user interface** (VESA on BIOS, GOP on UEFI) allows to interact
   with operating system.
8. **No general-purpose partitioning.** Custom MBR — contains: system id,
   operating system instance version, data location. The data volume is raw,
   unpartitioned space. Sole exception (required by the UEFI specification to
   boot at all): one partition-table entry describing a small FAT16 EFI System
   Partition that carries only the boot loader; nothing at runtime reads or
   writes it.

---

## Data model

The store holds **tables**. A table has an ordered list of **columns**; each
column has a name and a **type** and may be nullable. A table holds an
unordered multiset of **rows**. A column may carry a **foreign key** that references a column in another (or the same) table.

There is no implicit primary key. A foreign key may target any column that
carries a uniqueness guarantee (a `UNIQUE` column).

A table may also define an ordered set of **reference columns**: the columns
used to render one of its rows as a compact label wherever that row appears as
a reference (for example `id:1 name:John`). If none are configured the system
falls back automatically to the table's `UNIQUE` columns, or the first column
if there are none.

---

## Type system

| Type name | Logical domain |
|---|---|
| `integer` | Signed integer, unlimited magnitude |
| `unsigned integer` | Non-negative integer, unlimited magnitude |
| `decimal` | Signed decimal, unlimited digits and scale |
| `string` | UTF-8 text, unlimited length |
| `date` | Calendar date, no time zone, unlimited year range |
| `date tz` | Calendar date with a fixed zone offset |
| `time` | Time of day `[00:00:00, 24:00:00)`, unlimited sub-second precision |
| `date time` | `date` + `time`, no time zone |
| `date time tz` | `date time` with a zone offset |

---

## Foreign keys and relationships

### Declaration

A column may declare a foreign key referencing a unique column of any table,
including its own table (self-reference). The referenced column must be `UNIQUE`;
this is checked at definition time.

---

### Browsing related rows

The GUI shows relationships when browsing. A referenced row is shown by its
**reference label** (its reference columns, formatted `col:value …`), so
following a foreign key or scanning the "referenced by" list shows the related
row's identity directly rather than a bare key.

---

### Schema operations

| Operation | Effect |
|---|---|
| `Create table <name>` | Create an empty table. |
| `Drop table` | Delete a table and all its rows and FKs. |
| `Add column <name> <type> [null\|not null] [unique]` | Append a column. Existing rows get NULL (column must be nullable) . |
| `Drop column <name>` | Remove a column. Rejected if it carries or is targeted by a FK. |
| `Rename column <name> <new-name>` | Rename a column; any foreign key naming it is updated to match. |
| `Move column <name> <new-location>` | Reorder column. |
| `Add fk <col> -> <table>.<col>` | Define a foreign key. |
| `Drop fk <fk-name>` | Remove a foreign key |
| `Add unique` | Add unique to a column |
| `Remove unique` | Remove unique from a column |
| `Set reference columns` | Choose the ordered columns used to label a row when it appears as a reference. |
| `Set column width` | Set a column's display width. |


### Data Operations

| Operation | Effect |
|---|---|
| `Insert` | Insert a row. |
| `Update a row` | Update a row. |
| `Delete a row` | Delete a row. |
| `Order rows` | Sort the rows. |
| `Navigate back and forth` | Navigate relationships. |

### Maintenance Operations

| Operation | Effect |
|---|---|
| `shutdown` | Shut down the computer. |
| `New operating system on USB storage device` | Create a new (empty) TablesOS operating system on another USB storage device. |
| `Top up version` | Upgrade another TablesOS device in place to the running version, keeping its data. |
| `Keyboard layout` | Choose the active keyboard layout. |
| `About` | Show system information. |

---

## Durability and crash recovery

The pendrive may be removed at any moment. The store is **copy-on-write**: a
commit writes every changed page to a fresh physical location and then
atomically publishes the new state; the previous state stays intact until that
instant. After any power loss, mounting recovers a consistent store with at
most the last uncommitted transaction lost.

Because each write lands on a fresh location chosen across the whole device,
wear is spread evenly and no flash hot spots form.

---

## Boot medium identity

During boot the system reads the **unique id** of the medium (in MBR) it is booting
from. Once boot has finished, the system guarantees that every write is
directed only to the disk carrying the unique id captured during boot; a
foreign or swapped disk is never written to. The sole exception is the
explicit *New operating system on USB storage device* maintenance operation,
which deliberately writes a fresh system to a different, user-chosen device.

---

## Versioning and upgrade

TablesOS has a single version, stamped into every on-disk structure. A device
carrying an older version can be **topped up** in place from a running system:
its data is migrated, step by step, to the running version. Only devices at
the same or an older version are offered as top-up targets.

---

## User interface

Input comes from keyboard and mouse. System has multiple keyboard
layouts and can be switched at any time.

At boot the user may choose the display resolution from the modes the firmware
offers; a choice that is not confirmed in time reverts to a safe default.

Boot progress is traced on screen and can be reviewed at the end of boot,
before the GUI starts.

---

## Memory and execution model

TablesOS is a single-user OS:
- **No user processes, no networking, no general file system.**

---

## Limits

| Aspect | Limit |
|---|---|
| Volume size | `2^64` pages (`≈ 64 ZiB`); practically the device size. |
| Tables | `2^64 − 1`. |
| Rows per table | Bounded only by free space. |
| Value size (any unlimited type) | Bounded only by free space (overflow chains). |
| Identifier length (table/column/fk names) | UTF-8 `string`, no fixed cap. |
| Numeric precision/scale | Unlimited. |
| Date range | Unlimited (proleptic Gregorian, signed day number). |
| Time precision | Unlimited sub-second. |

---

## Implementation notes

Build emits an image that can be written to a pendrive of any size.
It represents TablesOS without any tables. The *New operating system on USB
storage device* and *Top up version* operations create the volume spanning the
full capacity of the target device.

OS is written in Rust. 
---
