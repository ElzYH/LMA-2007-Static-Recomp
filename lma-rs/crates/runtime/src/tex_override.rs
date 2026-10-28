//! Texture overrides (#694): replacement images for textures the game decodes from guest memory,
//! keyed by the hash of the guest bytes that `LMA_TEX_LOG` prints (16 hex digits). A directory
//! holds `<hash>.png` files (RGB or RGBA, 8 bits, the texture's own size): `LMA_TEX_OVERRIDES=<dir>`
//! installs it at start, `set` swaps it at runtime (the pad glyphs follow the device in use), and
//! the renderer drops its texture cache when `generation` has moved, so the next frame decodes
//! again. A replacement stands in for the decoded texels, i.e. as drawn after the fetch swizzle:
//! what `LMA_TEX_DUMP` writes is what to paint over. With nothing installed every frame is as before.
//! A replacement is level 0 only: the RGBA8 upload builds its mip chain on the GPU from it (the
//! guest's own BC mips are skipped for a replaced texture), which suits the 1:1 glyphs this is for.
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock, RwLock};

/// hash of the guest bytes -> (width, height, RGBA8 texels).
pub type Table = HashMap<u64, (u32, u32, Arc<Vec<u8>>)>;

static TABLE: OnceLock<RwLock<Option<Arc<Table>>>> = OnceLock::new();
/// Moves on every `set`; the renderer compares it with the generation its cache was filled under.
static GENERATION: AtomicU64 = AtomicU64::new(1);
/// The table and its generation are process-wide and cargo runs tests on parallel threads: a test
/// that calls `set` holds this, and so does a test that needs the texture cache to stay put
/// (raster's tex_cache_tests failed on Windows when a `set` here cleared their cache mid-test).
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn table() -> &'static RwLock<Option<Arc<Table>>> {
    TABLE.get_or_init(|| {
        let t = std::env::var_os("LMA_TEX_OVERRIDES").and_then(|d| match load(Path::new(&d)) {
            Ok(t) => Some(Arc::new(t)),
            Err(e) => {
                eprintln!("LMA_TEX_OVERRIDES: {e}");
                None
            }
        });
        RwLock::new(t)
    })
}

/// Every `<hash>.png` in `dir`, decoded to RGBA8. A file that isn't a readable 8-bit PNG, or whose
/// name isn't 16 hex digits, is an error naming it; other files are ignored.
pub fn load(dir: &Path) -> Result<Table, String> {
    let mut out = Table::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.extension().is_none_or(|x| !x.eq_ignore_ascii_case("png")) {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let hash = (stem.len() == 16).then(|| u64::from_str_radix(stem, 16).ok()).flatten().ok_or_else(|| format!("{}: the name must be the texture's 16-digit hash", path.display()))?;
        let (w, h, px) = read_png(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        out.insert(hash, (w, h, Arc::new(px)));
    }
    Ok(out)
}

/// A PNG as (width, height, RGBA8): 8-bit grey, grey+alpha, RGB or RGBA; anything else is an error.
fn read_png(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    let px = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        other => return Err(format!("{other:?} isn't an 8-bit colour type this reads")),
    };
    Ok((info.width, info.height, px))
}

/// Install `dir`'s images (None: no overrides). Returns how many. The renderer notices through
/// `generation` and decodes its textures again.
pub fn set(dir: Option<&Path>) -> Result<usize, String> {
    let t = dir.map(load).transpose()?;
    let n = t.as_ref().map_or(0, |t| t.len());
    *table().write().unwrap() = t.map(Arc::new);
    GENERATION.fetch_add(1, Relaxed);
    Ok(n)
}

/// Moves on every `set`.
pub fn generation() -> u64 {
    GENERATION.load(Relaxed)
}

/// Whether anything is installed (a renderer with an empty table skips the lookups).
pub fn any() -> bool {
    table().read().unwrap().as_ref().is_some_and(|t| !t.is_empty())
}

/// The replacement texels for the texture of `hash`, if one is installed at exactly `w` x `h`.
pub fn lookup(hash: u64, w: u32, h: u32) -> Option<Arc<Vec<u8>>> {
    let t = table().read().unwrap();
    let (tw, th, px) = t.as_ref()?.get(&hash)?;
    (*tw == w && *th == h).then(|| px.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_file(path: &Path, w: u32, h: u32, color: png::ColorType, data: &[u8]) {
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
    }

    /// A directory of `<hash>.png` loads into the table keyed by the hash; RGB expands to RGBA;
    /// a lookup needs the size to agree; a badly named file is an error that names it.
    #[test]
    fn overrides_load_by_hash_and_size() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = std::env::temp_dir().join(format!("lma-tex-overrides-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        png_file(&d.join("340157ca1854882d.png"), 2, 1, png::ColorType::Rgba, &[1, 2, 3, 4, 5, 6, 7, 8]);
        png_file(&d.join("56487c0a5b537226.png"), 1, 1, png::ColorType::Rgb, &[9, 8, 7]);
        std::fs::write(d.join("notes.txt"), "ignored").unwrap();
        let t = load(&d).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[&0x340157ca1854882d].0, 2);
        assert_eq!(*t[&0x56487c0a5b537226].2, vec![9, 8, 7, 255], "RGB gets an opaque alpha");
        let before = generation();
        assert_eq!(set(Some(&d)).unwrap(), 2);
        assert!(generation() > before);
        assert_eq!(lookup(0x340157ca1854882d, 2, 1).as_deref(), Some(&vec![1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(lookup(0x340157ca1854882d, 1, 2), None, "the size must match");
        assert_eq!(lookup(0x1, 2, 1), None);
        assert!(any());
        png_file(&d.join("glyph.png"), 1, 1, png::ColorType::Rgb, &[0, 0, 0]);
        assert!(load(&d).unwrap_err().contains("glyph.png"));
        assert_eq!(set(None).unwrap(), 0);
        assert!(!any());
        let _ = std::fs::remove_dir_all(&d);
    }
}
