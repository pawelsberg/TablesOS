//! USB host stack. See `memory/usb-stack-roadmap.md` for the phase plan.
//!
//! Submodules:
//!
//! * [`xhci`] — host-controller driver (currently read-only register
//!   inspection; future phases will add reset, command/event/transfer
//!   rings, port reset, and device enumeration).

pub mod xhci;
