//! USB Mass Storage **Bulk-Only Transport** wire formats and the SCSI CDBs
//! TablesOS uses — pure byte-level builders/parsers shared by host-controller
//! drivers. The xHCI driver predates this module and keeps its own inline
//! copies (left untouched on purpose: it is verified on real hardware); the
//! EHCI driver builds on these.

/// Command Block Wrapper: 31 bytes, little-endian, signature "USBC".
pub fn build_cbw(tag: u32, data_len: u32, data_in: bool, lun: u8, cb: &[u8]) -> [u8; 31] {
    let mut w = [0u8; 31];
    w[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes()); // 'USBC'
    w[4..8].copy_from_slice(&tag.to_le_bytes());
    w[8..12].copy_from_slice(&data_len.to_le_bytes());
    w[12] = if data_in { 0x80 } else { 0x00 };
    w[13] = lun;
    w[14] = cb.len() as u8;
    w[15..15 + cb.len()].copy_from_slice(cb);
    w
}

/// Command Status Wrapper: 13 bytes. Returns `(scsi_status, residue)` after
/// validating the signature and the tag.
pub fn parse_csw(csw: &[u8], expected_tag: u32) -> Result<(u8, u32), &'static str> {
    if csw.len() < 13 {
        return Err("CSW: short");
    }
    if csw[0..4] != 0x5342_5355u32.to_le_bytes() {
        return Err("CSW: bad signature");
    }
    if csw[4..8] != expected_tag.to_le_bytes() {
        return Err("CSW: tag mismatch");
    }
    let residue = u32::from_le_bytes(csw[8..12].try_into().unwrap());
    Ok((csw[12], residue))
}

// ---- SCSI command blocks ----------------------------------------------------

pub fn cdb_inquiry(len: u8) -> [u8; 6] {
    [0x12, 0, 0, 0, len, 0]
}

pub fn cdb_test_unit_ready() -> [u8; 6] {
    [0x00, 0, 0, 0, 0, 0]
}

pub fn cdb_read_capacity10() -> [u8; 10] {
    [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

pub fn cdb_read10(lba: u32, blocks: u16) -> [u8; 10] {
    [
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
    ]
}

/// `fua` asks for Force Unit Access (write-through past any device cache) —
/// see the xHCI driver's durability note; devices that reject it get a plain
/// retry there, and the same convention applies on EHCI.
pub fn cdb_write10(lba: u32, blocks: u16, fua: bool) -> [u8; 10] {
    [
        0x2A,
        if fua { 0x08 } else { 0x00 },
        (lba >> 24) as u8,
        (lba >> 16) as u8,
        (lba >> 8) as u8,
        lba as u8,
        0,
        (blocks >> 8) as u8,
        blocks as u8,
        0,
    ]
}
