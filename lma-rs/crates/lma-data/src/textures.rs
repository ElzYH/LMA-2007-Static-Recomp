//! The disc's texture files as RGBA8 (the asset converter, docs/NATIVE_GAME.md): DDS (DXT1, DXT3,
//! DXT5 and 32-bit A8R8G8B8: plain PC block data, not console-tiled; level 0 only) and TIM2
//! (`.tm2`: 4-bit and 8-bit palettised, the palette in the PS2 CLUT order for 256 colours as
//! `tools/badge_tm2.py`, and 16/24/32-bit direct). Byte layouts only; no file is read here.

use crate::{Error, Result};

/// An 8-bit RGBA picture, rows top to bottom.
#[derive(Clone, Debug, PartialEq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

fn le32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le16(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn bad(what: &str) -> Error {
    Error::Format(what.into())
}

/// Whether `d` starts like one of the formats `decode` reads.
pub fn is_texture(d: &[u8]) -> bool {
    matches!(d.get(0..4), Some(b"DDS ") | Some(b"TIM2"))
}

/// A DDS or TIM2 file to RGBA8.
pub fn decode(d: &[u8]) -> Result<Rgba> {
    match d.get(0..4) {
        Some(b"DDS ") => dds(d),
        Some(b"TIM2") => tim2(d),
        _ => Err(bad("not a DDS or TIM2 texture")),
    }
}

// ---------------------------------------------------------------- DDS

fn dds(d: &[u8]) -> Result<Rgba> {
    let height = le32(d, 12).ok_or_else(|| bad("DDS: short header"))?;
    let width = le32(d, 16).ok_or_else(|| bad("DDS: short header"))?;
    let pf_flags = le32(d, 80).ok_or_else(|| bad("DDS: short header"))?;
    let fourcc: [u8; 4] = d.get(84..88).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("DDS: short header"))?;
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(bad("DDS: bad size"));
    }
    let body = d.get(128..).ok_or_else(|| bad("DDS: short header"))?;
    if pf_flags & 4 != 0 {
        let (bytes_per_block, dxt) = match &fourcc {
            b"DXT1" => (8, 1),
            b"DXT2" | b"DXT3" => (16, 3),
            b"DXT4" | b"DXT5" => (16, 5),
            other => return Err(Error::Format(format!("DDS: unsupported fourcc {}", String::from_utf8_lossy(&other[..])))),
        };
        let (bw, bh) = (width.div_ceil(4) as usize, height.div_ceil(4) as usize);
        let need = bw * bh * bytes_per_block;
        let body = body.get(..need).ok_or_else(|| bad("DDS: block data short"))?;
        let mut out = vec![0u8; (width * height * 4) as usize];
        for by in 0..bh {
            for bx in 0..bw {
                let b = &body[(by * bw + bx) * bytes_per_block..][..bytes_per_block];
                let px = dxt_block(b, dxt);
                for py in 0..4 {
                    for pxi in 0..4 {
                        let (x, y) = (bx as u32 * 4 + pxi as u32, by as u32 * 4 + py as u32);
                        if x < width && y < height {
                            let at = ((y * width + x) * 4) as usize;
                            out[at..at + 4].copy_from_slice(&px[py * 4 + pxi]);
                        }
                    }
                }
            }
        }
        return Ok(Rgba { width, height, data: out });
    }
    let bits = le32(d, 88).ok_or_else(|| bad("DDS: short header"))?;
    let (rm, gm, bm, am) = (le32(d, 92).unwrap_or(0), le32(d, 96).unwrap_or(0), le32(d, 100).unwrap_or(0), le32(d, 104).unwrap_or(0));
    if bits != 32 {
        return Err(Error::Format(format!("DDS: unsupported uncompressed format, {bits} bits")));
    }
    let need = (width * height * 4) as usize;
    let body = body.get(..need).ok_or_else(|| bad("DDS: pixel data short"))?;
    let chan = |v: u32, mask: u32| -> u8 {
        if mask == 0 {
            return 255;
        }
        let shift = mask.trailing_zeros();
        let width_bits = (mask >> shift).count_ones();
        let raw = (v & mask) >> shift;
        ((raw * 255) / ((1u32 << width_bits) - 1)) as u8
    };
    let mut out = Vec::with_capacity(need);
    for p in body.chunks_exact(4) {
        let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        out.extend_from_slice(&[chan(v, rm), chan(v, gm), chan(v, bm), chan(v, am)]);
    }
    Ok(Rgba { width, height, data: out })
}

fn rgb565(c: u16) -> [u8; 4] {
    let c = c as u32;
    [(((c >> 11) & 31) * 255 / 31) as u8, (((c >> 5) & 63) * 255 / 63) as u8, ((c & 31) * 255 / 31) as u8, 255]
}

/// One 4x4 block of DXT1 (8 bytes), DXT3 or DXT5 (16 bytes: alpha block then colour block), as
/// 16 RGBA texels in row order.
fn dxt_block(b: &[u8], dxt: u8) -> [[u8; 4]; 16] {
    let cb = if dxt == 1 { b } else { &b[8..16] };
    let (c0, c1) = (u16::from_le_bytes([cb[0], cb[1]]), u16::from_le_bytes([cb[2], cb[3]]));
    let (a, bb) = (rgb565(c0), rgb565(c1));
    let mix = |x: u8, y: u8, wx: u32, wy: u32| ((x as u32 * wx + y as u32 * wy) / (wx + wy)) as u8;
    let pal: [[u8; 4]; 4] = if c0 > c1 || dxt != 1 {
        [a, bb, [mix(a[0], bb[0], 2, 1), mix(a[1], bb[1], 2, 1), mix(a[2], bb[2], 2, 1), 255], [mix(a[0], bb[0], 1, 2), mix(a[1], bb[1], 1, 2), mix(a[2], bb[2], 1, 2), 255]]
    } else {
        [a, bb, [mix(a[0], bb[0], 1, 1), mix(a[1], bb[1], 1, 1), mix(a[2], bb[2], 1, 1), 255], [0, 0, 0, 0]]
    };
    let bits = u32::from_le_bytes([cb[4], cb[5], cb[6], cb[7]]);
    let mut out = [[0u8; 4]; 16];
    for i in 0..16 {
        out[i] = pal[((bits >> (2 * i)) & 3) as usize];
    }
    match dxt {
        3 => {
            let a = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            for i in 0..16 {
                out[i][3] = (((a >> (4 * i)) & 15) * 17) as u8;
            }
        }
        5 => {
            let (a0, a1) = (b[0] as u32, b[1] as u32);
            let mut apal = [0u8; 8];
            apal[0] = a0 as u8;
            apal[1] = a1 as u8;
            if a0 > a1 {
                for i in 1..7 {
                    apal[i + 1] = (((7 - i as u32) * a0 + i as u32 * a1) / 7) as u8;
                }
            } else {
                for i in 1..5 {
                    apal[i + 1] = (((5 - i as u32) * a0 + i as u32 * a1) / 5) as u8;
                }
                apal[6] = 0;
                apal[7] = 255;
            }
            let bits = u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0]);
            for i in 0..16 {
                out[i][3] = apal[((bits >> (3 * i)) & 7) as usize];
            }
        }
        _ => {}
    }
    out
}

// ---------------------------------------------------------------- TIM2

/// Palette index -> position in a stored 256-colour CLUT (the PS2 order: per 32 colours the
/// blocks of 8 go 0, 2, 1, 3), as `tools/badge_tm2.py`.
fn clut_swizzle(i: usize) -> usize {
    let (blk, k) = (i / 32 * 32, i % 32);
    blk + [0, 2, 1, 3][k / 8] * 8 + k % 8
}

fn tim2(d: &[u8]) -> Result<Rgba> {
    // file header: "TIM2", version, format (0: picture header at 0x10; 1: at 0x80), picture count
    let format = *d.get(5).ok_or_else(|| bad("TIM2: short header"))?;
    let ph = if format == 1 { 0x80 } else { 0x10 };
    let short = || bad("TIM2: short picture header");
    let clut_size = le32(d, ph + 4).ok_or_else(short)? as usize;
    let image_size = le32(d, ph + 8).ok_or_else(short)? as usize;
    let header_size = le16(d, ph + 12).ok_or_else(short)? as usize;
    let clut_colours = le16(d, ph + 14).ok_or_else(short)? as usize;
    // picture header: total size, CLUT size, image size (u32 each), header size, CLUT colours (u16),
    // picture format, mip count, CLUT type, image type (u8 each), width, height (u16)
    let clut_type = *d.get(ph + 18).ok_or_else(short)?;
    let image_type = *d.get(ph + 19).ok_or_else(short)?;
    let width = le16(d, ph + 20).ok_or_else(short)? as u32;
    let height = le16(d, ph + 22).ok_or_else(short)? as u32;
    if width == 0 || height == 0 || header_size < 0x30 || header_size > 0x1000 {
        return Err(bad("TIM2: bad picture header"));
    }
    let image = d.get(ph + header_size..ph + header_size + image_size).ok_or_else(|| bad("TIM2: image data short"))?;
    let clut = d.get(ph + header_size + image_size..ph + header_size + image_size + clut_size).ok_or_else(|| bad("TIM2: palette short"))?;
    let n = (width * height) as usize;
    // a palette entry: the CLUT type's low 6 bits give the colour format (1: 16-bit, 2: 24-bit, 3: 32-bit)
    let entry = |c: &[u8], fmt: u8, i: usize| -> [u8; 4] {
        match fmt & 0x3F {
            1 => {
                let v = u16::from_le_bytes([c[i * 2], c[i * 2 + 1]]) as u32;
                [((v & 31) * 255 / 31) as u8, (((v >> 5) & 31) * 255 / 31) as u8, (((v >> 10) & 31) * 255 / 31) as u8, if v >> 15 == 1 { 255 } else { 0 }]
            }
            2 => [c[i * 3], c[i * 3 + 1], c[i * 3 + 2], 255],
            _ => [c[i * 4], c[i * 4 + 1], c[i * 4 + 2], c[i * 4 + 3].saturating_mul(2).min(255)], // PS2 alpha: 0x80 = opaque
        }
    };
    let mut out = Vec::with_capacity(n * 4);
    match image_type {
        4 | 5 => {
            let bytes_per_entry = match clut_type & 0x3F { 1 => 2, 2 => 3, _ => 4 };
            if clut_colours == 0 {
                return Err(bad("TIM2: no palette")); // the index clamp below needs one colour at least
            }
            if clut.len() < clut_colours * bytes_per_entry {
                return Err(bad("TIM2: palette short"));
            }
            let swizzled = clut_colours == 256 && clut_type & 0x80 == 0; // the PS2 order unless the CLUT is flagged linear
            for p in 0..n {
                let idx = if image_type == 4 {
                    let b = *image.get(p / 2).ok_or_else(|| bad("TIM2: image data short"))?;
                    if p % 2 == 0 { b & 15 } else { b >> 4 }
                } else {
                    *image.get(p).ok_or_else(|| bad("TIM2: image data short"))?
                } as usize;
                let i = if swizzled { clut_swizzle(idx) } else { idx };
                out.extend_from_slice(&entry(clut, clut_type, i.min(clut_colours - 1)));
            }
        }
        1 => {
            if image.len() < n * 2 {
                return Err(bad("TIM2: image data short"));
            }
            for p in 0..n {
                out.extend_from_slice(&entry(image, 1, p));
            }
        }
        2 => {
            if image.len() < n * 3 {
                return Err(bad("TIM2: image data short"));
            }
            for p in 0..n {
                out.extend_from_slice(&entry(image, 2, p));
            }
        }
        3 => {
            if image.len() < n * 4 {
                return Err(bad("TIM2: image data short"));
            }
            for p in 0..n {
                out.extend_from_slice(&entry(image, 3, p));
            }
        }
        t => return Err(Error::Format(format!("TIM2: unsupported image type {t}"))),
    }
    Ok(Rgba { width, height, data: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dds_header(w: u32, h: u32, fourcc: &[u8; 4]) -> Vec<u8> {
        let mut d = vec![0u8; 128];
        d[..4].copy_from_slice(b"DDS ");
        d[4..8].copy_from_slice(&124u32.to_le_bytes());
        d[12..16].copy_from_slice(&h.to_le_bytes());
        d[16..20].copy_from_slice(&w.to_le_bytes());
        d[76..80].copy_from_slice(&32u32.to_le_bytes());
        d[80..84].copy_from_slice(&4u32.to_le_bytes()); // DDPF_FOURCC
        d[84..88].copy_from_slice(fourcc);
        d
    }

    #[test]
    fn dxt1_block_decodes_its_four_colours_and_the_transparent_case() {
        // c0 = pure red (0xF800), c1 = pure blue (0x001F), c0 > c1: four opaque colours; indices 0,1,2,3 down the first column
        let mut d = dds_header(4, 4, b"DXT1");
        d.extend_from_slice(&[0x00, 0xF8, 0x1F, 0x00, 0b00_00_00_00, 0b01_01_01_01, 0b10_10_10_10, 0b11_11_11_11]);
        let t = decode(&d).unwrap();
        assert_eq!((t.width, t.height), (4, 4));
        let px = |x: usize, y: usize| &t.data[(y * 4 + x) * 4..][..4];
        assert_eq!(px(0, 0), &[255, 0, 0, 255]);
        assert_eq!(px(0, 1), &[0, 0, 255, 255]);
        assert_eq!(px(0, 2), &[170, 0, 85, 255], "two thirds red");
        assert_eq!(px(0, 3), &[85, 0, 170, 255]);
        // c0 <= c1: index 3 is transparent black
        let mut d = dds_header(4, 4, b"DXT1");
        d.extend_from_slice(&[0x1F, 0x00, 0x00, 0xF8, 0xFF, 0xFF, 0xFF, 0xFF]);
        let t = decode(&d).unwrap();
        assert_eq!(&t.data[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn dxt5_alpha_ramp_and_a_clipped_edge() {
        // 6x2 texture: two blocks wide, the second block's right half and rows 2..4 are outside the picture
        let mut d = dds_header(6, 2, b"DXT5");
        for _ in 0..2 {
            d.extend_from_slice(&[255, 0]); // a0 > a1: an 8-step ramp
            d.extend_from_slice(&[0b10_001_000, 0b0_100_011_0, 0b11_010_10, 0, 0, 0]); // indices 0,1,2,3,4,5,6,7 for texels 0..8
            d.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]); // white
        }
        let t = decode(&d).unwrap();
        assert_eq!((t.width, t.height), (6, 2));
        assert_eq!(t.data.len(), 6 * 2 * 4);
        let a = |x: usize, y: usize| t.data[(y * 6 + x) * 4 + 3];
        assert_eq!([a(0, 0), a(1, 0), a(2, 0), a(3, 0)], [255, 0, 218, 182], "a0, a1, then the ramp");
        assert_eq!(&t.data[..3], &[255, 255, 255]);
    }

    #[test]
    fn uncompressed_argb_maps_its_channels() {
        let mut d = dds_header(1, 1, b"\0\0\0\0");
        d[80..84].copy_from_slice(&0x41u32.to_le_bytes()); // RGB | ALPHAPIXELS
        d[88..92].copy_from_slice(&32u32.to_le_bytes());
        d[92..96].copy_from_slice(&0x00FF0000u32.to_le_bytes());
        d[96..100].copy_from_slice(&0x0000FF00u32.to_le_bytes());
        d[100..104].copy_from_slice(&0x000000FFu32.to_le_bytes());
        d[104..108].copy_from_slice(&0xFF000000u32.to_le_bytes());
        d.extend_from_slice(&0x80112233u32.to_le_bytes());
        assert_eq!(decode(&d).unwrap().data, vec![0x11, 0x22, 0x33, 0x80]);
    }

    /// A TIM2 picture header at `ph` (format 1 = 0x80): sizes, counts, types, then the image bytes and the CLUT.
    fn tim2_file(format: u8, w: u16, h: u16, image_type: u8, clut_type: u8, image: &[u8], clut: &[u8]) -> Vec<u8> {
        let ph = if format == 1 { 0x80 } else { 0x10 };
        let mut d = vec![0u8; ph];
        d[..4].copy_from_slice(b"TIM2");
        d[4] = 4;
        d[5] = format;
        d[6] = 1;
        let mut p = vec![0u8; 0x30];
        p[0..4].copy_from_slice(&((0x30 + image.len() + clut.len()) as u32).to_le_bytes());
        p[4..8].copy_from_slice(&(clut.len() as u32).to_le_bytes());
        p[8..12].copy_from_slice(&(image.len() as u32).to_le_bytes());
        p[12..14].copy_from_slice(&0x30u16.to_le_bytes());
        let colours = match clut_type & 0x3F { 1 => clut.len() / 2, 2 => clut.len() / 3, _ => clut.len() / 4 } as u16;
        p[14..16].copy_from_slice(&colours.to_le_bytes());
        p[18] = clut_type;
        p[19] = image_type;
        p[20..22].copy_from_slice(&w.to_le_bytes());
        p[22..24].copy_from_slice(&h.to_le_bytes());
        d.extend_from_slice(&p);
        d.extend_from_slice(image);
        d.extend_from_slice(clut);
        d
    }

    #[test]
    fn tim2_8bit_uses_the_ps2_clut_order_and_both_header_offsets() {
        // a 256-colour RGBA palette where entry k at its *stored* position is (k, 0, 0, 0x80)
        let mut clut = Vec::new();
        for k in 0..256u32 {
            clut.extend_from_slice(&[k as u8, 0, 0, 0x80]);
        }
        // pixels with palette indices 8 and 16: stored at positions 16 and 8 (the blocks of 8 swap)
        for format in [0u8, 1] {
            let d = tim2_file(format, 2, 1, 5, 3, &[8, 16], &clut);
            let t = decode(&d).unwrap();
            assert_eq!((t.width, t.height), (2, 1));
            assert_eq!(t.data, vec![16, 0, 0, 255, 8, 0, 0, 255], "format {format}");
        }
    }

    #[test]
    fn tim2_4bit_reads_the_low_nibble_first_and_a_linear_16_colour_palette() {
        let mut clut = Vec::new();
        for k in 0..16u32 {
            clut.extend_from_slice(&[0, k as u8 * 16, 0, 0x80]);
        }
        let d = tim2_file(1, 2, 1, 4, 3, &[0x21], &clut); // pixel 0 = index 1, pixel 1 = index 2
        assert_eq!(decode(&d).unwrap().data, vec![0, 16, 0, 255, 0, 32, 0, 255]);
    }

    #[test]
    fn tim2_16bit_direct_colour() {
        // ABGR1555: red 31, alpha set
        let d = tim2_file(1, 1, 1, 1, 0, &[0x1F, 0x80], &[]);
        assert_eq!(decode(&d).unwrap().data, vec![255, 0, 0, 255]);
    }

    #[test]
    fn a_palettised_tim2_without_a_palette_is_refused_not_a_panic() {
        let d = tim2_file(1, 2, 1, 5, 3, &[0, 1], &[]); // 8-bit pixels, zero CLUT colours
        assert!(decode(&d).unwrap_err().to_string().contains("no palette"));
        let d = tim2_file(0, 1, 1, 4, 3, &[0x10], &[]);
        assert!(decode(&d).is_err());
    }

    #[test]
    fn rejects_other_data() {
        assert!(decode(b"BG2\0junk").is_err());
        assert!(decode(b"DDS short").is_err());
        assert!(!is_texture(b"BG2\0"));
        assert!(is_texture(b"DDS |"));
    }
}
