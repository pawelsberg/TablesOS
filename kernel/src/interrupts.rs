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

/// Monotonic tick from the PIT (used for the panic blink / debouncing).
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
    idt
});

pub fn init() {
    IDT.load();
    unsafe { PICS.lock().initialize() };
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
