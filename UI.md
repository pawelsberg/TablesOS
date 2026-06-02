# TablesOS — User Interface Specification

## Overview

TablesOS is a browser and editor for the relational store.

---

## Display

| Aspect | Decision |
|---|---|
| Mode | Highest available resolution, no lower than `1024 × 720`. |
| Fallback | If no acceptable graphics mode exists, boot fails with a text message; the UI does not run. |
| Font | UTF-8 |

---

## Input

Input is keyboard and mouse. The mouse pointer can click a cell/row in any list or
grid to select it, and click the options in a modal.

---

## Screen map

```
   Table List
     ├──▶ Table Browser ──────▶ Schema Editor ──────▶ Reference columns
     │    │                                   ──────▶ Foreign key definition
     │    ├──▶ Row View / related-row navigation
     │    └──▶ Row Editor (Insert / Update) ──▶ A single field builder
     │
     ├──▶ New OS on USB:  pick target ─▶ confirm ─▶ result
     ├──▶ About / Licenses
     └──▶ Shutdown
```

Screens are color coded.

---

## Table List

The entry screen. Lists every table in the store.

- One table per line: name, column count, row count.
- Actions:
  - Open the selected table in the **Table Browser**.
  - Create a table (prompts for a name, then opens an empty Schema Editor).
  - **New operating system on USB** — install onto another USB drive (below).
  - Open **About / Licenses**.
  - **Shutdown**.

---

## Table Browser

A scrollable grid of one table's rows.
Cursor selects a single cell.

- Columns are the table's columns, in declared order, headed by name.
  A column carrying a foreign key is marked. A foreign-key cell shows the
  referenced row's **reference label** (e.g. `id:5 name:John`) rather than the
  bare key value.
- Each column is rendered at a fixed width — the **display width** set per
  column in the Schema Editor, or a default. A value wider than its column is
  truncated here (with a trailing `…`); the stored value is unchanged, and the
  full text is still visible in **Row View**.
- Rows are listed in retrieval order, or in the user-selected sort
  order.
- `NULL` renders as a dimmed literal `NULL`, distinct from an empty
  string (shown as `""`). Show `NULL` using darkec rolour.

Actions:

| Action |
|---|
| Open the selected row in **Row View**. |
| **Insert** a new row (Row Editor, empty). |
| **Update** the selected row (Row Editor, prefilled). |
| **Delete** the selected row (confirmation required). |
| **Sort** the current column: cycles unsorted → ascending → descending (NULLs always last). |
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
  by its reference label.

View allows also to go back to previous view.

---

## Row Editor (Insert / Update)

Similar to Row view.

- Focus moves between fields and the OK / Cancel buttons.
- Within the focused field, edit the text with a caret that moves through the
  value character by character or jumps to its start / end; typing inserts at
  the caret and the character before it can be deleted. The caret is a
  **blinking vertical bar** drawn as an overlay at its position — it does not
  shift the surrounding text. For a value longer than the field the view scrolls
  so the caret stays visible.
- Each field is validated against its column type **on commit** (on view exit).
- A nullable column has a `NULL` toggle.
- The column type is shown.
- Any field can be filled part-by-part with a **structured builder** that opens
  a separate field per component. The parts depend on the column type.

  The builder shows a live preview, validates on commit, and writes the
  canonical value back into the field. A long value (e.g. a big integer or a
  multi-line string) is shown **in full, wrapped across lines** — never
  truncated — in both the field and the preview. Each typed part edits with the
  same caret as the main editor; a sign part toggles between `+` and `-`.

On commit the editor surfaces, inline at the offending field:

- type-parse failures,
- `NOT NULL` violations,
- `UNIQUE` collisions,
- foreign-key violations (no such referenced value).

Nothing is written until every field passes.

You can accept or reject the change.

---

## Schema Editor

Edits the structure of one table. Lists columns with their type,
nullability, `UNIQUE` flag, and any foreign key.

| Operation |
|---|
| Add column — name, type picker, null/not-null, unique. |
| Drop column (rejected, with reason, if it carries or is targeted by a FK). |
| Rename the selected column (foreign keys naming it follow the new name). |
| Move the selected column up / down. |
| Add foreign key — pick a target table, then a `UNIQUE` column. |
| Drop a foreign key. |
| Toggle `UNIQUE` on the selected column (add / remove unique). |
| Set the column's **display width** — prompts for a character count (blank clears it back to the default). Presentational only: it controls how wide the column is in the Table Browser, never what is stored. |
| Set **reference columns** — opens a picker to choose and order the columns that label this table's rows when referenced. |
| Drop the whole table (confirmation required). |
| Back to **Table Browser**. |

The reference-columns picker lists every column with a checkbox and its
position in the label (`[1]`, `[2]`, …); a column can be toggled in or out of
the label and reordered within it, or the whole set cleared back to the
automatic default (the `UNIQUE` columns, or the first column). A live
"Label order" preview shows the resulting label. Each edit is saved
immediately.

The type picker is a fixed list.
Rejections (e.g. dropping a referenced column, or adding
`UNIQUE` to a column with duplicates) are shown inline with the reason;
no destructive schema change happens silently.

---

## Confirmations and errors

- **Destructive actions** (delete row, drop column, drop FK, drop
  table, overwrite a USB drive when installing a new OS, shutdown) open a
  confirmation.
- **Errors** never use a modal. They appear on the status bar and, when tied to a field, inline next to that field. The user is never blocked from reading the data behind an error.

---

## Shutdown

Switches off the power.

---

## New operating system on USB

Reached from the **Table List** — the *New operating system on USB storage
device* maintenance operation. It installs a fresh, empty TablesOS onto another
USB drive:

1. The USB autopilot runs and every **non-booted** USB mass-storage drive is
   offered as a target.
2. **Pick target** — a list of candidate drives (slot, model, size, current MBR
   state); choose one to continue, or cancel.
3. **Confirm** — a modal warns that the chosen drive will be *overwritten*
4. **Result** — a panel reports each step: boot prefix copied, fresh system
   GUID stamped, volume formatted, and the post-write MBR / mount verification.
   Dismissing it returns to the Table List.

The booted disk is never touched: the new image is given its own fresh system
GUID, which is what lets the running OS write a *different* drive without
tripping the boot-medium identity gate.

---

## About / Licenses

Reached from the Table List. A scrollable, read-only panel carrying the product
banner and the licenses of the software and its bundled components:

- **TablesOS code — MIT License** (the project's own license);
- **Bundled font** - if any.
---

## Non-goals

- No localization of the UI strings in the first version.
- No configuration screens — there is nothing user-tunable.

---
