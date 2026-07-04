//! xHCI host-controller driver — phase 4 (partial: read-only inspection).
//!
//! This pass establishes MMIO access and verifies register-layout
//! understanding by reading the capability, operational and per-port
//! registers and exposing them in the UI. No writable state is touched:
//! no reset, no rings, no doorbells. That's the next pass.
//!
//! Register layout follows the xHCI spec, section 5 "xHCI Register
//! Interface". Capability registers live at `BAR0 + 0`; operational
//! registers at `BAR0 + CAPLENGTH`; per-port register blocks at
//! `op_base + 0x400 + (port - 1) * 0x10`; runtime registers at
//! `BAR0 + RTSOFF`; doorbell array at `BAR0 + DBOFF`.

use alloc::vec::Vec;
use core::alloc::Layout;
use core::fmt::Write as _;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spin::Mutex;

use crate::ata::{self, DriveInfo, MbrInfo};
use crate::pci::{self, PciDevice};
use crate::serial_println;
use crate::time;

/// 4 GiB ceiling: the kernel's bootloader identity-maps 0..4 GiB; a BAR
/// higher than that can't be dereferenced without extending page tables,
/// which is a separate piece of work.
const MMIO_MAX: u64 = 1u64 << 32;

#[derive(Clone)]
pub struct XhciInfo {
    pub mmio_base: u64,
    pub mmio_accessible: bool,
    pub cap_length: u8,
    pub hci_version: u16,
    pub max_slots: u8,
    pub max_intrs: u16,
    pub max_ports: u8,
    pub ist: u8,
    pub erst_max_pow2: u8,
    pub max_scratchpad: u16,
    pub ac64: bool,
    pub csz_64: bool,
    pub xecp_dword: u32,
    pub dboff: u32,
    pub rtsoff: u32,
    pub page_size_bits: u32,
    pub usbcmd: u32,
    pub usbsts: u32,
    pub crcr: u64,
    pub dcbaap: u64,
    pub config: u32,
    pub mfindex: u32,
    pub ports: Vec<PortInfo>,
    /// Snapshot of the PCI command register before we enabled MMIO + bus
    /// master; useful as a sanity-check / "what did the BIOS leave us".
    pub pci_command_before: u16,
}

#[derive(Clone, Copy)]
pub struct PortInfo {
    pub index: u8, // 1-based, matches xHCI spec terminology
    pub portsc: u32,
    pub ccs: bool, // current connect status
    pub ped: bool, // port enabled/disabled
    pub oca: bool, // overcurrent active
    pub pr: bool,  // port reset
    pub pls: u8,   // port link state
    pub pp: bool,  // port power
    pub speed: u8, // 1=full, 2=low, 3=high, 4=super, 5=super+
}

/// Read the controller's static + current state. Read-only; safe to call
/// any number of times. Returns `None` if the BAR is unassigned, lives
/// above 4 GiB (would need extended paging), or the device isn't actually
/// xHCI (class 0x0C / subclass 0x03 / prog-if 0x30).
pub fn inspect(dev: &PciDevice) -> Option<XhciInfo> {
    if !(dev.class == 0x0C && dev.subclass == 0x03 && dev.prog_if == 0x30) {
        return None;
    }
    let mut mmio_base = pci::bar_address(dev, 0);
    if mmio_base == 0 {
        return None;
    }
    // UEFI firmware may park the 64-bit BAR above 4 GiB, beyond the kernel's
    // identity map. The kernel owns the machine, so re-home the BAR into the
    // 32-bit MMIO window instead of giving up on the controller.
    if mmio_base >= MMIO_MAX {
        if let Some(new_base) = pci::relocate_bar0_below_4g(dev) {
            serial_println!(
                "xhci: MMIO BAR relocated {:#x} -> {:#x} (was above 4 GiB)",
                mmio_base,
                new_base
            );
            mmio_base = new_base;
        }
    }
    // Enable Memory Space + Bus Master before touching MMIO. Save the
    // previous command so we can show it in the UI.
    let pci_command_before = pci::enable_mmio_and_bus_master(dev);
    let mmio_accessible = mmio_base < MMIO_MAX;
    if !mmio_accessible {
        return Some(XhciInfo {
            mmio_base,
            mmio_accessible: false,
            pci_command_before,
            ..empty_info(mmio_base, pci_command_before)
        });
    }

    let cap = mmio_base as *const u8;
    // Capability registers.
    let cap_length = unsafe { read_volatile(cap) };
    let hci_version = unsafe { read_volatile(cap.add(0x02) as *const u16) };
    let hcs1 = unsafe { read_volatile(cap.add(0x04) as *const u32) };
    let max_slots = hcs1 as u8;
    let max_intrs = ((hcs1 >> 8) & 0x7FF) as u16;
    let max_ports = (hcs1 >> 24) as u8;
    let hcs2 = unsafe { read_volatile(cap.add(0x08) as *const u32) };
    let ist = (hcs2 & 0xF) as u8;
    let erst_max_pow2 = ((hcs2 >> 4) & 0xF) as u8;
    // Max Scratchpad Bufs Hi (bits 25:21) << 5 | Lo (bits 31:27).
    let sp_hi = (hcs2 >> 21) & 0x1F;
    let sp_lo = (hcs2 >> 27) & 0x1F;
    let max_scratchpad = ((sp_hi << 5) | sp_lo) as u16;
    let hccp1 = unsafe { read_volatile(cap.add(0x10) as *const u32) };
    let ac64 = hccp1 & 0x01 != 0;
    let csz_64 = hccp1 & 0x04 != 0;
    let xecp_dword = (hccp1 >> 16) & 0xFFFF;
    let dboff = unsafe { read_volatile(cap.add(0x14) as *const u32) } & !0x3;
    let rtsoff = unsafe { read_volatile(cap.add(0x18) as *const u32) } & !0x1F;

    // Operational registers.
    let op = unsafe { cap.add(cap_length as usize) };
    let usbcmd = unsafe { read_volatile(op as *const u32) };
    let usbsts = unsafe { read_volatile(op.add(0x04) as *const u32) };
    let page_size_bits = unsafe { read_volatile(op.add(0x08) as *const u32) };
    let crcr = unsafe { read_volatile(op.add(0x18) as *const u64) };
    let dcbaap = unsafe { read_volatile(op.add(0x30) as *const u64) };
    let config = unsafe { read_volatile(op.add(0x38) as *const u32) };

    // Runtime: MFINDEX (microframe counter, ticks at 125 µs intervals
    // when the controller is running — interesting as a liveness signal).
    let runtime = unsafe { cap.add(rtsoff as usize) };
    let mfindex = unsafe { read_volatile(runtime as *const u32) };

    // Per-port PORTSC.
    let mut ports = Vec::with_capacity(max_ports as usize);
    for i in 0..max_ports {
        let port_base = unsafe { op.add(0x400 + (i as usize) * 0x10) };
        let portsc = unsafe { read_volatile(port_base as *const u32) };
        ports.push(PortInfo {
            index: i + 1,
            portsc,
            ccs: portsc & 0x0001 != 0,
            ped: portsc & 0x0002 != 0,
            oca: portsc & 0x0008 != 0,
            pr: portsc & 0x0010 != 0,
            pls: ((portsc >> 5) & 0xF) as u8,
            pp: portsc & 0x0200 != 0,
            speed: ((portsc >> 10) & 0xF) as u8,
        });
    }

    Some(XhciInfo {
        mmio_base,
        mmio_accessible: true,
        cap_length,
        hci_version,
        max_slots,
        max_intrs,
        max_ports,
        ist,
        erst_max_pow2,
        max_scratchpad,
        ac64,
        csz_64,
        xecp_dword,
        dboff,
        rtsoff,
        page_size_bits,
        usbcmd,
        usbsts,
        crcr,
        dcbaap,
        config,
        mfindex,
        ports,
        pci_command_before,
    })
}

fn empty_info(mmio_base: u64, pci_command_before: u16) -> XhciInfo {
    XhciInfo {
        mmio_base,
        mmio_accessible: false,
        cap_length: 0,
        hci_version: 0,
        max_slots: 0,
        max_intrs: 0,
        max_ports: 0,
        ist: 0,
        erst_max_pow2: 0,
        max_scratchpad: 0,
        ac64: false,
        csz_64: false,
        xecp_dword: 0,
        dboff: 0,
        rtsoff: 0,
        page_size_bits: 0,
        usbcmd: 0,
        usbsts: 0,
        crcr: 0,
        dcbaap: 0,
        config: 0,
        mfindex: 0,
        ports: Vec::new(),
        pci_command_before,
    }
}

pub fn pls_name(pls: u8) -> &'static str {
    match pls {
        0 => "U0 (running)",
        1 => "U1 (idle, low-power)",
        2 => "U2 (idle, deeper)",
        3 => "U3 (suspended)",
        4 => "Disabled",
        5 => "RxDetect",
        6 => "Inactive",
        7 => "Polling",
        8 => "Recovery",
        9 => "Hot Reset",
        10 => "Compliance Mode",
        11 => "Test Mode",
        15 => "Resume",
        _ => "(reserved)",
    }
}

pub fn speed_name(speed: u8) -> &'static str {
    match speed {
        0 => "(undefined)",
        1 => "Full (USB 1.1, 12 Mbit/s)",
        2 => "Low (USB 1.0, 1.5 Mbit/s)",
        3 => "High (USB 2.0, 480 Mbit/s)",
        4 => "Super (USB 3.0, 5 Gbit/s)",
        5 => "SuperPlus (USB 3.1, 10 Gbit/s)",
        _ => "(reserved)",
    }
}

/// Format the HCIVERSION BCD as "M.mm" (e.g. 0x0110 → "1.10").
pub fn version_string(v: u16) -> alloc::string::String {
    alloc::format!("{}.{:02X}", v >> 8, v & 0xFF)
}

// =====================================================================
// Phase 4 sub-pass 2: halt → reset → minimal configuration → R/S = 1.
//
// After this runs, the controller is *running*: MFINDEX is incrementing,
// the command ring is wired to CRCR, the event ring is wired to
// interrupter 0, the device-context base-address array exists, and the
// scratchpad pages (if the controller wanted any) are allocated. No
// commands have been issued yet and no ports have been reset — that's
// the next sub-pass.
// =====================================================================

/// Information returned by a successful bring-up. All addresses are
/// physical (which equals virtual on this kernel because BSS/heap is in
/// the identity-mapped 0–4 GiB range).
#[derive(Clone)]
pub struct BringUpInfo {
    pub mmio_base: u64,
    pub max_slots_en: u8,
    pub page_size_bytes: u32,
    pub dcbaa_addr: u64,
    pub scratchpad_arr_addr: u64,
    pub scratchpad_count: u16,
    pub cmd_ring_addr: u64,
    pub event_ring_addr: u64,
    pub erst_addr: u64,
}

/// Producer/consumer ring positions and the most recent command-completion
/// outcome. Lives alongside the `BringUpInfo` in the global state so we
/// can issue more commands after the initial bring-up without re-touching
/// the controller.
#[derive(Clone)]
struct RingPositions {
    cmd_enqueue: usize, // next free slot on the command ring (0..255)
    cmd_pcs: u8,        // current producer cycle state (0 or 1)
    event_dequeue: usize,
    event_ccs: u8, // current consumer cycle state
}

#[derive(Clone)]
struct XhciState {
    /// Cached at first bring-up so post-bring-up I/O (bulk transfers,
    /// control transfers issued by the `BlockDevice` wrapper) can find
    /// `mmio_base / cap_length / dboff / rtsoff / csz_64` without
    /// rescanning PCI.
    info: XhciInfo,
    bringup: BringUpInfo,
    rings: RingPositions,
    enumeration: Option<EnumResult>,
    /// Per-addressed-slot resources, kept around for follow-up control
    /// transfers (Get Configuration Descriptor, SET_CONFIGURATION, etc.).
    /// Indexed in `addressed` order; not exposed to the UI.
    slots: Vec<SlotResources>,
    addressed: Vec<AddressedDevice>,
    /// Most recent USB-MSC probe results.
    msc: Vec<MscProbe>,
    /// **Every** bound USB-HID boot mouse interface — polled cooperatively
    /// from the UI loop. Plural because a machine can have several composite
    /// HID devices (e.g. a keyboard and a separate wireless-mouse receiver
    /// each exposing both a keyboard and a mouse interface); we can't tell
    /// from descriptors which mouse interface has a physical mouse, so we
    /// poll them all — the idle ones simply never report.
    mice: Vec<MouseDevice>,
    /// Likewise every bound boot keyboard interface. Transfer events are
    /// dispatched to the right device by (slot, endpoint DCI).
    keyboards: Vec<KeyboardDevice>,
}

/// Per-slot allocations needed for Address Device and subsequent control
/// + bulk + interrupt transfers. Addresses are physical (identity-mapped
/// on this kernel).
#[allow(dead_code)]
#[derive(Clone)]
struct SlotResources {
    slot_id: u8,
    port: u8,
    speed: u8,
    max_packet_size_ep0: u16,
    /// Device Context — array of 32 entries pointed to by `DCBAA[slot_id]`.
    device_ctx: u64,
    /// Input Context — 33 entries used to feed Address-Device /
    /// Configure-Endpoint commands.
    input_ctx: u64,
    /// Default Control endpoint's transfer ring (256 × 16-byte TRBs,
    /// Link TRB at the end like the command ring).
    tr_ring: u64,
    tr_enqueue: usize,
    tr_pcs: u8,
    /// Non-control endpoints activated by Configure Endpoint. Each has
    /// its own transfer ring driven independently.
    endpoints: Vec<EndpointState>,
}

#[allow(dead_code)]
#[derive(Clone)]
struct EndpointState {
    /// USB endpoint address: bit 7 = direction (1 = IN), bits 3..0 = number.
    address: u8,
    /// Device Context Index (`DCI = 2 * ep_num + (dir_in ? 1 : 0)`).
    dci: u8,
    /// 0=Control, 1=Isoch, 2=Bulk, 3=Interrupt (USB classification).
    transfer_type: u8,
    max_packet_size: u16,
    direction_in: bool,
    /// Owning interface's `bInterfaceNumber` — so a composite device's
    /// endpoints can be matched to the right interface (e.g. binding the
    /// *mouse* interface's interrupt-IN endpoint, not the keyboard's).
    interface_number: u8,
    tr_ring: u64,
    tr_enqueue: usize,
    tr_pcs: u8,
}

/// A bound USB-HID boot-protocol mouse. We keep exactly one interrupt-IN
/// transfer TRB armed at all times; each completion accumulates a relative
/// delta + button state here (so no motion is lost between UI polls) and the
/// poll re-arms. The pointer position itself lives in `ps2` so USB and PS/2
/// share one cursor.
#[derive(Clone)]
struct MouseDevice {
    /// Index into `slots`.
    slot_idx: usize,
    /// Index into `slots[slot_idx].endpoints` of the interrupt-IN endpoint.
    ep_idx: usize,
    slot_id: u8,
    /// Endpoint DCI — distinguishes this endpoint's transfer events from a
    /// keyboard sharing the same (composite) slot.
    dci: u8,
    /// Leaked, identity-mapped DMA buffer the controller writes each report into.
    report_buf: u64,
    /// Bytes requested per transfer (endpoint max packet, clamped to 4..=8).
    report_len: u32,
    /// True while a TRB is queued (guards against arming two at once).
    armed: bool,
    /// Set when a completion came back halted; the next poll resets the endpoint
    /// (done there, not inside an event drain, to avoid re-entrant command waits).
    needs_reset: bool,
    /// Accumulated movement + latest button bitmap since the last UI poll.
    accum_dx: i32,
    accum_dy: i32,
    buttons: u8,
    dirty: bool,
}

/// A bound USB-HID boot-protocol keyboard. Mirrors [`MouseDevice`]: one
/// interrupt-IN TRB kept armed; each completed 8-byte boot report is decoded
/// against the previous one (edge detection) and the new keys fed straight
/// into the shared input queue via `ps2::feed_key`.
#[derive(Clone)]
struct KeyboardDevice {
    slot_idx: usize,
    ep_idx: usize,
    slot_id: u8,
    /// Endpoint DCI — matches the Transfer Event's endpoint id so we can tell
    /// keyboard events from mouse events on a shared (composite) slot.
    dci: u8,
    report_buf: u64,
    report_len: u32,
    armed: bool,
    needs_reset: bool,
    /// Previous boot report (modifiers + 6 key usages) for edge detection.
    last: [u8; 8],
}

#[derive(Clone)]
pub struct AddressedDevice {
    pub slot_id: u8,
    pub port: u8,
    pub speed: u8,
    pub addr_completion_code: u8,
    pub descriptor_completion_code: u8,
    pub descriptor: Option<DeviceDescriptor>,
    pub raw_descriptor: Vec<u8>,
    /// `Some(cc)` if Evaluate Context was issued (LS/FS bMaxPacketSize0
    /// turned out different from our initial guess); `None` if no
    /// correction was needed.
    pub eval_context_cc: Option<u8>,
    pub config_completion_code: u8,
    pub config: Option<Configuration>,
    pub raw_config: Vec<u8>,
    /// Set by `configure_endpoints`. 0 = never attempted, 1 = Success.
    pub configure_endpoint_cc: u8,
    /// SET_CONFIGURATION completion. 0 = never attempted, 1 = Success.
    pub set_config_cc: u8,
    /// Per-endpoint summary for the UI (one row per non-control endpoint
    /// that was wired up).
    pub configured_endpoints: Vec<ConfiguredEndpoint>,
}

#[derive(Clone, Copy)]
pub struct ConfiguredEndpoint {
    pub address: u8,
    pub dci: u8,
    pub direction_in: bool,
    pub transfer_type: u8,
    pub max_packet_size: u16,
}

#[derive(Clone)]
pub struct Configuration {
    pub total_length: u16,
    pub num_interfaces: u8,
    pub config_value: u8,
    pub attributes: u8,
    pub max_power_ma: u16,
    pub interfaces: Vec<Interface>,
}

#[derive(Clone)]
pub struct Interface {
    pub number: u8,
    pub alt_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<Endpoint>,
}

/// `number` is set but currently only displayed indirectly via `address`;
/// it'll be used by phase 4 sub-pass 7 to compute the Device Context Index
/// (`DCI = 2 * number + (direction_in ? 1 : 0)`) when issuing Configure
/// Endpoint.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub struct Endpoint {
    /// Raw `bEndpointAddress`: bit 7 = direction (1=IN), bits 3..0 = number.
    pub address: u8,
    pub direction_in: bool,
    pub number: u8,
    /// 0=Control, 1=Isoch, 2=Bulk, 3=Interrupt.
    pub transfer_type: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

/// Standard USB 2.0 / 3.x Device Descriptor (18 bytes, type 1).
#[derive(Clone, Copy)]
pub struct DeviceDescriptor {
    pub usb_bcd: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size_ep0: u8,
    pub id_vendor: u16,
    pub id_product: u16,
    pub device_bcd: u16,
    pub manufacturer_idx: u8,
    pub product_idx: u8,
    pub serial_idx: u8,
    pub num_configurations: u8,
}

/// Per-controller one-shots, keyed by each controller's MMIO base. Bring-up
/// is non-idempotent against the hardware (writes USBCMD, allocates leaked
/// DMA buffers) so each controller is brought up once and its state cached.
/// Real machines have several xHCI controllers (a PCH one carrying the
/// physical ports plus e.g. a Thunderbolt one) — the previous single global
/// state made the second controller silently reuse the first one's rings, so
/// its Enable Slot commands never completed and its devices never appeared.
static STATES: Mutex<Vec<XhciState>> = Mutex::new(Vec::new());

/// The cached state for one controller.
fn state_for(states: &mut Vec<XhciState>, mmio_base: u64) -> Option<&mut XhciState> {
    states.iter_mut().find(|s| s.info.mmio_base == mmio_base)
}

/// The cached state holding a given addressed slot — the lookup for the
/// slot-keyed I/O paths (`UsbMscDevice`, `msc_*`). First match wins: slot ids
/// are per-controller, but only controllers that addressed devices can match,
/// and every write is still behind the GUID identity gate.
fn state_for_slot(states: &mut Vec<XhciState>, slot_id: u8) -> Option<&mut XhciState> {
    states
        .iter_mut()
        .find(|s| s.addressed.iter().any(|a| a.slot_id == slot_id))
}

/// Diagnostics of the first brought-up controller (shown on the xHCI screen).
pub fn current_bringup() -> Option<BringUpInfo> {
    STATES.lock().first().map(|s| s.bringup.clone())
}

pub fn current_enumeration() -> Option<EnumResult> {
    STATES.lock().first().and_then(|s| s.enumeration.clone())
}

/// Halt → reset → allocate DCBAA/scratchpad/command-ring/event-ring →
/// R/S = 1. Idempotent: if it has already been run, returns the cached
/// `BringUpInfo` and touches no hardware. Errors leave the controller
/// halted and harmless.
/// BIOS->OS ownership handoff (xHCI USB Legacy Support Capability, ID 1). On
/// real hardware the firmware owns the controller — it just used it to boot the
/// USB stick — and starting it without claiming ownership wedges, because the
/// BIOS SMI handler still has it. `xecp` is the extended-capability pointer (in
/// dwords, from HCCPARAMS1). Walk the list for cap ID 1, set HC OS Owned, wait
/// for HC BIOS Owned to clear, then disable the controller's BIOS SMI sources.
/// QEMU's firmware never owns the controller, so this is a no-op there.
fn bios_handoff(mmio: u64, xecp: u32) {
    if xecp == 0 {
        crate::boot_status("bu: handoff: no xECP");
        return;
    }
    let mut cap_ptr = mmio + (xecp as u64) * 4;
    // Bounded walk so a malformed list can't loop forever.
    for _ in 0..64 {
        let cap = unsafe { read_volatile(cap_ptr as *const u32) };
        let id = cap & 0xFF;
        if id == 0 {
            break;
        }
        if id == 1 {
            // USBLEGSUP: bit 16 = HC BIOS Owned, bit 24 = HC OS Owned.
            unsafe { write_volatile(cap_ptr as *mut u32, cap | (1 << 24)) };
            let released = poll_until(cap_ptr as *const u32, 1 << 16, 0, 1_000_000);
            // USBLEGCTLSTS (cap+4): clear the SMI *enable* bits (low word) so the
            // BIOS stops trapping; the high-word status bits are RW1C and written
            // back as read, which clears any pending ones.
            let ctl = cap_ptr + 4;
            let v = unsafe { read_volatile(ctl as *const u32) };
            unsafe { write_volatile(ctl as *mut u32, v & 0xFFFF_0000) };
            crate::boot_status(if released {
                "bu: handoff OK"
            } else {
                "bu: handoff TIMEOUT (BIOS kept it)"
            });
            return;
        }
        let next = (cap >> 8) & 0xFF;
        if next == 0 {
            break;
        }
        cap_ptr += (next as u64) * 4;
    }
    crate::boot_status("bu: handoff: no legacy cap");
}

/// Bitmask (bit N = root port N, 1-based) of ports governed by a USB3
/// Supported Protocol Capability (xECP cap ID 2, major revision 0x03).
/// Warm Port Reset is only defined for these ports.
fn usb3_port_mask(info: &XhciInfo) -> u64 {
    if info.xecp_dword == 0 {
        return 0;
    }
    let mut mask = 0u64;
    let mut cap_ptr = info.mmio_base + (info.xecp_dword as u64) * 4;
    // Bounded walk so a malformed list can't loop forever.
    for _ in 0..64 {
        let cap = unsafe { read_volatile(cap_ptr as *const u32) };
        let id = cap & 0xFF;
        if id == 0 {
            break;
        }
        if id == 2 && (cap >> 24) as u8 == 0x03 {
            let d2 = unsafe { read_volatile((cap_ptr + 8) as *const u32) };
            let first = d2 & 0xFF; // compatible port offset, 1-based
            let count = (d2 >> 8) & 0xFF;
            for p in first..first + count {
                if p <= 63 {
                    mask |= 1 << p;
                }
            }
        }
        let next = (cap >> 8) & 0xFF;
        if next == 0 {
            break;
        }
        cap_ptr += (next as u64) * 4;
    }
    mask
}

pub fn bring_up(dev: &PciDevice, info: &XhciInfo) -> Result<BringUpInfo, &'static str> {
    let mut guard = STATES.lock();
    if let Some(existing) = state_for(&mut guard, info.mmio_base) {
        return Ok(existing.bringup.clone());
    }
    if !info.mmio_accessible {
        return Err("MMIO BAR above 4 GiB — extended paging not implemented");
    }
    pci::enable_mmio_and_bus_master(dev);

    let mmio = info.mmio_base;
    let op = (mmio + info.cap_length as u64) as *mut u8;
    let runtime = (mmio + info.rtsoff as u64) as *mut u8;

    serial_println!("xhci: bring-up starting (mmio=0x{:X})", mmio);
    crate::boot_status(&alloc::format!(
        "bu: {:04x}:{:04x} slots={} scratch={}",
        dev.vendor,
        dev.device,
        info.max_slots,
        info.max_scratchpad
    ));

    // Claim the controller from the BIOS before touching it (real-hardware
    // requirement; harmless under QEMU).
    bios_handoff(mmio, info.xecp_dword);

    // Older Intel PCHs mux the physical ports between EHCI and xHCI, and
    // firmware may have booted with them routed to EHCI — every xHCI port
    // then reads empty while the boot stick sits on a controller we don't
    // drive. Route all switchable ports here. No-op on non-Intel parts and
    // on machines already routed to xHCI.
    match pci::intel_route_usb_ports_to_xhci(dev) {
        Some((usb2, usb3)) => crate::boot_status(&alloc::format!(
            "bu: Intel port routing -> xHCI (usb2={:#x} usb3={:#x})",
            usb2, usb3
        )),
        None => crate::boot_status(if dev.vendor == 0x8086 {
            "bu: port routing: no switchable ports"
        } else {
            "bu: port routing: not an Intel PCH xHCI"
        }),
    }

    // ---- 1. Halt ---------------------------------------------------------
    crate::boot_status("bu: 1 halt");
    let usbcmd = unsafe { read_volatile(op as *const u32) };
    unsafe { write_volatile(op as *mut u32, usbcmd & !0x1) };
    if !poll_until(unsafe { op.add(0x04) } as *const u32, 0x1, 0x1, 100_000) {
        return Err("controller did not halt (USBSTS.HCH never set)");
    }
    serial_println!("xhci: halted");

    // ---- 2. Reset --------------------------------------------------------
    crate::boot_status("bu: 2 reset");
    unsafe { write_volatile(op as *mut u32, 0x2) }; // HCRST = 1
    if !poll_until(op as *const u32, 0x2, 0x0, 1_000_000) {
        return Err("HCRST did not self-clear");
    }
    if !poll_until(unsafe { op.add(0x04) } as *const u32, 0x800, 0x0, 1_000_000) {
        return Err("USBSTS.CNR stuck after reset");
    }
    serial_println!("xhci: reset complete (CNR cleared)");

    // ---- 3. Pick a page size ---------------------------------------------
    let page_size_bits = unsafe { read_volatile(op.add(0x08) as *const u32) };
    if page_size_bits == 0 {
        return Err("PAGESIZE register reports no supported page size");
    }
    let pg_bit = page_size_bits.trailing_zeros();
    let page_size: u32 = 1u32 << (pg_bit + 12);

    // ---- 4. CONFIG.MaxSlotsEn -------------------------------------------
    let max_slots_en = info.max_slots;
    unsafe { write_volatile(op.add(0x38) as *mut u32, max_slots_en as u32) };

    // ---- 5. DCBAA (Device Context Base Address Array) --------------------
    let dcbaa_size = (max_slots_en as usize + 1) * 8;
    let dcbaa = alloc_dma(dcbaa_size, 4096);

    // ---- 6. Scratchpad buffers (if the HC asked for any) ----------------
    let scratchpad_count = info.max_scratchpad;
    crate::boot_status(&alloc::format!(
        "bu: 3 scratchpad alloc n={} pgsz={}",
        scratchpad_count,
        page_size
    ));
    let mut scratchpad_arr_addr = 0u64;
    if scratchpad_count > 0 {
        let arr = alloc_dma(scratchpad_count as usize * 8, 4096);
        for i in 0..scratchpad_count as usize {
            let buf = alloc_dma(page_size as usize, page_size as usize);
            unsafe {
                write_volatile((arr as *mut u64).add(i), buf as u64);
            }
        }
        scratchpad_arr_addr = arr as u64;
        // DCBAA[0] is the scratchpad-buffer-array pointer.
        unsafe { write_volatile(dcbaa as *mut u64, arr as u64) };
        serial_println!(
            "xhci: scratchpad {} × {} bytes @ 0x{:X}",
            scratchpad_count,
            page_size,
            arr as u64
        );
    }

    // ---- 7. DCBAAP -------------------------------------------------------
    unsafe { write_volatile(op.add(0x30) as *mut u64, dcbaa as u64) };

    // ---- 8. Command ring (256 TRBs, last is a Link back to start) -------
    crate::boot_status("bu: 4 rings");
    const RING_TRBS: usize = 256;
    let cmd_ring = alloc_dma(RING_TRBS * 16, 4096);
    unsafe {
        let link = cmd_ring.add((RING_TRBS - 1) * 16);
        write_volatile(link as *mut u64, cmd_ring as u64);
        write_volatile(link.add(8) as *mut u32, 0);
        // TRB type 6 = Link, Toggle Cycle = 1, Cycle bit = 1.
        write_volatile(
            link.add(12) as *mut u32,
            (6u32 << 10) | (1 << 1) | 1,
        );
    }
    // CRCR: ring base | RCS = 1 (initial cycle state matches the TRBs).
    unsafe { write_volatile(op.add(0x18) as *mut u64, cmd_ring as u64 | 1) };

    // ---- 9. Event ring (one segment) + ERST + interrupter 0 -------------
    let event_ring = alloc_dma(RING_TRBS * 16, 4096);
    let erst = alloc_dma(16, 4096);
    unsafe {
        // Segment table entry 0: { base (u64), size (u32), reserved (u32) }.
        write_volatile(erst as *mut u64, event_ring as u64);
        write_volatile(erst.add(8) as *mut u32, RING_TRBS as u32);
        write_volatile(erst.add(12) as *mut u32, 0);
    }
    // Interrupter 0 at runtime + 0x20.
    let intr0 = unsafe { runtime.add(0x20) };
    unsafe {
        write_volatile(intr0.add(0x08) as *mut u32, 1); // ERSTSZ = 1 segment
        // ERDP first (xHCI requires ERDP set before ERSTBA to avoid spurious
        // event reports), then ERSTBA.
        write_volatile(intr0.add(0x18) as *mut u64, event_ring as u64);
        write_volatile(intr0.add(0x10) as *mut u64, erst as u64);
    }

    // ---- 10. Start -------------------------------------------------------
    crate::boot_status("bu: 5 start R/S");
    unsafe { write_volatile(op as *mut u32, 0x1) }; // R/S = 1
    crate::boot_status("bu: 5a polling HCH");
    let started =
        poll_until(unsafe { op.add(0x04) } as *const u32, 0x1, 0x0, 1_000_000);
    crate::boot_status(if started { "bu: 5b running" } else { "bu: 5b start TIMEOUT" });
    if !started {
        return Err("controller did not start (USBSTS.HCH stayed set)");
    }
    serial_println!(
        "xhci: running. DCBAAP=0x{:X} CRCR={:?} ERSTBA=0x{:X}",
        dcbaa as u64,
        cmd_ring as u64,
        erst as u64
    );

    let bringup = BringUpInfo {
        mmio_base: mmio,
        max_slots_en,
        page_size_bytes: page_size,
        dcbaa_addr: dcbaa as u64,
        scratchpad_arr_addr,
        scratchpad_count,
        cmd_ring_addr: cmd_ring as u64,
        event_ring_addr: event_ring as u64,
        erst_addr: erst as u64,
    };
    guard.push(XhciState {
        info: info.clone(),
        bringup: bringup.clone(),
        rings: RingPositions {
            cmd_enqueue: 0,
            cmd_pcs: 1,
            event_dequeue: 0,
            event_ccs: 1,
        },
        enumeration: None,
        slots: Vec::new(),
        addressed: Vec::new(),
        msc: Vec::new(),
        mice: Vec::new(),
        keyboards: Vec::new(),
    });
    Ok(bringup)
}

pub fn current_addressed() -> Vec<AddressedDevice> {
    STATES
        .lock()
        .iter()
        .flat_map(|s| s.addressed.iter().cloned())
        .collect()
}

/// Page-aligned (or stronger), zeroed DMA buffer. Identity-mapped so the
/// physical address equals the returned virtual pointer.
///
/// Use this for *permanent* allocations (rings, context arrays) — they
/// outlive every operation and leaking is the right thing because the
/// controller still owns those addresses.
///
/// For *transient* allocations (CBW/CSW/control-transfer data/SCSI data
/// buffers used for one round-trip and then dropped) use [`DmaBuffer`]
/// instead — it pairs the alloc with a `dealloc` on drop, which is what
/// keeps long write loops (~30 000 transient buffers in the
/// install-to-USB path) from exhausting the 32 MiB heap.
fn alloc_dma(size: usize, align: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, align).expect("bad DMA layout");
    let p = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if p.is_null() {
        panic!(
            "xhci: DMA alloc failed (size={} align={})",
            size, align
        );
    }
    p
}

/// Auto-freed DMA scratch. See `alloc_dma`'s doc comment for the
/// rationale on why this exists alongside the leaking helper.
struct DmaBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl DmaBuffer {
    fn new(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).expect("bad DMA layout");
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            panic!("xhci: DMA alloc failed (size={} align={})", size, align);
        }
        DmaBuffer { ptr, layout }
    }
    #[inline]
    fn ptr(&self) -> *mut u8 {
        self.ptr
    }
    #[inline]
    fn read_byte(&self, off: usize) -> u8 {
        unsafe { *self.ptr.add(off) }
    }
    #[inline]
    fn write_byte(&self, off: usize, v: u8) {
        unsafe { *self.ptr.add(off) = v };
    }
    fn write_slice(&self, off: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            unsafe { *self.ptr.add(off + i) = b };
        }
    }
    fn read_u32_le(&self, off: usize) -> u32 {
        let mut b = [0u8; 4];
        for (i, v) in b.iter_mut().enumerate() {
            *v = self.read_byte(off + i);
        }
        u32::from_le_bytes(b)
    }
    fn read_to_vec(&self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            out.push(self.read_byte(i));
        }
        out
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        unsafe { alloc::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// Spin-poll a 32-bit register until `(value & mask) == expect`, or
/// `max_us` microseconds elapse. Uses the calibrated TSC.
fn poll_until(reg: *const u32, mask: u32, expect: u32, max_us: u64) -> bool {
    let per = time::tsc_per_us().max(1);
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    loop {
        let v = unsafe { read_volatile(reg) };
        if v & mask == expect {
            return true;
        }
        let now = unsafe { core::arch::x86_64::_rdtsc() };
        if (now - start) / per > max_us {
            return false;
        }
        core::hint::spin_loop();
    }
}

// =====================================================================
// Phase 4 sub-pass 4: per-port reset and Enable-Slot command.
// First actual command round-trip with the controller — exercises the
// command ring (producer side), the doorbell, and the event ring
// (consumer side).
// =====================================================================

// PORTSC bit positions (xHCI spec 5.4.8).
const PORTSC_CCS: u32 = 1 << 0;  // current connect status, RO
const PORTSC_PED: u32 = 1 << 1;  // port enabled/disabled, RW1C
const PORTSC_PR: u32 = 1 << 4;   // port reset, RW1S
const PORTSC_PP: u32 = 1 << 9;   // port power, RW
const PORTSC_WPR: u32 = 1 << 31; // warm port reset (USB3 only), RW1S
const PORTSC_CSC: u32 = 1 << 17; // connect status change, RW1C
const PORTSC_PEC: u32 = 1 << 18; // port enabled change, RW1C
const PORTSC_WRC: u32 = 1 << 19; // warm reset change, RW1C
const PORTSC_OCC: u32 = 1 << 20; // overcurrent change, RW1C
const PORTSC_PRC: u32 = 1 << 21; // port reset change, RW1C
const PORTSC_PLC: u32 = 1 << 22; // port link state change, RW1C
const PORTSC_CEC: u32 = 1 << 23; // config error change, RW1C
const PORTSC_RW1C_MASK: u32 = PORTSC_CSC
    | PORTSC_PEC
    | PORTSC_WRC
    | PORTSC_OCC
    | PORTSC_PRC
    | PORTSC_PLC
    | PORTSC_CEC;
/// When writing PORTSC, mask out PED (writing 1 = disable port — never
/// what we want) and all RW1C bits (writing 1 = acknowledge change).
const PORTSC_PRESERVE: u32 = !(PORTSC_PED | PORTSC_RW1C_MASK);

#[derive(Clone)]
pub struct PortReport {
    pub port: u8,
    pub portsc_before: u32,
    pub portsc_after: u32,
    pub message: alloc::string::String,
}

#[derive(Clone)]
pub struct SlotReport {
    pub port: u8,
    pub speed: u8,
    pub completion_code: u8,
    pub slot_id: u8,
}

#[derive(Clone)]
pub struct EnumResult {
    pub ports: Vec<PortReport>,
    pub slots: Vec<SlotReport>,
}

/// Walk every port; for those reporting CCS=1 (a device is connected),
/// drive PORTSC.PR if needed, then issue Enable-Slot on the command ring
/// and consume the matching Command Completion Event from the event ring.
pub fn reset_and_enable_slots(
    dev: &PciDevice,
    info: &XhciInfo,
) -> Result<EnumResult, &'static str> {
    let _ = dev;
    let mut guard = STATES.lock();
    let st =
        state_for(&mut guard, info.mmio_base).ok_or("bring up the controller first ([b])")?;
    if !info.mmio_accessible {
        return Err("MMIO not accessible");
    }
    let mmio = info.mmio_base;
    let op = (mmio + info.cap_length as u64) as *mut u8;
    let portsc_at =
        |port: u8| unsafe { op.add(0x400 + (port as usize - 1) * 0x10) } as *mut u32;

    // ---- Real-hardware port discovery (all no-ops under QEMU) ------------
    // Right after the controller reset in `bring_up`, CCS=0 does not yet
    // mean "no device":
    //  - PPC=1 silicon comes out of HCRST with ports unpowered; nothing can
    //    ever connect until software sets PP (QEMU reports PP already 1).
    //  - Real devices need connect/debounce time; QEMU latches CCS instantly.
    //  - A SuperSpeed stick the UEFI firmware was just booting from often
    //    fails its post-reset link retrain (the device side is still in U0)
    //    and parks in Compliance Mode (PLS=10, CCS=0); per xHCI 4.19.1.2.2
    //    only a Warm Port Reset recovers it.
    for port in 1..=info.max_ports {
        let p = portsc_at(port);
        let v = unsafe { read_volatile(p) };
        if v & PORTSC_PP == 0 {
            unsafe { write_volatile(p, (v & PORTSC_PRESERVE) | PORTSC_PP) };
        }
    }
    // Wait (up to 1 s) for the first connect; costs nothing when a device is
    // already visible. A late first connect gets a short extra settle so the
    // remaining ports catch up too.
    let mut waited_ms = 0u32;
    loop {
        let any = (1..=info.max_ports)
            .any(|port| unsafe { read_volatile(portsc_at(port)) } & PORTSC_CCS != 0);
        if any || waited_ms >= 1_000 {
            break;
        }
        time::delay_ms(20);
        waited_ms += 20;
    }
    if waited_ms > 0 {
        time::delay_ms(150);
        crate::boot_status(&alloc::format!("xHCI: connect settled after {} ms", waited_ms));
    }
    // Warm-reset USB3 ports whose link is stuck with no connect — Compliance
    // Mode, or still Polling after the wait above.
    let usb3 = usb3_port_mask(info);
    for port in 1..=info.max_ports {
        if port > 63 || usb3 & (1u64 << port) == 0 {
            continue;
        }
        let p = portsc_at(port);
        let v = unsafe { read_volatile(p) };
        let pls = (v >> 5) & 0xF;
        if v & PORTSC_CCS != 0 || !(pls == 10 || pls == 7) {
            continue;
        }
        crate::boot_status(&alloc::format!(
            "xHCI: port {} stuck (pls={}) -> warm reset",
            port, pls
        ));
        unsafe { write_volatile(p, (v & PORTSC_PRESERVE) | PORTSC_WPR) };
        let _ = poll_until(p, PORTSC_WRC, PORTSC_WRC, 1_000_000);
        let v2 = unsafe { read_volatile(p) };
        unsafe {
            write_volatile(
                p,
                (v2 & PORTSC_PRESERVE) | (v2 & (PORTSC_WRC | PORTSC_PRC | PORTSC_CSC)),
            );
        }
        let _ = poll_until(p, PORTSC_CCS, PORTSC_CCS, 500_000);
    }

    let mut ports = Vec::new();
    let mut slots = Vec::new();
    for port in 1..=info.max_ports {
        let portsc_ptr = unsafe { op.add(0x400 + (port as usize - 1) * 0x10) } as *mut u32;
        let portsc_before = unsafe { read_volatile(portsc_ptr) };
        let mut msg = alloc::string::String::new();

        if portsc_before & PORTSC_CCS == 0 {
            ports.push(PortReport {
                port,
                portsc_before,
                portsc_after: portsc_before,
                message: "no device".into(),
            });
            continue;
        }

        if portsc_before & PORTSC_PED == 0 {
            // USB2-style port (and some USB3 corner cases): drive a real
            // reset. Clear any stale change bits first so PRC will be a
            // fresh edge we can wait on.
            let stale = portsc_before & PORTSC_RW1C_MASK;
            if stale != 0 {
                unsafe {
                    write_volatile(portsc_ptr, (portsc_before & PORTSC_PRESERVE) | stale);
                }
            }
            let portsc_now = unsafe { read_volatile(portsc_ptr) };
            unsafe {
                write_volatile(portsc_ptr, (portsc_now & PORTSC_PRESERVE) | PORTSC_PR);
            }
            if !poll_until(portsc_ptr, PORTSC_PRC, PORTSC_PRC, 1_000_000) {
                let portsc_after = unsafe { read_volatile(portsc_ptr) };
                ports.push(PortReport {
                    port,
                    portsc_before,
                    portsc_after,
                    message: "reset timed out".into(),
                });
                continue;
            }
            // Acknowledge PRC.
            let portsc_after_reset = unsafe { read_volatile(portsc_ptr) };
            unsafe {
                write_volatile(
                    portsc_ptr,
                    (portsc_after_reset & PORTSC_PRESERVE) | PORTSC_PRC,
                );
            }
            msg.push_str("reset; ");
        } else {
            msg.push_str("auto-enabled; ");
        }

        let portsc_after = unsafe { read_volatile(portsc_ptr) };
        let speed = ((portsc_after >> 10) & 0xF) as u8;
        if portsc_after & PORTSC_PED == 0 {
            msg.push_str("PED never set");
            ports.push(PortReport {
                port,
                portsc_before,
                portsc_after,
                message: msg,
            });
            continue;
        }

        let _ = write!(msg, "PED=1 speed={}", speed);
        ports.push(PortReport {
            port,
            portsc_before,
            portsc_after,
            message: msg.clone(),
        });

        // Issue Enable Slot on the command ring.
        match issue_enable_slot(info, st) {
            Ok((completion_code, slot_id)) => {
                serial_println!(
                    "xhci: port {} (speed {}) → Enable Slot cc={} slot={}",
                    port, speed, completion_code, slot_id
                );
                slots.push(SlotReport {
                    port,
                    speed,
                    completion_code,
                    slot_id,
                });
            }
            Err(e) => {
                serial_println!("xhci: port {}: Enable Slot failed: {}", port, e);
                slots.push(SlotReport {
                    port,
                    speed,
                    completion_code: 0,
                    slot_id: 0,
                });
            }
        }
    }

    let result = EnumResult { ports, slots };
    st.enumeration = Some(result.clone());
    Ok(result)
}

/// Build an Enable-Slot Command TRB at the current enqueue pointer, ring
/// the command doorbell, and consume the matching Command Completion
/// Event. Returns `(completion_code, slot_id)`.
fn issue_enable_slot(
    info: &XhciInfo,
    st: &mut XhciState,
) -> Result<(u8, u8), &'static str> {
    let cmd_ring = st.bringup.cmd_ring_addr as *mut u8;
    let trb = unsafe { cmd_ring.add(st.rings.cmd_enqueue * 16) };
    // Enable Slot Command: parameter = 0, status = 0,
    // control = type<<10 | C, type = 9, slot type = 0.
    unsafe {
        write_volatile(trb as *mut u64, 0);
        write_volatile(trb.add(8) as *mut u32, 0);
        write_volatile(
            trb.add(12) as *mut u32,
            (9u32 << 10) | st.rings.cmd_pcs as u32,
        );
    }
    // Advance enqueue. The 256th slot is the Link TRB at the end of the
    // ring; when we land on it, flip its cycle to the *current* PCS and
    // wrap the enqueue + toggle PCS.
    st.rings.cmd_enqueue += 1;
    if st.rings.cmd_enqueue == 255 {
        let link = unsafe { cmd_ring.add(255 * 16) };
        unsafe {
            let ctl = read_volatile(link.add(12) as *const u32);
            let new_ctl = (ctl & !1) | st.rings.cmd_pcs as u32;
            write_volatile(link.add(12) as *mut u32, new_ctl);
        }
        st.rings.cmd_enqueue = 0;
        st.rings.cmd_pcs ^= 1;
    }

    // Ring command doorbell (slot 0, target 0 = command).
    let db = (info.mmio_base + info.dboff as u64) as *mut u32;
    unsafe { write_volatile(db, 0) };

    drain_for_command_completion(info, st)
}

/// Snapshot of an event-ring TRB returned to a caller of `drain_event`.
/// `trb_type` and `parameter` are not consumed yet but will be needed
/// once we start parsing Port Status Change events and matching transfer
/// completions to their issuing TRB pointer.
#[allow(dead_code)]
#[derive(Clone, Copy)]
struct Event {
    trb_type: u8,
    completion_code: u8,
    slot_id: u8,
    /// Transfer Events: the Endpoint ID (DCI) the event is for — needed to
    /// tell a composite device's interrupt endpoints apart (keyboard vs mouse
    /// on the same slot).
    endpoint_id: u8,
    parameter: u64,
    /// Status field bits 0..23. For Transfer Events this is the
    /// *residue* (bytes not transferred); 0 = full transfer done.
    transfer_length: u32,
}

/// Drain TRBs from the event ring until one with `trb_type == want` is
/// found, or the deadline elapses. Every consumed TRB advances the
/// dequeue + writes ERDP with EHB cleared, so the ring doesn't lock up
/// even if unrelated events arrive first.
/// Consume one event TRB if the controller has produced one (its cycle bit
/// matches our consumer state); otherwise return `None` immediately. Advances
/// the dequeue pointer and writes ERDP (EHB cleared) so the ring never locks up.
/// The single non-blocking primitive under both the blocking `drain_event` and
/// the cooperative mouse poll.
fn try_consume_event(info: &XhciInfo, st: &mut XhciState) -> Option<Event> {
    let event_ring = st.bringup.event_ring_addr as *mut u8;
    let runtime = (info.mmio_base + info.rtsoff as u64) as *mut u8;
    let erdp_ptr = unsafe { runtime.add(0x20 + 0x18) } as *mut u64;

    let trb = unsafe { event_ring.add(st.rings.event_dequeue * 16) };
    let control = unsafe { read_volatile(trb.add(12) as *const u32) };
    let cycle = (control & 1) as u8;
    if cycle != st.rings.event_ccs {
        return None;
    }
    let trb_type = ((control >> 10) & 0x3F) as u8;
    let status = unsafe { read_volatile(trb.add(8) as *const u32) };
    let parameter = unsafe { read_volatile(trb as *const u64) };
    let ev = Event {
        trb_type,
        completion_code: ((status >> 24) & 0xFF) as u8,
        slot_id: ((control >> 24) & 0xFF) as u8,
        endpoint_id: ((control >> 16) & 0x1F) as u8,
        parameter,
        transfer_length: status & 0x00FF_FFFF,
    };
    st.rings.event_dequeue += 1;
    if st.rings.event_dequeue == 256 {
        st.rings.event_dequeue = 0;
        st.rings.event_ccs ^= 1;
    }
    let new_erdp =
        st.bringup.event_ring_addr + (st.rings.event_dequeue as u64) * 16;
    unsafe { write_volatile(erdp_ptr, new_erdp | 0x8) };
    Some(ev)
}

/// Drain TRBs from the event ring until one with `trb_type == want` is found, or
/// the deadline elapses. Transfer completions on the HID mouse's slot are
/// serviced inline (accumulate the report; the poll re-arms) and skipped — so a
/// mouse that moves mid-transfer can never be mistaken for the command/bulk
/// event this drain is waiting for. Every consumed TRB advances the dequeue, so
/// the ring doesn't lock up even when unrelated events arrive first.
fn drain_event(
    info: &XhciInfo,
    st: &mut XhciState,
    want: u8,
    max_us: u64,
) -> Result<Event, &'static str> {
    let per = time::tsc_per_us().max(1);
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    loop {
        match try_consume_event(info, st) {
            Some(ev) => {
                if service_hid_transfer(st, &ev) {
                    continue;
                }
                if ev.trb_type == want {
                    return Ok(ev);
                }
                // Unrelated event — skip and keep draining.
            }
            None => {
                let now = unsafe { core::arch::x86_64::_rdtsc() };
                if (now - start) / per > max_us {
                    return Err("event-ring deadline elapsed");
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// Like [`drain_event`] but for the Transfer Event of a **specific endpoint**.
/// HID interrupt completions are always serviced inline and skipped, so a
/// mouse/keyboard report landing on the shared event ring mid-command can never
/// be returned as this transfer's completion (which would hand back a wrong
/// `transfer_length` and a garbage buffer: "corrupt store: chain length" on real
/// hardware). The acceptance rule depends on `expect_trb`:
///
/// * `Some(trb)` — **bulk** transfers. Accept only the event whose TRB Pointer
///   is `trb`, the single Normal TRB we posted. This is the strongest match: a
///   single-TRB TD raises *both* its success and its error events against that
///   same TRB, so stalls/errors are still surfaced for BOT recovery, while a
///   stale completion from a previous transfer on this *same* endpoint (one that
///   timed out or was reset and landed late) carries a *different* TRB Pointer
///   and is correctly skipped. Slot+endpoint matching could not tell the two
///   apart — that residual hole was the "corrupt store: chain length" recurrence
///   on real USB sticks.
///
/// * `None` — **control** (EP0) transfers. A control TD is multi-TRB, and an
///   errored stage raises its event against the *failing* stage TRB (e.g. the
///   data stage), not the status TRB we'd name — so we match on slot+endpoint
///   instead, which still surfaces that error for EP0 reset+retry (the fix for
///   the flaky Alcor 058f:6387 config read). HID lives on a different slot, so
///   it's still excluded.
///
/// Do NOT "simplify" the bulk path back to slot+endpoint matching: it reopens
/// the stale-event corruption on real hardware that QEMU never reproduces.
fn drain_transfer_event(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_id: u8,
    endpoint_id: u8,
    expect_trb: Option<u64>,
    max_us: u64,
) -> Result<Event, &'static str> {
    let per = time::tsc_per_us().max(1);
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    loop {
        match try_consume_event(info, st) {
            Some(ev) => {
                if service_hid_transfer(st, &ev) {
                    continue;
                }
                let matched = match expect_trb {
                    Some(trb) => ev.trb_type == 32 && ev.parameter == trb,
                    None => {
                        ev.trb_type == 32
                            && ev.slot_id == slot_id
                            && ev.endpoint_id == endpoint_id
                    }
                };
                if matched {
                    return Ok(ev);
                }
                // Not our transfer (HID/stale/unrelated) — skip and keep draining.
            }
            None => {
                let now = unsafe { core::arch::x86_64::_rdtsc() };
                if (now - start) / per > max_us {
                    return Err("event-ring deadline elapsed");
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// Route a Transfer Event (type 32) to the HID mouse or keyboard it belongs
/// to, by (slot, endpoint DCI) — so a composite device's two interrupt
/// endpoints on one shared slot are told apart. Returns true if it was a HID
/// event (serviced), so the caller skips it.
fn service_hid_transfer(st: &mut XhciState, ev: &Event) -> bool {
    if ev.trb_type != 32 {
        return false;
    }
    if let Some(i) = st
        .mice
        .iter()
        .position(|m| m.slot_id == ev.slot_id && m.dci == ev.endpoint_id)
    {
        service_mouse(st, i, ev.completion_code);
        return true;
    }
    if let Some(i) = st
        .keyboards
        .iter()
        .position(|k| k.slot_id == ev.slot_id && k.dci == ev.endpoint_id)
    {
        service_keyboard(st, i, ev.completion_code);
        return true;
    }
    false
}

/// Handle one completed interrupt-IN transfer for the HID mouse. Parses the
/// boot-protocol report (byte0 = buttons, byte1 = dX i8, byte2 = dY i8 with
/// positive = down) and accumulates it; marks the TRB consumed so the next poll
/// re-arms. A non-Success/Short completion means the endpoint halted — flag it
/// for reset by the poll (not here, to avoid a re-entrant command drain).
fn service_mouse(st: &mut XhciState, idx: usize, cc: u8) {
    let buf = st.mice[idx].report_buf;
    let (b0, b1, b2) = unsafe {
        (
            read_volatile(buf as *const u8),
            read_volatile((buf as *const u8).add(1)),
            read_volatile((buf as *const u8).add(2)),
        )
    };
    let m = &mut st.mice[idx];
    m.armed = false; // its TRB was consumed
    if cc == 1 || cc == 13 {
        m.accum_dx += (b1 as i8) as i32;
        m.accum_dy += (b2 as i8) as i32;
        m.buttons = b0;
        m.dirty = true;
    } else {
        m.needs_reset = true;
    }
}

/// Queue one interrupt-IN transfer for mouse `idx` and ring its doorbell, so
/// the controller delivers the next report into the report buffer. Guarded by
/// `armed` so we never queue two at once. Does no draining, so it is safe to
/// call from inside an event drain.
fn arm_mouse(info: &XhciInfo, st: &mut XhciState, idx: usize) {
    let m = &st.mice[idx];
    if m.armed {
        return;
    }
    let (slot_idx, ep_idx, buf, len) = (m.slot_idx, m.ep_idx, m.report_buf, m.report_len);
    let (slot_id, dci, _) = post_normal_trb(st, slot_idx, ep_idx, buf, len);
    let db = (info.mmio_base + info.dboff as u64 + 4 * slot_id as u64) as *mut u32;
    unsafe { write_volatile(db, dci as u32) };
    st.mice[idx].armed = true;
}

/// Handle one completed interrupt-IN transfer for keyboard `idx`. Decodes the
/// 8-byte boot report (byte0 = modifiers, bytes 2..8 = up to 6 key usages)
/// against the previous one and feeds newly-pressed keys into the shared input
/// queue. A non-Success/Short completion flags an endpoint reset for the poll
/// to perform (not here — avoids a re-entrant command drain).
fn service_keyboard(st: &mut XhciState, idx: usize, cc: u8) {
    let (buf, last) = { let k = &st.keyboards[idx]; (k.report_buf, k.last) };
    let mut report = [0u8; 8];
    for (i, b) in report.iter_mut().enumerate() {
        *b = unsafe { read_volatile((buf as *const u8).add(i)) };
    }
    let armed_ok = cc == 1 || cc == 13;
    if armed_ok {
        // HID modifier byte: bit1/5 = Shift, bit6 = Right Alt (AltGr),
        // bit3/7 = GUI (Win).
        let mods = crate::keymap::Mods {
            shift: report[0] & 0x22 != 0,
            altgr: report[0] & 0x40 != 0,
            gui: report[0] & 0x88 != 0,
        };
        for i in 2..8 {
            let usage = report[i];
            if usage < 4 || last[2..8].contains(&usage) {
                continue; // empty/rollover, or held since the last report
            }
            if let Some(key) = crate::keymap::translate(usage, mods) {
                crate::ps2::feed_key(key);
            }
        }
    }
    let k = &mut st.keyboards[idx];
    k.armed = false;
    if armed_ok {
        k.last = report;
    } else {
        k.needs_reset = true;
    }
}

/// Queue one interrupt-IN transfer for keyboard `idx` and ring its doorbell.
/// Guarded by `armed`; does no draining (safe from inside an event drain).
fn arm_keyboard(info: &XhciInfo, st: &mut XhciState, idx: usize) {
    let k = &st.keyboards[idx];
    if k.armed {
        return;
    }
    let (slot_idx, ep_idx, buf, len) = (k.slot_idx, k.ep_idx, k.report_buf, k.report_len);
    let (slot_id, dci, _) = post_normal_trb(st, slot_idx, ep_idx, buf, len);
    let db = (info.mmio_base + info.dboff as u64 + 4 * slot_id as u64) as *mut u32;
    unsafe { write_volatile(db, dci as u32) };
    st.keyboards[idx].armed = true;
}

fn drain_for_command_completion(
    info: &XhciInfo,
    st: &mut XhciState,
) -> Result<(u8, u8), &'static str> {
    let ev = drain_event(info, st, 33, 1_000_000)?;
    Ok((ev.completion_code, ev.slot_id))
}

// =====================================================================
// Phase 4 sub-pass 5: Address Device + Get Device Descriptor.
// First time we actually receive bytes back from a USB device.
// =====================================================================

/// Initial guess for `bMaxPacketSize0` based on PORTSC speed. The real
/// value comes back in the Device Descriptor; on Low/Full Speed the
/// follow-up procedure (re-evaluate EP0 context) is still pending.
fn default_max_packet_ep0(speed: u8) -> u16 {
    match speed {
        1 => 64,  // Full Speed (usually 8/16/32/64; 64 is safe upper)
        2 => 8,   // Low Speed
        3 => 64,  // High Speed
        4 | 5 => 512, // Super / SuperPlus
        _ => 64,
    }
}

/// For every slot that completed Enable Slot successfully, build the
/// device + input + transfer-ring DMA structures, issue Address Device,
/// and (on success) fetch the 18-byte standard Device Descriptor.
pub fn address_enabled_slots(
    dev: &PciDevice,
    info: &XhciInfo,
) -> Result<Vec<AddressedDevice>, &'static str> {
    let _ = dev;
    let mut guard = STATES.lock();
    let st =
        state_for(&mut guard, info.mmio_base).ok_or("bring up the controller first ([b])")?;
    if !info.mmio_accessible {
        return Err("MMIO not accessible");
    }

    let enumeration = st
        .enumeration
        .clone()
        .ok_or("scan ports first ([e])")?;
    let entry_size = if info.csz_64 { 64usize } else { 32 };

    let mut out: Vec<AddressedDevice> = Vec::new();
    for slot_report in &enumeration.slots {
        if slot_report.slot_id == 0 || slot_report.completion_code != 1 {
            continue;
        }
        if st.addressed.iter().any(|a| a.slot_id == slot_report.slot_id) {
            // Already addressed in a previous invocation.
            continue;
        }
        let max_pkt = default_max_packet_ep0(slot_report.speed);
        let res = build_slot_resources(
            slot_report.slot_id,
            slot_report.port,
            slot_report.speed,
            max_pkt,
            entry_size,
        );
        // Wire DCBAA[slot_id] → device_ctx (must be done before issuing
        // Address Device).
        unsafe {
            let dcbaa_slot = (st.bringup.dcbaa_addr as *mut u64)
                .add(slot_report.slot_id as usize);
            write_volatile(dcbaa_slot, res.device_ctx);
        }

        let addr_cc = match issue_address_device(info, st, &res) {
            Ok(cc) => cc,
            Err(e) => {
                serial_println!(
                    "xhci: Address Device for slot {} failed: {}",
                    slot_report.slot_id, e
                );
                out.push(AddressedDevice {
                    slot_id: slot_report.slot_id,
                    port: slot_report.port,
                    speed: slot_report.speed,
                    addr_completion_code: 0,
                    descriptor_completion_code: 0,
                    descriptor: None,
                    raw_descriptor: Vec::new(),
                    eval_context_cc: None,
                    config_completion_code: 0,
                    config: None,
                    raw_config: Vec::new(),
                    configure_endpoint_cc: 0,
                    set_config_cc: 0,
                    configured_endpoints: Vec::new(),
                });
                continue;
            }
        };
        serial_println!(
            "xhci: slot {} Address Device cc={} ({})",
            slot_report.slot_id, addr_cc, completion_code_name(addr_cc)
        );

        let mut entry = AddressedDevice {
            slot_id: slot_report.slot_id,
            port: slot_report.port,
            speed: slot_report.speed,
            addr_completion_code: addr_cc,
            descriptor_completion_code: 0,
            descriptor: None,
            raw_descriptor: Vec::new(),
            eval_context_cc: None,
            config_completion_code: 0,
            config: None,
            raw_config: Vec::new(),
            configure_endpoint_cc: 0,
            set_config_cc: 0,
            configured_endpoints: Vec::new(),
        };

        if addr_cc == 1 {
            // USB 2.0 §9.2.6.3: a device needs a recovery interval (>=2 ms) after
            // its address is set before it reliably accepts further requests.
            // Cheap high-speed flash drives wedge if talked to too soon — the
            // first request may appear to work but the next transaction-errors
            // (observed on a real Alcor 058f:6387). QEMU has zero latency so it
            // never needs this. Use a generous margin.
            time::delay_ms(20);
            let mut res_mut = res.clone();
            match get_device_descriptor(info, st, &mut res_mut) {
                Ok((cc, bytes)) => {
                    entry.descriptor_completion_code = cc;
                    if cc == 1 && bytes.len() >= 18 {
                        entry.descriptor = Some(parse_device_descriptor(&bytes));
                    }
                    entry.raw_descriptor = bytes;
                    // Persist the advanced transfer-ring position.
                    st.slots.push(res_mut);
                }
                Err(e) => {
                    serial_println!(
                        "xhci: slot {} Get Device Descriptor failed: {}",
                        slot_report.slot_id, e
                    );
                    st.slots.push(res);
                }
            }
        } else {
            st.slots.push(res);
        }

        out.push(entry);
    }

    st.addressed.extend(out.iter().cloned());
    Ok(out)
}

fn build_slot_resources(
    slot_id: u8,
    port: u8,
    speed: u8,
    max_pkt: u16,
    entry_size: usize,
) -> SlotResources {
    // Device Context = 32 entries.
    let device_ctx = alloc_dma(entry_size * 32, 4096);
    // Input Context = 33 entries (control + slot + 31 EPs).
    let input_ctx = alloc_dma(entry_size * 33, 4096);

    // Input Control Context (entry 0): A0 = Slot, A1 = EP 0.
    unsafe {
        write_volatile(input_ctx as *mut u32, 0);
        write_volatile(input_ctx.add(4) as *mut u32, 0b11);
    }

    // Slot Context (entry 1).
    let slot_ctx = unsafe { input_ctx.add(entry_size) };
    unsafe {
        // DW0: Speed in [23:20], Context Entries = 1 in [31:27].
        write_volatile(
            slot_ctx as *mut u32,
            ((speed as u32) << 20) | (1u32 << 27),
        );
        // DW1: Root Hub Port Number in [23:16] (1-based).
        write_volatile(slot_ctx.add(4) as *mut u32, (port as u32) << 16);
        write_volatile(slot_ctx.add(8) as *mut u32, 0);
        write_volatile(slot_ctx.add(12) as *mut u32, 0);
    }

    // EP 0 Context (entry 2). Allocate transfer ring first.
    let tr_ring = alloc_dma(256 * 16, 4096);
    unsafe {
        let link = tr_ring.add(255 * 16);
        write_volatile(link as *mut u64, tr_ring as u64);
        write_volatile(link.add(8) as *mut u32, 0);
        write_volatile(
            link.add(12) as *mut u32,
            (6u32 << 10) | (1 << 1) | 1,
        );
    }
    let ep0 = unsafe { input_ctx.add(entry_size * 2) };
    unsafe {
        // DW0: zero (state/interval N/A for control).
        write_volatile(ep0 as *mut u32, 0);
        // DW1: CErr=3 in [2:1], EP Type=4 (Control) in [5:3],
        //      Max Packet Size in [31:16].
        write_volatile(
            ep0.add(4) as *mut u32,
            (3u32 << 1) | (4 << 3) | ((max_pkt as u32) << 16),
        );
        // DW2/3 (u64): TR Dequeue Pointer + DCS=1.
        write_volatile(ep0.add(8) as *mut u64, tr_ring as u64 | 1);
        // DW4: Average TRB Length = 8 (control transfers are tiny).
        write_volatile(ep0.add(16) as *mut u32, 8);
    }

    SlotResources {
        slot_id,
        port,
        speed,
        max_packet_size_ep0: max_pkt,
        device_ctx: device_ctx as u64,
        input_ctx: input_ctx as u64,
        tr_ring: tr_ring as u64,
        tr_enqueue: 0,
        tr_pcs: 1,
        endpoints: Vec::new(),
    }
}

fn issue_address_device(
    info: &XhciInfo,
    st: &mut XhciState,
    res: &SlotResources,
) -> Result<u8, &'static str> {
    let cmd_ring = st.bringup.cmd_ring_addr as *mut u8;
    let trb = unsafe { cmd_ring.add(st.rings.cmd_enqueue * 16) };
    let control: u32 = (11u32 << 10) // TRB type = Address Device
        | (st.rings.cmd_pcs as u32)
        | ((res.slot_id as u32) << 24);
    unsafe {
        write_volatile(trb as *mut u64, res.input_ctx);
        write_volatile(trb.add(8) as *mut u32, 0);
        write_volatile(trb.add(12) as *mut u32, control);
    }
    advance_cmd_enqueue(st);

    let db = (info.mmio_base + info.dboff as u64) as *mut u32;
    unsafe { write_volatile(db, 0) };

    let ev = drain_event(info, st, 33, 1_000_000)?;
    Ok(ev.completion_code)
}

fn advance_cmd_enqueue(st: &mut XhciState) {
    let cmd_ring = st.bringup.cmd_ring_addr as *mut u8;
    st.rings.cmd_enqueue += 1;
    if st.rings.cmd_enqueue == 255 {
        let link = unsafe { cmd_ring.add(255 * 16) };
        unsafe {
            let ctl = read_volatile(link.add(12) as *const u32);
            let new_ctl = (ctl & !1) | st.rings.cmd_pcs as u32;
            write_volatile(link.add(12) as *mut u32, new_ctl);
        }
        st.rings.cmd_enqueue = 0;
        st.rings.cmd_pcs ^= 1;
    }
}

/// Build the 3-stage (Setup / optional Data / Status) control transfer
/// and consume the resulting Transfer Event. Used for every standard
/// control request: GET_DESCRIPTOR, SET_CONFIGURATION, SET_ADDRESS-style
/// follow-ups, etc.
/// Control transfer with automatic recovery. On a transaction error / stall
/// (any completion code other than Success=1 or Short-Packet=13) xHCI leaves the
/// control endpoint HALTED, so further transfers on it also fail. Recover per the
/// spec — Reset Endpoint, then Set TR Dequeue Pointer to where the next attempt
/// will write — and retry. Real high-speed devices (e.g. an Alcor 058f:6387)
/// transaction-error on a control transfer that QEMU accepts; this makes the
/// stack resilient. QEMU never errors, so the recovery path stays dormant there.
fn control_transfer(
    info: &XhciInfo,
    st: &mut XhciState,
    res: &mut SlotResources,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
) -> Result<(u8, Vec<u8>), &'static str> {
    let mut attempt = 0u8;
    loop {
        let (cc, bytes) = control_transfer_once(
            info, st, res, request_type, request, value, index, length,
        )?;
        if cc == 1 || cc == 13 || attempt >= 2 {
            return Ok((cc, bytes));
        }
        attempt += 1;
        serial_println!(
            "xhci: slot {} control xfer cc={} -> reset EP0 + retry {}",
            res.slot_id, cc, attempt
        );
        // EP0 (DCI 1) is halted after the error: clear it, repoint its ring at
        // our current enqueue position, settle briefly, then retry.
        let _ = reset_endpoint(info, st, res.slot_id, 1);
        let _ = set_tr_dequeue(info, st, res, 1);
        time::delay_ms(2);
    }
}

/// Reset Endpoint command (TRB type 14) — clears a halted endpoint's state.
fn reset_endpoint(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_id: u8,
    ep_dci: u8,
) -> Result<u8, &'static str> {
    let cmd_ring = st.bringup.cmd_ring_addr as *mut u8;
    let trb = unsafe { cmd_ring.add(st.rings.cmd_enqueue * 16) };
    let control: u32 = (14u32 << 10)
        | (st.rings.cmd_pcs as u32)
        | ((ep_dci as u32) << 16)
        | ((slot_id as u32) << 24);
    unsafe {
        write_volatile(trb as *mut u64, 0);
        write_volatile(trb.add(8) as *mut u32, 0);
        write_volatile(trb.add(12) as *mut u32, control);
    }
    advance_cmd_enqueue(st);
    let db = (info.mmio_base + info.dboff as u64) as *mut u32;
    unsafe { write_volatile(db, 0) };
    let ev = drain_event(info, st, 33, 1_000_000)?;
    Ok(ev.completion_code)
}

/// Set TR Dequeue Pointer command (TRB type 16) — point an endpoint's transfer
/// ring at `dequeue` (a ring address ORed with the live cycle state), so the
/// next TRBs the host writes there are what the controller consumes.
fn set_tr_dequeue_raw(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_id: u8,
    ep_dci: u8,
    dequeue: u64,
) -> Result<u8, &'static str> {
    let cmd_ring = st.bringup.cmd_ring_addr as *mut u8;
    let trb = unsafe { cmd_ring.add(st.rings.cmd_enqueue * 16) };
    let control: u32 = (16u32 << 10)
        | (st.rings.cmd_pcs as u32)
        | ((ep_dci as u32) << 16)
        | ((slot_id as u32) << 24);
    unsafe {
        write_volatile(trb as *mut u64, dequeue);
        write_volatile(trb.add(8) as *mut u32, 0);
        write_volatile(trb.add(12) as *mut u32, control);
    }
    advance_cmd_enqueue(st);
    let db = (info.mmio_base + info.dboff as u64) as *mut u32;
    unsafe { write_volatile(db, 0) };
    let ev = drain_event(info, st, 33, 1_000_000)?;
    Ok(ev.completion_code)
}

/// Set TR Dequeue Pointer for the control endpoint (EP0), using `res`'s ring
/// position + cycle state.
fn set_tr_dequeue(
    info: &XhciInfo,
    st: &mut XhciState,
    res: &SlotResources,
    ep_dci: u8,
) -> Result<u8, &'static str> {
    let dequeue: u64 =
        (res.tr_ring + (res.tr_enqueue as u64) * 16) | (res.tr_pcs as u64);
    set_tr_dequeue_raw(info, st, res.slot_id, ep_dci, dequeue)
}

/// Clear a halted bulk endpoint and repoint its ring at the current enqueue
/// position. Used for MSC Bulk-Only Transport error recovery: a stall on a data
/// or status stage leaves the endpoint halted, and every later command on it
/// fails until it's reset. Best-effort (errors swallowed — nothing better to do).
fn reset_bulk_endpoint(info: &XhciInfo, st: &mut XhciState, slot_idx: usize, ep_idx: usize) {
    let (slot_id, dci, dequeue) = {
        let ep = &st.slots[slot_idx].endpoints[ep_idx];
        (
            st.slots[slot_idx].slot_id,
            ep.dci,
            (ep.tr_ring + (ep.tr_enqueue as u64) * 16) | (ep.tr_pcs as u64),
        )
    };
    let _ = reset_endpoint(info, st, slot_id, dci);
    let _ = set_tr_dequeue_raw(info, st, slot_id, dci, dequeue);
}

/// One control transfer attempt (Setup / optional Data / Status), no recovery.
fn control_transfer_once(
    info: &XhciInfo,
    st: &mut XhciState,
    res: &mut SlotResources,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
) -> Result<(u8, Vec<u8>), &'static str> {
    let buf: Option<DmaBuffer> = if length > 0 {
        Some(DmaBuffer::new(length as usize, 4096))
    } else {
        None
    };
    let buf_addr: u64 = buf.as_ref().map(|b| b.ptr() as u64).unwrap_or(0);
    let is_in = request_type & 0x80 != 0;

    // 8-byte SETUP packet, little-endian, packed into TRB parameter.
    let setup_packet: u64 = (request_type as u64)
        | ((request as u64) << 8)
        | ((value as u64) << 16)
        | ((index as u64) << 32)
        | ((length as u64) << 48);

    let tr_ring = res.tr_ring as *mut u8;

    // ---- Setup Stage TRB (type 2) -----------------------------------
    let setup = unsafe { tr_ring.add(res.tr_enqueue * 16) };
    let trt: u32 = if length == 0 {
        0 // No Data Stage
    } else if is_in {
        3 // IN Data Stage
    } else {
        2 // OUT Data Stage
    };
    unsafe {
        write_volatile(setup as *mut u64, setup_packet);
        write_volatile(setup.add(8) as *mut u32, 8);
        // Cycle | IDT=1 | TRB type=2 | TRT.
        write_volatile(
            setup.add(12) as *mut u32,
            (res.tr_pcs as u32) | (1u32 << 6) | (2u32 << 10) | (trt << 16),
        );
    }
    advance_tr_enqueue(res);

    // ---- Optional Data Stage TRB (type 3) ---------------------------
    if length > 0 {
        let data = unsafe { tr_ring.add(res.tr_enqueue * 16) };
        let dir: u32 = if is_in { 1 } else { 0 };
        unsafe {
            write_volatile(data as *mut u64, buf_addr);
            write_volatile(data.add(8) as *mut u32, length as u32);
            // Cycle | TRB type=3 | DIR.
            write_volatile(
                data.add(12) as *mut u32,
                (res.tr_pcs as u32) | (3u32 << 10) | (dir << 16),
            );
        }
        advance_tr_enqueue(res);
    }

    // ---- Status Stage TRB (type 4, IOC=1) ---------------------------
    // DIR is *opposite* of data direction; defaults to IN when there's
    // no data stage.
    let status_dir: u32 = if is_in && length > 0 { 0 } else { 1 };
    let stage = unsafe { tr_ring.add(res.tr_enqueue * 16) };
    unsafe {
        write_volatile(stage as *mut u64, 0);
        write_volatile(stage.add(8) as *mut u32, 0);
        write_volatile(
            stage.add(12) as *mut u32,
            (res.tr_pcs as u32) | (1u32 << 5) | (4u32 << 10) | (status_dir << 16),
        );
    }
    advance_tr_enqueue(res);

    // Ring doorbell for this slot, target = DCI 1 (EP0).
    let db = (info.mmio_base + info.dboff as u64 + 4 * res.slot_id as u64) as *mut u32;
    unsafe { write_volatile(db, 1) };

    // EP0 control transfer: match the Transfer Event by slot + EP0 (DCI 1). On
    // success that's the Status Stage event; on a data-stage error it's the
    // error event for EP0 — either way it's ours, and surfacing the error lets
    // the caller reset EP0 and retry instead of hanging to the deadline.
    let ev = drain_transfer_event(info, st, res.slot_id, 1, None, 2_000_000)?;
    let bytes = match (length > 0, &buf) {
        (true, Some(b)) if ev.completion_code == 1 || ev.completion_code == 13 => {
            b.read_to_vec(length as usize)
        }
        _ => Vec::new(),
    };
    // `buf` drops here; its DMA backing is freed.
    Ok((ev.completion_code, bytes))
}

/// GET_DESCRIPTOR(DEVICE, 0, 18) — first descriptor we ever ask for.
fn get_device_descriptor(
    info: &XhciInfo,
    st: &mut XhciState,
    res: &mut SlotResources,
) -> Result<(u8, Vec<u8>), &'static str> {
    control_transfer(info, st, res, 0x80, 6, 0x0100, 0, 18)
}

fn advance_tr_enqueue(res: &mut SlotResources) {
    res.tr_enqueue += 1;
    if res.tr_enqueue == 255 {
        let link = unsafe { (res.tr_ring as *mut u8).add(255 * 16) };
        unsafe {
            let ctl = read_volatile(link.add(12) as *const u32);
            let new_ctl = (ctl & !1) | res.tr_pcs as u32;
            write_volatile(link.add(12) as *mut u32, new_ctl);
        }
        res.tr_enqueue = 0;
        res.tr_pcs ^= 1;
    }
}

fn parse_device_descriptor(b: &[u8]) -> DeviceDescriptor {
    DeviceDescriptor {
        usb_bcd: u16::from_le_bytes([b[2], b[3]]),
        device_class: b[4],
        device_subclass: b[5],
        device_protocol: b[6],
        max_packet_size_ep0: b[7],
        id_vendor: u16::from_le_bytes([b[8], b[9]]),
        id_product: u16::from_le_bytes([b[10], b[11]]),
        device_bcd: u16::from_le_bytes([b[12], b[13]]),
        manufacturer_idx: b[14],
        product_idx: b[15],
        serial_idx: b[16],
        num_configurations: b[17],
    }
}

/// Friendly name for a Device Descriptor class byte. USB-IF assignments,
/// abbreviated.
pub fn device_class_name(c: u8) -> &'static str {
    match c {
        0x00 => "(interface-defined)",
        0x01 => "Audio",
        0x02 => "Communications/CDC",
        0x03 => "HID",
        0x05 => "Physical",
        0x06 => "Still Image",
        0x07 => "Printer",
        0x08 => "Mass Storage",
        0x09 => "Hub",
        0x0A => "CDC Data",
        0x0B => "Smart Card",
        0x0D => "Content Security",
        0x0E => "Video",
        0x0F => "Personal Healthcare",
        0x10 => "Audio/Video",
        0x11 => "Billboard",
        0xDC => "Diagnostic",
        0xE0 => "Wireless Controller",
        0xEF => "Misc.",
        0xFE => "Application Specific",
        0xFF => "Vendor Specific",
        _ => "(other)",
    }
}

/// Spec table 6-90 — only the codes we currently observe.
// =====================================================================
// Phase 4 sub-pass 6: Evaluate Context (LS/FS max-packet correction) +
// Get Configuration Descriptor (header + full + parse).
// =====================================================================

/// For every addressed device with a Device Descriptor, optionally issue
/// Evaluate Context if `bMaxPacketSize0` differs from our initial guess
/// (only meaningful on Low / Full Speed), then fetch the Configuration
/// Descriptor: first 9 bytes to learn `wTotalLength`, then the full
/// descriptor to walk interfaces and endpoints.
pub fn fetch_configurations(
    dev: &PciDevice,
    info: &XhciInfo,
) -> Result<Vec<AddressedDevice>, &'static str> {
    let _ = dev;
    let mut guard = STATES.lock();
    let st =
        state_for(&mut guard, info.mmio_base).ok_or("bring up the controller first ([b])")?;
    if !info.mmio_accessible {
        return Err("MMIO not accessible");
    }
    if st.addressed.is_empty() {
        return Err("address devices first ([a])");
    }

    let n = st.addressed.len();
    for i in 0..n {
        if st.addressed[i].descriptor.is_none() {
            continue;
        }
        if st.addressed[i].config.is_some() {
            continue; // Already fetched.
        }
        let slot_id = st.addressed[i].slot_id;
        let speed = st.addressed[i].speed;
        let reported_max_pkt = st.addressed[i]
            .descriptor
            .as_ref()
            .unwrap()
            .max_packet_size_ep0 as u16;
        let slot_idx = match st.slots.iter().position(|s| s.slot_id == slot_id) {
            Some(idx) => idx,
            None => continue,
        };
        let mut res = st.slots[slot_idx].clone();

        // ---- Evaluate Context (LS / FS only, when needed) ----------
        let mut eval_cc: Option<u8> = None;
        if (speed == 1 || speed == 2) && reported_max_pkt != res.max_packet_size_ep0 {
            match evaluate_max_packet(info, st, &mut res, reported_max_pkt) {
                Ok(cc) => {
                    eval_cc = Some(cc);
                    serial_println!(
                        "xhci: slot {} Evaluate Context cc={} (set MaxPkt EP0={})",
                        slot_id, cc, reported_max_pkt
                    );
                }
                Err(e) => {
                    serial_println!(
                        "xhci: slot {} Evaluate Context failed: {}",
                        slot_id, e
                    );
                }
            }
        }

        // ---- Get Configuration Descriptor, with retry ----------------
        // Some sticks/controllers return a Success CSW for the config-descriptor
        // control transfer but hand back empty/zero data (seen on a Cheshunt
        // root port: cc=1 yet no interfaces parsed → device never configured →
        // not recognised as mass storage → "no boot drive"). The device
        // descriptor read fine, so addressing is OK; only this fetch is flaky.
        // Re-fetch (with a growing settle) until it parses to a usable config.
        // Devices that answer correctly the first time break immediately, so
        // working machines are unaffected.
        let mut config_cc = 0u8;
        let mut bytes: Vec<u8> = Vec::new();
        let mut parsed: Option<Configuration> = None;
        for attempt in 0..4 {
            // Settle between control transfers; real HS devices can transaction-
            // error on a back-to-back request that QEMU accepts instantly. Later
            // attempts wait longer for a slow/settling port.
            time::delay_ms(if attempt == 0 { 5 } else { 20 * attempt as u64 });
            // 9-byte header first, to learn wTotalLength.
            let (cc1, hdr) =
                match control_transfer(info, st, &mut res, 0x80, 6, 0x0200, 0, 9) {
                    Ok(v) => v,
                    Err(e) => {
                        serial_println!(
                            "xhci: slot {} GET_DESCRIPTOR(CONFIG,short) failed: {}",
                            slot_id, e
                        );
                        continue;
                    }
                };
            config_cc = cc1;
            bytes = hdr.clone();
            if (cc1 == 1 || cc1 == 13) && hdr.len() >= 4 {
                let total = u16::from_le_bytes([hdr[2], hdr[3]]);
                if total > 9 {
                    match control_transfer(info, st, &mut res, 0x80, 6, 0x0200, 0, total) {
                        Ok((cc2, full)) => {
                            config_cc = cc2;
                            if !full.is_empty() {
                                bytes = full;
                            }
                        }
                        Err(e) => {
                            serial_println!(
                                "xhci: slot {} GET_DESCRIPTOR(CONFIG,full) failed: {}",
                                slot_id, e
                            );
                        }
                    }
                }
            }
            parsed = parse_configuration(&bytes);
            // Accept only a config that actually yielded interfaces; a bogus
            // (empty/zero) read parses to None or zero interfaces → retry.
            if parsed.as_ref().map_or(false, |c| !c.interfaces.is_empty()) {
                break;
            }
        }

        st.slots[slot_idx] = res;
        st.addressed[i].eval_context_cc = eval_cc;
        st.addressed[i].config_completion_code = config_cc;
        st.addressed[i].config = parsed;
        st.addressed[i].raw_config = bytes;
    }

    Ok(st.addressed.clone())
}

/// Issue Evaluate Context (TRB type 13) with `A1=1` (only EP 0 changed)
/// and the new `bMaxPacketSize0` written into the Input Context's EP 0
/// entry.
fn evaluate_max_packet(
    info: &XhciInfo,
    st: &mut XhciState,
    res: &mut SlotResources,
    new_max_pkt: u16,
) -> Result<u8, &'static str> {
    let entry_size = if info.csz_64 { 64usize } else { 32 };
    let input_ctx = res.input_ctx as *mut u8;
    // Input Control: A0 = 0, A1 = 1 (only EP 0 evaluated).
    unsafe {
        write_volatile(input_ctx as *mut u32, 0);
        write_volatile(input_ctx.add(4) as *mut u32, 0b10);
    }
    // Patch Max Packet Size on the EP 0 Context (entry 2 of Input Context).
    let ep0 = unsafe { input_ctx.add(entry_size * 2) };
    unsafe {
        let dw1 = read_volatile(ep0.add(4) as *const u32);
        let new_dw1 = (dw1 & 0x0000_FFFF) | ((new_max_pkt as u32) << 16);
        write_volatile(ep0.add(4) as *mut u32, new_dw1);
    }
    // Evaluate Context command on the command ring.
    let cmd_ring = st.bringup.cmd_ring_addr as *mut u8;
    let trb = unsafe { cmd_ring.add(st.rings.cmd_enqueue * 16) };
    let control: u32 = (13u32 << 10)
        | (st.rings.cmd_pcs as u32)
        | ((res.slot_id as u32) << 24);
    unsafe {
        write_volatile(trb as *mut u64, res.input_ctx);
        write_volatile(trb.add(8) as *mut u32, 0);
        write_volatile(trb.add(12) as *mut u32, control);
    }
    advance_cmd_enqueue(st);
    let db = (info.mmio_base + info.dboff as u64) as *mut u32;
    unsafe { write_volatile(db, 0) };

    let ev = drain_event(info, st, 33, 1_000_000)?;
    if ev.completion_code == 1 {
        res.max_packet_size_ep0 = new_max_pkt;
    }
    Ok(ev.completion_code)
}

/// Walk a Configuration Descriptor (header + interface + endpoint
/// descriptors, with class-specific descriptors interleaved). Returns
/// `None` if the buffer is shorter than the 9-byte header or the header
/// type/length bytes look wrong.
fn parse_configuration(b: &[u8]) -> Option<Configuration> {
    if b.len() < 9 || b[0] < 9 || b[1] != 2 {
        return None;
    }
    let total_length = u16::from_le_bytes([b[2], b[3]]);
    let num_interfaces = b[4];
    let config_value = b[5];
    let attributes = b[7];
    // bMaxPower units: 2 mA for USB 2, 8 mA for USB 3. The descriptor
    // itself doesn't tell us which; assume USB 2 (caller cross-references
    // with the Device Descriptor's USB BCD if it cares).
    let max_power_ma = (b[8] as u16) * 2;

    let mut interfaces: Vec<Interface> = Vec::new();
    let mut current: Option<Interface> = None;
    let mut pos = 9usize;
    while pos + 2 <= b.len() {
        let len = b[pos] as usize;
        if len == 0 || pos + len > b.len() {
            break;
        }
        let dtype = b[pos + 1];
        match dtype {
            4 if len >= 9 => {
                // Interface Descriptor (USB 9.6.5).
                if let Some(it) = current.take() {
                    interfaces.push(it);
                }
                current = Some(Interface {
                    number: b[pos + 2],
                    alt_setting: b[pos + 3],
                    class: b[pos + 5],
                    subclass: b[pos + 6],
                    protocol: b[pos + 7],
                    endpoints: Vec::new(),
                });
            }
            5 if len >= 7 => {
                // Endpoint Descriptor (USB 9.6.6).
                if let Some(it) = current.as_mut() {
                    let address = b[pos + 2];
                    it.endpoints.push(Endpoint {
                        address,
                        direction_in: address & 0x80 != 0,
                        number: address & 0x0F,
                        transfer_type: b[pos + 3] & 0x03,
                        max_packet_size: u16::from_le_bytes([b[pos + 4], b[pos + 5]])
                            & 0x07FF,
                        interval: b[pos + 6],
                    });
                }
            }
            // Class- or vendor-specific descriptors between interface
            // and endpoint records — ignored here, walked over by `len`.
            _ => {}
        }
        pos += len;
    }
    if let Some(it) = current.take() {
        interfaces.push(it);
    }

    Some(Configuration {
        total_length,
        num_interfaces,
        config_value,
        attributes,
        max_power_ma,
        interfaces,
    })
}

// =====================================================================
// Phase 4 sub-pass 7: Configure Endpoint + SET_CONFIGURATION.
// After this runs, the device's bulk / interrupt endpoints are valid
// from both the xHC side (Configure Endpoint allocated bandwidth and
// wired the transfer ring) and the device side (SET_CONFIGURATION put
// the device into the Configured state). Phase 5 (USB-MSC + SCSI) can
// then issue actual data transfers on those endpoints.
// =====================================================================

/// USB transfer type → xHCI EP Type encoding (xHCI spec table 6-9).
fn xhci_ep_type(usb_transfer_type: u8, direction_in: bool) -> u8 {
    match (usb_transfer_type, direction_in) {
        (1, false) => 1, // Isoch OUT
        (2, false) => 2, // Bulk OUT
        (3, false) => 3, // Interrupt OUT
        (0, _) => 4,     // Control (shouldn't appear in interface descriptors)
        (1, true) => 5,  // Isoch IN
        (2, true) => 6,  // Bulk IN
        (3, true) => 7,  // Interrupt IN
        _ => 0,          // Not Valid
    }
}

/// xHCI EP-context Interval encoding (spec §6.2.3.6): a value `N` meaning the
/// endpoint is serviced every `2^N × 125 µs`. Only periodic (Interrupt/Isoch)
/// endpoints use it; everything else returns 0.
fn xhci_interval(speed: u8, transfer_type: u8, b_interval: u8) -> u8 {
    if transfer_type != 3 && transfer_type != 1 {
        return 0; // Control/Bulk: Interval is ignored.
    }
    match speed {
        1 | 2 => {
            // Full(1)/Low(2) speed: bInterval is in 1 ms frames. Convert to
            // 125 µs units (×8), take floor(log2), clamp to the legal [3,10].
            let microframes = (b_interval.max(1) as u32) * 8;
            let mut n: u8 = 0;
            while (1u32 << (n + 1)) <= microframes && n < 10 {
                n += 1;
            }
            n.clamp(3, 10)
        }
        _ => {
            // High/Super speed: bInterval already encodes 2^(bInterval-1)
            // microframes, so the xHCI Interval is bInterval-1, clamped [0,15].
            (b_interval.max(1) - 1).min(15)
        }
    }
}

/// For every addressed device with a parsed Configuration, build the EP
/// contexts for every endpoint of its first interface, issue Configure
/// Endpoint (TRB type 12), and then SET_CONFIGURATION (control OUT,
/// no-data) to put the device into the Configured state.
pub fn configure_endpoints(
    dev: &PciDevice,
    info: &XhciInfo,
) -> Result<Vec<AddressedDevice>, &'static str> {
    let _ = dev;
    let mut guard = STATES.lock();
    let st =
        state_for(&mut guard, info.mmio_base).ok_or("bring up the controller first ([b])")?;
    if !info.mmio_accessible {
        return Err("MMIO not accessible");
    }
    if st.addressed.is_empty() {
        return Err("address devices first ([a])");
    }
    let entry_size = if info.csz_64 { 64usize } else { 32 };

    let n = st.addressed.len();
    for i in 0..n {
        if st.addressed[i].set_config_cc == 1 {
            continue; // Already configured this pass.
        }
        let cfg = match &st.addressed[i].config {
            Some(c) => c.clone(),
            None => continue,
        };
        if cfg.interfaces.is_empty() {
            continue;
        }
        let slot_id = st.addressed[i].slot_id;
        let slot_idx = match st.slots.iter().position(|s| s.slot_id == slot_id) {
            Some(idx) => idx,
            None => continue,
        };
        let mut res = st.slots[slot_idx].clone();

        // Configure the endpoints of EVERY interface (alt 0), not just the
        // first — a composite device (e.g. a combo keyboard+mouse: keyboard on
        // interface 0, mouse on interface 1) needs both interfaces' endpoints
        // wired, or the second function (the mouse) has no transfer ring and
        // `probe_hid_*` would otherwise fall back to the wrong interface's
        // endpoint. Endpoint numbers are device-unique, so DCIs never collide.
        let mut new_endpoints: Vec<EndpointState> = Vec::new();
        let mut summary: Vec<ConfiguredEndpoint> = Vec::new();
        let mut max_dci: u8 = 1;
        let mut add_flags: u32 = 1; // A0 = 1 (slot context always re-evaluated)
        let input_ctx_ptr = res.input_ctx as *mut u8;

        for ep in cfg
            .interfaces
            .iter()
            .filter(|i| i.alt_setting == 0)
            .flat_map(|i| i.endpoints.iter().map(move |e| (i.number, e)))
        {
            let (iface_num, ep) = ep;
            let dci = 2 * ep.number + (if ep.direction_in { 1 } else { 0 });
            let ep_type = xhci_ep_type(ep.transfer_type, ep.direction_in);
            if ep_type == 0 {
                continue; // Skip "Not Valid" combos.
            }
            // Allocate this endpoint's own transfer ring with a Link TRB.
            let tr = alloc_dma(256 * 16, 4096);
            unsafe {
                let link = tr.add(255 * 16);
                write_volatile(link as *mut u64, tr as u64);
                write_volatile(link.add(8) as *mut u32, 0);
                write_volatile(
                    link.add(12) as *mut u32,
                    (6u32 << 10) | (1u32 << 1) | 1,
                );
            }

            // EP Context lives at Input Context entry `dci + 1`
            // (entries: 0=Control, 1=Slot, 2=DCI1 (EP0), 3=DCI2 (EP1 OUT), …).
            let ep_ctx = unsafe { input_ctx_ptr.add(entry_size * (dci as usize + 1)) };
            // Zero the context first so leftover bytes can't bias the HC.
            for off in (0..entry_size).step_by(4) {
                unsafe { write_volatile(ep_ctx.add(off) as *mut u32, 0) };
            }
            // DW0: Interval [23:16] for periodic (Interrupt/Isoch) endpoints so
            // the controller actually schedules them — a HID mouse's interrupt-IN
            // EP needs this. Bulk/Control ignore Interval, so it stays 0 for the
            // MSC path. Max ESIT Payload Hi [31:24] left 0 (HID reports are tiny).
            let interval = xhci_interval(res.speed, ep.transfer_type, ep.interval);
            unsafe { write_volatile(ep_ctx as *mut u32, (interval as u32) << 16) };
            // DW1: CErr=3 in [2:1], EP Type in [5:3], Max Packet Size in [31:16].
            let dw1: u32 = (3u32 << 1)
                | ((ep_type as u32) << 3)
                | ((ep.max_packet_size as u32) << 16);
            unsafe { write_volatile(ep_ctx.add(4) as *mut u32, dw1) };
            // DW2..3 (u64): TR Dequeue Pointer | DCS=1.
            unsafe { write_volatile(ep_ctx.add(8) as *mut u64, tr as u64 | 1) };
            // DW4: Average TRB Length [15:0]; for periodic endpoints also Max
            // ESIT Payload Lo [31:16], so the controller reserves bandwidth.
            let esit: u32 = if ep.transfer_type == 3 || ep.transfer_type == 1 {
                ep.max_packet_size as u32
            } else {
                0
            };
            unsafe {
                write_volatile(
                    ep_ctx.add(16) as *mut u32,
                    (ep.max_packet_size as u32) | (esit << 16),
                )
            };

            new_endpoints.push(EndpointState {
                address: ep.address,
                dci,
                transfer_type: ep.transfer_type,
                max_packet_size: ep.max_packet_size,
                direction_in: ep.direction_in,
                interface_number: iface_num,
                tr_ring: tr as u64,
                tr_enqueue: 0,
                tr_pcs: 1,
            });
            summary.push(ConfiguredEndpoint {
                address: ep.address,
                dci,
                direction_in: ep.direction_in,
                transfer_type: ep.transfer_type,
                max_packet_size: ep.max_packet_size,
            });
            add_flags |= 1u32 << dci;
            if dci > max_dci {
                max_dci = dci;
            }
        }

        if new_endpoints.is_empty() {
            continue;
        }

        // Input Control Context: drop = 0, add = computed flags.
        unsafe {
            write_volatile(input_ctx_ptr as *mut u32, 0);
            write_volatile(input_ctx_ptr.add(4) as *mut u32, add_flags);
        }
        // Slot Context DW0: bump Context Entries (bits 27..31) to `max_dci`
        // without disturbing Speed / Hub / Route String.
        let slot_ctx = unsafe { input_ctx_ptr.add(entry_size) };
        unsafe {
            let dw0 = read_volatile(slot_ctx as *const u32);
            let new_dw0 = (dw0 & !(0x1Fu32 << 27)) | ((max_dci as u32) << 27);
            write_volatile(slot_ctx as *mut u32, new_dw0);
        }

        // Issue Configure Endpoint Command (TRB type 12).
        let cmd_ring = st.bringup.cmd_ring_addr as *mut u8;
        let trb = unsafe { cmd_ring.add(st.rings.cmd_enqueue * 16) };
        let control: u32 = (12u32 << 10)
            | (st.rings.cmd_pcs as u32)
            | ((slot_id as u32) << 24);
        unsafe {
            write_volatile(trb as *mut u64, res.input_ctx);
            write_volatile(trb.add(8) as *mut u32, 0);
            write_volatile(trb.add(12) as *mut u32, control);
        }
        advance_cmd_enqueue(st);
        let db = (info.mmio_base + info.dboff as u64) as *mut u32;
        unsafe { write_volatile(db, 0) };

        let cfg_ep_cc = match drain_event(info, st, 33, 1_000_000) {
            Ok(ev) => ev.completion_code,
            Err(e) => {
                serial_println!(
                    "xhci: slot {} Configure Endpoint failed: {}",
                    slot_id, e
                );
                0
            }
        };
        serial_println!(
            "xhci: slot {} Configure Endpoint cc={} ({} endpoints, max DCI {})",
            slot_id,
            cfg_ep_cc,
            new_endpoints.len(),
            max_dci
        );

        if cfg_ep_cc == 1 {
            res.endpoints = new_endpoints;
        }

        // SET_CONFIGURATION (USB 9.4.7): OUT, standard, device, no data.
        let mut set_cc = 0u8;
        if cfg_ep_cc == 1 {
            match control_transfer(
                info,
                st,
                &mut res,
                0x00,
                0x09,
                cfg.config_value as u16,
                0,
                0,
            ) {
                Ok((cc, _)) => {
                    set_cc = cc;
                    serial_println!(
                        "xhci: slot {} SET_CONFIGURATION({}) cc={}",
                        slot_id, cfg.config_value, set_cc
                    );
                }
                Err(e) => {
                    serial_println!(
                        "xhci: slot {} SET_CONFIGURATION failed: {}",
                        slot_id, e
                    );
                }
            }
        }

        st.slots[slot_idx] = res;
        st.addressed[i].configure_endpoint_cc = cfg_ep_cc;
        st.addressed[i].set_config_cc = set_cc;
        st.addressed[i].configured_endpoints = summary;
    }

    Ok(st.addressed.clone())
}

pub fn transfer_type_name(t: u8) -> &'static str {
    match t & 0x03 {
        0 => "Control",
        1 => "Isochronous",
        2 => "Bulk",
        3 => "Interrupt",
        _ => "?",
    }
}

// =====================================================================
// Phase 5: USB Mass Storage Class (Bulk-Only Transport) + minimum SCSI.
//
// Layered on top of the per-endpoint transfer rings allocated by
// sub-pass 7. One MSC command = three bulk transfers:
//   1. CBW (Command Block Wrapper, 31 B) on bulk OUT
//   2. optional data stage on bulk IN or bulk OUT
//   3. CSW (Command Status Wrapper, 13 B) on bulk IN
//
// SCSI subset implemented:
//   * INQUIRY (0x12)
//   * TEST UNIT READY (0x00)
//   * READ CAPACITY(10) (0x25)
//   * READ(10) (0x28)
// =====================================================================

const CBW_SIG: u32 = 0x43425355; // "USBC"
const CSW_SIG: u32 = 0x53425355; // "USBS"
const CBW_LEN: usize = 31;
const CSW_LEN: usize = 13;

#[derive(Clone)]
pub struct InquiryData {
    pub peripheral_device_type: u8, // bits 0..4 of byte 0 (0 = direct-access block device)
    pub removable: bool,
    pub vendor: alloc::string::String,
    pub product: alloc::string::String,
    pub revision: alloc::string::String,
}

#[derive(Clone, Copy)]
pub struct CapacityData {
    pub last_lba: u32,
    pub block_size: u32,
}

impl CapacityData {
    pub fn total_blocks(&self) -> u64 {
        self.last_lba as u64 + 1
    }
    pub fn total_bytes(&self) -> u64 {
        self.total_blocks() * self.block_size as u64
    }
}

#[derive(Clone)]
pub struct MscProbe {
    /// MMIO base of the xHCI controller this device lives on. Slot ids are
    /// only unique *per controller*, so on a machine with several xHCIs (PCH +
    /// add-in card) two drives can share slot id 1; this disambiguates them so
    /// I/O is routed to the right controller. See [`state_for`].
    pub mmio_base: u64,
    pub slot_id: u8,
    pub interface_number: u8,
    pub get_max_lun_cc: u8,
    pub max_lun: u8,
    pub inquiry: Option<InquiryData>,
    pub inquiry_scsi_status: u8,
    pub tur_scsi_status: u8,
    pub capacity: Option<CapacityData>,
    pub capacity_scsi_status: u8,
    /// First 512-byte block (LBA 0) read off the device. Trimmed to
    /// `block_size` if the block is smaller.
    pub first_block: Vec<u8>,
    pub read_scsi_status: u8,
}

/// Per-call accessor for the most recent MSC probe. Lives in `XhciState`.
pub fn current_msc() -> Vec<MscProbe> {
    STATES
        .lock()
        .iter()
        .flat_map(|s| s.msc.iter().cloned())
        .collect()
}

/// For every addressed + configured Mass-Storage device, send INQUIRY,
/// TEST UNIT READY, READ CAPACITY(10), and READ(10) of LBA 0; cache the
/// outcome.
pub fn probe_mass_storage(
    dev: &PciDevice,
    info: &XhciInfo,
) -> Result<Vec<MscProbe>, &'static str> {
    let _ = dev;
    let mut guard = STATES.lock();
    let st =
        state_for(&mut guard, info.mmio_base).ok_or("bring up the controller first ([b])")?;
    if !info.mmio_accessible {
        return Err("MMIO not accessible");
    }
    if st.addressed.is_empty() {
        return Err("address + configure devices first ([a] [c] [g])");
    }

    let mut tag: u32 = 0xABCD_0000;
    let mut probes: Vec<MscProbe> = Vec::new();

    // Snapshot just enough of `addressed` so we don't have to keep
    // re-borrowing across SCSI calls.
    let candidates: Vec<(u8, u8)> = st
        .addressed
        .iter()
        .filter(|d| d.set_config_cc == 1)
        .filter_map(|d| {
            d.config.as_ref().and_then(|c| {
                c.interfaces.iter().find_map(|ifd| {
                    if ifd.class == 0x08
                        && ifd.subclass == 0x06
                        && ifd.protocol == 0x50
                    {
                        Some((d.slot_id, ifd.number))
                    } else {
                        None
                    }
                })
            })
        })
        .collect();

    for (slot_id, interface_number) in candidates {
        let slot_idx = match st.slots.iter().position(|s| s.slot_id == slot_id) {
            Some(idx) => idx,
            None => continue,
        };
        let bulk_in_idx = st.slots[slot_idx]
            .endpoints
            .iter()
            .position(|e| e.direction_in && e.transfer_type == 2);
        let bulk_out_idx = st.slots[slot_idx]
            .endpoints
            .iter()
            .position(|e| !e.direction_in && e.transfer_type == 2);
        let (Some(bulk_in_idx), Some(bulk_out_idx)) = (bulk_in_idx, bulk_out_idx)
        else {
            serial_println!(
                "xhci: slot {} has MSC interface but no bulk pair",
                slot_id
            );
            continue;
        };

        let mut probe = MscProbe {
            mmio_base: info.mmio_base,
            slot_id,
            interface_number,
            get_max_lun_cc: 0,
            max_lun: 0,
            inquiry: None,
            inquiry_scsi_status: 0xFF,
            tur_scsi_status: 0xFF,
            capacity: None,
            capacity_scsi_status: 0xFF,
            first_block: Vec::new(),
            read_scsi_status: 0xFF,
        };

        // ---- GET MAX LUN (class request, control IN) ---------------
        let mut res = st.slots[slot_idx].clone();
        match control_transfer(
            info,
            st,
            &mut res,
            0xA1,
            0xFE,
            0,
            interface_number as u16,
            1,
        ) {
            Ok((cc, bytes)) => {
                probe.get_max_lun_cc = cc;
                probe.max_lun = bytes.first().copied().unwrap_or(0);
                serial_println!(
                    "xhci: slot {} GET_MAX_LUN cc={} max_lun={}",
                    slot_id, cc, probe.max_lun
                );
            }
            Err(e) => {
                // Some devices stall this; treat as max_lun=0 per spec.
                probe.get_max_lun_cc = 6; // pretend Stall
                serial_println!("xhci: slot {} GET_MAX_LUN: {}", slot_id, e);
            }
        }
        st.slots[slot_idx] = res;

        let lun = 0u8;

        // ---- SCSI INQUIRY (36 B) ----------------------------------
        tag = tag.wrapping_add(1);
        match scsi_inquiry(info, st, slot_idx, bulk_in_idx, bulk_out_idx, lun, tag) {
            Ok((scsi_status, bytes)) => {
                probe.inquiry_scsi_status = scsi_status;
                if scsi_status == 0 && bytes.len() >= 36 {
                    probe.inquiry = Some(parse_inquiry(&bytes));
                }
            }
            Err(e) => {
                serial_println!("xhci: slot {} INQUIRY failed: {}", slot_id, e);
            }
        }

        // ---- TEST UNIT READY ---------------------------------------
        tag = tag.wrapping_add(1);
        match scsi_test_unit_ready(info, st, slot_idx, bulk_in_idx, bulk_out_idx, lun, tag) {
            Ok(scsi_status) => probe.tur_scsi_status = scsi_status,
            Err(e) => {
                serial_println!("xhci: slot {} TUR failed: {}", slot_id, e);
            }
        }

        // ---- READ CAPACITY(10) (8 B) -------------------------------
        tag = tag.wrapping_add(1);
        match scsi_read_capacity10(info, st, slot_idx, bulk_in_idx, bulk_out_idx, lun, tag) {
            Ok((scsi_status, bytes)) => {
                probe.capacity_scsi_status = scsi_status;
                if scsi_status == 0 && bytes.len() >= 8 {
                    probe.capacity = Some(CapacityData {
                        last_lba: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                        block_size: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
                    });
                }
            }
            Err(e) => {
                serial_println!("xhci: slot {} READ CAPACITY(10) failed: {}", slot_id, e);
            }
        }

        // ---- READ(10) LBA 0, 1 block -------------------------------
        if let Some(cap) = probe.capacity {
            tag = tag.wrapping_add(1);
            match scsi_read10(
                info,
                st,
                slot_idx,
                bulk_in_idx,
                bulk_out_idx,
                lun,
                0,
                1,
                cap.block_size,
                tag,
            ) {
                Ok((scsi_status, bytes)) => {
                    probe.read_scsi_status = scsi_status;
                    if scsi_status == 0 {
                        probe.first_block = bytes;
                    }
                }
                Err(e) => {
                    serial_println!("xhci: slot {} READ(10) failed: {}", slot_id, e);
                }
            }
        }

        probes.push(probe);
    }

    st.msc = probes.clone();
    Ok(probes)
}

// ---- Bulk transfer primitive ------------------------------------------

/// Issue one Normal Transfer TRB on `st.slots[slot_idx].endpoints[ep_idx]`'s
/// transfer ring, ring the slot's doorbell with the endpoint's DCI, and
/// drain the matching Transfer Event. Returns `(completion_code, residue)`.
/// Enqueue one Normal Transfer TRB (Cycle | ISP | IOC, type 1) on
/// `slots[slot_idx].endpoints[ep_idx]`'s ring, handling Link-TRB wrap, and
/// return `(slot_id, dci)` so the caller can ring the doorbell. Does not drain —
/// shared by `bulk_transfer` (which then waits for completion) and the mouse arm
/// (which leaves the TRB pending and checks for it on a later poll).
/// Post a single Normal TRB to an endpoint's transfer ring. Returns
/// `(slot_id, dci, trb_addr)` — the TRB's address is the physical address the
/// controller reports back as the Transfer Event's TRB Pointer, so a bulk
/// caller can match its completion *exactly* (see `drain_transfer_event`).
fn post_normal_trb(
    st: &mut XhciState,
    slot_idx: usize,
    ep_idx: usize,
    buf: u64,
    length: u32,
) -> (u8, u8, u64) {
    let slot_id = st.slots[slot_idx].slot_id;
    let ep = &mut st.slots[slot_idx].endpoints[ep_idx];
    let dci = ep.dci;
    let tr_ring = ep.tr_ring as *mut u8;
    let trb = unsafe { tr_ring.add(ep.tr_enqueue * 16) };
    unsafe {
        write_volatile(trb as *mut u64, buf);
        // Status: length in [16:0], TD Size = 0, Interrupter = 0.
        write_volatile(trb.add(8) as *mut u32, length);
        // Control: Cycle | ISP=1 | IOC=1 | TRB type=1 (Normal).
        write_volatile(
            trb.add(12) as *mut u32,
            (ep.tr_pcs as u32) | (1u32 << 2) | (1u32 << 5) | (1u32 << 10),
        );
    }
    ep.tr_enqueue += 1;
    if ep.tr_enqueue == 255 {
        let link = unsafe { tr_ring.add(255 * 16) };
        unsafe {
            let ctl = read_volatile(link.add(12) as *const u32);
            let new_ctl = (ctl & !1) | ep.tr_pcs as u32;
            write_volatile(link.add(12) as *mut u32, new_ctl);
        }
        ep.tr_enqueue = 0;
        ep.tr_pcs ^= 1;
    }
    (slot_id, dci, trb as u64)
}

fn bulk_transfer(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_idx: usize,
    ep_idx: usize,
    buf: *mut u8,
    length: u32,
) -> Result<(u8, u32), &'static str> {
    let (slot_id, dci, trb) = post_normal_trb(st, slot_idx, ep_idx, buf as u64, length);
    // Ring the doorbell after the &mut borrow inside `post_normal_trb` is dropped.
    let db = (info.mmio_base + info.dboff as u64 + 4 * slot_id as u64) as *mut u32;
    unsafe { write_volatile(db, dci as u32) };
    // Match this bulk completion by the *exact* TRB we posted: it's a single
    // Normal TRB, so both its success AND error events carry this TRB Pointer.
    // This excludes HID reports and — critically — a stale completion from a
    // previous transfer on this same endpoint that timed out or was reset and
    // landed late (which slot+endpoint matching would wrongly accept, handing
    // back a wrong length + garbage buffer: "corrupt store: chain length").
    let ev = drain_transfer_event(info, st, slot_id, dci, Some(trb), 2_000_000)?;
    Ok((ev.completion_code, ev.transfer_length))
}

// ---- MSC BOT framing --------------------------------------------------

#[derive(Clone, Copy)]
struct MscOutcome {
    /// CSW `bCSWStatus`: 0 = Passed, 1 = Failed, 2 = Phase Error.
    scsi_status: u8,
    /// CSW `dCSWDataResidue`: bytes of the declared transfer NOT moved. Nonzero
    /// means a short data stage — the buffer tail is not real device data.
    residue: u32,
}

/// Send one MSC Bulk-Only Transport command: CBW → optional data → CSW.
/// Returns the SCSI status from the CSW; the caller's `data` buffer is
/// filled in-place on IN transfers.
fn msc_command(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_idx: usize,
    bulk_in_idx: usize,
    bulk_out_idx: usize,
    lun: u8,
    cb: &[u8],
    data: Option<(*mut u8, u32, bool)>, // (buf, length, direction_in)
    tag: u32,
) -> Result<MscOutcome, &'static str> {
    if cb.is_empty() || cb.len() > 16 {
        return Err("invalid SCSI CB length");
    }

    // ---- Build CBW -------------------------------------------------
    let cbw = DmaBuffer::new(CBW_LEN, 4096);
    let (data_len, dir_in) = match data {
        Some((_, l, d)) => (l, d),
        None => (0, false),
    };
    unsafe {
        write_volatile(cbw.ptr() as *mut u32, CBW_SIG);
        write_volatile(cbw.ptr().add(4) as *mut u32, tag);
        write_volatile(cbw.ptr().add(8) as *mut u32, data_len);
    }
    cbw.write_byte(12, if dir_in { 0x80 } else { 0x00 });
    cbw.write_byte(13, lun);
    cbw.write_byte(14, cb.len() as u8);
    cbw.write_slice(15, cb);
    // Trailing bytes beyond `cb.len()` are already zero (alloc_zeroed).

    // ---- CBW transfer (bulk OUT) ----------------------------------
    let (cc, _) = bulk_transfer(info, st, slot_idx, bulk_out_idx, cbw.ptr(), CBW_LEN as u32)?;
    if cc != 1 {
        // Clear the endpoint so the next command isn't blocked by a halt.
        reset_bulk_endpoint(info, st, slot_idx, bulk_out_idx);
        return Err("CBW transfer xHCI cc != Success");
    }

    // ---- Optional data stage --------------------------------------
    if let Some((data_buf, data_len, dir_in)) = data {
        let ep_idx = if dir_in { bulk_in_idx } else { bulk_out_idx };
        let (cc, _) = bulk_transfer(info, st, slot_idx, ep_idx, data_buf, data_len)?;
        if cc != 1 && cc != 13 {
            // BOT recovery: a stalled data stage halts this endpoint, but the
            // device still owes us a CSW — clear the halt and read it below.
            reset_bulk_endpoint(info, st, slot_idx, ep_idx);
        }
    }

    // ---- CSW transfer (bulk IN), with one stall-recovery retry ----
    let csw = DmaBuffer::new(CSW_LEN, 4096);
    let mut got_csw = false;
    for _ in 0..2 {
        let (cc, _) =
            bulk_transfer(info, st, slot_idx, bulk_in_idx, csw.ptr(), CSW_LEN as u32)?;
        if cc == 1 || cc == 13 {
            got_csw = true;
            break;
        }
        // Stall/error on the status stage: clear it and retry once.
        reset_bulk_endpoint(info, st, slot_idx, bulk_in_idx);
    }
    if !got_csw {
        return Err("CSW transfer xHCI cc != Success/Short Packet");
    }

    let sig = csw.read_u32_le(0);
    let csw_tag = csw.read_u32_le(4);
    let residue = csw.read_u32_le(8);
    let scsi_status = csw.read_byte(12);
    if sig != CSW_SIG {
        return Err("bad CSW signature");
    }
    if csw_tag != tag {
        return Err("CSW tag mismatch");
    }
    Ok(MscOutcome {
        scsi_status,
        residue,
    })
    // `cbw` and `csw` drop here; their DMA backing is freed.
}

// ---- Minimum SCSI command set -----------------------------------------

fn scsi_inquiry(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_idx: usize,
    bulk_in_idx: usize,
    bulk_out_idx: usize,
    lun: u8,
    tag: u32,
) -> Result<(u8, Vec<u8>), &'static str> {
    let cb: [u8; 6] = [0x12, 0, 0, 0, 36, 0];
    let buf = DmaBuffer::new(36, 4096);
    let outcome = msc_command(
        info,
        st,
        slot_idx,
        bulk_in_idx,
        bulk_out_idx,
        lun,
        &cb,
        Some((buf.ptr(), 36, true)),
        tag,
    )?;
    let data = if outcome.scsi_status == 0 {
        buf.read_to_vec(36)
    } else {
        Vec::new()
    };
    Ok((outcome.scsi_status, data))
}

fn scsi_test_unit_ready(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_idx: usize,
    bulk_in_idx: usize,
    bulk_out_idx: usize,
    lun: u8,
    tag: u32,
) -> Result<u8, &'static str> {
    let cb: [u8; 6] = [0; 6];
    let outcome = msc_command(
        info, st, slot_idx, bulk_in_idx, bulk_out_idx, lun, &cb, None, tag,
    )?;
    Ok(outcome.scsi_status)
}

fn scsi_read_capacity10(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_idx: usize,
    bulk_in_idx: usize,
    bulk_out_idx: usize,
    lun: u8,
    tag: u32,
) -> Result<(u8, Vec<u8>), &'static str> {
    let cb: [u8; 10] = [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let buf = DmaBuffer::new(8, 4096);
    let outcome = msc_command(
        info,
        st,
        slot_idx,
        bulk_in_idx,
        bulk_out_idx,
        lun,
        &cb,
        Some((buf.ptr(), 8, true)),
        tag,
    )?;
    let data = if outcome.scsi_status == 0 {
        buf.read_to_vec(8)
    } else {
        Vec::new()
    };
    Ok((outcome.scsi_status, data))
}

fn scsi_read10(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_idx: usize,
    bulk_in_idx: usize,
    bulk_out_idx: usize,
    lun: u8,
    lba: u32,
    blocks: u16,
    block_size: u32,
    tag: u32,
) -> Result<(u8, Vec<u8>), &'static str> {
    let cb: [u8; 10] = [
        0x28,
        0,
        (lba >> 24) as u8,
        (lba >> 16) as u8,
        (lba >> 8) as u8,
        lba as u8,
        0,
        (blocks >> 8) as u8,
        blocks as u8,
        0,
    ];
    let total = blocks as u32 * block_size;
    let buf = DmaBuffer::new(total as usize, 4096);
    let outcome = msc_command(
        info,
        st,
        slot_idx,
        bulk_in_idx,
        bulk_out_idx,
        lun,
        &cb,
        Some((buf.ptr(), total, true)),
        tag,
    )?;
    let data = if outcome.scsi_status == 0 {
        // Honour the CSW residue. A short data stage transferred fewer than
        // `total` bytes, and the unfilled tail of the zeroed DMA buffer is NOT
        // sector data — returning it would feed zero-padded garbage to the
        // store (and a later read-modify-write would persist that garbage,
        // corrupting the volume: "corrupt store: varint eof"). Hand back only
        // the bytes actually transferred so `msc_read_sector` sees a short read,
        // resets the endpoints, and retries instead of trusting the padding.
        let got = (total as usize).saturating_sub(outcome.residue as usize);
        buf.read_to_vec(got)
    } else {
        Vec::new()
    };
    Ok((outcome.scsi_status, data))
}

#[allow(dead_code)] // Will be used by the upcoming `BlockDevice` wrapper.
fn scsi_write10(
    info: &XhciInfo,
    st: &mut XhciState,
    slot_idx: usize,
    bulk_in_idx: usize,
    bulk_out_idx: usize,
    lun: u8,
    lba: u32,
    blocks: u16,
    block_size: u32,
    data: &[u8],
    tag: u32,
) -> Result<u8, &'static str> {
    let total = blocks as u32 * block_size;
    if data.len() < total as usize {
        return Err("WRITE(10): payload smaller than declared transfer length");
    }
    // Copy the payload into a DMA-aligned buffer.
    let buf = DmaBuffer::new(total as usize, 4096);
    buf.write_slice(0, &data[..total as usize]);

    // Durability: ask for Force Unit Access (CDB byte 1 bit 3) so the write
    // reaches the medium before the CSW — the journal's commit guarantee must
    // not depend on SYNCHRONIZE CACHE, which `flush` treats as best-effort
    // because cheap sticks routinely stall it. Devices that reject FUA
    // (CHECK CONDITION / stall) get one plain retry and a sticky opt-out.
    let mut use_fua = !FUA_UNSUPPORTED.load(Ordering::Relaxed);
    loop {
        let cb: [u8; 10] = [
            0x2A,
            if use_fua { 0x08 } else { 0x00 },
            (lba >> 24) as u8,
            (lba >> 16) as u8,
            (lba >> 8) as u8,
            lba as u8,
            0,
            (blocks >> 8) as u8,
            blocks as u8,
            0,
        ];
        let result = msc_command(
            info,
            st,
            slot_idx,
            bulk_in_idx,
            bulk_out_idx,
            lun,
            &cb,
            Some((buf.ptr(), total, false)),
            tag,
        );
        match result {
            Ok(outcome) if outcome.scsi_status == 0 && outcome.residue == 0 => return Ok(0),
            // Passed but short: the medium did not receive the whole sector.
            // Surface it as an error (msc_write_sector retries) rather than
            // reporting success for a partial write that corrupts the volume.
            Ok(outcome) if outcome.scsi_status == 0 => {
                return Err("WRITE(10) short transfer (CSW residue != 0)")
            }
            other if use_fua => {
                // Could be a device that doesn't know FUA — never retry with
                // it again, and re-issue this write plain.
                FUA_UNSUPPORTED.store(true, Ordering::Relaxed);
                serial_println!(
                    "xhci: WRITE(10)+FUA refused ({:?}); falling back to plain writes",
                    other.as_ref().map(|o| o.scsi_status)
                );
                use_fua = false;
            }
            Ok(outcome) => return Ok(outcome.scsi_status),
            Err(e) => return Err(e),
        }
    }
}

/// Latched once a device rejects WRITE(10)+FUA; all later writes go plain.
/// (Per-machine, not per-device — TablesOS only ever writes its boot disk,
/// plus the explicit install target.)
static FUA_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

fn parse_inquiry(b: &[u8]) -> InquiryData {
    let trim = |s: &str| -> alloc::string::String {
        s.trim_end_matches(' ').trim_end_matches('\0').into()
    };
    let ascii_slice = |start: usize, len: usize| -> alloc::string::String {
        let mut s = alloc::string::String::with_capacity(len);
        for i in 0..len {
            let c = b.get(start + i).copied().unwrap_or(b' ');
            if (b' '..=b'~').contains(&c) {
                s.push(c as char);
            } else {
                s.push(' ');
            }
        }
        trim(&s)
    };
    InquiryData {
        peripheral_device_type: b[0] & 0x1F,
        removable: b.get(1).copied().unwrap_or(0) & 0x80 != 0,
        vendor: ascii_slice(8, 8),
        product: ascii_slice(16, 16),
        revision: ascii_slice(32, 4),
    }
}

// =====================================================================
// Phase 6: surface successful USB-MSC probes in the Drives screen as
// regular `DriveInfo` entries alongside the IDE list. Also provides a
// one-shot `autopilot_usb_drives` that runs every previous phase and
// returns whatever USB block storage it discovered — so the Drives
// screen can re-run the whole pipeline with a single key press.
// =====================================================================

/// Build a `DriveInfo` for every cached MSC probe that has at least an
/// INQUIRY + READ CAPACITY result. The MBR classification reuses
/// `ata::parse_mbr` on the LBA-0 bytes we already pulled off the device.
pub fn usb_drives(booted_sys_guid: &[u8; 16]) -> Vec<DriveInfo> {
    usb_drives_with_slots(booted_sys_guid)
        .into_iter()
        .map(|(_, _, d)| d)
        .collect()
}

/// Same as `usb_drives`, but each row carries the owning controller's MMIO
/// base and the xHCI slot ID. Used by the boot-disk selection (which must
/// reach the exact controller — two xHCIs can share a slot id) and the
/// install-to-USB picker.
pub fn usb_drives_with_slots(booted_sys_guid: &[u8; 16]) -> Vec<(u64, u8, DriveInfo)> {
    let probes = current_msc();
    let addressed = current_addressed();
    let mut out = Vec::new();
    for p in &probes {
        let inquiry = match &p.inquiry {
            Some(i) => i,
            None => continue,
        };
        let capacity = match &p.capacity {
            Some(c) => *c,
            None => continue,
        };
        let port = addressed
            .iter()
            .find(|a| a.slot_id == p.slot_id)
            .map(|a| a.port)
            .unwrap_or(0);
        let mbr = if p.first_block.len() >= 512 {
            ata::parse_mbr(&p.first_block)
        } else {
            MbrInfo::Unreadable
        };
        let boot_sig_ok = p.first_block.len() >= 512
            && p.first_block[510] == 0x55
            && p.first_block[511] == 0xAA;
        let booted = match &mbr {
            MbrInfo::TablesOs { sys_guid, .. } => sys_guid == booted_sys_guid,
            _ => false,
        };
        let model = if inquiry.vendor.is_empty() {
            inquiry.product.clone()
        } else {
            alloc::format!("{} {}", inquiry.vendor, inquiry.product)
        };
        out.push((
            p.mmio_base,
            p.slot_id,
            DriveInfo {
                slot: alloc::format!("USB slot {} (xHCI port {})", p.slot_id, port),
                present: true,
                model,
                serial: alloc::string::String::new(),
                firmware: inquiry.revision.clone(),
                lba28_sectors: capacity.total_blocks(),
                lba48_sectors: 0,
                boot_sig_ok,
                mbr,
                booted,
            },
        ));
    }
    out
}

/// Run the entire USB pipeline (every xHCI controller found in PCI:
/// inspect → bring-up → port reset + Enable Slot → Address Device +
/// Device Descriptor → Eval Context + Configuration Descriptor →
/// Configure Endpoint + SET_CONFIGURATION → MSC probe) and return the
/// resulting USB drives. Each step is idempotent, so calling this
/// repeatedly is safe and only does new work as devices appear.
// =====================================================================
// USB-HID boot mouse. Built on the same enumeration/transfer machinery as
// the mass-storage path: once a device's interrupt-IN endpoint is wired up
// (configure_endpoints), put it in Boot Protocol and keep one interrupt
// transfer armed, polling cooperatively from the UI loop.
// See solved-issues/USB mouse on real hardware.md.
// =====================================================================

/// Relative movement + current button state, accumulated by `service_mouse`
/// and forwarded by `pump_hid`.
#[derive(Clone, Copy)]
pub struct MouseDelta {
    pub dx: i32,
    pub dy: i32,
    pub left: bool,
    pub right: bool,
}

/// Find an addressed + configured device exposing a HID **boot mouse** interface
/// (class 0x03 / subclass 0x01 / protocol 0x02) with a configured interrupt-IN
/// endpoint, put it in Boot Protocol (fixed 3-byte report — no HID report
/// descriptor parsing needed), request idle-on-change, and arm the first
/// transfer. Records it as `st.mouse`. Returns whether a mouse was set up.
/// Idempotent: a second call with a mouse already bound is a no-op success.
pub fn probe_hid_mouse(dev: &PciDevice, info: &XhciInfo) -> Result<bool, &'static str> {
    let _ = dev;
    let mut guard = STATES.lock();
    let st = state_for(&mut guard, info.mmio_base).ok_or("bring up the controller first")?;
    if !info.mmio_accessible {
        return Err("MMIO not accessible");
    }

    // EVERY configured device's boot-mouse interface(s) — there can be more
    // than one (a keyboard and a wireless-mouse receiver each expose a mouse
    // interface; only one has a physical mouse, but we can't tell which, so
    // bind and poll them all — the idle one never reports).
    let candidates: Vec<(u8, u8)> = st
        .addressed
        .iter()
        .filter(|d| d.set_config_cc == 1)
        .flat_map(|d| {
            d.config
                .as_ref()
                .map(|c| {
                    c.interfaces
                        .iter()
                        .filter(|i| i.class == 0x03 && i.subclass == 0x01 && i.protocol == 0x02)
                        .map(|i| (d.slot_id, i.number))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();

    let mut bound = false;
    for (slot_id, interface_number) in candidates {
        let Some(slot_idx) = st.slots.iter().position(|s| s.slot_id == slot_id) else {
            continue;
        };
        let Some(ep_idx) = st.slots[slot_idx].endpoints.iter().position(|e| {
            e.direction_in && e.transfer_type == 3 && e.interface_number == interface_number
        }) else {
            continue;
        };
        let dci = st.slots[slot_idx].endpoints[ep_idx].dci;
        // Skip if already bound (idempotent across repeated setup calls).
        if st.mice.iter().any(|m| m.slot_id == slot_id && m.dci == dci) {
            bound = true;
            continue;
        }

        let mut res = st.slots[slot_idx].clone();
        if let Err(e) = control_transfer(info, st, &mut res, 0x21, 0x0B, 0, interface_number as u16, 0) {
            serial_println!("xhci: slot {} mouse SET_PROTOCOL failed: {}", slot_id, e);
            st.slots[slot_idx] = res;
            continue;
        }
        let _ = control_transfer(info, st, &mut res, 0x21, 0x0A, 0, interface_number as u16, 0);
        st.slots[slot_idx] = res;

        let report_len =
            (st.slots[slot_idx].endpoints[ep_idx].max_packet_size as u32).clamp(4, 8);
        let report_buf = alloc_dma(report_len as usize, 64) as u64;
        st.mice.push(MouseDevice {
            slot_idx,
            ep_idx,
            slot_id,
            dci,
            report_buf,
            report_len,
            armed: false,
            needs_reset: false,
            accum_dx: 0,
            accum_dy: 0,
            buttons: 0,
            dirty: false,
        });
        let idx = st.mice.len() - 1;
        arm_mouse(info, st, idx);
        serial_println!(
            "xhci: HID boot mouse armed (slot {}, if {}, ep dci {}, {} B reports)",
            slot_id, interface_number, dci, report_len
        );
        bound = true;
    }
    Ok(bound)
}

/// Find a configured device exposing a HID **boot keyboard** interface
/// (class 3 / subclass 1 / protocol 1) with a configured interrupt-IN
/// endpoint *on that interface*, put it in Boot Protocol, SET_IDLE(0), and
/// arm the first transfer. Mirrors [`probe_hid_mouse`]; on a combo device the
/// keyboard shares the mouse's slot but uses its own interface + endpoint.
/// Idempotent: a no-op success once a keyboard is bound.
pub fn probe_hid_keyboard(dev: &PciDevice, info: &XhciInfo) -> Result<bool, &'static str> {
    let _ = dev;
    let mut guard = STATES.lock();
    let st = state_for(&mut guard, info.mmio_base).ok_or("bring up the controller first")?;
    if !info.mmio_accessible {
        return Err("MMIO not accessible");
    }

    // EVERY configured device's boot-keyboard interface(s) — bind and poll all
    // (a wireless-mouse receiver may also expose a vestigial keyboard
    // interface; polling it is harmless, it just never reports).
    let candidates: Vec<(u8, u8)> = st
        .addressed
        .iter()
        .filter(|d| d.set_config_cc == 1)
        .flat_map(|d| {
            d.config
                .as_ref()
                .map(|c| {
                    c.interfaces
                        .iter()
                        .filter(|i| i.class == 0x03 && i.subclass == 0x01 && i.protocol == 0x01)
                        .map(|i| (d.slot_id, i.number))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();

    let mut bound = false;
    for (slot_id, interface_number) in candidates {
        let Some(slot_idx) = st.slots.iter().position(|s| s.slot_id == slot_id) else {
            continue;
        };
        let Some(ep_idx) = st.slots[slot_idx].endpoints.iter().position(|e| {
            e.direction_in && e.transfer_type == 3 && e.interface_number == interface_number
        }) else {
            continue;
        };
        let dci = st.slots[slot_idx].endpoints[ep_idx].dci;
        if st.keyboards.iter().any(|k| k.slot_id == slot_id && k.dci == dci) {
            bound = true;
            continue;
        }

        let mut res = st.slots[slot_idx].clone();
        if let Err(e) = control_transfer(info, st, &mut res, 0x21, 0x0B, 0, interface_number as u16, 0) {
            serial_println!("xhci: slot {} kbd SET_PROTOCOL failed: {}", slot_id, e);
            st.slots[slot_idx] = res;
            continue;
        }
        let _ = control_transfer(info, st, &mut res, 0x21, 0x0A, 0, interface_number as u16, 0);
        st.slots[slot_idx] = res;

        let report_buf = alloc_dma(8, 64) as u64;
        st.keyboards.push(KeyboardDevice {
            slot_idx,
            ep_idx,
            slot_id,
            dci,
            report_buf,
            report_len: 8,
            armed: false,
            needs_reset: false,
            last: [0; 8],
        });
        let idx = st.keyboards.len() - 1;
        arm_keyboard(info, st, idx);
        serial_println!(
            "xhci: HID boot keyboard armed (slot {}, if {}, ep dci {})",
            slot_id, interface_number, dci
        );
        bound = true;
    }
    Ok(bound)
}

/// Bring up xHCI if needed, ensure devices are enumerated, then find + arm a
/// HID boot **mouse and keyboard**. Called once at boot before the UI. On a
/// USB-booted machine the enumeration pipeline already ran during boot-disk
/// discovery, so it is skipped here (re-running port reset could disturb the
/// open boot disk) and we only probe. On an ATA-booted machine (QEMU) nothing
/// has touched USB yet, so the full, idempotent pipeline runs first. Returns
/// whether any HID device was armed.
pub fn setup_hid() -> bool {
    let devices = pci::enumerate();
    let mut any = false;
    for dev in devices
        .iter()
        .filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.prog_if == 0x30)
    {
        let info = match inspect(dev) {
            Some(i) => i,
            None => continue,
        };
        // Per controller: if boot-disk discovery already enumerated THIS
        // controller, only probe (re-running port resets could disturb the
        // open boot disk). A controller nothing has touched yet gets the
        // full, idempotent pipeline — HID may live there.
        let untouched = {
            let mut guard = STATES.lock();
            state_for(&mut guard, info.mmio_base).map_or(true, |s| s.addressed.is_empty())
        };
        if untouched {
            let _ = bring_up(dev, &info);
            let _ = reset_and_enable_slots(dev, &info);
            let _ = address_enabled_slots(dev, &info);
            let _ = fetch_configurations(dev, &info);
            let _ = configure_endpoints(dev, &info);
        }
        // Probe both — a combo device exposes both on one slot, and we want
        // each bound to its own interface/endpoint.
        if let Ok(true) = probe_hid_mouse(dev, &info) {
            any = true;
        }
        if let Ok(true) = probe_hid_keyboard(dev, &info) {
            any = true;
        }
    }

    // DIAG (temporary): one line per bound HID input, so the boot trace shows
    // exactly what attached (the boot pager lets it be reviewed). Remove once
    // HID is solid on the targets.
    {
        let guard = STATES.lock();
        let mice: Vec<u8> = guard.iter().flat_map(|s| s.mice.iter().map(|m| m.slot_id)).collect();
        let kbds: Vec<u8> = guard.iter().flat_map(|s| s.keyboards.iter().map(|k| k.slot_id)).collect();
        crate::boot_status(&alloc::format!(
            "hid: bound {} mouse {:?}, {} keyboard {:?}",
            mice.len(), mice, kbds.len(), kbds
        ));
    }
    any
}

/// Is a USB-HID device (mouse or keyboard) bound? The UI uses this to choose
/// its idle wait strategy — HID has no IRQ, so the loop polls rather than
/// `hlt`-waiting for one that will never come.
pub fn hid_present() -> bool {
    STATES
        .lock()
        .iter()
        .any(|s| !s.mice.is_empty() || !s.keyboards.is_empty())
}

/// Cooperatively poll **every** bound xHCI HID device across **all**
/// controllers: recover any halted endpoint, drain every pending interrupt-IN
/// completion (dispatched to the right mouse/keyboard by slot+DCI), re-arm,
/// and forward accumulated mouse movement into the shared input queue
/// (keyboard keys are fed straight in by `service_keyboard`). Called from the
/// UI loop — never an IRQ (it takes the heap-backed xHCI lock). Cheap when
/// idle (one event-ring peek per controller).
pub fn pump_hid() {
    let mut deltas: Vec<MouseDelta> = Vec::new();
    {
        let mut guard = STATES.lock();
        for st in guard.iter_mut() {
            if st.mice.is_empty() && st.keyboards.is_empty() {
                continue;
            }
            let info = st.info.clone();

            // Recover halted endpoints (deferred here so we never issue a
            // command wait from inside an event drain).
            for i in 0..st.mice.len() {
                if st.mice[i].needs_reset {
                    let (si, ei) = (st.mice[i].slot_idx, st.mice[i].ep_idx);
                    reset_bulk_endpoint(&info, st, si, ei);
                    st.mice[i].needs_reset = false;
                    st.mice[i].armed = false;
                }
            }
            for i in 0..st.keyboards.len() {
                if st.keyboards[i].needs_reset {
                    let (si, ei) = (st.keyboards[i].slot_idx, st.keyboards[i].ep_idx);
                    reset_bulk_endpoint(&info, st, si, ei);
                    st.keyboards[i].needs_reset = false;
                    st.keyboards[i].armed = false;
                }
            }

            // Consume everything ready; completions serviced inline.
            while let Some(ev) = try_consume_event(&info, st) {
                service_hid_transfer(st, &ev);
            }
            // Keep one TRB pending on each so the next report is delivered.
            for i in 0..st.mice.len() {
                arm_mouse(&info, st, i);
            }
            for i in 0..st.keyboards.len() {
                arm_keyboard(&info, st, i);
            }

            // Collect accumulated motion (fed after the lock drops).
            for m in st.mice.iter_mut() {
                if m.dirty {
                    deltas.push(MouseDelta {
                        dx: m.accum_dx,
                        dy: m.accum_dy,
                        left: m.buttons & 0x01 != 0,
                        right: m.buttons & 0x02 != 0,
                    });
                    m.accum_dx = 0;
                    m.accum_dy = 0;
                    m.dirty = false;
                }
            }
        }
    }
    for d in deltas {
        crate::ps2::feed_mouse_delta(d.dx, d.dy, d.left, d.right);
    }
}

pub fn autopilot_usb_drives(booted_sys_guid: &[u8; 16]) -> Vec<DriveInfo> {
    let devices = pci::enumerate();
    for dev in devices.iter().filter(|d| {
        d.class == 0x0C && d.subclass == 0x03 && d.prog_if == 0x30
    }) {
        let info = match inspect(dev) {
            Some(i) => i,
            None => continue,
        };
        // Each step intentionally swallows its error: a failure at any
        // point just means later steps have less to do, and the user
        // sees partial progress on the xHCI / Drives screens.
        let _ = bring_up(dev, &info);
        let _ = reset_and_enable_slots(dev, &info);
        let _ = address_enabled_slots(dev, &info);
        let _ = fetch_configurations(dev, &info);
        let _ = configure_endpoints(dev, &info);
        let _ = probe_mass_storage(dev, &info);
    }
    usb_drives(booted_sys_guid)
}

pub fn peripheral_device_type_name(t: u8) -> &'static str {
    match t {
        0x00 => "Direct-access block device",
        0x01 => "Sequential-access (tape)",
        0x02 => "Printer",
        0x03 => "Processor",
        0x04 => "Write-once",
        0x05 => "CD/DVD",
        0x07 => "Optical memory",
        0x08 => "Medium changer",
        0x0C => "RAID controller",
        0x0E => "Simplified direct-access",
        _ => "(other)",
    }
}

pub fn completion_code_name(cc: u8) -> &'static str {
    match cc {
        0 => "Invalid",
        1 => "Success",
        2 => "Data Buffer Error",
        3 => "Babble Detected",
        4 => "USB Transaction Error",
        5 => "TRB Error",
        6 => "Stall Error",
        7 => "Resource Error",
        8 => "Bandwidth Error",
        9 => "No Slots Available",
        10 => "Invalid Stream Type",
        11 => "Slot Not Enabled",
        13 => "Short Packet",
        17 => "Parameter Error",
        19 => "Context State Error",
        37 => "Event Ring Full",
        _ => "(other)",
    }
}

// =====================================================================
// Phase 7: `BlockDevice` wrapper for a USB-MSC slot.
//
// `UsbMscDevice` is a tiny handle: it remembers the slot ID, sector
// count, and optionally the system GUID we expect to see in MBR LBA 0.
// Each `read_sector` / `write_sector` / `flush` locks `STATES`, looks up
// the slot's bulk endpoints, and dispatches to the SCSI helpers.
//
// Identity-gate (opt-in via `with_identity_gate`): before every write,
// READ(10) LBA 0 and confirm the on-disk `sys_guid` still matches the
// captured value. On mismatch the wrapper is *poisoned* — every
// subsequent call returns `StoreError::Io` and no write ever lands.
// Implements the future-USB-driver constraint from
// IMPLEMENTATION.md item 3 ("re-verify after every device reset /
// port re-enumeration, before letting the next write through; on
// mismatch fail closed").
// =====================================================================

use tablestore::block::SECTOR;
use tablestore::{BlockDevice as TsBlockDevice, Result as TsResult, StoreError};

/// Last low-level MSC failure detail, surfaced in the UI's otherwise-generic
/// "I/O error" (the laptop has no serial console). Set on any read/write/flush
/// failure; the UI takes and displays it.
static LAST_MSC_ERR: Mutex<Option<alloc::string::String>> = Mutex::new(None);
fn record_msc_err(detail: &str) {
    *LAST_MSC_ERR.lock() = Some(alloc::string::String::from(detail));
}
pub fn take_last_msc_err() -> Option<alloc::string::String> {
    LAST_MSC_ERR.lock().take()
}

/// Several public methods + the `block_size` field are part of the
/// future-mount API (USB-booted TablesOS volume) and not consumed yet
/// — kept here so the surface is in place and the identity-gate path
/// is testable end-to-end.
#[allow(dead_code)]
pub struct UsbMscDevice {
    slot_id: u8,
    /// MMIO base of the owning xHCI controller. Together with `slot_id` this
    /// uniquely identifies the device: slot ids are only unique per
    /// controller, and this machine has two xHCIs that both number a slot 1.
    mmio_base: u64,
    sectors: u64,
    block_size: u32,
    next_tag: u32,
    /// `Some(expected_sys_guid)` activates the identity gate. The
    /// wrapper re-reads MBR LBA 0 before every write and compares the
    /// 16-byte GUID at offset 0x1AC.
    identity_gate: Option<[u8; 16]>,
    /// Latched on any identity-gate failure. All subsequent calls fail.
    poisoned: bool,
}

#[allow(dead_code)]
impl UsbMscDevice {
    /// Build a wrapper from a cached probe. Refuses any block size other than
    /// 512 — the engine assumes that.
    fn from_probe(probe: &MscProbe) -> TsResult<Self> {
        let cap = probe.capacity.ok_or(StoreError::Io)?;
        if (cap.block_size as usize) != SECTOR {
            // The engine assumes 512-B sectors throughout. Refuse rather
            // than silently translate.
            return Err(StoreError::Corrupt("USB device block size != 512"));
        }
        Ok(UsbMscDevice {
            slot_id: probe.slot_id,
            mmio_base: probe.mmio_base,
            sectors: cap.total_blocks(),
            block_size: cap.block_size,
            next_tag: 0xCAFE_0000,
            identity_gate: None,
            poisoned: false,
        })
    }

    /// Open the USB-MSC device on the given slot. Requires a prior
    /// successful MSC probe (which populated `block_size`, `sectors`,
    /// and at least one INQUIRY). When two controllers both have this
    /// slot id, the first probe wins — callers that must reach an exact
    /// device (the boot disk) use [`open_with_identity_gate`], which keys
    /// on the controller too.
    pub fn open(slot_id: u8) -> TsResult<Self> {
        let probes = current_msc();
        let probe = probes
            .iter()
            .find(|p| p.slot_id == slot_id)
            .ok_or(StoreError::Io)?;
        Self::from_probe(probe)
    }

    /// Strict open: identifies the device by **controller + slot** (slot ids
    /// are only unique per controller), and every subsequent write is preceded
    /// by a READ(10) of LBA 0 and a comparison of the 16-byte GUID at MBR
    /// offset 0x1AC against `expected_sys_guid`. A mismatch poisons the
    /// wrapper.
    pub fn open_with_identity_gate(
        mmio_base: u64,
        slot_id: u8,
        expected_sys_guid: [u8; 16],
    ) -> TsResult<Self> {
        let probes = current_msc();
        let probe = probes
            .iter()
            .find(|p| p.mmio_base == mmio_base && p.slot_id == slot_id)
            .ok_or(StoreError::Io)?;
        let mut dev = Self::from_probe(probe)?;
        dev.identity_gate = Some(expected_sys_guid);
        Ok(dev)
    }

    pub fn slot_id(&self) -> u8 {
        self.slot_id
    }
    pub fn block_size(&self) -> u32 {
        self.block_size
    }
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    fn next_tag(&mut self) -> u32 {
        self.next_tag = self.next_tag.wrapping_add(1);
        self.next_tag
    }

    /// Read MBR (LBA 0) and compare the GUID. Returns Err if the device
    /// no longer matches the captured identity.
    fn verify_identity(&mut self) -> TsResult<()> {
        let Some(expected) = self.identity_gate else {
            return Ok(());
        };
        if self.poisoned {
            return Err(StoreError::Io);
        }
        let mut mbr = [0u8; SECTOR];
        let tag = self.next_tag();
        msc_read_sector(self.mmio_base, self.slot_id, 0, &mut mbr, tag).map_err(|e| {
            serial_println!("UsbMscDevice: identity check READ(10) failed: {}", e);
            record_msc_err(e);
            self.poisoned = true;
            StoreError::Io
        })?;
        let on_disk: &[u8; 16] = mbr[0x1AC..0x1AC + 16].try_into().unwrap();
        if on_disk != &expected {
            serial_println!(
                "UsbMscDevice: identity gate FAILED on slot {} — poisoning",
                self.slot_id
            );
            self.poisoned = true;
            return Err(StoreError::Corrupt("USB system GUID changed at runtime"));
        }
        Ok(())
    }

    /// Poll TEST UNIT READY until the medium is ready (CSW status 0) or a ~5 s
    /// budget elapses; returns whether it became ready.
    ///
    /// A USB flash stick that has just been powered (cold boot) answers
    /// INQUIRY / READ CAPACITY — which is all `open` consumes — while its
    /// flash-translation layer is still coming up, then fails the first
    /// READ(10) with "Not Ready, becoming ready". A warm *reset* leaves the
    /// stick powered and already spun up, which is the whole reason a reboot
    /// "fixes" a cold-boot `cannot read MBR`. Calling this before the first
    /// real read makes a cold boot behave like a reset. Best-effort: the read
    /// path keeps its own retry, so the caller may proceed even on `false`.
    pub fn wait_until_ready(&mut self) -> bool {
        if self.poisoned {
            return false;
        }
        // Up to ~5 s (the USB-MSC convention), 50 × 100 ms. The first probe has
        // no pre-delay, so an already-ready stick (warm reset) returns at once.
        for _ in 0..50 {
            let tag = self.next_tag();
            match msc_test_unit_ready(self.mmio_base, self.slot_id, tag) {
                Ok(0) => return true,
                // CHECK CONDITION (becoming ready) or a transient transport
                // stall while the stick wakes up — wait and try again.
                Ok(_) | Err(_) => time::delay_ms(100),
            }
        }
        false
    }
}

impl TsBlockDevice for UsbMscDevice {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> TsResult<()> {
        if self.poisoned || buf.len() != SECTOR || lba >= self.sectors {
            return Err(StoreError::Io);
        }
        if lba > u32::MAX as u64 {
            // READ(10) carries a 32-bit LBA. Multi-TiB volumes would
            // need READ(16); not implemented here.
            return Err(StoreError::Io);
        }
        let tag = self.next_tag();
        msc_read_sector(self.mmio_base, self.slot_id, lba as u32, buf, tag)
            .map_err(|e| {
                record_msc_err(e);
                StoreError::Io
            })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> TsResult<()> {
        let count = buf.len() / SECTOR;
        if self.poisoned
            || buf.is_empty()
            || buf.len() % SECTOR != 0
            || lba + count as u64 > self.sectors
            || lba > u32::MAX as u64
            || count > u16::MAX as usize
        {
            return Err(StoreError::Io);
        }
        let tag = self.next_tag();
        msc_read_blocks(self.mmio_base, self.slot_id, lba as u32, count as u16, buf, tag)
            .map_err(|e| {
                record_msc_err(e);
                StoreError::Io
            })
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> TsResult<()> {
        if self.poisoned || buf.len() != SECTOR || lba >= self.sectors {
            return Err(StoreError::Io);
        }
        if lba > u32::MAX as u64 {
            return Err(StoreError::Io);
        }
        // Identity gate: if armed, re-check before every write that
        // we're still talking to the originally-booted disk.
        self.verify_identity()?;
        let tag = self.next_tag();
        msc_write_sector(self.mmio_base, self.slot_id, lba as u32, buf, tag)
            .map_err(|e| {
                record_msc_err(e);
                StoreError::Io
            })
    }

    fn flush(&mut self) -> TsResult<()> {
        if self.poisoned {
            return Err(StoreError::Io);
        }
        let tag = self.next_tag();
        // SYNCHRONIZE CACHE is optional in USB Bulk-Only Transport; cheap sticks
        // stall or Fail it. The endpoint is recovered by msc_command, and writes
        // on these devices are write-through, so treat a failure as a no-op
        // rather than failing the whole operation.
        if let Err(e) = msc_sync_cache(self.mmio_base, self.slot_id, tag) {
            record_msc_err(e);
            serial_println!("UsbMscDevice: SYNC CACHE best-effort failed: {}", e);
        }
        Ok(())
    }
}

// ---- Internal MSC I/O helpers locked on the global state ------------

fn msc_resolve_bulk(
    st: &XhciState,
    slot_id: u8,
) -> Result<(usize, usize, usize), &'static str> {
    let slot_idx = st
        .slots
        .iter()
        .position(|s| s.slot_id == slot_id)
        .ok_or("no such slot")?;
    let bulk_in_idx = st.slots[slot_idx]
        .endpoints
        .iter()
        .position(|e| e.direction_in && e.transfer_type == 2)
        .ok_or("slot has no bulk IN endpoint")?;
    let bulk_out_idx = st.slots[slot_idx]
        .endpoints
        .iter()
        .position(|e| !e.direction_in && e.transfer_type == 2)
        .ok_or("slot has no bulk OUT endpoint")?;
    Ok((slot_idx, bulk_in_idx, bulk_out_idx))
}

fn msc_read_sector(
    mmio_base: u64,
    slot_id: u8,
    lba: u32,
    buf: &mut [u8],
    tag: u32,
) -> Result<(), &'static str> {
    let mut guard = STATES.lock();
    let st = state_for(&mut guard, mmio_base).ok_or("no such USB controller")?;
    let info = st.info.clone();
    let (slot_idx, bulk_in_idx, bulk_out_idx) = msc_resolve_bulk(st, slot_id)?;

    // Retry the whole READ(10) a few times, resetting both bulk endpoints
    // between attempts. Real sticks (and freshly-enumerated devices that are
    // not yet fully ready — seen on cold boot with two xHCI controllers)
    // intermittently stall or short a round-trip; the standard BOT recovery
    // is reset + re-issue. A short settle delay gives a not-ready device time.
    let mut last = "READ(10) not attempted";
    for attempt in 0..3 {
        if attempt > 0 {
            reset_bulk_endpoint(&info, st, slot_idx, bulk_in_idx);
            reset_bulk_endpoint(&info, st, slot_idx, bulk_out_idx);
            time::delay_ms(20);
        }
        match scsi_read10(
            &info, st, slot_idx, bulk_in_idx, bulk_out_idx, 0, lba, 1, SECTOR as u32, tag,
        ) {
            Ok((0, bytes)) if bytes.len() >= buf.len() => {
                buf.copy_from_slice(&bytes[..buf.len()]);
                return Ok(());
            }
            Ok((status, _)) if status != 0 => last = "READ(10) CSW status != 0",
            Ok(_) => last = "READ(10) returned short data",
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Read `count` consecutive sectors in a SINGLE READ(10) (one bulk data stage),
/// rather than `count` separate single-sector commands. Same BOT recovery as
/// `msc_read_sector`. `buf` must be `count * SECTOR` bytes.
fn msc_read_blocks(
    mmio_base: u64,
    slot_id: u8,
    lba: u32,
    count: u16,
    buf: &mut [u8],
    tag: u32,
) -> Result<(), &'static str> {
    let mut guard = STATES.lock();
    let st = state_for(&mut guard, mmio_base).ok_or("no such USB controller")?;
    let info = st.info.clone();
    let (slot_idx, bulk_in_idx, bulk_out_idx) = msc_resolve_bulk(st, slot_id)?;

    let mut last = "READ(10) not attempted";
    for attempt in 0..3 {
        if attempt > 0 {
            reset_bulk_endpoint(&info, st, slot_idx, bulk_in_idx);
            reset_bulk_endpoint(&info, st, slot_idx, bulk_out_idx);
            time::delay_ms(20);
        }
        match scsi_read10(
            &info, st, slot_idx, bulk_in_idx, bulk_out_idx, 0, lba, count, SECTOR as u32, tag,
        ) {
            Ok((0, bytes)) if bytes.len() >= buf.len() => {
                buf.copy_from_slice(&bytes[..buf.len()]);
                return Ok(());
            }
            Ok((status, _)) if status != 0 => last = "READ(10) CSW status != 0",
            Ok(_) => last = "READ(10) returned short data",
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn msc_write_sector(
    mmio_base: u64,
    slot_id: u8,
    lba: u32,
    data: &[u8],
    tag: u32,
) -> Result<(), &'static str> {
    let mut guard = STATES.lock();
    let st = state_for(&mut guard, mmio_base).ok_or("no such USB controller")?;
    let info = st.info.clone();
    let (slot_idx, bulk_in_idx, bulk_out_idx) = msc_resolve_bulk(st, slot_id)?;
    // Retry once: a transient bulk stall on the first attempt halts the endpoint,
    // which msc_command then clears (Reset Endpoint), so the second attempt lands.
    let mut last: Result<(), &'static str> = Err("WRITE(10) not attempted");
    for _ in 0..2 {
        match scsi_write10(
            &info, st, slot_idx, bulk_in_idx, bulk_out_idx, 0, lba, 1, SECTOR as u32,
            data, tag,
        ) {
            Ok(0) => return Ok(()),
            Ok(_) => last = Err("WRITE(10) CSW status != 0"),
            Err(e) => last = Err(e),
        }
    }
    last
}

fn msc_sync_cache(mmio_base: u64, slot_id: u8, tag: u32) -> Result<(), &'static str> {
    let mut guard = STATES.lock();
    let st = state_for(&mut guard, mmio_base).ok_or("no such USB controller")?;
    let info = st.info.clone();
    let (slot_idx, bulk_in_idx, bulk_out_idx) = msc_resolve_bulk(st, slot_id)?;
    // SCSI SYNCHRONIZE CACHE(10), opcode 0x35. No data stage.
    let cb: [u8; 10] = [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let outcome = msc_command(
        &info,
        st,
        slot_idx,
        bulk_in_idx,
        bulk_out_idx,
        0,
        &cb,
        None,
        tag,
    )?;
    if outcome.scsi_status != 0 {
        return Err("SYNCHRONIZE CACHE CSW status != 0");
    }
    Ok(())
}

/// Issue a single TEST UNIT READY and return its CSW status (0 = ready).
/// `msc_command` already recovers a stalled endpoint internally, so the
/// caller polls this directly while a freshly-powered stick spins up.
fn msc_test_unit_ready(mmio_base: u64, slot_id: u8, tag: u32) -> Result<u8, &'static str> {
    let mut guard = STATES.lock();
    let st = state_for(&mut guard, mmio_base).ok_or("no such USB controller")?;
    let info = st.info.clone();
    let (slot_idx, bulk_in_idx, bulk_out_idx) = msc_resolve_bulk(st, slot_id)?;
    scsi_test_unit_ready(&info, st, slot_idx, bulk_in_idx, bulk_out_idx, 0, tag)
}

/// Monotonic tag source for callers that don't carry their own (the
/// install-to-USB batched path). The per-`UsbMscDevice` counter is
/// separate; both just need uniqueness within an in-flight command.
static MSC_TAG: AtomicU32 = AtomicU32::new(0x5AC0_0000);

fn next_msc_tag() -> u32 {
    MSC_TAG.fetch_add(1, Ordering::Relaxed)
}

/// Write `blocks` consecutive 512-byte sectors starting at `lba` in a
/// single SCSI WRITE(10) — far fewer USB round-trips than one sector at
/// a time. `data.len()` must be at least `blocks * 512`.
pub fn msc_write_blocks(
    slot_id: u8,
    lba: u32,
    blocks: u16,
    data: &[u8],
) -> Result<(), &'static str> {
    let tag = next_msc_tag();
    let mut guard = STATES.lock();
    let st = state_for_slot(&mut guard, slot_id).ok_or("no controller owns this USB slot")?;
    let info = st.info.clone();
    let (slot_idx, bulk_in_idx, bulk_out_idx) = msc_resolve_bulk(st, slot_id)?;
    let status = scsi_write10(
        &info,
        st,
        slot_idx,
        bulk_in_idx,
        bulk_out_idx,
        0,
        lba,
        blocks,
        SECTOR as u32,
        data,
        tag,
    )?;
    if status != 0 {
        return Err("WRITE(10) CSW status != 0");
    }
    Ok(())
}

/// Slot-addressed batched read — the read counterpart of [`msc_write_blocks`].
/// Thin wrapper over the retrying [`msc_read_blocks`]: it resolves the owning
/// controller's MMIO base from `slot_id` and supplies a fresh tag. `buf.len()`
/// must be at least `blocks * 512`; exactly that many bytes are filled.
///
/// Currently unused — the upgrade flow used to relocate a volume by copying a
/// physical sector range with this, but the copy-on-write rebuild now reads the
/// old volume *logically* (scattered pages) instead. Retained as the read-side
/// counterpart of the still-used [`msc_write_blocks`].
#[allow(dead_code)]
pub fn msc_read_blocks_slot(
    slot_id: u8,
    lba: u32,
    blocks: u16,
    buf: &mut [u8],
) -> Result<(), &'static str> {
    let want = blocks as usize * SECTOR;
    if buf.len() < want {
        return Err("msc_read_blocks_slot: buffer too small");
    }
    let mmio_base = {
        let mut guard = STATES.lock();
        state_for_slot(&mut guard, slot_id)
            .ok_or("no controller owns this USB slot")?
            .info
            .mmio_base
    };
    let tag = next_msc_tag();
    msc_read_blocks(mmio_base, slot_id, lba, blocks, &mut buf[..want], tag)
}

// ---- Self-test: write a pattern, read it back, verify ---------------

#[derive(Clone)]
pub struct WriteTestResult {
    pub slot_id: u8,
    pub lba: u32,
    pub verify_ok: bool,
    pub message: alloc::string::String,
    pub readback_head: Vec<u8>,
}

/// Open `slot_id` as a `BlockDevice`, write a recognisable pattern to
/// `lba`, flush, read the same sector back, and verify byte-for-byte.
/// Restores the original contents on success/failure so the device is
/// not left modified.
pub fn run_write_test(slot_id: u8, lba: u32) -> WriteTestResult {
    let mut result = WriteTestResult {
        slot_id,
        lba,
        verify_ok: false,
        message: alloc::string::String::new(),
        readback_head: Vec::new(),
    };
    let mut dev = match UsbMscDevice::open(slot_id) {
        Ok(d) => d,
        Err(e) => {
            result.message = alloc::format!("open: {:?}", e);
            return result;
        }
    };

    let mut original = [0u8; SECTOR];
    if let Err(e) = dev.read_sector(lba as u64, &mut original) {
        result.message = alloc::format!("baseline read: {:?}", e);
        return result;
    }

    let mut pattern = [0u8; SECTOR];
    let header = b"TBLSOS-USB-MSC-BLOCKDEVICE-WRITE-TEST  ";
    pattern[..header.len()].copy_from_slice(header);
    for i in header.len()..SECTOR {
        pattern[i] = if i & 1 == 0 { 0xAA } else { 0x55 };
    }

    if let Err(e) = dev.write_sector(lba as u64, &pattern) {
        result.message = alloc::format!("write: {:?}", e);
        return result;
    }
    let _ = dev.flush();

    let mut readback = [0u8; SECTOR];
    if let Err(e) = dev.read_sector(lba as u64, &mut readback) {
        result.message = alloc::format!("readback: {:?}", e);
        return result;
    }
    let matches = pattern == readback;
    result.verify_ok = matches;
    result.readback_head = readback[..32.min(SECTOR)].to_vec();

    // Restore the original sector so we don't leave the device modified.
    let restored = dev.write_sector(lba as u64, &original).is_ok();
    let _ = dev.flush();

    result.message = if matches && restored {
        alloc::format!("OK — wrote, read back identical, restored")
    } else if matches {
        alloc::format!("verify passed but restore failed")
    } else {
        alloc::format!("readback differs from pattern")
    };
    result
}
