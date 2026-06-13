//! Minimal FAT16 image generator for the EFI System Partition.
//!
//! UEFI firmware can only start a boot loader from a FAT file system, so the
//! image carries one small (4 MiB) FAT16 partition holding exactly one file:
//! `\EFI\BOOT\BOOTX64.EFI` (the default removable-media boot path). Nothing
//! at runtime ever reads or writes it — the kernel still treats the device as
//! raw sectors — so this is a write-once, build-time structure and the
//! generator supports nothing beyond what that single file needs: 8.3 names,
//! one root-level directory chain, one contiguous file.

const SECTOR: usize = 512;

/// 4 MiB. With 1 sector per cluster that yields ~8095 data clusters — above
/// the 4085-cluster floor that makes the volume FAT16 (not FAT12), which
/// keeps the FAT entries simple 16-bit words.
pub const ESP_SECTORS: u64 = 8192;

const RESERVED_SECS: usize = 1;
const FAT_SECS: usize = 32; // 8192 two-byte entries — covers every cluster
const N_FATS: usize = 2;
const ROOT_ENTRIES: usize = 512;
const ROOT_SECS: usize = ROOT_ENTRIES * 32 / SECTOR;
const DATA_START_SEC: usize = RESERVED_SECS + N_FATS * FAT_SECS + ROOT_SECS;

/// Build the full ESP image: boot sector, two FATs, root directory, and
/// `/EFI/BOOT/BOOTX64.EFI` with `loader` as its contents. `hidden_lba` is the
/// partition's absolute start LBA (the BPB "hidden sectors" field).
pub fn build_esp(loader: &[u8], hidden_lba: u32) -> Vec<u8> {
    let total_clusters = ESP_SECTORS as usize - DATA_START_SEC;
    let file_clusters = (loader.len() + SECTOR - 1) / SECTOR;
    // Clusters 2 and 3 are the /EFI and /EFI/BOOT directories; the file body
    // starts at cluster 4.
    assert!(
        2 + file_clusters <= total_clusters,
        "BOOTX64.EFI too large for the ESP"
    );

    let mut img = vec![0u8; ESP_SECTORS as usize * SECTOR];

    // ---- boot sector / BPB ----
    let b = &mut img[..SECTOR];
    b[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]); // jmp short + nop
    b[3..11].copy_from_slice(b"TABLESOS"); // OEM name
    b[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    b[13] = 1; // sectors per cluster
    b[14..16].copy_from_slice(&(RESERVED_SECS as u16).to_le_bytes());
    b[16] = N_FATS as u8;
    b[17..19].copy_from_slice(&(ROOT_ENTRIES as u16).to_le_bytes());
    b[19..21].copy_from_slice(&(ESP_SECTORS as u16).to_le_bytes());
    b[21] = 0xF8; // media: fixed disk
    b[22..24].copy_from_slice(&(FAT_SECS as u16).to_le_bytes());
    b[24..26].copy_from_slice(&63u16.to_le_bytes()); // sectors/track (CHS lore)
    b[26..28].copy_from_slice(&255u16.to_le_bytes()); // heads
    b[28..32].copy_from_slice(&hidden_lba.to_le_bytes());
    b[36] = 0x80; // BIOS drive number
    b[38] = 0x29; // extended boot signature
    b[39..43].copy_from_slice(&0x7AB1_E505u32.to_le_bytes()); // volume id
    b[43..54].copy_from_slice(b"TABLESOSEFI"); // volume label (11 chars)
    b[54..62].copy_from_slice(b"FAT16   ");
    b[510] = 0x55;
    b[511] = 0xAA;

    // ---- FAT (built once, copied to both slots) ----
    let mut fat = vec![0u8; FAT_SECS * SECTOR];
    let set = |cluster: usize, value: u16, fat: &mut [u8]| {
        fat[cluster * 2..cluster * 2 + 2].copy_from_slice(&value.to_le_bytes());
    };
    set(0, 0xFFF8, &mut fat); // media byte + fill
    set(1, 0xFFFF, &mut fat);
    set(2, 0xFFFF, &mut fat); // /EFI — single-cluster directory
    set(3, 0xFFFF, &mut fat); // /EFI/BOOT — single-cluster directory
    for i in 0..file_clusters {
        let cluster = 4 + i;
        let next = if i + 1 == file_clusters { 0xFFFF } else { (cluster + 1) as u16 };
        set(cluster, next, &mut fat);
    }
    for f in 0..N_FATS {
        let off = (RESERVED_SECS + f * FAT_SECS) * SECTOR;
        img[off..off + fat.len()].copy_from_slice(&fat);
    }

    // ---- directories ----
    let root_off = (RESERVED_SECS + N_FATS * FAT_SECS) * SECTOR;
    dir_entry(&mut img, root_off, b"EFI        ", 0x10, 2, 0);

    let efi_off = cluster_off(2);
    dir_entry(&mut img, efi_off, b".          ", 0x10, 2, 0);
    dir_entry(&mut img, efi_off + 32, b"..         ", 0x10, 0, 0);
    dir_entry(&mut img, efi_off + 64, b"BOOT       ", 0x10, 3, 0);

    let boot_off = cluster_off(3);
    dir_entry(&mut img, boot_off, b".          ", 0x10, 3, 0);
    dir_entry(&mut img, boot_off + 32, b"..         ", 0x10, 2, 0);
    dir_entry(
        &mut img,
        boot_off + 64,
        b"BOOTX64 EFI",
        0x20,
        4,
        loader.len() as u32,
    );

    // ---- file body ----
    let off = cluster_off(4);
    img[off..off + loader.len()].copy_from_slice(loader);

    img
}

/// Byte offset of a data cluster inside the ESP image.
fn cluster_off(cluster: usize) -> usize {
    (DATA_START_SEC + (cluster - 2)) * SECTOR
}

/// Write one 32-byte 8.3 directory entry.
fn dir_entry(img: &mut [u8], off: usize, name: &[u8; 11], attr: u8, cluster: u16, size: u32) {
    let e = &mut img[off..off + 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    // A fixed, valid modification date (2026-06-11); FAT has no "no date".
    let date: u16 = ((2026 - 1980) << 9) | (6 << 5) | 11;
    e[24..26].copy_from_slice(&date.to_le_bytes());
    e[26..28].copy_from_slice(&cluster.to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
}
