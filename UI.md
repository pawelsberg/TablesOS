# TablesOS — User Interface Specification

## Overview

TablesOS is a browser and editor for the relational store.
This document specifies what the users see and how they interact with it.

---

## Display

| Aspect | Decision |
|---|---|
| Mode | Highest available resolution, no lower than `1024 × 768`. |
| Fallback | If no acceptable graphics mode exists, boot fails with a text message; the UI does not run. |
| Font | Font will support UTF-8 |

---

## Input

Input is keyboard and mouse.

---

## Screen map

```
            Table List
                │ 
                ▼
          Table Browser ──── s ──▶ Schema Editor
            │            │
            │            │
            │            ▼
        Row View/       Row Editor (Insert / Update)
        Related rows
        navigation
```

Screens are color coded.

---

## Table List

The entry screen. Lists every table in the store.

- One table per line: name, column count, row count.
- Actions: 
    open **Table Browser**, 
    create table (prompts for a nema and opens empty Schema Editor), 
    open **Drives** (read-only diagnostic view of every legacy-IDE slot:
    model / serial / firmware / size, MBR boot signature, and either the
    TablesOS volume header — version, data-location LBA, system GUID — or
    the classic MBR partition table; the slot whose on-disk system GUID
    matches the boot-time one is marked as "this disk"),
    open **About / Licenses** (`a`),
    shutdown

---

## 2. Table Browser

A scrollable grid of one table's rows.
Cursor selects a single cell.

- Columns are the table's columns, in declared order, headed by name. 
  A column carrying a foreign key is marked. A foreign-key cell shows the
  referenced row's **reference label** (e.g. `id:5 name:John`) rather than the
  bare key value.
- Rows are listed in retrieval order, or in the user-selected sort
  order.
- `NULL` renders as a dimmed literal `NULL`, distinct from an empty
  string (shown as `""`).

Actions:

| Action |
|---|
| Open the selected row in **Row View**. |
| **Insert** a new row (Row Editor, empty). |
| **Update** the selected row (Row Editor, prefilled). |
| **Delete** the selected row (confirmation required). |
| **Sort** by one column or unsorted. |
| Open the **Schema Editor** for this table. |
| Back to the previous view. |

---

## Row View

A single row, every column on its own line — the full, untruncated
value, wrapped across lines if needed. A foreign-key field is annotated with
the referenced row's **reference label** next to its value
(e.g. `boss (unsigned integer): 5  → id:5 name:John`).
Below there is a list "Referenced by". Allows to open a Table Browser with
rows referencing the current row.

Foreign-key navigation:

- A column with a foreign key allows following the key: it opens a
  Table Browser filtered to the referenced row(s).
- The relationships list shows each outgoing target and each referencing row
  by its reference label (`→ person: id:5 name:John`,
  `← order: id:3 customer:John`).

View allows also to go back to previous view.

---

## Row Editor (Insert / Update)

Similar to Row view.

- Move between fields.
- Each field is validated against its column type **on commit** - on view exit
- A nullable column has a `[ ] NULL` toggle.
- Type visible
- Any field can be filled part-by-part with a **structured builder**: pressing
  `→` opens a builder with a separate field per component. The parts depend on
  the column type:
  - **numbers** — sign, digits (and, for `decimal`, an integer part and a
    fraction);
  - **string** — a single text field;
  - **date/time** (`date`, `date tz`, `time`, `date time`, `date time tz`) —
    year, month, day, hour, minute, second, sub-second, and the zone offset's
    sign/hours/minutes, with **`[n]` = fill the current date/time** from the
    system clock.

  The builder shows a live preview, validates on commit, and writes the
  canonical value back into the field. A long value (e.g. a big integer or a
  multi-line string) is shown **in full, wrapped across lines** — never
  truncated — in both the field and the preview.

On commit the editor surfaces, inline at the offending field:

- type-parse failures,
- `NOT NULL` violations,
- `UNIQUE` collisions,
- foreign-key violations (no such referenced value).

Nothing is written until every field passes. 

"OK" "Cancel" buttons at the end.

---

## Schema Editor

Edits the structure of one table. Lists columns with their type,
nullability, `UNIQUE` flag, and any foreign key.

| Operation |
|---|
| Add column — name, type picker, null/not-null, unique. |
| Drop column (rejected, with reason, if it carries or is targeted by a FK). |
| Rename the selected column (foreign keys naming it follow the new name). |
| Move the selected column up / down |
| Add foreign key — pick a target table, then a `UNIQUE` column. |
| Drop a foreign key. |
| Toggle `UNIQUE` on the selected column (add / remove unique). |
| Set **reference columns** (`R`) — opens a picker to choose and order the columns that label this table's rows when referenced. |
| Drop the whole table (confirmation required). |
| Back to **Table Browser**. |

The reference-columns picker lists every column with a checkbox and its
position in the label (`[1]`, `[2]`, …); `Enter`/`Space` toggles a column in or
out, `,`/`.` reorder the selected column within the label, `c` clears back to
the automatic default (the `UNIQUE` columns, or the first column). A live
"Label order" preview shows the resulting label. Each edit is saved
immediately.

The type picker is a fixed list.
Rejections (e.g. dropping a referenced column, or adding
`UNIQUE` to a column with duplicates) are shown inline with the reason;
no destructive schema change happens silently.

---

## 6. Confirmations and errors

- **Destructive actions** (delete row, drop column, drop FK, drop
  table, shutdown) open a centered modal: a one-line summary and
  `Enter` = confirm / `Esc` = cancel. Default focus is *cancel*.
- **Errors** never use a modal. They appear on the status bar (brief)
  and, when tied to a field, inline next to that field. The user is
  never blocked from reading the data behind an error.

---

## Durability feedback

The store uses write-ahead journaling and the pendrive may be pulled
at any instant.

---

## Shutdown

Switches off the power.

---

## About / Licenses

Reached from the Table List with `a`. A scrollable, read-only panel
(`↑↓ PgUp/PgDn` to scroll, `Esc` to leave) carrying the product banner and the
licenses of the software and its bundled components:

- **TablesOS code — MIT License** (the project's own license);
- **Bundled font — SIL Open Font License 1.1**: the on-screen glyph atlas is a
  bitmap derivative of **Cascadia Mono** (© Microsoft Corporation, Reserved
  Font Name "Cascadia Code"), shown with its attribution and the full OFL text.

Both license texts are embedded in the kernel, so they ship inside the bootable
image and satisfy the requirement (MIT, and OFL 1.1 clause 2) that the notice
and license accompany every copy in a form the user can view.

---

## Non-goals

- No localization of the UI strings in the first version.
- No configuration screens — there is nothing user-tunable.

---
