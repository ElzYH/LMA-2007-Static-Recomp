//! The game's string tables, `Common/GameData/Misc/Lang/LMA_<E|F|G|I|S>.lng`: "LANG", a version
//! (1), the string count, then a little-endian u32 offset per string into the string data that
//! follows the table; each string is cp1252, NUL-terminated and zero-padded to 4 bytes. UI text,
//! messages and names (the 59 sponsors of `Sponsors.bin` among them) are looked up by index.

use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"LANG";

/// A string table: the version word and the strings (raw cp1252, without the NUL).
#[derive(Clone, Debug, PartialEq)]
pub struct Lang {
    pub version: u32,
    pub strings: Vec<Vec<u8>>,
}

impl Lang {
    pub fn parse(d: &[u8]) -> Result<Self> {
        let word = |o: usize| -> Result<u32> { d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| Error::Format(format!("lng: ends at {o}"))) };
        if d.get(..4) != Some(MAGIC) {
            return Err(Error::Format("lng: no LANG header".into()));
        }
        let (version, n) = (word(4)?, word(8)? as usize);
        let base = 12 + 4 * n;
        let mut strings = Vec::with_capacity(n);
        for i in 0..n {
            let o = base + word(12 + 4 * i)? as usize;
            let s = d.get(o..).ok_or_else(|| Error::Format(format!("lng: string {i} at {o} past the end")))?;
            let end = s.iter().position(|&b| b == 0).ok_or_else(|| Error::Format(format!("lng: string {i} has no NUL")))?;
            strings.push(s[..end].to_vec());
        }
        Ok(Self { version, strings })
    }

    pub fn build(&self) -> Vec<u8> {
        let mut data = Vec::new();
        let mut table = Vec::with_capacity(self.strings.len() * 4);
        for s in &self.strings {
            table.extend_from_slice(&(data.len() as u32).to_le_bytes());
            data.extend_from_slice(s);
            data.push(0);
            data.resize(data.len().next_multiple_of(4), 0);
        }
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&(self.strings.len() as u32).to_le_bytes());
        out.extend_from_slice(&table);
        out.extend_from_slice(&data);
        out
    }

    /// String `i` decoded, or None past the end.
    pub fn get(&self, i: usize) -> Option<String> {
        self.strings.get(i).map(|s| crate::text::decode(s))
    }

    /// Replace string `i` with `text` (cp1252); an error past the end or for a character the
    /// game's font can't draw.
    pub fn set(&mut self, i: usize, text: &str) -> Result<()> {
        let n = self.strings.len();
        let enc = crate::text::encode(text).ok_or_else(|| Error::Value(format!("lng string {i}: {text:?} has a character outside cp1252")))?;
        *self.strings.get_mut(i).ok_or_else(|| Error::Value(format!("lng string {i}: the table has {n}")))? = enc;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table built from strings parses back to them, laid out as the game's: offsets from the
    /// end of the table, each string NUL-terminated and padded to 4 bytes.
    #[test]
    fn build_parse_roundtrip() {
        let l = Lang { version: 1, strings: vec![b"Main Menu".to_vec(), b"".to_vec(), b"Adidas".to_vec(), b"Sports Goods".to_vec()] };
        let b = l.build();
        assert_eq!(&b[..12], b"LANG\x01\0\0\0\x04\0\0\0");
        let offs: Vec<u32> = (0..4).map(|i| u32::from_le_bytes(b[12 + 4 * i..16 + 4 * i].try_into().unwrap())).collect();
        assert_eq!(offs, [0, 12, 16, 24], "Main Menu + NUL padded to 12, the empty string to 4");
        assert_eq!(b.len() % 4, 0);
        assert_eq!(Lang::parse(&b).unwrap(), l);
    }

    #[test]
    fn set_replaces_and_checks() {
        let mut l = Lang { version: 1, strings: vec![b"Adidas".to_vec(), b"Nescaf\xe9".to_vec()] };
        assert_eq!(l.get(1).as_deref(), Some("Nescafé"));
        l.set(0, "Emirates").unwrap();
        assert_eq!(l.get(0).as_deref(), Some("Emirates"));
        assert!(l.set(2, "x").is_err(), "past the end");
        assert!(l.set(0, "Œ€").is_ok(), "cp1252 has both");
        assert!(l.set(0, "日本").is_err(), "outside cp1252");
        assert!(Lang::parse(b"LANX").is_err());
    }

    /// With the user's own files (LMA_GAME_DIR): every language parses and rebuilds byte for byte.
    #[test]
    fn game_tables_rebuild_exactly() {
        let Ok(dir) = std::env::var("LMA_GAME_DIR") else { return };
        for l in ["E", "F", "G", "I", "S"] {
            let p = std::path::Path::new(&dir).join(format!("Common/GameData/Misc/Lang/LMA_{l}.lng"));
            let b = std::fs::read(&p).unwrap();
            assert_eq!(Lang::parse(&b).unwrap().build(), b, "{}", p.display());
        }
    }
}
