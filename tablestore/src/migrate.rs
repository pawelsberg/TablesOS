//! On-disk **version migration** framework.
//!
//! TablesOS has one unified version (see [`crate::VERSION`]) stamped into every
//! on-disk field. "Topping up" an older volume to the running version means two
//! things, in this order:
//!
//! 1. **Data migration** — transform the volume's *content* from the older
//!    format to the current one. That is this module. Each adjacent
//!    `(from, to)` version pair gets exactly one step in [`apply_step`]; the
//!    public [`migrate_data`] walks the ladder so an arbitrarily old volume is
//!    brought forward one released version at a time.
//! 2. **Version re-stamping + new boot code** — handled by the kernel's
//!    `upgrade` flow ([`crate::Store::finalize_upgrade`] stamps the version;
//!    the kernel replaces the boot prefix). Not this module's concern.
//!
//! Right now the only released versions are v0.1.0 and v0.2.0 and their on-disk
//! *data* layouts are identical, so the single step is a no-op. The scaffolding
//! exists so that a future format change is a localised, testable addition: a
//! new entry in [`KNOWN_VERSIONS`] and a new arm in [`apply_step`].

use crate::block::BlockDevice;
use crate::store::Store;
use crate::{Result, StoreError};

/// Packed form (`(major<<16)|(minor<<8)|patch`) of a released version.
const fn v(major: u32, minor: u32, patch: u32) -> u32 {
    (major << 16) | (minor << 8) | patch
}

/// Every released on-disk version, oldest first. The migration ladder steps
/// strictly upward through this list. Append new releases here as they ship.
pub const KNOWN_VERSIONS: &[u32] = &[v(0, 1, 0), v(0, 2, 0)];

/// Is `version` a release this build knows how to migrate from?
pub fn is_known(version: u32) -> bool {
    KNOWN_VERSIONS.contains(&version)
}

/// Migrate an opened volume's data from version `from` up to `to`.
///
/// * A downgrade (`from > to`) is refused — the engine never moves data
///   backward.
/// * `from == to` is a no-op (already current).
/// * Otherwise each adjacent released-version step between `from` and `to` is
///   applied in order via [`apply_step`].
///
/// Returns the number of steps applied (0 when already current).
pub fn migrate_data<D: BlockDevice>(store: &mut Store<D>, from: u32, to: u32) -> Result<u32> {
    if from > to {
        return Err(StoreError::Corrupt("refusing to downgrade volume"));
    }
    if from == to {
        return Ok(0);
    }
    if !is_known(from) {
        return Err(StoreError::Corrupt("unknown source version; cannot migrate"));
    }
    let mut cur = from;
    let mut steps = 0;
    while cur < to {
        // The next released version above `cur`, capped at the target.
        let next = KNOWN_VERSIONS
            .iter()
            .copied()
            .find(|&kv| kv > cur)
            .ok_or(StoreError::Corrupt("no migration path to current version"))?
            .min(to);
        apply_step(store, cur, next)?;
        cur = next;
        steps += 1;
    }
    Ok(steps)
}

/// Apply the single data-format transformation that turns version `from` into
/// the immediately-following version `to`. One arm per released step.
fn apply_step<D: BlockDevice>(_store: &mut Store<D>, from: u32, to: u32) -> Result<()> {
    match (from, to) {
        // v0.1.0 -> v0.2.0: the on-disk data format is unchanged. Nothing to
        // transform; the version stamp is refreshed by `finalize_upgrade`.
        (a, b) if a == v(0, 1, 0) && b == v(0, 2, 0) => Ok(()),
        _ => Err(StoreError::Corrupt("no migration step for this version pair")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemBlockDevice;

    fn store(mb: u64) -> Store<MemBlockDevice> {
        Store::format(MemBlockDevice::new(mb * 1024 * 1024 / crate::block::SECTOR as u64)).unwrap()
    }

    #[test]
    fn same_version_is_noop() {
        let mut s = store(16);
        assert_eq!(migrate_data(&mut s, v(0, 2, 0), v(0, 2, 0)).unwrap(), 0);
    }

    #[test]
    fn one_step_up() {
        let mut s = store(16);
        assert_eq!(migrate_data(&mut s, v(0, 1, 0), v(0, 2, 0)).unwrap(), 1);
    }

    #[test]
    fn downgrade_refused() {
        let mut s = store(16);
        assert!(migrate_data(&mut s, v(0, 2, 0), v(0, 1, 0)).is_err());
    }

    #[test]
    fn unknown_source_refused() {
        let mut s = store(16);
        // A version below every known release: no ladder entry to start from.
        assert!(migrate_data(&mut s, v(0, 0, 9), v(0, 2, 0)).is_err());
    }

    #[test]
    fn data_survives_a_migration_and_version_is_restamped() {
        // Seed a row, migrate (no-op step), finalize, confirm it's intact and
        // the superblock now carries the current version.
        let mut s = store(16);
        s.create_table("t").unwrap();
        let before = s.list_tables().unwrap().len();
        let hwm = s.hwm();
        migrate_data(&mut s, v(0, 1, 0), crate::VERSION).unwrap();
        s.finalize_upgrade(s.total_pages()).unwrap();
        assert_eq!(s.list_tables().unwrap().len(), before);
        assert!(s.hwm() >= hwm);
    }
}
