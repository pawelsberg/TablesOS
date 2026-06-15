//! Global heap. The relational engine is `alloc`-based (it frees pages,
//! schemas and rows constantly), so a real freeing allocator is required.
//! We back `linked_list_allocator` with a region the *bootloader* reserved
//! and reported in `BootInfo` (free, identity-mapped, below 4 GiB, clear of
//! the kernel footprint) — see boot/layout.md.
//!
//! This is deliberately *not* a fixed array in `.bss` any more. A large static
//! heap forced the kernel's footprint, pinned at the 16 MiB load address, to
//! span hundreds of MiB of low RAM — which collided with firmware-reserved
//! memory on some laptops (an SGIN M15 Pro reserves everything from 256 MiB up)
//! and made the UEFI loader refuse to boot. Letting each bootloader *place* the
//! heap wherever there is free RAM dodges any such reservation. A small static
//! fallback remains so the kernel still comes up if the region is missing.

// Note: no `#[alloc_error_handler]` — on current nightly the default handler
// (abort) is used, which avoids depending on the unstable feature gate.

use core::alloc::{GlobalAlloc, Layout};
use linked_list_allocator::LockedHeap;

/// Small fallback heap, used only when the bootloader didn't report a valid
/// region. Big enough to bring the system up far enough to surface an error,
/// not to run the engine. It lives in `.bss`, so it is the only heap memory in
/// the kernel footprint — keep it small.
const FALLBACK_HEAP_SIZE: usize = 8 * 1024 * 1024;
static mut FALLBACK_HEAP: [u8; FALLBACK_HEAP_SIZE] = [0; FALLBACK_HEAP_SIZE];

/// A bootloader-provided region smaller than this is treated as invalid (the
/// engine's scene buffers alone need far more) and the fallback is used.
const MIN_HEAP: u64 = 16 * 1024 * 1024;
/// The kernel runs on the bootloader's identity map (0–4 GiB), so the heap must
/// lie entirely below 4 GiB.
const MAX_HEAP_END: u64 = 4 << 30;

#[global_allocator]
static ALLOCATOR: Heap = Heap(LockedHeap::empty());

struct Heap(LockedHeap);

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.0.dealloc(ptr, layout)
    }
}

extern "C" {
    /// Top of the kernel footprint (image + `.bss`), from the linker script.
    static __bss_end: u8;
}

/// Initialise the global heap over the bootloader-reported region `[base,
/// base+size)`, falling back to the small static heap when that region is
/// absent or fails validation. Returns the `(base, size)` actually used so the
/// caller can log it. Must be called before any allocation.
pub fn init(base: u64, size: u64) -> (u64, usize) {
    let kernel_end = core::ptr::addr_of!(__bss_end) as u64;
    // Trust the region only if it is non-overlapping with the kernel, sits in
    // the identity-mapped sub-4-GiB range, and is large enough to be useful.
    let valid = base >= kernel_end
        && size >= MIN_HEAP
        && base.checked_add(size).is_some_and(|end| end <= MAX_HEAP_END);
    unsafe {
        if valid {
            ALLOCATOR.0.lock().init(base as *mut u8, size as usize);
            (base, size as usize)
        } else {
            let start = core::ptr::addr_of_mut!(FALLBACK_HEAP) as *mut u8;
            ALLOCATOR.0.lock().init(start, FALLBACK_HEAP_SIZE);
            (start as u64, FALLBACK_HEAP_SIZE)
        }
    }
}
