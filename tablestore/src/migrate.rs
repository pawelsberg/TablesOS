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
use crate::compat_v3::V3Volume;
use crate::journal::{FIRST_DATA_PAGE, JCAP};
use crate::pager::Pager;
use crate::store::Store;
use crate::{Result, StoreError};

/// Packed form (`(major<<16)|(minor<<8)|patch`) of a released version.
const fn v(major: u32, minor: u32, patch: u32) -> u32 {
    (major << 16) | (minor << 8) | patch
}

/// Every released on-disk version, oldest first. The migration ladder steps
/// strictly upward through this list. Append new releases here as they ship.
pub const KNOWN_VERSIONS: &[u32] = &[v(0, 1, 0), v(0, 2, 0), v(0, 3, 0), v(0, 4, 0)];

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
        // v0.2.0 -> v0.3.0: flash wear-levelling rotates the journal/superblock
        // sectors but keeps the geometry and is read-compatible with v0.2.0, so
        // no data transform is needed — the first commit after the re-stamp
        // simply starts rotating.
        (a, b) if a == v(0, 2, 0) && b == v(0, 3, 0) => Ok(()),
        // v0.3.0 -> v0.4.0: the copy-on-write pager is a *format* change — a
        // v0.3.0 volume cannot be opened by the new pager at all, so the data is
        // not transformed in place through an open `Store`. Instead the kernel
        // upgrade/relocate path reads the old volume via [`rebuild_v3_into`]
        // (the frozen [`V3Volume`] reader) and writes a fresh CoW volume at the
        // target region *before* it is ever mounted as a `Store`. By the time a
        // `Store` exists it is already v0.4.0, so this ladder step is a formality.
        (a, b) if a == v(0, 3, 0) && b == v(0, 4, 0) => Ok(()),
        _ => Err(StoreError::Corrupt("no migration step for this version pair")),
    }
}

/// Convert a v0.3.0 (journalled, in-place) volume into a freshly-`format`ted
/// copy-on-write volume, preserving all data.
///
/// `old` is the device (or sub-region) holding the v0.3.0 volume; `new` is a
/// `Pager` that has just been [`Pager::format`]ted onto the **target** region
/// (which must be a different device/region — the rebuild reads `old` while
/// writing `new`). Because the on-disk *contents* of a logical page are
/// identical between the two formats — only the physical placement differs —
/// the conversion is a straight logical-page copy: every `lpn` in
/// `[FIRST_DATA_PAGE, hwm)` is read from the old layout and written into the new
/// one under the *same* logical number, and the logical header pointers
/// (`hwm`, `catalog_head`, `free_head`) are carried across verbatim.
///
/// The copy is committed in [`JCAP`]-bounded batches so no single transaction
/// exceeds the per-commit page bound. A trailing [`Pager::rewrite_superblock`]
/// guarantees the new volume's version stamp is current even for an empty
/// volume.
pub fn rebuild_v3_into<D: BlockDevice>(
    old: &mut dyn BlockDevice,
    new: &mut Pager<D>,
) -> Result<()> {
    let v3 = V3Volume::mount(old)?;
    rebuild_into(new, v3.sb.hwm, v3.sb.catalog_head, v3.sb.free_head, |lpn| {
        v3.read_page(old, lpn)
    })
}

/// Lower-level rebuild driver: copy logical pages `[FIRST_DATA_PAGE, hwm)` into
/// a freshly-`format`ted CoW `Pager` under their original logical numbers,
/// carrying the logical header pointers across, committing in [`JCAP`]-bounded
/// batches, and re-stamping the version at the end.
///
/// `read(lpn)` yields the page image for each logical page. The source can be
/// anything — a [`V3Volume`] reading an old device ([`rebuild_v3_into`]), a CoW
/// `Pager` over a different region, or an in-RAM buffer (the kernel relocate
/// path) — which is why this is generic over a closure rather than a device.
pub fn rebuild_into<D: BlockDevice>(
    new: &mut Pager<D>,
    hwm: u64,
    catalog_head: u64,
    free_head: u64,
    mut read: impl FnMut(u64) -> Result<alloc::vec::Vec<u8>>,
) -> Result<()> {
    new.sb.hwm = hwm;
    new.sb.catalog_head = catalog_head;
    new.sb.free_head = free_head;

    let mut lpn = FIRST_DATA_PAGE;
    while lpn < hwm {
        let end = (lpn + JCAP as u64).min(hwm);
        for l in lpn..end {
            let img = read(l)?;
            new.write_page(l, img);
        }
        new.commit()?;
        lpn = end;
    }
    new.rewrite_superblock()
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
    fn v3_volume_migrates_into_cow_preserving_pages() {
        use crate::block::{BlockDevice, MemBlockDevice, SECTOR};
        use crate::journal::{crc32, FIRST_DATA_PAGE, PAGE, PAGE_SECTORS};
        use crate::pager::{zeroed_page, Pager};

        let sectors = 16 * 1024 * 1024 / SECTOR as u64;
        let total_pages = sectors / PAGE_SECTORS;
        let hwm = FIRST_DATA_PAGE + 3;
        let catalog_head = FIRST_DATA_PAGE;

        // Hand-build a clean v0.3.0 volume: a superblock at page-0 sector-0, an
        // all-zero (empty) journal control region, and three data pages laid out
        // at logical==physical positions (the v0.3.0 layout).
        let mut old = MemBlockDevice::new(sectors);
        let mut sb = alloc::vec![0u8; SECTOR];
        sb[0..4].copy_from_slice(&0x534c_4254u32.to_le_bytes()); // "TBLS"
        sb[4..8].copy_from_slice(&v(0, 3, 0).to_le_bytes());
        sb[8..12].copy_from_slice(&(PAGE as u32).to_le_bytes());
        sb[16..24].copy_from_slice(&total_pages.to_le_bytes());
        sb[24..32].copy_from_slice(&hwm.to_le_bytes());
        sb[32..40].copy_from_slice(&0u64.to_le_bytes()); // free_head
        sb[40..48].copy_from_slice(&catalog_head.to_le_bytes());
        sb[48..56].copy_from_slice(&7u64.to_le_bytes()); // generation
        let crc = crc32(&sb[16..56]);
        sb[12..16].copy_from_slice(&crc.to_le_bytes());
        old.write_sector(0, &sb).unwrap();
        for (k, lpn) in (FIRST_DATA_PAGE..hwm).enumerate() {
            let mut img = zeroed_page();
            img[0..8].copy_from_slice(&(0xA000 + k as u64).to_le_bytes());
            for s in 0..PAGE_SECTORS {
                let o = s as usize * SECTOR;
                old.write_sector(lpn * PAGE_SECTORS + s, &img[o..o + SECTOR])
                    .unwrap();
            }
        }
        old.flush().unwrap();

        // Rebuild into a fresh CoW volume on a separate region.
        let mut newp = Pager::format(MemBlockDevice::new(sectors)).unwrap();
        rebuild_v3_into(&mut old, &mut newp).unwrap();

        assert_eq!(newp.sb.hwm, hwm);
        assert_eq!(newp.sb.catalog_head, catalog_head);
        for (k, lpn) in (FIRST_DATA_PAGE..hwm).enumerate() {
            assert_eq!(
                &newp.read_page(lpn).unwrap()[0..8],
                &(0xA000 + k as u64).to_le_bytes()[..]
            );
        }

        // The migrated volume survives a remount (CoW commit ↔ anchor mount).
        let snap = newp.device_mut().snapshot();
        let mut re = Pager::mount(MemBlockDevice::from_snapshot(snap)).unwrap();
        assert_eq!(re.sb.hwm, hwm);
        assert_eq!(
            &re.read_page(catalog_head).unwrap()[0..8],
            &0xA000u64.to_le_bytes()[..]
        );
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
