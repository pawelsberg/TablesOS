//! IDT, the 8259 PIC pair, and the keyboard/mouse/timer IRQ handlers.

use pic8259::ChainedPics;
use spin::{Lazy, Mutex};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::{gdt, ps2, serial_println};

pub const PIC1: u8 = 32;
const PIC2: u8 = 40;

#[derive(Clone, Copy)]
#[repr(u8)]
enum Irq {
    Timer = PIC1,
    Keyboard = PIC1 + 1,
    Mouse = PIC2 + 4, // IRQ12
}

pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC1, PIC2) });

/// PIT channel-0 (timer IRQ) rate programmed in `init`. 125 Hz makes one tick
/// exactly the 8 ms USB-HID polling cadence, so the idle loops can rest in
/// `hlt` (woken by this tick) instead of busy-spinning on the TSC. USB-HID
/// delivers no IRQ — it is polled cooperatively — so this tick is what keeps a
/// USB pointer smooth while the CPU actually sleeps.
pub const TICK_HZ: u64 = 125;

/// Monotonic tick from the PIT (used for the caret blink / countdowns).
pub static TICKS: Mutex<u64> = Mutex::new(0);

static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint);
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    }
    idt.general_protection_fault.set_handler_fn(gpf);
    idt.page_fault.set_handler_fn(page_fault);
    idt[Irq::Timer as u8].set_handler_fn(timer);
    idt[Irq::Keyboard as u8].set_handler_fn(keyboard);
    idt[Irq::Mouse as u8].set_handler_fn(mouse);
    // Spurious IRQ7/IRQ15. The 8259 raises these on real hardware when a line
    // deasserts before the CPU's interrupt-acknowledge; QEMU practically never
    // does. Without handlers the not-present IDT entry faults (#GP) the instant
    // interrupts are enabled, which panics before the framebuffer is even up —
    // a silent freeze in `init`. Every PIC glitch surfaces as exactly these two
    // vectors, so handling them covers all spurious cases.
    idt[PIC1 + 7].set_handler_fn(spurious_primary);
    idt[PIC2 + 7].set_handler_fn(spurious_secondary);
    idt
});

pub fn init() {
    IDT.load();
    unsafe { PICS.lock().initialize() };
    // Raise the PIT tick from the BIOS default ~18.2 Hz to TICK_HZ. Channel 0,
    // lobyte/hibyte, mode 2 (rate generator), binary. Channel 2 (used by
    // `time::init` for TSC calibration) is untouched.
    unsafe {
        use x86_64::instructions::port::Port;
        let div = (crate::time::PIT_HZ / TICK_HZ) as u16;
        Port::<u8>::new(0x43).write(0b0011_0100);
        let mut ch0: Port<u8> = Port::new(0x40);
        ch0.write(div as u8);
        ch0.write((div >> 8) as u8);
    }
    // Drain anything the 8042 buffered before we owned it — e.g. the break
    // (release) code of a key pressed in the boot-time resolution chooser,
    // which lands after stage2's `cli` and is never read. A leftover byte keeps
    // the output buffer full (OBF) and the keyboard IRQ line asserted; because
    // that IRQ is edge-triggered, no fresh edge occurs when we unmask, so the
    // byte is never consumed and every later keypress is wedged — the keyboard
    // looks dead. Reading port 0x60 until OBF clears restores a clean edge.
    unsafe {
        use x86_64::instructions::port::Port;
        let mut status: Port<u8> = Port::new(0x64);
        let mut data: Port<u8> = Port::new(0x60);
        let mut guard = 0;
        while status.read() & 1 != 0 && guard < 16 {
            let _ = data.read();
            guard += 1;
        }
    }
    // Unmask only timer (0), keyboard (1), cascade (2) on PIC1 and mouse
    // (IRQ12) on PIC2.
    unsafe {
        use x86_64::instructions::port::Port;
        Port::<u8>::new(0x21).write(0b1111_1000);
        Port::<u8>::new(0xA1).write(0b1110_1111);
    }
    x86_64::instructions::interrupts::enable();
}

fn eoi(irq: u8) {
    unsafe { PICS.lock().notify_end_of_interrupt(irq) }
}

/// Monotonic PIT tick count (`TICK_HZ`). The timer IRQ also locks `TICKS`, so
/// the read masks interrupts to avoid deadlocking against a tick on this core.
pub fn ticks() -> u64 {
    x86_64::instructions::interrupts::without_interrupts(|| *TICKS.lock())
}

/// Rest the CPU until the next interrupt — at most one PIT tick (~8 ms at
/// `TICK_HZ`), sooner if a PS/2 IRQ arrives. The power-friendly pacing step
/// for the cooperative USB-HID poll loops, replacing their `delay_ms(8)` TSC
/// busy-spin. If interrupts are off, `hlt` would sleep forever, so fall back
/// to the bounded busy-wait instead.
pub fn wait_for_tick() {
    if x86_64::instructions::interrupts::are_enabled() {
        x86_64::instructions::hlt();
    } else {
        crate::time::delay_ms(1000 / TICK_HZ);
    }
}

extern "x86-interrupt" fn breakpoint(f: InterruptStackFrame) {
    serial_println!("breakpoint: {:?}", f);
}

extern "x86-interrupt" fn double_fault(f: InterruptStackFrame, _e: u64) -> ! {
    panic!("double fault\n{:#?}", f);
}

extern "x86-interrupt" fn gpf(f: InterruptStackFrame, e: u64) {
    panic!("general protection fault (code {e:#x})\n{:#?}", f);
}

extern "x86-interrupt" fn page_fault(
    f: InterruptStackFrame,
    e: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;
    // `read_raw` is stable across x86_64 versions (returns the faulting
    // address as a u64) regardless of whether `read` returns a Result.
    panic!(
        "page fault at {:#x} ({:?})\n{:#?}",
        Cr2::read_raw(),
        e,
        f
    );
}

extern "x86-interrupt" fn timer(_f: InterruptStackFrame) {
    *TICKS.lock() += 1;
    eoi(Irq::Timer as u8);
}

extern "x86-interrupt" fn keyboard(_f: InterruptStackFrame) {
    use x86_64::instructions::port::Port;
    let scancode: u8 = unsafe { Port::new(0x60).read() };
    ps2::on_keyboard_byte(scancode);
    eoi(Irq::Keyboard as u8);
}

extern "x86-interrupt" fn mouse(_f: InterruptStackFrame) {
    use x86_64::instructions::port::Port;
    let byte: u8 = unsafe { Port::new(0x60).read() };
    ps2::on_mouse_byte(byte);
    eoi(Irq::Mouse as u8);
}

/// Read a PIC's In-Service Register via OCW3 (command port 0x20 or 0xA0).
fn read_isr(cmd_port: u16) -> u8 {
    use x86_64::instructions::port::Port;
    unsafe {
        let mut p: Port<u8> = Port::new(cmd_port);
        p.write(0x0B); // OCW3: next read returns the ISR
        p.read()
    }
}

/// Master-PIC IRQ7 vector. A *spurious* IRQ7 (ISR bit 7 clear) must not be
/// EOI'd — the PIC latched nothing. A genuine IRQ7 is EOI'd normally.
extern "x86-interrupt" fn spurious_primary(_f: InterruptStackFrame) {
    if read_isr(0x20) & 0x80 != 0 {
        eoi(PIC1 + 7);
    }
}

/// Slave-PIC IRQ15 vector. If spurious (slave ISR bit 7 clear) the slave
/// latched nothing, but the master still saw the cascade (IRQ2) — EOI the
/// master only; otherwise EOI both.
extern "x86-interrupt" fn spurious_secondary(_f: InterruptStackFrame) {
    if read_isr(0xA0) & 0x80 != 0 {
        eoi(PIC2 + 7);
    } else {
        unsafe { PICS.lock().notify_end_of_interrupt(PIC1 + 2) };
    }
}
