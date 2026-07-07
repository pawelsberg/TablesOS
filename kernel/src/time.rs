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
pub const PIT_HZ: u64 = 1_193_182;

static TSC_PER_US: AtomicU64 = AtomicU64::new(0);

/// Latch and read PIT channel-2's live count (16-bit, counts down).
unsafe fn read_ch2_count() -> u16 {
    let mut cmd: Port<u8> = Port::new(0x43);
    cmd.write(0b1000_0000); // latch-count command for channel 2
    let mut ch2: Port<u8> = Port::new(0x42);
    let lo = ch2.read() as u16;
    let hi = ch2.read() as u16;
    (hi << 8) | lo
}

/// One-time TSC calibration. Idempotent — the first successful call wins.
pub fn init() {
    if TSC_PER_US.load(Ordering::Relaxed) != 0 {
        return;
    }

    // Sample channel 2 twice and measure the TSC across the *same* span. The PIT
    // runs at a fixed 1.193182 MHz, so the counter's drop gives elapsed time
    // (and hence the CPU's TSC rate) independent of CPU clock. This avoids the
    // OUT2-bit poll (unwired on many real chipsets — it hung early boot) and the
    // mode-0 wrap/zero detection (which mis-measured on the Sony VAIO's PIT,
    // yielding a TSC rate ~300x too high → every USB delay hundreds of times too
    // long → a 5-minute boot). Load 0xFFFF and wait for a ~27 ms drop (0x8000
    // ticks), well short of a 16-bit wrap; a counter that never moves bails.
    let raw = unsafe {
        let mut ppi: Port<u8> = Port::new(0x61);
        // PPI bit 0 = GATE2 (enable channel-2 counting), bit 1 = SPEAKER.
        let v = ppi.read();
        ppi.write((v & !0x02) | 0x01);

        // Channel 2, access lobyte then hibyte, mode 0, binary; load 0xFFFF.
        let mut cmd: Port<u8> = Port::new(0x43);
        cmd.write(0b1011_0000);
        let mut ch2: Port<u8> = Port::new(0x42);
        ch2.write(0xFF);
        ch2.write(0xFF);

        let c0 = read_ch2_count();
        let t0 = _rdtsc();
        let mut spins: u32 = 0;
        let mut moved = false;
        loop {
            let c = read_ch2_count();
            if c0.wrapping_sub(c) >= 0x8000 {
                moved = true;
                break; // counter fell 0x8000 ticks ≈ 27 ms
            }
            spins += 1;
            if spins > 5_000_000 {
                break; // counter not advancing
            }
            core::hint::spin_loop();
        }
        let t1 = _rdtsc();

        // Stop counting (clear GATE2). Leaves SPEAKER alone.
        let v = ppi.read();
        ppi.write(v & !0x01);

        if moved {
            // elapsed_us = 0x8000 / 1.193182 MHz ≈ 27462 µs
            let elapsed_us = 0x8000u64 * 1_000_000 / PIT_HZ;
            (t1 - t0) / elapsed_us
        } else {
            0
        }
    };

    // Accept only a physically plausible TSC rate (~0.15–8 GHz). A real x86 TSC
    // is 150–8000 ticks/µs; anything outside means the PIT lied, and using it
    // would make every delay wildly wrong. Substitute a conservative ~2 GHz
    // estimate so delays stay within a small factor of correct rather than
    // hundreds of times off (the difference between a few-second and a
    // multi-minute boot). A zero (PIT never counted) keeps the coarse fallback.
    let per = match raw {
        0 => 0,
        r if (150..=8000).contains(&r) => r,
        _ => 2000,
    };
    if per != 0 {
        TSC_PER_US.store(per, Ordering::Relaxed);
    }
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
