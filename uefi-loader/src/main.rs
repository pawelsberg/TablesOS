//! TablesOS UEFI loader — `\EFI\BOOT\BOOTX64.EFI` on the image's FAT16 ESP.
//!
//! UEFI counterpart of `boot/stage1.s` + `boot/stage2.s`: firmware (no Secure
//! Boot) loads this PE binary from the EFI System Partition; it then ignores
//! the file system entirely and works from the raw disk, exactly like the
//! BIOS path:
//!
//! 1. Scan every whole-disk Block I/O handle for the `TBLSBOOT` header in
//!    sector 0 (see boot/layout.md) — that is the TablesOS disk.
//! 2. Read the flat kernel image from its raw LBAs into a staging buffer and
//!    reserve the kernel's whole RAM footprint (image + .bss + stack, the
//!    header's `kernel_mem_mib`) at the fixed load address 0x1000000.
//! 3. Pick the highest ≥1024×720 32-bpp GOP mode (the BIOS path does the same
//!    via VBE) and find the ACPI RSDP in the EFI configuration table.
//! 4. Build `BootInfo` and identity page tables (0–4 GiB in 2 MiB pages, plus
//!    the framebuffer, this image and the current stack if they sit higher).
//! 5. `ExitBootServices`, switch to our page tables, copy the kernel to
//!    0x1000000 and jump to it with `BootInfo` in RDI (SysV ABI) — from there
//!    boot is identical to the BIOS path.

#![no_std]
#![no_main]

mod efi;

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use efi::*;

// ---- on-disk TBLSBOOT header, sector 0 (boot/layout.md, version 2) ---------
const H_MAGIC: usize = 0x180;
const H_VERSION: usize = 0x188;
const H_DATA_LBA: usize = 0x18C;
const H_KMEM_MIB: usize = 0x19A;
const H_K_LBA: usize = 0x19C;
const H_K_SECS: usize = 0x1A0;
const H_K_LOAD: usize = 0x1A4;
const H_K_ENTRY: usize = 0x1A8;
const H_SYS_GUID: usize = 0x1AC;
const HEADER_VERSION: u16 = 2;

const SECTOR: usize = 512;
const PAGE: u64 = 4096;
const BOOT_MAGIC: u32 = 0x5342_544F; // 'OTBS'

/// What the TBLSBOOT header told us about the disk we are to boot.
struct BootDisk {
    io: *mut BlockIo,
    media_id: u32,
    data_lba: u64,
    kernel_lba: u64,
    kernel_sectors: u64,
    kernel_load: u64,
    kernel_entry: u64,
    kernel_mem_bytes: u64,
    sys_guid: [u8; 16],
}

struct Framebuffer {
    addr: u64,
    width: u32,
    height: u32,
    pitch: u32,
    bgr: bool,
}

static CON_OUT: AtomicPtr<SimpleTextOutput> = AtomicPtr::new(ptr::null_mut());

#[no_mangle]
pub extern "efiapi" fn efi_main(image: Handle, st: *mut SystemTable) -> Status {
    let st = unsafe { &mut *st };
    CON_OUT.store(st.con_out, Ordering::Relaxed);
    let bs = unsafe { &mut *st.boot_services };

    // The 5-minute boot watchdog would reset mid-load on a very slow USB read.
    unsafe { (bs.set_watchdog_timer)(0, 0, 0, ptr::null_mut()) };

    out("TablesOS UEFI loader\r\n");
    match boot(image, st, bs) {
        // `boot` only returns on failure; the message is already on screen.
        // Returning lets the firmware try the next boot option.
        Err(msg) => {
            out("UEFI boot failed: ");
            out(msg);
            out("\r\n");
            ERR_BIT | 1 // EFI_LOAD_ERROR
        }
        Ok(never) => match never {},
    }
}

enum Never {}

fn boot(image: Handle, st: &mut SystemTable, bs: &mut BootServices) -> Result<Never, &'static str> {
    let disk = find_tablesos_disk(bs)?;
    out("TablesOS disk found; loading kernel\r\n");

    // Sanity-check the header's load address (the value itself drives
    // everything below): at/above 16 MiB keeps the footprint clear of the
    // low-RAM islands firmware needs alive (OVMF: ACPI NVS at 8 MiB).
    if disk.kernel_load < 0x100_0000
        || disk.kernel_load & (PAGE - 1) != 0
        || disk.kernel_entry < disk.kernel_load
    {
        return Err("unexpected kernel load/entry address in header");
    }
    let kernel_bytes = disk.kernel_sectors * SECTOR as u64;
    if disk.kernel_mem_bytes < kernel_bytes {
        return Err("kernel_mem_mib smaller than kernel image");
    }
    let footprint = (disk.kernel_load, disk.kernel_load + disk.kernel_mem_bytes);

    // Reserve the kernel's whole RAM footprint at the fixed load address. If
    // the firmware granted it, nothing else can live there and the handoff is
    // trivially safe. If not (something already occupies part of the range),
    // we still proceed, but only after the final memory map proves the range
    // holds nothing that must outlive ExitBootServices.
    let mut claim_addr = footprint.0;
    let claimed = unsafe {
        (bs.allocate_pages)(
            ALLOCATE_ADDRESS,
            LOADER_DATA,
            pages(disk.kernel_mem_bytes),
            &mut claim_addr,
        )
    } == SUCCESS;
    if !claimed {
        out("note: kernel range busy pre-boot; will verify the memory map\r\n");
    }

    // This loader's own image must not sit inside the kernel footprint — the
    // copy below would overwrite the very code performing it.
    let mut li: *mut c_void = ptr::null_mut();
    let li_range = if unsafe { (bs.handle_protocol)(image, &LOADED_IMAGE_GUID, &mut li) } == SUCCESS
    {
        let li = unsafe { &*(li as *mut LoadedImage) };
        let base = li.image_base as u64;
        (base, base + li.image_size)
    } else {
        (0, 0)
    };
    if overlaps(li_range, footprint) {
        return Err("loader image loaded inside the kernel range");
    }

    // Stage the kernel anywhere below 4 GiB but outside the footprint.
    let staging = alloc_pages_avoiding(bs, pages(kernel_bytes), footprint)?;
    read_blocks_chunked(&disk, disk.kernel_lba, kernel_bytes, staging)?;

    let fb = pick_video_mode(bs)?;
    let rsdp = find_rsdp(st);

    // BootInfo — same layout the BIOS stage 2 builds at 0x7000 (boot/layout.md).
    let bootinfo = alloc_pages_avoiding(bs, 1, footprint)? as *mut u8;
    unsafe {
        ptr::write_bytes(bootinfo, 0, PAGE as usize);
        wr32(bootinfo, 0x00, BOOT_MAGIC);
        wr32(bootinfo, 0x04, fb.width);
        wr32(bootinfo, 0x08, fb.height);
        wr32(bootinfo, 0x0C, fb.pitch);
        wr64(bootinfo, 0x10, fb.addr);
        *bootinfo.add(0x18) = 4; // bytes per pixel
        *bootinfo.add(0x19) = if fb.bgr { 1 } else { 0 };
        wr64(bootinfo, 0x20, disk.data_lba);
        *bootinfo.add(0x28) = 0x80; // boot_drive: BIOS notion, fixed under UEFI
        ptr::copy_nonoverlapping(disk.sys_guid.as_ptr(), bootinfo.add(0x30), 16);
        wr64(bootinfo, 0x40, rsdp);
    }

    // Identity page tables: 0–4 GiB plus anything we still need that may sit
    // higher (framebuffer, this image, the firmware stack we are running on).
    let mut pool = PtPool::new(bs, footprint)?;
    let pml4 = pool.zeroed_page()?;
    map_range(pml4, &mut pool, 0, 4 << 30)?;
    let fb_bytes = fb.pitch as u64 * fb.height as u64;
    map_range(pml4, &mut pool, fb.addr, fb.addr + fb_bytes)?;
    if li_range.1 > li_range.0 {
        map_range(pml4, &mut pool, li_range.0, li_range.1)?;
    }
    let rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp) };
    map_range(pml4, &mut pool, rsp.saturating_sub(8 << 20), rsp + (8 << 20))?;

    out("exiting boot services\r\n");
    exit_boot_services(image, bs, footprint, claimed)?;

    // Firmware is gone. From here: no prints, no allocations, no returns.
    unsafe {
        core::arch::asm!("cli");
        core::arch::asm!("mov cr3, {}", in(reg) pml4, options(nostack));
        ptr::copy_nonoverlapping(
            staging as *const u8,
            disk.kernel_load as *mut u8,
            kernel_bytes as usize,
        );
        let entry: extern "sysv64" fn(*const u8) -> ! =
            core::mem::transmute(disk.kernel_entry as usize);
        entry(bootinfo)
    }
}

// ---- boot disk --------------------------------------------------------------

/// Scan every whole-disk Block I/O handle for the `TBLSBOOT` header.
fn find_tablesos_disk(bs: &mut BootServices) -> Result<BootDisk, &'static str> {
    let mut n: usize = 0;
    let mut handles: *mut Handle = ptr::null_mut();
    let status = unsafe {
        (bs.locate_handle_buffer)(
            BY_PROTOCOL,
            &BLOCK_IO_GUID,
            ptr::null_mut(),
            &mut n,
            &mut handles,
        )
    };
    if status != SUCCESS || n == 0 {
        return Err("no Block I/O handles");
    }

    // One page as the sector-0 read buffer (page alignment satisfies any
    // IoAlign the device demands).
    let mut buf_addr: u64 = 0xFFFF_F000;
    if unsafe { (bs.allocate_pages)(ALLOCATE_MAX_ADDRESS, LOADER_DATA, 1, &mut buf_addr) }
        != SUCCESS
    {
        return Err("sector buffer allocation failed");
    }
    let buf = buf_addr as *mut u8;

    let mut found: Option<BootDisk> = None;
    for i in 0..n {
        let handle = unsafe { *handles.add(i) };
        let mut io: *mut c_void = ptr::null_mut();
        if unsafe { (bs.handle_protocol)(handle, &BLOCK_IO_GUID, &mut io) } != SUCCESS {
            continue;
        }
        let io = io as *mut BlockIo;
        let media = unsafe { &*(*io).media };
        // Whole disks only (the kernel's LBA math, like the BIOS path, assumes
        // 512-byte sectors and absolute device LBAs).
        if media.media_present == 0
            || media.logical_partition != 0
            || media.block_size as usize != SECTOR
        {
            continue;
        }
        let status = unsafe {
            ((*io).read_blocks)(io, media.media_id, 0, SECTOR, buf)
        };
        if status != SUCCESS {
            continue;
        }
        let s = unsafe { core::slice::from_raw_parts(buf, SECTOR) };
        if &s[H_MAGIC..H_MAGIC + 8] != b"TBLSBOOT" || s[510] != 0x55 || s[511] != 0xAA {
            continue;
        }
        if rd16(s, H_VERSION) != HEADER_VERSION {
            out("skipping TablesOS disk with unsupported header version\r\n");
            continue;
        }
        let mut sys_guid = [0u8; 16];
        sys_guid.copy_from_slice(&s[H_SYS_GUID..H_SYS_GUID + 16]);
        if found.is_some() {
            // Two TablesOS disks attached: keep the first, like the BIOS path
            // keeps whatever drive the firmware booted; the kernel's GUID
            // identity gate protects against writing the wrong one.
            out("warning: multiple TablesOS disks; using the first\r\n");
            break;
        }
        found = Some(BootDisk {
            io,
            media_id: media.media_id,
            data_lba: rd64(s, H_DATA_LBA),
            kernel_lba: rd32(s, H_K_LBA) as u64,
            kernel_sectors: rd32(s, H_K_SECS) as u64,
            kernel_load: rd32(s, H_K_LOAD) as u64,
            kernel_entry: rd32(s, H_K_ENTRY) as u64,
            kernel_mem_bytes: rd16(s, H_KMEM_MIB) as u64 * (1 << 20),
            sys_guid,
        });
    }
    unsafe {
        (bs.free_pages)(buf_addr, 1);
        (bs.free_pool)(handles as *mut u8);
    }
    found.ok_or("no TablesOS disk (TBLSBOOT header) found")
}

/// Read `bytes` from `lba` into `dst`, 1 MiB per ReadBlocks call.
fn read_blocks_chunked(disk: &BootDisk, lba: u64, bytes: u64, dst: u64) -> Result<(), &'static str> {
    let mut done: u64 = 0;
    while done < bytes {
        let chunk = (bytes - done).min(1 << 20);
        let status = unsafe {
            ((*disk.io).read_blocks)(
                disk.io,
                disk.media_id,
                lba + done / SECTOR as u64,
                chunk as usize,
                (dst + done) as *mut u8,
            )
        };
        if status != SUCCESS {
            return Err("kernel read failed");
        }
        done += chunk;
    }
    Ok(())
}

// ---- video -------------------------------------------------------------------

/// Highest ≥1024×720 32-bpp RGB/BGR GOP mode, like stage 2's VBE pick.
fn pick_video_mode(bs: &mut BootServices) -> Result<Framebuffer, &'static str> {
    let mut gop: *mut c_void = ptr::null_mut();
    if unsafe { (bs.locate_protocol)(&GOP_GUID, ptr::null_mut(), &mut gop) } != SUCCESS {
        return Err("no Graphics Output Protocol");
    }
    let gop = gop as *mut Gop;
    let mode = unsafe { &*(*gop).mode };

    let mut best: Option<(u32, u64)> = None; // (mode number, area)
    for m in 0..mode.max_mode {
        let mut info: *mut GopModeInfo = ptr::null_mut();
        let mut size = 0usize;
        if unsafe { ((*gop).query_mode)(gop, m, &mut size, &mut info) } != SUCCESS {
            continue;
        }
        let i = unsafe { &*info };
        let ok_format = i.pixel_format == PIXEL_RGB_RESERVED_8BPC
            || i.pixel_format == PIXEL_BGR_RESERVED_8BPC;
        if !ok_format || i.horizontal_resolution < 1024 || i.vertical_resolution < 720 {
            continue;
        }
        let area = i.horizontal_resolution as u64 * i.vertical_resolution as u64;
        let better = match best {
            Some((bm, ba)) => area > ba || (area == ba && bm != mode.mode && m == mode.mode),
            None => true,
        };
        if better {
            best = Some((m, area));
        }
    }
    let (m, _) = best.ok_or("no >=1024x720 32bpp GOP mode")?;
    if m != mode.mode && unsafe { ((*gop).set_mode)(gop, m) } != SUCCESS {
        return Err("GOP SetMode failed");
    }

    // Re-read after SetMode — the mode struct now describes the active mode.
    let mode = unsafe { &*(*gop).mode };
    let info = unsafe { &*mode.info };
    Ok(Framebuffer {
        addr: mode.frame_buffer_base,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        pitch: info.pixels_per_scan_line * 4,
        bgr: info.pixel_format == PIXEL_BGR_RESERVED_8BPC,
    })
}

// ---- ACPI ---------------------------------------------------------------------

/// RSDP physical address from the EFI configuration table (ACPI 2.0 entry
/// preferred), or 0 — the kernel then falls back to the legacy BIOS-area scan.
fn find_rsdp(st: &SystemTable) -> u64 {
    let tables =
        unsafe { core::slice::from_raw_parts(st.configuration_table, st.number_of_table_entries) };
    let mut rsdp = 0u64;
    for t in tables {
        if t.vendor_guid == ACPI20_TABLE_GUID {
            return t.vendor_table as u64;
        }
        if t.vendor_guid == ACPI10_TABLE_GUID {
            rsdp = t.vendor_table as u64;
        }
    }
    rsdp
}

// ---- memory + paging ------------------------------------------------------------

fn pages(bytes: u64) -> usize {
    ((bytes + PAGE - 1) / PAGE) as usize
}

fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Allocate pages below 4 GiB, retrying when the firmware hands back a range
/// intersecting `avoid` (possible only when the kernel-footprint claim failed
/// and parts of that range are free). Rejected ranges are freed afterwards.
fn alloc_pages_avoiding(
    bs: &mut BootServices,
    count: usize,
    avoid: (u64, u64),
) -> Result<u64, &'static str> {
    let mut rejected = [0u64; 8];
    let mut n_rejected = 0;
    let mut result = Err("allocation kept landing in the kernel range");
    for _ in 0..8 {
        let mut addr: u64 = 0xFFFF_FFFF; // below 4 GiB (cap is inclusive)
        if unsafe { (bs.allocate_pages)(ALLOCATE_MAX_ADDRESS, LOADER_DATA, count, &mut addr) }
            != SUCCESS
        {
            result = Err("page allocation failed");
            break;
        }
        if !overlaps((addr, addr + count as u64 * PAGE), avoid) {
            result = Ok(addr);
            break;
        }
        rejected[n_rejected] = addr;
        n_rejected += 1;
        if n_rejected == rejected.len() {
            break;
        }
    }
    for &r in &rejected[..n_rejected] {
        unsafe { (bs.free_pages)(r, count) };
    }
    result
}

const PTE_P: u64 = 1;
const PTE_W: u64 = 1 << 1;
const PTE_PS: u64 = 1 << 7;

/// A small pool of zeroed page-table pages, allocated up front (16 pages: six
/// cover 0–4 GiB, the rest absorb high framebuffer/image/stack mappings).
struct PtPool {
    next: u64,
    end: u64,
}

impl PtPool {
    fn new(bs: &mut BootServices, avoid: (u64, u64)) -> Result<PtPool, &'static str> {
        const POOL_PAGES: usize = 16;
        let base = alloc_pages_avoiding(bs, POOL_PAGES, avoid)?;
        Ok(PtPool { next: base, end: base + POOL_PAGES as u64 * PAGE })
    }

    fn zeroed_page(&mut self) -> Result<u64, &'static str> {
        if self.next == self.end {
            return Err("page-table pool exhausted");
        }
        let p = self.next;
        self.next += PAGE;
        unsafe { ptr::write_bytes(p as *mut u8, 0, PAGE as usize) };
        Ok(p)
    }
}

/// Read table entry `idx` of the table page at `table`; if absent, allocate a
/// child table page and install it.
fn child_table(table: u64, idx: u64, pool: &mut PtPool) -> Result<u64, &'static str> {
    let entry = unsafe { &mut *((table + idx * 8) as *mut u64) };
    if *entry & PTE_P == 0 {
        let page = pool.zeroed_page()?;
        *entry = page | PTE_P | PTE_W;
    }
    Ok(*entry & 0x000F_FFFF_FFFF_F000)
}

/// Identity-map `[start, end)` with 2 MiB pages (bounds rounded outward).
fn map_range(pml4: u64, pool: &mut PtPool, start: u64, end: u64) -> Result<(), &'static str> {
    const TWO_MIB: u64 = 2 << 20;
    let mut addr = start & !(TWO_MIB - 1);
    while addr < end {
        let pdpt = child_table(pml4, (addr >> 39) & 0x1FF, pool)?;
        let pd = child_table(pdpt, (addr >> 30) & 0x1FF, pool)?;
        let entry = unsafe { &mut *((pd + ((addr >> 21) & 0x1FF) * 8) as *mut u64) };
        if *entry & PTE_P == 0 {
            *entry = addr | PTE_P | PTE_W | PTE_PS;
        }
        addr += TWO_MIB;
    }
    Ok(())
}

/// GetMemoryMap + ExitBootServices, with the spec-mandated retry when the map
/// key went stale. When the kernel footprint was not claimable up front, the
/// final memory map must prove the whole range is reclaimable after
/// ExitBootServices (free, boot-services or loader memory — nothing the
/// platform needs, like ACPI tables, runtime services or MMIO).
fn exit_boot_services(
    image: Handle,
    bs: &mut BootServices,
    footprint: (u64, u64),
    claimed: bool,
) -> Result<(), &'static str> {
    let mut buf: *mut u8 = ptr::null_mut();
    let mut buf_size = 0usize;
    for _ in 0..8 {
        let mut size = buf_size;
        let mut key = 0usize;
        let mut desc_size = 0usize;
        let mut desc_ver = 0u32;
        let status = unsafe {
            (bs.get_memory_map)(&mut size, buf, &mut key, &mut desc_size, &mut desc_ver)
        };
        if status == BUFFER_TOO_SMALL || buf.is_null() {
            if !buf.is_null() {
                unsafe { (bs.free_pool)(buf) };
                buf = ptr::null_mut();
            }
            // Slack: the allocation below itself changes the map a little.
            buf_size = size + 4 * 1024;
            if unsafe { (bs.allocate_pool)(LOADER_DATA, buf_size, &mut buf) } != SUCCESS {
                return Err("memory-map buffer allocation failed");
            }
            continue;
        }
        if status != SUCCESS {
            return Err("GetMemoryMap failed");
        }

        if !claimed && !footprint_reclaimable(buf, size, desc_size, footprint) {
            dump_overlaps(buf, size, desc_size, footprint);
            return Err("kernel range holds firmware-owned memory");
        }

        if unsafe { (bs.exit_boot_services)(image, key) } == SUCCESS {
            return Ok(());
        }
        // Stale key (something changed the map): loop and refetch.
    }
    Err("ExitBootServices kept failing")
}

/// True when every byte of `range` is covered by memory-map descriptors whose
/// type the kernel may overwrite once boot services are gone.
fn footprint_reclaimable(map: *const u8, map_size: usize, desc_size: usize, range: (u64, u64)) -> bool {
    let mut covered: u64 = 0;
    let mut off = 0usize;
    while off + desc_size <= map_size {
        let d = unsafe { &*(map.add(off) as *const MemoryDescriptor) };
        off += desc_size;
        let start = d.physical_start.max(range.0);
        let end = (d.physical_start + d.number_of_pages * PAGE).min(range.1);
        if start >= end {
            continue;
        }
        match d.type_ {
            MEM_LOADER_DATA | MEM_BOOT_SERVICES_CODE | MEM_BOOT_SERVICES_DATA
            | MEM_CONVENTIONAL => covered += end - start,
            // LoaderCode is this image; everything else (ACPI, runtime
            // services, MMIO, reserved, unusable) must survive the kernel.
            _ => return false,
        }
    }
    covered == range.1 - range.0
}

/// Diagnostic: list every memory-map descriptor overlapping `range` (printed
/// before giving up, while console output still works).
fn dump_overlaps(map: *const u8, map_size: usize, desc_size: usize, range: (u64, u64)) {
    let mut off = 0usize;
    while off + desc_size <= map_size {
        let d = unsafe { &*(map.add(off) as *const MemoryDescriptor) };
        off += desc_size;
        let end = d.physical_start + d.number_of_pages * PAGE;
        if !overlaps((d.physical_start, end), range) {
            continue;
        }
        out("  type=");
        out_hex(d.type_ as u64);
        out(" at ");
        out_hex(d.physical_start);
        out("..");
        out_hex(end);
        out("\r\n");
    }
}

fn out_hex(v: u64) {
    let mut buf = [0u8; 18];
    let mut n = 0;
    let mut started = false;
    for i in (0..16).rev() {
        let d = ((v >> (i * 4)) & 0xF) as u8;
        if d != 0 || started || i == 0 {
            buf[n] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
            n += 1;
            started = true;
        }
    }
    out(core::str::from_utf8(&buf[..n]).unwrap_or("?"));
}

// ---- console -----------------------------------------------------------------

/// Print ASCII to the firmware console (pre-ExitBootServices only).
fn out(s: &str) {
    let con = CON_OUT.load(Ordering::Relaxed);
    if con.is_null() {
        return;
    }
    let mut buf = [0u16; 129];
    let mut n = 0;
    for &b in s.as_bytes() {
        buf[n] = b as u16;
        n += 1;
        if n == buf.len() - 1 {
            buf[n] = 0;
            unsafe { ((*con).output_string)(con, buf.as_ptr()) };
            n = 0;
        }
    }
    if n > 0 {
        buf[n] = 0;
        unsafe { ((*con).output_string)(con, buf.as_ptr()) };
    }
}

// ---- little-endian field access -------------------------------------------------

fn rd16(s: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([s[off], s[off + 1]])
}
fn rd32(s: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(s[off..off + 4].try_into().unwrap())
}
fn rd64(s: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(s[off..off + 8].try_into().unwrap())
}
unsafe fn wr32(p: *mut u8, off: usize, v: u32) {
    ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), p.add(off), 4);
}
unsafe fn wr64(p: *mut u8, off: usize, v: u64) {
    ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), p.add(off), 8);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    out("UEFI loader panic\r\n");
    loop {
        core::hint::spin_loop();
    }
}
