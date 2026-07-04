//! PS/2 keyboard + mouse.
//!
//! The BIOS leaves the 8042 controller translating to scancode **set 1** with
//! the keyboard enabled, so the keyboard needs no init. The mouse (second
//! port) does: enable it, set defaults, turn on streaming. IRQ handlers feed
//! raw bytes here; the UI loop drains decoded [`Event`]s.

use spin::Mutex;
use x86_64::instructions::port::Port;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
    /// The GUI(Win)+Space chord switched the keyboard layout. Carries no
    /// character — a feedback signal so the UI can show the new layout.
    LayoutSwitched,
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Key(Key),
    /// Absolute pointer position after a move.
    MouseMove(usize, usize),
    /// Left button pressed at this position (a click for the UI — acts as Enter).
    Click(usize, usize),
    /// Right button pressed (the UI treats it as Esc — back/cancel). Position
    /// is omitted: like Esc it always acts on the focused screen.
    RightClick,
}

struct Kbd {
    shift: bool,
    altgr: bool,
    gui: bool,
    /// GUI+Space latch: typematic auto-repeat resends the Space make code
    /// while held, but the chord must cycle once per physical press.
    gui_space: bool,
    extended: bool,
}

/// Fixed-capacity event ring shared between the IRQ handlers (producers) and
/// the UI loop (consumer). Deliberately **heap-free**: the global allocator is
/// one spinlock shared with the main thread, which allocates with interrupts
/// enabled. A growing `VecDeque` would take that allocator lock *inside* the
/// keyboard/mouse interrupt; if the main thread were mid-allocation the IRQ
/// would spin on the held lock forever, hanging the core (mouse and keyboard
/// dead). A plain array can never allocate, so that deadlock cannot occur.
const QUEUE_CAP: usize = 512;

struct EventRing {
    buf: [Event; QUEUE_CAP],
    head: usize,
    len: usize,
}
impl EventRing {
    const fn new() -> EventRing {
        EventRing {
            buf: [Event::MouseMove(0, 0); QUEUE_CAP],
            head: 0,
            len: 0,
        }
    }
    /// Append an event; drop it if the ring is full (input outran the UI).
    fn push(&mut self, e: Event) {
        if self.len == QUEUE_CAP {
            return;
        }
        let tail = (self.head + self.len) % QUEUE_CAP;
        self.buf[tail] = e;
        self.len += 1;
    }
    fn pop(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let e = self.buf[self.head];
        self.head = (self.head + 1) % QUEUE_CAP;
        self.len -= 1;
        Some(e)
    }
}

static QUEUE: Mutex<EventRing> = Mutex::new(EventRing::new());
static KBD: Mutex<Kbd> = Mutex::new(Kbd {
    shift: false,
    altgr: false,
    gui: false,
    gui_space: false,
    extended: false,
});

// Mouse assembly + absolute state.
struct MouseState {
    packet: [u8; 3],
    idx: usize,
    x: i32,
    y: i32,
    max_x: i32,
    max_y: i32,
    left_was_down: bool,
    right_was_down: bool,
}
static MOUSE: Mutex<MouseState> = Mutex::new(MouseState {
    packet: [0; 3],
    idx: 0,
    x: 0,
    y: 0,
    max_x: 1023,
    max_y: 767,
    left_was_down: false,
    right_was_down: false,
});

/// These locks are also taken by the keyboard/mouse IRQ handlers. A spinlock
/// taken on the main path while an interrupt fires on the same core would
/// deadlock, so every main-side access masks interrupts first.
fn no_irq<R>(f: impl FnOnce() -> R) -> R {
    x86_64::instructions::interrupts::without_interrupts(f)
}

pub fn set_bounds(w: usize, h: usize) {
    no_irq(|| {
        let mut m = MOUSE.lock();
        m.max_x = w as i32 - 1;
        m.max_y = h as i32 - 1;
        m.x = m.max_x / 2;
        m.y = m.max_y / 2;
    });
}

pub fn poll() -> Option<Event> {
    no_irq(|| QUEUE.lock().pop())
}

pub fn mouse_pos() -> (usize, usize) {
    no_irq(|| {
        let m = MOUSE.lock();
        (m.x as usize, m.y as usize)
    })
}

fn push(e: Event) {
    QUEUE.lock().push(e);
}

// ---- keyboard (scancode set 1) ----

pub fn on_keyboard_byte(code: u8) {
    let mut k = KBD.lock();
    if code == 0xE0 {
        k.extended = true;
        return;
    }
    let released = code & 0x80 != 0;
    let make = code & 0x7F;
    let ext = k.extended;
    k.extended = false;

    // Modifier state. Right Alt is `E0 38` (AltGr); plain `38` is left Alt.
    if !ext && (make == 0x2A || make == 0x36) {
        k.shift = !released;
        return;
    }
    if ext && make == 0x38 {
        k.altgr = !released;
        return;
    }
    // GUI (Win) keys: `E0 5B` (left) / `E0 5C` (right). Releasing GUI also
    // re-arms the GUI+Space chord latch.
    if ext && (make == 0x5B || make == 0x5C) {
        k.gui = !released;
        if released {
            k.gui_space = false;
        }
        return;
    }
    if released {
        if !ext && make == 0x39 {
            k.gui_space = false; // Space break re-arms the chord
        }
        return;
    }
    // GUI+Space chord: latch so typematic repeat of the held Space cycles
    // the layout exactly once per physical press.
    if k.gui && !ext && make == 0x39 {
        if k.gui_space {
            return;
        }
        k.gui_space = true;
    }
    let mods = crate::keymap::Mods {
        shift: k.shift,
        altgr: k.altgr,
        gui: k.gui,
    };
    drop(k);

    // Everything else goes through the shared layout engine: scancode → HID
    // usage → (layout, modifiers) → Key. `translate` is heap-free, so it is
    // safe here inside the keyboard IRQ.
    let Some(usage) = crate::keymap::ps2_to_usage(make, ext) else {
        return;
    };
    if let Some(key) = crate::keymap::translate(usage, mods) {
        push(Event::Key(key));
    }
}

// ---- mouse ----

fn wait_write() {
    let mut status: Port<u8> = Port::new(0x64);
    for _ in 0..100_000 {
        if unsafe { status.read() } & 0x02 == 0 {
            return;
        }
    }
}
fn wait_read() {
    let mut status: Port<u8> = Port::new(0x64);
    for _ in 0..100_000 {
        if unsafe { status.read() } & 0x01 != 0 {
            return;
        }
    }
}

fn cmd(byte: u8) {
    wait_write();
    unsafe { Port::<u8>::new(0x64).write(byte) };
}
fn write_aux(byte: u8) {
    cmd(0xD4); // address the mouse
    wait_write();
    unsafe { Port::<u8>::new(0x60).write(byte) };
    wait_read();
    let _ack: u8 = unsafe { Port::<u8>::new(0x60).read() };
}

/// Enable and configure the PS/2 mouse. Best-effort: hardware may differ, but
/// QEMU's PS/2 mouse follows this sequence.
pub fn init_mouse() {
    // Run the whole handshake with the mouse IRQ masked so the handler does
    // not steal our ACK bytes.
    no_irq(init_mouse_inner);
}

fn init_mouse_inner() {
    cmd(0xA8); // enable aux device
    cmd(0x20); // read controller config
    wait_read();
    let mut cfg: u8 = unsafe { Port::<u8>::new(0x60).read() };
    cfg |= 0b10; // enable IRQ12
    cfg &= !0b10_0000; // enable mouse clock
    cmd(0x60);
    wait_write();
    unsafe { Port::<u8>::new(0x60).write(cfg) };
    write_aux(0xF6); // set defaults
    write_aux(0xF4); // enable data reporting
}

pub fn on_mouse_byte(byte: u8) {
    let mut m = MOUSE.lock();
    // Resync: first packet byte always has bit 3 set.
    if m.idx == 0 && byte & 0x08 == 0 {
        return;
    }
    let i = m.idx;
    m.packet[i] = byte;
    m.idx += 1;
    if m.idx < 3 {
        return;
    }
    m.idx = 0;

    let flags = m.packet[0];
    let mut dx = m.packet[1] as i32;
    let mut dy = m.packet[2] as i32;
    if flags & 0x10 != 0 {
        dx -= 256;
    }
    if flags & 0x20 != 0 {
        dy -= 256;
    }
    // Overflow bits → ignore that axis.
    if flags & 0xC0 != 0 {
        dx = 0;
        dy = 0;
    }
    let left = flags & 0x01 != 0;
    let right = flags & 0x02 != 0;
    drop(m);

    // PS/2 reports dy positive = up; the screen's Y grows downward, so negate
    // into the shared screen-space apply path.
    apply_motion(dx, -dy, left, right);
}

/// Apply a pointer delta (screen coordinates: `dy_down` positive = downward),
/// update the absolute position + button-edge state, and enqueue the resulting
/// [`Event`]s. Shared by the PS/2 IRQ path and the USB-HID feed below, so both
/// input sources move the same cursor and post to the same queue.
fn apply_motion(dx: i32, dy_down: i32, left: bool, right: bool) {
    let (px, py, left_edge, right_edge) = {
        let mut m = MOUSE.lock();
        m.x = (m.x + dx).clamp(0, m.max_x);
        m.y = (m.y + dy_down).clamp(0, m.max_y);
        let left_edge = left && !m.left_was_down;
        let right_edge = right && !m.right_was_down;
        m.left_was_down = left;
        m.right_was_down = right;
        (m.x as usize, m.y as usize, left_edge, right_edge)
    };
    push(Event::MouseMove(px, py));
    if left_edge {
        push(Event::Click(px, py));
    }
    if right_edge {
        push(Event::RightClick);
    }
}

/// Inject a pointer delta from an external source — the USB-HID boot mouse,
/// which is polled cooperatively from the UI loop rather than via an IRQ (the
/// xHCI transfer path allocates, which an interrupt handler must never do, per
/// the heap-free-IRQ invariant). `dy` is in screen coordinates (positive =
/// downward), matching the HID boot-mouse convention. Masks interrupts because
/// it touches the same `MOUSE`/`QUEUE` state the PS/2 IRQ handler does.
pub fn feed_mouse_delta(dx: i32, dy: i32, left: bool, right: bool) {
    no_irq(|| apply_motion(dx, dy, left, right));
}

/// Inject an already-decoded key from an external source — the USB-HID boot
/// keyboard, which (like the boot mouse) is polled cooperatively from the UI
/// loop rather than via an IRQ. The HID driver does its own usage→[`Key`]
/// translation and edge detection; this just enqueues the event onto the same
/// queue the PS/2 path uses, masking interrupts because it touches `QUEUE`.
pub fn feed_key(key: Key) {
    no_irq(|| push(Event::Key(key)));
}
