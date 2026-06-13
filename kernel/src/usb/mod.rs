//! USB host stack. See `memory/usb-stack-roadmap.md` for the phase plan.
//!
//! Submodules:
//!
//! * [`xhci`] — the primary host-controller driver: full enumeration,
//!   HID boot mouse, mass storage, install-to-USB.
//! * [`ehci`] — USB 2.0 controller driver for pre-xHCI machines (boot-disk
//!   path only, hubs included — old chipsets put a Rate-Matching Hub in
//!   front of every port). Only consulted when xHCI finds no boot drive.
//! * [`bot`] — shared Bulk-Only-Transport / SCSI byte-level builders.

pub mod bot;
pub mod ehci;
pub mod xhci;
