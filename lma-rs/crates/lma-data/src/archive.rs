//! "BG2" archive (`*.mng`): the container for the database (`Export.mng`) and sound banks.
//!
//! Layout (big-endian): `"BG2\0"`, u32 0x14, u32 table_bytes, u32 data_start, u32 data_len,
//! then entries of `{ name: [u8; 0x100], offset: u32, size: u32 }` with offsets relative to
//! `data_start`. Files are aligned to 0x800; the padding after the last file is omitted.
//! The PS2's archives are the same with the magic `"MG2\0"` and every number little-endian (#3).

use crate::{Endian, Error, Result};

const ENTRY: usize = 0x108;
const ALIGN: usize = 0x800;

#[derive(Clone, Debug)]
pub struct Archive {
    data_start: usize,
    /// Big for the Xbox 360's `BG2`, little for the PS2's `MG2`; the files inside follow it.
    pub endian: Endian,
    pub entries: Vec<(String, Vec<u8>)>,
}

impl Archive {
    pub fn parse(d: &[u8]) -> Result<Self> {
        let e = match d.get(0..4) {
            Some(b"BG2\0") => Endian::Big,
            Some(b"MG2\0") => Endian::Little,
            _ => return Err(Error::Format("not a BG2/MG2 archive".into())),
        };
        if d.len() < 0x14 || e.u32(d, 4) != 0x14 {
            return Err(Error::Format("not a BG2/MG2 archive".into()));
        }
        let table = e.u32(d, 8) as usize;
        let data_start = e.u32(d, 12) as usize;
        let mut entries = Vec::new();
        for i in 0..table / ENTRY {
            let at = 0x14 + i * ENTRY;
            let raw = &d[at..at + 0x100];
            let name_len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let name = String::from_utf8_lossy(&raw[..name_len]).into_owned();
            let off = data_start + e.u32(d, at + 0x100) as usize;
            let size = e.u32(d, at + 0x104) as usize;
            let blob = d
                .get(off..off + size)
                .ok_or_else(|| Error::Format(format!("{name}: entry out of range")))?;
            entries.push((name, blob.to_vec()));
        }
        Ok(Self { data_start, endian: e, entries })
    }

    /// A new archive of `entries` (built with the smallest data start).
    pub fn new(endian: Endian, entries: Vec<(String, Vec<u8>)>) -> Self {
        Self { data_start: 0, endian, entries }
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, d)| d.as_slice())
    }

    pub fn set(&mut self, name: &str, data: Vec<u8>) {
        match self.entries.iter_mut().find(|(n, _)| n == name) {
            Some(e) => e.1 = data,
            None => self.entries.push((name.to_string(), data)),
        }
    }

    /// Whether `d` starts like a BG2/MG2 archive (the disc nests them up to three deep, #873).
    pub fn is_archive(d: &[u8]) -> bool {
        matches!(d.get(0..4), Some(b"BG2\0") | Some(b"MG2\0")) && d.len() >= 0x14
    }

    /// Every leaf file of this archive and the archives nested in it, in table order, as
    /// `f(path, bytes)` with the path `outer.mng/inner.mng/file.dds`: an entry that parses as an
    /// archive is descended into, anything else (a bad nested table included) is a leaf.
    pub fn walk(&self, f: &mut dyn FnMut(&str, &[u8])) {
        self.walk_into("", f);
    }

    fn walk_into(&self, prefix: &str, f: &mut dyn FnMut(&str, &[u8])) {
        for (name, data) in &self.entries {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            match Self::is_archive(data).then(|| Self::parse(data)) {
                Some(Ok(inner)) => inner.walk_into(&path, f),
                _ => f(&path, data),
            }
        }
    }

    pub fn build(&self) -> Vec<u8> {
        let mut body = Vec::new();
        let mut table = Vec::new();
        let mut last_end = 0;
        for (name, data) in &self.entries {
            let off = body.len();
            body.extend_from_slice(data);
            last_end = body.len();
            body.resize(body.len().next_multiple_of(ALIGN), 0);
            let mut n = name.as_bytes().to_vec();
            n.resize(0x100, 0);
            table.extend_from_slice(&n);
            table.extend_from_slice(&self.endian.u32_bytes(off as u32));
            table.extend_from_slice(&self.endian.u32_bytes(data.len() as u32));
        }
        body.truncate(last_end);
        // the original data start, unless entries were added and the table no longer fits before it
        let data_start = self.data_start.max((0x14 + table.len()).next_multiple_of(ALIGN));
        let mut out = Vec::with_capacity(data_start + body.len());
        out.extend_from_slice(if self.endian == Endian::Big { b"BG2\0" } else { b"MG2\0" });
        for v in [0x14, table.len(), data_start, body.len()] {
            out.extend_from_slice(&self.endian.u32_bytes(v as u32));
        }
        out.extend_from_slice(&table);
        out.resize(data_start, 0);
        out.extend_from_slice(&body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BG2 archive written by hand: entries at 0x800-aligned offsets, the last one unpadded.
    fn sample(entries: &[(&str, &[u8])], data_start: usize) -> Vec<u8> {
        sample_in(Endian::Big, entries, data_start)
    }

    /// The same as a BG2 (big-endian) or MG2 (little-endian, the PS2's) archive.
    fn sample_in(e: Endian, entries: &[(&str, &[u8])], data_start: usize) -> Vec<u8> {
        let mut table = Vec::new();
        let mut body = Vec::new();
        for (name, data) in entries {
            let mut n = name.as_bytes().to_vec();
            n.resize(0x100, 0);
            table.extend_from_slice(&n);
            table.extend_from_slice(&e.u32_bytes(body.len() as u32));
            table.extend_from_slice(&e.u32_bytes(data.len() as u32));
            body.extend_from_slice(data);
            body.resize(body.len().next_multiple_of(ALIGN), 0);
        }
        let last = entries.last().map_or(0, |e| e.1.len());
        body.truncate(body.len() - (last.next_multiple_of(ALIGN) - last));
        let mut out = if e == Endian::Big { b"BG2\0".to_vec() } else { b"MG2\0".to_vec() };
        for v in [0x14, table.len(), data_start, body.len()] {
            out.extend_from_slice(&e.u32_bytes(v as u32));
        }
        out.extend_from_slice(&table);
        out.resize(data_start, 0);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn round_trip_is_byte_identical() {
        let bytes = sample(&[("db.bin", &[1, 2, 3, 4, 5]), ("names.txt", b"Henry\0Pires\0")], 0x800);
        let ar = Archive::parse(&bytes).unwrap();
        assert_eq!(ar.get("names.txt"), Some(&b"Henry\0Pires\0"[..]));
        assert_eq!(ar.get("missing"), None);
        assert_eq!(ar.build(), bytes);
    }

    #[test]
    fn replaced_entry_moves_the_next_one() {
        let bytes = sample(&[("a", &[7; 10]), ("b", &[9; 3])], 0x800);
        let mut ar = Archive::parse(&bytes).unwrap();
        ar.set("a", vec![8; 0x900]); // now longer than one 0x800 block
        let back = Archive::parse(&ar.build()).unwrap();
        assert_eq!(back.get("a"), Some(&[8u8; 0x900][..]));
        assert_eq!(back.get("b"), Some(&[9u8; 3][..]));
        assert_eq!(back.build(), ar.build());
    }

    /// Adding entries until the table no longer fits before the data must not truncate it.
    #[test]
    fn added_entries_grow_the_table() {
        let bytes = sample(&[("a", &[1; 4])], 0x800);
        let mut ar = Archive::parse(&bytes).unwrap();
        for i in 0..10 {
            ar.set(&format!("extra{i}"), vec![i as u8; 5]);
        }
        let back = Archive::parse(&ar.build()).unwrap();
        assert_eq!(back.entries.len(), 11);
        assert_eq!(back.get("extra9"), Some(&[9u8; 5][..]));
        assert_eq!(back.get("a"), Some(&[1u8; 4][..]));
    }

    #[test]
    fn little_endian_mg2_round_trips() {
        let bytes = sample_in(Endian::Little, &[("Players.bin", &[1, 2, 3]), ("Clubs.bin", &[4; 0x801])], 0x800);
        let ar = Archive::parse(&bytes).unwrap();
        assert_eq!(ar.endian, Endian::Little);
        assert_eq!(ar.get("Clubs.bin"), Some(&[4u8; 0x801][..]));
        assert_eq!(ar.build(), bytes);
    }

    #[test]
    fn walk_descends_into_nested_archives_and_keeps_leaves() {
        let inner = sample(&[("kit.dds", &[1, 2, 3]), ("junk.bin", b"BG2\0 not a table")], 0x800);
        let outer = sample(&[("club.mng", &inner), ("readme.txt", b"hi")], 0x800);
        let ar = Archive::parse(&outer).unwrap();
        let mut leaves: Vec<(String, usize)> = Vec::new();
        ar.walk(&mut |p, d| leaves.push((p.to_string(), d.len())));
        assert_eq!(leaves, vec![("club.mng/kit.dds".to_string(), 3), ("club.mng/junk.bin".to_string(), 16), ("readme.txt".to_string(), 2)]);
    }

    #[test]
    fn rejects_other_files() {
        assert!(Archive::parse(b"XG2\0\x14\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0").is_err());
        assert!(Archive::parse(b"BG2").is_err());
        assert!(Archive::parse(b"MG2\0\0\0\0\x14\0\0\0\0\0\0\0\0\0\0\0\0").is_err(), "a big-endian header in an MG2");
    }
}
