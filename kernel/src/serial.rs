//! Minimal 16550 UART on COM1 — used only for boot/panic diagnostics
//! (`-serial stdio` under QEMU). It is not part of the user-facing system.

use core::fmt::{self, Write};
use spin::Mutex;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

pub struct Serial {
    data: Port<u8>,
    line_status: Port<u8>,
}

impl Serial {
    const fn new() -> Self {
        Serial {
            data: Port::new(COM1),
            line_status: Port::new(COM1 + 5),
        }
    }

    fn init(&mut self) {
        unsafe {
            Port::<u8>::new(COM1 + 1).write(0x00); // disable interrupts
            Port::<u8>::new(COM1 + 3).write(0x80); // enable DLAB
            Port::<u8>::new(COM1 + 0).write(0x03); // divisor lo (38400 baud)
            Port::<u8>::new(COM1 + 1).write(0x00); // divisor hi
            Port::<u8>::new(COM1 + 3).write(0x03); // 8N1
            Port::<u8>::new(COM1 + 2).write(0xC7); // FIFO, clear
            Port::<u8>::new(COM1 + 4).write(0x0B); // IRQs, RTS/DSR
        }
    }

    fn put(&mut self, b: u8) {
        unsafe {
            while self.line_status.read() & 0x20 == 0 {}
            self.data.write(b);
        }
    }
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
        }
        Ok(())
    }
}

static SERIAL: Mutex<Serial> = Mutex::new(Serial::new());

pub fn init() {
    SERIAL.lock().init();
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        let _ = SERIAL.lock().write_fmt(args);
    });
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => ($crate::serial::_print(format_args!($($arg)*)));
}
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}
