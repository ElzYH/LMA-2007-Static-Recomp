//! Windows-1252, the encoding used by every string in the database.

const HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

pub fn decode(b: &[u8]) -> String {
    b.iter()
        .map(|&c| match c {
            0x80..=0x9F => HIGH[(c - 0x80) as usize],
            _ => c as char,
        })
        .collect()
}

/// Returns None if a character has no cp1252 code (the game font can't draw it).
pub fn encode(s: &str) -> Option<Vec<u8>> {
    s.chars()
        .map(|ch| {
            let c = ch as u32;
            if c < 0x80 || (0xA0..=0xFF).contains(&c) {
                Some(c as u8)
            } else {
                HIGH.iter().position(|&h| h == ch).map(|i| 0x80 + i as u8)
            }
        })
        .collect()
}

/// NUL-terminated string at `off`.
pub fn cstr(d: &[u8], off: usize) -> String {
    let end = d[off..].iter().position(|&b| b == 0).map_or(d.len(), |p| off + p);
    decode(&d[off..end])
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        for s in ["Tomáš Rosický", "Eboué", "Œ€"] {
            assert_eq!(super::decode(&super::encode(s).unwrap()), s);
        }
        assert!(super::encode("日本").is_none());
    }
}
