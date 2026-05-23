//! Boot-time loading of the optional embedded bitmap assets.
//!
//! `build.rs` generates `assets_gen.rs` (in `OUT_DIR`) with one
//! `Option<&[u8]>` per asset — `Some(include_bytes!(..))` when the PNG exists
//! in `kernel/assets/`, else `None`. Here we decode whatever is present with
//! the `pngdec` crate and hand the RGBA pixels to the framebuffer, which uses
//! them in place of the procedural backdrop / 8×8 font. Anything missing or
//! undecodable is logged on the serial console and falls back silently.

use crate::framebuffer as fbm;
use crate::serial_println;

// Generated: BG_LIST, BG_BROWSER, BG_ROW, BG_EDIT, BG_SCHEMA, BG_SHARED, FONT.
include!(concat!(env!("OUT_DIR"), "/assets_gen.rs"));

/// Attribution + full SIL OFL 1.1 text for the bundled glyph atlas, embedded
/// verbatim so the notice and license ship *inside* the bootable image — not
/// just as a loose source file. OFL 1.1 clause 2 requires every redistributed
/// copy to carry the copyright notice and license in a user-viewable form; the
/// UI's About / Licenses screen renders this string to satisfy that.
pub const FONT_LICENSE: &str = include_str!("../assets/FONT_LICENSE.txt");

/// TablesOS's own code license (MIT), embedded verbatim from the workspace
/// `LICENSE` file. MIT requires the copyright + permission notice to travel
/// with every copy or substantial portion of the software, so — like the font
/// notice — it ships inside the bootable image and is shown on the About /
/// Licenses screen.
pub const PROJECT_LICENSE: &str = include_str!("../../LICENSE");

/// Decode and install every embedded asset. Call once, after
/// `framebuffer::init`.
pub fn install() {
    // Per-view backgrounds: slot order matches `framebuffer::slot_for`.
    load_background(BG_LIST, 0, "bg_list.png");
    load_background(BG_BROWSER, 1, "bg_browser.png");
    load_background(BG_ROW, 2, "bg_row.png");
    load_background(BG_EDIT, 3, "bg_edit.png");
    load_background(BG_SCHEMA, 4, "bg_schema.png");

    if let Some(bytes) = BG_SHARED {
        match pngdec::decode(bytes) {
            Ok(img) => {
                fbm::with(|d| d.set_shared_bg(img.width, img.height, img.rgba));
                serial_println!("asset bg.png: shared backdrop loaded");
            }
            Err(e) => serial_println!("asset bg.png: decode failed ({:?}), using fallback", e),
        }
    }

    if let Some(bytes) = FONT {
        match pngdec::decode(bytes) {
            Ok(img) => {
                let (w, h) = (img.width, img.height);
                fbm::with(|d| d.set_atlas(w, h, img.rgba));
                serial_println!("asset font.png: {}x{} glyph atlas loaded", w, h);
                // OFL 1.1 attribution — also viewable in-OS via About / Licenses.
                serial_println!(
                    "  glyph atlas derived from Cascadia Mono, (c) 2019-Present Microsoft \
                     Corporation, RFN 'Cascadia Code', SIL OFL 1.1"
                );
            }
            Err(e) => serial_println!("asset font.png: decode failed ({:?}), using 8x8 font", e),
        }
    }
}

fn load_background(asset: Option<&[u8]>, slot: usize, name: &str) {
    let Some(bytes) = asset else { return };
    match pngdec::decode(bytes) {
        Ok(img) => {
            fbm::with(|d| d.set_background(slot, img.width, img.height, img.rgba));
            serial_println!("asset {}: loaded", name);
        }
        Err(e) => serial_println!("asset {}: decode failed ({:?}), using fallback", name, e),
    }
}
