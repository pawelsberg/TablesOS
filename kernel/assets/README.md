# kernel/assets — embedded bitmaps

Drop PNGs here and `build.rs` embeds them into the kernel; the kernel decodes
them at boot (via the `pngdec` crate) and uses them, falling back to the
procedural backdrop / built-in 8×8 font for anything missing.

See [`../../ASSET_PROMPTS.md`](../../ASSET_PROMPTS.md) for the graphical-AI
prompts and the exact spec (filenames, dimensions, formats, size budget).

| File | Used for |
|---|---|
| `bg_list.png`    | Table List background |
| `bg_browser.png` | Table Browser background |
| `bg_row.png`     | Row View background |
| `bg_edit.png`    | Row Editor background |
| `bg_schema.png`  | Schema Editor / system / modal background |
| `bg.png`         | optional shared fallback background (tinted per view) |
| `font.png`       | higher-resolution monospace glyph atlas (16×7 cells) |

Requirements (enforced/relied on by `pngdec`): **8-bit, non-interlaced** PNG;
color type grayscale/RGB/palette/RGBA. Keep total embedded size small
(≤ ~900 KB) or the BIOS bootloader will struggle to load the kernel.
