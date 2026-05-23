//! GDT + TSS with a dedicated stack for double faults, so a kernel stack
//! overflow produces a clean fault instead of a triple-fault reboot.

use spin::Lazy;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

static TSS: Lazy<TaskStateSegment> = Lazy::new(|| {
    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
        const STACK_SIZE: usize = 4096 * 5;
        static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
        let start = VirtAddr::from_ptr(core::ptr::addr_of!(STACK));
        start + STACK_SIZE as u64
    };
    tss
});

struct Selectors {
    code: SegmentSelector,
    tss: SegmentSelector,
}

static GDT: Lazy<(GlobalDescriptorTable, Selectors)> = Lazy::new(|| {
    let mut gdt = GlobalDescriptorTable::new();
    let code = gdt.append(Descriptor::kernel_code_segment());
    let tss = gdt.append(Descriptor::tss_segment(&TSS));
    (gdt, Selectors { code, tss })
});

pub fn init() {
    use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
    use x86_64::instructions::tables::load_tss;
    use x86_64::structures::gdt::SegmentSelector;

    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.code);
        // The selectors the bootloader left in SS/DS/ES index into *its* GDT;
        // against ours they would fault (e.g. resolve to the TSS descriptor)
        // the moment an interrupt pushes a stack frame. In 64-bit ring 0 a
        // null data selector is valid, so reset them.
        SS::set_reg(SegmentSelector::NULL);
        DS::set_reg(SegmentSelector::NULL);
        ES::set_reg(SegmentSelector::NULL);
        load_tss(GDT.1.tss);
    }
}
