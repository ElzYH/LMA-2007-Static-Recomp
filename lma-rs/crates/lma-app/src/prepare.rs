//! First run, second half: the app that's downloaded carries no game code (its game-blocks are the
//! empty stubs), so after the disc is installed (install.rs) it builds the game for this computer:
//! a private Rust toolchain (unless one is installed), the source shipped with the app, the game's
//! code recompiled from the player's own default.xex, and a release build. The launcher shows each
//! stage with a progress bar; once built, this app and every later start hand over to the built
//! game at once (`handover`). Nothing from the game is downloaded or uploaded.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Where the preparation stands; shared with the worker thread.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    /// `stage` of `STAGES`, its progress in percent (None: no measure), and a detail line.
    Working { stage: usize, pct: Option<u32>, detail: String },
    /// The built game.
    Done(PathBuf),
    Failed(String),
}

pub const STAGES: [&str; 4] = ["Setting up the compiler (once)", "Copying the source", "Recompiling the game's code from your disc", "Building the game for this computer"];

#[derive(bevy::prelude::Resource, Clone)]
pub struct Prepare(pub Arc<Mutex<State>>);

impl Default for Prepare {
    fn default() -> Self {
        Prepare(Arc::new(Mutex::new(State::Idle)))
    }
}

/// This app has no recompiled game in it (the downloadable shell).
pub fn is_shell() -> bool {
    game_blocks::PARTS.iter().all(|p| p.is_empty())
}

/// The app's data folder (next to its config file; `LMA_DATA_DIR` moves it, for tests).
fn data_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("LMA_DATA_DIR") {
        return Some(PathBuf::from(d));
    }
    crate::config::path().and_then(|p| p.parent().map(Path::to_path_buf))
}

const EXE: &str = std::env::consts::EXE_SUFFIX;

/// The built game, if it is built and belongs to this app (the stamp: this app's own size and
/// modification time, so an updated app builds again).
pub fn built_game() -> Option<PathBuf> {
    let build = data_dir()?.join("build");
    let exe = build.join("lma-rs").join("target").join("release").join(format!("lma-app{EXE}"));
    (exe.is_file() && std::fs::read_to_string(build.join(".built-by")).ok()? == stamp()).then_some(exe)
}

fn stamp() -> String {
    let me = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok());
    me.map(|m| format!("{} {:?}", m.len(), m.modified().ok())).unwrap_or_default()
}

/// Replaces this process with the built game (same arguments); returns only if that failed.
pub fn handover(exe: &Path) -> std::io::Error {
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    // the built game looks for its button glyphs beside its own exe, deep in the build folder:
    // hand it the sets this app found (a release's assets/, LMA.app's Resources)
    for (var, dir) in glyph_env(crate::device::glyphs_dir, |v| std::env::var_os(v).is_some()) {
        cmd.env(var, dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.exec()
    }
    #[cfg(not(unix))]
    {
        match cmd.spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => e,
        }
    }
}

/// `LMA_GLYPHS_<SET>` for each glyph set `find` finds whose variable isn't set already (`is_set`).
fn glyph_env(find: impl Fn(&str) -> Option<PathBuf>, is_set: impl Fn(&str) -> bool) -> Vec<(String, PathBuf)> {
    ["ps5", "keyboard"].into_iter().filter_map(|set| {
        let var = format!("LMA_GLYPHS_{}", set.to_uppercase());
        if is_set(&var) { None } else { find(set).map(|d| (var, d)) }
    }).collect()
}

/// The source shipped with the app: `source/` next to the executable (or in a macOS bundle's
/// Resources), `LMA_SOURCE`, or, when run from a checkout, the checkout itself.
pub fn source_dir() -> Option<PathBuf> {
    let has = |d: &Path| d.join("lma-rs").join("Cargo.toml").is_file();
    if let Some(d) = std::env::var_os("LMA_SOURCE").map(PathBuf::from).filter(|d| has(d)) {
        return Some(d);
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for d in [dir.join("source"), dir.join("../Resources/source")] {
        if has(&d) {
            return Some(d);
        }
    }
    dir.ancestors().find(|d| has(d)).map(Path::to_path_buf)
}

/// A cargo to build with: the app's own toolchain, rustup's usual place, or one on PATH.
fn find_cargo(tools: &Path) -> Option<PathBuf> {
    let name = format!("cargo{EXE}");
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let candidates = [Some(tools.join("cargo").join("bin").join(&name)), home.map(|h| h.join(".cargo").join("bin").join(&name))];
    candidates.into_iter().flatten().find(|p| p.is_file()).or_else(|| {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|d| d.join(&name)).find(|p| p.is_file())
    })
}

/// The toolchain installed when there's none: on Windows the GNU one, which brings its own linker
/// (MSVC's needs Visual Studio; the PC builds with GNU too).
const TOOLCHAIN: &str = if cfg!(windows) { "stable-x86_64-pc-windows-gnu" } else { "stable" };

/// What a build needs besides Rust: a C compiler and linker (macOS: Apple's Command Line Tools,
/// whose installer this opens; Linux: the distribution's build tools). None when it's there.
fn missing_c_tools() -> Option<String> {
    let has = |cmd: &str, args: &[&str]| Command::new(cmd).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if cfg!(target_os = "macos") {
        if has("xcode-select", &["-p"]) {
            return None;
        }
        // the installer's window once a run: a retry while it installs doesn't open it again
        static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let _ = Command::new("xcode-select").arg("--install").stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        return Some("Apple's Command Line Tools are needed to build the game: install them in the window that just opened (a few minutes), then press Enter".into());
    }
    if cfg!(windows) || has("cc", &["--version"]) {
        return None;
    }
    Some("a C compiler is needed to build the game: install your distribution's build tools (build-essential, base-devel or gcc), then press Enter".into())
}

/// rustup's installer for this computer.
fn rustup_url() -> Option<String> {
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-gnu",
        _ => return None,
    };
    Some(format!("https://static.rust-lang.org/rustup/dist/{triple}/rustup-init{EXE}"))
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    let ok = if cfg!(windows) {
        Command::new("powershell").args(["-NoProfile", "-Command", &format!("Invoke-WebRequest -UseBasicParsing -Uri '{url}' -OutFile '{}'", to.display())]).status()
    } else {
        Command::new("curl").args(["-fsSL", "-o"]).arg(to).arg(url).status()
    };
    match ok {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!("couldn't download the compiler from {url} (is this computer online?)")),
    }
}

/// The toolchain's environment: a private one lives in `tools/` (CARGO_HOME, RUSTUP_HOME).
fn cargo_env(cmd: &mut Command, tools: &Path, cargo: &Path) {
    if cargo.starts_with(tools) {
        cmd.env("CARGO_HOME", tools.join("cargo")).env("RUSTUP_HOME", tools.join("rustup"));
    }
    if let Some(bin) = cargo.parent() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&path))).unwrap_or(path);
        cmd.env("PATH", joined);
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        // never the source's own build output, git data or recompiled parts
        if matches!(name.to_str(), Some("target" | "target-pgo" | ".git" | "parts")) {
            continue;
        }
        let (src, dst) = (e.path(), to.join(&name));
        if e.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// The packages in `cargo tree --prefix none` output (a repeated one ends with "(*)").
fn tree_count(out: &[u8]) -> u32 {
    let s = String::from_utf8_lossy(out);
    s.lines().map(|l| l.trim_end_matches(" (*)").trim()).filter(|l| !l.is_empty()).collect::<std::collections::BTreeSet<_>>().len() as u32
}

/// The number of crates a build of `-p lma-app` compiles, from cargo's lock (the progress bar's
/// whole; build scripts make the true count a little higher, so the bar stops at 99 until done).
fn crate_count(lock: &Path) -> u32 {
    std::fs::read_to_string(lock).map(|s| s.matches("[[package]]").count() as u32).unwrap_or(600).max(1)
}

/// The whole preparation (run on its own thread): ends Done or Failed.
pub fn run(xex: &Path, state: &Mutex<State>) {
    let set = |s: State| *state.lock().unwrap() = s;
    match prepare(xex, &set) {
        Ok(exe) => set(State::Done(exe)),
        Err(e) => set(State::Failed(e)),
    }
}

fn prepare(xex: &Path, set: &dyn Fn(State)) -> Result<PathBuf, String> {
    let work = |stage, pct, detail: &str| set(State::Working { stage, pct, detail: detail.to_string() });
    let data = data_dir().ok_or("no folder for the app's data on this computer")?;
    let (tools, build) = (data.join("tools"), data.join("build"));
    // 1. a compiler
    work(0, None, "Looking for Rust on this computer");
    if let Some(e) = missing_c_tools() {
        return Err(e);
    }
    let cargo = match find_cargo(&tools) {
        Some(c) => c,
        None => {
            std::fs::create_dir_all(&tools).map_err(|e| e.to_string())?;
            let url = rustup_url().ok_or("this computer's system isn't supported yet")?;
            let init = tools.join(format!("rustup-init{EXE}"));
            work(0, None, "Downloading the Rust compiler (about 300 MB once)");
            download(&url, &init)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o755));
            }
            work(0, None, "Installing the Rust compiler (private to this app)");
            let st = Command::new(&init)
                .args(["-y", "--profile", "minimal", "--default-toolchain", TOOLCHAIN, "--no-modify-path"])
                .env("CARGO_HOME", tools.join("cargo"))
                .env("RUSTUP_HOME", tools.join("rustup"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|e| e.to_string())?;
            if !st.success() {
                return Err("the Rust compiler couldn't be installed".into());
            }
            find_cargo(&tools).ok_or("the Rust compiler was installed but cargo isn't there")?
        }
    };
    // 2. the source, copied where the build may write
    work(1, None, "");
    let src = source_dir().ok_or("the game's source isn't next to the app (a 'source' folder)")?;
    if !build.join("lma-rs").join("Cargo.toml").is_file() || std::fs::read_to_string(build.join(".built-by")).ok().as_deref() != Some(&stamp()) {
        let _ = std::fs::remove_file(build.join(".built-by"));
        copy_tree(&src, &build).map_err(|e| format!("copying the source: {e}"))?;
    }
    let rs = build.join("lma-rs");
    let cargo_cmd = |args: &[&str]| {
        let mut c = Command::new(&cargo);
        c.args(args).current_dir(&rs);
        cargo_env(&mut c, &tools, &cargo);
        c
    };
    // 3. the recompiler, then the game's code from the player's default.xex
    work(2, None, "Building the recompiler");
    let out = cargo_cmd(&["build", "--release", "-p", "recomp"]).stdout(Stdio::null()).stderr(Stdio::piped()).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("building the recompiler failed:\n{}", tail(&out.stderr)));
    }
    work(2, None, "Translating every function of the game (a minute or two)");
    let recomp = rs.join("target").join("release").join(format!("lma-recomp{EXE}"));
    let out = Command::new(&recomp).arg(xex).args(["all", "crates/game-blocks", "999999", "32"]).current_dir(&rs).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("recompiling the game failed:\n{}", tail(&out.stderr)));
    }
    // 4. the game, with a progress bar from cargo's own messages
    // the whole: the packages this build compiles (cargo tree), else every package in the lock
    let tree = cargo_cmd(&["tree", "-p", "lma-app", "-e", "normal,build", "--prefix", "none"]).stderr(Stdio::null()).output().ok();
    let total = tree.filter(|o| o.status.success()).map(|o| tree_count(&o.stdout)).filter(|&n| n > 0).unwrap_or_else(|| crate_count(&rs.join("Cargo.lock")));
    work(3, Some(0), "");
    // the game's sound: the decoder-only static FFmpeg shipped beside the source (tools/ffmpeg_min.sh's
    // output as `ffmpeg-min/`, LGPL); without it the game builds and runs silent
    let ffmpeg = build.join("ffmpeg-min");
    let sound = ffmpeg.join("include").is_dir() && ffmpeg.join("lib").is_dir();
    let mut args = vec!["build", "--release", "-p", "lma-app", "--message-format", "json-render-diagnostics"];
    if sound {
        args.extend(["--features", "xma-ffmpeg"]);
    }
    let mut cmd = cargo_cmd(&args);
    if sound {
        cmd.env("FFMPEG_DIR", &ffmpeg).env("FFMPEG_STATIC", "1");
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stderr = child.stderr.take().map(|e| std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(e), &mut v);
        v
    }));
    let mut done = 0u32;
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        // one artifact per package (a build script is a second one: not counted)
        if line.contains("\"reason\":\"compiler-artifact\"") && !line.contains("\"kind\":[\"custom-build\"]") {
            done += 1;
            let name = line.split("\"name\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
            // the 32 parts (the game's own code) are the slow ones
            let detail = if name.starts_with("gb") { format!("the game's code: part {name}") } else { name.to_string() };
            work(3, Some((done * 100 / total).min(99)), &detail);
        }
    }
    let st = child.wait().map_err(|e| e.to_string())?;
    let err = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    if !st.success() {
        return Err(format!("building the game failed:\n{}", tail(&err)));
    }
    let exe = rs.join("target").join("release").join(format!("lma-app{EXE}"));
    std::fs::write(build.join(".built-by"), stamp()).map_err(|e| e.to_string())?;
    Ok(exe)
}

/// The last lines of a tool's output, for an error message.
fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(8)..].join("\n")
}

/// The launcher's text for the preparation.
pub fn note(state: &State) -> String {
    match state {
        State::Idle => String::new(),
        State::Working { stage, pct, detail } => {
            let mut s = String::from("Getting LMA Manager 2007 ready for this computer.\nThis happens once and can take 10-30 minutes; you can leave it running.\n\n");
            for (i, name) in STAGES.iter().enumerate() {
                let mark = if i < *stage { "done " } else if i == *stage { ">>   " } else { "     " };
                s += &format!("{mark}{name}\n");
            }
            if let Some(p) = pct {
                let filled = (*p as usize * 30) / 100;
                s += &format!("\n[{}{}] {p}%\n", "#".repeat(filled), "-".repeat(30 - filled));
            }
            if !detail.is_empty() {
                s += &format!("\n{detail}\n");
            }
            s
        }
        State::Done(_) => "Ready. Starting the game...".into(),
        State::Failed(e) => format!("Getting the game ready didn't work:\n\n{e}\n\nPress Enter (A on a pad) to try again."),
    }
}

/// Starts the preparation for the game at `xex` (once).
pub fn start(prepare: &Prepare, xex: PathBuf) {
    let mut s = prepare.0.lock().unwrap();
    if matches!(*s, State::Working { .. } | State::Done(_)) {
        return;
    }
    *s = State::Working { stage: 0, pct: None, detail: String::new() };
    let state = prepare.0.clone();
    std::thread::spawn(move || run(&xex, &state));
}

/// The launcher follows the preparation, and hands over to the built game when it's done.
pub fn follow(prepare: bevy::prelude::Res<Prepare>, mut launcher: bevy::prelude::ResMut<crate::launcher::Launcher>) {
    let state = prepare.0.lock().unwrap().clone();
    if state == State::Idle {
        return;
    }
    let text = note(&state);
    if launcher.note != text {
        launcher.note = text;
    }
    if let State::Done(exe) = state {
        let e = handover(&exe);
        *prepare.0.lock().unwrap() = State::Failed(format!("the game is built but couldn't be started: {e}\n{}", exe.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built game gets the glyph sets the shell app found, but never over a variable the
    /// player set (#908 review).
    #[test]
    fn the_handover_passes_the_glyph_sets_on() {
        let find = |set: &str| (set == "ps5").then(|| PathBuf::from("/app/assets/glyphs/ps5"));
        assert_eq!(glyph_env(find, |_| false), [("LMA_GLYPHS_PS5".to_string(), PathBuf::from("/app/assets/glyphs/ps5"))]);
        assert!(glyph_env(find, |v| v == "LMA_GLYPHS_PS5").is_empty());
    }

    #[test]
    fn the_note_lists_the_stages_with_a_bar() {
        let n = note(&State::Working { stage: 3, pct: Some(50), detail: "the game's code: part gb07".into() });
        assert!(n.contains("done Setting up the compiler"), "{n}");
        assert!(n.contains(">>   Building the game"), "{n}");
        assert!(n.contains(&format!("[{}{}] 50%", "#".repeat(15), "-".repeat(15))), "{n}");
        assert!(n.ends_with("part gb07\n"), "{n}");
    }

    #[test]
    fn the_copy_skips_build_output_and_parts() {
        let d = std::env::temp_dir().join(format!("lma-prepare-copy-{}", std::process::id()));
        let (src, dst) = (d.join("src"), d.join("dst"));
        for p in ["lma-rs/crates/a.rs", "lma-rs/target/big", "lma-rs/crates/game-blocks/parts/gb00.rs", ".git/HEAD"] {
            let f = src.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"x").unwrap();
        }
        copy_tree(&src, &dst).unwrap();
        assert!(dst.join("lma-rs/crates/a.rs").is_file());
        for p in ["lma-rs/target", "lma-rs/crates/game-blocks/parts", ".git"] {
            assert!(!dst.join(p).exists(), "{p} copied");
        }
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_rustup_installer_for_this_computer() {
        let url = rustup_url().expect("the Mac and the PC are supported");
        assert!(url.starts_with("https://static.rust-lang.org/rustup/dist/") && url.contains("rustup-init"), "{url}");
    }

    /// The machines that build this (the Mac with its Command Line Tools, CI) have a C compiler,
    /// so the check passes there; on Windows the GNU toolchain brings its own.
    #[test]
    fn a_build_machine_has_its_c_tools() {
        assert_eq!(missing_c_tools(), None);
        assert!(!cfg!(windows) || TOOLCHAIN.ends_with("windows-gnu"));
    }

    /// A later start hands over to the built game only while the stamp is this app's: an updated
    /// app (another size or time) builds again.
    #[test]
    fn the_built_game_is_used_only_with_this_apps_stamp() {
        let d = std::env::temp_dir().join(format!("lma-prepare-built-{}", std::process::id()));
        let exe = d.join("build/lma-rs/target/release").join(format!("lma-app{EXE}"));
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();
        // LMA_DATA_DIR is read only by data_dir(), and no other test sets it
        std::env::set_var("LMA_DATA_DIR", &d);
        assert_eq!(built_game(), None, "no stamp: not built by this app");
        std::fs::write(d.join("build/.built-by"), "1 other").unwrap();
        assert_eq!(built_game(), None, "another app's stamp");
        std::fs::write(d.join("build/.built-by"), stamp()).unwrap();
        assert_eq!(built_game(), Some(exe));
        std::env::remove_var("LMA_DATA_DIR");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_failure_says_why_and_how_to_retry() {
        let n = note(&State::Failed("no network".into()));
        assert!(n.contains("no network") && n.contains("Press Enter (A on a pad) to try again"), "{n}");
    }

    #[test]
    fn the_tree_counts_each_package_once() {
        assert_eq!(tree_count(b"lma-app v0.1.0 (/x)\nbevy v0.16.1\nlog v0.4.27\nlog v0.4.27 (*)\n\n"), 3);
    }

    #[test]
    fn the_crate_count_comes_from_the_lock() {
        let d = std::env::temp_dir().join(format!("lma-prepare-lock-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("Cargo.lock"), "[[package]]\nname = \"a\"\n\n[[package]]\nname = \"b\"\n").unwrap();
        assert_eq!(crate_count(&d.join("Cargo.lock")), 2);
        std::fs::remove_dir_all(d).ok();
    }

    /// The whole first run, from a default.xex to a built game (10-30 min):
    /// `LMA_PREPARE_XEX=<default.xex> LMA_DATA_DIR=<empty dir> LMA_SOURCE=<checkout> cargo test -p lma-app --bin lma-app -- --ignored the_whole_first_run --nocapture`
    #[test]
    #[ignore]
    fn the_whole_first_run() {
        let xex = PathBuf::from(std::env::var_os("LMA_PREPARE_XEX").expect("LMA_PREPARE_XEX"));
        let last = Mutex::new(String::new());
        let r = prepare(&xex, &|s| {
            let n = note(&s);
            let mut l = last.lock().unwrap();
            if *l != n {
                eprintln!("{n}\n--");
                *l = n;
            }
        });
        let exe = r.expect("the preparation failed");
        assert!(exe.is_file(), "{}", exe.display());
    }
}
