# TablesOS — Graphical-AI Asset Prompts

Prompts to feed a **graphical AI** (Midjourney, DALL·E 3, Stable Diffusion /
Flux, Adobe Firefly, …) to generate the bitmaps TablesOS loads at boot:

1. **Five SciFi backgrounds**, one per UI view.
2. **One monospace font atlas** (higher-resolution than the built-in 8×8 face).

Everything here is pinned to what the kernel actually renders — the exact
per-view palette (`kernel/src/framebuffer.rs`), the exact character set
(`kernel/src/ui.rs` + `kernel/src/font.rs`) and the boot size budget
(`Cargo.toml` profile notes). Generate to **these** specs and the assets drop
straight in.

---

## 0. The asset contract (read this first)

The kernel decodes these PNGs **at boot** with the in-tree `pngdec` crate, so:

| Constraint | Value | Why |
|---|---|---|
| Folder | `kernel/assets/` | `build.rs` embeds whatever is present here. |
| Format | **PNG**, 8-bit, **non-interlaced** (no Adam7) | `pngdec` supports color types 0/2/3/6 at bit-depth 8, no interlace. |
| Backgrounds aspect | **16:10** | Matches the real modes (QEMU gives 2560×1600; floor is 1024×768). The kernel scales-to-fill, so author once. |
| Backgrounds size | **1280 × 800** (1920×1200 if budget allows) | Soft/dark art upscales cleanly; small files keep the kernel bootable. |
| **Per-file cap** | **≤ 150 KB** each | See the budget below. |
| **Total asset cap** | **≤ 900 KB** all files combined | The BIOS stage-2 loader stalls if the kernel image grows past a few MB; it's ~2.5 MB today. Embedding the *compressed* PNG (not raw pixels) is what keeps us under budget. |
| Missing files | fine | Any asset you don't supply falls back to the existing **procedural** backdrop / 8×8 font. The build never breaks. |

> **Keep PNGs small:** export backgrounds as **indexed / palette PNG, ≤ 64
> colors**. A dark nebula with a tight palette compresses to 50–150 KB. A
> 24-bit photographic export will be 2–6 MB and **will not fit** — it will
> push the kernel past the size the bootloader can load.
>
> **If your generator only exports full-colour PNG** (often 0.6–1 MB each at
> 1280×800), either re-export as indexed-palette, **or just downscale to
> ~600 px wide** — the kernel bilinear-upscales the source to fill the screen,
> so a small soft background still looks smooth behind text. The bundled set
> was produced this way: 5 backgrounds at 600×403, ~220 KB each.

### Filenames the kernel looks for

| File | Used by view | Tint floor (the view's color code) |
|---|---|---|
| `kernel/assets/bg_list.png`    | Table List       | `#051D33` teal/cyan |
| `kernel/assets/bg_browser.png` | Table Browser    | `#03221A` green |
| `kernel/assets/bg_row.png`     | Row View         | `#1C092E` violet |
| `kernel/assets/bg_edit.png`    | Row Editor       | `#2A1404` amber |
| `kernel/assets/bg_schema.png`  | Schema Editor / system / modals | `#061236` indigo |
| `kernel/assets/bg.png`         | *optional* shared fallback for any of the above that's missing | — |
| `kernel/assets/font.png`       | all text (higher-res face) | — |

> **Tight on budget? Ship only `bg.png`.** One shared 16:10 background is
> tinted five different ways by the kernel (it multiplies the image toward
> each view's floor color), so a single ~120 KB file gives every screen its
> color-coded look. Per-view files just let you art-direct each screen
> individually.

---

## 1. Backgrounds — shared art direction

These run **behind** a full HUD: a title bar across the top, a bordered
content panel with neon corner emblems, body text in the middle, and a status
bar across the bottom. So every background must **stay out of the way of text**.

Put this **shared block** at the top of every background prompt, then append
the per-view block:

```
Deep-space science-fiction UI background plate, 16:10, 1280x800.
Extremely dark, near-black base; this is a backdrop UNDER bright HUD text, so
it must read as quiet and low-contrast. Heavy vignette: the central 60% is
darkest and almost flat so white text stays legible; visual interest lives at
the edges and corners. Subtle, fine detail only — no large bright shapes, no
text, no logos, no UI widgets, no characters, no readable glyphs, no lens
flares crossing the center. Cohesive palette of deep blues and blacks with a
few thin neon-cyan (#3CF2FF) and neon-mint (#3BFFB4) accent lines. Cinematic,
high-end spaceship-console aesthetic. Faint horizontal scanline texture.
Flat, even, front-on framing (no perspective floor/horizon).
```

**Negative prompt (all backgrounds):**

```
text, letters, numbers, words, watermark, signature, logo, UI buttons, icons,
menus, cursor, bright center, high contrast, busy clutter, faces, people,
characters, vibrant saturated fills, overexposed highlights, white background,
photo grain, jpeg artifacts
```

**Export (all backgrounds):** PNG, 16:10 (1280×800), **indexed ≤ 64 colors**,
non-interlaced, **≤ 150 KB**.

---

### 1a. Table List → `bg_list.png` (teal/cyan `#051D33`)

The catalog/index screen. Feel: a star-chart directory.

```
[shared block above]
Theme: a vast cool teal-cyan star catalog. Floor color deep teal #051D33
fading up into near-black navy #02050B at the top. Faint constellation grid
and tiny distant stars concentrated toward the upper and outer edges, a soft
nebula glow in the far corners. Calm, archival, "index of everything".
```

### 1b. Table Browser → `bg_browser.png` (green `#03221A`)

The data-grid screen. Feel: a live sensor matrix.

```
[shared block above]
Theme: a dark emerald sensor-grid readout. Floor color deep green #03221A
fading up into near-black navy #02050B. Faint isometric data-grid / matrix of
thin mint lines fading into darkness toward the center, a few green telemetry
glints at the far edges. Technical, "scanning rows of data".
```

### 1c. Row View → `bg_row.png` (violet `#1C092E`)

The single-record inspection screen. Feel: a focused specimen chamber.

```
[shared block above]
Theme: a deep violet inspection chamber. Floor color dark violet #1C092E
fading up into near-black navy #02050B. A single soft column of cool light
suggested far in the background, faint magenta-cyan haze in the corners, very
quiet center. Focused, intimate, "one record under the lens".
```

### 1d. Row Editor → `bg_edit.png` (amber `#2A1404`)

The editing screen. Amber is the system's "active input / caution" color.

```
[shared block above]
Theme: an amber active-console editing bay. Floor color dark amber #2A1404
fading up into near-black navy #02050B. Warm amber edge-glow and a few thin
caution-stripe accents in the far corners only, cool cyan still present as a
secondary accent. Center stays dark and neutral. Alert but not alarming,
"hands-on the controls".
```

### 1e. Schema Editor → `bg_schema.png` (indigo `#061236`)

The structure/blueprint screen (also used by system & modal views).

```
[shared block above]
Theme: an indigo engineering blueprint void. Floor color deep indigo #061236
fading up into near-black navy #02050B. Faint blueprint wireframe / schematic
linework and node points drifting at the outer edges, thin cyan dimension
lines. Architectural, precise, "the structure behind the data".
```

---

## 2. Font atlas → `font.png`

> ### ⚠️ Honesty up front
> Image generators are **excellent at backgrounds and unreliable at fonts**.
> They hallucinate letterforms, misorder glyphs, and won't hold a precise
> pixel grid — so an AI-generated atlas usually needs hand-fixing before it's
> legible. The prompt below is tuned to give the best shot, **but** for a
> production-quality result the reliable path is to render a real monospace
> font into this exact atlas with a tiny script — see
> **§2.3 Reliable alternative**. Either output drops into `kernel/assets/font.png`.

### 2.1 Exact atlas layout (the kernel reads this fixed grid)

- **Grid:** 16 columns × 7 rows of glyph cells.
- **Cell size:** **32 × 32 px** per glyph → total image **512 × 224 px**.
- **Monospace, fills the cell width:** every glyph centered in its own 32×32
  cell, identical advance. The kernel maps each square cell into a square
  on-screen box, so a tall-narrow face left at its natural width looks sparse
  (big inter-character gaps). Glyphs should therefore **fill most of the cell
  width** — the §2.3 script does this by stretching the face horizontally.
- **Color:** **white glyphs (#FFFFFF) on a pure-black (#000000) background.**
  The kernel reads brightness as coverage (alpha) and tints each glyph to the
  UI color at draw time, so do **not** color the glyphs.
- **Order:** cell index `= codepoint − 0x20`, filled **left-to-right,
  top-to-bottom**. Rows 0–5 are ASCII `0x20`–`0x7E`; cell 95 (DEL) is blank;
  row 6 holds the special glyphs the UI uses, in this order:

```
Row 0:  (space) ! " # $ % & ' ( ) * + , - . /
Row 1:  0 1 2 3 4 5 6 7 8 9 : ; < = > ?
Row 2:  @ A B C D E F G H I J K L M N O
Row 3:  P Q R S T U V W X Y Z [ \ ] ^ _
Row 4:  `(backtick) a b c d e f g h i j k l m n o
Row 5:  p q r s t u v w x y z { | } ~ (blank)
Row 6:  µ × — … ← ↑ → ↓ ⮡  (then 7 blank cells)
```

(`µ` U+00B5, `×` U+00D7, `—` U+2014, `…` U+2026, `←` U+2190, `↑` U+2191,
`→` U+2192, `↓` U+2193, `⮡` U+2BA1.) These are every non-ASCII glyph `ui.rs`
renders — the built-in 8×8 face is currently **missing `µ` and `×`**, so the
atlas is also a coverage fix.

### 2.2 Generation prompt (best-effort, expect cleanup)

```
A monospaced bitmap pixel-font sprite sheet / glyph atlas, 512x224 pixels,
arranged as a strict uniform grid of 16 columns by 7 rows of 32x32 cells.
Pure white (#FFFFFF) glyphs on a pure black (#000000) background, no anti-alias
halos beyond 1px, no color. Clean technical sci-fi console typeface: squared,
geometric, slightly condensed, bold even stems, flat terminals — like a
spaceship terminal readout. Every glyph the same height, sharing one baseline,
each centered in its own 32x32 cell with even spacing, perfectly aligned to the
grid. Glyph set in reading order, one per cell, left to right then top to
bottom: space ! " # $ % & ' ( ) * + , - . / 0 1 2 3 4 5 6 7 8 9 : ; < = > ?
@ A B C D E F G H I J K L M N O P Q R S T U V W X Y Z [ \ ] ^ _ ` a b c d e f g
h i j k l m n o p q r s t u v w x y z { | } ~ . High legibility, no decorative
flourishes.
```

**Negative prompt:** `color, gradient, drop shadow, glow, blurry, antialiased
soft edges, misaligned grid, missing letters, duplicate letters, cursive,
serif, handwriting, ornaments, background texture, photo`

**Export:** PNG, 512×224, grayscale or RGB, non-interlaced, ≤ 100 KB.

### 2.3 Reliable alternative — render a real font to the atlas

This is what the repo actually ships: `kernel/assets/font.png` is rendered from
**Cascadia Mono** (SIL Open Font License 1.1 — ships with Visual Studio /
Windows Terminal; see `kernel/assets/FONT_LICENSE.txt`). Regenerate the exact
same atlas from any installed monospace face with this PowerShell one-shot
(uses Windows' built-in `System.Drawing`):

```powershell
Add-Type -AssemblyName System.Drawing
$cell=32; $cols=16; $rows=7
$bmp=New-Object System.Drawing.Bitmap (($cols*$cell),($rows*$cell))
$g=[System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::Black)
$g.TextRenderingHint='AntiAlias'   # transform-aware (GridFit ignores the scale)
# 28px fills the 32px cell height; the 1.7x horizontal scale fills the cell
# WIDTH so the square-grid kernel layout doesn't show big gaps between chars.
$size=28; $sx=1.7
$font=New-Object System.Drawing.Font('Cascadia Mono',$size,[System.Drawing.FontStyle]::Bold,[System.Drawing.GraphicsUnit]::Pixel)
$fmt=New-Object System.Drawing.StringFormat
$fmt.Alignment='Center'; $fmt.LineAlignment='Center'
$white=[System.Drawing.Brushes]::White
# codepoints in atlas order: ASCII 0x20..0x7E, then cell 95 (DEL) left blank,
# then the specials at cell 96+ — this gap is mandatory: the kernel's
# `atlas_cell` maps µ→96, ←→100, →→102, …, so packing µ into cell 95 shifts
# every special glyph (all the arrows) one cell early.
$cps=@(); 0x20..0x7E | ForEach-Object { $cps+=$_ }
$cps+=-1   # cell 95 (DEL): intentionally blank
$cps+=@(0xB5,0xD7,0x2014,0x2026,0x2190,0x2191,0x2192,0x2193,0x2BA1)
for($i=0;$i -lt $cps.Count;$i++){
  if($cps[$i] -lt 0x20){ continue }   # leave DEL / placeholder cells blank
  $ch=[char]::ConvertFromUtf32($cps[$i])
  $cx=($i % $cols)*$cell; $cy=[math]::Floor($i/$cols)*$cell
  # Draw each glyph centered in its cell, stretched horizontally by $sx.
  $g.ResetTransform()
  $g.TranslateTransform([single]($cx+$cell/2.0),[single]($cy+$cell/2.0))
  $g.ScaleTransform([single]$sx,[single]1.0)
  $g.DrawString($ch,$font,$white,[single]0,[single]0,$fmt)
}
$g.ResetTransform(); $g.Dispose()
$bmp.Save("$PSScriptRoot\kernel\assets\font.png",[System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
"wrote kernel/assets/font.png"
```

> **Licensing — this matters for a public repo.** A bitmap atlas is a
> derivative of the font you render it from. Render only from an **open**
> face (Cascadia Mono / DejaVu Sans Mono / JetBrains Mono — OFL/permissive)
> and **ship its license** next to `font.png`. **Do *not* commit an atlas
> rendered from a proprietary font** (Consolas, Lucida Console, …) to a
> public repository.

The arrow glyphs (`← ↑ → ↓ ⮡`) depend on the chosen font having them; Cascadia
Mono covers all of them. Any cell the kernel finds blank falls back to the
built-in 8×8 glyph, so partial coverage is safe.

---

## 3. After you generate the assets

1. Drop the PNGs into `kernel/assets/` using the exact filenames in §0.
2. Confirm each is within its size cap (`Get-ChildItem kernel/assets`).
3. Rebuild and run — `build.rs` embeds whatever is present; the kernel decodes
   and uses it, and falls back to the procedural look for anything absent:

```powershell
$env:Path="$env:USERPROFILE\.cargo\bin;C:\msys64\mingw64\bin;$env:Path"
cargo run
```

If a PNG is rejected at boot (wrong color type, interlaced, or oversized) the
kernel logs it on the serial console and uses the fallback for that asset —
re-export per §0 and rebuild.
