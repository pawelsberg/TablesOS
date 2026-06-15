//! The minimal hand-written UEFI FFI surface the loader needs: system table,
//! boot services, text output, Graphics Output Protocol, Block I/O and Loaded
//! Image. Layouts follow the UEFI 2.x spec; unused function-pointer slots are
//! `usize` placeholders so the structs stay ABI-correct without dragging in
//! every signature.

#![allow(dead_code)]

use core::ffi::c_void;

pub type Status = usize;
pub type Handle = *mut c_void;

pub const SUCCESS: Status = 0;
/// High bit set = error. `EFI_INVALID_PARAMETER` is error code 2.
pub const ERR_BIT: Status = 1 << (usize::BITS - 1);
pub const INVALID_PARAMETER: Status = ERR_BIT | 2;
pub const BUFFER_TOO_SMALL: Status = ERR_BIT | 5;
/// `EFI_NOT_READY` — ReadKeyStroke returns this when no key is buffered.
pub const NOT_READY: Status = ERR_BIT | 6;
/// `EFI_SCAN_CODE` for the Escape key (UEFI 2.x simple text input).
pub const SCAN_ESC: u16 = 0x17;
/// Carriage return in `unicode_char` (the Enter key).
pub const CHAR_CR: u16 = 0x0D;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub const GOP_GUID: Guid = Guid {
    data1: 0x9042_a9de,
    data2: 0x23dc,
    data3: 0x4a38,
    data4: [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a],
};
pub const BLOCK_IO_GUID: Guid = Guid {
    data1: 0x964e_5b21,
    data2: 0x6459,
    data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};
pub const LOADED_IMAGE_GUID: Guid = Guid {
    data1: 0x5b1b_31a1,
    data2: 0x9562,
    data3: 0x11d2,
    data4: [0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};
/// ACPI 2.0+ RSDP in the configuration table.
pub const ACPI20_TABLE_GUID: Guid = Guid {
    data1: 0x8868_e871,
    data2: 0xe4f1,
    data3: 0x11d3,
    data4: [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81],
};
/// ACPI 1.0 RSDP (fallback when no 2.0 entry exists).
pub const ACPI10_TABLE_GUID: Guid = Guid {
    data1: 0xeb9d_2d30,
    data2: 0x2d88,
    data3: 0x11d3,
    data4: [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d],
};

#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *mut c_void,
}

#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut SimpleTextInput,
    pub console_out_handle: Handle,
    pub con_out: *mut SimpleTextOutput,
    pub standard_error_handle: Handle,
    pub std_err: *mut SimpleTextOutput,
    pub runtime_services: *mut c_void,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *mut ConfigurationTable,
}

#[repr(C)]
pub struct SimpleTextOutput {
    pub reset: usize,
    pub output_string:
        unsafe extern "efiapi" fn(this: *mut SimpleTextOutput, string: *const u16) -> Status,
    // (test_string, query_mode, set_mode, set_attribute, clear_screen,
    //  set_cursor_position, enable_cursor, mode — unused)
}

/// `EFI_INPUT_KEY` — one keystroke from the simple text input protocol.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputKey {
    pub scan_code: u16,
    pub unicode_char: u16,
}

#[repr(C)]
pub struct SimpleTextInput {
    pub reset: unsafe extern "efiapi" fn(this: *mut SimpleTextInput, extended: bool) -> Status,
    pub read_key_stroke:
        unsafe extern "efiapi" fn(this: *mut SimpleTextInput, key: *mut InputKey) -> Status,
    pub wait_for_key: *mut c_void,
}

/// `EFI_ALLOCATE_TYPE`
pub const ALLOCATE_ANY_PAGES: u32 = 0;
pub const ALLOCATE_MAX_ADDRESS: u32 = 1;
pub const ALLOCATE_ADDRESS: u32 = 2;
/// `EFI_MEMORY_TYPE::EfiLoaderData`
pub const LOADER_DATA: u32 = 2;
/// `EFI_LOCATE_SEARCH_TYPE::ByProtocol`
pub const BY_PROTOCOL: u32 = 2;

// EFI_MEMORY_TYPE values the post-ExitBootServices kernel region may reuse.
pub const MEM_LOADER_CODE: u32 = 1;
pub const MEM_LOADER_DATA: u32 = 2;
pub const MEM_BOOT_SERVICES_CODE: u32 = 3;
pub const MEM_BOOT_SERVICES_DATA: u32 = 4;
pub const MEM_CONVENTIONAL: u32 = 7;

#[repr(C)]
pub struct MemoryDescriptor {
    pub type_: u32,
    // 4 bytes implicit padding (u64 alignment)
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader,
    pub raise_tpl: usize,
    pub restore_tpl: usize,
    pub allocate_pages: unsafe extern "efiapi" fn(
        alloc_type: u32,
        memory_type: u32,
        pages: usize,
        memory: *mut u64,
    ) -> Status,
    pub free_pages: unsafe extern "efiapi" fn(memory: u64, pages: usize) -> Status,
    pub get_memory_map: unsafe extern "efiapi" fn(
        memory_map_size: *mut usize,
        memory_map: *mut u8,
        map_key: *mut usize,
        descriptor_size: *mut usize,
        descriptor_version: *mut u32,
    ) -> Status,
    pub allocate_pool:
        unsafe extern "efiapi" fn(pool_type: u32, size: usize, buffer: *mut *mut u8) -> Status,
    pub free_pool: unsafe extern "efiapi" fn(buffer: *mut u8) -> Status,
    pub create_event: usize,
    pub set_timer: usize,
    pub wait_for_event: usize,
    pub signal_event: usize,
    pub close_event: usize,
    pub check_event: usize,
    pub install_protocol_interface: usize,
    pub reinstall_protocol_interface: usize,
    pub uninstall_protocol_interface: usize,
    pub handle_protocol: unsafe extern "efiapi" fn(
        handle: Handle,
        protocol: *const Guid,
        interface: *mut *mut c_void,
    ) -> Status,
    pub reserved: usize,
    pub register_protocol_notify: usize,
    pub locate_handle: usize,
    pub locate_device_path: usize,
    pub install_configuration_table: usize,
    pub load_image: usize,
    pub start_image: usize,
    pub exit: usize,
    pub unload_image: usize,
    pub exit_boot_services:
        unsafe extern "efiapi" fn(image_handle: Handle, map_key: usize) -> Status,
    pub get_next_monotonic_count: usize,
    pub stall: unsafe extern "efiapi" fn(microseconds: usize) -> Status,
    pub set_watchdog_timer: unsafe extern "efiapi" fn(
        timeout: usize,
        watchdog_code: u64,
        data_size: usize,
        watchdog_data: *mut u16,
    ) -> Status,
    pub connect_controller: usize,
    pub disconnect_controller: usize,
    pub open_protocol: usize,
    pub close_protocol: usize,
    pub open_protocol_information: usize,
    pub protocols_per_handle: usize,
    pub locate_handle_buffer: unsafe extern "efiapi" fn(
        search_type: u32,
        protocol: *const Guid,
        search_key: *mut c_void,
        no_handles: *mut usize,
        buffer: *mut *mut Handle,
    ) -> Status,
    pub locate_protocol: unsafe extern "efiapi" fn(
        protocol: *const Guid,
        registration: *mut c_void,
        interface: *mut *mut c_void,
    ) -> Status,
    // (install/uninstall_multiple_protocol_interfaces, calculate_crc32,
    //  copy_mem, set_mem, create_event_ex — unused)
}

// ---- Graphics Output Protocol ---------------------------------------------

/// `EFI_GRAPHICS_PIXEL_FORMAT`: byte order in memory.
pub const PIXEL_RGB_RESERVED_8BPC: u32 = 0; // R,G,B,reserved
pub const PIXEL_BGR_RESERVED_8BPC: u32 = 1; // B,G,R,reserved

#[repr(C)]
pub struct GopModeInfo {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    pub pixel_format: u32,
    pub pixel_bitmask: [u32; 4],
    pub pixels_per_scan_line: u32,
}

#[repr(C)]
pub struct GopMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *mut GopModeInfo,
    pub size_of_info: usize,
    pub frame_buffer_base: u64,
    pub frame_buffer_size: usize,
}

#[repr(C)]
pub struct Gop {
    pub query_mode: unsafe extern "efiapi" fn(
        this: *mut Gop,
        mode_number: u32,
        size_of_info: *mut usize,
        info: *mut *mut GopModeInfo,
    ) -> Status,
    pub set_mode: unsafe extern "efiapi" fn(this: *mut Gop, mode_number: u32) -> Status,
    pub blt: usize,
    pub mode: *mut GopMode,
}

// ---- Block I/O Protocol ----------------------------------------------------

#[repr(C)]
pub struct BlockIoMedia {
    pub media_id: u32,
    pub removable_media: u8,
    pub media_present: u8,
    pub logical_partition: u8,
    pub read_only: u8,
    pub write_caching: u8,
    // 3 bytes implicit padding (u32 alignment)
    pub block_size: u32,
    pub io_align: u32,
    // 4 bytes implicit padding (u64 alignment)
    pub last_block: u64,
    // (revision 2/3 fields follow — unused)
}

#[repr(C)]
pub struct BlockIo {
    pub revision: u64,
    pub media: *mut BlockIoMedia,
    pub reset: usize,
    pub read_blocks: unsafe extern "efiapi" fn(
        this: *mut BlockIo,
        media_id: u32,
        lba: u64,
        buffer_size: usize,
        buffer: *mut u8,
    ) -> Status,
    pub write_blocks: usize,
    pub flush_blocks: usize,
}

// ---- Loaded Image Protocol -------------------------------------------------

#[repr(C)]
pub struct LoadedImage {
    pub revision: u32,
    pub parent_handle: Handle,
    pub system_table: *mut c_void,
    pub device_handle: Handle,
    pub file_path: *mut c_void,
    pub reserved: *mut c_void,
    pub load_options_size: u32,
    pub load_options: *mut c_void,
    pub image_base: *mut c_void,
    pub image_size: u64,
    pub image_code_type: u32,
    pub image_data_type: u32,
    pub unload: usize,
}
