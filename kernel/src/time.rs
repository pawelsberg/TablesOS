//! Precise µs / ms busy-wait delays. Phase 2 of the USB-stack roadmap.
//!
//! Approach: at boot, channel 2 of the legacy PIT (fixed 1.193182 MHz)
//! one-shots a known interval. The TSC is sampled before and after, which
//! gives us TSC ticks per microsecond regardless of CPU clock speed.
//! After that, `delay_us` busy-loops on `rdtsc` — cheap and precise enough
//! for every timing point USB enumeration requires (port reset 50 ms,
//! address recovery 2 ms, status-stage 5 µs, …).
//!
//! Channel 2 is used (not channel 0) because we own the gate via the PPI
//! register and don't have to fight an IRQ handler. The kernel is
//! single-threaded and polls everything anyway.

use core::arch::x86_64::_rdtsc;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

/// PIT base frequency, hard-wired in the 8254 spec.
const PIT_HZ: u64 = 1_193_182;

/// Calibration interval. 10 ms gives us ≈11932 PIT ticks (well under the
/// 16-bit counter's 65535 limit) and a TSC delta of ~30M ticks at 3 GHz —
/// plenty of resolution to divide down to a per-µs scale.
const CAL_US: u64 = 10_000;

static TSC_PER_US: AtomicU64 = AtomicU64::new(0);

/// One-time TSC calibration. Idempotent — the first successful call wins.
pub fn init() {
    if TSC_PER_US.load(Ordering::Relaxed) != 0 {
        return;
    }
    let count = PIT_HZ * CAL_US / 1_000_000;
    if count == 0 || count > 0xFFFF {
        return; // requested interval doesn't fit a 16-bit one-shot
    }

    let delta = unsafe {
        let mut ppi: Port<u8> = Port::new(0x61);
        // PPI bit 0 = GATE2 (enable channel-2 counting), bit 1 = SPEAKER.
        // Set GATE2, clear SPEAKER, leave everything else.
        let v = ppi.read();
        ppi.write((v & !0x02) | 0x01);

        // Command: channel 2, access lobyte then hibyte, mode 0 (one-shot),
        // binary count.
        let mut cmd: Port<u8> = Port::new(0x43);
        cmd.write(0b1011_0000);

        let mut ch2: Port<u8> = Port::new(0x42);
        ch2.write(count as u8);
        ch2.write((count >> 8) as u8);

        let t0 = _rdtsc();
        // OUT2 (bit 5 of PPI) goes high when the count expires.
        while ppi.read() & 0x20 == 0 {
            core::hint::spin_loop();
        }
        let t1 = _rdtsc();

        // Stop counting (clear GATE2). Leaves SPEAKER alone.
        let v = ppi.read();
        ppi.write(v & !0x01);

        t1 - t0
    };
    TSC_PER_US.store(delta / CAL_US, Ordering::Relaxed);
}

/// Calibrated TSC ticks per microsecond. Zero means calibration hasn't
/// run yet; callers can treat that as "use coarse fallback".
#[inline]
pub fn tsc_per_us() -> u64 {
    TSC_PER_US.load(Ordering::Relaxed)
}

/// Busy-wait approximately `us` microseconds. Precise once `init()` ran.
pub fn delay_us(us: u64) {
    let per = TSC_PER_US.load(Ordering::Relaxed);
    if per == 0 {
        // Pre-calibration fallback. Coarse, but guaranteed not to hang.
        for _ in 0..(us.saturating_mul(100)) {
            core::hint::spin_loop();
        }
        return;
    }
    let start = unsafe { _rdtsc() };
    let target = start + us.saturating_mul(per);
    while unsafe { _rdtsc() } < target {
        core::hint::spin_loop();
    }
}

pub fn delay_ms(ms: u64) {
    delay_us(ms.saturating_mul(1000));
}
