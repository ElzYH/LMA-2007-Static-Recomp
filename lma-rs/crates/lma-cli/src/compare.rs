//! `lma-cli compare`: frame-by-frame comparison of rendering backends, the correctness gate for
//! the GPU backend (#6).
//!
//!   lma-cli compare <default.xex> <state> --frames N --write-ref <dir> [--backend software]
//!   lma-cli compare <default.xex> <state> --ref <dir> [--backend B] [--out <dir>]
//!                   [--tolerance T] [--max-changed P]
//!
//! Loads the save state, renders the next N frames the game presents with backend B and either
//! stores them as a reference set (`frame_NNNNN.png` + `manifest.txt`) or compares them with one.
//! For each frame it reports the changed pixels (any of R, G, B differing by more than T), the
//! largest difference and the PSNR, and writes a diff PNG for frames that differ (`--out`,
//! default `frames/compare`). It fails when a frame has more than P changed pixels (default 0).

use runtime::backend::RenderBackend;
use std::path::{Path, PathBuf};

type Res<T> = Result<T, Box<dyn std::error::Error>>;
type Frame = (u64, u32, u32, Vec<u8>);

/// Backends by name.
pub fn backend(name: &str) -> Result<Box<dyn RenderBackend>, String> {
    match name {
        "software" => Ok(Box::new(runtime::raster::Renderer::default())),
        "wgpu" => Ok(Box::new(runtime::gpu_wgpu::WgpuBackend::new()?)),
        _ => Err(format!("unknown backend {name:?} (available: software, wgpu)")),
    }
}

struct Opts {
    frames: usize,
    write_ref: Option<PathBuf>,
    reference: Option<PathBuf>,
    backend: String,
    out: PathBuf,
    tolerance: u8,
    max_changed: u64,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { frames: 10, write_ref: None, reference: None, backend: "software".into(), out: "frames/compare".into(), tolerance: 0, max_changed: 0 };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
        let num = |v: String| v.parse::<u64>().map_err(|_| format!("{flag}: not a number: {v}"));
        match flag.as_str() {
            "--frames" => o.frames = num(val()?)?.max(1) as usize,
            "--write-ref" => o.write_ref = Some(val()?.into()),
            "--ref" => o.reference = Some(val()?.into()),
            "--backend" => o.backend = val()?,
            "--out" => o.out = val()?.into(),
            "--tolerance" => o.tolerance = num(val()?)?.min(255) as u8,
            "--max-changed" => o.max_changed = num(val()?)?,
            f => return Err(format!("unknown option {f}")),
        }
    }
    if o.write_ref.is_some() == o.reference.is_some() {
        return Err("give exactly one of --write-ref <dir> or --ref <dir>".into());
    }
    Ok(o)
}

pub fn main(xex: &str, state: &str, args: &[String]) -> Res<()> {
    let o = parse_opts(args)?;
    let state = Path::new(state);
    let state_name = state.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let state_bytes = std::fs::metadata(state)?.len();
    if let Some(dir) = &o.write_ref {
        let frames = render(xex, state, &o.backend, Wanted::Next(o.frames))?;
        std::fs::create_dir_all(dir)?;
        for (n, w, h, px) in &frames {
            crate::write_png(&dir.join(format!("frame_{n:05}.png")), *w, *h, px)?;
        }
        let list: Vec<String> = frames.iter().map(|f| f.0.to_string()).collect();
        let manifest = Manifest { state: state_name, state_bytes, backend: o.backend.clone(), frames: frames.iter().map(|f| f.0).collect(), commit: git_commit() };
        std::fs::write(dir.join("manifest.txt"), manifest.to_text())?;
        println!("wrote {} reference frames ({}) to {} with backend {}", frames.len(), list.join(","), dir.display(), o.backend);
        return Ok(());
    }
    let dir = o.reference.as_ref().unwrap();
    let manifest = Manifest::parse(&std::fs::read_to_string(dir.join("manifest.txt"))?)?;
    if manifest.state != state_name || manifest.state_bytes != state_bytes {
        return Err(format!("{} was made from {} ({} bytes), not {} ({state_bytes} bytes)", dir.display(), manifest.state, manifest.state_bytes, state_name).into());
    }
    let frames = render(xex, state, &o.backend, Wanted::These(manifest.frames.clone()))?;
    std::fs::create_dir_all(&o.out)?;
    let mut report = format!("{} ({}) vs reference {} ({}, commit {})\nframe    changed      max   psnr\n", o.backend, git_commit(), dir.display(), manifest.backend, manifest.commit);
    let mut failed = 0;
    for &n in &manifest.frames {
        let Some((_, w, h, px)) = frames.iter().find(|f| f.0 == n) else {
            report.push_str(&format!("{n:05}  not rendered\n"));
            failed += 1;
            continue;
        };
        let (rw, rh, reference) = crate::read_png(&dir.join(format!("frame_{n:05}.png")))?;
        if (rw, rh) != (*w, *h) {
            report.push_str(&format!("{n:05}  size {w}x{h}, reference {rw}x{rh}\n"));
            failed += 1;
            continue;
        }
        let d = diff(&reference, px, o.tolerance);
        let over = d.changed > o.max_changed;
        failed += over as usize;
        report.push_str(&format!("{n:05}  {:>9}  {:>7}  {:>5}{}\n", d.changed, d.max, d.psnr_text(), if over { "  FAIL" } else { "" }));
        if d.max > 0 {
            crate::write_png(&o.out.join(format!("diff_{n:05}.png")), *w, *h, &diff_image(&reference, px, o.tolerance))?;
            crate::write_png(&o.out.join(format!("frame_{n:05}.png")), *w, *h, px)?;
        }
    }
    report.push_str(&format!("{} of {} frames over tolerance {} / max changed {}\n", failed, manifest.frames.len(), o.tolerance, o.max_changed));
    print!("{report}");
    std::fs::write(o.out.join("report.txt"), &report)?;
    if failed > 0 {
        return Err(format!("{failed} frame(s) differ from the reference").into());
    }
    Ok(())
}

enum Wanted {
    Next(usize),
    These(Vec<u64>),
}

/// Load `state` with `backend` and render until the wanted frames have been presented.
fn render(xex: &str, state: &Path, backend_name: &str, wanted: Wanted) -> Res<Vec<Frame>> {
    let x = xex::Xex::parse(&std::fs::read(xex)?)?;
    let game_dir = Path::new(xex).parent().unwrap_or(Path::new("."));
    let mut m = runtime::machine::Machine::new(&x, game_dir, &std::env::current_dir()?.join("userdata"));
    if std::env::var("LMA_INTERP").is_err() {
        m.install_blocks(game_blocks::PARTS);
    }
    // reference frames must be reproducible: instruction-count time, never the host clock (#56)
    m.deterministic = true;
    // the backend goes in first so the state's EDRAM is restored into it
    m.gpu.renderer = backend(backend_name)?;
    m.load_state(state)?;
    let start = m.gpu.stats.frames;
    m.gpu.dump_frames = match wanted {
        Wanted::Next(n) => (start..start + n as u64).collect(),
        Wanted::These(v) => v,
    };
    m.gpu.render = true;
    m.gpu.render_every = 1;
    let want = m.gpu.dump_frames.len();
    let last = m.gpu.dump_frames.iter().copied().max().unwrap_or(start);
    if last < start {
        return Err(format!("the state is already at frame {start}, past the frames asked for").into());
    }
    let t = std::time::Instant::now();
    // a presented frame takes ~1-2 M instructions in menus; stop if the game stops presenting
    let deadline = m.clock + 50_000_000 * (last - start + 1);
    while m.gpu.frames_out.len() < want && m.clock < deadline {
        if !matches!(m.run(m.clock + 5_000_000), runtime::machine::Exit::Limit) {
            break;
        }
    }
    let frames = std::mem::take(&mut m.gpu.frames_out);
    eprintln!("{backend_name}: {} frames from frame {start} in {:.2?}", frames.len(), t.elapsed());
    Ok(frames)
}

/// How two RGBA8 images differ (alpha ignored).
#[derive(Debug, PartialEq)]
pub struct Diff {
    /// Pixels where any of R, G, B differs by more than the tolerance.
    pub changed: u64,
    /// Largest channel difference.
    pub max: u8,
    /// Peak signal-to-noise ratio over R, G, B in dB (infinite when identical).
    pub psnr: f64,
}

impl Diff {
    fn psnr_text(&self) -> String {
        if self.psnr.is_finite() { format!("{:.1}", self.psnr) } else { "inf".into() }
    }
}

pub fn diff(reference: &[u8], actual: &[u8], tolerance: u8) -> Diff {
    let (mut changed, mut max, mut sq) = (0u64, 0u8, 0f64);
    for (a, b) in reference.chunks_exact(4).zip(actual.chunks_exact(4)) {
        let d = (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap();
        changed += (d > tolerance) as u64;
        max = max.max(d);
        sq += (0..3).map(|c| (a[c] as f64 - b[c] as f64).powi(2)).sum::<f64>();
    }
    let mse = sq / (reference.len() / 4 * 3).max(1) as f64;
    Diff { changed, max, psnr: if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() } }
}

/// The reference dimmed to grey, with changed pixels in red (brighter = larger difference)
/// and pixels within the tolerance but not identical in yellow.
pub fn diff_image(reference: &[u8], actual: &[u8], tolerance: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(reference.len());
    for (a, b) in reference.chunks_exact(4).zip(actual.chunks_exact(4)) {
        let d = (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap();
        let grey = ((a[0] as u32 + a[1] as u32 + a[2] as u32) / 3 / 3) as u8;
        out.extend(match d {
            0 => [grey, grey, grey, 255],
            d if d <= tolerance => [160, 160, 0, 255],
            d => [128u8.saturating_add(d / 2).max(160), 0, 0, 255],
        });
    }
    out
}

struct Manifest {
    state: String,
    state_bytes: u64,
    backend: String,
    frames: Vec<u64>,
    commit: String,
}

impl Manifest {
    fn to_text(&self) -> String {
        let frames: Vec<String> = self.frames.iter().map(u64::to_string).collect();
        format!("state={}\nstate_bytes={}\nbackend={}\nframes={}\ncommit={}\n", self.state, self.state_bytes, self.backend, frames.join(","), self.commit)
    }

    fn parse(text: &str) -> Res<Self> {
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(str::to_string).ok_or_else(|| format!("manifest.txt: no {k}"));
        Ok(Self {
            state: get("state")?,
            state_bytes: get("state_bytes")?.parse()?,
            backend: get("backend")?,
            frames: get("frames")?.split(',').filter(|s| !s.is_empty()).map(str::parse).collect::<Result<_, _>>()?,
            commit: get("commit").unwrap_or_else(|_| "unknown".into()),
        })
    }
}

/// The commit (and "-dirty") these frames come from, for shared references (CLAUDE.md rule 5).
fn git_commit() -> String {
    let git = |args: &[&str]| std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    match git(&["rev-parse", "--short", "HEAD"]) {
        Some(c) => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            format!("{c}{}", if dirty { "-dirty" } else { "" })
        }
        None => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(px: &[[u8; 3]]) -> Vec<u8> {
        px.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect()
    }

    #[test]
    fn identical_frames() {
        let a = img(&[[1, 2, 3], [200, 100, 0]]);
        assert_eq!(diff(&a, &a, 0), Diff { changed: 0, max: 0, psnr: f64::INFINITY });
    }

    #[test]
    fn tolerance_counts_only_larger_differences() {
        let a = img(&[[10, 10, 10], [10, 10, 10], [0, 0, 0]]);
        let b = img(&[[10, 10, 13], [10, 30, 10], [0, 0, 0]]);
        let strict = diff(&a, &b, 0);
        assert_eq!((strict.changed, strict.max), (2, 20));
        assert_eq!(diff(&a, &b, 3).changed, 1);
        assert_eq!(diff(&a, &b, 20).changed, 0);
        // MSE = (9 + 400) / 9 channels
        assert!((strict.psnr - 10.0 * (65025.0_f64 / (409.0 / 9.0)).log10()).abs() < 1e-9);
        let mut alpha = b.clone();
        alpha[3] = 0;
        assert_eq!(diff(&b, &alpha, 0).changed, 0, "alpha is ignored");
    }

    #[test]
    fn diff_image_marks_changes() {
        let a = img(&[[90, 90, 90], [0, 0, 0], [0, 0, 0]]);
        let b = img(&[[90, 90, 90], [2, 0, 0], [255, 0, 0]]);
        let d = diff_image(&a, &b, 2);
        assert_eq!(&d[0..4], &[30, 30, 30, 255]);
        assert_eq!(&d[4..8], &[160, 160, 0, 255]);
        assert_eq!(&d[8..12], &[255, 0, 0, 255]);
    }

    #[test]
    fn manifest_round_trip_and_options() {
        let m = Manifest { state: "team_select.state".into(), state_bytes: 42, backend: "software".into(), frames: vec![5112, 5113], commit: "abc1234".into() };
        let back = Manifest::parse(&m.to_text()).unwrap();
        assert_eq!((back.state.as_str(), back.state_bytes, back.frames, back.commit.as_str()), ("team_select.state", 42, vec![5112, 5113], "abc1234"));
        let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert!(parse_opts(&args("--frames 3")).is_err(), "needs --ref or --write-ref");
        let o = parse_opts(&args("--ref r --tolerance 2 --max-changed 100 --backend software")).unwrap();
        assert_eq!((o.tolerance, o.max_changed, o.frames), (2, 100, 10));
        assert!(backend("no-such-backend").is_err());
    }
}
