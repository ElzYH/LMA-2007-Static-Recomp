//! The game's menu fonts (#766 c): the Swiss 721 faces the front end draws with, read from the
//! user's own disc at run time so the launcher can look like LMA.
//!
//! `Common/GameData/Textures/Fonts.mng` (a BG2 archive) holds one nested archive per face
//! (`swiss_heavy_23.mng`, `swiss_medium_13.mng`, ...), and each of those a `.tm2` glyph sheet and a
//! `.pcf` metrics file under the face's name:
//! - `.tm2` (TIM2): a file header (byte 5 = 1: the picture at 0x80, else 0x10), then the picture header (total size, palette size, image
//!   size, header size, palette colours, format, mipmaps, palette type, image type, width, height);
//!   4-bit (16 colours: the fonts) or 8-bit palette indices follow the header and the palette
//!   follows the pixels: RGBA entries, alpha 0..128; 256-colour palettes are stored in the PS2's
//!   CSM1 order (per 32 colours: 0-7, 16-23, 8-15, 24-31) unless the palette type has bit 0x80.
//! - `.pcf`: six u32 (version 3, glyph count, line height, then three more: 8, 2, 0 in
//!   SWISS_MEDIUM_13, the second of them used as the spacing between glyphs), a 256-entry u16 map
//!   from character to glyph (0xFFFF: none), then a 16-byte record per glyph: x, y, width, height and
//!   the character as u16, then six bytes not needed to draw.
//!
//! Nothing from the game is bundled: everything is read from the user's own copy.

use crate::archive::Archive;
use crate::{Error, Result};

/// One glyph's rectangle in the sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// A face: its glyph sheet as RGBA and the glyphs' places in it.
#[derive(Clone, Debug)]
pub struct Font {
    pub name: String,
    /// The sheet, `width * height * 4` bytes of RGBA (alpha 0..255).
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The distance between lines.
    pub line_height: u16,
    /// Extra pixels after each glyph.
    pub spacing: u16,
    glyphs: [Option<Glyph>; 256],
}

impl Font {
    /// The face from its `.pcf` metrics and `.tm2` sheet.
    pub fn parse(name: &str, pcf: &[u8], tm2: &[u8]) -> Result<Self> {
        let (rgba, width, height) = decode_tm2(tm2)?;
        let u32_at = |o: usize| pcf.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        let u16_at = |o: usize| pcf.get(o..o + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()));
        let bad = || Error::Format(format!("{name}.pcf: too short"));
        if u32_at(0).ok_or_else(bad)? != 3 {
            return Err(Error::Format(format!("{name}.pcf: version {} (3 expected)", u32_at(0).unwrap())));
        }
        let count = u32_at(4).ok_or_else(bad)? as usize;
        let line_height = u32_at(8).ok_or_else(bad)? as u16;
        let spacing = u32_at(16).ok_or_else(bad)? as u16;
        const MAP: usize = 0x18;
        const RECORDS: usize = MAP + 512;
        let mut glyphs = [None; 256];
        for c in 0..256 {
            let i = u16_at(MAP + 2 * c).ok_or_else(bad)?;
            if i == 0xFFFF {
                continue;
            }
            if i as usize >= count {
                return Err(Error::Format(format!("{name}.pcf: char {c} maps to glyph {i} of {count}")));
            }
            let r = RECORDS + 16 * i as usize;
            let g = Glyph { x: u16_at(r).ok_or_else(bad)?, y: u16_at(r + 2).ok_or_else(bad)?, w: u16_at(r + 4).ok_or_else(bad)?, h: u16_at(r + 6).ok_or_else(bad)? };
            if g.x as u32 + g.w as u32 > width || g.y as u32 + g.h as u32 > height {
                return Err(Error::Format(format!("{name}.pcf: glyph {i} at {},{} {}x{} is outside the {width}x{height} sheet", g.x, g.y, g.w, g.h)));
            }
            glyphs[c] = Some(g);
        }
        Ok(Font { name: name.to_string(), rgba, width, height, line_height, spacing, glyphs })
    }

    /// A face out of the game's `Fonts.mng`, by name (`"swiss_heavy_23"`, any case).
    pub fn from_fonts_mng(fonts_mng: &[u8], name: &str) -> Result<Self> {
        let outer = Archive::parse(fonts_mng)?;
        let want = |e: &str, ext: &str| e.eq_ignore_ascii_case(&format!("{name}.{ext}"));
        let mut data = outer.entries.iter().find(|(n, _)| want(n, "mng")).map(|(_, d)| d.clone()).ok_or_else(|| Error::Format(format!("no font {name} in Fonts.mng")))?;
        // the face's archive can hold another of the same name before the files themselves
        for _ in 0..4 {
            let inner = Archive::parse(&data)?;
            let pcf = inner.entries.iter().find(|(n, _)| want(n, "pcf"));
            let tm2 = inner.entries.iter().find(|(n, _)| want(n, "tm2"));
            if let (Some((_, pcf)), Some((_, tm2))) = (pcf, tm2) {
                return Self::parse(name, pcf, tm2);
            }
            data = inner.entries.iter().find(|(n, _)| want(n, "mng")).map(|(_, d)| d.clone()).ok_or_else(|| Error::Format(format!("{name}: no .pcf and .tm2 inside")))?;
        }
        Err(Error::Format(format!("{name}: archives nested too deep")))
    }

    pub fn glyph(&self, c: char) -> Option<Glyph> {
        let i = c as u32;
        if i < 256 { self.glyphs[i as usize] } else { None }
    }

    /// Where each character of `text` goes on one line: (glyph, x), and the line's width. A
    /// character the face lacks takes the width of a space, else of an 'i' (the game's faces have
    /// no space glyph: a third of a line looked like double spaces, live check 22 Oct), else a
    /// quarter of a line, so text never collapses.
    pub fn layout(&self, text: &str) -> (Vec<(Glyph, u32)>, u32) {
        let blank = self.glyph(' ').or_else(|| self.glyph('i')).map_or(self.line_height as u32 / 4, |g| g.w as u32);
        let mut x = 0u32;
        let mut out = Vec::new();
        for c in text.chars() {
            match self.glyph(c) {
                Some(g) => {
                    out.push((g, x));
                    x += g.w as u32 + self.spacing as u32;
                }
                None => x += blank + self.spacing as u32,
            }
        }
        (out, x.saturating_sub(self.spacing as u32))
    }

    /// `text` (lines split at '\n') drawn in `colour` on a transparent background: (RGBA, width,
    /// height), lines `line_height` apart. The sheet's own colour is multiplied by `colour`, so
    /// the game's white glyphs come out in it.
    pub fn render(&self, text: &str, colour: [u8; 3]) -> (Vec<u8>, u32, u32) {
        let lines: Vec<_> = text.split('\n').map(|l| self.layout(l)).collect();
        let w = lines.iter().map(|(_, w)| *w).max().unwrap_or(0).max(1);
        let lh = (self.line_height as u32).max(1);
        let h = lines.len() as u32 * lh;
        let mut out = vec![0u8; (w * h * 4) as usize];
        for (row, (glyphs, _)) in lines.iter().enumerate() {
            for &(g, x) in glyphs {
                for dy in 0..(g.h as u32).min(lh) {
                    let (sy, ty) = (g.y as u32 + dy, row as u32 * lh + dy);
                    for dx in 0..g.w as u32 {
                        let (sx, tx) = (g.x as u32 + dx, x + dx);
                        let s = ((sy * self.width + sx) * 4) as usize;
                        let t = ((ty * w + tx) * 4) as usize;
                        let p = &self.rgba[s..s + 4];
                        if p[3] > out[t + 3] {
                            out[t..t + 4].copy_from_slice(&[(p[0] as u32 * colour[0] as u32 / 255) as u8, (p[1] as u32 * colour[1] as u32 / 255) as u8, (p[2] as u32 * colour[2] as u32 / 255) as u8, p[3]]);
                        }
                    }
                }
            }
        }
        (out, w, h)
    }
}

/// A TIM2 picture with a 4- or 8-bit palette, to RGBA: (pixels, width, height).
pub fn decode_tm2(d: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    if d.get(..4) != Some(b"TIM2") || d.len() < 0x10 {
        return Err(Error::Format("not a TIM2 picture".into()));
    }
    // file header byte 5: 0 = pictures follow at 0x10, 1 = aligned to 128 bytes (the game's fonts)
    let pic = if d[5] == 1 { 0x80 } else { 0x10 };
    if d.len() < pic + 0x18 {
        return Err(Error::Format("TIM2 picture header missing".into()));
    }
    let u16_at = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let (clut_size, image_size, header_size) = (u32_at(pic + 4) as usize, u32_at(pic + 8) as usize, u16_at(pic + 12) as usize);
    let colours = u16_at(pic + 14) as usize;
    let (clut_type, image_type) = (d[pic + 18], d[pic + 19]);
    let (w, h) = (u16_at(pic + 20) as u32, u16_at(pic + 22) as u32);
    // image type 4: 4-bit indices (low nibble first), 5: 8-bit; palette type 3: 32-bit RGBA
    let bits = match (image_type, colours) {
        (4, 16) => 4,
        (5, 256) => 8,
        _ => 0,
    };
    if bits == 0 || clut_type & 0x3F != 3 {
        return Err(Error::Format(format!("TIM2 kind not handled: image type {image_type}, palette type {clut_type:#x}, {colours} colours (4-bit/16 or 8-bit/256 with a 32-bit palette expected)")));
    }
    let pixels = pic + header_size;
    let clut = pixels + image_size;
    let need = (w * h) as usize * bits / 8;
    if image_size < need || clut_size < 4 * colours || d.len() < clut + 4 * colours {
        return Err(Error::Format("TIM2 picture shorter than its header says".into()));
    }
    // PS2 CSM1 (256 colours): within each 32 the stored order is 0-7, 16-23, 8-15, 24-31
    let csm1 = colours == 256 && clut_type & 0x80 == 0;
    let palette: Vec<[u8; 4]> = (0..colours)
        .map(|i| {
            let at = if csm1 { i / 32 * 32 + [0, 2, 1, 3][i % 32 / 8] * 8 + i % 8 } else { i };
            let e = &d[clut + 4 * at..clut + 4 * at + 4];
            [e[0], e[1], e[2], (e[3] as u32 * 255 / 128).min(255) as u8]
        })
        .collect();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for i in 0..(w * h) as usize {
        let p = if bits == 8 { d[pixels + i] } else { (d[pixels + i / 2] >> (4 * (i % 2))) & 0xF };
        rgba.extend_from_slice(&palette[p as usize]);
    }
    Ok((rgba, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TIM2 like the game's: `w x h` 8-bit, the palette in CSM1 order, colour i = (i, 255 - i, 7, alpha 128 for i > 0).
    fn tm2(w: u16, h: u16, pixel: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let image = w as usize * h as usize;
        let mut d = b"TIM2".to_vec();
        d.extend_from_slice(&[4, 0, 1, 0]);
        d.resize(0x10, 0);
        d.extend_from_slice(&((0x30 + image + 1024) as u32).to_le_bytes());
        d.extend_from_slice(&1024u32.to_le_bytes());
        d.extend_from_slice(&(image as u32).to_le_bytes());
        d.extend_from_slice(&0x30u16.to_le_bytes());
        d.extend_from_slice(&256u16.to_le_bytes());
        d.extend_from_slice(&[0, 1, 3, 5]);
        d.extend_from_slice(&w.to_le_bytes());
        d.extend_from_slice(&h.to_le_bytes());
        d.resize(0x40, 0);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                d.push(pixel(x, y));
            }
        }
        let mut clut = vec![0u8; 1024];
        for i in 0..256usize {
            let at = i / 32 * 32 + [0, 2, 1, 3][i % 32 / 8] * 8 + i % 8;
            clut[4 * at..4 * at + 4].copy_from_slice(&[i as u8, 255 - i as u8, 7, if i > 0 { 128 } else { 0 }]);
        }
        d.extend(clut);
        d
    }

    /// A .pcf with glyphs for 'A' (at 0,0 4x6) and 'B' (at 4,0 3x6), line height 8, spacing 2.
    fn pcf() -> Vec<u8> {
        let mut d = Vec::new();
        for v in [3u32, 2, 8, 8, 2, 0] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let mut map = [0xFFFFu16; 256];
        map[b'A' as usize] = 0;
        map[b'B' as usize] = 1;
        for m in map {
            d.extend_from_slice(&m.to_le_bytes());
        }
        for (x, y, w, h, c) in [(0u16, 0u16, 4u16, 6u16, b'A' as u16), (4, 0, 3, 6, b'B' as u16)] {
            for v in [x, y, w, h, c, 0xCD00] {
                d.extend_from_slice(&v.to_le_bytes());
            }
            d.extend_from_slice(&[0xF8, 7, 0, 0]);
        }
        d
    }

    #[test]
    fn the_sheet_decodes_through_the_ps2_palette_order() {
        let (rgba, w, h) = decode_tm2(&tm2(8, 2, |x, y| (x + 8 * y) as u8 * 5)).unwrap();
        assert_eq!((w, h, rgba.len()), (8, 2, 64));
        // pixel 3 has index 15 (stored at CLUT slot 23): colour 15, alpha 128 -> 255
        assert_eq!(&rgba[12..16], &[15, 240, 7, 255]);
        // index 0 is transparent
        assert_eq!(rgba[3], 0);
    }

    /// The game's own fonts: 4-bit, 16 colours, the picture header 128-byte aligned (file header
    /// byte 5 = 1), the low nibble first.
    #[test]
    fn a_4bit_aligned_sheet() {
        let mut d = b"TIM2".to_vec();
        d.extend_from_slice(&[4, 1, 1, 0]);
        d.resize(0x80, 0);
        d.extend_from_slice(&((0x80 + 2 + 64) as u32).to_le_bytes());
        d.extend_from_slice(&64u32.to_le_bytes());
        d.extend_from_slice(&2u32.to_le_bytes());
        d.extend_from_slice(&0x80u16.to_le_bytes());
        d.extend_from_slice(&16u16.to_le_bytes());
        d.extend_from_slice(&[0, 1, 3, 4]);
        d.extend_from_slice(&4u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.resize(0x100, 0);
        d.extend_from_slice(&[0x21, 0xF3]); // pixels 1, 2, 3, 15
        for i in 0..16u8 {
            d.extend_from_slice(&[i * 16, 0, 0, if i == 0 { 0 } else { 128 }]);
        }
        let (rgba, w, h) = decode_tm2(&d).unwrap();
        assert_eq!((w, h), (4, 1));
        assert_eq!(rgba.chunks(4).map(|p| p[0]).collect::<Vec<_>>(), vec![16, 32, 48, 240], "no CSM1 reorder for 16 colours");
        assert_eq!(rgba[3], 255);
    }

    #[test]
    fn metrics_and_layout() {
        let f = Font::parse("t", &pcf(), &tm2(16, 8, |_, _| 1)).unwrap();
        assert_eq!((f.line_height, f.spacing), (8, 2));
        assert_eq!(f.glyph('A'), Some(Glyph { x: 0, y: 0, w: 4, h: 6 }));
        assert_eq!(f.glyph('a'), None);
        let (glyphs, width) = f.layout("AB A");
        // A (4 + 2), B (3 + 2), a blank: no space or 'i' in this face, so a quarter of the line (2 + 2), A
        assert_eq!(glyphs.iter().map(|(_, x)| *x).collect::<Vec<_>>(), [0, 6, 15]);
        assert_eq!(width, 19);
    }

    #[test]
    fn lines_render_in_a_colour() {
        // every pixel of the sheet is index 1: colour (1, 254, 7), opaque
        let f = Font::parse("t", &pcf(), &tm2(16, 8, |_, _| 1)).unwrap();
        let (rgba, w, h) = f.render("A\n BA", [255, 128, 0]);
        // " BA": a 2 px blank (+2), B (3 + 2), A (4): 13 wide; two lines of 8
        assert_eq!((w, h, rgba.len()), (13, 16, 13 * 16 * 4));
        let px = |x: u32, y: u32| &rgba[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize];
        assert_eq!(px(0, 0), &[1, 127, 0, 255], "A's top-left, the sheet's colour times orange");
        assert_eq!(px(5, 0), &[0, 0, 0, 0], "right of A on line 1: nothing");
        assert_eq!(px(0, 8), &[0, 0, 0, 0], "the blank at the start of line 2");
        assert_eq!(px(4, 13), &[1, 127, 0, 255], "B's bottom row on line 2");
        assert_eq!(px(4, 14), &[0, 0, 0, 0], "below the 6 px glyphs");
        assert_eq!(f.render("", [255; 3]).1, 1, "empty text: a 1 px wide image, not a zero-sized one");
    }

    #[test]
    fn bad_files_are_errors() {
        assert!(decode_tm2(b"PNG not tim").is_err());
        let mut p = pcf();
        p[0] = 4;
        assert!(Font::parse("t", &p, &tm2(16, 8, |_, _| 0)).unwrap_err().to_string().contains("version 4"));
        // a glyph outside the sheet
        assert!(Font::parse("t", &pcf(), &tm2(4, 4, |_, _| 0)).unwrap_err().to_string().contains("outside"));
    }

    const FACE_FOR_TESTS: &str = "swiss_medium_15";

    /// With `LMA_GAME_DIR` (the user's extracted game, never in the repo): every face in its
    /// Fonts.mng loads, and the launcher's has the characters it writes.
    #[test]
    fn the_game_faces_load() {
        let Some(dir) = std::env::var_os("LMA_GAME_DIR") else { return };
        let d = std::fs::read(std::path::Path::new(&dir).join("Common/GameData/Textures/Fonts.mng")).unwrap();
        for name in ["swiss_black_26", "swiss_heavy_15", "swiss_heavy_23", "swiss_heavy_35", "swiss_medium_13", "swiss_medium_15"] {
            let f = Font::from_fonts_mng(&d, name).unwrap_or_else(|e| panic!("{name}: {e}"));
            // the launcher's face has every character it writes (the big faces are for scores)
            if name == FACE_FOR_TESTS {
                for c in "LMA Manager 2007 Version Start>[x]:/()".chars().filter(|c| *c != ' ') {
                    assert!(f.glyph(c).is_some(), "{name} has no {c:?}");
                }
            }
            let (_, w, h) = f.render("LMA Manager 2007", [255; 3]);
            eprintln!("{name}: sheet {}x{}, line {}, spacing {}, title {w}x{h}", f.width, f.height, f.line_height, f.spacing);
        }
    }

    #[test]
    fn a_face_from_nested_archives() {
        // Fonts.mng -> swiss_test.mng -> swiss_test.mng -> {.pcf, .tm2}, as on the disc
        let leaf = Archive::new(crate::Endian::Big, vec![("swiss_test.pcf".into(), pcf()), ("swiss_test.tm2".into(), tm2(16, 8, |_, _| 2))]).build();
        let mid = Archive::new(crate::Endian::Big, vec![("swiss_test.mng".into(), leaf)]).build();
        let fonts = Archive::new(crate::Endian::Big, vec![("numbers.mng".into(), vec![0; 16]), ("swiss_test.mng".into(), mid)]).build();
        let f = Font::from_fonts_mng(&fonts, "SWISS_TEST").unwrap();
        assert_eq!(f.glyph('B'), Some(Glyph { x: 4, y: 0, w: 3, h: 6 }));
        assert!(Font::from_fonts_mng(&fonts, "swiss_none").unwrap_err().to_string().contains("no font"));
    }
}
