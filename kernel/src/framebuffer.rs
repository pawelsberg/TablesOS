//! VESA linear-framebuffer graphics, **double-buffered**.
//!
//! The `bootloader` crate sets a graphics mode via BIOS VBE and hands over a
//! linear framebuffer in [`BootInfo`]. All drawing goes to an off-screen
//! `scene` buffer in RAM; [`Display::present`] copies the whole scene to the
//! hardware framebuffer in one pass. That removes the clear-then-redraw
//! flicker entirely and makes a full repaint a single contiguous memcpy.
//!
//! The mouse pointer is composited on top of the hardware buffer using the
//! clean `scene` as the restore source, so moving the mouse only touches a
//! tiny rectangle and never triggers a scene repaint.

use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use crate::font::{self, Font};

pub const SCALE: usize = 2; // 8×8 glyphs → 16×16 cells, readable at ≥1024×768
pub const CELL_W: usize = font::GLYPH_W * SCALE;
pub const CELL_H: usize = font::GLYPH_H * SCALE;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

// Sci-fi "deep-space HUD" palette. Every screen shares a near-black navy at
// the top of its gradient (`C_BG_TOP`) and fades down to a per-screen tint —
// that floor colour is what keeps the spec's screen colour-coding while the
// rest of the chrome (neon-cyan rules, glowing brackets) stays unified.
pub const C_BG_TOP: Rgb = Rgb(0x02, 0x05, 0x0b); // shared gradient ceiling
pub const C_BG_LIST: Rgb = Rgb(0x05, 0x1d, 0x33); // teal/cyan — Table List
pub const C_BG_BROWSER: Rgb = Rgb(0x03, 0x22, 0x1a); // green — Browser
pub const C_BG_ROW: Rgb = Rgb(0x1c, 0x09, 0x2e); // violet — Row View
pub const C_BG_EDIT: Rgb = Rgb(0x2a, 0x14, 0x04); // amber — Editor
pub const C_BG_SCHEMA: Rgb = Rgb(0x06, 0x12, 0x36); // indigo — Schema/system

pub const C_FG: Rgb = Rgb(0xd6, 0xf0, 0xff); // cool near-white text
pub const C_DIM: Rgb = Rgb(0x6c, 0x90, 0xa8); // muted slate (hints)
pub const C_SEL: Rgb = Rgb(0x0b, 0x35, 0x4e); // selection-bar fill (dark, text-safe)
// `C_CELL_SEL` is the bright cell-cursor *marker* (outline / lit edge / emblem
// mid-tone). `C_CELL_SEL_FILL` is the darker fill drawn behind the selected
// cell's text so near-white glyphs stay legible (the bright colour washed them
// out). The cell cursor = dark fill + bright outline.
pub const C_CELL_SEL: Rgb = Rgb(0x23, 0x9d, 0xcf);
pub const C_CELL_SEL_FILL: Rgb = Rgb(0x10, 0x52, 0x74);
pub const C_ACCENT: Rgb = Rgb(0x3c, 0xf2, 0xff); // neon cyan — primary accent
pub const C_ACCENT2: Rgb = Rgb(0x3b, 0xff, 0xb4); // neon mint — secondary
pub const C_ERR: Rgb = Rgb(0xff, 0x49, 0x6e); // neon red/pink
pub const C_BAR: Rgb = Rgb(0x02, 0x08, 0x12); // HUD bar fill
pub const C_PANEL_EDGE: Rgb = Rgb(0x1b, 0x57, 0x77); // content-panel border
pub const C_GLOW: Rgb = Rgb(0x0e, 0x55, 0x6e); // accent halo / rule echo

// Gradient stops for the chrome typography. Each text run fades top→bottom
// from a near-white highlight to a saturated neon, giving glyphs a glossy,
// back-lit look.
pub const C_TITLE_TOP: Rgb = Rgb(0xea, 0xff, 0xff);
pub const C_TITLE_BOT: Rgb = Rgb(0x15, 0xb4, 0xea);
pub const C_TELE_TOP: Rgb = Rgb(0xd2, 0xff, 0xe8);
pub const C_TELE_BOT: Rgb = Rgb(0x11, 0xc6, 0x88);
pub const C_HEAD_TOP: Rgb = Rgb(0xaa, 0xfb, 0xff);
pub const C_HEAD_BOT: Rgb = Rgb(0x17, 0xa4, 0xd6);
pub const C_STAT_TOP: Rgb = Rgb(0xe8, 0xff, 0xff);
pub const C_STAT_BOT: Rgb = Rgb(0x2a, 0xb6, 0xe0);
// Slightly lit floor for the HUD bars, so they read as brushed panels rather
// than flat black.
pub const C_BAR_LO: Rgb = Rgb(0x06, 0x14, 0x22);

const CURSOR_W: usize = 16;
const CURSOR_H: usize = 24;

// HUD pointer: a neon arrow with a lit tip, authored at 16×24 so it draws at
// roughly twice the old footprint while keeping crisp 1-pixel diagonal edges
// (a plain 2× upscale of the old 8×12 art would look blocky). The hotspot is
// the tip at [0][0], which sits exactly under the reported mouse position.
// 1 = cyan fill, 2 = dark outline, 3 = white highlight, 0 = transparent.
#[rustfmt::skip]
const CURSOR: [[u8; CURSOR_W]; CURSOR_H] = [
    [2,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,2,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,3,2,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,3,1,2,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,3,3,1,2,0,0,0,0,0,0,0,0,0,0,0],
    [2,3,3,1,1,2,0,0,0,0,0,0,0,0,0,0],
    [2,3,3,1,1,1,2,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,2,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,1,2,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,1,1,2,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,1,1,1,2,0,0,0,0,0],
    [2,1,1,1,1,1,1,1,1,1,1,2,0,0,0,0],
    [2,1,1,1,1,1,1,1,1,1,1,1,2,0,0,0],
    [2,1,1,1,1,1,1,2,2,2,2,2,2,2,0,0],
    [2,1,1,1,1,2,1,2,0,0,0,0,0,0,0,0],
    [2,1,1,2,2,0,2,1,2,0,0,0,0,0,0,0],
    [2,2,2,0,0,0,0,2,1,2,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,2,1,2,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,2,1,2,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,2,1,2,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,2,1,2,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,2,1,2,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
];

/// Top-left frame-corner emblem: a nested HUD bracket with a node and a tick.
/// Authored as a 16×16 pixel-art tile with three brightness levels
/// (3 = bright, 2 = mid, 1 = dim, 0 = transparent); [`Display::corner_emblem`]
/// blits it `SCALE`-magnified and mirrors it into the other three corners.
const EMBLEM_N: usize = 16;
/// On-screen size of a corner emblem (the tile, `SCALE`-magnified).
pub const EMBLEM_PX: usize = EMBLEM_N * SCALE;
#[rustfmt::skip]
const FRAME_CORNER: [[u8; EMBLEM_N]; EMBLEM_N] = [
    [3,3,3,3,3,3,3,3,3,3,3,3,0,0,0,0],
    [3,3,3,3,3,3,3,3,3,3,3,3,0,0,0,0],
    [3,3,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [3,3,0,0,2,2,2,2,2,2,0,0,0,0,0,0],
    [3,3,0,0,2,0,0,0,0,0,0,0,0,0,0,0],
    [3,3,0,0,2,0,0,0,0,0,0,0,0,0,0,0],
    [3,3,0,0,2,0,0,3,3,0,0,0,0,0,0,0],
    [3,3,0,0,2,0,0,3,3,0,0,0,0,0,0,0],
    [3,3,0,0,2,0,0,0,0,0,0,0,0,0,0,0],
    [3,3,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [3,3,0,0,1,0,0,0,0,0,0,0,0,0,0,0],
    [3,3,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
];

/// Framebuffer geometry handed over by our bootloader (`boot/layout.md`).
#[derive(Clone, Copy)]
pub struct FbInfo {
    pub width: usize,
    pub height: usize,
    pub pitch: usize, // bytes per scanline
    pub bpp: usize,   // bytes per pixel (3 or 4)
    pub bgr: bool,    // true = BGR, false = RGB
}

/// A decoded RGBA bitmap kept around for blitting (a view background).
struct OwnedImage {
    w: usize,
    h: usize,
    rgba: Vec<u8>, // w*h*4
}

/// A monospace glyph atlas decoded from `font.png`: a `cols`×`rows` grid of
/// equal `cellw`×`cellh` cells, stored as one coverage byte (0..=255) per
/// pixel. `filled[cell]` is false for blank cells, so an in-range-but-empty
/// glyph falls back to the built-in 8×8 face instead of drawing nothing.
struct GlyphAtlas {
    w: usize,
    cols: usize,
    rows: usize,
    cellw: usize,
    cellh: usize,
    cov: Vec<u8>,
    filled: Vec<bool>,
}

/// Atlas cell index for a code point, matching the layout in `ASSET_PROMPTS.md`
/// (ASCII `0x20`..=`0x7E`, the specials the UI uses, then the keyboard-layout
/// blocks: currency/accents, the Greek block in code-point order, Polish).
/// `None` => not in the atlas, use the 8×8 fallback. A mapped-but-blank cell
/// (unassigned holes inside the Greek range) also falls back via `filled`.
fn atlas_cell(ch: char) -> Option<usize> {
    let c = ch as u32;
    if (0x20..=0x7E).contains(&c) {
        return Some((c - 0x20) as usize);
    }
    // Greek: tonos ΄ / dialytika-tonos ΅ / accented capitals / Α–Ω / α–ω /
    // accented vowels — one contiguous code-point run, cells 112..=186.
    if (0x0384..=0x03CE).contains(&c) {
        return Some(112 + (c - 0x0384) as usize);
    }
    Some(match c {
        0x00B5 => 96,  // µ
        0x00D7 => 97,  // ×
        0x2014 => 98,  // —
        0x2026 => 99,  // …
        0x2190 => 100, // ←
        0x2191 => 101, // ↑
        0x2192 => 102, // →
        0x2193 => 103, // ↓
        0x2BA1 => 104, // ⮡
        0x00A3 => 105, // £
        0x00AC => 106, // ¬
        0x20AC => 107, // €
        0x00A6 => 108, // ¦
        0x00A8 => 109, // ¨
        // Polish, cells 187..=204.
        0x0104 => 187, // Ą
        0x0105 => 188, // ą
        0x0106 => 189, // Ć
        0x0107 => 190, // ć
        0x0118 => 191, // Ę
        0x0119 => 192, // ę
        0x0141 => 193, // Ł
        0x0142 => 194, // ł
        0x0143 => 195, // Ń
        0x0144 => 196, // ń
        0x00D3 => 197, // Ó
        0x00F3 => 198, // ó
        0x015A => 199, // Ś
        0x015B => 200, // ś
        0x0179 => 201, // Ź
        0x017A => 202, // ź
        0x017B => 203, // Ż
        0x017C => 204, // ż
        _ => return None,
    })
}

/// Which background slot a screen's gradient-floor tint selects.
fn slot_for(tint: Rgb) -> usize {
    if tint == C_BG_LIST {
        0
    } else if tint == C_BG_BROWSER {
        1
    } else if tint == C_BG_ROW {
        2
    } else if tint == C_BG_EDIT {
        3
    } else {
        4 // schema / system / modals
    }
}

/// Brighten a (dark) floor tint to a full-intensity hue, used to colour-tint
/// the *shared* background per view without crushing it to near-black.
fn norm_hue(c: Rgb) -> Rgb {
    let m = (c.0.max(c.1).max(c.2)).max(1) as u32;
    Rgb(
        (c.0 as u32 * 255 / m) as u8,
        (c.1 as u32 * 255 / m) as u8,
        (c.2 as u32 * 255 / m) as u8,
    )
}

/// Read one pixel back out of a buffer (for alpha-blending glyph coverage over
/// the already-drawn background).
#[inline]
fn get_px(buf: &[u8], info: &FbInfo, x: usize, y: usize) -> Rgb {
    let o = y * info.pitch + x * info.bpp;
    if o + info.bpp > buf.len() {
        return Rgb(0, 0, 0);
    }
    if info.bgr {
        Rgb(buf[o + 2], buf[o + 1], buf[o])
    } else {
        Rgb(buf[o], buf[o + 1], buf[o + 2])
    }
}

/// Stretch-to-fill blit of `img` across the whole buffer with **bilinear**
/// sampling (8-bit fixed-point), so a small source upscales smoothly to the
/// framebuffer. `tint`, if set, multiplies each pixel by that hue (shared
/// background path).
fn blit_stretch(scene: &mut [u8], info: &FbInfo, img: &OwnedImage, tint: Option<Rgb>) {
    if img.w == 0 || img.h == 0 || img.rgba.len() < img.w * img.h * 4 {
        return;
    }
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 {
        return;
    }
    // Precompute per-column source x0/x1 and fractional weight, so the hot
    // loop carries no 64-bit divide (the dominant cost under emulation).
    let mut xmap: Vec<(usize, usize, u32)> = Vec::with_capacity(w);
    for dx in 0..w {
        let gx = (dx as u64 * img.w as u64 * 256 / w as u64) as u32;
        let x0 = (gx >> 8) as usize;
        xmap.push((x0, (x0 + 1).min(img.w - 1), gx & 0xff));
    }
    let mix = |a: u32, b: u32, f: u32| (a * (256 - f) + b * f) >> 8;
    let rgba = &img.rgba;
    for dy in 0..h {
        let gy = (dy as u64 * img.h as u64 * 256 / h as u64) as u32;
        let y0 = (gy >> 8) as usize;
        let y1 = (y0 + 1).min(img.h - 1);
        let fy = gy & 0xff;
        let (r0, r1) = (y0 * img.w, y1 * img.w);
        for (dx, &(x0, x1, fx)) in xmap.iter().enumerate() {
            let (o00, o10) = ((r0 + x0) * 4, (r0 + x1) * 4);
            let (o01, o11) = ((r1 + x0) * 4, (r1 + x1) * 4);
            let r = mix(mix(rgba[o00] as u32, rgba[o10] as u32, fx), mix(rgba[o01] as u32, rgba[o11] as u32, fx), fy);
            let g = mix(mix(rgba[o00 + 1] as u32, rgba[o10 + 1] as u32, fx), mix(rgba[o01 + 1] as u32, rgba[o11 + 1] as u32, fx), fy);
            let b = mix(mix(rgba[o00 + 2] as u32, rgba[o10 + 2] as u32, fx), mix(rgba[o01 + 2] as u32, rgba[o11 + 2] as u32, fx), fy);
            let c = match tint {
                Some(t) => Rgb(
                    (r * t.0 as u32 / 255) as u8,
                    (g * t.1 as u32 / 255) as u8,
                    (b * t.2 as u32 / 255) as u8,
                ),
                None => Rgb(r as u8, g as u8, b as u8),
            };
            put(scene, info, dx, dy, c);
        }
    }
}

/// Paint one atlas glyph into `scene` at `(x, y)`, area-averaging the
/// `cellw`×`cellh` source cell down to the `bw`×`bh` output box, tinting with a
/// vertical `top`→`bot` gradient, and alpha-blending coverage over the
/// background already present in `scene`.
#[allow(clippy::too_many_arguments)]
fn paint_atlas_glyph(
    scene: &mut [u8],
    info: &FbInfo,
    atlas: &GlyphAtlas,
    cell: usize,
    x: usize,
    y: usize,
    bw: usize,
    bh: usize,
    top: Rgb,
    bot: Rgb,
) {
    let cellx = (cell % atlas.cols) * atlas.cellw;
    let celly = (cell / atlas.cols) * atlas.cellh;
    let den = bh.saturating_sub(1).max(1);
    for ty in 0..bh {
        let sy0 = ty * atlas.cellh / bh;
        let sy1 = (((ty + 1) * atlas.cellh / bh).max(sy0 + 1)).min(atlas.cellh);
        let color = lerp(top, bot, ty, den);
        let py = y + ty;
        if py >= info.height {
            break;
        }
        for tx in 0..bw {
            let sx0 = tx * atlas.cellw / bw;
            let sx1 = (((tx + 1) * atlas.cellw / bw).max(sx0 + 1)).min(atlas.cellw);
            let mut sum = 0u32;
            let mut n = 0u32;
            for yy in sy0..sy1 {
                for xx in sx0..sx1 {
                    sum += atlas.cov[(celly + yy) * atlas.w + (cellx + xx)] as u32;
                    n += 1;
                }
            }
            let cov = (sum / n.max(1)) as usize;
            if cov == 0 {
                continue;
            }
            let px = x + tx;
            if px >= info.width {
                continue;
            }
            let bg = get_px(scene, info, px, py);
            put(scene, info, px, py, lerp(bg, color, cov, 255));
        }
    }
}

/// Copy `src` over `dst` (copies the shorter length) a **qword at a time**
/// (`rep movsq`) with a byte remainder. Two reasons not to use the generic
/// `copy_from_slice`:
///   * On real hardware `rep movs` produces the write-combining bursts a WC
///     framebuffer wants (and uses the CPU's fast-string path).
///   * Under QEMU's TCG emulation each `rep movs` *iteration* is interpreted, so
///     the element width matters a lot — `rep movsb` is emulated byte-by-byte
///     (8× the iterations) and was measurably slower; `rep movsq` moves 8 bytes
///     per iteration, matching the old word-wide `memcpy`.
/// DF is 0 per the ABI (and our `_start` `cld`).
#[inline]
fn fast_copy(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    if n == 0 {
        return;
    }
    let words = n / 8;
    let rem = n % 8;
    unsafe {
        core::arch::asm!(
            "rep movsq",        // bulk: 8 bytes per iteration (rcx = qwords)
            "mov rcx, {rem}",
            "rep movsb",        // tail: remaining 0..7 bytes
            rem = in(reg) rem,
            inout("rcx") words => _,
            inout("rsi") src.as_ptr() => _,
            inout("rdi") dst.as_mut_ptr() => _,
            options(nostack, preserves_flags),
        );
    }
}

/// Write one pixel into an arbitrary buffer (scene or hardware). Bounds are
/// the caller's responsibility except the final slice check.
#[inline]
fn put(buf: &mut [u8], info: &FbInfo, x: usize, y: usize, c: Rgb) {
    if x >= info.width || y >= info.height {
        return;
    }
    let bpp = info.bpp;
    let o = y * info.pitch + x * bpp;
    if o + bpp > buf.len() {
        return;
    }
    let px = &mut buf[o..o + bpp];
    if info.bgr {
        px[0] = c.2;
        px[1] = c.1;
        px[2] = c.0;
    } else {
        px[0] = c.0;
        px[1] = c.1;
        px[2] = c.2;
    }
}

/// Linear interpolate between `a` (num=0) and `b` (num=den) per channel.
#[inline]
fn lerp(a: Rgb, b: Rgb, num: usize, den: usize) -> Rgb {
    let d = den.max(1) as u32;
    let n = (num.min(den)) as u32;
    let mix = |x: u8, y: u8| ((x as u32 * (d - n) + y as u32 * n) / d) as u8;
    Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

pub struct Display {
    fb: &'static mut [u8],
    scene: Vec<u8>,
    /// Shadow copy (in RAM) of what was last pushed to the hardware buffer,
    /// *without* the cursor. [`Display::present`] diffs `scene` against this
    /// per scanline and copies only the rows that changed, so a partial update
    /// (a keystroke, a moved selection, a drawn line) writes a few KiB to VRAM
    /// instead of the whole framebuffer. Empty until the first present (and
    /// after a full [`Display::blit`]), which forces one full copy.
    shadow: Vec<u8>,
    /// TSC ticks the last [`Display::present`] took (for the on-screen FB
    /// readout — VRAM writes are the dominant cost on real hardware).
    last_present_ticks: u64,
    info: FbInfo,
    cursor_at: Option<(usize, usize)>,
    /// Per-view background bitmaps (index = `slot_for`); `None` => procedural.
    backgrounds: [Option<OwnedImage>; 5],
    /// Shared background, tinted per view; used when a slot has no own bitmap.
    bg_shared: Option<OwnedImage>,
    /// Higher-resolution glyph atlas; `None` => built-in 8×8 font.
    atlas: Option<GlyphAtlas>,
    /// Composited background per view (index = `slot_for`), scene-sized.
    /// Composing one (a bilinear upscale of a bitmap, or the procedural
    /// gradient+starfield) is full-screen and costly under emulation, so each
    /// view's result is kept and reused. That makes not just same-view repaints
    /// but also switching *back* to an already-visited view a single memcpy —
    /// previously only one composite was cached, so every view change paid the
    /// full ~1 s recompute. See [`Display::paint_background`].
    bg_cache: [Option<Vec<u8>>; 5],
}

impl Display {
    pub fn width(&self) -> usize {
        self.info.width
    }
    pub fn height(&self) -> usize {
        self.info.height
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Rgb) {
        let (mw, mh) = (self.info.width, self.info.height);
        for yy in y..(y + h).min(mh) {
            for xx in x..(x + w).min(mw) {
                put(&mut self.scene, &self.info, xx, yy, c);
            }
        }
    }

    /// Draw one glyph at pixel `(x, y)`, `SCALE`-magnified, into the scene.
    pub fn draw_glyph(&mut self, x: usize, y: usize, ch: char, fg: Rgb, font: Font) {
        self.paint_glyph(x, y, ch, fg, fg, font, SCALE);
    }

    /// One glyph, `scale`-magnified, with a vertical `top`→`bot` colour
    /// gradient. Uses the higher-resolution atlas (area-averaged + alpha
    /// blended) when one is loaded, otherwise the built-in 8×8 face. The
    /// `Display` face folds a–z to A–Z before the atlas lookup, preserving the
    /// all-caps chrome convention.
    fn paint_glyph(
        &mut self,
        x: usize,
        y: usize,
        ch: char,
        top: Rgb,
        bot: Rgb,
        font: Font,
        scale: usize,
    ) {
        let bw = font::GLYPH_W * scale;
        let bh = font::GLYPH_H * scale;
        let info = self.info;
        // Display face is all-caps chrome: Unicode-fold so ż→Ż and α→Α (the
        // 8×8 fallback in `font.rs` applies the same fold).
        let lookup = if font == Font::Display {
            ch.to_uppercase().next().unwrap_or(ch)
        } else {
            ch
        };
        if let Some(a) = &self.atlas {
            if let Some(cell) = atlas_cell(lookup) {
                if cell < a.cols * a.rows && (a.filled[cell] || lookup == ' ') {
                    paint_atlas_glyph(&mut self.scene, &info, a, cell, x, y, bw, bh, top, bot);
                    return;
                }
            }
        }
        // 1bpp fallback (font8x8 / display face), per-row gradient.
        let g = font::glyph(font, ch);
        let den = bh.saturating_sub(1).max(1);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..font::GLYPH_W {
                if bits & (1 << col) != 0 {
                    for sy in 0..scale {
                        let py = row * scale + sy;
                        let c = lerp(top, bot, py, den);
                        for sx in 0..scale {
                            put(&mut self.scene, &info, x + col * scale + sx, y + py, c);
                        }
                    }
                }
            }
        }
    }

    // ---- embedded-asset installers (called once at boot) -----------------

    /// Install a decoded per-view background (RGBA). `slot` follows
    /// [`slot_for`]; out-of-range or short buffers are ignored.
    pub fn set_background(&mut self, slot: usize, w: usize, h: usize, rgba: Vec<u8>) {
        if slot < 5 && rgba.len() >= w * h * 4 {
            self.backgrounds[slot] = Some(OwnedImage { w, h, rgba });
            self.bg_cache[slot] = None; // invalidate this view's cached composite
        }
    }

    /// Install the shared (per-view-tinted) background (RGBA).
    pub fn set_shared_bg(&mut self, w: usize, h: usize, rgba: Vec<u8>) {
        if rgba.len() >= w * h * 4 {
            self.bg_shared = Some(OwnedImage { w, h, rgba });
            self.bg_cache = [None, None, None, None, None]; // shared bg feeds every view
        }
    }

    /// Install the glyph atlas from a decoded RGBA `font.png`. Expects a 16×7
    /// cell grid (see `ASSET_PROMPTS.md`); a size that isn't an exact multiple
    /// is rejected and the 8×8 font stays in use.
    pub fn set_atlas(&mut self, w: usize, h: usize, rgba: Vec<u8>) {
        // 16 columns of square cells; the row count comes from the image
        // height, so the atlas can grow more glyph rows without a code change.
        let cols = 16usize;
        if w == 0 || h == 0 || w % cols != 0 || rgba.len() < w * h * 4 {
            return;
        }
        let (cellw, cellh) = (w / cols, w / cols);
        if h % cellh != 0 {
            return;
        }
        let rows = h / cellh;
        let mut cov = vec![0u8; w * h];
        for (i, c) in cov.iter_mut().enumerate() {
            // White-on-black: coverage = brightest channel.
            *c = rgba[i * 4].max(rgba[i * 4 + 1]).max(rgba[i * 4 + 2]);
        }
        let mut filled = vec![false; cols * rows];
        for cy in 0..rows {
            for cx in 0..cols {
                let mut s = 0u32;
                for yy in 0..cellh {
                    for xx in 0..cellw {
                        s += cov[(cy * cellh + yy) * w + (cx * cellw + xx)] as u32;
                    }
                }
                filled[cy * cols + cx] = s > 0;
            }
        }
        self.atlas = Some(GlyphAtlas { w, cols, rows, cellw, cellh, cov, filled });
    }

    /// Fill the scene with this view's background, **cached**: the first call
    /// for a given `tint` composites it (a per-view bitmap blitted as-authored,
    /// else the shared bitmap tinted toward the view's floor colour, else the
    /// procedural `top`→`tint` gradient + starfield) and stores the result;
    /// every later call for the same view is a single memcpy from the cache.
    /// This keeps repaints cheap — composing is full-screen and, for a bitmap,
    /// a bilinear upscale of millions of pixels.
    pub fn paint_background(&mut self, top: Rgb, tint: Rgb) {
        let slot = slot_for(tint);
        // Fast path: reuse this view's cached composite (a plain memcpy).
        if self.bg_cache[slot].as_ref().map(|c| c.len()) == Some(self.scene.len()) {
            // take/put-back so the immutable cache borrow doesn't clash with
            // the mutable `scene` borrow.
            let cached = self.bg_cache[slot].take().unwrap();
            fast_copy(&mut self.scene, &cached);
            self.bg_cache[slot] = Some(cached);
            return;
        }
        // Compose fresh.
        let info = self.info;
        if let Some(img) = &self.backgrounds[slot] {
            blit_stretch(&mut self.scene, &info, img, None);
        } else if let Some(img) = &self.bg_shared {
            blit_stretch(&mut self.scene, &info, img, Some(norm_hue(tint)));
        } else {
            self.backdrop(top, tint);
            self.starfield(160);
        }
        // Cache this view's composite for every later repaint and revisit.
        self.bg_cache[slot] = Some(self.scene.clone());
    }

    /// Draw UTF-8 text in `font`; returns the x advance. No wrapping (caller
    /// decides). Both faces advance one cell per glyph, so a `Display` run and
    /// a `Body` run line up column-for-column.
    pub fn draw_text(&mut self, x: usize, y: usize, s: &str, fg: Rgb, font: Font) -> usize {
        let mut cx = x;
        for ch in s.chars() {
            self.draw_glyph(cx, y, ch, fg, font);
            cx += CELL_W;
        }
        cx
    }

    /// Paint the whole scene with a vertical gradient from `top` (y=0) to
    /// `bottom` (y=height), with subtle CRT scanlines folded in. This is the
    /// sci-fi backdrop every screen sits on; the gradient floor is the
    /// per-screen tint, so screens stay colour-coded.
    pub fn backdrop(&mut self, top: Rgb, bottom: Rgb) {
        let (w, h) = (self.info.width, self.info.height);
        let denom = (h.max(2) - 1) as u32;
        for y in 0..h {
            let t = y as u32;
            let lerp = |a: u8, b: u8| -> u32 {
                (a as u32 * (denom - t) + b as u32 * t) / denom
            };
            let (mut r, mut g, mut b) = (lerp(top.0, bottom.0), lerp(top.1, bottom.1), lerp(top.2, bottom.2));
            // Darken alternate 2px bands for a soft scanline shimmer.
            if (y / 2) & 1 == 1 {
                r = r * 84 / 100;
                g = g * 84 / 100;
                b = b * 84 / 100;
            }
            let c = Rgb(r as u8, g as u8, b as u8);
            for x in 0..w {
                put(&mut self.scene, &self.info, x, y, c);
            }
        }
    }

    /// Scatter `count` deterministic stars across the scene. The same LCG seed
    /// runs every frame, so stars hold still between repaints instead of
    /// twinkling on each keypress.
    pub fn starfield(&mut self, count: usize) {
        let (w, h) = (self.info.width, self.info.height);
        if w == 0 || h == 0 {
            return;
        }
        let mut s: u32 = 0x9E37_79B9;
        let mut next = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            s
        };
        for _ in 0..count {
            let x = next() as usize % w;
            let y = next() as usize % h;
            let c = match next() % 8 {
                0 => Rgb(0xcf, 0xe9, 0xff), // rare bright star
                1 | 2 => Rgb(0x6f, 0x93, 0xb4),
                _ => Rgb(0x33, 0x4c, 0x66), // faint dust
            };
            put(&mut self.scene, &self.info, x, y, c);
        }
    }

    /// Stroke a `t`-pixel-thick rectangle outline.
    pub fn stroke_rect(&mut self, x: usize, y: usize, w: usize, h: usize, t: usize, c: Rgb) {
        if w == 0 || h == 0 {
            return;
        }
        self.fill_rect(x, y, w, t, c);
        self.fill_rect(x, y + h.saturating_sub(t), w, t, c);
        self.fill_rect(x, y, t, h, c);
        self.fill_rect(x + w.saturating_sub(t), y, t, h, c);
    }

    /// Draw text with a neon halo: the string is stamped in `glow` at four
    /// offsets, then crisply in `fg` on top. Reserve for the title — it costs
    /// five glyph passes.
    pub fn draw_text_glow(&mut self, x: usize, y: usize, s: &str, fg: Rgb, glow: Rgb, font: Font) {
        const O: usize = 2;
        self.draw_text(x + O, y, s, glow, font);
        self.draw_text(x.saturating_sub(O), y, s, glow, font);
        self.draw_text(x, y + O, s, glow, font);
        self.draw_text(x, y.saturating_sub(O), s, glow, font);
        self.draw_text(x, y, s, fg, font);
    }

    /// One glyph, `scale`-magnified, filled with a vertical `top`→`bot`
    /// gradient across its height (atlas-aware via [`Self::paint_glyph`]).
    fn glyph_grad(
        &mut self,
        x: usize,
        y: usize,
        ch: char,
        top: Rgb,
        bot: Rgb,
        font: Font,
        scale: usize,
    ) {
        self.paint_glyph(x, y, ch, top, bot, font, scale);
    }

    /// Draw `scale`-magnified, gradient-filled UTF-8 text; returns the x
    /// advance. Each glyph advances `GLYPH_W * scale`.
    pub fn draw_text_grad(
        &mut self,
        x: usize,
        y: usize,
        s: &str,
        top: Rgb,
        bot: Rgb,
        font: Font,
        scale: usize,
    ) -> usize {
        let mut cx = x;
        for ch in s.chars() {
            self.glyph_grad(cx, y, ch, top, bot, font, scale);
            cx += font::GLYPH_W * scale;
        }
        cx
    }

    /// Gradient text with a neon halo, at an arbitrary scale. The headline
    /// treatment: a flat `glow` stamped at four offsets, then the gradient
    /// fill crisply on top.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_glow_grad(
        &mut self,
        x: usize,
        y: usize,
        s: &str,
        top: Rgb,
        bot: Rgb,
        glow: Rgb,
        font: Font,
        scale: usize,
    ) {
        let o = scale;
        self.draw_text_grad(x + o, y, s, glow, glow, font, scale);
        self.draw_text_grad(x.saturating_sub(o), y, s, glow, glow, font, scale);
        self.draw_text_grad(x, y + o, s, glow, glow, font, scale);
        self.draw_text_grad(x, y.saturating_sub(o), s, glow, glow, font, scale);
        self.draw_text_grad(x, y, s, top, bot, font, scale);
    }

    /// Fill a rectangle with a vertical `top`→`bot` gradient. Used for the HUD
    /// bars so they read as lit panels.
    pub fn fill_grad_rect(&mut self, x: usize, y: usize, w: usize, h: usize, top: Rgb, bot: Rgb) {
        let (mw, mh) = (self.info.width, self.info.height);
        let den = h.saturating_sub(1);
        for yy in 0..h {
            let py = y + yy;
            if py >= mh {
                break;
            }
            let c = lerp(top, bot, yy, den);
            for xx in x..(x + w).min(mw) {
                put(&mut self.scene, &self.info, xx, py, c);
            }
        }
    }

    /// Blit the [`FRAME_CORNER`] emblem `SCALE`-magnified at `(x, y)`, mirrored
    /// per `flip_x`/`flip_y` so one authored tile serves all four corners. The
    /// three brightness levels map to `accent`/`mid`/`dim`.
    pub fn corner_emblem(
        &mut self,
        x: usize,
        y: usize,
        accent: Rgb,
        mid: Rgb,
        dim: Rgb,
        flip_x: bool,
        flip_y: bool,
    ) {
        for ry in 0..EMBLEM_N {
            for rx in 0..EMBLEM_N {
                let lvl = FRAME_CORNER[ry][rx];
                if lvl == 0 {
                    continue;
                }
                let c = match lvl {
                    3 => accent,
                    2 => mid,
                    _ => dim,
                };
                let cx = if flip_x { EMBLEM_N - 1 - rx } else { rx };
                let cy = if flip_y { EMBLEM_N - 1 - ry } else { ry };
                for sy in 0..SCALE {
                    for sx in 0..SCALE {
                        put(&mut self.scene, &self.info, x + cx * SCALE + sx, y + cy * SCALE + sy, c);
                    }
                }
            }
        }
    }

    /// Rows of text that fit.
    pub fn rows(&self) -> usize {
        self.info.height / CELL_H
    }

    // ---- compositing -----------------------------------------------------

    fn paint_cursor(&mut self) {
        let Some((cx, cy)) = self.cursor_at else {
            return;
        };
        for ry in 0..CURSOR_H {
            for rx in 0..CURSOR_W {
                let c = match CURSOR[ry][rx] {
                    1 => C_ACCENT,
                    2 => Rgb(0x01, 0x06, 0x0e),
                    3 => Rgb(0xff, 0xff, 0xff),
                    _ => continue,
                };
                put(self.fb, &self.info, cx + rx, cy + ry, c);
            }
        }
    }

    /// Restore the rectangle the cursor last occupied from the clean scene.
    fn restore_cursor_bg(&mut self, ox: usize, oy: usize) {
        let bpp = self.info.bpp;
        for ry in 0..CURSOR_H {
            let py = oy + ry;
            if py >= self.info.height {
                continue;
            }
            for rx in 0..CURSOR_W {
                let px = ox + rx;
                if px >= self.info.width {
                    continue;
                }
                let o = py * self.info.pitch + px * bpp;
                if o + bpp <= self.fb.len() {
                    self.fb[o..o + bpp].copy_from_slice(&self.scene[o..o + bpp]);
                }
            }
        }
    }

    /// Blit the whole scene to the screen (no cursor). For fatal screens.
    pub fn blit(&mut self) {
        let n = self.fb.len().min(self.scene.len());
        fast_copy(&mut self.fb[..n], &self.scene[..n]);
        self.cursor_at = None;
        self.shadow.clear(); // force the next present() to do a full copy
    }

    /// Present a freshly drawn scene and stamp the pointer at `cursor`.
    ///
    /// Only the scanlines that changed since the last present are copied to the
    /// hardware buffer. The compare runs against the in-RAM `shadow` (cheap);
    /// VRAM — the expensive side on real hardware — sees only the changed rows.
    /// A full-screen change (e.g. switching views) still copies everything;
    /// the common case of a small edit copies a handful of rows.
    pub fn present(&mut self, cursor: (usize, usize)) {
        let t0 = unsafe { core::arch::x86_64::_rdtsc() };
        // Erase the previous pointer using the clean scene first, so a scanline
        // the diff leaves untouched can't keep a stale cursor ghost.
        if let Some((ox, oy)) = self.cursor_at.take() {
            self.restore_cursor_bg(ox, oy);
        }
        let n = self.fb.len().min(self.scene.len());
        if self.shadow.len() != self.scene.len() {
            // First present, or after a full blit: copy everything, seed shadow.
            fast_copy(&mut self.fb[..n], &self.scene[..n]);
            self.shadow.clear();
            self.shadow.extend_from_slice(&self.scene);
        } else {
            let pitch = self.info.pitch;
            for y in 0..self.info.height {
                let s = y * pitch;
                let e = (s + pitch).min(n);
                if s >= e {
                    break;
                }
                if self.scene[s..e] != self.shadow[s..e] {
                    fast_copy(&mut self.fb[s..e], &self.scene[s..e]);
                    fast_copy(&mut self.shadow[s..e], &self.scene[s..e]);
                }
            }
        }
        self.cursor_at = Some(cursor);
        self.paint_cursor();
        let t1 = unsafe { core::arch::x86_64::_rdtsc() };
        self.last_present_ticks = t1.wrapping_sub(t0);
    }

    /// Move just the pointer (no scene repaint): erase the old position from
    /// the scene, draw it at the new one. This is what makes mouse motion
    /// cheap and never disturbs the rendered screen.
    pub fn move_cursor(&mut self, x: usize, y: usize) {
        if let Some((ox, oy)) = self.cursor_at {
            if (ox, oy) == (x, y) {
                return;
            }
            self.restore_cursor_bg(ox, oy);
        }
        self.cursor_at = Some((x, y));
        self.paint_cursor();
    }
}

static DISPLAY: Mutex<Option<Display>> = Mutex::new(None);

/// Install the framebuffer the bootloader gave us. `buffer` is `'static`.
pub fn init(buffer: &'static mut [u8], info: FbInfo) {
    let scene = vec![0u8; buffer.len()];
    let mut d = DISPLAY.lock();
    *d = Some(Display {
        fb: buffer,
        scene,
        shadow: Vec::new(),
        last_present_ticks: 0,
        info,
        cursor_at: None,
        backgrounds: [None, None, None, None, None],
        bg_shared: None,
        atlas: None,
        bg_cache: [None, None, None, None, None],
    });
}

/// TSC ticks the most recent `present()` took (0 if no display). Divide by
/// `time::tsc_per_us()` for microseconds — surfaced in the HUD readout so the
/// real cost of a frame is visible on hardware that has no serial console.
pub fn last_present_ticks() -> u64 {
    with(|d| d.last_present_ticks).unwrap_or(0)
}

/// Run `f` against the display with interrupts masked (input IRQs also move
/// the cursor, so all drawing is serialised).
pub fn with<R>(f: impl FnOnce(&mut Display) -> R) -> Option<R> {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| DISPLAY.lock().as_mut().map(f))
}

/// Used by the panic handler: paint the hardware buffer red directly.
pub unsafe fn emergency_fill(r: u8, g: u8, b: u8) {
    if let Some(d) = DISPLAY.lock().as_mut() {
        let info = d.info;
        for y in 0..info.height {
            for x in 0..info.width {
                put(d.fb, &info, x, y, Rgb(r, g, b));
            }
        }
    }
}
