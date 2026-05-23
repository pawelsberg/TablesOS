//! Wall-clock read-out from the legacy CMOS real-time clock (ports
//! 0x70/0x71). Used by the Row Editor's date/time field builder to offer
//! "fill the current value" — the one place the OS needs to know what time it
//! is. We never *set* the clock and never enable its periodic IRQ; this is a
//! pure, polled read.
//!
//! The value reported is whatever wall clock the firmware keeps (UTC under
//! QEMU's default `-rtc base=utc`; possibly local time on bare metal). The
//! builder presents it as plain calendar components with a zero zone offset,
//! and the user can adjust before committing — so an off-by-a-zone clock is a
//! starting point, never a silent error.

use x86_64::instructions::interrupts::without_interrupts;
use x86_64::instructions::port::Port;

const ADDR: u16 = 0x70;
const DATA: u16 = 0x71;

/// A snapshot of the RTC's calendar fields, already normalised to binary and
/// 24-hour form. `year` is the full proleptic year (century applied).
#[derive(Debug, Clone, Copy)]
pub struct Rtc {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

unsafe fn read_reg(reg: u8) -> u8 {
    let mut addr: Port<u8> = Port::new(ADDR);
    let mut data: Port<u8> = Port::new(DATA);
    // Keep bit 7 set so selecting a register also leaves NMI disabled for the
    // brief access window (standard CMOS access etiquette); the actual RTC
    // registers we read live in 0x00..=0x32, so masking it back off is fine.
    addr.write(reg | 0x80);
    data.read()
}

fn update_in_progress() -> bool {
    // Status register A bit 7 (UIP) is set while the RTC is mid-update; reading
    // the time then can return a torn value.
    unsafe { read_reg(0x0A) & 0x80 != 0 }
}

/// Read one raw, untranslated snapshot once no update is in progress. Bounded
/// spins so a wedged clock can never hang the UI.
fn read_raw() -> Raw {
    for _ in 0..100_000 {
        if !update_in_progress() {
            break;
        }
        core::hint::spin_loop();
    }
    unsafe {
        Raw {
            second: read_reg(0x00),
            minute: read_reg(0x02),
            hour: read_reg(0x04),
            day: read_reg(0x07),
            month: read_reg(0x08),
            year: read_reg(0x09),
            century: read_reg(0x32),
            status_b: read_reg(0x0B),
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct Raw {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
    status_b: u8,
}

fn bcd(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) * 10)
}

/// Best-effort current wall-clock time, or `None` if the read never settles.
pub fn now() -> Option<Rtc> {
    without_interrupts(|| {
        // Read twice in a row and accept only when two consecutive snapshots
        // agree — guards against catching a value that ticked over mid-read.
        let mut last = read_raw();
        for _ in 0..10 {
            let cur = read_raw();
            if cur == last {
                return Some(translate(cur));
            }
            last = cur;
        }
        None
    })
}

fn translate(r: Raw) -> Rtc {
    let bcd_mode = r.status_b & 0x04 == 0; // bit 2 clear => values are BCD
    let h24 = r.status_b & 0x02 != 0; // bit 1 set => 24-hour format

    let pm = r.hour & 0x80 != 0; // 12-hour PM flag lives on the raw hour byte
    let raw_hour = r.hour & 0x7F;

    let (second, minute, mut hour, day, month, year, century) = if bcd_mode {
        (
            bcd(r.second),
            bcd(r.minute),
            bcd(raw_hour),
            bcd(r.day),
            bcd(r.month),
            bcd(r.year),
            bcd(r.century),
        )
    } else {
        (
            r.second, r.minute, raw_hour, r.day, r.month, r.year, r.century,
        )
    };

    if !h24 {
        // 12 -> 0 for AM, +12 for PM (noon/midnight handled by the %24).
        hour = hour % 12;
        if pm {
            hour += 12;
        }
    }

    // Use the century register when it holds something believable; otherwise
    // assume the 2000s, which is what every machine this boots on will be in.
    let full_year = if (19..=99).contains(&century) {
        century as u32 * 100 + year as u32
    } else {
        2000 + year as u32
    };

    Rtc {
        year: full_year,
        month,
        day,
        hour,
        minute,
        second,
    }
}
