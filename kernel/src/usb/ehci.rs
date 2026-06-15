//! EHCI (USB 2.0) host-controller driver — the boot path for machines whose
//! USB ports are wired to the chipset EHCI rather than an xHCI (pre-2012
//! laptops: e.g. Intel 5-Series/Ibex Peak, whose only xHCI — if any — is a
//! discrete chip serving ports the BIOS may not even boot from).
//!
//! Deliberately minimal and fully **polled**, mirroring the xHCI driver's
//! philosophy: synchronous one-transfer-at-a-time, no interrupts, no
//! periodic schedule. The async schedule holds a single self-linked QH that
//! is rebuilt per transfer stage (the schedule is stopped around each
//! rebuild — slow but unambiguous, and boot-time I/O is the only load).
//!
//! Unlike xHCI there are no slots: addressing is manual (`SET_ADDRESS`),
//! and **hub support is mandatory** — Ibex Peak routes every physical port
//! through an integrated Rate-Matching Hub, so even a root-port stick
//! appears behind a standard USB 2.0 hub. Only High-Speed devices are
//! supported (Full/Low-Speed need split transactions / companion
//! controllers, which TablesOS does not implement); the boot stick is HS.
//!
//! Everything here is additive: nothing in the xHCI path calls into this
//! module, and `discover_boot_disk` only tries EHCI after xHCI found no
//! matching boot drive.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use super::bot;
use crate::pci::{self, PciDevice};
use crate::serial_println;
use crate::time;
use tablestore::{BlockDevice as TsBlockDevice, Result as TsResult, StoreError};

const SECTOR: usize = 512;

// ---- registers ---------------------------------------------------------------

const USBCMD: u64 = 0x00;
const USBSTS: u64 = 0x04;
const USBINTR: u64 = 0x08;
const CTRLDSSEGMENT: u64 = 0x10;
const ASYNCLISTADDR: u64 = 0x18;
const CONFIGFLAG: u64 = 0x40;
const PORTSC_BASE: u64 = 0x44;

const CMD_RS: u32 = 1 << 0;
const CMD_HCRESET: u32 = 1 << 1;
const CMD_ASE: u32 = 1 << 5;
const STS_HCHALTED: u32 = 1 << 12;
const STS_ASS: u32 = 1 << 15;

const PSC_CCS: u32 = 1 << 0;
const PSC_CSC: u32 = 1 << 1;
const PSC_PED: u32 = 1 << 2;
const PSC_PEDC: u32 = 1 << 3;
const PSC_OCC: u32 = 1 << 5;
const PSC_PR: u32 = 1 << 8;
const PSC_PP: u32 = 1 << 12;
const PSC_RW1C: u32 = PSC_CSC | PSC_PEDC | PSC_OCC;

// qTD token bits.
const TOK_ACTIVE: u32 = 1 << 7;
const TOK_HALTED: u32 = 1 << 6;
const TOK_BABBLE: u32 = 1 << 4;
const TOK_XACTERR: u32 = 1 << 3;
const PID_OUT: u32 = 0 << 8;
const PID_IN: u32 = 1 << 8;
const PID_SETUP: u32 = 2 << 8;
const TOK_CERR3: u32 = 3 << 10;

const QH_DTC: u32 = 1 << 14; // data toggle from each qTD (software-tracked)
const QH_HEAD: u32 = 1 << 15;
/// Control-endpoint flag (FS/LS only) — tells the HC to run the split
/// control-transfer state machine through the hub's Transaction Translator.
const QH_CTL: u32 = 1 << 27;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Speed {
    Low,
    Full,
    High,
}

impl Speed {
    fn eps_bits(self) -> u32 {
        match self {
            Speed::Full => 0 << 12,
            Speed::Low => 1 << 12,
            Speed::High => 2 << 12,
        }
    }
}

/// Everything needed to address one device's default pipe: its address and
/// speed plus, for Full/Low-Speed devices, the High-Speed hub + port whose
/// Transaction Translator carries the split transactions (the chipset
/// Rate-Matching Hub, on the machines this driver exists for).
#[derive(Clone, Copy)]
struct Pipe {
    addr: u8,
    speed: Speed,
    /// `(hub_address, hub_port)`; `(0, 0)` for High-Speed devices.
    tt: (u8, u8),
    mps0: u16,
}

// ---- DMA helpers ---------------------------------------------------------------

/// Page-aligned, zeroed, identity-mapped allocation freed on drop. The heap
/// lives below 4 GiB, so pointers are valid 32-bit EHCI bus addresses.
struct Dma {
    ptr: *mut u8,
    layout: Layout,
}

impl Dma {
    fn new(size: usize) -> Dma {
        let layout = Layout::from_size_align(size, 4096).expect("bad DMA layout");
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            panic!("ehci: DMA alloc failed ({} bytes)", size);
        }
        Dma { ptr, layout }
    }
    fn addr(&self) -> u32 {
        self.ptr as u32
    }
    fn write(&self, off: usize, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            unsafe { write_volatile(self.ptr.add(off + i), b) };
        }
    }
    fn read_vec(&self, off: usize, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        for i in 0..len {
            v.push(unsafe { read_volatile(self.ptr.add(off + i)) });
        }
        v
    }
}

impl Drop for Dma {
    fn drop(&mut self) {
        unsafe { alloc::alloc::dealloc(self.ptr, self.layout) };
    }
}

// The per-controller QH/qTD ring is kept as a plain address (a deliberately
// leaked page) so the state stays `Send` for the static mutex — same pattern
// as the xHCI driver's permanent ring allocations.
fn wr32(base: u64, off: usize, v: u32) {
    unsafe { write_volatile((base + off as u64) as *mut u32, v) };
}
fn rd32(base: u64, off: usize) -> u32 {
    unsafe { read_volatile((base + off as u64) as *const u32) }
}

// ---- controller state ------------------------------------------------------------

struct Endpoint {
    ep: u8, // endpoint number (no direction bit)
    mps: u16,
    toggle: bool,
}

/// One enumerated mass-storage device.
pub struct MscDev {
    pub addr: u8,
    #[allow(dead_code)] // identification, for future Drives-screen listing
    pub vid: u16,
    #[allow(dead_code)]
    pub pid: u16,
    pipe: Pipe,
    bulk_in: Endpoint,
    bulk_out: Endpoint,
    pub block_size: u32,
    pub total_blocks: u64,
    /// LBA 0 as read during the probe (boot-signature / GUID matching).
    pub first_block: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HidKind {
    Keyboard,
    Mouse,
}

/// A HID **boot-protocol** keyboard or mouse. EHCI-era machines lose the
/// BIOS's SMM input emulation the moment we claim the controller, so the
/// kernel drives these itself — polled with GET_REPORT control requests
/// (boot devices must support them, HID 1.11 §7.2.1), which avoids the
/// whole periodic schedule.
struct HidDev {
    pipe: Pipe,
    iface: u16,
    kind: HidKind,
    /// Interrupt IN endpoint (number, max-packet) — captured for a future
    /// switch from control GET_REPORT to interrupt-endpoint polling, which
    /// real devices implement more reliably. Logged at registration.
    #[allow(dead_code)]
    int_in: (u8, u16),
    /// Last keyboard report (modifiers + 6 usages), for edge detection.
    last: [u8; 8],
    /// Consecutive failures; the device is parked after too many.
    errors: u8,
}

struct EhciState {
    op: u64, // operational register base (MMIO, identity-mapped)
    n_ports: u8,
    /// One permanently-allocated page holding the single transfer QH and the
    /// qTDs (see `QH_OFF` / `QTD_OFF`). Address, not a pointer: the page is
    /// leaked on purpose (the controller keeps referencing it) and the state
    /// must be `Send` for the static mutex.
    ring: u64,
    next_addr: u8,
    msc: Vec<MscDev>,
    hid: Vec<HidDev>,
}

static STATES: Mutex<Vec<EhciState>> = Mutex::new(Vec::new());

fn op_read(st: &EhciState, off: u64) -> u32 {
    unsafe { read_volatile((st.op + off) as *const u32) }
}
fn op_write(st: &EhciState, off: u64, v: u32) {
    unsafe { write_volatile((st.op + off) as *mut u32, v) };
}

fn poll_op(st: &EhciState, off: u64, mask: u32, expect: u32, max_us: u64) -> bool {
    let per = time::tsc_per_us().max(1);
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    loop {
        if op_read(st, off) & mask == expect {
            return true;
        }
        if (unsafe { core::arch::x86_64::_rdtsc() } - start) / per > max_us {
            return false;
        }
        core::hint::spin_loop();
    }
}

// ---- bring-up -----------------------------------------------------------------

/// BIOS→OS handoff: EHCI's USB Legacy Support capability lives in PCI
/// **config** space, located by HCCPARAMS.EECP. Same dance as on xHCI.
fn bios_handoff(dev: &PciDevice, hccparams: u32) {
    let mut off = ((hccparams >> 8) & 0xFF) as u8;
    for _ in 0..16 {
        if off < 0x40 {
            break;
        }
        let cap = pci::config_read32(dev, off);
        if cap & 0xFF == 0x01 {
            // USBLEGSUP: bit16 = BIOS owned, bit24 = OS owned; USBLEGCTLSTS at
            // off+4 holds the BIOS SMI enables. On some machines (the Sony VAIO
            // PCG-31311M) a single USBLEGSUP ownership write blocks for *minutes*
            // — it traps into a slow synchronous BIOS SMM handler. So disable the
            // SMI sources FIRST (that stops the handler firing), THEN seize
            // ownership directly, confirming with a short wall-clock-bounded
            // poll. We halt+reset the controller next, so the BIOS must stop
            // servicing it from SMM regardless.
            pci::config_write32(dev, off + 4, 0); // disable BIOS SMI sources
            pci::config_write32(dev, off, (cap | (1 << 24)) & !(1 << 16)); // OS owned, BIOS not
            let per = time::tsc_per_us().max(1);
            let start = unsafe { core::arch::x86_64::_rdtsc() };
            let mut released = false;
            loop {
                if pci::config_read32(dev, off) & (1 << 16) == 0 {
                    released = true;
                    break;
                }
                if (unsafe { core::arch::x86_64::_rdtsc() } - start) / per > 200_000 {
                    break;
                }
                time::delay_ms(5);
            }
            crate::boot_status(if released {
                "ehci: handoff OK"
            } else {
                "ehci: handoff forced (BIOS kept it)"
            });
            return;
        }
        off = ((cap >> 8) & 0xFF) as u8;
    }
    crate::boot_status("ehci: no legacy cap");
}

/// Reset + start one EHCI controller and return its state index, or an error
/// string. Idempotent per controller (keyed by the operational base).
fn bring_up(dev: &PciDevice) -> Result<usize, &'static str> {
    let bar = pci::bar_address(dev, 0);
    if bar == 0 || bar >= 0x1_0000_0000 {
        return Err("EHCI BAR unusable");
    }
    pci::enable_mmio_and_bus_master(dev);

    let cap = bar; // identity-mapped
    let cap_length = unsafe { read_volatile(cap as *const u8) };
    let hcsparams = unsafe { read_volatile((cap + 0x4) as *const u32) };
    let hccparams = unsafe { read_volatile((cap + 0x8) as *const u32) };
    let op = cap + cap_length as u64;

    let mut guard = STATES.lock();
    if let Some(i) = guard.iter().position(|s| s.op == op) {
        return Ok(i);
    }

    crate::boot_status(&format!(
        "ehci: {:04x}:{:04x} ports={}",
        dev.vendor,
        dev.device,
        hcsparams & 0xF
    ));

    bios_handoff(dev, hccparams);

    let ring_page = Dma::new(4096);
    let ring = ring_page.addr() as u64;
    core::mem::forget(ring_page); // permanent: the controller references it
    let st = EhciState {
        op,
        n_ports: (hcsparams & 0xF) as u8,
        ring,
        next_addr: 1,
        msc: Vec::new(),
        hid: Vec::new(),
    };

    // Halt → reset.
    op_write(&st, USBCMD, op_read(&st, USBCMD) & !CMD_RS);
    if !poll_op(&st, USBSTS, STS_HCHALTED, STS_HCHALTED, 100_000) {
        return Err("EHCI did not halt");
    }
    op_write(&st, USBCMD, CMD_HCRESET);
    if !poll_op(&st, USBCMD, CMD_HCRESET, 0, 500_000) {
        return Err("EHCI reset stuck");
    }

    // Polled operation: no interrupts, no periodic schedule. Run, then claim
    // every port (CONFIGFLAG routes shared ports away from any companions).
    op_write(&st, USBINTR, 0);
    op_write(&st, CTRLDSSEGMENT, 0);
    op_write(&st, USBCMD, (8 << 16) | CMD_RS);
    op_write(&st, CONFIGFLAG, 1);

    // Port power (when the controller has per-port switches), then let
    // connects settle — same reasoning as the xHCI path.
    let ppc = hcsparams & (1 << 4) != 0;
    if ppc {
        for p in 0..st.n_ports {
            let v = port_read(&st, p);
            port_write(&st, p, v | PSC_PP);
        }
    }
    let mut waited = 0u32;
    loop {
        let any =
            (0..st.n_ports).any(|p| port_read(&st, p) & PSC_CCS != 0);
        if any || waited >= 1_000 {
            break;
        }
        time::delay_ms(20);
        waited += 20;
    }
    if waited > 0 {
        time::delay_ms(100);
    }

    guard.push(st);
    Ok(guard.len() - 1)
}

fn port_read(st: &EhciState, port: u8) -> u32 {
    op_read(st, PORTSC_BASE + 4 * port as u64)
}
fn port_write(st: &EhciState, port: u8, v: u32) {
    op_write(st, PORTSC_BASE + 4 * port as u64, v & !PSC_RW1C);
}

// ---- transfers --------------------------------------------------------------------

/// Ring layout inside the state's DMA page.
const QH_OFF: usize = 0; // 48 bytes used, 64 reserved
const QTD_OFF: [usize; 3] = [0x100, 0x140, 0x180]; // 32 bytes each, 32-aligned

fn qtd_write(ring: u64, off: usize, next: u32, token: u32, buf: u32) {
    wr32(ring, off, next);
    wr32(ring, off + 0x4, 1); // alternate next = Terminate
    wr32(ring, off + 0x8, token);
    // Buffer pointer list: first entry carries the byte offset; the rest are
    // the following 4 KiB pages (transfers here never exceed two pages).
    wr32(ring, off + 0xC, buf);
    wr32(ring, off + 0x10, if buf != 0 { (buf & !0xFFF).wrapping_add(0x1000) } else { 0 });
    wr32(ring, off + 0x14, 0);
    wr32(ring, off + 0x18, 0);
    wr32(ring, off + 0x1C, 0);
}

/// Stop the async schedule, point it at a fresh single QH executing the qTD
/// chain at `first_qtd`, start it, and poll the chain's last token to
/// completion. Returns the final token of `last_qtd`.
fn run_chain(
    st: &EhciState,
    pipe: Pipe,
    ep: u8,
    mps: u16,
    is_control: bool,
    first_qtd: usize,
    last_qtd: usize,
    max_us: u64,
) -> Result<u32, &'static str> {
    let ring = st.ring;

    // Schedule off while the QH is rebuilt.
    op_write(st, USBCMD, op_read(st, USBCMD) & !CMD_ASE);
    if !poll_op(st, USBSTS, STS_ASS, 0, 100_000) {
        return Err("async schedule stuck on");
    }

    let split_ctl = is_control && pipe.speed != Speed::High;
    let qh = ring as u32 + QH_OFF as u32;
    wr32(ring, QH_OFF, qh | 0x2); // horizontal: itself, type QH
    wr32(
        ring,
        QH_OFF + 0x4,
        (pipe.addr as u32)
            | ((ep as u32) << 8)
            | pipe.speed.eps_bits()
            | QH_DTC
            | QH_HEAD
            | ((mps as u32) << 16)
            | if split_ctl { QH_CTL } else { 0 },
    );
    // One transaction per microframe; for FS/LS the splits go through the
    // Transaction Translator at (hub address, hub port). The µframe S/C
    // masks stay zero — they are periodic-schedule fields, and the HC times
    // async splits itself.
    let (hub_addr, hub_port) = pipe.tt;
    wr32(
        ring,
        QH_OFF + 0x8,
        (1 << 30) | ((hub_addr as u32) << 16) | ((hub_port as u32) << 23),
    );
    wr32(ring, QH_OFF + 0xC, 0); // current qTD
    // Transfer overlay: next = first qTD, everything else clean.
    wr32(ring, QH_OFF + 0x10, ring as u32 + first_qtd as u32);
    wr32(ring, QH_OFF + 0x14, 1);
    wr32(ring, QH_OFF + 0x18, 0);

    op_write(st, ASYNCLISTADDR, qh);
    op_write(st, USBCMD, op_read(st, USBCMD) | CMD_ASE);

    // Poll the last qTD until the controller retires it.
    let per = time::tsc_per_us().max(1);
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    let token = loop {
        let t = rd32(ring, last_qtd + 0x8);
        if t & TOK_ACTIVE == 0 {
            break t;
        }
        // An earlier qTD erroring halts the QH and never retires the last
        // one — detect via the overlay token.
        let ov = rd32(ring, QH_OFF + 0x18);
        if ov & TOK_HALTED != 0 {
            break ov;
        }
        if (unsafe { core::arch::x86_64::_rdtsc() } - start) / per > max_us {
            op_write(st, USBCMD, op_read(st, USBCMD) & !CMD_ASE);
            return Err("transfer timeout");
        }
        core::hint::spin_loop();
    };

    op_write(st, USBCMD, op_read(st, USBCMD) & !CMD_ASE);
    let _ = poll_op(st, USBSTS, STS_ASS, 0, 100_000);
    Ok(token)
}

fn token_err(token: u32) -> Option<&'static str> {
    if token & TOK_HALTED == 0 {
        return None;
    }
    Some(if token & TOK_BABBLE != 0 {
        "babble"
    } else if token & TOK_XACTERR != 0 {
        "transaction error"
    } else {
        "stall"
    })
}

/// Bytes left un-transferred in a retired qTD.
fn token_residue(token: u32) -> u32 {
    (token >> 16) & 0x7FFF
}

/// Synchronous control transfer. `data`: `Some((len, data_in, payload))`.
/// Returns the IN data (empty for OUT / no-data).
fn control(
    st: &EhciState,
    pipe: Pipe,
    bm_request_type: u8,
    b_request: u8,
    w_value: u16,
    w_index: u16,
    data: Option<(usize, bool, &[u8])>,
) -> Result<Vec<u8>, &'static str> {
    let (len, data_in) = match data {
        Some((l, i, _)) => (l, i),
        None => (0, false),
    };
    if len > 4096 {
        return Err("control transfer too large");
    }

    let setup = Dma::new(4096);
    setup.write(
        0,
        &[
            bm_request_type,
            b_request,
            w_value as u8,
            (w_value >> 8) as u8,
            w_index as u8,
            (w_index >> 8) as u8,
            len as u8,
            (len >> 8) as u8,
        ],
    );
    let buf = Dma::new(4096);
    if let Some((l, false, payload)) = data {
        buf.write(0, &payload[..l]);
    }

    let ring = st.ring;
    // SETUP (toggle 0) → optional DATA (toggle 1) → STATUS (toggle 1, zero
    // length). The status stage runs opposite to the data stage — and with
    // no data stage it is always IN (USB 2.0 §8.5.3).
    let status_pid = if len > 0 && data_in { PID_OUT } else { PID_IN };
    let status_off = if len > 0 { QTD_OFF[2] } else { QTD_OFF[1] };
    qtd_write(
        ring,
        QTD_OFF[0],
        ring as u32 + (if len > 0 { QTD_OFF[1] } else { status_off }) as u32,
        TOK_ACTIVE | TOK_CERR3 | PID_SETUP | (8 << 16),
        setup.addr(),
    );
    if len > 0 {
        qtd_write(
            ring,
            QTD_OFF[1],
            ring as u32 + status_off as u32,
            TOK_ACTIVE
                | TOK_CERR3
                | (if data_in { PID_IN } else { PID_OUT })
                | ((len as u32) << 16)
                | (1 << 31),
            buf.addr(),
        );
    }
    qtd_write(
        ring,
        status_off,
        1, // Terminate
        TOK_ACTIVE | TOK_CERR3 | status_pid | (1 << 31),
        0,
    );

    let token = run_chain(st, pipe, 0, pipe.mps0, true, QTD_OFF[0], status_off, 2_000_000)?;
    if let Some(e) = token_err(token) {
        return Err(e);
    }
    if data_in && len > 0 {
        let got = len - token_residue(rd32(ring, QTD_OFF[1] + 0x8)) as usize;
        Ok(buf.read_vec(0, got.min(len)))
    } else {
        Ok(Vec::new())
    }
}

/// Synchronous bulk transfer on one endpoint; tracks the data toggle in
/// software (the QH is rebuilt per transfer, so the toggle can't live in
/// hardware). Returns bytes actually transferred.
fn bulk(
    st: &EhciState,
    pipe: Pipe,
    ep: &mut Endpoint,
    data_in: bool,
    buf: &Dma,
    len: usize,
    max_us: u64,
) -> Result<usize, &'static str> {
    if len > 4096 {
        return Err("bulk transfer too large");
    }
    let ring = st.ring;
    qtd_write(
        ring,
        QTD_OFF[0],
        1,
        TOK_ACTIVE
            | TOK_CERR3
            | (if data_in { PID_IN } else { PID_OUT })
            | ((len as u32) << 16)
            | ((ep.toggle as u32) << 31),
        buf.addr(),
    );
    let token = run_chain(st, pipe, ep.ep, ep.mps, false, QTD_OFF[0], QTD_OFF[0], max_us)?;
    let done = len - token_residue(token) as usize;
    // The toggle advances once per packet, including a terminating short one.
    let packets = if done == 0 { 1 } else { (done + ep.mps as usize - 1) / ep.mps as usize };
    if token_err(token).is_none() && packets % 2 == 1 {
        ep.toggle = !ep.toggle;
    }
    if let Some(e) = token_err(token) {
        return Err(e);
    }
    Ok(done)
}

/// CLEAR_FEATURE(ENDPOINT_HALT) + software toggle reset — the recovery for a
/// stalled bulk endpoint (BOT requires it after a failed stage).
fn clear_stall(st: &EhciState, pipe: Pipe, ep: &mut Endpoint, dir_in: bool) {
    let w_index = ep.ep as u16 | if dir_in { 0x80 } else { 0 };
    let _ = control(st, pipe, 0x02, 0x01, 0, w_index, None);
    ep.toggle = false;
}

// ---- enumeration --------------------------------------------------------------------

/// A parsed interface: its real `bInterfaceNumber` (needed as the `wIndex`
/// of every interface-targeted control request — using the Vec position
/// instead silently mis-targets composite devices), class/subclass/protocol,
/// and endpoints `(addr, type, mps)`.
struct ParsedIface {
    number: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    endpoints: Vec<(u8, u8, u16)>,
}

struct ParsedConfig {
    config_value: u8,
    interfaces: Vec<ParsedIface>,
}

fn parse_config(raw: &[u8]) -> ParsedConfig {
    let mut out = ParsedConfig {
        config_value: raw.get(5).copied().unwrap_or(1),
        interfaces: Vec::new(),
    };
    let mut i = 0usize;
    while i + 1 < raw.len() {
        let len = raw[i] as usize;
        if len == 0 {
            break;
        }
        match raw.get(i + 1) {
            Some(4) if i + 9 <= raw.len() => {
                // Interface descriptor (alternate setting 0 only).
                if raw[i + 3] == 0 {
                    out.interfaces.push(ParsedIface {
                        number: raw[i + 2],
                        class: raw[i + 5],
                        subclass: raw[i + 6],
                        protocol: raw[i + 7],
                        endpoints: Vec::new(),
                    });
                }
            }
            Some(5) if i + 7 <= raw.len() => {
                if let Some(last) = out.interfaces.last_mut() {
                    let mps = u16::from_le_bytes([raw[i + 4], raw[i + 5]]);
                    last.endpoints.push((raw[i + 2], raw[i + 3] & 0x3, mps));
                }
            }
            _ => {}
        }
        i += len;
    }
    out
}

/// Enumerate the (single) device currently answering at address 0: read its
/// descriptors, give it an address, configure it, then recurse into hubs or
/// register MSC / HID functions. `speed` + `tt` come from the parent port.
fn enumerate_addr0(
    states: &mut Vec<EhciState>,
    idx: usize,
    depth: u8,
    speed: Speed,
    tt: (u8, u8),
) -> Result<(), &'static str> {
    let addr = {
        let st = &mut states[idx];
        let a = st.next_addr;
        st.next_addr += 1;
        a
    };
    let st = &states[idx];

    // EP0 max packet: 64 for HS, 8 until the descriptor says otherwise for
    // FS/LS. Read 8 bytes first — every stack does, and picky devices need it.
    let mut pipe = Pipe {
        addr: 0,
        speed,
        tt,
        mps0: if speed == Speed::High { 64 } else { 8 },
    };
    let first8 = control(st, pipe, 0x80, 6, 1 << 8, 0, Some((8, true, &[])))?;
    if first8.len() >= 8 && speed != Speed::High {
        pipe.mps0 = first8[7] as u16;
    }
    control(st, pipe, 0x00, 5, addr as u16, 0, None)?;
    time::delay_ms(10); // SET_ADDRESS recovery interval
    pipe.addr = addr;

    let dev_desc = control(st, pipe, 0x80, 6, 1 << 8, 0, Some((18, true, &[])))?;
    if dev_desc.len() < 18 {
        return Err("short device descriptor");
    }
    let vid = u16::from_le_bytes([dev_desc[8], dev_desc[9]]);
    let pid = u16::from_le_bytes([dev_desc[10], dev_desc[11]]);
    let dev_class = dev_desc[4];

    let head = control(st, pipe, 0x80, 6, 2 << 8, 0, Some((9, true, &[])))?;
    let total = u16::from_le_bytes([head[2], head[3]]) as usize;
    let full = control(st, pipe, 0x80, 6, 2 << 8, 0, Some((total.min(4096), true, &[])))?;
    let cfg = parse_config(&full);
    control(st, pipe, 0x00, 9, cfg.config_value as u16, 0, None)?;

    let is_hub = dev_class == 9 || cfg.interfaces.iter().any(|i| i.class == 9);
    if is_hub {
        crate::boot_status(&format!("ehci: addr {} = hub {:04x}:{:04x}", addr, vid, pid));
        if depth >= 3 {
            return Err("hub nesting too deep");
        }
        drive_hub(states, idx, pipe, depth + 1)?;
        return Ok(());
    }

    // Mass storage, SCSI transparent, Bulk-Only Transport?
    if let Some(ifd) = cfg
        .interfaces
        .iter()
        .find(|i| i.class == 0x08 && i.subclass == 0x06 && i.protocol == 0x50)
    {
        let bulk_in = ifd
            .endpoints
            .iter()
            .find(|e| e.1 == 2 && e.0 & 0x80 != 0)
            .ok_or("MSC without bulk IN")?;
        let bulk_out = ifd
            .endpoints
            .iter()
            .find(|e| e.1 == 2 && e.0 & 0x80 == 0)
            .ok_or("MSC without bulk OUT")?;
        crate::boot_status(&format!("ehci: addr {} = MSC {:04x}:{:04x}", addr, vid, pid));
        states[idx].msc.push(MscDev {
            addr,
            vid,
            pid,
            pipe,
            bulk_in: Endpoint { ep: bulk_in.0 & 0xF, mps: bulk_in.2, toggle: false },
            bulk_out: Endpoint { ep: bulk_out.0 & 0xF, mps: bulk_out.2, toggle: false },
            block_size: 0,
            total_blocks: 0,
            first_block: Vec::new(),
        });
        return Ok(());
    }

    // Register EVERY HID boot interface (class 3, subclass 1 = boot, protocol
    // 1=keyboard / 2=mouse), not just the first — a combo wireless receiver or
    // a keyboard with an integrated pointer is one device with both. Each is
    // targeted by its real `bInterfaceNumber`. Collect first so the borrow of
    // `cfg` ends before we touch `states[idx]` mutably.
    let hids: Vec<(u16, HidKind, (u8, u16))> = cfg
        .interfaces
        .iter()
        .filter(|i| i.class == 0x03 && i.subclass == 0x01)
        .filter_map(|i| {
            let kind = match i.protocol {
                1 => HidKind::Keyboard,
                2 => HidKind::Mouse,
                _ => return None,
            };
            // Interrupt IN endpoint (type 3, address bit 7 set).
            let int_in = i
                .endpoints
                .iter()
                .find(|e| e.1 == 3 && e.0 & 0x80 != 0)
                .map(|e| (e.0 & 0xF, e.2))
                .unwrap_or((0, 8));
            Some((i.number as u16, kind, int_in))
        })
        .collect();
    if !hids.is_empty() {
        for (ifn, kind, int_in) in hids {
            let st = &states[idx];
            // SET_PROTOCOL(boot) — must reach the right interface or reports
            // keep their report-mode layout and decode to garbage; SET_IDLE(0)
            // is best-effort. Non-fatal: a device that NAKs/stalls these
            // shouldn't abort enumeration of the rest.
            let sp = control(st, pipe, 0x21, 0x0B, 0, ifn, None);
            let _ = control(st, pipe, 0x21, 0x0A, 0, ifn, None);
            crate::boot_status(&format!(
                "ehci: addr {} if {} = HID {} {:04x}:{:04x} epIN={} mps={} setproto={}",
                addr,
                ifn,
                if kind == HidKind::Keyboard { "keyboard" } else { "mouse" },
                vid,
                pid,
                int_in.0,
                int_in.1,
                if sp.is_ok() { "ok" } else { "ERR" },
            ));
            states[idx].hid.push(HidDev {
                pipe,
                iface: ifn,
                kind,
                int_in,
                last: [0; 8],
                errors: 0,
            });
        }
        return Ok(());
    }

    crate::boot_status(&format!(
        "ehci: addr {} = {:04x}:{:04x} class {:02x} (unused)",
        addr, vid, pid, dev_class
    ));
    Ok(())
}

/// Standard hub-class walk: power the ports, reset whatever is connected,
/// enumerate every child. Full/Low-Speed children run through the closest
/// High-Speed hub's Transaction Translator: this hub itself when it is HS,
/// otherwise the TT inherited from the parent chain.
fn drive_hub(
    states: &mut Vec<EhciState>,
    idx: usize,
    hub: Pipe,
    depth: u8,
) -> Result<(), &'static str> {
    let (n_ports, pwr_ms) = {
        let st = &states[idx];
        let d = control(st, hub, 0xA0, 6, 0x29 << 8, 0, Some((9, true, &[])))?;
        if d.len() < 6 {
            return Err("short hub descriptor");
        }
        (d[2], (d[5] as u32) * 2 + 100)
    };
    crate::boot_status(&format!("ehci: hub {}: {} ports", hub.addr, n_ports));

    for port in 1..=n_ports {
        {
            let st = &states[idx];
            // SET_PORT_FEATURE(PORT_POWER)
            control(st, hub, 0x23, 3, 8, port as u16, None)?;
        }
        time::delay_ms(pwr_ms as u64);
        let status = {
            let st = &states[idx];
            control(st, hub, 0xA3, 0, 0, port as u16, Some((4, true, &[])))?
        };
        if status.len() < 4 || status[0] & 0x01 == 0 {
            continue; // nothing connected
        }
        {
            let st = &states[idx];
            // SET_PORT_FEATURE(PORT_RESET), then wait for completion.
            control(st, hub, 0x23, 3, 4, port as u16, None)?;
        }
        time::delay_ms(60);
        let mut enabled = false;
        let mut child_speed = Speed::Full;
        for _ in 0..20 {
            let s = {
                let st = &states[idx];
                control(st, hub, 0xA3, 0, 0, port as u16, Some((4, true, &[])))?
            };
            if s.len() >= 4 && s[0] & 0x10 == 0 {
                enabled = s[0] & 0x02 != 0;
                child_speed = if s[1] & 0x04 != 0 {
                    Speed::High // wPortStatus bit 10
                } else if s[1] & 0x02 != 0 {
                    Speed::Low // wPortStatus bit 9
                } else {
                    Speed::Full
                };
                break;
            }
            time::delay_ms(10);
        }
        {
            let st = &states[idx];
            // CLEAR_FEATURE(C_PORT_RESET) + (C_PORT_CONNECTION)
            let _ = control(st, hub, 0x23, 1, 20, port as u16, None);
            let _ = control(st, hub, 0x23, 1, 16, port as u16, None);
        }
        if !enabled {
            crate::boot_status(&format!("ehci: hub {} port {}: reset failed", hub.addr, port));
            continue;
        }
        // The TT for a non-HS child sits in the closest HS hub: this one if
        // it is HS, else whatever this (FS) hub already used.
        let child_tt = if child_speed == Speed::High {
            (0, 0)
        } else if hub.speed == Speed::High {
            (hub.addr, port)
        } else {
            hub.tt
        };
        time::delay_ms(20); // post-reset recovery before addressing
        if let Err(e) = enumerate_addr0(states, idx, depth, child_speed, child_tt) {
            crate::boot_status(&format!(
                "ehci: hub {} port {}: enumerate: {}",
                hub.addr, port, e
            ));
        }
    }
    Ok(())
}

/// Reset + enumerate every root port of one controller.
fn enumerate_root(states: &mut Vec<EhciState>, idx: usize) {
    let n = states[idx].n_ports;
    for p in 0..n {
        let v = {
            let st = &states[idx];
            port_read(st, p)
        };
        if v & PSC_CCS == 0 {
            continue;
        }
        {
            let st = &states[idx];
            // Port reset: PR=1 for 50 ms, then clear and wait for PED.
            port_write(st, p, (v & !PSC_PED) | PSC_PR);
            time::delay_ms(50);
            let v2 = port_read(st, p);
            port_write(st, p, v2 & !PSC_PR);
            let per = time::tsc_per_us().max(1);
            let t0 = unsafe { core::arch::x86_64::_rdtsc() };
            while port_read(st, p) & PSC_PR != 0 {
                if (unsafe { core::arch::x86_64::_rdtsc() } - t0) / per > 100_000 {
                    break;
                }
            }
        }
        let after = {
            let st = &states[idx];
            port_read(st, p)
        };
        crate::boot_status(&format!("ehci: root port {} SC={:08x}", p + 1, after));
        if after & PSC_PED == 0 {
            // A root port that won't enable holds a Full/Low-Speed device —
            // those belong to a companion controller (UHCI/OHCI), which this
            // driver does not implement. On RMH chipsets this never happens.
            crate::boot_status("ehci: port did not enable (FS/LS device?) — skipping");
            continue;
        }
        time::delay_ms(20);
        // Root-attached and enabled on EHCI = High-Speed by definition.
        if let Err(e) = enumerate_addr0(states, idx, 0, Speed::High, (0, 0)) {
            crate::boot_status(&format!("ehci: root port {}: enumerate: {}", p + 1, e));
        }
    }
}

// ---- Bulk-Only Transport ------------------------------------------------------------

/// One BOT command on an MSC device. `data`: `(len, data_in, payload)`.
fn bot_command(
    st: &mut EhciState,
    msc_idx: usize,
    lun: u8,
    cb: &[u8],
    data: Option<(usize, bool, &[u8])>,
    tag: u32,
) -> Result<(u8, Vec<u8>), &'static str> {
    let (dlen, din) = data.map(|(l, i, _)| (l, i)).unwrap_or((0, false));
    let pipe = st.msc[msc_idx].pipe;

    // CBW.
    let cbw_bytes = bot::build_cbw(tag, dlen as u32, din, lun, cb);
    let cbw = Dma::new(4096);
    cbw.write(0, &cbw_bytes);
    {
        let mut ep = take_out(st, msc_idx);
        let r = bulk(st, pipe, &mut ep, false, &cbw, 31, 1_000_000);
        put_out(st, msc_idx, ep);
        if let Err(e) = r {
            let mut epo = take_out(st, msc_idx);
            clear_stall(st, pipe, &mut epo, false);
            put_out(st, msc_idx, epo);
            return Err(e);
        }
    }

    // Data stage (a stall here is legal — BOT says read the CSW anyway).
    let dbuf = Dma::new(4096.max(dlen));
    let mut got = 0usize;
    if dlen > 0 {
        if !din {
            if let Some((l, _, payload)) = data {
                dbuf.write(0, &payload[..l]);
            }
        }
        let mut ep = if din { take_in(st, msc_idx) } else { take_out(st, msc_idx) };
        let r = bulk(st, pipe, &mut ep, din, &dbuf, dlen, 3_000_000);
        if din {
            put_in(st, msc_idx, ep);
        } else {
            put_out(st, msc_idx, ep);
        }
        match r {
            Ok(n) => got = n,
            Err(_) => {
                let mut ep2 = if din { take_in(st, msc_idx) } else { take_out(st, msc_idx) };
                clear_stall(st, pipe, &mut ep2, din);
                if din {
                    put_in(st, msc_idx, ep2);
                } else {
                    put_out(st, msc_idx, ep2);
                }
            }
        }
    }

    // CSW (retry once after clearing a stalled IN endpoint).
    let csw = Dma::new(4096);
    let mut csw_bytes: Option<Vec<u8>> = None;
    for attempt in 0..2 {
        let mut ep = take_in(st, msc_idx);
        let r = bulk(st, pipe, &mut ep, true, &csw, 13, 1_000_000);
        put_in(st, msc_idx, ep);
        match r {
            Ok(n) if n >= 13 => {
                csw_bytes = Some(csw.read_vec(0, 13));
                break;
            }
            _ if attempt == 0 => {
                let mut ep2 = take_in(st, msc_idx);
                clear_stall(st, pipe, &mut ep2, true);
                put_in(st, msc_idx, ep2);
            }
            _ => {}
        }
    }
    let csw_bytes = csw_bytes.ok_or("CSW unreadable")?;
    let (status, _residue) = bot::parse_csw(&csw_bytes, tag)?;
    let out = if din && dlen > 0 { dbuf.read_vec(0, got) } else { Vec::new() };
    Ok((status, out))
}

// Endpoint take/put helpers: `bulk` needs `&EhciState` plus `&mut Endpoint`,
// which both live in the state — temporarily move the endpoint out.
fn take_in(st: &mut EhciState, i: usize) -> Endpoint {
    core::mem::replace(&mut st.msc[i].bulk_in, Endpoint { ep: 0, mps: 512, toggle: false })
}
fn put_in(st: &mut EhciState, i: usize, ep: Endpoint) {
    st.msc[i].bulk_in = ep;
}
fn take_out(st: &mut EhciState, i: usize) -> Endpoint {
    core::mem::replace(&mut st.msc[i].bulk_out, Endpoint { ep: 0, mps: 512, toggle: false })
}
fn put_out(st: &mut EhciState, i: usize, ep: Endpoint) {
    st.msc[i].bulk_out = ep;
}

static TAG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0xEC10_0000);
fn next_tag() -> u32 {
    TAG.fetch_add(1, Ordering::Relaxed)
}

/// Last low-level failure detail, surfaced where an `Err` would otherwise be
/// opaque (the kernel's "cannot read MBR" fatal, write errors). The laptop /
/// desktop have no serial console, so this is how a real-hardware failure
/// names itself on screen.
static LAST_ERR: Mutex<Option<String>> = Mutex::new(None);
fn record_err(detail: &str) {
    *LAST_ERR.lock() = Some(String::from(detail));
}
pub fn take_last_err() -> Option<String> {
    LAST_ERR.lock().take()
}

/// Reset both bulk endpoints of an MSC device — CLEAR_FEATURE(ENDPOINT_HALT)
/// on each plus a software toggle reset. The command-level recovery a flaky
/// stick needs when a whole READ/WRITE round-trip fails (toggle desync, a
/// transient stall the per-stage recovery in `bot_command` didn't clear).
fn reset_msc_endpoints(st: &mut EhciState, msc_idx: usize) {
    let pipe = st.msc[msc_idx].pipe;
    let mut ein = take_in(st, msc_idx);
    clear_stall(st, pipe, &mut ein, true);
    put_in(st, msc_idx, ein);
    let mut eout = take_out(st, msc_idx);
    clear_stall(st, pipe, &mut eout, false);
    put_out(st, msc_idx, eout);
}

/// INQUIRY / TEST UNIT READY / READ CAPACITY / READ(10) of LBA 0 on every
/// enumerated MSC device — fills in geometry + first block.
fn probe_msc(st: &mut EhciState) {
    for i in 0..st.msc.len() {
        let _ = bot_command(st, i, 0, &bot::cdb_inquiry(36), Some((36, true, &[])), next_tag());
        // Wait for the medium to spin up. A freshly-powered stick (cold boot)
        // answers INQUIRY while its flash-translation layer is still coming up
        // and reports "becoming ready" to TEST UNIT READY; a warm reset leaves
        // it already ready. Give it the USB-MSC-conventional ~5 s.
        //
        // Bound by WALL-CLOCK, not an iteration count: a cold stick NAKs every
        // probe until it is ready, and each NAK burns the full ~1 s bulk
        // transfer timeout — counting 50 iterations turned a cold boot into ~1
        // minute on real hardware (an Alcor 058f:6387 that never reports ready
        // via TUR yet is perfectly readable). Give up at the deadline and let
        // READ CAPACITY / READ(10) below proceed regardless.
        let per = time::tsc_per_us().max(1);
        let ready_start = unsafe { core::arch::x86_64::_rdtsc() };
        loop {
            if let Ok((0, _)) =
                bot_command(st, i, 0, &bot::cdb_test_unit_ready(), None, next_tag())
            {
                break;
            }
            if (unsafe { core::arch::x86_64::_rdtsc() } - ready_start) / per > 5_000_000 {
                break;
            }
            time::delay_ms(100);
        }
        if let Ok((0, cap)) = bot_command(
            st,
            i,
            0,
            &bot::cdb_read_capacity10(),
            Some((8, true, &[])),
            next_tag(),
        ) {
            if cap.len() >= 8 {
                let last = u32::from_be_bytes(cap[0..4].try_into().unwrap());
                st.msc[i].block_size = u32::from_be_bytes(cap[4..8].try_into().unwrap());
                st.msc[i].total_blocks = last as u64 + 1;
            }
        }
        if st.msc[i].block_size as usize == SECTOR {
            if let Ok((0, blk)) = bot_command(
                st,
                i,
                0,
                &bot::cdb_read10(0, 1),
                Some((SECTOR, true, &[])),
                next_tag(),
            ) {
                st.msc[i].first_block = blk;
            }
        }
        crate::boot_status(&format!(
            "ehci: MSC addr {}: bs={} blocks={} mbr={}",
            st.msc[i].addr,
            st.msc[i].block_size,
            st.msc[i].total_blocks,
            st.msc[i].first_block.len()
        ));
    }
}

// ---- public: boot-disk discovery -----------------------------------------------------

/// Offset of the system GUID in the TBLSBOOT header (boot/layout.md v2).
const SYS_GUID_OFF: usize = 0x1AC;

/// Bring up EHCI controllers one at a time — enumerate (hubs included),
/// probe MSC — and return an identity-gated block device for the drive whose
/// MBR GUID matches the booted system. Controllers are abandoned **as soon
/// as the boot drive is found**: an untouched controller keeps its BIOS SMM
/// ownership, so USB keyboards/mice attached to it keep working through the
/// firmware's legacy i8042 emulation (devices on touched controllers are
/// served by this driver's own HID polling instead).
pub fn find_boot_drive(sys_guid: &[u8; 16]) -> Option<(EhciMscDevice, u64)> {
    let devices = pci::enumerate();
    let mut any = false;
    for dev in devices
        .iter()
        .filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.prog_if == 0x20)
    {
        any = true;
        let idx = match bring_up(dev) {
            Ok(i) => i,
            Err(e) => {
                crate::boot_status(&format!("ehci: bring-up FAILED: {}", e));
                continue;
            }
        };
        let mut guard = STATES.lock();
        enumerate_root(&mut guard, idx);
        probe_msc(&mut guard[idx]);

        let st = &guard[idx];
        for (mi, m) in st.msc.iter().enumerate() {
            if m.first_block.len() < 512 || m.block_size as usize != SECTOR {
                continue;
            }
            let sig = m.first_block[510] == 0x55 && m.first_block[511] == 0xAA;
            let booted = &m.first_block[SYS_GUID_OFF..SYS_GUID_OFF + 16] == &sys_guid[..];
            crate::boot_status(&format!(
                "ehci: drive addr {} sig={} booted={}",
                m.addr, sig, booted
            ));
            if booted {
                return Some((
                    EhciMscDevice {
                        ctrl: idx,
                        msc: mi,
                        sectors: m.total_blocks,
                        sys_guid: *sys_guid,
                        poisoned: false,
                    },
                    m.total_blocks,
                ));
            }
        }
    }
    if !any {
        crate::boot_status("ehci: no EHCI controller in PCI");
    } else {
        crate::boot_status("ehci: no matching boot drive");
    }
    None
}

// ---- HID polling -----------------------------------------------------------------

/// Is at least one EHCI HID device bound? The UI uses this to keep polling
/// instead of `hlt`-waiting on IRQs that will never come.
pub fn hid_present() -> bool {
    STATES.lock().iter().any(|s| !s.hid.is_empty())
}

/// Poll every bound HID device with GET_REPORT and feed decoded events into
/// the shared PS/2-shaped input queue. Called from the UI loop, never from
/// an IRQ. A device that keeps failing is parked, not retried forever.
pub fn pump_hid() {
    let mut guard = STATES.lock();
    for st in guard.iter_mut() {
        // Index, not `iter_mut`: `control` borrows `*st` immutably, which can't
        // overlap a mutable borrow of `st.hid`. Pull out the fields the call
        // needs, run it, then re-borrow the element to record the result.
        for i in 0..st.hid.len() {
            let (errors, kind, pipe, iface) =
                (st.hid[i].errors, st.hid[i].kind, st.hid[i].pipe, st.hid[i].iface);
            if errors >= 8 {
                continue;
            }
            let len = if kind == HidKind::Keyboard { 8 } else { 4 };
            // GET_REPORT(Input, id 0) on the interface.
            match control(st, pipe, 0xA1, 0x01, 1 << 8, iface, Some((len, true, &[]))) {
                Ok(report) => {
                    st.hid[i].errors = 0;
                    match kind {
                        HidKind::Keyboard => decode_keyboard(&mut st.hid[i], &report),
                        HidKind::Mouse => decode_mouse(&report),
                    }
                }
                Err(_) => st.hid[i].errors += 1,
            }
        }
    }
}

fn decode_mouse(report: &[u8]) {
    if report.len() < 3 {
        return;
    }
    let dx = report[1] as i8 as i32;
    let dy = report[2] as i8 as i32;
    let left = report[0] & 0x01 != 0;
    let right = report[0] & 0x02 != 0;
    if dx != 0 || dy != 0 || report[0] & 0x07 != 0 {
        // HID boot-mouse `dy` is already screen-oriented (positive = down) —
        // `feed_mouse_delta` expects that convention, so pass it straight
        // through, matching the xHCI mouse path. (Negating it inverts Y.)
        crate::ps2::feed_mouse_delta(dx, dy, left, right);
    }
}

fn decode_keyboard(h: &mut HidDev, report: &[u8]) {
    if report.len() < 8 {
        return;
    }
    let shift = report[0] & 0x22 != 0;
    // Edge detection: emit a key once when its usage appears in the report.
    for i in 2..8 {
        let usage = report[i];
        if usage < 4 {
            continue; // no key / error rollover
        }
        if h.last[2..8].contains(&usage) {
            continue; // still held from the previous poll
        }
        if let Some(key) = hid_usage_to_key(usage, shift) {
            crate::ps2::feed_key(key);
        }
    }
    h.last.copy_from_slice(&report[..8]);
}

/// HID boot-keyboard usage → the kernel's key events (US layout, mirroring
/// the PS/2 scancode tables in `ps2.rs`).
pub(crate) fn hid_usage_to_key(usage: u8, shift: bool) -> Option<crate::ps2::Key> {
    use crate::ps2::Key;
    let ch = |a: char, b: char| Some(Key::Char(if shift { b } else { a }));
    match usage {
        0x04..=0x1D => {
            let c = (b'a' + usage - 0x04) as char;
            Some(Key::Char(if shift { c.to_ascii_uppercase() } else { c }))
        }
        0x1E..=0x27 => {
            const PLAIN: [char; 10] = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
            const SHIFTED: [char; 10] = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')'];
            let i = (usage - 0x1E) as usize;
            Some(Key::Char(if shift { SHIFTED[i] } else { PLAIN[i] }))
        }
        0x28 => Some(Key::Enter),
        0x29 => Some(Key::Esc),
        0x2A => Some(Key::Backspace),
        0x2B => Some(Key::Tab),
        0x2C => Some(Key::Char(' ')),
        0x2D => ch('-', '_'),
        0x2E => ch('=', '+'),
        0x2F => ch('[', '{'),
        0x30 => ch(']', '}'),
        0x31 => ch('\\', '|'),
        0x33 => ch(';', ':'),
        0x34 => ch('\'', '"'),
        0x35 => ch('`', '~'),
        0x36 => ch(',', '<'),
        0x37 => ch('.', '>'),
        0x38 => ch('/', '?'),
        0x4A => Some(Key::Home),
        0x4B => Some(Key::PageUp),
        0x4C => Some(Key::Delete),
        0x4D => Some(Key::End),
        0x4E => Some(Key::PageDown),
        0x4F => Some(Key::Right),
        0x50 => Some(Key::Left),
        0x51 => Some(Key::Down),
        0x52 => Some(Key::Up),
        _ => None,
    }
}

// ---- BlockDevice ----------------------------------------------------------------------

/// FUA opt-out, mirroring the xHCI driver's durability convention.
static FUA_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// The booted TablesOS disk reached over EHCI. Always identity-gated: every
/// write re-reads LBA 0 and requires the system GUID captured at boot, and a
/// mismatch poisons the device (same contract as the xHCI `UsbMscDevice`).
pub struct EhciMscDevice {
    ctrl: usize,
    msc: usize,
    sectors: u64,
    sys_guid: [u8; 16],
    poisoned: bool,
}

impl EhciMscDevice {
    fn read_lba(&self, lba: u32, buf: &mut [u8]) -> TsResult<()> {
        let mut guard = STATES.lock();
        let st = guard.get_mut(self.ctrl).ok_or(StoreError::Io)?;
        // Retry the whole READ(10) once after a full bulk-endpoint reset: cheap
        // USB sticks intermittently stall or desync a toggle on a round-trip,
        // and the standard BOT recovery is to reset and re-issue.
        let mut last = "no attempt";
        for attempt in 0..2 {
            if attempt == 1 {
                reset_msc_endpoints(st, self.msc);
            }
            match bot_command(
                st,
                self.msc,
                0,
                &bot::cdb_read10(lba, 1),
                Some((SECTOR, true, &[])),
                next_tag(),
            ) {
                Ok((0, data)) if data.len() >= SECTOR => {
                    buf.copy_from_slice(&data[..SECTOR]);
                    return Ok(());
                }
                Ok((status, data)) => {
                    last = if status != 0 {
                        "READ(10) CSW status != 0"
                    } else {
                        let _ = data;
                        "READ(10) short data"
                    };
                }
                Err(e) => last = e,
            }
        }
        record_err(&format!("READ(10) lba {} failed: {}", lba, last));
        Err(StoreError::Io)
    }

    fn verify_identity(&mut self) -> TsResult<()> {
        if self.poisoned {
            return Err(StoreError::Io);
        }
        let mut mbr = [0u8; SECTOR];
        self.read_lba(0, &mut mbr)?;
        if mbr[SYS_GUID_OFF..SYS_GUID_OFF + 16] != self.sys_guid[..] {
            serial_println!("ehci: identity gate FAILED — poisoning");
            self.poisoned = true;
            return Err(StoreError::Corrupt("USB system GUID changed at runtime"));
        }
        Ok(())
    }
}

impl TsBlockDevice for EhciMscDevice {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> TsResult<()> {
        if self.poisoned || buf.len() != SECTOR || lba > u32::MAX as u64 {
            return Err(StoreError::Io);
        }
        self.read_lba(lba as u32, buf)
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8]) -> TsResult<()> {
        if self.poisoned || buf.len() != SECTOR || lba > u32::MAX as u64 {
            return Err(StoreError::Io);
        }
        self.verify_identity()?;
        let mut guard = STATES.lock();
        let st = guard.get_mut(self.ctrl).ok_or(StoreError::Io)?;
        // FUA first for durability; one plain retry + sticky opt-out when a
        // device rejects it (same policy as the xHCI write path).
        let mut fua = !FUA_UNSUPPORTED.load(Ordering::Relaxed);
        loop {
            let r = bot_command(
                st,
                self.msc,
                0,
                &bot::cdb_write10(lba as u32, 1, fua),
                Some((SECTOR, false, buf)),
                next_tag(),
            );
            match r {
                Ok((0, _)) => return Ok(()),
                _ if fua => {
                    FUA_UNSUPPORTED.store(true, Ordering::Relaxed);
                    serial_println!("ehci: WRITE(10)+FUA refused; plain writes from now on");
                    fua = false;
                }
                _ => return Err(StoreError::Io),
            }
        }
    }

    fn flush(&mut self) -> TsResult<()> {
        if self.poisoned {
            return Err(StoreError::Io);
        }
        let mut guard = STATES.lock();
        let st = guard.get_mut(self.ctrl).ok_or(StoreError::Io)?;
        // SYNCHRONIZE CACHE(10), best-effort like the xHCI path (FUA writes
        // carry the durability guarantee).
        let cb: [u8; 10] = [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let _ = bot_command(st, self.msc, 0, &cb, None, next_tag());
        Ok(())
    }
}
