//! Global heap. The relational engine is `alloc`-based (it frees pages,
//! schemas and rows constantly), so a real freeing allocator is required.
//! We back `linked_list_allocator` with a fixed BSS region — no paging code
//! needed, and BSS costs nothing in the boot image.

// Note: no `#[alloc_error_handler]` — on current nightly the default handler
// (abort) is used, which avoids depending on the unstable feature gate.

use core::alloc::{GlobalAlloc, Layout};
use linked_list_allocator::LockedHeap;

/// 256 MiB heap. Lives in `.bss` (NOLOAD), so it is zero-cost on disk and on
/// load time — it just needs RAM at runtime, which the bootloader identity-maps
/// (0–4 GiB) and which TablesOS, the only thing running, owns entirely. Sized
/// to hold the off-screen scene buffer **and** one cached composite *per view*
/// (five slots — so switching back to a visited view is a memcpy, not a
/// full-screen ~1 s bilinear recompute), plus the decoded background bitmaps
/// and the engine's working set. At the framebuffer's full resolution six
/// scene-sized buffers dominate this (e.g. ~16 MiB each at 2560×1600).
const HEAP_SIZE: usize = 256 * 1024 * 1024;
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

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

pub fn init() {
    unsafe {
        let start = core::ptr::addr_of_mut!(HEAP) as *mut u8;
        ALLOCATOR.0.lock().init(start, HEAP_SIZE);
    }
}
