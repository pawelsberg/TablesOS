//! The GUI: the only thing the user ever sees. Keyboard-driven (the spec's
//! "input is keyboard and mouse" — the mouse is a visible pointer that can
//! click a list/grid row to activate it and click any on-screen `[…]` shortcut
//! to fire its key; every action also has a key so the system is fully operable
//! from the keyboard). Screens are colour-coded and map 1:1 to UI.md.
//!
//! Clicks resolve through the per-line [`Hit`] targets recorded while the frame
//! is built, so the layout code is the single source of truth for what each row
//! does — `on_click` never re-derives screen geometry.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use tablestore::schema::{Column, ForeignKey};
use tablestore::store::RowId;
use tablestore::value::Value;
use tablestore::{BlockDevice, Store, StoreError, Type};

use crate::ata::{self, DriveInfo, MbrInfo};
use crate::font::{self, Font};
use crate::framebuffer::{self as fbm, Rgb, CELL_H, CELL_W};
use crate::install::{self, InstallReport};
use crate::interrupts;
use crate::pci::{self, PciDevice};
use crate::ps2::{self, Event, Key};
use crate::rtc;
use crate::serial_println;
use crate::time;
use crate::upgrade::{self, UpgradeReport};
use crate::usb::ehci;
use crate::usb::xhci::{self, XhciInfo};

const MARGIN: usize = CELL_W; // one-cell border

// Char width at which the standalone field builder wraps long values across
// lines (rather than truncating them). Matches the value column the builder
// lays out after its 17-char label prefix.
const BUILDER_WRAP: usize = 60;

// Table Browser column sizing. Each column slot is its content width plus a
// one-char gap. `DEFAULT_COL_W` is the content width used when a column has no
// per-column [`display_width`](tablestore::schema::Column::display_width); the
// editor clamps an explicit width to `MIN_COL_W..=MAX_COL_W`.
const DEFAULT_COL_W: usize = 15;
const MIN_COL_W: u16 = 1;
const MAX_COL_W: u16 = 120;

// Sci-fi HUD geometry. Body text lives inside a corner-bracketed panel, framed
// by a glowing title bar above and a status bar below.
const TOPBAR_H: usize = 2 * CELL_H + 8; // glowing title strip
const BOTBAR_H: usize = CELL_H + 12; // status strip
const PANEL_X: usize = CELL_W / 2; // panel inset from the screen edge
const PANEL_Y: usize = TOPBAR_H + 6; // panel top
const BODY_X: usize = PANEL_X + 14; // text left padding inside the panel
// First body line — pushed below the top corner emblems (each spans
// `EMBLEM_PX` from the panel's top edge) plus a small gap, so the bracket
// chrome never crowds the text. `visible_rows` reserves the matching band at
// the bottom, so all four corners stay clear.
const BODY_TOP: usize = PANEL_Y + fbm::EMBLEM_PX + 8;

/// What to do once a modal (prompt / picker / confirm) resolves.
#[derive(Clone)]
enum Action {
    CreateTable,
    /// Resolves the Power picker ("Shut down" / "Reboot").
    PowerMenu,
    /// Resolves the keyboard-layout picker (choice = layout display name).
    KbdLayout,
    DeleteRow(String, RowId),
    DropTable(String),
    DropColumn(String, String),
    /// Drop a foreign key from `table`; the FK name comes from the picker
    /// selection when the modal resolves.
    DropFk(String),
    AddColPickType(String, String), // table, new column name
    RenameColumn(String, String),   // table, current column name
    /// Set a column's Table Browser display width; the entered number (blank
    /// clears it to the default) comes from the prompt when it resolves.
    SetDisplayWidth(String, String), // table, column name
    AddFkPickFrom(String),
    AddFkPickToTable(String, String), // table, from_col
    AddFkPickToCol(String, String, String), // table, from_col, to_table
    /// Install a fresh TablesOS image onto the given USB slot. The label
    /// is shown in the confirmation modal.
    CreateOs(u8, String),
    /// Top up (upgrade) the existing TablesOS volume on the given USB slot to
    /// the running version, keeping its data. Label shown in the confirmation.
    UpgradeOs(u8, String),
}

enum Screen {
    List {
        sel: usize,
    },
    Browser {
        table: String,
        row: usize,
        col: usize,
        top: usize,
        /// Leftmost visible column — the grid scrolls horizontally so the
        /// selected `col` stays on screen when a table is wider than the panel.
        col_off: usize,
        filter: Option<(String, Value)>,
        // (column index, ascending). NULLs always sort last regardless of
        // direction. `None` keeps the engine's natural retrieval order.
        sort: Option<(usize, bool)>,
    },
    RowView {
        table: String,
        id: RowId,
        sel: usize,
    },
    Editor(Editor),
    /// Structured component editor for a single field of the Row Editor below
    /// it on the stack: it breaks the value into labelled parts (date/time
    /// components, a number's sign/digits/fraction, or a string's text),
    /// composes a canonical value and writes it back into that field on
    /// commit. Reached with `→`; the date/time types also offer `[n]` to fill
    /// the current wall-clock value.
    Builder(FieldBuilder),
    Schema {
        table: String,
        sel: usize,
    },
    /// Choose the ordered set of **reference columns** for a table — the
    /// columns shown as a row's label wherever it appears as a reference
    /// (Row View relationships, FK fields, Browser FK cells). `order` is the
    /// working label order; `sel` indexes the full column list. Reached from
    /// the Schema Editor via `R`.
    RefCols {
        table: String,
        order: Vec<String>,
        sel: usize,
    },
    Prompt {
        title: String,
        buf: String,
        /// Insertion caret into `buf` (char index).
        caret: usize,
        action: Action,
    },
    Pick {
        title: String,
        options: Vec<String>,
        sel: usize,
        action: Action,
    },
    Confirm {
        msg: String,
        action: Action,
    },
    /// Read-only diagnostic listing of every legacy-IDE slot's drive.
    Drives {
        drives: Vec<DriveInfo>,
        sel: usize,
        top: usize,
    },
    /// Read-only PCI bus enumeration; foundation for the future USB stack.
    Pci {
        devices: Vec<PciDevice>,
        sel: usize,
        top: usize,
    },
    /// Read-only xHCI register inspection. Selected from a USB xHCI entry
    /// on the PCI screen via Enter. No writable controller state is
    /// touched here — that's the next pass of phase 4.
    Xhci {
        info: XhciInfo,
        dev: PciDevice,
        /// First content line shown — the register/port/probe dump easily
        /// overflows the panel, so the view scrolls (↑↓/PgUp/PgDn).
        top: usize,
    },
    /// Pick a target USB drive on which to install a fresh TablesOS
    /// image. Reached from the Table List via `n`.
    CreateOsPick {
        candidates: Vec<(u8, DriveInfo)>,
        sel: usize,
    },
    /// Detailed result panel shown after an install attempt.
    InstallResult {
        report: InstallReport,
    },
    /// Pick an existing (older-or-equal-version) TablesOS USB drive to top up to
    /// the running version, keeping its data. Reached from the Table List via
    /// `u`.
    UpgradePick {
        candidates: Vec<(u8, DriveInfo)>,
        sel: usize,
    },
    /// Detailed result panel shown after an upgrade ("top up version") attempt.
    UpgradeResult {
        report: UpgradeReport,
    },
    /// Scrollable "About / Licenses" panel. Carries the bundled-font
    /// attribution and the full SIL OFL 1.1 text (OFL clause 2: the notice and
    /// license must accompany every copy in a user-viewable form).
    About {
        top: usize,
    },
    /// Pick a value from the target table of a foreign key. Reached from the Row
    /// Editor when opening the builder (PgDn) on an FK field. Shows all rows
    /// from the target table with their reference labels. On selection, the
    /// target column's value is extracted and filled back into the FK field.
    FkPick {
        field_idx: usize,         // Index in the Editor's fields to fill on selection
        from_col: String,         // Name of the FK source column (for feedback)
        to_table: String,         // Target table name
        to_col: String,           // Target column to extract (the column the FK references)
        rows: Vec<(RowId, String)>, // (row id in target table, reference label)
        sel: usize,               // Selected row index
        top: usize,               // For scrolling if many rows
    },
}

struct Field {
    value: String,
    is_null: bool,
    /// Insertion caret, a char index in `0..=value.chars().count()`. The text
    /// inputs all carry one so the arrow keys can navigate within a value.
    caret: usize,
}

struct Editor {
    table: String,
    id: Option<RowId>, // None = insert
    cols: Vec<Column>,
    fields: Vec<Field>,
    focus: usize, // 0..cols.len() = fields, then OK, then Cancel
    error: Option<(usize, String)>,
}

/// Which component of a value a [`PartField`] holds. Drives both how the field
/// is edited and where it lands when the value is composed. Covers the
/// calendar/clock parts of the date/time types and the sign/digits/text parts
/// of the numeric and string types.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PartId {
    // date / time
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Frac,
    OffSign,
    OffHour,
    OffMin,
    // numbers
    Sign,
    IntDigits,
    DecFrac,
    // string
    Text,
}

/// One labelled component in the field builder. `buf` holds the entered text;
/// `neg` is the sign for the toggle parts (`Year`/`OffSign`/`Sign`).
struct PartField {
    id: PartId,
    label: &'static str,
    buf: String,
    neg: bool,
    /// Insertion caret into `buf` (char index). Sign-only parts ignore it.
    caret: usize,
}

impl PartId {
    /// Digit cap. The fixed-width clock parts hold two; everything else is
    /// unbounded — the spec's unlimited year range, sub-second precision and
    /// numeric magnitude/scale, plus free-form string text.
    fn max_digits(self) -> usize {
        match self {
            PartId::Month
            | PartId::Day
            | PartId::Hour
            | PartId::Minute
            | PartId::Second
            | PartId::OffHour
            | PartId::OffMin => 2,
            _ => usize::MAX,
        }
    }

    /// A `+`/`-` toggle rather than a typed field.
    fn is_sign(self) -> bool {
        matches!(self, PartId::OffSign | PartId::Sign)
    }

    /// The one part that accepts arbitrary text, not just digits.
    fn is_text(self) -> bool {
        matches!(self, PartId::Text)
    }
}

struct FieldBuilder {
    field: usize, // index of the Row Editor field this writes back to
    col_name: String,
    ty: Type,
    fields: Vec<PartField>,
    focus: usize, // 0..fields.len() = fields, then [Now,] [Next,] OK, Cancel
    error: Option<String>,
    /// Canonical text of `max + 1` over the column's existing values, when this
    /// is an *insert* into a UNIQUE integer/unsigned column. `Some` enables the
    /// "[ Next ]" shortcut that fills a fresh, non-colliding key; `None`
    /// otherwise (edits, non-unique columns, non-integer types).
    next_value: Option<String>,
}

/// Does this type carry calendar/clock components (and thus a "fill current
/// value" option)?
fn is_datetime(ty: Type) -> bool {
    matches!(
        ty,
        Type::Date | Type::DateTz | Type::Time | Type::DateTime | Type::DateTimeTz
    )
}

/// Split a canonical signed-decimal string into `(negative, int, frac)`.
fn split_number(s: &str) -> (bool, String, String) {
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    match rest.split_once('.') {
        Some((i, fr)) => (neg, i.to_string(), fr.to_string()),
        None => (neg, rest.to_string(), String::new()),
    }
}

impl FieldBuilder {
    /// Build an empty part set for `ty`, in display order. Offset/sign parts
    /// default to non-negative (`+`).
    fn new(field: usize, col_name: String, ty: Type) -> FieldBuilder {
        let f = |id: PartId, label: &'static str| PartField {
            id,
            label,
            buf: String::new(),
            neg: false,
            caret: 0,
        };
        let date = || alloc::vec![f(PartId::Year, "Year"), f(PartId::Month, "Month"), f(PartId::Day, "Day")];
        let time = || {
            alloc::vec![
                f(PartId::Hour, "Hour"),
                f(PartId::Minute, "Minute"),
                f(PartId::Second, "Second"),
                f(PartId::Frac, "Sub-second"),
            ]
        };
        let off = || {
            alloc::vec![
                f(PartId::OffSign, "Zone sign"),
                f(PartId::OffHour, "Zone hour"),
                f(PartId::OffMin, "Zone min"),
            ]
        };
        let mut fields = Vec::new();
        match ty {
            Type::Integer => {
                fields.push(f(PartId::Sign, "Sign"));
                fields.push(f(PartId::IntDigits, "Digits"));
            }
            Type::UnsignedInteger => fields.push(f(PartId::IntDigits, "Digits")),
            Type::Decimal => {
                fields.push(f(PartId::Sign, "Sign"));
                fields.push(f(PartId::IntDigits, "Integer part"));
                fields.push(f(PartId::DecFrac, "Fraction"));
            }
            Type::String => fields.push(f(PartId::Text, "Text")),
            Type::Date => fields.extend(date()),
            Type::DateTz => {
                fields.extend(date());
                fields.extend(off());
            }
            Type::Time => fields.extend(time()),
            Type::DateTime => {
                fields.extend(date());
                fields.extend(time());
            }
            Type::DateTimeTz => {
                fields.extend(date());
                fields.extend(time());
                fields.extend(off());
            }
        }
        FieldBuilder {
            field,
            col_name,
            ty,
            fields,
            focus: 0,
            error: None,
            next_value: None,
        }
    }

    /// Does this builder offer the "fill current value" action?
    fn has_now(&self) -> bool {
        is_datetime(self.ty)
    }

    /// Does this builder offer the "fill max+1" action (UNIQUE integer/unsigned
    /// column on insert)?
    fn has_next(&self) -> bool {
        self.next_value.is_some()
    }

    /// Focus index of the `[ Now ]` button, if present.
    fn now_index(&self) -> Option<usize> {
        if self.has_now() {
            Some(self.fields.len())
        } else {
            None
        }
    }
    /// Focus index of the `[ Next ]` button, if present. It follows `[ Now ]`
    /// (the two are mutually exclusive in practice — a column is never both a
    /// date/time and an integer — but the layout stays well-defined regardless).
    fn next_index(&self) -> Option<usize> {
        if self.has_next() {
            Some(self.fields.len() + self.has_now() as usize)
        } else {
            None
        }
    }
    fn ok_index(&self) -> usize {
        self.fields.len() + self.has_now() as usize + self.has_next() as usize
    }
    fn cancel_index(&self) -> usize {
        self.ok_index() + 1
    }
    fn total(&self) -> usize {
        self.cancel_index() + 1
    }

    fn set(&mut self, id: PartId, buf: String, neg: bool) {
        if let Some(f) = self.fields.iter_mut().find(|f| f.id == id) {
            f.caret = buf.chars().count();
            f.buf = buf;
            f.neg = neg;
        }
    }

    fn get(&self, id: PartId) -> Option<&PartField> {
        self.fields.iter().find(|f| f.id == id)
    }

    /// Pre-fill the parts from an already-parsed value of this type. `set` is a
    /// no-op for parts this type doesn't have, so the numeric calls below are
    /// safe across integer / unsigned / decimal.
    fn load_from(&mut self, v: &Value) {
        let set_date = |s: &mut Self, d: &tablestore::value::Date| {
            s.set(PartId::Year, d.year.magnitude().to_dec_string(), d.year.is_negative());
            s.set(PartId::Month, num2(d.month), false);
            s.set(PartId::Day, num2(d.day), false);
        };
        let set_time = |s: &mut Self, t: &tablestore::value::TimeOfDay| {
            s.set(PartId::Hour, num2((t.secs / 3600) as u8), false);
            s.set(PartId::Minute, num2(((t.secs % 3600) / 60) as u8), false);
            s.set(PartId::Second, num2((t.secs % 60) as u8), false);
            let frac: String = t.frac.iter().map(|d| (b'0' + d) as char).collect();
            s.set(PartId::Frac, frac, false);
        };
        let set_off = |s: &mut Self, off: i16| {
            let a = off.unsigned_abs() as u32;
            s.set(PartId::OffSign, String::new(), off < 0);
            s.set(PartId::OffHour, num2((a / 60) as u8), false);
            s.set(PartId::OffMin, num2((a % 60) as u8), false);
        };
        match v {
            Value::Integer(_) | Value::Unsigned(_) | Value::Decimal(_) => {
                let (neg, int, frac) = split_number(&v.display());
                self.set(PartId::Sign, String::new(), neg);
                self.set(PartId::IntDigits, int, false);
                self.set(PartId::DecFrac, frac, false);
            }
            Value::Str(s) => self.set(PartId::Text, s.clone(), false),
            Value::Date(d) => set_date(self, d),
            Value::DateTz(d, off) => {
                set_date(self, d);
                set_off(self, *off);
            }
            Value::Time(t) => set_time(self, t),
            Value::DateTime(d, t) => {
                set_date(self, d);
                set_time(self, t);
            }
            Value::DateTimeTz(d, t, off) => {
                set_date(self, d);
                set_time(self, t);
                set_off(self, *off);
            }
        }
    }

    /// Replace the calendar/clock components with the current wall clock; the
    /// zone offset (if any) is reset to `+00:00`. Only meaningful for the
    /// date/time types. Returns false if the RTC could not be read.
    fn fill_now(&mut self) -> bool {
        let Some(n) = rtc::now() else {
            return false;
        };
        self.set(PartId::Year, alloc::format!("{}", n.year), false);
        self.set(PartId::Month, num2(n.month), false);
        self.set(PartId::Day, num2(n.day), false);
        self.set(PartId::Hour, num2(n.hour), false);
        self.set(PartId::Minute, num2(n.minute), false);
        self.set(PartId::Second, num2(n.second), false);
        self.set(PartId::Frac, String::new(), false);
        self.set(PartId::OffSign, String::new(), false);
        self.set(PartId::OffHour, num2(0), false);
        self.set(PartId::OffMin, num2(0), false);
        true
    }

    /// Fill the sign/digits parts from the precomputed `next_value` (`max + 1`).
    /// A no-op unless [`has_next`](FieldBuilder::has_next). The unsigned type has
    /// no `Sign` part, so that `set` is silently ignored there.
    fn fill_next(&mut self) -> bool {
        let Some(text) = self.next_value.clone() else {
            return false;
        };
        let (neg, int, _frac) = split_number(&text);
        self.set(PartId::Sign, String::new(), neg);
        self.set(PartId::IntDigits, int, false);
        true
    }

    /// Assemble the canonical text for this type from its parts, ready to hand
    /// to `Value::parse`. Fixed-width clock parts are zero-padded.
    fn compose(&self) -> String {
        let neg_of = |s: &Self, id: PartId| s.get(id).map(|f| f.neg).unwrap_or(false);
        let buf_of = |s: &Self, id: PartId| s.get(id).map(|f| f.buf.clone()).unwrap_or_default();
        let yr = |s: &Self| alloc::format!("{}{}", if neg_of(s, PartId::Year) { "-" } else { "" }, buf_of(s, PartId::Year));
        let p2 = |s: &Self, id: PartId| pad2(&buf_of(s, id));
        let date = |s: &Self| alloc::format!("{}-{}-{}", yr(s), p2(s, PartId::Month), p2(s, PartId::Day));
        let time = |s: &Self| {
            let mut t = alloc::format!(
                "{}:{}:{}",
                p2(s, PartId::Hour),
                p2(s, PartId::Minute),
                p2(s, PartId::Second)
            );
            let frac = buf_of(s, PartId::Frac);
            if !frac.is_empty() {
                t.push('.');
                t.push_str(&frac);
            }
            t
        };
        let off = |s: &Self| {
            alloc::format!(
                "{}{}:{}",
                if neg_of(s, PartId::OffSign) { "-" } else { "+" },
                p2(s, PartId::OffHour),
                p2(s, PartId::OffMin)
            )
        };
        let sign = |s: &Self| if neg_of(s, PartId::Sign) { "-" } else { "" };
        match self.ty {
            Type::Integer => alloc::format!("{}{}", sign(self), buf_of(self, PartId::IntDigits)),
            Type::UnsignedInteger => buf_of(self, PartId::IntDigits),
            Type::Decimal => {
                let mut d = alloc::format!("{}{}", sign(self), buf_of(self, PartId::IntDigits));
                let frac = buf_of(self, PartId::DecFrac);
                if !frac.is_empty() {
                    d.push('.');
                    d.push_str(&frac);
                }
                d
            }
            Type::String => buf_of(self, PartId::Text),
            Type::Date => date(self),
            Type::DateTz => alloc::format!("{}{}", date(self), off(self)),
            Type::Time => time(self),
            Type::DateTime => alloc::format!("{}T{}", date(self), time(self)),
            Type::DateTimeTz => alloc::format!("{}T{}{}", date(self), time(self), off(self)),
        }
    }
}

/// Two-digit zero-padded form of a small number (clock components).
fn num2(v: u8) -> String {
    alloc::format!("{:02}", v)
}

/// Left-pad digits to two for a fixed-width field; empty becomes `00`.
fn pad2(s: &str) -> String {
    match s.len() {
        0 => "00".to_string(),
        1 => alloc::format!("0{s}"),
        _ => s.to_string(),
    }
}

/// What a screen's key handler wants to do to the navigation stack, applied
/// only after the current screen has been put back.
enum Nav {
    None,
    Pop,
    Push(Screen),
    PopToList,
}

pub fn run<D: BlockDevice>(
    store: Store<D>,
    booted_sys_guid: [u8; 16],
    data_lba: u64,
) -> ! {
    let mut app = App {
        store,
        stack: alloc::vec![Screen::List { sel: 0 }],
        status: String::from("TablesOS ready"),
        booted_sys_guid,
        data_lba,
        last_usb_test: None,
        hitmap: Vec::new(),
        row_cache: None,
        cursor_on: true,
        last_blink: interrupts::ticks(),
    };
    app.render();
    loop {
        // Pull any USB-HID input into the shared input queue first, so the
        // drain below treats it exactly like a PS/2 event.
        xhci::pump_hid();
        ehci::pump_hid();
        // A full repaint blits the whole framebuffer, so doing one per input
        // event makes fast key-repeat (held arrows) enqueue faster than we can
        // draw — the screen appears to freeze under the backlog. Instead drain
        // every pending event, applying each, and repaint just once at the end.
        // We out-poll the input IRQs comfortably, so the queue empties promptly.
        let mut repaint = false;
        let mut interacted = false;
        let mut moved_to: Option<(usize, usize)> = None;
        while let Some(e) = ps2::poll() {
            match e {
                Event::MouseMove(x, y) => moved_to = Some((x, y)),
                Event::Click(x, y) => {
                    app.on_click(x, y);
                    repaint = true;
                    interacted = true;
                }
                Event::RightClick => {
                    app.on_right_click();
                    repaint = true;
                }
                Event::Key(k) => {
                    app.on_key(k);
                    repaint = true;
                    interacted = true;
                }
            }
        }
        // After typing or clicking, show the caret solid and restart its blink,
        // so it's steady while you work and only blinks once you pause — the
        // conventional behaviour.
        if interacted {
            app.cursor_on = true;
            app.last_blink = interrupts::ticks();
        }
        // Blink: while a text field is focused, flip the caret on a fixed
        // cadence. The timer IRQ already wakes the `hlt` below, so this
        // costs one repaint per half-period and nothing while idle elsewhere.
        if !repaint && app.has_text_cursor() {
            let now = interrupts::ticks();
            if now.wrapping_sub(app.last_blink) >= BLINK_TICKS {
                app.last_blink = now;
                app.cursor_on = !app.cursor_on;
                repaint = true;
            }
        }
        if repaint {
            // render() stamps the pointer at the current position itself, so a
            // pending move needs no separate handling here.
            app.render();
        } else if let Some((x, y)) = moved_to {
            app.draw_cursor(x, y);
        } else {
            // Idle: rest in `hlt` until the next IRQ. USB-HID input has no IRQ
            // — it is polled at the top of the loop — but the TICK_HZ (125 Hz)
            // timer bounds the sleep at one 8 ms poll period, so a USB pointer
            // stays smooth and a USB keyboard responsive while the CPU actually
            // sleeps instead of busy-spinning. PS/2 IRQs wake it even sooner.
            interrupts::wait_for_tick();
        }
    }
}

/// Caret blink half-period, in PIT ticks (`TICK_HZ`). ≈0.5 s.
const BLINK_TICKS: u64 = interrupts::TICK_HZ / 2;

struct App<D: BlockDevice> {
    store: Store<D>,
    stack: Vec<Screen>,
    status: String,
    /// System GUID captured at boot; the Drives screen marks the slot whose
    /// on-disk GUID matches as "this disk".
    booted_sys_guid: [u8; 16],
    /// Sectors of boot prefix (`[MBR | stage2 | kernel]`) in front of the
    /// TablesOS volume — needed when replicating that prefix to another
    /// drive in the install-to-USB flow.
    data_lba: u64,
    /// Latest USB-MSC write-test result, if any. Surfaced on the xHCI
    /// screen so the user has visible proof the BlockDevice path works.
    last_usb_test: Option<xhci::WriteTestResult>,
    /// Click targets for the body lines of the most recently rendered frame,
    /// indexed by body line. Lets `on_click` map a pointer position straight to
    /// the row/shortcut it is over, instead of re-deriving the layout.
    hitmap: Vec<ClickRow>,
    /// Memoised Table Browser row list. Decoding a table's rows is by far the
    /// costliest thing the GUI does (500 rows = 500 blob decodes); without this
    /// every keystroke re-decoded the whole table twice (dispatch + render),
    /// making navigation crawl. Reused while the table, filter, sort and store
    /// generation all match; any commit bumps the generation and invalidates it.
    row_cache: Option<RowCache>,
    /// Text-caret blink phase (drawn when true) and the PIT tick at which it
    /// last flipped. The timer IRQ wakes the idle `hlt` loop, which
    /// toggles this on a fixed cadence while a text field is focused.
    cursor_on: bool,
    last_blink: u64,
}

/// A decoded Browser row list, tagged with everything that would make it stale.
struct RowCache {
    table: String,
    filter: Option<(String, Value)>,
    sort: Option<(usize, bool)>,
    generation: u64,
    rows: Vec<(RowId, Vec<Option<Value>>)>,
}

impl<D: BlockDevice> App<D> {
    fn top(&mut self) -> &mut Screen {
        self.stack.last_mut().unwrap()
    }
    fn push(&mut self, s: Screen) {
        self.stack.push(s);
    }
    fn pop(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }
    fn err(&mut self, e: StoreError) {
        self.status = describe(&e);
    }

    /// Is a text field currently focused (so the blinking caret should run)?
    /// Mirrors which screens `build_frame` sets `Frame::cursor` for: the editor
    /// and builder when on a field (not the OK/Cancel buttons), and the prompt.
    fn has_text_cursor(&self) -> bool {
        match self.stack.last() {
            Some(Screen::Editor(ed)) => ed.focus < ed.cols.len(),
            Some(Screen::Builder(b)) => b.focus < b.fields.len(),
            Some(Screen::Prompt { .. }) => true,
            _ => false,
        }
    }

    // ---- input -----------------------------------------------------------

    fn on_key(&mut self, k: Key) {
        // Win+Space already cycled the layout inside `keymap::translate` (so
        // it works on every screen, even mid-text-entry); this is only the
        // feedback. Intercepted before the per-screen handlers below.
        if matches!(k, Key::LayoutSwitched) {
            self.status = format!("keyboard layout: {}", crate::keymap::active_name());
            return;
        }
        // Screens with a dedicated key handler that re-borrows `self` (the
        // store) internally. Matching on the variant — rather than an integer
        // tag that has to be kept in sync by hand — means adding a new `Screen`
        // forces a decision here at compile time instead of silently falling
        // through to the generic `dispatch` path below.
        match self.top() {
            Screen::Prompt { .. } => return self.key_prompt(k),
            Screen::Pick { .. } => return self.key_pick(k),
            Screen::Confirm { .. } => return self.key_confirm(k),
            Screen::Editor(_) => return self.key_editor(k),
            Screen::Builder(_) => return self.key_builder(k),
            // Everything else is handled by the generic `dispatch` path below.
            // Listed explicitly (no `_`) so adding a `Screen` forces a choice:
            // give it a dedicated handler here, or let it fall to `dispatch`.
            Screen::List { .. }
            | Screen::Browser { .. }
            | Screen::RowView { .. }
            | Screen::Schema { .. }
            | Screen::RefCols { .. }
            | Screen::Drives { .. }
            | Screen::Pci { .. }
            | Screen::Xhci { .. }
            | Screen::CreateOsPick { .. }
            | Screen::InstallResult { .. }
            | Screen::UpgradePick { .. }
            | Screen::UpgradeResult { .. }
            | Screen::About { .. }
            | Screen::FkPick { .. } => {}
        }
        // Hotkey screens: a letter typed on a non-Latin layout (Greek) folds
        // back to the Latin letter on the same physical key, so `[c]reate`,
        // `[p]ower` … work regardless of the active layout. Text entry
        // (Prompt/Editor/Builder) returned above and keeps the layout's
        // characters.
        let k = match k {
            Key::Char(c) => Key::Char(crate::keymap::hotkey_fold(c)),
            other => other,
        };
        // Pull the current screen out so screen logic can freely borrow
        // `self` (the store). It is put back *before* any navigation, so a
        // pushed screen is never clobbered.
        let scr = core::mem::replace(self.top(), Screen::List { sel: 0 });
        let (restored, nav) = self.dispatch(scr, k);
        *self.top() = restored;
        match nav {
            Nav::None => {}
            Nav::Push(s) => self.push(s),
            // Going *back* leaves the current screen, so a status that was an
            // instruction for it (e.g. "select a target drive") must not
            // outlive the screen. Result messages are set with `Nav::None` and
            // are left untouched.
            Nav::Pop => {
                self.status.clear();
                self.pop();
            }
            Nav::PopToList => {
                self.status.clear();
                while !matches!(self.top(), Screen::List { .. }) {
                    self.pop();
                }
            }
        }
    }

    fn dispatch(&mut self, scr: Screen, k: Key) -> (Screen, Nav) {
        let mut nav = Nav::None;
        let scr = match scr {
            Screen::List { mut sel } => {
                let tables = self.store.list_tables().unwrap_or_default();
                sel = sel.min(tables.len().saturating_sub(1));
                match k {
                    Key::Up if sel > 0 => sel -= 1,
                    Key::Down if sel + 1 < tables.len() => sel += 1,
                    Key::Enter if !tables.is_empty() => {
                        let t = tables[sel].name.clone();
                        nav = Nav::Push(Screen::Browser {
                            table: t,
                            row: 0,
                            col: 0,
                            top: 0,
                            col_off: 0,
                            filter: None,
                            sort: None,
                        });
                    }
                    Key::Char('c') | Key::Char('C') => {
                        nav = Nav::Push(Screen::Prompt {
                            title: "New table name".into(),
                            buf: String::new(),
                            caret: 0,
                            action: Action::CreateTable,
                        });
                    }
                    Key::Char('p') | Key::Char('P') => {
                        nav = Nav::Push(Screen::Pick {
                            title: "Power".into(),
                            options: alloc::vec!["Shut down".into(), "Reboot".into()],
                            sel: 0,
                            action: Action::PowerMenu,
                        });
                    }
                    Key::Char('k') | Key::Char('K') => {
                        let options: Vec<String> =
                            crate::keymap::layout_names().map(String::from).collect();
                        let sel = options
                            .iter()
                            .position(|n| n == crate::keymap::active_name())
                            .unwrap_or(0);
                        nav = Nav::Push(Screen::Pick {
                            title: "Keyboard layout".into(),
                            options,
                            sel,
                            action: Action::KbdLayout,
                        });
                    }
                    Key::Char('d') | Key::Char('D') => {
                        let mut drives = ata::enumerate_drives(&self.booted_sys_guid);
                        let usb = xhci::autopilot_usb_drives(&self.booted_sys_guid);
                        let usb_n = usb.len();
                        drives.extend(usb);
                        self.status = format!(
                            "enumerated {} IDE slot(s); {} USB drive(s) via autopilot",
                            drives.len() - usb_n,
                            usb_n
                        );
                        nav = Nav::Push(Screen::Drives {
                            drives,
                            sel: 0,
                            top: 0,
                        });
                    }
                    Key::Char('a') | Key::Char('A') => {
                        nav = Nav::Push(Screen::About { top: 0 });
                    }
                    Key::Char('n') | Key::Char('N') => {
                        // Run the USB autopilot then offer non-booted USB
                        // drives as install targets.
                        let _ = xhci::autopilot_usb_drives(&self.booted_sys_guid);
                        let candidates: Vec<(u8, DriveInfo)> =
                            xhci::usb_drives_with_slots(&self.booted_sys_guid)
                                .into_iter()
                                .filter(|(_, _, d)| !d.booted)
                                .map(|(_, slot, d)| (slot, d))
                                .collect();
                        if candidates.is_empty() {
                            self.status =
                                "no eligible USB target drives — plug one in and press [d] to refresh"
                                    .into();
                        } else {
                            self.status = format!(
                                "select a target drive ({} candidate(s))",
                                candidates.len()
                            );
                            nav = Nav::Push(Screen::CreateOsPick {
                                candidates,
                                sel: 0,
                            });
                        }
                    }
                    Key::Char('u') | Key::Char('U') => {
                        // Top up version: offer existing TablesOS USB volumes of
                        // an older-or-equal, known version (never the booted disk,
                        // never a newer one) as upgrade targets.
                        let _ = xhci::autopilot_usb_drives(&self.booted_sys_guid);
                        let upgradable = |d: &DriveInfo| -> bool {
                            if d.booted {
                                return false;
                            }
                            if let ata::MbrInfo::TablesOs { version, .. } = d.mbr {
                                version <= tablestore::VERSION
                                    && tablestore::migrate::is_known(version)
                            } else {
                                false
                            }
                        };
                        let candidates: Vec<(u8, DriveInfo)> =
                            xhci::usb_drives_with_slots(&self.booted_sys_guid)
                                .into_iter()
                                .filter(|(_, _, d)| upgradable(d))
                                .map(|(_, slot, d)| (slot, d))
                                .collect();
                        if candidates.is_empty() {
                            self.status =
                                "no upgradable TablesOS USB volumes — plug one in and press [d] to refresh"
                                    .into();
                        } else {
                            self.status = format!(
                                "select a volume to top up ({} candidate(s))",
                                candidates.len()
                            );
                            nav = Nav::Push(Screen::UpgradePick {
                                candidates,
                                sel: 0,
                            });
                        }
                    }
                    _ => {}
                }
                Screen::List { sel }
            }
            Screen::Browser {
                table,
                mut row,
                mut col,
                mut top,
                mut col_off,
                filter,
                mut sort,
            } => {
                let ncols = self
                    .store
                    .get_table(&table)
                    .map(|t| t.columns.len())
                    .unwrap_or(0);
                // Cached row count: pure navigation never decodes the table.
                let nrows = self.cached_rows(&table, &filter, sort).len();
                row = row.min(nrows.saturating_sub(1));
                col = col.min(ncols.saturating_sub(1));
                // One page = the rows visible at once, so PageUp/PageDown move
                // the cursor (and thus the viewport) by a whole screen.
                let vis = visible_rows();
                match k {
                    Key::Up if row > 0 => row -= 1,
                    Key::Down if row + 1 < nrows => row += 1,
                    Key::Left if col > 0 => col -= 1,
                    Key::Right if col + 1 < ncols => col += 1,
                    Key::PageUp => row = row.saturating_sub(vis),
                    Key::PageDown => row = (row + vis).min(nrows.saturating_sub(1)),
                    Key::Esc => nav = Nav::Pop,
                    // Cycle sort on the column under the cursor:
                    //   unsorted → ascending → descending → unsorted.
                    // The cursor sticks to the same RowId so the selection
                    // visually follows the row across reorderings.
                    Key::Char('o') if ncols > 0 => {
                        let anchor = self.cached_rows(&table, &filter, sort).get(row).map(|(id, _)| *id);
                        sort = match sort {
                            Some((c, true)) if c == col => Some((col, false)),
                            Some((c, false)) if c == col => None,
                            _ => Some((col, true)),
                        };
                        if let Some(id) = anchor {
                            if let Some(p) = self
                                .cached_rows(&table, &filter, sort)
                                .iter()
                                .position(|(rid, _)| *rid == id)
                            {
                                row = p;
                            }
                        }
                        self.status = match sort {
                            Some((_, true)) => "sorted ↑ on current column".into(),
                            Some((_, false)) => "sorted ↓ on current column".into(),
                            None => "sort cleared".into(),
                        };
                    }
                    Key::Enter if nrows > 0 => {
                        let id = self.cached_rows(&table, &filter, sort)[row].0;
                        nav = Nav::Push(Screen::RowView {
                            table: table.clone(),
                            id,
                            sel: 0,
                        });
                    }
                    Key::Char('i') => {
                        if let Some(s) = self.open_editor(&table, None) {
                            nav = Nav::Push(s);
                        }
                    }
                    Key::Char('u') if nrows > 0 => {
                        let id = self.cached_rows(&table, &filter, sort)[row].0;
                        if let Some(s) = self.open_editor(&table, Some(id)) {
                            nav = Nav::Push(s);
                        }
                    }
                    Key::Char('d') if nrows > 0 => {
                        let id = self.cached_rows(&table, &filter, sort)[row].0;
                        nav = Nav::Push(Screen::Confirm {
                            msg: format!("Delete the selected row from '{table}'?"),
                            action: Action::DeleteRow(table.clone(), id),
                        });
                    }
                    Key::Char('s') => {
                        nav = Nav::Push(Screen::Schema {
                            table: table.clone(),
                            sel: 0,
                        });
                    }
                    _ => {}
                }
                if row < top {
                    top = row;
                }
                if row >= top + vis {
                    top = row + 1 - vis;
                }
                // Horizontal scroll: keep the selected column on screen.
                col_off = self.browser_col_off(&table, col, col_off);
                Screen::Browser {
                    table,
                    row,
                    col,
                    top,
                    col_off,
                    filter,
                    sort,
                }
            }
            Screen::RowView {
                table,
                id,
                mut sel,
            } => {
                let navs = self.nav_targets(&table, id);
                sel = sel.min(navs.len().saturating_sub(1));
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up if sel > 0 => sel -= 1,
                    Key::Down if sel + 1 < navs.len() => sel += 1,
                    Key::Enter if !navs.is_empty() => {
                        let (tbl, colname, val) = navs[sel].1.clone();
                        nav = Nav::Push(Screen::Browser {
                            table: tbl,
                            row: 0,
                            col: 0,
                            top: 0,
                            col_off: 0,
                            filter: Some((colname, val)),
                            sort: None,
                        });
                    }
                    _ => {}
                }
                Screen::RowView { table, id, sel }
            }
            Screen::Schema { table, mut sel } => {
                let t = self.store.get_table(&table).ok();
                let n = t.as_ref().map(|t| t.columns.len()).unwrap_or(0);
                sel = sel.min(n.saturating_sub(1));
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up if sel > 0 => sel -= 1,
                    Key::Down if sel + 1 < n => sel += 1,
                    Key::Char('a') => {
                        nav = Nav::Push(Screen::Prompt {
                            title: "New column name".into(),
                            buf: String::new(),
                            caret: 0,
                            action: Action::AddColPickType(table.clone(), String::new()),
                        })
                    }
                    Key::Char('x') if n > 0 => {
                        let c = t.as_ref().unwrap().columns[sel].name.clone();
                        nav = Nav::Push(Screen::Confirm {
                            msg: format!("Drop column '{c}'?"),
                            action: Action::DropColumn(table.clone(), c),
                        });
                    }
                    Key::Char('r') if n > 0 => {
                        let c = t.as_ref().unwrap().columns[sel].name.clone();
                        nav = Nav::Push(Screen::Prompt {
                            title: format!("Rename column '{c}' to"),
                            caret: c.chars().count(),
                            buf: c.clone(),
                            action: Action::RenameColumn(table.clone(), c),
                        });
                    }
                    Key::Char('w') if n > 0 => {
                        let col = &t.as_ref().unwrap().columns[sel];
                        let buf = col
                            .display_width
                            .map(|w| w.to_string())
                            .unwrap_or_default();
                        nav = Nav::Push(Screen::Prompt {
                            title: format!(
                                "Browser width for '{}' (blank = default)",
                                col.name
                            ),
                            caret: buf.chars().count(),
                            buf,
                            action: Action::SetDisplayWidth(table.clone(), col.name.clone()),
                        });
                    }
                    Key::Char('u') if n > 0 => {
                        let col = t.as_ref().unwrap().columns[sel].clone();
                        match self.store.set_unique(&table, &col.name, !col.unique) {
                            Ok(_) => self.status = "UNIQUE toggled".into(),
                            Err(e) => self.err(e),
                        }
                    }
                    // Reorder columns: `,` moves the selected column up one
                    // position, `.` moves it down. Rows are permuted to match.
                    Key::Char(',') if n >= 2 && sel > 0 => {
                        let name = t.as_ref().unwrap().columns[sel].name.clone();
                        match self.store.move_column(&table, &name, sel - 1) {
                            Ok(_) => {
                                self.status = format!("moved '{name}' up");
                                sel -= 1;
                            }
                            Err(e) => self.err(e),
                        }
                    }
                    Key::Char('.') if n >= 2 && sel + 1 < n => {
                        let name = t.as_ref().unwrap().columns[sel].name.clone();
                        match self.store.move_column(&table, &name, sel + 1) {
                            Ok(_) => {
                                self.status = format!("moved '{name}' down");
                                sel += 1;
                            }
                            Err(e) => self.err(e),
                        }
                    }
                    Key::Char('k') => {
                        nav = Nav::Push(self.pick_columns(
                            &table,
                            "Foreign key: pick source column",
                            Action::AddFkPickFrom(table.clone()),
                        ))
                    }
                    Key::Char('K') => {
                        let fks: Vec<String> = t
                            .as_ref()
                            .map(|t| t.fks.iter().map(|f| f.name.clone()).collect())
                            .unwrap_or_default();
                        if !fks.is_empty() {
                            nav = Nav::Push(Screen::Pick {
                                title: "Drop which foreign key?".into(),
                                options: fks,
                                sel: 0,
                                action: Action::DropFk(table.clone()),
                            });
                        }
                    }
                    Key::Char('D') => {
                        nav = Nav::Push(Screen::Confirm {
                            msg: format!("Drop the WHOLE table '{table}' and all its rows?"),
                            action: Action::DropTable(table.clone()),
                        })
                    }
                    Key::Char('R') => {
                        let order = t.as_ref().map(|t| t.ref_cols.clone()).unwrap_or_default();
                        nav = Nav::Push(Screen::RefCols {
                            table: table.clone(),
                            order,
                            sel: 0,
                        });
                    }
                    _ => {}
                }
                Screen::Schema { table, sel }
            }
            Screen::RefCols {
                table,
                mut order,
                mut sel,
            } => {
                let cols: Vec<String> = self
                    .store
                    .get_table(&table)
                    .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default();
                let n = cols.len();
                if n > 0 {
                    sel = sel.min(n - 1);
                }
                // Every edit commits immediately (one transaction), matching
                // the rest of the Schema Editor; `Esc` just returns.
                let mut commit = false;
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up if sel > 0 => sel -= 1,
                    Key::Down if sel + 1 < n => sel += 1,
                    Key::Enter | Key::Char(' ') if n > 0 => {
                        let name = &cols[sel];
                        if let Some(p) = order.iter().position(|x| x == name) {
                            order.remove(p);
                        } else {
                            order.push(name.clone());
                        }
                        commit = true;
                    }
                    // Reorder the selected column within the label order.
                    Key::Char(',') if n > 0 => {
                        if let Some(p) = order.iter().position(|x| *x == cols[sel]) {
                            if p > 0 {
                                order.swap(p, p - 1);
                                commit = true;
                            }
                        }
                    }
                    Key::Char('.') if n > 0 => {
                        if let Some(p) = order.iter().position(|x| *x == cols[sel]) {
                            if p + 1 < order.len() {
                                order.swap(p, p + 1);
                                commit = true;
                            }
                        }
                    }
                    Key::Char('c') if !order.is_empty() => {
                        order.clear();
                        commit = true;
                    }
                    _ => {}
                }
                if commit {
                    match self.store.set_reference_columns(&table, order.clone()) {
                        Ok(_) if order.is_empty() => self.status = "reference columns cleared (auto)".into(),
                        Ok(_) => self.status = format!("reference label: {}", order.join(" ")),
                        Err(e) => self.err(e),
                    }
                }
                Screen::RefCols { table, order, sel }
            }
            Screen::Drives {
                mut drives,
                mut sel,
                mut top,
            } => {
                let present_count = drives.iter().filter(|d| d.present).count();
                let n = drives.len();
                if sel >= n {
                    sel = n.saturating_sub(1);
                }
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up if sel > 0 => sel -= 1,
                    Key::Down if sel + 1 < n => sel += 1,
                    Key::Char('r') | Key::Char('R') => {
                        let ide = ata::enumerate_drives(&self.booted_sys_guid);
                        let usb = xhci::autopilot_usb_drives(&self.booted_sys_guid);
                        let new_total = ide.len() + usb.len();
                        drives = ide;
                        drives.extend(usb);
                        let new_present = drives.iter().filter(|d| d.present).count();
                        self.status = format!(
                            "re-enumerated drives ({} present of {}; was {} of {})",
                            new_present, new_total, present_count, n
                        );
                    }
                    Key::Char('p') | Key::Char('P') => {
                        let devices = pci::enumerate();
                        self.status = format!("found {} PCI device(s)", devices.len());
                        nav = Nav::Push(Screen::Pci {
                            devices,
                            sel: 0,
                            top: 0,
                        });
                    }
                    _ => {}
                }
                if sel < top {
                    top = sel;
                }
                Screen::Drives { drives, sel, top }
            }
            Screen::Pci {
                mut devices,
                mut sel,
                mut top,
            } => {
                let n = devices.len();
                if sel >= n {
                    sel = n.saturating_sub(1);
                }
                let vis = pci_visible_rows();
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up if sel > 0 => sel -= 1,
                    Key::Down if sel + 1 < n => sel += 1,
                    Key::PageUp => sel = sel.saturating_sub(10),
                    Key::PageDown => sel = (sel + 10).min(n.saturating_sub(1)),
                    Key::Enter if !devices.is_empty() => {
                        let d = devices[sel];
                        if d.class == 0x0C && d.subclass == 0x03 && d.prog_if == 0x30 {
                            match xhci::inspect(&d) {
                                Some(info) => {
                                    self.status = format!(
                                        "xHCI {} — {} slots, {} ports",
                                        xhci::version_string(info.hci_version),
                                        info.max_slots,
                                        info.max_ports
                                    );
                                    nav = Nav::Push(Screen::Xhci { info, dev: d, top: 0 });
                                }
                                None => {
                                    self.status =
                                        "xHCI inspection failed (BAR unassigned or > 4 GiB)"
                                            .into();
                                }
                            }
                        } else {
                            self.status =
                                "Enter only inspects xHCI controllers for now".into();
                        }
                    }
                    Key::Char('r') | Key::Char('R') => {
                        devices = pci::enumerate();
                        self.status = format!("re-enumerated PCI ({} devices)", devices.len());
                    }
                    // Empirical calibration check: TSC delays should match
                    // wall-clock time. Press 't' for a 1-second sleep that
                    // the user can time against a stopwatch / status update.
                    Key::Char('t') | Key::Char('T') => {
                        let t0 = unsafe { core::arch::x86_64::_rdtsc() };
                        time::delay_ms(1000);
                        let t1 = unsafe { core::arch::x86_64::_rdtsc() };
                        let elapsed_us =
                            (t1 - t0) / time::tsc_per_us().max(1);
                        self.status = format!(
                            "delay_ms(1000) ran for {} µs (TSC: {} ticks/µs)",
                            elapsed_us,
                            time::tsc_per_us()
                        );
                    }
                    _ => {}
                }
                if sel < top {
                    top = sel;
                }
                if sel >= top + vis {
                    top = sel + 1 - vis;
                }
                Screen::Pci { devices, sel, top }
            }
            Screen::CreateOsPick { candidates, mut sel } => {
                let n = candidates.len();
                if sel >= n {
                    sel = n.saturating_sub(1);
                }
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up if sel > 0 => sel -= 1,
                    Key::Down if sel + 1 < n => sel += 1,
                    Key::Enter if !candidates.is_empty() => {
                        let (slot_id, drive) = candidates[sel].clone();
                        let label = drive.slot.clone();
                        let mbr_state = match &drive.mbr {
                            crate::ata::MbrInfo::TablesOs { .. } => "an existing TablesOS volume",
                            crate::ata::MbrInfo::Partitioned { .. } => "an existing partition table",
                            crate::ata::MbrInfo::Blank => "(empty disk)",
                            _ => "an unrecognised layout",
                        };
                        nav = Nav::Push(Screen::Confirm {
                            msg: format!(
                                "Overwrite {} ({} MiB) — currently {} — with a fresh TablesOS image?",
                                label,
                                drive.lba28_sectors / 2048,
                                mbr_state,
                            ),
                            action: Action::CreateOs(slot_id, label),
                        });
                    }
                    _ => {}
                }
                Screen::CreateOsPick { candidates, sel }
            }
            Screen::UpgradePick { candidates, mut sel } => {
                let n = candidates.len();
                if sel >= n {
                    sel = n.saturating_sub(1);
                }
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up if sel > 0 => sel -= 1,
                    Key::Down if sel + 1 < n => sel += 1,
                    Key::Enter if !candidates.is_empty() => {
                        let (slot_id, drive) = candidates[sel].clone();
                        let label = drive.slot.clone();
                        let from = match &drive.mbr {
                            crate::ata::MbrInfo::TablesOs { version, .. } => {
                                tablestore::version_string(*version)
                            }
                            _ => "?".into(),
                        };
                        nav = Nav::Push(Screen::Confirm {
                            msg: format!(
                                "Top up {} from {} to {}? Existing data is kept and migrated.",
                                label,
                                from,
                                tablestore::VERSION_STR,
                            ),
                            action: Action::UpgradeOs(slot_id, label),
                        });
                    }
                    _ => {}
                }
                Screen::UpgradePick { candidates, sel }
            }
            Screen::FkPick { field_idx, from_col, to_table, to_col, rows, mut sel, mut top } => {
                let n = rows.len();
                if sel >= n {
                    sel = n.saturating_sub(1);
                }
                let visible = visible_rows().max(1);
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up if sel > 0 => {
                        sel -= 1;
                        if sel < top {
                            top = sel;
                        }
                    }
                    Key::Down if sel + 1 < n => {
                        sel += 1;
                        if sel >= top + visible {
                            top = sel - visible + 1;
                        }
                    }
                    Key::PageUp => {
                        sel = sel.saturating_sub(visible);
                        top = top.saturating_sub(visible);
                    }
                    Key::PageDown => {
                        sel = (sel + visible).min(n.saturating_sub(1));
                        top = (top + visible).min(n.saturating_sub(visible));
                    }
                    Key::Enter if !rows.is_empty() => {
                        let (selected_id, _label) = rows[sel].clone();
                        // Extract the to_col value from the selected row and fill
                        // it back. Only close the picker if it succeeded; on
                        // failure stay open so the error status survives (a
                        // `Nav::Pop` clears the status).
                        if self.fk_select(field_idx, selected_id, to_table.clone(), to_col.clone()) {
                            nav = Nav::Pop;
                        }
                    }
                    _ => {}
                }
                Screen::FkPick {
                    field_idx,
                    from_col,
                    to_table,
                    to_col,
                    rows,
                    sel,
                    top,
                }
            }
            Screen::InstallResult { report } => {
                if matches!(k, Key::Esc | Key::Enter) {
                    nav = Nav::PopToList;
                }
                Screen::InstallResult { report }
            }
            Screen::UpgradeResult { report } => {
                if matches!(k, Key::Esc | Key::Enter) {
                    nav = Nav::PopToList;
                }
                Screen::UpgradeResult { report }
            }
            Screen::About { mut top } => {
                let max_top = about_lines().len().saturating_sub(visible_rows().max(1));
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up => top = top.saturating_sub(1),
                    Key::Down if top < max_top => top += 1,
                    Key::PageUp => top = top.saturating_sub(10),
                    Key::PageDown => top = (top + 10).min(max_top),
                    _ => {}
                }
                Screen::About { top }
            }
            Screen::Xhci { mut info, dev, mut top } => {
                // Window size for the scrollable content (one fewer than the
                // generic body region: this screen carries an extra header
                // line — the pipeline hint — above the content).
                let vis = visible_rows().saturating_sub(1).max(1);
                match k {
                    Key::Esc => nav = Nav::Pop,
                    Key::Up => top = top.saturating_sub(1),
                    Key::Down => top += 1,
                    Key::PageUp => top = top.saturating_sub(vis),
                    Key::PageDown => top += vis,
                    Key::Char('r') | Key::Char('R') => {
                        if let Some(fresh) = xhci::inspect(&dev) {
                            info = fresh;
                            self.status =
                                format!("xHCI registers re-read; MFINDEX={}", info.mfindex);
                        } else {
                            self.status = "xHCI re-read failed".into();
                        }
                    }
                    Key::Char('b') | Key::Char('B') => {
                        match xhci::bring_up(&dev, &info) {
                            Ok(_) => {
                                if let Some(fresh) = xhci::inspect(&dev) {
                                    info = fresh;
                                }
                                self.status = format!(
                                    "xHCI brought up — controller running, MFINDEX={}",
                                    info.mfindex
                                );
                            }
                            Err(e) => {
                                if let Some(fresh) = xhci::inspect(&dev) {
                                    info = fresh;
                                }
                                self.status = format!("xHCI bring-up failed: {}", e);
                            }
                        }
                    }
                    Key::Char('e') | Key::Char('E') => {
                        match xhci::reset_and_enable_slots(&dev, &info) {
                            Ok(result) => {
                                let assigned =
                                    result.slots.iter().filter(|s| s.slot_id != 0).count();
                                if let Some(fresh) = xhci::inspect(&dev) {
                                    info = fresh;
                                }
                                self.status = format!(
                                    "port scan complete — {} connected, {} slot(s) assigned",
                                    result.slots.len(),
                                    assigned
                                );
                            }
                            Err(e) => {
                                self.status = format!("port enumeration failed: {}", e);
                            }
                        }
                    }
                    Key::Char('a') | Key::Char('A') => {
                        match xhci::address_enabled_slots(&dev, &info) {
                            Ok(addr) => {
                                let described = addr
                                    .iter()
                                    .filter(|d| d.descriptor.is_some())
                                    .count();
                                self.status = format!(
                                    "addressed {} slot(s); fetched {} descriptor(s)",
                                    addr.len(),
                                    described,
                                );
                            }
                            Err(e) => {
                                self.status = format!("Address Device failed: {}", e);
                            }
                        }
                    }
                    Key::Char('c') | Key::Char('C') => {
                        match xhci::fetch_configurations(&dev, &info) {
                            Ok(addr) => {
                                let configured =
                                    addr.iter().filter(|d| d.config.is_some()).count();
                                self.status = format!(
                                    "fetched {} configuration descriptor(s)",
                                    configured
                                );
                            }
                            Err(e) => {
                                self.status = format!("Get Configuration failed: {}", e);
                            }
                        }
                    }
                    Key::Char('g') | Key::Char('G') => {
                        match xhci::configure_endpoints(&dev, &info) {
                            Ok(addr) => {
                                let ok = addr
                                    .iter()
                                    .filter(|d| d.set_config_cc == 1)
                                    .count();
                                let eps: usize = addr
                                    .iter()
                                    .map(|d| d.configured_endpoints.len())
                                    .sum();
                                self.status = format!(
                                    "Configure Endpoint + SET_CONFIGURATION on {} device(s); {} endpoint(s) wired",
                                    ok, eps,
                                );
                            }
                            Err(e) => {
                                self.status = format!("Configure Endpoint failed: {}", e);
                            }
                        }
                    }
                    Key::Char('m') | Key::Char('M') => {
                        match xhci::probe_mass_storage(&dev, &info) {
                            Ok(probes) => {
                                let ok = probes
                                    .iter()
                                    .filter(|p| p.capacity.is_some())
                                    .count();
                                self.status = format!(
                                    "MSC probe: {} device(s); {} with capacity",
                                    probes.len(),
                                    ok
                                );
                            }
                            Err(e) => {
                                self.status = format!("MSC probe failed: {}", e);
                            }
                        }
                    }
                    Key::Char('w') | Key::Char('W') => {
                        // Pick the first MSC probe that has a usable
                        // capacity. Writes to LBA 1 (the second sector
                        // — LBA 0 is reserved for the MBR), then
                        // restores the original contents.
                        let probes = xhci::current_msc();
                        let target = probes.iter().find(|p| {
                            p.capacity
                                .map(|c| c.block_size == 512 && c.total_blocks() > 1)
                                .unwrap_or(false)
                        });
                        match target {
                            Some(p) => {
                                let result = xhci::run_write_test(p.slot_id, 1);
                                self.status = if result.verify_ok {
                                    format!("USB BlockDevice write/read OK on slot {}", result.slot_id)
                                } else {
                                    format!("USB BlockDevice write test failed: {}", result.message)
                                };
                                self.last_usb_test = Some(result);
                            }
                            None => {
                                self.status =
                                    "no MSC device with 512-byte sectors available — run [m] first".into();
                            }
                        }
                    }
                    _ => {}
                }
                // Clamp against the (possibly just-grown) content: a pipeline
                // step can add many lines, so re-derive the max each time.
                let max_top = self.xhci_content(&info, &dev).len().saturating_sub(vis);
                top = top.min(max_top);
                Screen::Xhci { info, dev, top }
            }
            // Handled before `dispatch` is reached, by the variant match in
            // `on_key` that routes to their dedicated `key_*` handlers. Listed
            // explicitly (returning the screen unchanged) so this match stays
            // exhaustive: a new `Screen` must be wired into input handling here
            // or in `on_key`, the same way `build_frame` forces a render arm.
            scr @ (Screen::Prompt { .. }
            | Screen::Pick { .. }
            | Screen::Confirm { .. }
            | Screen::Editor(_)
            | Screen::Builder(_)) => scr,
        };
        (scr, nav)
    }

    fn key_prompt(&mut self, k: Key) {
        let mut done: Option<(Action, String)> = None;
        {
            let Screen::Prompt { buf, caret, action, .. } = self.top() else {
                return;
            };
            match k {
                Key::Esc => {
                    self.pop();
                    return;
                }
                Key::Left => ce_left(buf, caret),
                Key::Right => ce_right(buf, caret),
                Key::Home => *caret = 0,
                Key::End => *caret = buf.chars().count(),
                Key::Backspace => {
                    ce_backspace(buf, caret);
                }
                Key::Delete => {
                    ce_delete(buf, caret);
                }
                Key::Char(c) => ce_insert(buf, caret, c),
                Key::Enter => done = Some((action.clone(), buf.clone())),
                _ => {}
            }
        }
        if let Some((act, text)) = done {
            self.pop();
            self.resolve_prompt(act, text);
        }
    }

    fn key_pick(&mut self, k: Key) {
        let mut done: Option<(Action, String)> = None;
        {
            let Screen::Pick {
                options,
                sel,
                action,
                ..
            } = self.top()
            else {
                return;
            };
            match k {
                Key::Esc => {
                    self.pop();
                    return;
                }
                Key::Up if *sel > 0 => *sel -= 1,
                Key::Down if *sel + 1 < options.len() => *sel += 1,
                Key::Enter if !options.is_empty() => {
                    done = Some((action.clone(), options[*sel].clone()))
                }
                _ => {}
            }
        }
        if let Some((act, choice)) = done {
            self.pop();
            self.resolve_pick(act, choice);
        }
    }

    fn key_confirm(&mut self, k: Key) {
        let mut act = None;
        {
            let Screen::Confirm { action, .. } = self.top() else {
                return;
            };
            match k {
                Key::Enter => act = Some(action.clone()),
                Key::Esc => {
                    self.pop();
                    return;
                }
                _ => {}
            }
        }
        if let Some(act) = act {
            self.pop();
            self.resolve_confirm(act);
        }
    }

    // ---- modal resolution -------------------------------------------------

    fn resolve_prompt(&mut self, action: Action, text: String) {
        match action {
            Action::CreateTable => match self.store.create_table(&text) {
                Ok(_) => {
                    self.status = format!("created table '{text}'");
                    // Stack a Browser under the Schema Editor so that leaving
                    // the column-definition step lands in the new table, not
                    // back on the table list.
                    self.push(Screen::Browser {
                        table: text.clone(),
                        row: 0,
                        col: 0,
                        top: 0,
                        col_off: 0,
                        filter: None,
                        sort: None,
                    });
                    self.push(Screen::Schema {
                        table: text,
                        sel: 0,
                    });
                }
                Err(e) => self.err(e),
            },
            Action::AddColPickType(table, _) => {
                if text.is_empty() {
                    self.status = "column name required".into();
                    return;
                }
                let opts = Type::ALL.iter().map(|t| t.name().to_string()).collect();
                self.push(Screen::Pick {
                    title: format!("Type for column '{text}'"),
                    options: opts,
                    sel: 0,
                    action: Action::AddColPickType(table, text),
                });
            }
            Action::RenameColumn(table, old) => {
                if text.is_empty() {
                    self.status = "column name required".into();
                    return;
                }
                match self.store.rename_column(&table, &old, &text) {
                    Ok(_) => self.status = format!("renamed '{old}' to '{text}'"),
                    Err(e) => self.err(e),
                }
            }
            Action::SetDisplayWidth(table, col) => {
                let trimmed = text.trim();
                let width = if trimmed.is_empty() {
                    None
                } else {
                    match trimmed.parse::<u32>() {
                        Ok(w) => Some(
                            w.clamp(MIN_COL_W as u32, MAX_COL_W as u32) as u16,
                        ),
                        Err(_) => {
                            self.status =
                                "width must be a whole number (blank to clear)".into();
                            return;
                        }
                    }
                };
                match self.store.set_display_width(&table, &col, width) {
                    Ok(_) => {
                        self.status = match width {
                            Some(w) => format!("'{col}' browser width set to {w}"),
                            None => format!("'{col}' browser width reset to default"),
                        }
                    }
                    Err(e) => self.err(e),
                }
            }
            _ => {}
        }
    }

    fn resolve_pick(&mut self, action: Action, choice: String) {
        match action {
            Action::PowerMenu => {
                if choice == "Reboot" {
                    reboot();
                } else {
                    shutdown();
                }
            }
            Action::KbdLayout => {
                if crate::keymap::set_layout(&choice) {
                    self.status = format!("keyboard layout: {choice}");
                } else {
                    self.status = "unknown keyboard layout".into();
                }
            }
            Action::AddColPickType(table, name) => {
                let ty = Type::from_name(&choice).unwrap_or(Type::String);
                // Spec: a freshly added column must be nullable; existing rows
                // get NULL. UNIQUE is then toggled from the Schema Editor.
                let col = Column {
                    name,
                    ty,
                    nullable: true,
                    unique: false,
                    display_width: None,
                };
                match self.store.add_column(&table, col) {
                    Ok(_) => self.status = "column added (nullable)".into(),
                    Err(e) => self.err(e),
                }
            }
            Action::AddFkPickFrom(table) => {
                let tables = self
                    .store
                    .list_tables()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.name)
                    .collect();
                self.push(Screen::Pick {
                    title: "Foreign key: target table".into(),
                    options: tables,
                    sel: 0,
                    action: Action::AddFkPickToTable(table, choice),
                });
            }
            Action::AddFkPickToTable(table, from_col) => {
                let cols = self
                    .store
                    .get_table(&choice)
                    .map(|t| {
                        t.columns
                            .iter()
                            .filter(|c| c.unique)
                            .map(|c| c.name.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if cols.is_empty() {
                    self.status = format!("'{choice}' has no UNIQUE column to target");
                    return;
                }
                self.push(Screen::Pick {
                    title: format!("UNIQUE column in '{choice}'"),
                    options: cols,
                    sel: 0,
                    action: Action::AddFkPickToCol(table, from_col, choice),
                });
            }
            Action::AddFkPickToCol(table, from_col, to_table) => {
                match self.store.add_fk(&table, &from_col, &to_table, &choice) {
                    Ok(_) => self.status = "foreign key added".into(),
                    Err(e) => self.err(e),
                }
            }
            Action::DropFk(table) => match self.store.drop_fk(&table, &choice) {
                Ok(_) => self.status = "foreign key dropped".into(),
                Err(e) => self.err(e),
            },
            _ => {}
        }
    }

    fn resolve_confirm(&mut self, action: Action) {
        match action {
            Action::DeleteRow(t, id) => match self.store.delete(&t, id) {
                Ok(_) => self.status = "row deleted".into(),
                Err(e) => self.err(e),
            },
            Action::DropTable(t) => match self.store.drop_table(&t) {
                Ok(_) => {
                    self.status = format!("dropped table '{t}'");
                    // Leave the schema/browser screens that referenced it.
                    while !matches!(self.top(), Screen::List { .. }) {
                        self.pop();
                    }
                }
                Err(e) => self.err(e),
            },
            Action::DropColumn(t, c) => match self.store.drop_column(&t, &c) {
                Ok(_) => self.status = format!("dropped column '{c}'"),
                Err(e) => self.err(e),
            },
            Action::CreateOs(slot_id, label) => {
                self.status = format!("installing fresh TablesOS onto {} …", label);
                let report =
                    install::install_to_usb(slot_id, self.data_lba, &self.booted_sys_guid);
                self.status = if report.verify_mount_ok {
                    format!("install OK — slot {} ready to boot", slot_id)
                } else {
                    format!("install failed: {}", report.message)
                };
                // Replace the picker screen with the result panel.
                while !matches!(self.top(), Screen::List { .. }) {
                    self.pop();
                }
                self.push(Screen::InstallResult { report });
            }
            Action::UpgradeOs(slot_id, label) => {
                self.status = format!("topping up {} to {} …", label, tablestore::VERSION_STR);
                let report =
                    upgrade::upgrade_usb(slot_id, self.data_lba, &self.booted_sys_guid);
                self.status = if report.verify_mount_ok {
                    format!("upgrade OK — slot {} now {}", slot_id, tablestore::VERSION_STR)
                } else {
                    format!("upgrade failed: {}", report.message)
                };
                // Replace the picker screen with the result panel.
                while !matches!(self.top(), Screen::List { .. }) {
                    self.pop();
                }
                self.push(Screen::UpgradeResult { report });
            }
            _ => {}
        }
    }

    // ---- editor -----------------------------------------------------------

    fn open_editor(&mut self, table: &str, id: Option<RowId>) -> Option<Screen> {
        let t = match self.store.get_table(table) {
            Ok(t) => t,
            Err(e) => {
                self.err(e);
                return None;
            }
        };
        let existing = id.and_then(|i| self.store.get_row(table, i).ok());
        let mut fields = Vec::new();
        for (ci, c) in t.columns.iter().enumerate() {
            let (value, is_null) = match existing.as_ref().and_then(|r| r.get(ci)) {
                Some(Some(v)) => (v.display(), false),
                Some(None) => (String::new(), true),
                None => (String::new(), c.nullable),
            };
            fields.push(Field { caret: value.chars().count(), value, is_null });
        }
        Some(Screen::Editor(Editor {
            table: table.to_string(),
            id,
            cols: t.columns.clone(),
            fields,
            focus: 0,
            error: None,
        }))
    }

    fn key_editor(&mut self, k: Key) {
        let mut want_cancel = false;
        let mut want_commit = false;
        let mut want_parts: Option<usize> = None;
        {
            let Screen::Editor(ed) = self.top() else {
                return;
            };
            let nf = ed.cols.len();
            let on_field = ed.focus < nf;
            match k {
                Key::Esc => want_cancel = true,
                // Tab and ↑/↓ move between fields (and the OK/Cancel buttons);
                // ←/→/Home/End now navigate *within* the focused field's text.
                Key::Tab | Key::Down => ed.focus = (ed.focus + 1) % (nf + 2),
                Key::Up => ed.focus = (ed.focus + nf + 1) % (nf + 2),
                Key::Left if on_field => {
                    let f = &mut ed.fields[ed.focus];
                    ce_left(&f.value, &mut f.caret);
                }
                Key::Right if on_field => {
                    let f = &mut ed.fields[ed.focus];
                    ce_right(&f.value, &mut f.caret);
                }
                Key::Home if on_field => ed.fields[ed.focus].caret = 0,
                Key::End if on_field => {
                    let f = &mut ed.fields[ed.focus];
                    f.caret = f.value.chars().count();
                }
                Key::Delete if on_field && ed.cols[ed.focus].nullable => {
                    let f = &mut ed.fields[ed.focus];
                    f.is_null = !f.is_null;
                }
                // `→` is now caret movement, so the structured value builder
                // (part fields, plus "fill current value" for date/time types)
                // opens with PgDn instead.
                Key::PageDown if on_field => {
                    want_parts = Some(ed.focus);
                }
                Key::Backspace if on_field => {
                    let f = &mut ed.fields[ed.focus];
                    f.is_null = false;
                    ce_backspace(&mut f.value, &mut f.caret);
                }
                Key::Char(c) if on_field => {
                    let f = &mut ed.fields[ed.focus];
                    f.is_null = false;
                    ce_insert(&mut f.value, &mut f.caret, c);
                }
                Key::Enter => {
                    if ed.focus == nf + 1 {
                        want_cancel = true;
                    } else {
                        // Either the OK button or a field: try to commit.
                        want_commit = true;
                    }
                }
                _ => {}
            }
        }
        if want_cancel {
            self.pop();
        } else if want_commit {
            self.commit_editor();
        } else if let Some(fi) = want_parts {
            self.open_builder(fi);
        }
    }

    /// Open the structured value builder for editor field `fi`, seeded from its
    /// current text when that parses as the column's type. For FK fields,
    /// opens a picker instead of the part-based builder.
    fn open_builder(&mut self, fi: usize) {
        let (ty, col_name, table, unique, is_insert) = {
            let Screen::Editor(ed) = self.top() else {
                return;
            };
            let col = &ed.cols[fi];
            (col.ty, col.name.clone(), ed.table.clone(), col.unique, ed.id.is_none())
        };
        // After releasing the borrow, check if this column is a FK source.
        let fk_target = self.store.get_table(&table)
            .ok()
            .and_then(|t| {
                t.fks
                    .iter()
                    .find(|fk| fk.from_col == col_name)
                    .map(|fk| (fk.to_table.clone(), fk.to_col.clone()))
            });
        // FK fields: open a picker instead of the part-based builder.
        if let Some((to_table, to_col)) = fk_target {
            self.open_fk_picker(fi, &col_name, &to_table, &to_col);
            return;
        }
        let mut b = FieldBuilder::new(fi, col_name, ty);
        // Seed from existing value if present and parseable.
        {
            let Screen::Editor(ed) = self.top() else {
                return;
            };
            let f = &ed.fields[fi];
            if !f.is_null && !f.value.is_empty() {
                if let Ok(v) = Value::parse(ty, &f.value) {
                    b.load_from(&v);
                }
            }
        }
        // On insert into a UNIQUE integer/unsigned column, offer the "[ Next ]"
        // shortcut that fills max+1 — a fresh, non-colliding key.
        if is_insert && unique {
            b.next_value = self.next_unique_value(&table, fi, ty);
        }
        self.push(Screen::Builder(b));
    }

    /// Open an FK value picker for the source column `from_col`, which targets
    /// `to_table.to_col`. Loads all rows from the target table and shows their
    /// reference labels.
    fn open_fk_picker(&mut self, field_idx: usize, from_col: &str, to_table: &str, to_col: &str) {
        let rows = match self.store.scan(to_table) {
            Ok(r) => r,
            Err(e) => {
                self.err(e);
                return;
            }
        };
        let schema = match self.store.get_table(to_table) {
            Ok(s) => s,
            Err(e) => {
                self.err(e);
                return;
            }
        };
        // The field's current text is the display form of the target column
        // value (that's what fk_select writes back), so the picker can open on
        // the row that's already selected rather than always the first one.
        let current: Option<String> = match self.top() {
            Screen::Editor(ed) => ed
                .fields
                .get(field_idx)
                .filter(|f| !f.is_null && !f.value.is_empty())
                .map(|f| f.value.clone()),
            _ => None,
        };
        let to_col_idx = schema.column_index(to_col);
        // Extract rows as (id, reference_label) pairs. Skip rows where to_col is NULL.
        let mut fk_rows: Vec<(RowId, String)> = Vec::new();
        let mut sel: Option<usize> = None;
        for (id, row) in rows {
            if sel.is_none() {
                if let (Some(cur), Some(ci)) = (current.as_deref(), to_col_idx) {
                    if let Some(Some(v)) = row.get(ci) {
                        if v.display() == cur {
                            sel = Some(fk_rows.len());
                        }
                    }
                }
            }
            let label = schema.reference_label(&row);
            fk_rows.push((id, label));
        }
        // If empty, warn but don't crash — let the user cancel and retry.
        if fk_rows.is_empty() {
            self.status = format!("no rows in '{to_table}' to pick from");
            return;
        }
        let sel = sel.unwrap_or(0);
        // Scroll so the pre-selected row is visible (at the bottom of the
        // window when it's beyond the first page, matching ↓'s behaviour).
        let visible = visible_rows().max(1);
        let top = sel.saturating_sub(visible - 1);
        self.push(Screen::FkPick {
            field_idx,
            from_col: from_col.to_string(),
            to_table: to_table.to_string(),
            to_col: to_col.to_string(),
            rows: fk_rows,
            sel,
            top,
        });
    }

    /// Canonical text of `max(col) + 1` for a UNIQUE integer/unsigned column,
    /// used to seed the Row Editor's "[ Next ]" shortcut. Returns `None` for
    /// other types or when the table can't be scanned. An empty (or all-NULL)
    /// column seeds at `"1"`, so the first generated key is 1.
    fn next_unique_value(&mut self, table: &str, col: usize, ty: Type) -> Option<String> {
        if ty != Type::Integer && ty != Type::UnsignedInteger {
            return None;
        }
        let rows = self.store.scan(table).ok()?;
        let max = rows
            .iter()
            .filter_map(|(_, r)| r.get(col).and_then(|c| c.as_ref()))
            .max();
        Some(match max {
            Some(Value::Unsigned(u)) => Value::Unsigned(u.succ()).display(),
            Some(Value::Integer(i)) => Value::Integer(i.succ()).display(),
            _ => String::from("1"),
        })
    }

    /// Extract the selected FK value from the target row and fill it back into
    /// the Editor field. Returns `true` on success (so the caller pops the
    /// picker); on failure it sets `self.status` and returns `false` so the
    /// picker stays open with the message visible (a `Nav::Pop` would clear it).
    fn fk_select(&mut self, field_idx: usize, selected_id: RowId, to_table: String, to_col: String) -> bool {
        // Get the column index in the target table.
        let Some(target_schema) = self.store.get_table(&to_table).ok() else {
            self.status = format!("could not load '{to_table}'");
            return false;
        };
        let Some(col_idx) = target_schema.column_index(&to_col) else {
            self.status = format!("column '{to_col}' not found in '{to_table}'");
            return false;
        };
        // Get the selected row and extract the value.
        let Ok(row) = self.store.get_row(&to_table, selected_id) else {
            self.status = "could not load selected row".into();
            return false;
        };
        let value_text = match row.get(col_idx) {
            Some(Some(v)) => v.display(),
            Some(None) => {
                // Target column is NULL — not valid for FK assignment.
                self.status = "selected row has NULL in the target column".into();
                return false;
            }
            None => {
                self.status = "column index out of range".into();
                return false;
            }
        };
        // Write the value into the Row Editor. We're called from inside
        // `dispatch`, where the FkPick screen has been temporarily moved out and
        // a placeholder put on `top()`; the Editor is the screen *beneath* it.
        // Reach down to the nearest Editor on the stack rather than `top()`
        // (which is the placeholder, not the Editor).
        let editor = self.stack.iter_mut().rev().find_map(|s| match s {
            Screen::Editor(ed) => Some(ed),
            _ => None,
        });
        let Some(ed) = editor else {
            return false;
        };
        let Some(f) = ed.fields.get_mut(field_idx) else {
            return false;
        };
        f.caret = value_text.chars().count();
        f.value = value_text;
        f.is_null = false;
        ed.error = None;
        ed.focus = field_idx;
        true
    }

    fn key_builder(&mut self, k: Key) {
        let mut want_cancel = false;
        let mut want_commit = false;
        let mut want_now = false;
        let mut want_next = false;
        {
            let Screen::Builder(b) = self.top() else {
                return;
            };
            let nf = b.fields.len();
            let total = b.total();
            let now_i = b.now_index();
            let next_i = b.next_index();
            let cancel_i = b.cancel_index();
            match k {
                Key::Esc => want_cancel = true,
                Key::Tab | Key::Down => b.focus = (b.focus + 1) % total,
                Key::Up => b.focus = (b.focus + total - 1) % total,
                // `n` fills the current value — but only for the date/time
                // types; for a string field `n` is a literal to be typed.
                Key::Char('n') | Key::Char('N') if b.has_now() => want_now = true,
                // For a UNIQUE integer/unsigned column `n` instead fills max+1
                // (the two never coexist — a column isn't both a date and an
                // integer). Digits don't reach this arm, so it can't shadow them.
                Key::Char('n') | Key::Char('N') if b.has_next() => want_next = true,
                // On a sign part ←/→ flip the sign; on a typed part they move
                // the caret. (The sign arm is listed first so it wins.)
                Key::Left | Key::Right if b.focus < nf && b.fields[b.focus].id.is_sign() => {
                    b.fields[b.focus].neg = !b.fields[b.focus].neg;
                }
                Key::Left if b.focus < nf => {
                    let f = &mut b.fields[b.focus];
                    ce_left(&f.buf, &mut f.caret);
                }
                Key::Right if b.focus < nf => {
                    let f = &mut b.fields[b.focus];
                    ce_right(&f.buf, &mut f.caret);
                }
                Key::Home if b.focus < nf => b.fields[b.focus].caret = 0,
                Key::End if b.focus < nf => {
                    let f = &mut b.fields[b.focus];
                    f.caret = f.buf.chars().count();
                }
                Key::Backspace if b.focus < nf => {
                    let f = &mut b.fields[b.focus];
                    ce_backspace(&mut f.buf, &mut f.caret);
                }
                Key::Delete if b.focus < nf => {
                    let f = &mut b.fields[b.focus];
                    ce_delete(&mut f.buf, &mut f.caret);
                }
                Key::Char(c) if b.focus < nf => {
                    let f = &mut b.fields[b.focus];
                    if f.id.is_sign() {
                        if c == '+' {
                            f.neg = false;
                        } else if c == '-' {
                            f.neg = true;
                        }
                    } else if f.id == PartId::Year && (c == '-' || c == '+') {
                        f.neg = c == '-';
                    } else if f.id.is_text() {
                        ce_insert(&mut f.buf, &mut f.caret, c);
                    } else if c.is_ascii_digit() && f.buf.chars().count() < f.id.max_digits() {
                        ce_insert(&mut f.buf, &mut f.caret, c);
                    }
                }
                Key::Enter => {
                    if Some(b.focus) == now_i {
                        want_now = true;
                    } else if Some(b.focus) == next_i {
                        want_next = true;
                    } else if b.focus == cancel_i {
                        want_cancel = true;
                    } else {
                        // A part field or the OK button: try to compose.
                        want_commit = true;
                    }
                }
                _ => {}
            }
        }
        if want_cancel {
            self.pop();
        } else if want_now {
            if let Screen::Builder(b) = self.top() {
                if b.fill_now() {
                    b.error = None;
                    self.status = "filled current date/time".into();
                } else {
                    self.status = "RTC unavailable — enter the value manually".into();
                }
            }
        } else if want_next {
            if let Screen::Builder(b) = self.top() {
                if b.fill_next() {
                    b.error = None;
                    self.status = "filled next unique value (max+1)".into();
                }
            }
        } else if want_commit {
            self.commit_builder();
        }
    }

    /// Compose the builder's parts and write the canonical value back to the
    /// Row Editor field. Stays open with an inline error if invalid.
    fn commit_builder(&mut self) {
        let Screen::Builder(mut b) = core::mem::replace(self.top(), Screen::List { sel: 0 })
        else {
            return;
        };
        match Value::parse(b.ty, &b.compose()) {
            Ok(v) => {
                let disp = v.display();
                let fi = b.field;
                self.pop(); // remove the placeholder, back to the editor
                if let Screen::Editor(ed) = self.top() {
                    if let Some(f) = ed.fields.get_mut(fi) {
                        f.caret = disp.chars().count();
                        f.value = disp;
                        f.is_null = false;
                    }
                    ed.error = None;
                    ed.focus = fi;
                }
                self.status = "value set from fields".into();
            }
            Err(e) => {
                b.error = Some(describe(&e));
                *self.top() = Screen::Builder(b);
            }
        }
    }

    fn commit_editor(&mut self) {
        // Pull the editor out to satisfy the borrow checker, put it back if we
        // stay on the screen.
        let Screen::Editor(mut ed) = core::mem::replace(self.top(), Screen::List { sel: 0 })
        else {
            return;
        };
        let mut cells: Vec<Option<Value>> = Vec::with_capacity(ed.cols.len());
        ed.error = None;
        for (i, c) in ed.cols.iter().enumerate() {
            let f = &ed.fields[i];
            if f.is_null {
                if !c.nullable {
                    ed.error = Some((i, "NOT NULL".into()));
                    break;
                }
                cells.push(None);
            } else {
                match Value::parse(c.ty, &f.value) {
                    Ok(v) => cells.push(Some(v)),
                    Err(StoreError::Parse(m)) => {
                        ed.error = Some((i, m));
                        break;
                    }
                    Err(e) => {
                        ed.error = Some((i, describe(&e)));
                        break;
                    }
                }
            }
        }
        if ed.error.is_none() {
            let res = match ed.id {
                Some(id) => self.store.update(&ed.table, id, cells).map(|_| None),
                None => self.store.insert(&ed.table, cells).map(Some),
            };
            match res {
                Ok(_) => {
                    self.status = "saved".into();
                    *self.top() = Screen::List { sel: 0 }; // placeholder
                    self.pop(); // back to Browser
                    return;
                }
                Err(e) => {
                    // Attach the error to a column when we can identify it. The
                    // FK list is needed to resolve a foreign-key violation (it
                    // names the FK, not the column) back to its source field.
                    let fks = self.store.get_table(&ed.table).map(|t| t.fks).unwrap_or_default();
                    let idx = column_for_error(&ed.cols, &fks, &e);
                    ed.error = Some((idx, describe(&e)));
                }
            }
        }
        *self.top() = Screen::Editor(ed);
    }

    // ---- data helpers -----------------------------------------------------

    /// Borrow the Browser's row list, decoding the table only when the cache
    /// is stale (different table/filter/sort, or a commit has bumped the store
    /// generation). Pure navigation hits the cache, so it never re-decodes.
    fn cached_rows(
        &mut self,
        table: &str,
        filter: &Option<(String, Value)>,
        sort: Option<(usize, bool)>,
    ) -> &[(RowId, Vec<Option<Value>>)] {
        let generation = self.store.generation();
        let fresh = matches!(
            &self.row_cache,
            Some(c) if c.table == table
                && c.filter == *filter
                && c.sort == sort
                && c.generation == generation
        );
        if !fresh {
            let rows = self.rows(table, filter, sort);
            self.row_cache = Some(RowCache {
                table: table.to_string(),
                filter: filter.clone(),
                sort,
                generation,
                rows,
            });
        }
        &self.row_cache.as_ref().unwrap().rows
    }

    fn rows(
        &mut self,
        table: &str,
        filter: &Option<(String, Value)>,
        sort: Option<(usize, bool)>,
    ) -> Vec<(RowId, Vec<Option<Value>>)> {
        let mut rows = self.store.scan(table).unwrap_or_default();
        if let Some((col, val)) = filter {
            if let Ok(t) = self.store.get_table(table) {
                if let Some(ci) = t.column_index(col) {
                    rows.retain(|(_, r)| r.get(ci).cloned().flatten().as_ref() == Some(val));
                }
            }
        }
        if let Some((ci, asc)) = sort {
            // NULLs sort to the bottom in either direction; non-NULL values
            // use the engine's total order on `Value`. `sort_by` is stable so
            // ties keep their retrieval order.
            rows.sort_by(|a, b| {
                let av = a.1.get(ci).and_then(|c| c.as_ref());
                let bv = b.1.get(ci).and_then(|c| c.as_ref());
                match (av, bv) {
                    (Some(x), Some(y)) => if asc { x.cmp(y) } else { y.cmp(x) },
                    (Some(_), None) => core::cmp::Ordering::Less,
                    (None, Some(_)) => core::cmp::Ordering::Greater,
                    (None, None) => core::cmp::Ordering::Equal,
                }
            });
        }
        rows
    }

    /// Navigation targets for Row View: every outgoing FK that has a value,
    /// then every incoming "referenced by" group.
    #[allow(clippy::type_complexity)]
    fn nav_targets(
        &mut self,
        table: &str,
        id: RowId,
    ) -> Vec<(String, (String, String, Value))> {
        let mut out = Vec::new();
        if let Ok(t) = self.store.get_table(table) {
            if let Ok(row) = self.store.get_row(table, id) {
                for fk in &t.fks {
                    if let Some(ci) = t.column_index(&fk.from_col) {
                        if let Some(v) = row.get(ci).cloned().flatten() {
                            // Resolve the referenced row so the target shows as
                            // its reference label, e.g. "→ person: id:5 name:John".
                            let label = match self.store.follow_fk(table, id, &fk.name) {
                                Ok((to_table, matches)) => match matches.first() {
                                    Some((_, trow)) => match self.store.get_table(&to_table) {
                                        Ok(tt) => {
                                            format!("→ {to_table}: {}", tt.reference_label(trow))
                                        }
                                        Err(_) => format!("→ follow {} to {}", fk.name, fk.to_table),
                                    },
                                    None => format!("→ follow {} to {}", fk.name, fk.to_table),
                                },
                                Err(_) => format!("→ follow {} to {}", fk.name, fk.to_table),
                            };
                            out.push((label, (fk.to_table.clone(), fk.to_col.clone(), v)));
                        }
                    }
                }
            }
        }
        if let Ok(refs) = self.store.referencing_rows(table, id) {
            for (tbl, fkname, _rid, r) in refs {
                // Re-derive the key column the FK points at for filtering.
                if let (Ok(ot), Ok(_)) = (self.store.get_table(&tbl), self.store.get_table(table)) {
                    if let Some(fk) = ot.fks.iter().find(|f| f.name == fkname) {
                        if let Ok(srow) = self.store.get_row(table, id) {
                            if let Some(tci) =
                                self.store.get_table(table).ok().and_then(|t| {
                                    t.column_index(&fk.to_col)
                                })
                            {
                                if let Some(v) = srow.get(tci).cloned().flatten() {
                                    // The referencing row, shown as its own label.
                                    out.push((
                                        format!("← {tbl}: {}", ot.reference_label(&r)),
                                        (tbl.clone(), fk.from_col.clone(), v),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// For row `id` of `table`, the source-column index of each outgoing FK
    /// that holds a value, paired with the referenced row's reference label —
    /// used to annotate the FK fields in Row View.
    fn fk_field_labels(&mut self, table: &str, id: RowId) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        let Ok(t) = self.store.get_table(table) else {
            return out;
        };
        for fk in &t.fks {
            let Some(ci) = t.column_index(&fk.from_col) else {
                continue;
            };
            if let Ok((to_table, matches)) = self.store.follow_fk(table, id, &fk.name) {
                if let Some((_, trow)) = matches.first() {
                    if let Ok(tt) = self.store.get_table(&to_table) {
                        out.push((ci, tt.reference_label(trow)));
                    }
                }
            }
        }
        out
    }

    fn pick_columns(&mut self, table: &str, title: &str, action: Action) -> Screen {
        let cols = self
            .store
            .get_table(table)
            .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        Screen::Pick {
            title: title.into(),
            options: cols,
            sel: 0,
            action,
        }
    }

    // ---- mouse ------------------------------------------------------------

    fn on_click(&mut self, x: usize, y: usize) {
        // Map the pointer to the body line it is over, then act on that line's
        // recorded click target. Clicking off any row or shortcut does nothing.
        if y < BODY_TOP {
            return;
        }
        let li = (y - BODY_TOP) / CELL_H;
        let (hit, text) = match self.hitmap.get(li) {
            Some(r) => (r.hit, r.text.clone()),
            None => return,
        };
        match hit {
            Hit::None => {}
            Hit::Focus(i) => self.set_cursor(i),
            Hit::Activate(i) => {
                self.set_cursor(i);
                self.on_key(Key::Enter);
            }
            Hit::Shortcuts => {
                if x >= BODY_X {
                    if let Some(k) = shortcut_at(&text, (x - BODY_X) / CELL_W) {
                        self.on_key(k);
                    }
                }
            }
        }
    }

    /// Move the focused screen's cursor/selection to item `i`. Mirrors the
    /// per-screen selection field so a click can place the cursor before an
    /// activation (Enter) runs against it.
    fn set_cursor(&mut self, i: usize) {
        match self.top() {
            Screen::List { sel } => *sel = i,
            Screen::Browser { row, .. } => *row = i,
            Screen::RowView { sel, .. } => *sel = i,
            Screen::Schema { sel, .. } => *sel = i,
            Screen::RefCols { sel, .. } => *sel = i,
            Screen::Pick { sel, .. } => *sel = i,
            Screen::Drives { sel, .. } => *sel = i,
            Screen::Pci { sel, .. } => *sel = i,
            Screen::CreateOsPick { sel, .. } => *sel = i,
            Screen::UpgradePick { sel, .. } => *sel = i,
            Screen::FkPick { sel, .. } => *sel = i,
            Screen::Editor(ed) => ed.focus = i,
            Screen::Builder(b) => b.focus = i,
            _ => {}
        }
    }

    fn on_right_click(&mut self) {
        // The right button mirrors Esc: back out of / cancel the current
        // screen, so the pointer alone can navigate both directions. Position
        // is irrelevant — like Esc, it always acts on the focused screen.
        self.on_key(Key::Esc);
    }

    fn draw_cursor(&self, x: usize, y: usize) {
        // Pointer-only update: tiny rectangle, no scene repaint, no flicker.
        fbm::with(|d| d.move_cursor(x, y));
    }

    // ---- rendering --------------------------------------------------------

    /// Right-aligned title-bar telemetry: a system tag plus uptime since boot.
    /// Uptime comes from the calibrated TSC; before calibration it's omitted.
    fn hud_readout(&self) -> String {
        let per = time::tsc_per_us();
        let ver = tablestore::VERSION_STR;
        // The layout indicator replaces the old "ONLINE" flair: same width, so
        // the readout still leaves the title its space at 1024×768.
        let kbd = crate::keymap::active_short();
        if per == 0 {
            return format!("TABLESOS {ver} // SYS ONLINE // KBD {kbd}");
        }
        let secs = unsafe { core::arch::x86_64::_rdtsc() } / (per.saturating_mul(1_000_000)).max(1);
        // Last frame's present cost (VRAM copy) — diagnostic for graphics speed
        // on real hardware, where there is no serial console.
        let fbus = fbm::last_present_ticks() / per.max(1);
        format!("TABLESOS {ver} // T+{}s // FB {}us // KBD {kbd}", secs, fbus)
    }

    fn render(&mut self) {
        // Gather everything that needs a &mut self *before* locking the
        // framebuffer (drawing borrows the display, not the store).
        let frame = self.build_frame();
        // Capture this frame's click targets so `on_click` can resolve a
        // pointer position against exactly what is on screen.
        self.hitmap = frame
            .body
            .iter()
            .map(|l| ClickRow {
                hit: l.hit,
                text: if matches!(l.hit, Hit::Shortcuts) {
                    l.text.clone()
                } else {
                    String::new()
                },
            })
            .collect();
        let (mx, my) = ps2::mouse_pos();
        let readout = self.hud_readout();
        // The text caret is a blinking overlay: only painted on the "on" phase.
        let cursor = if self.cursor_on { frame.cursor } else { None };
        fbm::with(|d| {
            let (w, h) = (d.width(), d.height());

            // 1. Deep-space backdrop: an embedded bitmap background for this
            //    view (else the procedural gradient + starfield). Cached per
            //    view, so a repaint is a memcpy rather than a full-screen
            //    bilinear recompute — see `paint_background`.
            d.paint_background(fbm::C_BG_TOP, frame.bg);

            // 2. Title bar — a lit panel carrying a logo tab, the screen title
            //    (gradient-filled, haloed and enlarged when it fits), a
            //    right-aligned gradient readout, and a glowing rule + echo.
            d.fill_grad_rect(0, 0, w, TOPBAR_H, fbm::C_BAR, fbm::C_BAR_LO);
            // Right-aligned telemetry, placed first so the title can size to
            // the gap left in front of it.
            let rw = readout.chars().count() * CELL_W;
            let tele_x = w.saturating_sub(MARGIN + rw);
            let tele_y = (TOPBAR_H - CELL_H) / 2;
            if w > rw + 2 * MARGIN {
                d.draw_text_grad(tele_x, tele_y, &readout, fbm::C_TELE_TOP, fbm::C_TELE_BOT, Font::Display, fbm::SCALE);
            }
            // Bump the title to 3× when it fits in the space before the
            // telemetry, otherwise keep it at the body scale.
            let title_n = frame.title.chars().count();
            let ts = if BODY_X + title_n * font::GLYPH_W * 3 + CELL_W < tele_x {
                3
            } else {
                fbm::SCALE
            };
            let th = font::GLYPH_H * ts;
            let ttl_y = TOPBAR_H.saturating_sub(th) / 2;
            d.fill_rect(PANEL_X, ttl_y.saturating_sub(2), 5, th + 4, fbm::C_ACCENT2);
            d.draw_text_glow_grad(
                BODY_X, ttl_y, &frame.title, fbm::C_TITLE_TOP, fbm::C_TITLE_BOT, fbm::C_GLOW, Font::Display, ts,
            );
            d.fill_rect(0, TOPBAR_H - 2, w, 2, fbm::C_ACCENT);
            d.fill_rect(0, TOPBAR_H, w, 1, fbm::C_GLOW);

            // 3. Content panel: thin edge + bitmap HUD corner emblems (one
            //    authored tile, mirrored into all four corners).
            let panel_w = w.saturating_sub(2 * PANEL_X);
            let panel_h = h.saturating_sub(PANEL_Y + BOTBAR_H + 4);
            d.stroke_rect(PANEL_X, PANEL_Y, panel_w, panel_h, 1, fbm::C_PANEL_EDGE);
            let em = fbm::EMBLEM_PX;
            let (ex1, ey1) = (PANEL_X + panel_w - em, PANEL_Y + panel_h - em);
            d.corner_emblem(PANEL_X, PANEL_Y, fbm::C_ACCENT, fbm::C_CELL_SEL, fbm::C_GLOW, false, false);
            d.corner_emblem(ex1, PANEL_Y, fbm::C_ACCENT, fbm::C_CELL_SEL, fbm::C_GLOW, true, false);
            d.corner_emblem(PANEL_X, ey1, fbm::C_ACCENT, fbm::C_CELL_SEL, fbm::C_GLOW, false, true);
            d.corner_emblem(ex1, ey1, fbm::C_ACCENT, fbm::C_CELL_SEL, fbm::C_GLOW, true, true);
            // Reactor-light nodes at the four panel-edge midpoints.
            let (ncx, ncy) = (PANEL_X + panel_w / 2, PANEL_Y + panel_h / 2);
            d.fill_rect(ncx - 3, PANEL_Y - 1, 7, 3, fbm::C_ACCENT2);
            d.fill_rect(ncx - 3, PANEL_Y + panel_h - 2, 7, 3, fbm::C_ACCENT2);
            d.fill_rect(PANEL_X - 1, ncy - 3, 3, 7, fbm::C_ACCENT2);
            d.fill_rect(PANEL_X + panel_w - 2, ncy - 3, 3, 7, fbm::C_ACCENT2);

            // 4. Body lines.
            let mut y = BODY_TOP;
            for (li, line) in frame.body.iter().enumerate() {
                let color = match line.kind {
                    LineKind::Normal => fbm::C_FG,
                    LineKind::Dim => fbm::C_DIM,
                    LineKind::Selected => fbm::C_FG,
                    LineKind::Error => fbm::C_ERR,
                    LineKind::Accent => fbm::C_ACCENT,
                };
                if line.kind == LineKind::Selected {
                    // Glowing "scanning bar": fill, bright left notch, lit edge.
                    let bx = PANEL_X + 3;
                    let bw = panel_w.saturating_sub(6);
                    d.fill_rect(bx, y - 2, bw, CELL_H + 2, fbm::C_SEL);
                    d.fill_rect(bx, y - 2, 3, CELL_H + 2, fbm::C_ACCENT);
                    d.fill_rect(bx, y - 2, bw, 1, fbm::C_CELL_SEL);
                }
                for &(hl_li, cx, cw, hl_color) in &frame.cell_hls {
                    if hl_li == li {
                        let (hx, hw, hh) = (BODY_X + cx * CELL_W, cw * CELL_W, CELL_H + 2);
                        // Dark fill keeps the cell's text legible; a bright
                        // 2px outline marks the active cell/column clearly.
                        d.fill_rect(hx, y - 2, hw, hh, hl_color);
                        d.stroke_rect(hx, y - 2, hw, hh, 2, fbm::C_CELL_SEL);
                    }
                }
                // Section headers: Display face, gradient-filled, flagged with
                // a mint gutter marker. Everything else uses the legible Body
                // face in a flat colour.
                if line.kind == LineKind::Accent {
                    d.fill_rect(PANEL_X + 4, y + 1, 3, CELL_H - 3, fbm::C_ACCENT2);
                    d.draw_text_grad(
                        BODY_X, y, &line.text, fbm::C_HEAD_TOP, fbm::C_HEAD_BOT, Font::Display, fbm::SCALE,
                    );
                } else {
                    d.draw_text(BODY_X, y, &line.text, color, Font::Body);
                }
                // Blinking insertion caret: a 2px vertical bar at the cell, drawn
                // over the text so it sits between glyphs without shifting them.
                if let Some((cl, cc)) = cursor {
                    if cl == li {
                        d.fill_rect(BODY_X + cc * CELL_W, y, fbm::SCALE, CELL_H, fbm::C_FG);
                    }
                }
                y += CELL_H;
            }

            // 5. Status bar: lit panel, neon top rule + echo, a mint status
            //    LED, and the gradient status text.
            let sy = h - BOTBAR_H;
            d.fill_grad_rect(0, sy, w, BOTBAR_H, fbm::C_BAR_LO, fbm::C_BAR);
            d.fill_rect(0, sy, w, 2, fbm::C_ACCENT);
            d.fill_rect(0, sy + 2, w, 1, fbm::C_GLOW);
            let st_y = sy + (BOTBAR_H - CELL_H) / 2 + 1;
            d.fill_rect(BODY_X, st_y + 4, 8, 8, fbm::C_ACCENT2);
            d.draw_text_grad(
                BODY_X + CELL_W, st_y, &frame.status, fbm::C_STAT_TOP, fbm::C_STAT_BOT, Font::Display, fbm::SCALE,
            );

            // Single contiguous blit + pointer: no clear-then-draw flicker.
            d.present((mx, my));
        });
    }

    fn build_frame(&mut self) -> Frame {
        let status = self.status.clone();
        // Match on the screen variant so every `Screen` is mapped to a frame
        // explicitly: adding a new variant becomes a compile error here rather
        // than silently rendering as the `_` fallback. The scrutinee's shared
        // borrow of `self.stack` ends before each arm's `&mut self` call, so the
        // store methods are free to re-borrow as needed.
        let mut frame = match self.stack.last() {
            None | Some(Screen::List { .. }) => self.frame_list(status),
            Some(Screen::Browser { .. }) => self.frame_browser(status),
            Some(Screen::RowView { .. }) => self.frame_rowview(status),
            Some(Screen::Editor(_)) => self.frame_editor(status),
            Some(Screen::Schema { .. }) => self.frame_schema(status),
            Some(Screen::Prompt { .. }) => self.frame_prompt(status),
            Some(Screen::Pick { .. }) => self.frame_pick(status),
            Some(Screen::Confirm { .. }) => self.frame_confirm(status),
            Some(Screen::Drives { .. }) => self.frame_drives(status),
            Some(Screen::Pci { .. }) => self.frame_pci(status),
            Some(Screen::Xhci { .. }) => self.frame_xhci(status),
            Some(Screen::CreateOsPick { .. }) => self.frame_create_os_pick(status),
            Some(Screen::UpgradePick { .. }) => self.frame_upgrade_pick(status),
            Some(Screen::FkPick { .. }) => self.frame_fk_pick(status),
            Some(Screen::InstallResult { .. }) => self.frame_install_result(status),
            Some(Screen::UpgradeResult { .. }) => self.frame_upgrade_result(status),
            Some(Screen::Builder(_)) => self.frame_builder(status),
            Some(Screen::RefCols { .. }) => self.frame_refcols(status),
            Some(Screen::About { .. }) => self.frame_about(status),
        };
        // Reflow the key-hint / options bars to the current resolution so they
        // never run off a narrow screen — done once here rather than in every
        // `frame_*`, and before `render` snapshots the hitmap, so the wrapped
        // lines stay clickable.
        wrap_shortcut_bars(&mut frame, self.body_cols());
        frame
    }

    /// Character cells available for body text at the current resolution: from
    /// the panel's left text inset (`BODY_X`) to its right edge (`PANEL_X`
    /// inset), less a one-cell gap so a wrapped bar never touches the bracket
    /// chrome. Drives [`wrap_shortcut_bars`].
    fn body_cols(&self) -> usize {
        let w = fbm::with(|d| d.width()).unwrap_or(0);
        w.saturating_sub(BODY_X + PANEL_X + CELL_W) / CELL_W
    }

    /// Choose the leftmost visible Browser column so the selected `col` stays on
    /// screen. Keeps as many left columns as fit: scrolls left when the cursor
    /// moves before the window, and advances the window from the left only as
    /// far as needed to bring a rightward cursor fully into view. A wide column
    /// can still exceed the panel on its own — then it anchors the left edge.
    fn browser_col_off(&mut self, table: &str, col: usize, mut col_off: usize) -> usize {
        let widths: Vec<usize> = self
            .store
            .get_table(table)
            .ok()
            .map(|t| {
                t.columns
                    .iter()
                    .map(|c| c.display_width.map(|w| w as usize).unwrap_or(DEFAULT_COL_W))
                    .collect()
            })
            .unwrap_or_default();
        let slot = |ci: usize| widths.get(ci).copied().unwrap_or(DEFAULT_COL_W) + 1;
        if col < col_off {
            col_off = col;
        }
        let avail = self.body_cols();
        while col_off < col {
            let span: usize = (col_off..=col).map(slot).sum();
            if span <= avail {
                break;
            }
            col_off += 1;
        }
        col_off
    }

    fn frame_list(&mut self, status: String) -> Frame {
        let sel = if let Screen::List { sel } = self.top() {
            *sel
        } else {
            0
        };
        let tables = self.store.list_tables().unwrap_or_default();
        let mut body = Vec::new();
        body.push(line(
            "Tables  [↑↓] select  [Enter] open  [c]reate  [d]rives  [n]ew OS on USB  top [u]p USB  [k]eyboard  [a]bout  [p]ower",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        if tables.is_empty() {
            body.push(line("(no tables yet — press 'c' to create one)", LineKind::Dim));
        }
        for (i, t) in tables.iter().enumerate() {
            let mut s = String::new();
            let _ = write!(
                s,
                "{:<28} {:>3} cols  {:>8} rows",
                t.name, t.columns, t.rows
            );
            body.push(line(
                &s,
                if i == sel {
                    LineKind::Selected
                } else {
                    LineKind::Normal
                },
            ).hit(Hit::Activate(i)));
        }
        Frame {
            title: "TablesOS — Table List".into(),
            bg: fbm::C_BG_LIST,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_browser(&mut self, status: String) -> Frame {
        let (table, row, col, top, col_off, filter, sort) = match self.top() {
            Screen::Browser {
                table,
                row,
                col,
                top,
                col_off,
                filter,
                sort,
            } => (table.clone(), *row, *col, *top, *col_off, filter.clone(), *sort),
            _ => unreachable!(),
        };
        let schema = self.store.get_table(&table).ok();
        // Per-column content width: the configured display width, or the
        // default. The on-screen slot is one wider, for the gap between columns.
        let widths: Vec<usize> = schema
            .as_ref()
            .map(|t| {
                t.columns
                    .iter()
                    .map(|c| c.display_width.map(|w| w as usize).unwrap_or(DEFAULT_COL_W))
                    .collect()
            })
            .unwrap_or_default();
        let content_w = |ci: usize| widths.get(ci).copied().unwrap_or(DEFAULT_COL_W);
        let mut body = Vec::new();
        body.push(line(
            "[↑↓←→] cell  [PgUp/Dn] page  [Enter] row  [i]nsert [u]pdate [d]elete  [o]rder  [s]chema  [Esc] back",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        let mut hdr_line_idx: Option<usize> = None;
        if let Some(t) = &schema {
            let mut hdr = String::new();
            for (ci, c) in t.columns.iter().enumerate() {
                if ci < col_off {
                    continue;
                }
                let fk_mark = if t.fks.iter().any(|f| f.from_col == c.name) {
                    "*"
                } else {
                    ""
                };
                let sort_mark = match sort {
                    Some((sc, true)) if sc == ci => " ↑",
                    Some((sc, false)) if sc == ci => " ↓",
                    _ => "",
                };
                hdr.push_str(&cell_pad(
                    &format!("{}{}{}", c.name, fk_mark, sort_mark),
                    content_w(ci),
                ));
            }
            body.push(line(&hdr, LineKind::Accent));
            hdr_line_idx = Some(body.len() - 1);
        }
        // For each FK source column, resolve every key to the referenced row's
        // reference label so the grid shows "id:5 name:John" rather than the
        // raw key. Each target table is scanned once per repaint.
        let mut fk_cell_labels: Vec<(usize, BTreeMap<String, String>)> = Vec::new();
        if let Some(t) = &schema {
            for fk in &t.fks {
                let Some(fci) = t.column_index(&fk.from_col) else {
                    continue;
                };
                let mut map = BTreeMap::new();
                if let (Ok(tt), Ok(trows)) =
                    (self.store.get_table(&fk.to_table), self.store.scan(&fk.to_table))
                {
                    if let Some(tci) = tt.column_index(&fk.to_col) {
                        for (_, trow) in &trows {
                            if let Some(Some(key)) = trow.get(tci) {
                                map.insert(key.display(), tt.reference_label(trow));
                            }
                        }
                    }
                }
                fk_cell_labels.push((fci, map));
            }
        }
        let vis = visible_rows();
        // Borrow the (memoised) row list only now — after every `self.store`
        // call above — so the FK-label scans don't fight the immutable borrow.
        let rows = self.cached_rows(&table, &filter, sort);
        let mut row_line_idx: Option<usize> = None;
        for (vi, (_, cells)) in rows.iter().enumerate().skip(top).take(vis) {
            let mut s = String::new();
            for (ci, cell) in cells.iter().enumerate() {
                if ci < col_off {
                    continue;
                }
                let txt = match cell {
                    None => "NULL".to_string(),
                    Some(Value::Str(v)) if v.is_empty() => "\"\"".to_string(),
                    Some(v) => {
                        // FK columns: show the referenced row's label if resolvable.
                        match fk_cell_labels.iter().find(|(f, _)| *f == ci) {
                            Some((_, map)) => map
                                .get(&v.display())
                                .cloned()
                                .unwrap_or_else(|| v.display()),
                            None => v.display(),
                        }
                    }
                };
                // Per-column display width: wider values are truncated here, in
                // the grid only — the stored value is untouched.
                s.push_str(&cell_pad(&txt, content_w(ci)));
            }
            body.push(line(
                &s,
                if vi == row {
                    LineKind::Selected
                } else {
                    LineKind::Normal
                },
            ).hit(Hit::Activate(vi)));
            if vi == row {
                row_line_idx = Some(body.len() - 1);
            }
        }
        if rows.is_empty() {
            body.push(line("(no rows)", LineKind::Dim));
        }
        // Column cursor: brighter rectangle on the selected column, both in
        // the header (always shown if there is a schema) and at the selected
        // cell within the highlighted row. The slot is the column's content
        // width plus the one-char gap; its x is the sum of the slots before it.
        let mut cell_hls: Vec<(usize, usize, usize, Rgb)> = Vec::new();
        // x is measured from the first *visible* column, so the cursor lines up
        // after the grid has been scrolled horizontally.
        let col_x: usize = (col_off..col).map(|ci| content_w(ci) + 1).sum();
        let col_slot = content_w(col) + 1;
        if let Some(li) = hdr_line_idx {
            cell_hls.push((li, col_x, col_slot, fbm::C_CELL_SEL_FILL));
        }
        if let Some(li) = row_line_idx {
            cell_hls.push((li, col_x, col_slot, fbm::C_CELL_SEL_FILL));
        }
        let title = match &filter {
            Some((c, v)) => format!("Browser — {table}  (filtered: {c} = {})", trunc(&v.display(), 24)),
            None => format!("Browser — {table}"),
        };
        Frame {
            title,
            bg: fbm::C_BG_BROWSER,
            body,
            status,
            cell_hls,
            cursor: None,
        }
    }

    fn frame_rowview(&mut self, status: String) -> Frame {
        let (table, id, sel) = match self.top() {
            Screen::RowView { table, id, sel } => (table.clone(), *id, *sel),
            _ => unreachable!(),
        };
        let t = self.store.get_table(&table).ok();
        let row = self.store.get_row(&table, id).ok();
        // Outgoing FK fields are annotated with the referenced row's label.
        let fk_labels = self.fk_field_labels(&table, id);
        let mut body = Vec::new();
        body.push(line("[↑↓] target  [Enter] navigate  [Esc] back", LineKind::Dim).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        if let (Some(t), Some(row)) = (&t, &row) {
            for (ci, c) in t.columns.iter().enumerate() {
                let val = match row.get(ci) {
                    Some(Some(v)) => v.display(),
                    _ => "NULL".to_string(),
                };
                // " → id:5 name:John" next to the key, for an FK column.
                let fk_note = fk_labels
                    .iter()
                    .find(|(fci, _)| *fci == ci)
                    .map(|(_, l)| format!("  → {l}"))
                    .unwrap_or_default();
                for (li, chunk) in wrap(&val, 70).into_iter().enumerate() {
                    let label = if li == 0 {
                        format!("{} ({}): ", c.name, c.ty.name())
                    } else {
                        "    ".to_string()
                    };
                    // The annotation rides on the first (or only) line.
                    let note = if li == 0 { fk_note.as_str() } else { "" };
                    body.push(line(&format!("{label}{chunk}{note}"), LineKind::Normal));
                }
            }
        }
        body.push(line("", LineKind::Normal));
        body.push(line("Relationships / Referenced by:", LineKind::Accent));
        for (i, (label, _)) in self.nav_targets(&table, id).iter().enumerate() {
            body.push(line(
                label,
                if i == sel {
                    LineKind::Selected
                } else {
                    LineKind::Normal
                },
            ).hit(Hit::Activate(i)));
        }
        Frame {
            title: format!("Row View — {table}"),
            bg: fbm::C_BG_ROW,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_editor(&mut self, status: String) -> Frame {
        let Screen::Editor(ed) = self.top() else {
            unreachable!()
        };
        let nfields = ed.cols.len();
        let mut body = Vec::new();
        let mut cursor = None;
        body.push(line(
            "[Tab/↑↓] field  [←→] caret  [Home/End]  type to edit  [Del] NULL  [PgDn] builder  [Enter] OK  [Esc] cancel",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        for (i, c) in ed.cols.iter().enumerate() {
            let f = &ed.fields[i];
            let focused = ed.focus == i;
            let nullbox = if c.nullable {
                if f.is_null {
                    "[x] NULL"
                } else {
                    "[ ] NULL"
                }
            } else {
                "        "
            };
            let shown = if f.is_null { String::new() } else { f.value.clone() };
            // The builder is on PgDn now that `→` moves the caret. The fixed
            // prefix (name/type/null-box) is measured rather than assumed, so a
            // long column name still places the caret over the right glyph.
            let prefix = format!("{:<16} {:<14} {} : ", c.name, c.ty.name(), nullbox);
            let view = if focused {
                let (v, col) = field_view(&shown, f.caret, 48);
                cursor = Some((body.len(), prefix.chars().count() + col));
                v
            } else {
                trunc(&shown, 48)
            };
            body.push(line(
                &format!("{prefix}{view}"),
                if focused {
                    LineKind::Selected
                } else {
                    LineKind::Normal
                },
            ).hit(Hit::Focus(i)));
            if let Some((ei, msg)) = &ed.error {
                if *ei == i {
                    body.push(line(&format!("    ⮡ {msg}"), LineKind::Error));
                }
            }
        }
        body.push(line("", LineKind::Normal));
        body.push(line(
            "[ OK ]",
            if ed.focus == nfields {
                LineKind::Selected
            } else {
                LineKind::Accent
            },
        ).hit(Hit::Activate(nfields)));
        body.push(line(
            "[ Cancel ]",
            if ed.focus == nfields + 1 {
                LineKind::Selected
            } else {
                LineKind::Normal
            },
        ).hit(Hit::Activate(nfields + 1)));
        let what = if ed.id.is_some() { "Update" } else { "Insert" };
        Frame {
            title: format!("Row Editor ({what}) — {}", ed.table),
            bg: fbm::C_BG_EDIT,
            body,
            status,
            cell_hls: Vec::new(),
            cursor,
        }
    }

    fn frame_builder(&mut self, status: String) -> Frame {
        let Screen::Builder(b) = self.top() else {
            unreachable!()
        };
        let mut body = Vec::new();
        let mut cursor = None;
        // Every part line is `"<label:14> : <value>"`, so the value (and the
        // caret) begins at this fixed column; continuation lines align under it.
        const HEAD: usize = 17;
        body.push(line(
            if b.has_now() {
                "[Tab/↑↓] field  [←→ Home/End] caret  type to edit  [n] = current value  [Enter] OK  [Esc] cancel"
            } else if b.has_next() {
                "[Tab/↑↓] field  [←→ Home/End] caret  type to edit  [n] = max+1  [Enter] OK  [Esc] cancel"
            } else {
                "[Tab/↑↓] field  [←→ Home/End] caret  type to edit  [Enter] OK  [Esc] cancel"
            },
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line(
            &format!("Editing '{}' ({})", b.col_name, b.ty.name()),
            LineKind::Accent,
        ));
        body.push(line("", LineKind::Normal));
        for (i, f) in b.fields.iter().enumerate() {
            let focused = b.focus == i;
            let kind = if focused {
                LineKind::Selected
            } else {
                LineKind::Normal
            };
            // Sign toggles are a single character; render them on one line. A
            // focused sign part carries the cursor right after the sign.
            if f.id.is_sign() {
                let shown = if f.neg { "-" } else { "+" };
                if focused {
                    cursor = Some((body.len(), HEAD + shown.chars().count()));
                }
                body.push(line(&format!("{:<14} : {}", f.label, shown), kind).hit(Hit::Focus(i)));
                continue;
            }
            // Everything else (integer/decimal digits, free text, year,
            // sub-second) is shown in full, wrapped across lines instead of
            // truncated, so a long value stays entirely readable in the
            // standalone builder. Continuation lines align under the value.
            let prefix = if f.id == PartId::Year && f.neg { "-" } else { "" };
            let full = format!("{}{}", prefix, f.buf);
            let chunks = wrap(&full, BUILDER_WRAP);
            if focused {
                // Place the cursor on the wrapped line/column its char index
                // falls on (clamped to the last line at a wrap boundary).
                let pos = prefix.chars().count() + f.caret;
                let mut chunk_i = pos / BUILDER_WRAP;
                let mut col = pos % BUILDER_WRAP;
                if chunk_i >= chunks.len() {
                    chunk_i = chunks.len() - 1;
                    col = BUILDER_WRAP;
                }
                cursor = Some((body.len() + chunk_i, HEAD + col));
            }
            for (ci, chunk) in chunks.iter().enumerate() {
                let head = if ci == 0 {
                    format!("{:<14} : ", f.label) // 14 label + " : " = HEAD
                } else {
                    " ".repeat(HEAD) // align continuation under the value
                };
                body.push(line(&format!("{head}{chunk}"), kind).hit(Hit::Focus(i)));
            }
        }
        body.push(line("", LineKind::Normal));
        // Live preview of the canonical value that will be stored — also
        // wrapped in full rather than truncated.
        for (ci, chunk) in wrap(&b.compose(), BUILDER_WRAP).iter().enumerate() {
            let head = if ci == 0 { "Preview: " } else { "         " };
            body.push(line(&format!("{head}{chunk}"), LineKind::Dim));
        }
        if let Some(msg) = &b.error {
            body.push(line(&format!("⮡ {msg}"), LineKind::Error));
        }
        body.push(line("", LineKind::Normal));
        if let Some(ni) = b.now_index() {
            body.push(line(
                "[ Now ]",
                if b.focus == ni {
                    LineKind::Selected
                } else {
                    LineKind::Accent
                },
            ).hit(Hit::Activate(ni)));
        }
        if let Some(xi) = b.next_index() {
            // The generated value rides along in the label so a glance (or a
            // hover) shows exactly what will be filled in.
            let label = match &b.next_value {
                Some(v) => format!("[ Next: {v} ]"),
                None => String::from("[ Next ]"),
            };
            body.push(line(
                &label,
                if b.focus == xi {
                    LineKind::Selected
                } else {
                    LineKind::Accent
                },
            ).hit(Hit::Activate(xi)));
        }
        body.push(line(
            "[ OK ]",
            if b.focus == b.ok_index() {
                LineKind::Selected
            } else {
                LineKind::Accent
            },
        ).hit(Hit::Activate(b.ok_index())));
        body.push(line(
            "[ Cancel ]",
            if b.focus == b.cancel_index() {
                LineKind::Selected
            } else {
                LineKind::Normal
            },
        ).hit(Hit::Activate(b.cancel_index())));
        Frame {
            title: format!("Field builder — {} ({})", b.col_name, b.ty.name()),
            bg: fbm::C_BG_EDIT,
            body,
            status,
            cell_hls: Vec::new(),
            cursor,
        }
    }

    fn frame_schema(&mut self, status: String) -> Frame {
        let (table, sel) = match self.top() {
            Screen::Schema { table, sel } => (table.clone(), *sel),
            _ => unreachable!(),
        };
        let t = self.store.get_table(&table).ok();
        let mut body = Vec::new();
        body.push(line(
            "[a]dd [r]ename [x]drop col [u]niq [w]idth [,/.] reorder [k]add fk [K]drop fk [R]ef cols [D]rop table [Esc]back",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        if let Some(t) = &t {
            for (i, c) in t.columns.iter().enumerate() {
                let width = c
                    .display_width
                    .map(|w| w.to_string())
                    .unwrap_or_else(|| "default".into());
                let s = format!(
                    "{:<20} {:<14} {} {:<6} w:{}",
                    c.name,
                    c.ty.name(),
                    if c.nullable { "NULL    " } else { "NOT NULL" },
                    if c.unique { "UNIQUE" } else { "" },
                    width,
                );
                body.push(line(
                    &s,
                    if i == sel {
                        LineKind::Selected
                    } else {
                        LineKind::Normal
                    },
                ).hit(Hit::Focus(i)));
            }
            body.push(line("", LineKind::Normal));
            body.push(line("Foreign keys:", LineKind::Accent));
            if t.fks.is_empty() {
                body.push(line("  (none)", LineKind::Dim));
            }
            for f in &t.fks {
                body.push(line(
                    &format!("  {} : {} → {}.{}", f.name, f.from_col, f.to_table, f.to_col),
                    LineKind::Normal,
                ));
            }
        }
        Frame {
            title: format!("Schema Editor — {table}"),
            bg: fbm::C_BG_SCHEMA,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_refcols(&mut self, status: String) -> Frame {
        let (table, order, sel) = match self.top() {
            Screen::RefCols { table, order, sel } => (table.clone(), order.clone(), *sel),
            _ => unreachable!(),
        };
        let t = self.store.get_table(&table).ok();
        let mut body = Vec::new();
        body.push(line(
            "[↑↓] column  [Enter/Space] toggle  [,/.] reorder  [c]lear  [Esc] back",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        body.push(line(
            "Reference columns — this row's label wherever it is referenced elsewhere.",
            LineKind::Accent,
        ));
        body.push(line("", LineKind::Normal));
        if let Some(t) = &t {
            for (i, c) in t.columns.iter().enumerate() {
                // `[2]` = this column is 2nd in the label; `[ ]` = not used.
                let mark = match order.iter().position(|x| *x == c.name) {
                    Some(p) => format!("[{}]", p + 1),
                    None => "[ ]".to_string(),
                };
                let s = format!("{:<4} {:<20} {}", mark, c.name, c.ty.name());
                body.push(line(
                    &s,
                    if i == sel {
                        LineKind::Selected
                    } else {
                        LineKind::Normal
                    },
                ).hit(Hit::Activate(i)));
            }
        }
        body.push(line("", LineKind::Normal));
        // Live preview of the label order; when none is set, show what the
        // automatic default (UNIQUE-or-first column) resolves to.
        let preview = if !order.is_empty() {
            order.join(" ")
        } else if let Some(t) = &t {
            let names: Vec<String> = t
                .ref_col_indices()
                .into_iter()
                .map(|ci| t.columns[ci].name.clone())
                .collect();
            if names.is_empty() {
                "(no columns)".into()
            } else {
                format!("(auto) {}", names.join(" "))
            }
        } else {
            String::new()
        };
        body.push(line(&format!("Label order: {preview}"), LineKind::Accent));
        Frame {
            title: format!("Reference Columns — {table}"),
            bg: fbm::C_BG_SCHEMA,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_prompt(&mut self, status: String) -> Frame {
        let (title, buf, caret) = match self.top() {
            Screen::Prompt { title, buf, caret, .. } => (title.clone(), buf.clone(), *caret),
            _ => unreachable!(),
        };
        Frame {
            title: "Input".into(),
            bg: fbm::C_BG_LIST,
            body: alloc::vec![
                line("", LineKind::Normal),
                line(&title, LineKind::Accent),
                line(&format!("> {buf}"), LineKind::Normal),
                line("", LineKind::Normal),
                line("[←→ Home/End] move   [Enter] accept   [Esc] cancel", LineKind::Dim).hit(Hit::Shortcuts),
            ],
            status,
            cell_hls: Vec::new(),
            // Input line is body index 2; the "> " prefix is 2 cells wide.
            cursor: Some((2, 2 + caret)),
        }
    }

    fn frame_pick(&mut self, status: String) -> Frame {
        let (title, options, sel) = match self.top() {
            Screen::Pick {
                title,
                options,
                sel,
                ..
            } => (title.clone(), options.clone(), *sel),
            _ => unreachable!(),
        };
        let mut body = alloc::vec![
            line(&title, LineKind::Accent),
            line("[↑↓] select  [Enter] choose  [Esc] cancel", LineKind::Dim).hit(Hit::Shortcuts),
            line("", LineKind::Normal),
        ];
        for (i, o) in options.iter().enumerate() {
            body.push(line(
                o,
                if i == sel {
                    LineKind::Selected
                } else {
                    LineKind::Normal
                },
            ).hit(Hit::Activate(i)));
        }
        Frame {
            title: "Pick".into(),
            bg: fbm::C_BG_SCHEMA,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_confirm(&mut self, status: String) -> Frame {
        let msg = match self.top() {
            Screen::Confirm { msg, .. } => msg.clone(),
            _ => unreachable!(),
        };
        Frame {
            title: "Confirm".into(),
            bg: fbm::C_BG_ROW,
            body: alloc::vec![
                line("", LineKind::Normal),
                line(&msg, LineKind::Error),
                line("", LineKind::Normal),
                line("[Enter] = confirm     [Esc] = cancel  (default: cancel)", LineKind::Dim).hit(Hit::Shortcuts),
            ],
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_drives(&mut self, status: String) -> Frame {
        let (drives, sel, top) = match self.top() {
            Screen::Drives { drives, sel, top } => (drives.clone(), *sel, *top),
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line(
            "[↑↓] select drive   [r] re-enumerate   [p]ci   [Esc] back",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line(
            "Read-only diagnostic. No writes happen here; TablesOS only ever uses the disk it booted from.",
            LineKind::Dim,
        ));
        body.push(line("", LineKind::Normal));

        for (i, d) in drives.iter().enumerate().skip(top) {
            push_drive_card(&mut body, d, i == sel, i);
        }

        Frame {
            title: "Drives — legacy IDE enumeration".into(),
            bg: fbm::C_BG_SCHEMA,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_pci(&mut self, status: String) -> Frame {
        let (devices, sel, top) = match self.top() {
            Screen::Pci { devices, sel, top } => (devices.clone(), *sel, *top),
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line(
            "[↑↓ PgUp/Dn] select   [Enter] inspect xHCI   [r] re-enumerate   [t] test 1s delay   [Esc] back",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line(
            "PCI bus enumeration. The kernel drives legacy IDE plus USB (xHCI/EHCI) mass storage; AHCI / NVMe are listed but not yet used.",
            LineKind::Dim,
        ));
        body.push(line(
            &format!(
                "Timing: PIT-calibrated TSC = {} ticks/µs (phase 2 of the USB-stack roadmap)",
                time::tsc_per_us()
            ),
            LineKind::Dim,
        ));
        body.push(line("", LineKind::Normal));
        if devices.is_empty() {
            body.push(line("(no PCI devices found)", LineKind::Dim));
        }
        let vis = pci_visible_rows();
        for (i, d) in devices.iter().enumerate().skip(top).take(vis) {
            let usb = pci::is_usb_host(d);
            let storage = pci::is_mass_storage(d);
            let tag = if usb {
                // The kernel really does drive USB now. Reflect that, and flag
                // the controller this session has actually brought up so the
                // user can tell the live one from the dormant siblings.
                let bar0 = pci::bar_address(d, 0);
                let driven = bar0 != 0
                    && xhci::current_bringup().is_some_and(|b| b.mmio_base == bar0);
                match d.prog_if {
                    0x30 if driven => "  ← USB xHCI (driven — [Enter] to manage)",
                    0x30 => "  ← USB xHCI ([Enter] to drive)",
                    0x20 => "  ← USB EHCI (driven at boot)",
                    0x10 => "  ← USB OHCI (legacy — not driven)",
                    0x00 => "  ← USB UHCI (legacy — not driven)",
                    _ => "  ← USB host controller",
                }
            } else if storage {
                "  ← storage"
            } else {
                ""
            };
            let s = format!(
                "{:02X}:{:02X}.{}  {:04X}:{:04X}  {:<22} {}{}",
                d.bus,
                d.slot,
                d.func,
                d.vendor,
                d.device,
                pci::class_name(d.class, d.subclass, d.prog_if),
                pci::vendor_name(d.vendor),
                tag,
            );
            body.push(line(
                &s,
                if i == sel {
                    LineKind::Selected
                } else {
                    LineKind::Normal
                },
            ).hit(Hit::Activate(i)));
        }
        body.push(line("", LineKind::Normal));
        body.push(line("Selected device:", LineKind::Accent));
        if let Some(d) = devices.get(sel) {
            body.push(line(
                &format!(
                    "  location: bus {:02X}  slot {:02X}  function {}",
                    d.bus, d.slot, d.func
                ),
                LineKind::Normal,
            ));
            body.push(line(
                &format!(
                    "  vendor:  0x{:04X}  {}",
                    d.vendor,
                    pci::vendor_name(d.vendor)
                ),
                LineKind::Normal,
            ));
            body.push(line(
                &format!("  device:  0x{:04X}  revision 0x{:02X}", d.device, d.revision),
                LineKind::Normal,
            ));
            body.push(line(
                &format!(
                    "  class:   {:02X}:{:02X}:{:02X}  ({})",
                    d.class,
                    d.subclass,
                    d.prog_if,
                    pci::class_name(d.class, d.subclass, d.prog_if)
                ),
                LineKind::Normal,
            ));
            body.push(line(
                &format!(
                    "  header:  0x{:02X}{}   IRQ line {} pin {}",
                    d.header_type,
                    if d.header_type & 0x80 != 0 {
                        " (multi-function)"
                    } else {
                        ""
                    },
                    d.irq_line,
                    d.irq_pin,
                ),
                LineKind::Normal,
            ));
            if d.header_type & 0x7F == 0 {
                let mut s = String::from("  BARs:   ");
                let mut any = false;
                for (i, b) in d.bars.iter().enumerate() {
                    if *b == 0 {
                        continue;
                    }
                    any = true;
                    let kind = if b & 1 == 1 {
                        format!("I/O 0x{:08X}", b & !0x3)
                    } else {
                        format!("MEM 0x{:08X}", b & !0xF)
                    };
                    let _ = write!(s, "[{i}]={kind}  ");
                }
                if !any {
                    s.push_str("(none)");
                }
                body.push(line(&s, LineKind::Normal));
            }
            if pci::is_usb_host(d) {
                body.push(line("", LineKind::Normal));
                match d.prog_if {
                    0x30 => {
                        body.push(line(
                            "USB 3.x host controller (xHCI). The kernel drives this controller:",
                            LineKind::Accent,
                        ));
                        body.push(line(
                            "press [Enter] to inspect ports, enumerate devices, and run the MSC pipeline.",
                            LineKind::Accent,
                        ));
                    }
                    0x20 => {
                        body.push(line(
                            "USB 2.0 host controller (EHCI). The kernel drives EHCI mass storage at",
                            LineKind::Accent,
                        ));
                        body.push(line(
                            "boot; a pendrive on this controller can hold the TablesOS volume.",
                            LineKind::Accent,
                        ));
                    }
                    _ => {
                        body.push(line(
                            "Legacy USB host controller (UHCI/OHCI). Not driven by the kernel —",
                            LineKind::Accent,
                        ));
                        body.push(line(
                            "connect storage through an xHCI or EHCI port instead.",
                            LineKind::Accent,
                        ));
                    }
                }
            }
        }
        Frame {
            title: "PCI devices".into(),
            bg: fbm::C_BG_SCHEMA,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_xhci(&mut self, status: String) -> Frame {
        let (info, dev, top) = match self.top() {
            Screen::Xhci { info, dev, top } => (info.clone(), *dev, *top),
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line(
            "[↑↓ PgUp/Dn] scroll  [r] re-read  [b] up  [e] enable-slot  [a] addr+desc  [c] config-desc  [g] configure EPs  [m] MSC probe  [w] write+verify  [Esc] back",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line(
            "Pipeline: [b]→[e]→[a]→[c]→[g]→[m]→[w] BlockDevice write+read round-trip on LBA 1 (phase 7).",
            LineKind::Dim,
        ));
        body.push(line("", LineKind::Normal));

        // The content overflows the panel, so window it against `top`. One
        // fewer than `visible_rows()` because the pipeline hint above is an
        // extra header line beyond the two `visible_rows()` already reserves.
        let vis = visible_rows().saturating_sub(1).max(1);
        for tl in self.xhci_content(&info, &dev).into_iter().skip(top).take(vis) {
            body.push(tl);
        }
        Frame {
            title: "xHCI controller — read-only inspection".into(),
            bg: fbm::C_BG_SCHEMA,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    /// Build the scrollable body of the xHCI inspector — register dumps, port
    /// state, and the results of each pipeline step. Every line is wrapped to
    /// the current panel width so long literals stay fully visible; the caller
    /// windows the result against the screen's `top` scroll offset.
    fn xhci_content(&self, info: &XhciInfo, dev: &PciDevice) -> Vec<TextLine> {
        let mut body = Vec::new();
        body.push(line(
            &format!(
                "Location: PCI {:02X}:{:02X}.{}   vendor:device 0x{:04X}:0x{:04X}",
                dev.bus, dev.slot, dev.func, dev.vendor, dev.device
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "MMIO BAR: 0x{:016X}   PCI cmd before enable: 0x{:04X}",
                info.mmio_base, info.pci_command_before
            ),
            LineKind::Normal,
        ));
        if !info.mmio_accessible {
            body.push(line(
                "MMIO BAR is above 4 GiB — would need extended paging to read.",
                LineKind::Error,
            ));
            return wrap_body(body, self.body_cols());
        }
        body.push(line("", LineKind::Normal));

        body.push(line("Capability registers", LineKind::Accent));
        body.push(line(
            &format!(
                "  HCIVERSION  {}    CAPLENGTH 0x{:02X}",
                xhci::version_string(info.hci_version),
                info.cap_length
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  MaxSlots    {}    MaxIntrs {}    MaxPorts {}",
                info.max_slots, info.max_intrs, info.max_ports
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  IST {}   ERST max {}   ScratchpadBufs {}",
                info.ist,
                1u32 << info.erst_max_pow2,
                info.max_scratchpad
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  AC64 (64-bit addr) {}   Context size {}-byte   xECP @ dword {}",
                if info.ac64 { "yes" } else { "no" },
                if info.csz_64 { 64 } else { 32 },
                info.xecp_dword
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  DBOFF 0x{:X}   RTSOFF 0x{:X}",
                info.dboff, info.rtsoff
            ),
            LineKind::Normal,
        ));
        body.push(line("", LineKind::Normal));

        body.push(line("Operational registers", LineKind::Accent));
        body.push(line(
            &format!("  USBCMD  0x{:08X}   {}", info.usbcmd, decode_usbcmd(info.usbcmd)),
            LineKind::Normal,
        ));
        body.push(line(
            &format!("  USBSTS  0x{:08X}   {}", info.usbsts, decode_usbsts(info.usbsts)),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  PAGESIZE 0x{:08X}   (supports {})",
                info.page_size_bits,
                decode_page_sizes(info.page_size_bits)
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  CRCR    0x{:016X}   DCBAAP 0x{:016X}",
                info.crcr, info.dcbaap
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  CONFIG  0x{:08X}   MaxSlotsEn = {}",
                info.config,
                info.config & 0xFF
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!("  MFINDEX 0x{:08X}   (microframe counter — increments while R/S=1)", info.mfindex),
            LineKind::Normal,
        ));
        body.push(line("", LineKind::Normal));

        body.push(line(
            &format!("Ports ({})", info.ports.len()),
            LineKind::Accent,
        ));
        if info.ports.is_empty() {
            body.push(line("  (none reported)", LineKind::Dim));
        }
        for p in &info.ports {
            let connected = if p.ccs { "CONNECTED " } else { "" };
            let powered = if p.pp { "powered " } else { "off " };
            let enabled = if p.ped { "enabled " } else { "disabled " };
            let resetting = if p.pr { "RESETTING " } else { "" };
            let oc = if p.oca { "OVERCURRENT " } else { "" };
            body.push(line(
                &format!(
                    "  port {:>2}  PORTSC 0x{:08X}  {}{}{}{}{}link={}",
                    p.index,
                    p.portsc,
                    connected,
                    powered,
                    enabled,
                    resetting,
                    oc,
                    xhci::pls_name(p.pls),
                ),
                LineKind::Normal,
            ));
            if p.ccs {
                body.push(line(
                    &format!("           speed: {}", xhci::speed_name(p.speed)),
                    LineKind::Accent,
                ));
            }
        }

        if let Some(b) = xhci::current_bringup() {
            body.push(line("", LineKind::Normal));
            body.push(line("Bring-up state (this session)", LineKind::Accent));
            body.push(line(
                &format!("  controller @ 0x{:016X}", b.mmio_base),
                LineKind::Normal,
            ));
            body.push(line(
                &format!(
                    "  MaxSlotsEn programmed: {}    page size: {} bytes",
                    b.max_slots_en, b.page_size_bytes
                ),
                LineKind::Normal,
            ));
            body.push(line(
                &format!("  DCBAA       @ 0x{:016X}", b.dcbaa_addr),
                LineKind::Normal,
            ));
            if b.scratchpad_count > 0 {
                body.push(line(
                    &format!(
                        "  Scratchpad  @ 0x{:016X}  ({} entries × {} bytes)",
                        b.scratchpad_arr_addr, b.scratchpad_count, b.page_size_bytes
                    ),
                    LineKind::Normal,
                ));
            } else {
                body.push(line("  Scratchpad  (controller requested 0 buffers)", LineKind::Normal));
            }
            body.push(line(
                &format!("  Cmd ring    @ 0x{:016X}  (256 TRBs, Link TRB at end)", b.cmd_ring_addr),
                LineKind::Normal,
            ));
            body.push(line(
                &format!("  Event ring  @ 0x{:016X}", b.event_ring_addr),
                LineKind::Normal,
            ));
            body.push(line(
                &format!("  ERST        @ 0x{:016X}  (1 segment)", b.erst_addr),
                LineKind::Normal,
            ));
        }

        if let Some(e) = xhci::current_enumeration() {
            body.push(line("", LineKind::Normal));
            body.push(line(
                &format!(
                    "Port enumeration — {} port(s) scanned, {} Enable-Slot completion(s)",
                    e.ports.len(),
                    e.slots.len()
                ),
                LineKind::Accent,
            ));
            for p in &e.ports {
                let kind = if p.message == "no device" {
                    LineKind::Dim
                } else {
                    LineKind::Normal
                };
                body.push(line(
                    &format!(
                        "  port {:>2}  PORTSC {:08X}→{:08X}  {}",
                        p.port, p.portsc_before, p.portsc_after, p.message
                    ),
                    kind,
                ));
            }
            if !e.slots.is_empty() {
                body.push(line("", LineKind::Normal));
                body.push(line("Slot assignments", LineKind::Accent));
                for s in &e.slots {
                    body.push(line(
                        &format!(
                            "  port {:>2}  speed {} ({})  → slot {}  cc={} ({})",
                            s.port,
                            s.speed,
                            xhci::speed_name(s.speed),
                            s.slot_id,
                            s.completion_code,
                            xhci::completion_code_name(s.completion_code),
                        ),
                        if s.completion_code == 1 {
                            LineKind::Normal
                        } else {
                            LineKind::Error
                        },
                    ));
                }
            }
        }

        let addressed = xhci::current_addressed();
        if !addressed.is_empty() {
            body.push(line("", LineKind::Normal));
            body.push(line("Addressed devices", LineKind::Accent));
            for a in &addressed {
                body.push(line(
                    &format!(
                        "  slot {} (port {}, speed {})  Address Device cc={} ({})",
                        a.slot_id,
                        a.port,
                        a.speed,
                        a.addr_completion_code,
                        xhci::completion_code_name(a.addr_completion_code),
                    ),
                    if a.addr_completion_code == 1 {
                        LineKind::Normal
                    } else {
                        LineKind::Error
                    },
                ));
                match &a.descriptor {
                    Some(d) => {
                        body.push(line(
                            &format!(
                                "    USB {:X}.{:02X}    class {:02X} ({})    subclass {:02X}  protocol {:02X}",
                                d.usb_bcd >> 8,
                                d.usb_bcd & 0xFF,
                                d.device_class,
                                xhci::device_class_name(d.device_class),
                                d.device_subclass,
                                d.device_protocol,
                            ),
                            LineKind::Normal,
                        ));
                        body.push(line(
                            &format!(
                                "    VID:PID  {:04X}:{:04X}    device {:X}.{:02X}    bMaxPacketSize0 = {}    {} config(s)",
                                d.id_vendor,
                                d.id_product,
                                d.device_bcd >> 8,
                                d.device_bcd & 0xFF,
                                d.max_packet_size_ep0,
                                d.num_configurations,
                            ),
                            LineKind::Normal,
                        ));
                        body.push(line(
                            &format!(
                                "    string indices  manufacturer={}  product={}  serial={}",
                                d.manufacturer_idx, d.product_idx, d.serial_idx,
                            ),
                            LineKind::Dim,
                        ));
                    }
                    None if a.addr_completion_code == 1 => {
                        body.push(line(
                            &format!(
                                "    descriptor fetch cc={} ({})",
                                a.descriptor_completion_code,
                                xhci::completion_code_name(a.descriptor_completion_code),
                            ),
                            LineKind::Error,
                        ));
                    }
                    None => {}
                }
                if let Some(cc) = a.eval_context_cc {
                    body.push(line(
                        &format!(
                            "    Evaluate Context cc={} ({})",
                            cc,
                            xhci::completion_code_name(cc),
                        ),
                        if cc == 1 { LineKind::Dim } else { LineKind::Error },
                    ));
                }
                if let Some(c) = &a.config {
                    let powered = if c.attributes & 0x40 != 0 {
                        "self-powered"
                    } else {
                        "bus-powered"
                    };
                    body.push(line(
                        &format!(
                            "    config #{}  {} interface(s)  {} mA  {}  total {} B",
                            c.config_value,
                            c.num_interfaces,
                            c.max_power_ma,
                            powered,
                            c.total_length,
                        ),
                        LineKind::Accent,
                    ));
                    for ifd in &c.interfaces {
                        body.push(line(
                            &format!(
                                "      interface {}.{}  class {:02X} ({})  sub {:02X}  proto {:02X}",
                                ifd.number,
                                ifd.alt_setting,
                                ifd.class,
                                xhci::device_class_name(ifd.class),
                                ifd.subclass,
                                ifd.protocol,
                            ),
                            LineKind::Normal,
                        ));
                        for ep in &ifd.endpoints {
                            body.push(line(
                                &format!(
                                    "        EP 0x{:02X}  {:>3}  {:<11} {} B  interval {}",
                                    ep.address,
                                    if ep.direction_in { "IN" } else { "OUT" },
                                    xhci::transfer_type_name(ep.transfer_type),
                                    ep.max_packet_size,
                                    ep.interval,
                                ),
                                LineKind::Normal,
                            ));
                        }
                    }
                } else if a.config_completion_code != 0 {
                    body.push(line(
                        &format!(
                            "    config fetch cc={} ({})",
                            a.config_completion_code,
                            xhci::completion_code_name(a.config_completion_code),
                        ),
                        LineKind::Error,
                    ));
                }
                if a.configure_endpoint_cc != 0 || a.set_config_cc != 0 {
                    body.push(line(
                        &format!(
                            "    Configure Endpoint cc={} ({})    SET_CONFIGURATION cc={} ({})",
                            a.configure_endpoint_cc,
                            xhci::completion_code_name(a.configure_endpoint_cc),
                            a.set_config_cc,
                            xhci::completion_code_name(a.set_config_cc),
                        ),
                        if a.configure_endpoint_cc == 1 && a.set_config_cc == 1 {
                            LineKind::Accent
                        } else {
                            LineKind::Error
                        },
                    ));
                    for ep in &a.configured_endpoints {
                        body.push(line(
                            &format!(
                                "      wired EP 0x{:02X}  {:>3}  {:<11} {} B  (DCI {})",
                                ep.address,
                                if ep.direction_in { "IN" } else { "OUT" },
                                xhci::transfer_type_name(ep.transfer_type),
                                ep.max_packet_size,
                                ep.dci,
                            ),
                            LineKind::Normal,
                        ));
                    }
                }
            }
        }

        let probes = xhci::current_msc();
        if !probes.is_empty() {
            body.push(line("", LineKind::Normal));
            body.push(line("Mass-storage probe (Bulk-Only Transport + SCSI)", LineKind::Accent));
            for p in &probes {
                body.push(line(
                    &format!(
                        "  slot {} interface {}  max LUN {} (GET_MAX_LUN cc={})",
                        p.slot_id, p.interface_number, p.max_lun, p.get_max_lun_cc
                    ),
                    LineKind::Normal,
                ));
                if let Some(inq) = &p.inquiry {
                    body.push(line(
                        &format!(
                            "    INQUIRY  type {:02X} ({})  {}  vendor: {:?}  product: {:?}  rev: {:?}",
                            inq.peripheral_device_type,
                            xhci::peripheral_device_type_name(inq.peripheral_device_type),
                            if inq.removable { "removable" } else { "fixed" },
                            inq.vendor,
                            inq.product,
                            inq.revision,
                        ),
                        LineKind::Normal,
                    ));
                } else {
                    body.push(line(
                        &format!(
                            "    INQUIRY  scsi-status=0x{:02X}",
                            p.inquiry_scsi_status
                        ),
                        LineKind::Error,
                    ));
                }
                body.push(line(
                    &format!(
                        "    TEST UNIT READY  scsi-status=0x{:02X}",
                        p.tur_scsi_status
                    ),
                    if p.tur_scsi_status == 0 { LineKind::Normal } else { LineKind::Error },
                ));
                if let Some(c) = &p.capacity {
                    let total = c.total_bytes();
                    let human = if total >= 1u64 << 30 {
                        format!("{}.{:02} GiB", total >> 30, (total * 100 >> 30) % 100)
                    } else if total >= 1u64 << 20 {
                        format!("{}.{:02} MiB", total >> 20, (total * 100 >> 20) % 100)
                    } else {
                        format!("{} B", total)
                    };
                    body.push(line(
                        &format!(
                            "    READ CAPACITY  last LBA {}  block {} B  → {} block(s), {}",
                            c.last_lba,
                            c.block_size,
                            c.total_blocks(),
                            human
                        ),
                        LineKind::Normal,
                    ));
                }
                if !p.first_block.is_empty() {
                    body.push(line(
                        &format!(
                            "    READ(10) LBA 0  scsi-status=0x{:02X}  first {} byte(s):",
                            p.read_scsi_status,
                            p.first_block.len().min(32),
                        ),
                        LineKind::Accent,
                    ));
                    body.push(line(
                        &format!("      {}", hex_dump(&p.first_block, 32)),
                        LineKind::Normal,
                    ));
                }
            }
        }

        if let Some(t) = &self.last_usb_test {
            body.push(line("", LineKind::Normal));
            body.push(line(
                "BlockDevice write/read self-test",
                LineKind::Accent,
            ));
            body.push(line(
                &format!(
                    "  slot {} LBA {}: {}",
                    t.slot_id,
                    t.lba,
                    t.message,
                ),
                if t.verify_ok { LineKind::Accent } else { LineKind::Error },
            ));
            if !t.readback_head.is_empty() {
                body.push(line(
                    &format!("  readback head: {}", hex_dump(&t.readback_head, 32)),
                    LineKind::Normal,
                ));
            }
        }

        wrap_body(body, self.body_cols())
    }

    fn frame_create_os_pick(&mut self, status: String) -> Frame {
        let (candidates, sel) = match self.top() {
            Screen::CreateOsPick { candidates, sel } => (candidates.clone(), *sel),
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line(
            "[↑↓] select  [Enter] use this drive  [Esc] cancel",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line(
            "Picks a USB drive to overwrite with a fresh TablesOS image. The booted disk is excluded automatically.",
            LineKind::Dim,
        ));
        body.push(line("", LineKind::Normal));
        if candidates.is_empty() {
            body.push(line("(no eligible USB drives)", LineKind::Dim));
        }
        for (i, (slot_id, d)) in candidates.iter().enumerate() {
            let head = if i == sel {
                LineKind::Selected
            } else {
                LineKind::Accent
            };
            body.push(line(
                &format!(
                    "[{}]  {}    slot {}    {} MiB",
                    d.slot,
                    d.model,
                    slot_id,
                    d.lba28_sectors / 2048
                ),
                head,
            ).hit(Hit::Activate(i)));
            let mbr_desc = match &d.mbr {
                MbrInfo::TablesOs { version, sys_guid, .. } => alloc::format!(
                    "  currently: TablesOS disk ({})  GUID {}",
                    tablestore::version_string(*version),
                    ata::fmt_guid(sys_guid),
                ),
                MbrInfo::Partitioned { parts } => alloc::format!(
                    "  currently: classic MBR with {} partition(s)",
                    parts.len()
                ),
                MbrInfo::Blank => "  currently: empty disk (all zeros)".into(),
                MbrInfo::Unknown => "  currently: unrecognised MBR".into(),
                MbrInfo::Unreadable => "  sector 0 could not be read".into(),
            };
            body.push(line(&mbr_desc, LineKind::Normal));
        }
        Frame {
            title: "Install TablesOS on USB — pick a target".into(),
            bg: fbm::C_BG_EDIT,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_fk_pick(&mut self, status: String) -> Frame {
        let (from_col, to_table, to_col, rows, sel, top) = match self.top() {
            Screen::FkPick { from_col, to_table, to_col, rows, sel, top, .. } => {
                (from_col.clone(), to_table.clone(), to_col.clone(), rows.clone(), *sel, *top)
            }
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line(
            "[↑↓] select  [PgUp/Dn] page  [Enter] pick  [Esc] cancel",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        body.push(line(
            &format!("Pick a {to_table} for '{from_col}' (references {to_col})"),
            LineKind::Accent,
        ));
        body.push(line("", LineKind::Normal));
        let visible = visible_rows().max(1);
        for (i, (_id, label)) in rows.iter().enumerate() {
            if i < top {
                continue;
            }
            if i >= top + visible {
                break;
            }
            let kind = if i == sel {
                LineKind::Selected
            } else {
                LineKind::Normal
            };
            body.push(line(label, kind).hit(Hit::Activate(i)));
        }
        // If the list is empty, show a message.
        if rows.is_empty() {
            body.push(line("(no rows to pick from)", LineKind::Dim));
        }
        Frame {
            title: format!("FK Picker — {}", to_table),
            bg: fbm::C_BG_EDIT,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_install_result(&mut self, status: String) -> Frame {
        let report = match self.top() {
            Screen::InstallResult { report } => report.clone(),
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line(
            "[Enter] or [Esc] back to Table List",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        body.push(line(
            &format!("Install report — target USB slot {}", report.target_slot),
            LineKind::Accent,
        ));
        body.push(line(
            &format!(
                "  Sectors of boot prefix written: {}  /  data location LBA: {}",
                report.sectors_copied, self.data_lba
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!("  Volume sectors formatted: {}", report.volume_sectors),
            LineKind::Normal,
        ));
        body.push(line(
            &format!("  New system GUID: {}", ata::fmt_guid(&report.new_sys_guid)),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  Verify MBR re-read: {}    Verify volume mount: {}",
                if report.verify_mbr_ok { "OK" } else { "FAIL" },
                if report.verify_mount_ok { "OK" } else { "FAIL" }
            ),
            if report.verify_mount_ok {
                LineKind::Accent
            } else {
                LineKind::Error
            },
        ));
        body.push(line("", LineKind::Normal));
        body.push(line(
            &format!("Outcome: {}", report.message),
            if report.verify_mount_ok {
                LineKind::Normal
            } else {
                LineKind::Error
            },
        ));
        Frame {
            title: "TablesOS install — result".into(),
            bg: fbm::C_BG_ROW,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_upgrade_pick(&mut self, status: String) -> Frame {
        let (candidates, sel) = match self.top() {
            Screen::UpgradePick { candidates, sel } => (candidates.clone(), *sel),
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line(
            "[↑↓] select  [Enter] top up this drive  [Esc] cancel",
            LineKind::Dim,
        ).hit(Hit::Shortcuts));
        body.push(line(
            &format!(
                "Upgrades an existing TablesOS USB volume to {} in place — its data is kept and migrated. Only older-or-equal, known versions are listed; the booted disk is excluded.",
                tablestore::VERSION_STR,
            ),
            LineKind::Dim,
        ));
        body.push(line("", LineKind::Normal));
        if candidates.is_empty() {
            body.push(line("(no upgradable TablesOS USB volumes)", LineKind::Dim));
        }
        for (i, (slot_id, d)) in candidates.iter().enumerate() {
            let head = if i == sel {
                LineKind::Selected
            } else {
                LineKind::Accent
            };
            body.push(line(
                &format!(
                    "[{}]  {}    slot {}    {} MiB",
                    d.slot,
                    d.model,
                    slot_id,
                    d.lba28_sectors / 2048
                ),
                head,
            ).hit(Hit::Activate(i)));
            let desc = match &d.mbr {
                MbrInfo::TablesOs { version, sys_guid, .. } => alloc::format!(
                    "  currently {}  →  {}   GUID {}",
                    tablestore::version_string(*version),
                    tablestore::VERSION_STR,
                    ata::fmt_guid(sys_guid),
                ),
                _ => "  (not a TablesOS volume)".into(),
            };
            body.push(line(&desc, LineKind::Normal));
        }
        Frame {
            title: "Top up version on USB — pick a volume".into(),
            bg: fbm::C_BG_EDIT,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_upgrade_result(&mut self, status: String) -> Frame {
        let report = match self.top() {
            Screen::UpgradeResult { report } => report.clone(),
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line("[Enter] or [Esc] back to Table List", LineKind::Dim).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        body.push(line(
            &format!("Top-up report — target USB slot {}", report.target_slot),
            LineKind::Accent,
        ));
        body.push(line(
            &format!(
                "  Version: {}  →  {}",
                tablestore::version_string(report.from_version),
                tablestore::version_string(report.to_version),
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  Data location LBA: {}  →  {}",
                report.old_data_lba, report.new_data_lba
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  Migration steps applied: {}    Live pages relocated: {}",
                report.migration_steps, report.pages_relocated
            ),
            LineKind::Normal,
        ));
        body.push(line(
            &format!("  System GUID (preserved): {}", ata::fmt_guid(&report.sys_guid)),
            LineKind::Normal,
        ));
        body.push(line(
            &format!("  Tables after upgrade: {}", report.tables_after),
            LineKind::Normal,
        ));
        body.push(line(
            &format!(
                "  Verify MBR re-read: {}    Verify volume mount: {}",
                if report.verify_mbr_ok { "OK" } else { "FAIL" },
                if report.verify_mount_ok { "OK" } else { "FAIL" }
            ),
            if report.verify_mount_ok {
                LineKind::Accent
            } else {
                LineKind::Error
            },
        ));
        body.push(line("", LineKind::Normal));
        body.push(line(
            &format!("Outcome: {}", report.message),
            if report.verify_mount_ok {
                LineKind::Normal
            } else {
                LineKind::Error
            },
        ));
        Frame {
            title: "TablesOS top up — result".into(),
            bg: fbm::C_BG_ROW,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }

    fn frame_about(&mut self, status: String) -> Frame {
        let top = match self.top() {
            Screen::About { top } => *top,
            _ => unreachable!(),
        };
        let mut body = Vec::new();
        body.push(line("[↑↓ PgUp/PgDn] scroll   [Esc] back", LineKind::Dim).hit(Hit::Shortcuts));
        body.push(line("", LineKind::Normal));
        let vis = visible_rows();
        for tl in about_lines().into_iter().skip(top).take(vis) {
            body.push(tl);
        }
        Frame {
            title: "About / Licenses".into(),
            bg: fbm::C_BG_SCHEMA,
            body,
            status,
            cell_hls: Vec::new(),
            cursor: None,
        }
    }
}

/// The full content of the About / Licenses screen: a short banner followed by
/// the embedded bundled-font attribution + SIL OFL 1.1 text, wrapped to the
/// panel width. Built once per key/repaint; cheap (a few KiB of static text).
/// Shared by `frame_about` (render) and the scroll handler (clamping `top`).
fn about_lines() -> Vec<TextLine> {
    let mut out = Vec::new();
    out.push(line(
        &format!("TablesOS {}", tablestore::VERSION_STR),
        LineKind::Accent,
    ));
    out.push(line(
        "A relational table store that boots on bare metal.",
        LineKind::Normal,
    ));
    out.push(line("", LineKind::Normal));
    out.push(line(
        "This software and its bundled components, and their licenses, follow.",
        LineKind::Dim,
    ));
    out.push(line("", LineKind::Normal));
    out.push(line(
        "================ TablesOS code — MIT License ================",
        LineKind::Accent,
    ));
    for src in crate::assets::PROJECT_LICENSE.lines() {
        for chunk in wrap(src, 88) {
            out.push(line(&chunk, LineKind::Dim));
        }
    }
    out.push(line("", LineKind::Normal));
    out.push(line(
        "================ Bundled font — SIL OFL 1.1 ================",
        LineKind::Accent,
    ));
    for src in crate::assets::FONT_LICENSE.lines() {
        for chunk in wrap(src, 88) {
            out.push(line(&chunk, LineKind::Dim));
        }
    }
    out
}

fn decode_usbcmd(v: u32) -> &'static str {
    if v & 0x01 != 0 {
        "(R/S = 1, controller RUNNING)"
    } else {
        "(R/S = 0, controller halted)"
    }
}

fn decode_usbsts(v: u32) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    if v & 0x001 != 0 {
        s.push_str("HCHalted ");
    }
    if v & 0x004 != 0 {
        s.push_str("HSE ");
    }
    if v & 0x008 != 0 {
        s.push_str("EINT ");
    }
    if v & 0x010 != 0 {
        s.push_str("PCD ");
    }
    if v & 0x100 != 0 {
        s.push_str("SSS ");
    }
    if v & 0x200 != 0 {
        s.push_str("RSS ");
    }
    if v & 0x400 != 0 {
        s.push_str("SRE ");
    }
    if v & 0x800 != 0 {
        s.push_str("CNR ");
    }
    if v & 0x1000 != 0 {
        s.push_str("HCE ");
    }
    if s.is_empty() {
        s.push_str("(idle, no flags)");
    }
    s
}

fn decode_page_sizes(bits: u32) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    for i in 0..16 {
        if bits & (1 << i) != 0 {
            let bytes: u32 = 1u32 << (i + 12);
            if !s.is_empty() {
                s.push_str(", ");
            }
            let _ = write!(s, "{} B", bytes);
        }
    }
    if s.is_empty() {
        s.push_str("(none)");
    }
    s
}

// ---- frame model ---------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum LineKind {
    Normal,
    Dim,
    Selected,
    Error,
    Accent,
}

/// What a mouse click on a body line does. Co-located with the line so the
/// layout code that emits a row is the single source of truth for *where* that
/// row sits and *what* clicking it means — the click handler never re-derives
/// the screen geometry (the drift that used to make clicks land on the wrong
/// row).
#[derive(Clone, Copy)]
enum Hit {
    /// Inert: clicking this line does nothing.
    None,
    /// Select item `i` (move the cursor / focus) without activating it.
    Focus(usize),
    /// Select item `i` and act as if Enter were pressed on it.
    Activate(usize),
    /// A key-hint bar: the bracketed `[…]` token under the pointer resolves to
    /// a key and is injected, so each shortcut is clickable.
    Shortcuts,
}

struct TextLine {
    text: String,
    kind: LineKind,
    hit: Hit,
}
impl TextLine {
    /// Attach a click target (builder-style, for the layout code).
    fn hit(mut self, h: Hit) -> TextLine {
        self.hit = h;
        self
    }
}
fn line(t: &str, kind: LineKind) -> TextLine {
    TextLine {
        text: t.to_string(),
        kind,
        hit: Hit::None,
    }
}

/// One body line's click target, captured from the last rendered frame for the
/// click handler. `text` is only populated for `Hit::Shortcuts`, where the
/// `[…]` token under the pointer has to be parsed.
struct ClickRow {
    hit: Hit,
    text: String,
}

/// Resolve a click at character column `cc` on a key-hint line to the key it
/// names. A "hot word" runs from a `[` to the next space, so `[c]reate` is
/// clickable across the whole word; the text inside the brackets names the
/// key. Multi-key navigation hints (`[↑↓]`, `[Tab/↑↓]`, `[,/.]`) name no single
/// action and resolve to nothing.
fn shortcut_at(text: &str, cc: usize) -> Option<Key> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '[' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut tok = String::new();
        while j < chars.len() && chars[j] != ']' {
            tok.push(chars[j]);
            j += 1;
        }
        // The hot word extends past the closing bracket to the next space.
        let mut end = (j + 1).min(chars.len());
        while end < chars.len() && chars[end] != ' ' {
            end += 1;
        }
        if (i..end).contains(&cc) {
            return key_for_token(&tok);
        }
        i = end;
    }
    None
}

/// Map the text inside a `[…]` hint to a key, or `None` for anything that is
/// not a single unambiguous action.
fn key_for_token(tok: &str) -> Option<Key> {
    match tok {
        "Enter" | "Enter/Space" => return Some(Key::Enter),
        "Esc" => return Some(Key::Esc),
        "Del" => return Some(Key::Delete),
        "Tab" => return Some(Key::Tab),
        "PgDn" => return Some(Key::PageDown),
        "PgUp" => return Some(Key::PageUp),
        "Space" => return Some(Key::Char(' ')),
        "→" => return Some(Key::Right),
        "←" => return Some(Key::Left),
        "↑" => return Some(Key::Up),
        "↓" => return Some(Key::Down),
        _ => {}
    }
    // A lone character is a literal shortcut (`c`, `D`, `,`, …); anything longer
    // is a multi-key hint with no single action.
    let mut it = tok.chars();
    let c = it.next()?;
    if it.next().is_none() && !matches!(c, '↑' | '↓' | '←' | '→') {
        Some(Key::Char(c))
    } else {
        None
    }
}

struct Frame {
    title: String,
    bg: Rgb,
    body: Vec<TextLine>,
    status: String,
    /// Per-cell overlay rectangles: `(body_line_idx, char_x, char_w, color)`.
    /// Painted on top of any row strip and underneath the text, so the text
    /// stays readable. Used to make the Table Browser's column cursor visible.
    cell_hls: Vec<(usize, usize, usize, Rgb)>,
    /// Text-input caret, `(body_line_idx, char_col_from_BODY_X)`. Drawn as a
    /// blinking vertical bar at that cell — an overlay that doesn't displace
    /// text, unlike inserting a glyph. Only the focused text screens set it.
    cursor: Option<(usize, usize)>,
}

fn visible_rows() -> usize {
    // Rows that fit in the body region: from `BODY_TOP` (already below the top
    // emblems) down to above the status bar and the bottom corner emblems,
    // minus the two leading lines (key-hint + column header).
    fbm::with(|d| {
        let bottom = d
            .height()
            .saturating_sub(BOTBAR_H + fbm::EMBLEM_PX + 8);
        (bottom.saturating_sub(BODY_TOP) / CELL_H).saturating_sub(2)
    })
    .unwrap_or(20)
}

/// Hex-dump the first `max` bytes of `b` as space-separated 2-digit
/// uppercase pairs. Used to show the first sector of a freshly-read MSC
/// device on the xHCI screen.
fn hex_dump(b: &[u8], max: usize) -> String {
    let n = b.len().min(max);
    let mut s = String::with_capacity(n * 3);
    for (i, byte) in b.iter().take(n).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{:02X}", byte);
    }
    s
}

/// Visible PCI-list rows. The PCI screen reserves an ~10-line detail panel
/// below the list, plus 3 help/header lines and the status bar — so the
/// scrollable region is smaller than the generic `visible_rows()`.
fn pci_visible_rows() -> usize {
    fbm::with(|d| d.rows().saturating_sub(16)).unwrap_or(15)
}

/// Render one drive's information as several body lines. The header line is
/// `LineKind::Selected` (full-row highlight) when this drive is the cursor
/// target; subsequent detail lines are always normal so the highlight just
/// names the focused card without smearing across all its details.
fn push_drive_card(body: &mut Vec<TextLine>, d: &DriveInfo, selected: bool, idx: usize) {
    let header_kind = if selected {
        LineKind::Selected
    } else {
        LineKind::Accent
    };
    if !d.present {
        body.push(line(
            &format!("[{}]  (no device)", d.slot),
            header_kind,
        ).hit(Hit::Focus(idx)));
        body.push(line("", LineKind::Normal));
        return;
    }
    let booted_tag = if d.booted { "  ← this disk" } else { "" };
    body.push(line(
        &format!("[{}]  {}{}", d.slot, d.model, booted_tag),
        header_kind,
    ).hit(Hit::Focus(idx)));
    body.push(line(
        &format!(
            "  serial: {}    firmware: {}",
            if d.serial.is_empty() { "—" } else { &d.serial },
            if d.firmware.is_empty() { "—" } else { &d.firmware },
        ),
        LineKind::Normal,
    ));
    let size_line = if d.lba48_sectors > 0 && d.lba48_sectors != d.lba28_sectors {
        format!(
            "  size: {} ({} sectors, LBA48)   LBA28 view: {}",
            ata::human_size_sectors(d.lba48_sectors),
            d.lba48_sectors,
            ata::human_size_sectors(d.lba28_sectors),
        )
    } else {
        format!(
            "  size: {} ({} sectors)",
            ata::human_size_sectors(d.lba28_sectors),
            d.lba28_sectors,
        )
    };
    body.push(line(&size_line, LineKind::Normal));
    body.push(line(
        &format!(
            "  MBR boot signature 0x55AA: {}",
            if d.boot_sig_ok { "present" } else { "missing" }
        ),
        LineKind::Normal,
    ));
    match &d.mbr {
        MbrInfo::TablesOs {
            version,
            data_loc_lba,
            stage2_lba,
            stage2_sectors,
            kernel_lba,
            kernel_sectors,
            kernel_load,
            kernel_entry,
            sys_guid,
        } => {
            body.push(line(
                &format!(
                    "  TablesOS MBR header — version {}   data @ LBA {}",
                    tablestore::version_string(*version), data_loc_lba
                ),
                LineKind::Accent,
            ));
            body.push(line(
                &format!(
                    "    stage2 @ LBA {}  ({} sectors)   kernel @ LBA {}  ({} sectors)",
                    stage2_lba, stage2_sectors, kernel_lba, kernel_sectors
                ),
                LineKind::Normal,
            ));
            body.push(line(
                &format!(
                    "    kernel load 0x{:08X}   entry 0x{:08X}",
                    kernel_load, kernel_entry
                ),
                LineKind::Normal,
            ));
            body.push(line(
                &format!("    system GUID: {}", ata::fmt_guid(sys_guid)),
                LineKind::Normal,
            ));
        }
        MbrInfo::Partitioned { parts } => {
            body.push(line(
                &format!("  Classic MBR — {} partition entr{}",
                    parts.len(),
                    if parts.len() == 1 { "y" } else { "ies" }),
                LineKind::Accent,
            ));
            for (i, p) in parts.iter().enumerate() {
                body.push(line(
                    &format!(
                        "    {}: type 0x{:02X} ({})  start LBA {}  {} sectors ({}){}",
                        i + 1,
                        p.type_byte,
                        ata::partition_type_name(p.type_byte),
                        p.start_lba,
                        p.sectors,
                        ata::human_size_sectors(p.sectors as u64),
                        if p.bootable { "  [bootable]" } else { "" },
                    ),
                    LineKind::Normal,
                ));
            }
        }
        MbrInfo::Blank => {
            body.push(line("  sector 0 is all zeros (uninitialised disk)", LineKind::Normal));
        }
        MbrInfo::Unknown => {
            body.push(line(
                "  sector 0 has no recognisable MBR layout",
                LineKind::Normal,
            ));
        }
        MbrInfo::Unreadable => {
            body.push(line(
                "  sector 0 could not be read",
                LineKind::Error,
            ));
        }
    }
    body.push(line("", LineKind::Normal));
}

// ---- single-line text-field editing --------------------------------------
//
// The three text inputs (Row Editor fields, the builder's typed parts, the
// input Prompt) share a `(text, caret)` model where `caret` is a char index in
// `0..=text.chars().count()`, so the arrow keys navigate identically in each.
// All work in char units, not bytes, so a stored multi-byte value is safe.

/// Byte offset of char index `idx` in `s` (clamped to `s.len()` at the end).
fn byte_of(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(b, _)| b).unwrap_or(s.len())
}

/// Insert `c` at the caret and step past it.
fn ce_insert(text: &mut String, caret: &mut usize, c: char) {
    let at = (*caret).min(text.chars().count());
    text.insert(byte_of(text, at), c);
    *caret = at + 1;
}

/// Delete the char before the caret (Backspace).
fn ce_backspace(text: &mut String, caret: &mut usize) {
    let at = (*caret).min(text.chars().count());
    if at > 0 {
        text.remove(byte_of(text, at - 1));
        *caret = at - 1;
    }
}

/// Delete the char at the caret (forward Delete).
fn ce_delete(text: &mut String, caret: &mut usize) {
    let n = text.chars().count();
    let at = (*caret).min(n);
    if at < n {
        text.remove(byte_of(text, at));
        *caret = at;
    }
}

/// Step the caret one char left.
fn ce_left(text: &str, caret: &mut usize) {
    *caret = (*caret).min(text.chars().count()).saturating_sub(1);
}

/// Step the caret one char right.
fn ce_right(text: &str, caret: &mut usize) {
    *caret = (*caret + 1).min(text.chars().count());
}

/// Window a single-line value to at most `width` glyphs around the caret,
/// returning `(visible text, caret column within it)`. The caret itself is a
/// separate blinking overlay, so no glyph is inserted and nothing shifts. The
/// window right-anchors on the caret once the value overflows, so typing at the
/// end keeps the tail in view; `caret_col` can equal `width` (just past the
/// last visible glyph, at the right edge).
fn field_view(text: &str, caret: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let caret = caret.min(n);
    let start = if caret > width { caret - width } else { 0 };
    let end = (start + width).min(n);
    (chars[start..end].iter().collect(), caret - start)
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Render one Table Browser cell: truncate `s` to `content_w` characters (an
/// over-long value gets a trailing `…`), then pad with spaces to `content_w + 1`
/// so adjacent columns keep a one-char gap. The result is always exactly
/// `content_w + 1` columns wide, which is what the column-cursor geometry and
/// the fixed-width header assume.
fn cell_pad(s: &str, content_w: usize) -> String {
    let mut out = trunc(s, content_w);
    let pad = (content_w + 1).saturating_sub(out.chars().count());
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

fn wrap(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return alloc::vec![String::new()];
    }
    chars.chunks(n).map(|c| c.iter().collect()).collect()
}

/// Reflow a body so every line fits within `cols` character cells: each line
/// is split into `cols`-wide chunks (over-wide register/literal dumps spill
/// onto continuation lines instead of running off the right edge), preserving
/// each line's [`LineKind`]. Used by the scrollable xHCI inspector so its
/// content is fully visible at any resolution.
fn wrap_body(lines: Vec<TextLine>, cols: usize) -> Vec<TextLine> {
    let cols = cols.max(16);
    let mut out = Vec::with_capacity(lines.len());
    for tl in lines {
        for chunk in wrap(&tl.text, cols) {
            out.push(line(&chunk, tl.kind));
        }
    }
    out
}

/// Reflow every key-hint / options bar in `frame` so it fits within `max_cols`
/// character cells, wrapping any over-wide bar across extra lines. Breaks land
/// only between options (never inside one — see [`wrap_options`]), and each
/// produced line keeps `Hit::Shortcuts`, so per-line click resolution still
/// works. Line-index references in `cell_hls`/`cursor` are shifted to track the
/// lines we insert. A no-op at wide resolutions, where every bar already fits.
fn wrap_shortcut_bars(frame: &mut Frame, max_cols: usize) {
    if max_cols == 0
        || !frame
            .body
            .iter()
            .any(|l| matches!(l.hit, Hit::Shortcuts) && l.text.chars().count() > max_cols)
    {
        return;
    }
    let mut new_body: Vec<TextLine> = Vec::with_capacity(frame.body.len() + 4);
    // `extra[i]` = lines inserted *before* original body line `i`, so a
    // reference to line `i` moves to `i + extra[i]`.
    let mut extra: Vec<usize> = Vec::with_capacity(frame.body.len());
    let mut acc = 0usize;
    for l in frame.body.drain(..) {
        extra.push(acc);
        if matches!(l.hit, Hit::Shortcuts) {
            let chunks = wrap_options(&l.text, max_cols);
            acc += chunks.len().saturating_sub(1);
            for c in chunks {
                new_body.push(line(&c, l.kind).hit(Hit::Shortcuts));
            }
        } else {
            new_body.push(l);
        }
    }
    let last = extra.last().copied().unwrap_or(0);
    let shift = |li: usize| li + extra.get(li).copied().unwrap_or(last);
    for hl in &mut frame.cell_hls {
        hl.0 = shift(hl.0);
    }
    if let Some((cl, cc)) = frame.cursor {
        frame.cursor = Some((shift(cl), cc));
    }
    frame.body = new_body;
}

/// Pack an options bar into the fewest lines that each fit `max_cols`, breaking
/// only between options. An *option* is a run of text delimited by two-or-more
/// spaces; single spaces live *inside* an option (e.g. `[n]ew OS on USB`), so a
/// break never lands mid-option. Options on the same line are rejoined with the
/// two-space separator. A lone option wider than `max_cols` still gets its own
/// line rather than being cut.
fn wrap_options(text: &str, max_cols: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for seg in split_options(text) {
        let seg_len = seg.chars().count();
        if cur.is_empty() {
            cur = seg;
        } else if cur.chars().count() + 2 + seg_len <= max_cols {
            cur.push_str("  ");
            cur.push_str(&seg);
        } else {
            lines.push(core::mem::take(&mut cur));
            cur = seg;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Split an options bar into its individual options, breaking on runs of two or
/// more spaces and preserving the single spaces that live inside an option.
fn split_options(text: &str) -> Vec<String> {
    let mut segs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut spaces = 0usize;
    for ch in text.chars() {
        if ch == ' ' {
            spaces += 1;
            continue;
        }
        if spaces >= 2 {
            if !cur.is_empty() {
                segs.push(core::mem::take(&mut cur));
            }
        } else if spaces == 1 && !cur.is_empty() {
            cur.push(' ');
        }
        spaces = 0;
        cur.push(ch);
    }
    if !cur.is_empty() {
        segs.push(cur);
    }
    segs
}

fn describe(e: &StoreError) -> String {
    match e {
        StoreError::Io => match crate::usb::xhci::take_last_msc_err() {
            Some(detail) => format!("I/O error: {detail}"),
            None => "I/O error".into(),
        },
        StoreError::Corrupt(m) => format!("corrupt store: {m}"),
        StoreError::CorruptDetail(m) => format!("corrupt store: {m}"),
        StoreError::OutOfSpace => "out of space (or transaction too large)".into(),
        StoreError::Parse(m) => m.clone(),
        StoreError::NullViolation { column } => format!("'{column}' must not be NULL"),
        StoreError::UniqueViolation { column } => format!("'{column}' must be unique"),
        StoreError::ForeignKeyViolation { fk } => {
            format!("foreign key '{fk}': no matching referenced row")
        }
        StoreError::SchemaRejected(m) => m.clone(),
        StoreError::NotFound(m) => format!("not found: {m}"),
        StoreError::Duplicate(m) => format!("already exists: {m}"),
    }
}

fn column_for_error(cols: &[Column], fks: &[ForeignKey], e: &StoreError) -> usize {
    let name = match e {
        StoreError::NullViolation { column } | StoreError::UniqueViolation { column } => {
            Some(column.as_str())
        }
        // A foreign-key violation names the *FK*, not the column; map it back
        // to the FK's source column so the error lands under that field.
        StoreError::ForeignKeyViolation { fk } => {
            fks.iter().find(|f| f.name == *fk).map(|f| f.from_col.as_str())
        }
        _ => None,
    };
    name.and_then(|n| cols.iter().position(|c| c.name == n))
        .unwrap_or(0)
}

/// Power off the machine.
///
/// Order matters and is the fix for the "shutdown hangs on real hardware" bug
/// (`solved-issues/Shutdown on real hardware.md`): the old code wrote only the
/// QEMU/Bochs/virt emulator power ports, which are unconnected on a physical PC,
/// so it silently fell into the `hlt` loop and the machine appeared to freeze.
///
///   1. **ACPI S5 soft-off** ([`crate::acpi::poweroff`]) — the correct,
///      universal power-off and the only thing that works on real hardware. It
///      also powers off QEMU (whose FADT puts PM1a_CNT at 0x604 with
///      SLP_TYPa=0), so this path is exercised in the emulator on every run.
///      It returns only if it could *not* power off.
///   2. **Emulator magic ports** — kept as a belt-and-suspenders fall-back in
///      case ACPI-table discovery ever fails. No-ops on real hardware.
///   3. **Halt with an explicit on-screen message** — so a machine that truly
///      can't self-power-off shows "safe to turn off" plus the ACPI failure
///      reason, instead of a frozen-looking UI (the laptop has no serial).
fn reboot() -> ! {
    use x86_64::instructions::port::Port;

    x86_64::instructions::interrupts::disable();
    fbm::with(|d| {
        let (w, h) = (d.width(), d.height());
        d.fill_rect(0, 0, w, h, fbm::C_BAR);
        let x = MARGIN * 2;
        d.draw_text_glow(x, h / 2 - CELL_H, "TABLESOS - REBOOTING", fbm::C_FG, fbm::C_GLOW, Font::Display);
        d.draw_text(x, h / 2 + CELL_H, "Restarting...", fbm::C_DIM, Font::Body);
        d.blit();
    });

    unsafe {
        // 0xCF9 reset-control port (modern PCH): pulse RST_CPU|SYS_RST. This is
        // the reliable hardware reset on real machines and QEMU.
        let mut cf9 = Port::<u8>::new(0xCF9);
        cf9.write(0x02);
        cf9.write(0x06);
        cf9.write(0x0E);
        // Fall back to the legacy 8042 keyboard-controller pulse-reset line.
        let mut kbd = Port::<u8>::new(0x64);
        kbd.write(0xFE);
    }
    // Nothing reset us — halt (a triple fault would also reboot, but halting is
    // safer than provoking undefined state).
    loop {
        x86_64::instructions::hlt();
    }
}

fn shutdown() -> ! {
    use x86_64::instructions::port::Port;

    // No more input; mask interrupts so nothing races the power-off sequence.
    x86_64::instructions::interrupts::disable();

    // A frozen UI looks like a crash — tell the user we're on our way out.
    fbm::with(|d| {
        let (w, h) = (d.width(), d.height());
        d.fill_rect(0, 0, w, h, fbm::C_BAR);
        let x = MARGIN * 2;
        d.draw_text_glow(x, h / 2 - CELL_H, "TABLESOS - SHUTTING DOWN", fbm::C_FG, fbm::C_GLOW, Font::Display);
        d.draw_text(x, h / 2 + CELL_H, "Powering off...", fbm::C_DIM, Font::Body);
        d.blit();
    });

    // 1. The real fix: ACPI S5 soft-off (works on physical hardware and QEMU).
    let why = crate::acpi::poweroff();

    // 2. Fall-back for emulators if ACPI discovery failed; no-ops on real iron.
    unsafe {
        Port::<u16>::new(0x604).write(0x2000); // QEMU >= 2.0
        Port::<u16>::new(0xB004).write(0x2000); // older QEMU / Bochs
        Port::<u16>::new(0x4004).write(0x3400); // virt
    }

    // 3. Nothing cut the power. Say so plainly (with the diagnostic reason) and
    //    halt, so it's safe to hold the hardware power button.
    serial_println!("shutdown: could not power off ({why}); halted");
    fbm::with(|d| {
        let (w, h) = (d.width(), d.height());
        d.fill_rect(0, 0, w, h, fbm::C_BAR);
        let x = MARGIN * 2;
        d.draw_text_glow(x, h / 2 - CELL_H, "IT IS NOW SAFE TO TURN OFF YOUR COMPUTER", fbm::C_FG, fbm::C_GLOW, Font::Display);
        d.draw_text(x, h / 2 + CELL_H, "Automatic power-off is unavailable on this machine.", fbm::C_DIM, Font::Body);
        d.draw_text(x, h / 2 + CELL_H * 2, why, fbm::C_DIM, Font::Body);
        d.blit();
    });
    loop {
        x86_64::instructions::hlt();
    }
}
