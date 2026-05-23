//! Minimal PNG decoder — `no_std + alloc`, no `unsafe`.
//!
//! Decodes the subset of PNG that a graphical-AI export and the in-tree asset
//! tooling produce: **bit depth 8, non-interlaced**, color types
//! grayscale (0), RGB (2), palette (3), grayscale+alpha (4) and RGBA (6).
//! Output is always tightly-packed RGBA (`width * height * 4` bytes), which is
//! what the framebuffer blitter wants.
//!
//! The DEFLATE/zlib inflate is a compact implementation of the canonical
//! algorithm (puff-style Huffman decode). CRC of chunks is not verified — the
//! assets are embedded at build time, so corruption is a build problem, not a
//! runtime one.
//!
//! Host-tested (`cargo test -p pngdec`) against real PNGs in `tests/`.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

/// A decoded image as tightly-packed RGBA8 (row-major, top-left origin).
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// `width * height * 4` bytes, R,G,B,A per pixel.
    pub rgba: Vec<u8>,
}

/// Why a PNG could not be decoded. The kernel maps any of these to "use the
/// procedural fallback for this asset".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Missing/!= the 8-byte PNG signature.
    Signature,
    /// Ran off the end of the buffer.
    Truncated,
    /// A chunk was malformed (e.g. short IHDR).
    BadChunk,
    /// Valid PNG, but a feature we don't implement (bit depth != 8,
    /// interlaced, or an unknown color type / filter).
    Unsupported,
    /// The compressed image data did not inflate.
    Inflate,
    /// Zero width or height.
    Dimensions,
}

// ---- DEFLATE / zlib inflate ----------------------------------------------

/// LSB-first bit reader over the compressed stream.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, bitbuf: 0, bitcnt: 0 }
    }

    #[inline]
    fn bit(&mut self) -> Result<u32, Error> {
        if self.bitcnt == 0 {
            if self.pos >= self.data.len() {
                return Err(Error::Inflate);
            }
            self.bitbuf = self.data[self.pos] as u32;
            self.pos += 1;
            self.bitcnt = 8;
        }
        let b = self.bitbuf & 1;
        self.bitbuf >>= 1;
        self.bitcnt -= 1;
        Ok(b)
    }

    #[inline]
    fn bits(&mut self, n: u32) -> Result<u32, Error> {
        let mut v = 0u32;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }

    /// Drop the rest of the current byte (stored blocks are byte-aligned).
    fn align(&mut self) {
        self.bitcnt = 0;
    }
}

/// Canonical Huffman table built from a list of per-symbol code lengths.
struct Huffman {
    /// Number of codes of each length 0..=15.
    counts: [u16; 16],
    /// Symbols ordered by (length, symbol).
    symbols: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0; // length-0 means "symbol unused"
        let mut offs = [0u16; 16];
        let mut sum = 0u16;
        for i in 1..16 {
            offs[i] = sum;
            sum += counts[i];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    fn decode(&self, br: &mut BitReader) -> Result<u16, Error> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= br.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                let i = (index + (code - first)) as usize;
                return self.symbols.get(i).copied().ok_or(Error::Inflate);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(Error::Inflate)
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_lit() -> Huffman {
    let mut lengths = [0u8; 288];
    for (i, l) in lengths.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    Huffman::build(&lengths)
}

fn fixed_dist() -> Huffman {
    Huffman::build(&[5u8; 32])
}

fn read_dynamic(br: &mut BitReader) -> Result<(Huffman, Huffman), Error> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let mut cl = [0u8; 19];
    for &o in ORDER.iter().take(hclen) {
        cl[o] = br.bits(3)? as u8;
    }
    let cl_huff = Huffman::build(&cl);
    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    while i < total {
        let sym = cl_huff.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(Error::Inflate);
                }
                let prev = lengths[i - 1];
                let rep = 3 + br.bits(2)? as usize;
                for _ in 0..rep {
                    if i >= total {
                        return Err(Error::Inflate);
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let rep = 3 + br.bits(3)? as usize;
                for _ in 0..rep {
                    if i >= total {
                        return Err(Error::Inflate);
                    }
                    lengths[i] = 0;
                    i += 1;
                }
            }
            18 => {
                let rep = 11 + br.bits(7)? as usize;
                for _ in 0..rep {
                    if i >= total {
                        return Err(Error::Inflate);
                    }
                    lengths[i] = 0;
                    i += 1;
                }
            }
            _ => return Err(Error::Inflate),
        }
    }
    let lit = Huffman::build(&lengths[..hlit]);
    let dist = Huffman::build(&lengths[hlit..]);
    Ok((lit, dist))
}

fn inflate_block(
    br: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), Error> {
    loop {
        let sym = lit.decode(br)?;
        if sym == 256 {
            return Ok(());
        }
        if sym < 256 {
            out.push(sym as u8);
        } else {
            let s = (sym - 257) as usize;
            if s >= LEN_BASE.len() {
                return Err(Error::Inflate);
            }
            let len = LEN_BASE[s] as usize + br.bits(LEN_EXTRA[s] as u32)? as usize;
            let dsym = dist.decode(br)? as usize;
            if dsym >= DIST_BASE.len() {
                return Err(Error::Inflate);
            }
            let distance = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym] as u32)? as usize;
            if distance == 0 || distance > out.len() {
                return Err(Error::Inflate);
            }
            let start = out.len() - distance;
            for k in 0..len {
                let b = out[start + k];
                out.push(b);
            }
        }
    }
}

/// Inflate a raw DEFLATE stream (no zlib header).
fn inflate(data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut br = BitReader::new(data);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = br.bit()?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                br.align();
                if br.pos + 4 > br.data.len() {
                    return Err(Error::Inflate);
                }
                let len = br.data[br.pos] as usize | ((br.data[br.pos + 1] as usize) << 8);
                br.pos += 4; // LEN + NLEN
                if br.pos + len > br.data.len() {
                    return Err(Error::Inflate);
                }
                out.extend_from_slice(&br.data[br.pos..br.pos + len]);
                br.pos += len;
            }
            1 => inflate_block(&mut br, &mut out, &fixed_lit(), &fixed_dist())?,
            2 => {
                let (lit, dist) = read_dynamic(&mut br)?;
                inflate_block(&mut br, &mut out, &lit, &dist)?;
            }
            _ => return Err(Error::Inflate),
        }
        if bfinal == 1 {
            return Ok(out);
        }
    }
}

// ---- PNG container --------------------------------------------------------

#[inline]
fn be32(b: &[u8]) -> usize {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
}

#[inline]
fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Decode a PNG byte slice into RGBA8. See the module docs for the supported
/// subset.
pub fn decode(bytes: &[u8]) -> Result<Image, Error> {
    const SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if bytes.len() < 8 || bytes[..8] != SIG {
        return Err(Error::Signature);
    }

    let mut pos = 8;
    let (mut width, mut height) = (0usize, 0usize);
    let (mut bit_depth, mut color_type, mut interlace) = (0u8, 0u8, 0u8);
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();

    loop {
        if pos + 8 > bytes.len() {
            return Err(Error::Truncated);
        }
        let len = be32(&bytes[pos..pos + 4]);
        let ctype = &bytes[pos + 4..pos + 8];
        let dstart = pos + 8;
        if dstart + len + 4 > bytes.len() {
            return Err(Error::Truncated);
        }
        let data = &bytes[dstart..dstart + len];
        match ctype {
            b"IHDR" => {
                if len < 13 {
                    return Err(Error::BadChunk);
                }
                width = be32(&data[0..4]);
                height = be32(&data[4..8]);
                bit_depth = data[8];
                color_type = data[9];
                interlace = data[12];
            }
            b"PLTE" => {
                palette = data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"tRNS" => trns = data.to_vec(),
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        pos = dstart + len + 4; // data + 4-byte CRC
    }

    if width == 0 || height == 0 {
        return Err(Error::Dimensions);
    }
    if bit_depth != 8 || interlace != 0 {
        return Err(Error::Unsupported);
    }
    let channels: usize = match color_type {
        0 => 1, // grayscale
        2 => 3, // RGB
        3 => 1, // palette index
        4 => 2, // grayscale + alpha
        6 => 4, // RGBA
        _ => return Err(Error::Unsupported),
    };
    if idat.len() < 2 {
        return Err(Error::Truncated);
    }

    // Guard against absurd dimensions overflowing the allocation math.
    let pixels = width.checked_mul(height).ok_or(Error::Dimensions)?;
    let stride = width.checked_mul(channels).ok_or(Error::Dimensions)?;

    let raw = inflate(&idat[2..])?; // skip the 2-byte zlib header
    if raw.len() < (stride + 1) * height {
        return Err(Error::Inflate);
    }

    // Reverse the per-scanline filters in place into `img`.
    let bpp = channels;
    let mut img = vec![0u8; stride * height];
    for y in 0..height {
        let filter = raw[y * (stride + 1)];
        let row_in = &raw[y * (stride + 1) + 1..y * (stride + 1) + 1 + stride];
        for x in 0..stride {
            let rb = row_in[x] as i32;
            let a = if x >= bpp { img[y * stride + x - bpp] as i32 } else { 0 };
            let b = if y > 0 { img[(y - 1) * stride + x] as i32 } else { 0 };
            let c = if y > 0 && x >= bpp {
                img[(y - 1) * stride + x - bpp] as i32
            } else {
                0
            };
            let v = match filter {
                0 => rb,
                1 => rb + a,
                2 => rb + b,
                3 => rb + (a + b) / 2,
                4 => rb + paeth(a, b, c),
                _ => return Err(Error::Unsupported),
            };
            img[y * stride + x] = (v & 0xff) as u8;
        }
    }

    // Expand to RGBA.
    let mut rgba = vec![0u8; pixels * 4];
    for i in 0..pixels {
        let (r, g, b, al) = match color_type {
            0 => {
                let v = img[i];
                (v, v, v, 255)
            }
            2 => (img[i * 3], img[i * 3 + 1], img[i * 3 + 2], 255),
            3 => {
                let idx = img[i] as usize;
                let p = palette.get(idx).copied().unwrap_or([0, 0, 0]);
                let al = trns.get(idx).copied().unwrap_or(255);
                (p[0], p[1], p[2], al)
            }
            4 => {
                let v = img[i * 2];
                (v, v, v, img[i * 2 + 1])
            }
            6 => (img[i * 4], img[i * 4 + 1], img[i * 4 + 2], img[i * 4 + 3]),
            _ => unreachable!(),
        };
        rgba[i * 4] = r;
        rgba[i * 4 + 1] = g;
        rgba[i * 4 + 2] = b;
        rgba[i * 4 + 3] = al;
    }

    Ok(Image { width, height, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(img: &Image, x: usize, y: usize) -> (u8, u8, u8, u8) {
        let i = (y * img.width + x) * 4;
        (img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3])
    }

    // RGB (color type 2): a 4x4 with a known corner layout.
    #[test]
    fn decode_rgb_corners() {
        let img = decode(include_bytes!("../tests/rgb4.png")).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        // Generator paints: TL red, TR green, BL blue, BR white.
        assert_eq!(px(&img, 0, 0), (255, 0, 0, 255));
        assert_eq!(px(&img, 3, 0), (0, 255, 0, 255));
        assert_eq!(px(&img, 0, 3), (0, 0, 255, 255));
        assert_eq!(px(&img, 3, 3), (255, 255, 255, 255));
    }

    // RGBA (color type 6): alpha must survive.
    #[test]
    fn decode_rgba_alpha() {
        let img = decode(include_bytes!("../tests/rgba4.png")).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        // TL is opaque red, BR is fully transparent.
        assert_eq!(px(&img, 0, 0), (255, 0, 0, 255));
        assert_eq!(px(&img, 3, 3).3, 0);
    }

    // Palette (color type 3): index expansion via PLTE.
    #[test]
    fn decode_palette() {
        let img = decode(include_bytes!("../tests/pal16.png")).unwrap();
        assert_eq!((img.width, img.height), (16, 16));
        // A larger image forces real (dynamic-Huffman) compression too.
        assert_eq!(px(&img, 0, 0), (255, 0, 0, 255));
    }

    // A wider gradient image: exercises filters + back-references heavily and
    // checks the decode is self-consistent (size, opaque).
    #[test]
    fn decode_gradient_consistent() {
        let img = decode(include_bytes!("../tests/grad64.png")).unwrap();
        assert_eq!((img.width, img.height), (64, 64));
        assert_eq!(img.rgba.len(), 64 * 64 * 4);
        for p in img.rgba.chunks_exact(4) {
            assert_eq!(p[3], 255);
        }
    }

    #[test]
    fn rejects_non_png() {
        assert!(matches!(decode(b"not a png at all"), Err(Error::Signature)));
    }
}
