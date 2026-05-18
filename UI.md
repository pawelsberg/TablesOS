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
    shutdown

---

## 2. Table Browser

A scrollable grid of one table's rows.
Cursor selects a single cell.

- Columns are the table's columns, in declared order, headed by name. 
  A column carrying a foreign key is marked.
- Rows are listed in retrieval order.
- `NULL` renders as a dimmed literal `NULL`, distinct from an empty
  string (shown as `""`).

Actions:

| Action |
|---|
| Open the selected row in **Row View**. |
| **Insert** a new row (Row Editor, empty). |
| **Update** the selected row (Row Editor, prefilled). |
| **Delete** the selected row (confirmation required). |
| Open the **Schema Editor** for this table. |
| Back to the previous view. |

---

## Row View

A single row, every column on its own line — the full, untruncated
value, wrapped across lines if needed.
Below there is a list "Referenced by". Allows to open a Table Browser with
rows referencing the current row.

Foreign-key navigation:

- A column with a foreign key allows following the key: it opens a
  Table Browser filtered to the referenced row(s).

View allows also to go back to previous view.

---

## Row Editor (Insert / Update)

Similar to Row view.

- Move between fields.
- Each field is validated against its column type **on commit** - on view exit
- A nullable column has a `[ ] NULL` toggle.
- Type visible

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
| Add foreign key — pick a target table, then a `UNIQUE` column. |
| Drop a foreign key. |
| Toggle `UNIQUE` on the selected column (add / remove unique). |
| Drop the whole table (confirmation required). |
| Back to **Table Browser**. |

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

## Non-goals

- No localization of the UI strings in the first version.
- No configuration screens — there is nothing user-tunable.

---
