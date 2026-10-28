//! XEX2 (Xbox 360 executable) loader: header parsing, AES decryption, basic decompression
//! and the import table. Produces the in-memory PE image the recompiler works on.

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use aes::Aes128;

const RETAIL_KEY: [u8; 16] = [
    0x20, 0xB1, 0x85, 0xA5, 0x9D, 0x28, 0xFD, 0xC3, 0x40, 0x58, 0x3F, 0xBB, 0x08, 0x96, 0xBF, 0x91,
];
const DEVKIT_KEY: [u8; 16] = [0; 16];

// optional header keys
const FILE_FORMAT_INFO: u32 = 0x0000_03FF;
const ENTRY_POINT: u32 = 0x0001_0100;
const IMAGE_BASE: u32 = 0x0001_0201;
const IMPORT_LIBRARIES: u32 = 0x0001_03FF;

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}

fn err<T>(m: impl Into<String>) -> Result<T, Error> {
    Err(Error(m.into()))
}

fn u32be(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(d[o..o + 4].try_into().unwrap())
}
fn u16be(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes(d[o..o + 2].try_into().unwrap())
}

#[derive(Debug, Clone)]
pub struct ImportLibrary {
    pub name: String,
    pub version: u32,
    /// Guest addresses of the import records (variable slot, and thunk for functions).
    pub records: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Import {
    pub library: String,
    pub ordinal: u32,
    /// Address the game reads/calls through.
    pub address: u32,
    /// 0 = variable, 1 = function thunk.
    pub kind: u8,
}

#[derive(Debug)]
pub struct Xex {
    pub module_flags: u32,
    pub entry_point: u32,
    pub image_base: u32,
    pub image: Vec<u8>,
    pub libraries: Vec<ImportLibrary>,
    pub used_devkit_key: bool,
}

impl Xex {
    pub fn parse(d: &[u8]) -> Result<Self, Error> {
        if d.len() < 0x18 || &d[0..4] != b"XEX2" {
            return err("not an XEX2 file");
        }
        let module_flags = u32be(d, 4);
        let pe_off = u32be(d, 8) as usize;
        let sec_off = u32be(d, 0x10) as usize;
        let nopt = u32be(d, 0x14) as usize;
        let opt = |key: u32| {
            (0..nopt).map(|i| 0x18 + i * 8).find(|&o| u32be(d, o) == key).map(|o| u32be(d, o + 4))
        };

        let entry_point = opt(ENTRY_POINT).ok_or(Error("no entry point".into()))?;
        let image_base = opt(IMAGE_BASE).unwrap_or(u32be(d, sec_off + 0x110));
        let image_size = u32be(d, sec_off + 4) as usize;
        let file_key: [u8; 16] = d[sec_off + 0x150..sec_off + 0x160].try_into().unwrap();

        let ffi = opt(FILE_FORMAT_INFO).ok_or(Error("no file format info".into()))? as usize;
        let ffi_size = u32be(d, ffi) as usize;
        let encryption = u16be(d, ffi + 4);
        let compression = u16be(d, ffi + 6);

        let payload = &d[pe_off..];
        let try_key = |k: &[u8; 16]| -> Result<Vec<u8>, Error> {
            let plain = if encryption == 1 { decrypt(payload, &session_key(k, &file_key)) } else { payload.to_vec() };
            let image = match compression {
                0 => plain,
                1 => basic_decompress(&plain, &d[ffi + 8..ffi + ffi_size], image_size)?,
                2 => return err("LZX-compressed XEX is not supported yet"),
                c => return err(format!("unknown compression type {c}")),
            };
            if image.get(0..2) != Some(b"MZ") {
                return err("decrypted image is not a PE file");
            }
            Ok(image)
        };
        let (image, used_devkit_key) = match try_key(&RETAIL_KEY) {
            Ok(i) => (i, false),
            Err(_) => (try_key(&DEVKIT_KEY)?, true),
        };

        let libraries = match opt(IMPORT_LIBRARIES) {
            Some(o) => parse_imports(d, o as usize)?,
            None => Vec::new(),
        };
        Ok(Self { module_flags, entry_point, image_base, image, libraries, used_devkit_key })
    }

    /// Byte at guest address `addr`.
    pub fn read_u32(&self, addr: u32) -> Option<u32> {
        let o = addr.checked_sub(self.image_base)? as usize;
        self.image.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }

    /// Every import with its ordinal, decoded from the record words in the image.
    pub fn imports(&self) -> Vec<Import> {
        let mut out = Vec::new();
        for lib in &self.libraries {
            for &addr in &lib.records {
                let Some(v) = self.read_u32(addr) else { continue };
                out.push(Import {
                    library: lib.name.clone(),
                    ordinal: v & 0xFFFF,
                    address: addr,
                    kind: (v >> 24) as u8,
                });
            }
        }
        out
    }

    /// PE sections: (name, virtual address, virtual size, characteristics).
    pub fn sections(&self) -> Vec<(String, u32, u32, u32)> {
        let i = &self.image;
        let le32 = |o: usize| u32::from_le_bytes(i[o..o + 4].try_into().unwrap());
        let pe = le32(0x3C) as usize;
        let nsec = u16::from_le_bytes([i[pe + 6], i[pe + 7]]) as usize;
        let opt_size = u16::from_le_bytes([i[pe + 20], i[pe + 21]]) as usize;
        let first = pe + 24 + opt_size;
        (0..nsec)
            .map(|k| {
                let s = first + k * 40;
                let name = String::from_utf8_lossy(&i[s..s + 8]).trim_end_matches('\0').to_string();
                (name, self.image_base + le32(s + 12), le32(s + 8), le32(s + 36))
            })
            .collect()
    }
}

fn session_key(key: &[u8; 16], file_key: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut b = GenericArray::clone_from_slice(file_key);
    cipher.decrypt_block(&mut b);
    b.into()
}

/// AES-128-CBC with a zero IV over whole blocks.
fn decrypt(data: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = Vec::with_capacity(data.len());
    let mut prev = [0u8; 16];
    for chunk in data.chunks_exact(16) {
        let mut b = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut b);
        for k in 0..16 {
            out.push(b[k] ^ prev[k]);
        }
        prev.copy_from_slice(chunk);
    }
    out
}

/// "Basic" compression: (data_size, zero_size) pairs; copy data, then append zeros.
fn basic_decompress(src: &[u8], blocks: &[u8], image_size: usize) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(image_size);
    let mut pos = 0;
    for b in blocks.chunks_exact(8) {
        let data = u32be(b, 0) as usize;
        let zero = u32be(b, 4) as usize;
        let chunk = src.get(pos..pos + data).ok_or(Error("basic block past end of file".into()))?;
        out.extend_from_slice(chunk);
        out.resize(out.len() + zero, 0);
        pos += data;
    }
    out.resize(image_size.max(out.len()), 0);
    Ok(out)
}

fn parse_imports(d: &[u8], o: usize) -> Result<Vec<ImportLibrary>, Error> {
    let total = u32be(d, o) as usize;
    let strtab_size = u32be(d, o + 4) as usize;
    let nstr = u32be(d, o + 8) as usize;
    let mut names = Vec::new();
    let mut p = o + 12;
    let strtab_end = p + strtab_size;
    while names.len() < nstr && p < strtab_end {
        let end = d[p..strtab_end].iter().position(|&b| b == 0).map_or(strtab_end, |e| p + e);
        names.push(String::from_utf8_lossy(&d[p..end]).into_owned());
        p = end + 1;
        while p < strtab_end && d[p] == 0 {
            p += 1; // strings are padded to 4 bytes
        }
    }
    let mut libs = Vec::new();
    let mut p = strtab_end;
    while p < o + total {
        let size = u32be(d, p) as usize;
        if size == 0 {
            break;
        }
        // size, digest[20], id, version, version_min, name_index u16, count u16, records[]
        let version = u32be(d, p + 0x1C);
        let name_index = u16be(d, p + 0x24) as usize;
        let count = u16be(d, p + 0x26) as usize;
        let records = (0..count).map(|k| u32be(d, p + 0x28 + 4 * k)).collect();
        libs.push(ImportLibrary {
            name: names.get(name_index).cloned().unwrap_or_else(|| format!("lib{name_index}")),
            version,
            records,
        });
        p += size;
    }
    Ok(libs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;

    const BASE: u32 = 0x8200_0000;

    /// The PE image the tests expect back: "MZ", then an import record word at offset 0x10
    /// (a function thunk for ordinal 0x123).
    fn image() -> Vec<u8> {
        let mut i = vec![0u8; 0x40];
        i[0..2].copy_from_slice(b"MZ");
        i[2..8].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        i[0x10..0x14].copy_from_slice(&0x0100_0123u32.to_be_bytes());
        i
    }

    /// An XEX2 around `payload`: entry point, image base, file format info (`encryption`,
    /// `compression`, `blocks`), one import library, and a security info with `file_key`.
    fn xex(payload: &[u8], encryption: u16, compression: u16, blocks: &[(u32, u32)], file_key: [u8; 16]) -> Vec<u8> {
        let (ffi, imports, sec, pe) = (0x100usize, 0x200usize, 0x400usize, 0x800usize);
        let mut d = vec![0u8; pe];
        let put = |d: &mut Vec<u8>, o: usize, v: u32| d[o..o + 4].copy_from_slice(&v.to_be_bytes());
        d[0..4].copy_from_slice(b"XEX2");
        put(&mut d, 8, pe as u32);
        put(&mut d, 0x10, sec as u32);
        put(&mut d, 0x14, 4);
        for (k, (key, v)) in [(ENTRY_POINT, BASE + 0x20), (IMAGE_BASE, BASE), (FILE_FORMAT_INFO, ffi as u32), (IMPORT_LIBRARIES, imports as u32)].iter().enumerate() {
            put(&mut d, 0x18 + 8 * k, *key);
            put(&mut d, 0x1C + 8 * k, *v);
        }
        put(&mut d, ffi, 8 + 8 * blocks.len() as u32);
        d[ffi + 4..ffi + 6].copy_from_slice(&encryption.to_be_bytes());
        d[ffi + 6..ffi + 8].copy_from_slice(&compression.to_be_bytes());
        for (k, (data, zero)) in blocks.iter().enumerate() {
            put(&mut d, ffi + 8 + 8 * k, *data);
            put(&mut d, ffi + 12 + 8 * k, *zero);
        }
        // imports: string table "xboxkrnl.exe" (padded to 4), one library with one record
        let names = b"xboxkrnl.exe\0\0\0\0";
        put(&mut d, imports + 4, names.len() as u32);
        put(&mut d, imports + 8, 1);
        d[imports + 12..imports + 12 + names.len()].copy_from_slice(names);
        let lib = imports + 12 + names.len();
        put(&mut d, lib, 0x2C);
        put(&mut d, lib + 0x1C, 0x2000_0000); // version
        d[lib + 0x26..lib + 0x28].copy_from_slice(&1u16.to_be_bytes());
        put(&mut d, lib + 0x28, BASE + 0x10);
        put(&mut d, imports, (lib + 0x2C - imports) as u32);
        put(&mut d, sec + 4, image().len() as u32);
        d[sec + 0x150..sec + 0x160].copy_from_slice(&file_key);
        d.extend_from_slice(payload);
        d
    }

    #[test]
    fn plain_basic_compressed_with_imports() {
        // basic compression: the 8 data bytes, then 0x38 zeros; the record word as its own block
        let img = image();
        let payload = [&img[0..8], &img[0x10..0x14]].concat();
        let x = Xex::parse(&xex(&payload, 0, 1, &[(8, 8), (4, 0x2C)], [0; 16])).unwrap();
        assert_eq!(x.image, img);
        assert_eq!((x.entry_point, x.image_base, x.used_devkit_key), (BASE + 0x20, BASE, false));
        assert_eq!(x.libraries.len(), 1);
        assert_eq!((x.libraries[0].name.as_str(), x.libraries[0].version), ("xboxkrnl.exe", 0x2000_0000));
        let imports = x.imports();
        assert_eq!(imports.len(), 1);
        assert_eq!((imports[0].ordinal, imports[0].kind, imports[0].address), (0x123, 1, BASE + 0x10));
        assert_eq!(x.read_u32(BASE + 0x10), Some(0x0100_0123));
        assert_eq!(x.read_u32(BASE + 0x1000), None);
    }

    /// Encrypted with the devkit (zero) key: the retail key fails the "MZ" check and the loader
    /// falls back to the devkit key.
    #[test]
    fn encrypted_devkit_key() {
        let file_key = [7u8; 16];
        let session = session_key(&DEVKIT_KEY, &file_key);
        let cipher = Aes128::new(GenericArray::from_slice(&session));
        let mut prev = [0u8; 16];
        let mut payload = Vec::new();
        for chunk in image().chunks(16) {
            let mut b = [0u8; 16];
            for k in 0..16 {
                b[k] = chunk[k] ^ prev[k];
            }
            let mut g = GenericArray::clone_from_slice(&b);
            cipher.encrypt_block(&mut g);
            prev.copy_from_slice(&g);
            payload.extend_from_slice(&g);
        }
        let x = Xex::parse(&xex(&payload, 1, 0, &[], file_key)).unwrap();
        assert!(x.used_devkit_key);
        assert_eq!(x.image, image());
    }

    #[test]
    fn rejects() {
        assert!(Xex::parse(b"XEX1............................").is_err());
        assert!(Xex::parse(&xex(&image(), 0, 2, &[], [0; 16])).unwrap_err().0.contains("LZX"));
        let mut not_pe = image();
        not_pe[0] = b'Z';
        assert!(Xex::parse(&xex(&not_pe, 0, 0, &[], [0; 16])).is_err());
    }
}
