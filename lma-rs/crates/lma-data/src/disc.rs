//! The game disc as the user has it (#766 b, the first-run wizard): an Xbox 360 disc image (XISO,
//! the XDVDFS volume at one of the known partition offsets), a Games-on-Demand package made by
//! "Copy DVD" (an SVOD header file beside its `<header>.data/Data0000..` parts), or a folder that
//! already holds the extracted files. `Disc::open` tells them apart, `entries` lists the files and
//! `extract` writes them out. The Rust port of `tools/xiso.py` and `tools/god.py`.
//!
//! Nothing from the disc is bundled: the user picks their own image at first run.

use crate::{Error, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// XDVDFS sector size.
pub const SECTOR: u64 = 2048;
/// The volume descriptor's magic, at sector 32 of the partition (and again at its end).
pub const MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";
/// Where the game partition starts in a full disc image: an XISO (0), XGD2 (0xFD90000), XGD1
/// (0x18300000) and XGD3 (0x2080000).
pub const PARTITION_OFFSETS: [u64; 4] = [0, 0xFD9_0000, 0x208_0000, 0x1830_0000];

/// One file or folder of the disc, `path` with `/` separators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub sector: u32,
    pub size: u32,
    pub dir: bool,
}

/// Where the bytes come from.
enum Source {
    /// A disc image: the partition starts at byte `base`.
    Image { file: File, base: u64 },
    /// A GOD (SVOD) package: the `DataNNNN` parts and the header's start block / enhanced GDF flag.
    God { parts: Vec<File>, start: u64, egdf: bool },
    /// Files already extracted (the folder holding `default.xex`).
    Folder(PathBuf),
}

/// A disc the game can be installed from.
pub struct Disc {
    source: Source,
    root: (u32, u32),
}

impl std::fmt::Debug for Disc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disc").field("kind", &self.kind()).field("root", &self.root).finish()
    }
}

/// What `Disc::open` found at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    God,
    Folder,
}

impl Disc {
    /// Open a disc image file, a GOD header file (with its `.data` folder beside it), or a folder of
    /// extracted files (it must hold `default.xex`).
    pub fn open(path: &Path) -> Result<Self> {
        if path.is_dir() {
            if path.join("default.xex").is_file() {
                return Ok(Disc { source: Source::Folder(path.to_path_buf()), root: (0, 0) });
            }
            // a GOD package picked by its title folder: <TitleID>/00007000/<header>
            if let Some(header) = god_header_in(path) {
                return Self::open(&header);
            }
            return Err(Error::Format(format!("{}: no default.xex and no Games-on-Demand package in this folder", path.display())));
        }
        let data_dir = god_data_dir(path);
        if data_dir.is_dir() {
            return Self::open_god(path, &data_dir);
        }
        let mut file = File::open(path)?;
        for base in PARTITION_OFFSETS {
            if let Some(root) = volume_at(&mut file, base)? {
                return Ok(Disc { source: Source::Image { file, base }, root });
            }
        }
        Err(Error::Format(format!("{}: not an Xbox 360 disc image (no XDVDFS volume)", path.display())))
    }

    fn open_god(header: &Path, data_dir: &Path) -> Result<Self> {
        let mut h = [0u8; 0x400];
        File::open(header)?.read_exact(&mut h)?;
        let (start, egdf) = svod_params(&h);
        let mut names: Vec<PathBuf> = std::fs::read_dir(data_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().starts_with("data")))
            .collect();
        names.sort();
        if names.is_empty() {
            return Err(Error::Format(format!("{}: no Data0000 parts", data_dir.display())));
        }
        let parts = names.iter().map(File::open).collect::<std::io::Result<Vec<_>>>()?;
        let mut disc = Disc { source: Source::God { parts, start, egdf }, root: (0, 0) };
        // the volume descriptor is at disc sector 32, as on a disc
        let mut vd = [0u8; 28];
        disc.read_at(32 * SECTOR, &mut vd)?;
        if &vd[..20] != MAGIC {
            return Err(Error::Format(format!("{}: Games-on-Demand package without an XDVDFS volume", header.display())));
        }
        disc.root = (u32::from_le_bytes(vd[20..24].try_into().unwrap()), u32::from_le_bytes(vd[24..28].try_into().unwrap()));
        Ok(disc)
    }

    pub fn kind(&self) -> Kind {
        match self.source {
            Source::Image { .. } => Kind::Image,
            Source::God { .. } => Kind::God,
            Source::Folder(_) => Kind::Folder,
        }
    }

    /// Read `buf.len()` bytes at partition byte `off`.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        match &mut self.source {
            Source::Image { file, base } => {
                file.seek(SeekFrom::Start(*base + off))?;
                file.read_exact(buf)?;
            }
            Source::God { parts, start, egdf } => {
                let mut done = 0;
                while done < buf.len() {
                    let pos = off + done as u64;
                    let (sector, within) = (pos / SECTOR, pos % SECTOR);
                    let (part, phys) = god_offset(sector, *start, *egdf);
                    let file = parts.get_mut(part).ok_or_else(|| Error::Format(format!("disc sector {sector} is past the package's {} parts", part)))?;
                    let take = (buf.len() - done).min((SECTOR - within) as usize);
                    file.seek(SeekFrom::Start(phys + within))?;
                    file.read_exact(&mut buf[done..done + take])?;
                    done += take;
                }
            }
            Source::Folder(_) => return Err(Error::Value("a folder has no sectors".into())),
        }
        Ok(())
    }

    /// Every file and folder, sorted by path.
    pub fn entries(&mut self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        if let Source::Folder(dir) = &self.source {
            let dir = dir.clone();
            walk_folder(&dir, &dir, &mut out)?;
        } else {
            let (root, size) = self.root;
            self.walk(root, size, "", &mut out, 0)?;
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// A directory table: a binary tree of entries (left, right as 4-byte offsets in the table,
    /// sector, size, attributes, name length, name), padded with 0xFF.
    fn walk(&mut self, sector: u32, size: u32, prefix: &str, out: &mut Vec<Entry>, depth: u32) -> Result<()> {
        if size == 0 {
            return Ok(());
        }
        if depth > 64 {
            return Err(Error::Format("directory tree deeper than 64 levels".into()));
        }
        // the size comes from the disc: real tables are a few KB, so a damaged one can't ask for GBs
        if size > MAX_TABLE {
            return Err(Error::Format(format!("a directory table of {size} bytes (at most {MAX_TABLE})")));
        }
        let mut table = vec![0u8; size as usize];
        self.read_at(sector as u64 * SECTOR, &mut table)?;
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![0usize];
        while let Some(off) = stack.pop() {
            if off + 14 > table.len() || !seen.insert(off) {
                continue;
            }
            let u16_at = |o: usize| u16::from_le_bytes([table[o], table[o + 1]]);
            let u32_at = |o: usize| u32::from_le_bytes(table[o..o + 4].try_into().unwrap());
            let (left, right) = (u16_at(off), u16_at(off + 2));
            if left == 0xFFFF {
                continue; // padding
            }
            let (start, length, attr, nlen) = (u32_at(off + 4), u32_at(off + 8), table[off + 12], table[off + 13] as usize);
            if off + 14 + nlen > table.len() {
                return Err(Error::Format(format!("directory entry at {off} runs past its table")));
            }
            // names are Latin-1 on the disc
            let name: String = table[off + 14..off + 14 + nlen].iter().map(|&b| b as char).collect();
            if !safe_name(&name) {
                return Err(Error::Format(format!("bad name {name:?} in a directory table")));
            }
            let path = format!("{prefix}{name}");
            let dir = attr & 0x10 != 0;
            out.push(Entry { path: path.clone(), sector: start, size: length, dir });
            if dir {
                self.walk(start, length, &format!("{path}/"), out, depth + 1)?;
            }
            if left != 0 {
                stack.push(left as usize * 4);
            }
            if right != 0 {
                stack.push(right as usize * 4);
            }
        }
        Ok(())
    }

    /// Write the files to `out` (folders made as needed). Files already there with the right size
    /// are skipped, so an interrupted install resumes. `want` picks the files (all: `|_| true`);
    /// `progress(done_bytes, total_bytes)` is called after each file. Returns the files written.
    pub fn extract(&mut self, out: &Path, want: impl Fn(&Entry) -> bool, mut progress: impl FnMut(u64, u64)) -> Result<usize> {
        let files: Vec<Entry> = self.entries()?.into_iter().filter(|e| !e.dir && want(e)).collect();
        let total: u64 = files.iter().map(|e| e.size as u64).sum();
        let (mut done, mut written) = (0u64, 0);
        let mut buf = vec![0u8; 1 << 22];
        for e in &files {
            let dst = out.join(e.path.replace('/', std::path::MAIN_SEPARATOR_STR));
            // every part a plain name: no root, drive prefix or `..` on any OS (names are checked
            // when read too, `safe_name`; this holds for a folder's entries as well)
            let rel = Path::new(&e.path);
            if !rel.components().all(|c| matches!(c, std::path::Component::Normal(_))) || !dst.starts_with(out) {
                return Err(Error::Format(format!("{} would be written outside {}", e.path, out.display())));
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::metadata(&dst).is_ok_and(|m| m.len() == e.size as u64) {
                done += e.size as u64;
                progress(done, total);
                continue;
            }
            match &self.source {
                Source::Folder(dir) => {
                    std::fs::copy(dir.join(&e.path), &dst)?;
                }
                _ => {
                    // write to a temporary name, renamed once complete, so a half-written file
                    // never passes the size check above
                    let part = dst.with_extension("part");
                    let mut o = File::create(&part)?;
                    let mut left = e.size as u64;
                    let mut pos = e.sector as u64 * SECTOR;
                    while left > 0 {
                        let take = left.min(buf.len() as u64) as usize;
                        self.read_at(pos, &mut buf[..take])?;
                        o.write_all(&buf[..take])?;
                        pos += take as u64;
                        left -= take as u64;
                    }
                    drop(o);
                    std::fs::rename(&part, &dst)?;
                }
            }
            written += 1;
            done += e.size as u64;
            progress(done, total);
        }
        Ok(written)
    }
}

/// The largest directory table read (real ones are a few KB).
const MAX_TABLE: u32 = 16 << 20;

/// Whether a disc file name can be written as one path component on every OS: not empty, not `.`
/// or `..`, and no separator (`/`, `\`), drive colon or control character. A crafted disc with
/// `..\x` or `C:x` would otherwise write outside the install folder on Windows (#781 review).
fn safe_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.chars().any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
}

/// The XDVDFS root (sector, size) if the volume descriptor is at `base`.
fn volume_at(file: &mut File, base: u64) -> Result<Option<(u32, u32)>> {
    let mut vd = [0u8; 28];
    if file.seek(SeekFrom::Start(base + 32 * SECTOR)).is_err() || file.read_exact(&mut vd).is_err() {
        return Ok(None);
    }
    if &vd[..20] != MAGIC {
        return Ok(None);
    }
    Ok(Some((u32::from_le_bytes(vd[20..24].try_into().unwrap()), u32::from_le_bytes(vd[24..28].try_into().unwrap()))))
}

/// `<header>.data`, the folder of a GOD package's parts.
fn god_data_dir(header: &Path) -> PathBuf {
    let mut s = header.as_os_str().to_owned();
    s.push(".data");
    PathBuf::from(s)
}

/// A GOD header under a title folder (`<TitleID>/00007000/<header>`) or a content-type folder.
fn god_header_in(dir: &Path) -> Option<PathBuf> {
    let content = if dir.join("00007000").is_dir() { dir.join("00007000") } else { dir.to_path_buf() };
    std::fs::read_dir(&content).ok()?.flatten().map(|e| e.path()).find(|p| p.is_file() && god_data_dir(p).is_dir())
}

/// The SVOD volume descriptor in a LIVE/CON header: (start data block, enhanced GDF).
fn svod_params(h: &[u8]) -> (u64, bool) {
    let d = &h[0x379..0x379 + 0x24];
    let start = d[0x1C] as u64 | (d[0x1D] as u64) << 8 | (d[0x1E] as u64) << 16;
    (start, d[0x18] & 0x40 != 0)
}

/// Disc sector -> (part, byte offset in it) in an SVOD package (Xenia's BlockToOffset): 0x800-byte
/// blocks, a 0x1000-byte hash table before every 0x198 blocks and another before every 0xA1C4 of
/// those, 0x14388 blocks a part.
pub fn god_offset(sector: u64, start: u64, egdf: bool) -> (usize, u64) {
    const BLOCK: u64 = 0x800;
    const HASH: u64 = 0x1000;
    const L0: u64 = 0x198;
    const L1: u64 = 0xA1C4;
    const PER_FILE: u64 = 0x14388;
    let tb = (sector + if egdf { 2 } else { 0 }).saturating_sub(start * 2);
    let (fb, fi) = (tb % PER_FILE, tb / PER_FILE);
    let l0 = fb / L0 + 1;
    let off = l0 * HASH + (l0 / L1 + 1) * HASH;
    (fi as usize, fb * BLOCK + off)
}

fn walk_folder(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> Result<()> {
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let md = e.metadata()?;
        out.push(Entry { path: rel, sector: 0, size: md.len().min(u32::MAX as u64) as u32, dir: md.is_dir() });
        if md.is_dir() {
            walk_folder(root, &p, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names that would leave the install folder on some OS are refused (#781 review).
    #[test]
    fn unsafe_names_are_refused() {
        for bad in ["", ".", "..", "a/b", "..\\..\\x", "C:x", "a\u{0}b", "a\nb"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
        for good in ["default.xex", "Data_01.xwb", "MATCH.MNG", "a b", "Ünïcødé"] {
            assert!(safe_name(good), "{good:?}");
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lma-disc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A directory table holding `items` (name, sector, size, dir) as a right-leaning tree.
    fn table(items: &[(&str, u32, u32, bool)]) -> Vec<u8> {
        let mut t = Vec::new();
        for (i, (name, sector, size, dir)) in items.iter().enumerate() {
            let start = t.len();
            let next_len = 14 + name.len();
            let right = if i + 1 < items.len() { ((start + next_len + 3) / 4) as u16 } else { 0 };
            t.extend_from_slice(&0u16.to_le_bytes());
            t.extend_from_slice(&right.to_le_bytes());
            t.extend_from_slice(&sector.to_le_bytes());
            t.extend_from_slice(&size.to_le_bytes());
            t.push(if *dir { 0x10 } else { 0x80 });
            t.push(name.len() as u8);
            t.extend_from_slice(name.as_bytes());
            while t.len() % 4 != 0 {
                t.push(0xFF);
            }
        }
        t
    }

    /// A small XDVDFS partition: default.xex and Common/GameData/a.bin, with sector numbers.
    fn partition() -> Vec<u8> {
        let xex = b"XEX2 not really".to_vec();
        let a: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
        // sectors: 32 volume descriptor, 33 root, 34 Common, 35 GameData, 36 xex, 37.. a.bin
        let gamedata = table(&[("a.bin", 37, a.len() as u32, false)]);
        let common = table(&[("GameData", 35, gamedata.len() as u32, true)]);
        let root = table(&[("Common", 34, common.len() as u32, true), ("default.xex", 36, xex.len() as u32, false)]);
        let mut p = vec![0u8; 40 * SECTOR as usize];
        let put = |p: &mut Vec<u8>, sector: u64, bytes: &[u8]| p[(sector * SECTOR) as usize..(sector * SECTOR) as usize + bytes.len()].copy_from_slice(bytes);
        let mut vd = MAGIC.to_vec();
        vd.extend_from_slice(&33u32.to_le_bytes());
        vd.extend_from_slice(&(root.len() as u32).to_le_bytes());
        put(&mut p, 32, &vd);
        put(&mut p, 33, &root);
        put(&mut p, 34, &common);
        put(&mut p, 35, &gamedata);
        put(&mut p, 36, &xex);
        put(&mut p, 37, &a);
        p
    }

    #[test]
    fn an_image_at_each_partition_offset_lists_and_extracts() {
        let dir = temp("image");
        for base in [0u64, 0x208_0000] {
            let iso = dir.join(format!("game-{base:x}.iso"));
            let mut bytes = vec![0u8; base as usize];
            bytes.extend(partition());
            std::fs::write(&iso, &bytes).unwrap();
            let mut disc = Disc::open(&iso).unwrap();
            assert_eq!(disc.kind(), Kind::Image);
            let paths: Vec<(String, bool)> = disc.entries().unwrap().into_iter().map(|e| (e.path, e.dir)).collect();
            assert_eq!(paths, [("Common".into(), true), ("Common/GameData".into(), true), ("Common/GameData/a.bin".into(), false), ("default.xex".into(), false)]);
            let out = dir.join(format!("out-{base:x}"));
            let mut calls = Vec::new();
            assert_eq!(disc.extract(&out, |_| true, |d, t| calls.push((d, t))).unwrap(), 2);
            assert_eq!(std::fs::read(out.join("default.xex")).unwrap(), b"XEX2 not really");
            let a: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
            assert_eq!(std::fs::read(out.join("Common").join("GameData").join("a.bin")).unwrap(), a);
            assert_eq!(calls.last(), Some(&(5015, 5015)));
            // a second run finds both files in place and writes nothing
            assert_eq!(disc.extract(&out, |_| true, |_, _| {}).unwrap(), 0);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn not_a_disc_and_an_empty_folder_are_refused() {
        let dir = temp("refuse");
        std::fs::write(dir.join("notes.txt"), vec![0u8; 100 * 1024]).unwrap();
        assert!(Disc::open(&dir.join("notes.txt")).unwrap_err().to_string().contains("not an Xbox 360 disc image"));
        assert!(Disc::open(&dir).unwrap_err().to_string().contains("no default.xex"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_extracted_folder_is_a_disc_too() {
        let dir = temp("folder");
        std::fs::create_dir_all(dir.join("Common")).unwrap();
        std::fs::write(dir.join("default.xex"), b"x").unwrap();
        std::fs::write(dir.join("Common").join("b.bin"), b"bb").unwrap();
        let mut disc = Disc::open(&dir).unwrap();
        assert_eq!(disc.kind(), Kind::Folder);
        let files: Vec<String> = disc.entries().unwrap().into_iter().filter(|e| !e.dir).map(|e| e.path).collect();
        assert_eq!(files, ["Common/b.bin", "default.xex"]);
        let out = temp("folder-out");
        assert_eq!(disc.extract(&out, |e| e.path.ends_with(".bin"), |_, _| {}).unwrap(), 1);
        assert_eq!(std::fs::read(out.join("Common").join("b.bin")).unwrap(), b"bb");
        assert!(!out.join("default.xex").exists(), "the filter picks the files");
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(out).ok();
    }

    #[test]
    fn a_cyclic_tree_ends() {
        // A -> right B -> right C -> left B: a cycle the visited set must cut
        let mut t = table(&[("A", 36, 1, false), ("B", 36, 1, false), ("C", 36, 1, false)]);
        let c = t.len() - 16; // C's entry: 14 bytes + "C" padded to 16
        let b_index = u16::from_le_bytes([t[2], t[3]]);
        t[c..c + 2].copy_from_slice(&b_index.to_le_bytes());
        let dir = temp("cycle");
        let mut p = partition();
        let mut vd = MAGIC.to_vec();
        vd.extend_from_slice(&38u32.to_le_bytes());
        vd.extend_from_slice(&(t.len() as u32).to_le_bytes());
        p[(32 * SECTOR) as usize..(32 * SECTOR) as usize + 28].copy_from_slice(&vd);
        p[(38 * SECTOR) as usize..(38 * SECTOR) as usize + t.len()].copy_from_slice(&t);
        let iso = dir.join("cycle.iso");
        std::fs::write(&iso, &p).unwrap();
        let names: Vec<String> = Disc::open(&iso).unwrap().entries().unwrap().into_iter().map(|e| e.path).collect();
        assert_eq!(names, ["A", "B", "C"], "each table offset is visited once");
        std::fs::remove_dir_all(dir).ok();
    }

    /// The SVOD mapping, checked against values worked out from tools/god.py's formula.
    #[test]
    fn god_sector_mapping_matches_god_py() {
        // start block 0, no EGDF: sector 0 is block 0 of part 0, after the level-0 and level-1 hash tables
        assert_eq!(god_offset(0, 0, false), (0, 0x2000));
        assert_eq!(god_offset(1, 0, false), (0, 0x2800));
        // the next level-0 hash table comes after 0x198 blocks
        assert_eq!(god_offset(0x198, 0, false), (0, 0x198 * 0x800 + 0x3000));
        // a new part after 0x14388 blocks
        assert_eq!(god_offset(0x14388, 0, false), (1, 0x2000));
        // EGDF shifts by two blocks; the start block by two sectors a block
        assert_eq!(god_offset(0, 0, true), god_offset(2, 0, false));
        assert_eq!(god_offset(10, 3, false), god_offset(4, 0, false));
    }

    /// A GOD package built from the test partition with the inverse mapping lists like the image.
    #[test]
    fn a_god_package_lists_and_extracts() {
        let dir = temp("god");
        let header = dir.join("4D5307D5").join("00007000").join("ABCDEF0123456789");
        let data = god_data_dir(&header);
        std::fs::create_dir_all(&data).unwrap();
        let mut h = vec![0u8; 0x400];
        h[0x379 + 0x18] = 0; // no EGDF, start block 0
        std::fs::write(&header, &h).unwrap();
        let p = partition();
        let sectors = p.len() as u64 / SECTOR;
        let (_, last) = god_offset(sectors - 1, 0, false);
        let mut part0 = vec![0u8; (last + SECTOR) as usize];
        for s in 0..sectors {
            let (fi, off) = god_offset(s, 0, false);
            assert_eq!(fi, 0);
            part0[off as usize..(off + SECTOR) as usize].copy_from_slice(&p[(s * SECTOR) as usize..((s + 1) * SECTOR) as usize]);
        }
        std::fs::write(data.join("Data0000"), &part0).unwrap();
        // picked by the header file, or by the title folder
        for pick in [header.clone(), dir.join("4D5307D5")] {
            let mut disc = Disc::open(&pick).unwrap();
            assert_eq!(disc.kind(), Kind::God);
            assert_eq!(disc.entries().unwrap().len(), 4);
        }
        let mut disc = Disc::open(&header).unwrap();
        let out = dir.join("out");
        assert_eq!(disc.extract(&out, |_| true, |_, _| {}).unwrap(), 2);
        assert_eq!(std::fs::read(out.join("default.xex")).unwrap(), b"XEX2 not really");
        std::fs::remove_dir_all(dir).ok();
    }
}
