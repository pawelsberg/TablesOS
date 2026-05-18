# TablesOS — System Specification

## Overview

TablesOS is an operating system. It boots directly from a USB
flash drive ("pendrive"), takes full control of the machine, and presents the
user with one thing: a relational store of **tables** and the **relationships**
between them.

There is no general-purpose file system, no user processes, no shell scripting.
The entire device is one custom-formatted volume.

Characteristics:

1. **Boot from any pendrive**, regardless of size, on commodity x86-64 hardware.
2. **All capacity is data capacity.**
3. **Unlimited values.** Integers, decimals, strings and dates have no built-in
   width limit; a single value may take arbitrarily big space.
4. **Relational integrity.** Foreign keys connect tables and are enforced.
5. **Crash safety.** The pendrive may be removed at any instant; the store is
   never left structurally inconsistent.
6. **SSD safe** system uses SSD in a safe manner - taking into account nature of
   flash memmory. Preventing physical damage of the medium.
7. **Graphic user interface** (VESA) allows to interact with operating system.

---

## Data model

The store holds **tables**. A table has an ordered list of **columns**; each
column has a name and a **type** and may be nullable. A table holds an
unordered multiset of **rows**. A column may carry a **foreign key** that
references a column in another (or the same) table.

There is no implicit primary key. A foreign key may target any column that
carries a uniqueness guarantee (a `UNIQUE` column).

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
including its own table (self-reference). The referenced column must be `UNIQUE`
this is checked at definition time.

---

### Browsing related rows

The GUI shows relationships when browsing.

---

### Schema operations

| Operation | Effect |
|---|---|
| `Create table <name>` | Create an empty table. |
| `Drop table` | Delete a table and all its rows and FKs. |
| `Add column <name> <type> [null\|not null] [unique]` | Append a column. Existing rows get NULL (column must be nullable) . |
| `Drop column <name>` | Remove a column. Rejected if it carries or is targeted by a FK. |
| `Add fk <col> -> <table>.<col>` | Define a foreign key. |
| `Drop fk <fk-name>` | Remove a foreign key |
| `Add unique` | Add unique to a column |
| `Remove unique` | Remove unique from a column |


### Data Operations

| Operation | Effect |
|---|---|
| `Insert` | Insert a row. |
| `Update a row` | Update a row. |
| `Delete a row` | Delete a row. |
| `Navigate back and forth` | Navigate relationships. |

### Maintenance Operations

| Operation | Effect |
|---|---|
| `shutdown` | Shut down the computer. |

---

## Durability and crash recovery

The pendrive may be removed at any moment. System uses **write-ahead journaling**
to guarantee that the store is always recoverable to a consistent state.
After any power loss, mounting recovers a consistent store with at most the last 
uncommitted transaction lost.

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
It represents TableOS without any tables. OS claims the rest of the pendrive during runtime.

OS is written in Rust. 

---
