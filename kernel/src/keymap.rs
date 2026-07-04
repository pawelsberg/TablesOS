//! Keyboard layouts: one shared translation point for both input drivers.
//!
//! The canonical physical-key code is the **USB HID usage ID** (the boot
//! keyboard delivers these natively; the PS/2 driver converts set-1 scancodes
//! via [`ps2_to_usage`]). [`translate`] then maps `(usage, modifiers)` to a
//! [`Key`] through the active layout, so PS/2 and USB keyboards always agree
//! and a layout is defined exactly once.
//!
//! Layouts are static data: every layout is a set of per-usage **overrides**
//! over the US base table, four output levels per key (plain / Shift / AltGr /
//! Shift+AltGr), plus a dead-key composition list. Dead keys (the Greek tonos
//! and dialytika) emit nothing and combine with the next character; dead key
//! followed by space yields the accent itself, and an impossible combination
//! drops the accent and keeps the letter.
//!
//! Everything here is static tables and atomics — **no allocation, no locks**
//! — because [`translate`] runs inside the PS/2 keyboard IRQ handler (see the
//! heap-free-IRQ invariant in `ps2.rs`).

use crate::ps2::Key;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Modifier state relevant to character selection. Callers track it from
/// their own protocol (PS/2 make/break codes, HID report byte 0).
#[derive(Clone, Copy, Default)]
pub struct Mods {
    pub shift: bool,
    /// Right Alt (AltGr) — selects the third/fourth level on layouts that
    /// have one (Polish programmer, UK `€ ¦`).
    pub altgr: bool,
}

/// What one key level produces.
#[derive(Clone, Copy)]
enum Out {
    /// Nothing (level not defined on this key).
    None,
    /// A character.
    Ch(char),
    /// A dead key: remember the accent, combine with the next character.
    Dead(char),
}
use Out::{Ch, Dead};

/// The tonos / dialytika / tonos+dialytika accents, named by their standalone
/// (spacing) code points — also what a dead key followed by space produces.
const TONOS: char = '\u{0384}'; // ΄
const DIALYTIKA: char = '\u{00A8}'; // ¨
const BOTH: char = '\u{0385}'; // ΅

struct Layout {
    name: &'static str,
    /// Per-usage overrides `(hid_usage, [plain, shift, altgr, shift+altgr])`,
    /// consulted before the US base table.
    keys: &'static [(u8, [Out; 4])],
    /// Dead-key composition: `(accent, base, composed)`.
    compose: &'static [(char, char, char)],
}

// ---- US base ---------------------------------------------------------------

/// The US-QWERTY base table every layout falls back to for keys it does not
/// override. Mirrors the HID boot-keyboard usage page.
fn us_base(usage: u8, level: usize) -> Out {
    // Letters a..z (usages 0x04..=0x1D).
    if (0x04..=0x1D).contains(&usage) {
        let c = (b'a' + usage - 0x04) as char;
        return match level {
            0 => Ch(c),
            1 => Ch(c.to_ascii_uppercase()),
            _ => Out::None,
        };
    }
    // Digit row 1..0 (usages 0x1E..=0x27).
    if (0x1E..=0x27).contains(&usage) {
        const PLAIN: [char; 10] = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
        const SHIFTED: [char; 10] = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')'];
        let i = (usage - 0x1E) as usize;
        return match level {
            0 => Ch(PLAIN[i]),
            1 => Ch(SHIFTED[i]),
            _ => Out::None,
        };
    }
    let pair = |a: char, b: char| match level {
        0 => Ch(a),
        1 => Ch(b),
        _ => Out::None,
    };
    match usage {
        0x2C => Ch(' '),
        0x2D => pair('-', '_'),
        0x2E => pair('=', '+'),
        0x2F => pair('[', '{'),
        0x30 => pair(']', '}'),
        0x31 => pair('\\', '|'), // ANSI key next to Enter
        0x32 => pair('\\', '|'), // ISO "non-US #" key next to Enter
        0x33 => pair(';', ':'),
        0x34 => pair('\'', '"'),
        0x35 => pair('`', '~'),
        0x36 => pair(',', '<'),
        0x37 => pair('.', '>'),
        0x38 => pair('/', '?'),
        0x64 => pair('\\', '|'), // ISO 102nd key (left of Z)
        _ => Out::None,
    }
}

/// Layout-independent non-character keys.
fn control_key(usage: u8) -> Option<Key> {
    Some(match usage {
        0x28 | 0x58 => Key::Enter, // main + keypad Enter
        0x29 => Key::Esc,
        0x2A => Key::Backspace,
        0x2B => Key::Tab,
        0x4A => Key::Home,
        0x4B => Key::PageUp,
        0x4C => Key::Delete,
        0x4D => Key::End,
        0x4E => Key::PageDown,
        0x4F => Key::Right,
        0x50 => Key::Left,
        0x51 => Key::Down,
        0x52 => Key::Up,
        _ => return None,
    })
}

// ---- layouts ---------------------------------------------------------------

static US: Layout = Layout {
    name: "US",
    keys: &[],
    compose: &[],
};

/// UK: `£` on Shift+3, `@`/`"` swapped, `# ~` next to Enter, `\ |` on the
/// 102nd key, `¬`/`¦` on the backquote key, `€` on AltGr+4.
static UK: Layout = Layout {
    name: "UK",
    keys: &[
        (0x1F, [Ch('2'), Ch('"'), Out::None, Out::None]),
        (0x20, [Ch('3'), Ch('£'), Out::None, Out::None]),
        (0x21, [Ch('4'), Ch('$'), Ch('€'), Out::None]),
        (0x34, [Ch('\''), Ch('@'), Out::None, Out::None]),
        (0x31, [Ch('#'), Ch('~'), Out::None, Out::None]),
        (0x32, [Ch('#'), Ch('~'), Out::None, Out::None]),
        (0x35, [Ch('`'), Ch('¬'), Ch('¦'), Out::None]),
    ],
    compose: &[],
};

/// Polish (programmer): US plus an AltGr layer for the nine ogonek/acute/
/// stroke/dot letters, `€` on AltGr+U.
static PL: Layout = Layout {
    name: "Polish (programmer)",
    keys: &[
        (0x04, [Ch('a'), Ch('A'), Ch('ą'), Ch('Ą')]),
        (0x06, [Ch('c'), Ch('C'), Ch('ć'), Ch('Ć')]),
        (0x08, [Ch('e'), Ch('E'), Ch('ę'), Ch('Ę')]),
        (0x0F, [Ch('l'), Ch('L'), Ch('ł'), Ch('Ł')]),
        (0x11, [Ch('n'), Ch('N'), Ch('ń'), Ch('Ń')]),
        (0x12, [Ch('o'), Ch('O'), Ch('ó'), Ch('Ó')]),
        (0x16, [Ch('s'), Ch('S'), Ch('ś'), Ch('Ś')]),
        (0x18, [Ch('u'), Ch('U'), Ch('€'), Out::None]),
        (0x1B, [Ch('x'), Ch('X'), Ch('ź'), Ch('Ź')]),
        (0x1D, [Ch('z'), Ch('Z'), Ch('ż'), Ch('Ż')]),
    ],
    compose: &[],
};

/// Greek: the letter block remapped, `;` a dead tonos (Shift: dead
/// dialytika), Shift+W the dead tonos+dialytika, Q → `;`/`:` as on the
/// standard el-GR layout.
static GR: Layout = Layout {
    name: "Greek",
    keys: &[
        (0x04, [Ch('α'), Ch('Α'), Out::None, Out::None]),
        (0x05, [Ch('β'), Ch('Β'), Out::None, Out::None]),
        (0x06, [Ch('ψ'), Ch('Ψ'), Out::None, Out::None]),
        (0x07, [Ch('δ'), Ch('Δ'), Out::None, Out::None]),
        (0x08, [Ch('ε'), Ch('Ε'), Out::None, Out::None]),
        (0x09, [Ch('φ'), Ch('Φ'), Out::None, Out::None]),
        (0x0A, [Ch('γ'), Ch('Γ'), Out::None, Out::None]),
        (0x0B, [Ch('η'), Ch('Η'), Out::None, Out::None]),
        (0x0C, [Ch('ι'), Ch('Ι'), Out::None, Out::None]),
        (0x0D, [Ch('ξ'), Ch('Ξ'), Out::None, Out::None]),
        (0x0E, [Ch('κ'), Ch('Κ'), Out::None, Out::None]),
        (0x0F, [Ch('λ'), Ch('Λ'), Out::None, Out::None]),
        (0x10, [Ch('μ'), Ch('Μ'), Out::None, Out::None]),
        (0x11, [Ch('ν'), Ch('Ν'), Out::None, Out::None]),
        (0x12, [Ch('ο'), Ch('Ο'), Out::None, Out::None]),
        (0x13, [Ch('π'), Ch('Π'), Out::None, Out::None]),
        (0x14, [Ch(';'), Ch(':'), Out::None, Out::None]),
        (0x15, [Ch('ρ'), Ch('Ρ'), Out::None, Out::None]),
        (0x16, [Ch('σ'), Ch('Σ'), Out::None, Out::None]),
        (0x17, [Ch('τ'), Ch('Τ'), Out::None, Out::None]),
        (0x18, [Ch('θ'), Ch('Θ'), Out::None, Out::None]),
        (0x19, [Ch('ω'), Ch('Ω'), Out::None, Out::None]),
        (0x1A, [Ch('ς'), Dead(BOTH), Out::None, Out::None]),
        (0x1B, [Ch('χ'), Ch('Χ'), Out::None, Out::None]),
        (0x1C, [Ch('υ'), Ch('Υ'), Out::None, Out::None]),
        (0x1D, [Ch('ζ'), Ch('Ζ'), Out::None, Out::None]),
        (0x33, [Dead(TONOS), Dead(DIALYTIKA), Out::None, Out::None]),
    ],
    compose: &[
        (TONOS, 'α', 'ά'),
        (TONOS, 'ε', 'έ'),
        (TONOS, 'η', 'ή'),
        (TONOS, 'ι', 'ί'),
        (TONOS, 'ο', 'ό'),
        (TONOS, 'υ', 'ύ'),
        (TONOS, 'ω', 'ώ'),
        (TONOS, 'Α', 'Ά'),
        (TONOS, 'Ε', 'Έ'),
        (TONOS, 'Η', 'Ή'),
        (TONOS, 'Ι', 'Ί'),
        (TONOS, 'Ο', 'Ό'),
        (TONOS, 'Υ', 'Ύ'),
        (TONOS, 'Ω', 'Ώ'),
        (DIALYTIKA, 'ι', 'ϊ'),
        (DIALYTIKA, 'υ', 'ϋ'),
        (DIALYTIKA, 'Ι', 'Ϊ'),
        (DIALYTIKA, 'Υ', 'Ϋ'),
        (BOTH, 'ι', 'ΐ'),
        (BOTH, 'υ', 'ΰ'),
    ],
};

static LAYOUTS: [&Layout; 4] = [&US, &UK, &PL, &GR];

/// Index into [`LAYOUTS`] of the active layout.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// Pending dead-key accent (`char` bits; 0 = none). Shared by both input
/// sources so a tonos typed on PS/2 composes with a vowel typed on USB.
static PENDING: AtomicU32 = AtomicU32::new(0);

pub fn layout_names() -> impl Iterator<Item = &'static str> {
    LAYOUTS.iter().map(|l| l.name)
}

pub fn active_name() -> &'static str {
    LAYOUTS[ACTIVE.load(Ordering::Relaxed) % LAYOUTS.len()].name
}

/// Activate the layout with this display name (from [`layout_names`]).
/// Returns false (and keeps the current layout) for an unknown name.
pub fn set_layout(name: &str) -> bool {
    match LAYOUTS.iter().position(|l| l.name == name) {
        Some(i) => {
            ACTIVE.store(i, Ordering::Relaxed);
            PENDING.store(0, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Translate one pressed key (HID usage + modifiers) into a [`Key`] through
/// the active layout, handling dead-key composition. `None` means the press
/// produces nothing (unknown usage, empty level, or a dead key being stored).
pub fn translate(usage: u8, mods: Mods) -> Option<Key> {
    if let Some(k) = control_key(usage) {
        // Navigation/editing cancels a pending accent, like other OSes.
        PENDING.store(0, Ordering::Relaxed);
        return Some(k);
    }
    let level = (mods.shift as usize) | ((mods.altgr as usize) << 1);
    let lay = LAYOUTS[ACTIVE.load(Ordering::Relaxed) % LAYOUTS.len()];
    let out = lay
        .keys
        .iter()
        .find(|(u, _)| *u == usage)
        .map(|(_, levels)| levels[level])
        .unwrap_or_else(|| us_base(usage, level));
    match out {
        Out::None => None,
        Out::Dead(d) => {
            PENDING.store(d as u32, Ordering::Relaxed);
            None
        }
        Out::Ch(c) => {
            let pending = PENDING.swap(0, Ordering::Relaxed);
            if pending == 0 {
                return Some(Key::Char(c));
            }
            let d = char::from_u32(pending)?;
            if c == ' ' {
                return Some(Key::Char(d)); // dead + space = the accent itself
            }
            let composed = lay
                .compose
                .iter()
                .find(|&&(a, b, _)| a == d && b == c)
                .map(|&(_, _, x)| x);
            Some(Key::Char(composed.unwrap_or(c)))
        }
    }
}

/// Fold a character produced by the active layout back to the Latin letter on
/// the same **physical key**, for UI hotkey matching (`[c]reate` must work
/// when the C key types `ψ` on the Greek layout). Only non-ASCII characters
/// fold — ASCII output means a Latin-compatible layout where hotkeys already
/// match — and only via the letter-block usages, so punctuation that happens
/// to live on a letter key (Greek `;` on Q) keeps its own meaning.
pub fn hotkey_fold(c: char) -> char {
    if c.is_ascii() {
        return c;
    }
    let lay = LAYOUTS[ACTIVE.load(Ordering::Relaxed) % LAYOUTS.len()];
    for &(usage, levels) in lay.keys {
        if !(0x04..=0x1D).contains(&usage) {
            continue;
        }
        for (level, out) in levels.iter().enumerate() {
            if let Ch(x) = out {
                if *x == c {
                    let base = (b'a' + usage - 0x04) as char;
                    // Odd levels are the shifted ones → upper-case hotkey.
                    return if level & 1 == 1 {
                        base.to_ascii_uppercase()
                    } else {
                        base
                    };
                }
            }
        }
    }
    c
}

// ---- PS/2 set-1 scancode → HID usage ---------------------------------------

/// Convert a PS/2 set-1 make code (with its `E0` prefix flag) to the HID
/// usage of the same physical key. Modifier keys are not mapped — the PS/2
/// driver tracks those from make/break codes itself.
pub fn ps2_to_usage(make: u8, extended: bool) -> Option<u8> {
    if extended {
        return Some(match make {
            0x48 => 0x52, // Up
            0x50 => 0x51, // Down
            0x4B => 0x50, // Left
            0x4D => 0x4F, // Right
            0x47 => 0x4A, // Home
            0x4F => 0x4D, // End
            0x53 => 0x4C, // Delete
            0x49 => 0x4B, // PageUp
            0x51 => 0x4E, // PageDown
            0x1C => 0x58, // keypad Enter
            _ => return None,
        });
    }
    Some(match make {
        0x01 => 0x29, // Esc
        // Digit row 1..=0.
        0x02..=0x0B => 0x1E + (make - 0x02),
        0x0C => 0x2D, // -
        0x0D => 0x2E, // =
        0x0E => 0x2A, // Backspace
        0x0F => 0x2B, // Tab
        // q w e r t y u i o p
        0x10 => 0x14,
        0x11 => 0x1A,
        0x12 => 0x08,
        0x13 => 0x15,
        0x14 => 0x17,
        0x15 => 0x1C,
        0x16 => 0x18,
        0x17 => 0x0C,
        0x18 => 0x12,
        0x19 => 0x13,
        0x1A => 0x2F, // [
        0x1B => 0x30, // ]
        0x1C => 0x28, // Enter
        // a s d f g h j k l
        0x1E => 0x04,
        0x1F => 0x16,
        0x20 => 0x07,
        0x21 => 0x09,
        0x22 => 0x0A,
        0x23 => 0x0B,
        0x24 => 0x0D,
        0x25 => 0x0E,
        0x26 => 0x0F,
        0x27 => 0x33, // ;
        0x28 => 0x34, // '
        0x29 => 0x35, // `
        0x2B => 0x31, // \ (ANSI, next to Enter)
        // z x c v b n m
        0x2C => 0x1D,
        0x2D => 0x1B,
        0x2E => 0x06,
        0x2F => 0x19,
        0x30 => 0x05,
        0x31 => 0x11,
        0x32 => 0x10,
        0x33 => 0x36, // ,
        0x34 => 0x37, // .
        0x35 => 0x38, // /
        0x39 => 0x2C, // space
        0x56 => 0x64, // ISO 102nd key (left of Z)
        _ => return None,
    })
}
