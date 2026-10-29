//! First run (#766 b): when no game is configured, the launcher asks for the player's own disc.
//! Dropping a disc image (.iso), a Games-on-Demand package (its title folder or header file) or a
//! folder holding `default.xex` onto the window starts the install: a folder is used where it is,
//! an image or package is extracted (`lma_data::disc`) into the app's data folder, next to the
//! config file, with its progress on the launcher. Once done the game folder is remembered in the
//! config and the launcher shows the versions as usual. Nothing from the game ships with the app.

use bevy::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where an install stands; shared with the worker thread.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Waiting for the player to drop their disc.
    Idle,
    /// Reading `from`: `done` of `total` bytes extracted.
    Working { from: String, done: u64, total: u64 },
    /// Decoding the game's sound once (`audio_cache_dir`): `done` of `total` files.
    Caching { done: usize, total: usize },
    /// The game folder (holds default.xex).
    Done(PathBuf),
    /// What went wrong; the player can drop something else.
    Failed(String),
}

#[derive(Resource, Clone)]
pub struct Install(pub Arc<Mutex<State>>);

impl Default for Install {
    fn default() -> Self {
        Install(Arc::new(Mutex::new(State::Idle)))
    }
}

/// The folder an extracted disc goes to: `game/` next to the config file.
pub fn install_dir() -> Option<PathBuf> {
    crate::config::path().and_then(|p| p.parent().map(|d| d.join("game")))
}

/// The decoded-audio cache the install builds (`runtime::xact_cache`, #824): every XMA wave of
/// the disc decoded once, next to the config file, so native audio (#828) never decodes during
/// play. `use_audio_cache` hands it to the runtime.
pub fn audio_cache_dir() -> Option<PathBuf> {
    crate::config::path().and_then(|p| p.parent().map(|d| d.join("audio-cache")))
}

/// Point the runtime at the install's audio cache, when there is one and LMA_AUDIO_CACHE isn't set.
pub fn use_audio_cache() {
    if std::env::var_os("LMA_AUDIO_CACHE").is_none() {
        if let Some(dir) = audio_cache_dir().filter(|d| d.is_dir()) {
            runtime::native_audio::set_audio_cache(dir);
        }
    }
}

/// Build the audio cache for the game in `game` into `out` (one commentary language, the one
/// native audio plays: LMA_AUDIO_LANGUAGE, default Data_01), with progress in `state`. A cache that
/// is already there is kept. Errors are reported and the install still succeeds: native audio
/// then decodes on demand.
fn build_audio_cache(game: &Path, out: &Path, state: &Mutex<State>) {
    if out.is_dir() {
        return;
    }
    let language = std::env::var("LMA_AUDIO_LANGUAGE").unwrap_or_else(|_| "Data_01".into());
    let tmp = out.with_extension("part");
    let _ = std::fs::remove_dir_all(&tmp);
    let res = runtime::xact_cache::build(game, Some(&language), &tmp, &|done, total| *state.lock().unwrap() = State::Caching { done, total });
    match res {
        // nothing decoded (no sound banks found): no cache, rather than an empty one that hides them
        Ok(sum) if sum.waves == 0 => {
            let _ = std::fs::remove_dir_all(&tmp);
            eprintln!("install: no sound banks to decode; native audio decodes as it plays");
        }
        Ok(_) => {
            if let Err(e) = std::fs::rename(&tmp, out) {
                eprintln!("install: the audio cache couldn't be put in place ({e})");
            }
        }
        Err(e) => eprintln!("install: the audio cache wasn't built ({e}); native audio decodes as it plays"),
    }
}

/// The launcher's text for the install: what to do, or how far it has got.
pub fn note(state: &State) -> String {
    const ASK: &str = "Drop your LMA Manager 2007 disc onto this window,\nor press Enter (A on a pad) to browse for it:\n  a disc image (.iso), a Games-on-Demand folder,\n  or a folder that holds default.xex.\nNothing is uploaded: the files stay on this computer.";
    match state {
        State::Idle => format!("Welcome. The game isn't set up yet.\n\n{ASK}"),
        State::Working { from, done, total } => {
            let pct = if *total > 0 { done * 100 / total } else { 0 };
            format!("Installing from {from}\n{pct}%  ({} of {} MB)\n\nThis reads the whole disc once; it can take a few minutes.", done / 1_000_000, total / 1_000_000)
        }
        State::Caching { done, total } => format!("Preparing the game's sound ({done} of {total} files)\n\nThis happens once, after the install."),
        State::Done(dir) => format!("Installed: {}", dir.display()),
        State::Failed(e) => format!("That didn't work: {e}\n\n{ASK}"),
    }
}

/// The install itself (run on its own thread): a folder holding default.xex is used as is,
/// anything else is opened as a disc and extracted to `dest`; then the game's sound is decoded
/// into `cache` when given (`audio_cache_dir`). The state ends Done or Failed.
pub fn run(source: &Path, dest: &Path, cache: Option<&Path>, state: &Mutex<State>) {
    let set = |s: State| *state.lock().unwrap() = s;
    let from = source.file_name().map_or_else(|| source.display().to_string(), |n| n.to_string_lossy().into_owned());
    let mut disc = match lma_data::disc::Disc::open(source) {
        Ok(d) => d,
        Err(e) => return set(State::Failed(e.to_string())),
    };
    if disc.kind() == lma_data::disc::Kind::Folder {
        if let Some(out) = cache {
            build_audio_cache(source, out, state);
        }
        return set(State::Done(source.to_path_buf()));
    }
    set(State::Working { from: from.clone(), done: 0, total: 0 });
    let res = disc.extract(dest, |_| true, |done, total| set(State::Working { from: from.clone(), done, total }));
    match res {
        Ok(_) if dest.join("default.xex").is_file() => {
            if let Some(out) = cache {
                build_audio_cache(dest, out, state);
            }
            set(State::Done(dest.to_path_buf()))
        }
        Ok(_) => set(State::Failed(format!("{from} has no default.xex: is it LMA Manager 2007?"))),
        Err(e) => set(State::Failed(e.to_string())),
    }
}

/// Starts installing from `source` on its own thread.
fn begin(source: PathBuf, install: &Install) {
    let Some(dest) = install_dir() else {
        *install.0.lock().unwrap() = State::Failed("no folder for the app's data on this computer".into());
        return;
    };
    let state = install.0.clone();
    *state.lock().unwrap() = State::Working { from: source.display().to_string(), done: 0, total: 0 };
    let cache = audio_cache_dir();
    std::thread::spawn(move || run(&source, &dest, cache.as_deref(), &state));
}

/// A file dropped on the window while the game isn't set up starts the install (one at a time).
pub fn on_drop(mut drops: MessageReader<bevy::window::FileDragAndDrop>, launcher: Res<crate::launcher::Launcher>, install: Res<Install>) {
    for d in drops.read() {
        let bevy::window::FileDragAndDrop::DroppedFile { path_buf, .. } = d else { continue };
        if !launcher.needs_game() || matches!(*install.0.lock().unwrap(), State::Working { .. } | State::Caching { .. }) {
            continue;
        }
        begin(path_buf.clone(), &install);
    }
}

/// The system's own file picker, in its own process (no window toolkit in the app): Ok(None) if the
/// player cancelled, Err if this computer has none (Linux without zenity or kdialog).
pub fn pick_file() -> Result<Option<PathBuf>, String> {
    const PROMPT: &str = "Choose your LMA Manager 2007 disc image (.iso)";
    let out = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript").args(["-e", &format!("POSIX path of (choose file with prompt \"{PROMPT}\")")]).output()
    } else if cfg!(windows) {
        let ps = format!("Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.OpenFileDialog; $d.Title = '{PROMPT}'; $d.Filter = 'Disc image (*.iso)|*.iso|All files (*.*)|*.*'; if ($d.ShowDialog() -eq 'OK') {{ $d.FileName }}");
        std::process::Command::new("powershell").args(["-NoProfile", "-STA", "-Command", &ps]).output()
    } else {
        std::process::Command::new("zenity")
            .args(["--file-selection", &format!("--title={PROMPT}")])
            .output()
            .or_else(|_| std::process::Command::new("kdialog").args(["--getopenfilename", ".", "*.iso", "--title", PROMPT]).output())
    };
    let out = out.map_err(|_| "this computer has no file picker (zenity or kdialog): drop the disc image onto this window instead".to_string())?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

/// Enter / Space / A / a click while the game isn't set up opens the file picker (one at a time).
pub fn browse(keys: Res<ButtonInput<KeyCode>>, mouse: Res<ButtonInput<MouseButton>>, gamepads: Query<&Gamepad>, launcher: Res<crate::launcher::Launcher>, install: Res<Install>, prepare: Res<crate::prepare::Prepare>, mut open: Local<Option<std::sync::mpsc::Receiver<Result<Option<PathBuf>, String>>>>) {
    if let Some(rx) = open.as_ref() {
        match rx.try_recv() {
            Ok(Ok(Some(path))) => {
                *open = None;
                begin(path, &install);
            }
            Ok(Err(e)) => {
                *open = None;
                *install.0.lock().unwrap() = State::Failed(e);
            }
            Ok(Ok(None)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => *open = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        return;
    }
    let pressed = keys.any_just_pressed([KeyCode::Enter, KeyCode::Space]) || mouse.just_pressed(MouseButton::Left) || gamepads.iter().any(|g| g.just_pressed(GamepadButton::South));
    // a preparation that failed (no network, a full disk) is tried again, from the installed game
    let failed = matches!(*prepare.0.lock().unwrap(), crate::prepare::State::Failed(_));
    if failed && pressed {
        if let Some(dir) = crate::config::Config::load().game_dir {
            crate::prepare::start(&prepare, dir.join("default.xex"));
        }
        return;
    }
    if !launcher.needs_game() || matches!(*install.0.lock().unwrap(), State::Working { .. } | State::Caching { .. } | State::Done(_)) || *prepare.0.lock().unwrap() != crate::prepare::State::Idle {
        return;
    }
    if pressed {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(pick_file());
        });
        *open = Some(rx);
    }
}

/// The launcher follows the install: its note while working, and once done the game folder
/// remembered in the config and the versions offered.
pub fn follow(install: Res<Install>, prepare: Res<crate::prepare::Prepare>, mut launcher: ResMut<crate::launcher::Launcher>) {
    // once the preparation runs (prepare.rs), it has the launcher's text
    if !launcher.needs_game() || *prepare.0.lock().unwrap() != crate::prepare::State::Idle {
        return;
    }
    let state = install.0.lock().unwrap().clone();
    if let State::Done(dir) = &state {
        use_audio_cache();
        let mut cfg = crate::config::Config::load();
        cfg.game_dir = Some(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone()));
        if let Err(e) = cfg.save() {
            launcher.note = format!("Installed, but the setting couldn't be saved: {e}");
        }
        // the downloaded app has no game in it: build it for this computer first (prepare.rs)
        if crate::prepare::is_shell() {
            crate::prepare::start(&prepare, dir.join("default.xex"));
            return;
        }
        launcher.game_found(dir.join("default.xex").to_string_lossy().into_owned());
        return;
    }
    let text = note(&state);
    if launcher.note != text {
        launcher.note = text;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_with_the_game_is_used_where_it_is() {
        let dir = std::env::temp_dir().join(format!("lma-install-folder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("default.xex"), b"x").unwrap();
        let state = Mutex::new(State::Idle);
        run(&dir, &dir.join("unused"), None, &state);
        assert_eq!(*state.lock().unwrap(), State::Done(dir.clone()));
        assert!(!dir.join("unused").exists(), "nothing is copied");
        std::fs::remove_dir_all(dir).ok();
    }

    /// An XDVDFS image with `default.xex` alone in its root (sector 32 the volume descriptor,
    /// 33 the root table, 34 the file).
    fn image(xex: &[u8]) -> Vec<u8> {
        use lma_data::disc::{MAGIC, SECTOR};
        let name = b"default.xex";
        let mut root = vec![0, 0, 0, 0];
        root.extend_from_slice(&34u32.to_le_bytes());
        root.extend_from_slice(&(xex.len() as u32).to_le_bytes());
        root.extend_from_slice(&[0x80, name.len() as u8]);
        root.extend_from_slice(name);
        let mut img = vec![0u8; 35 * SECTOR as usize];
        let at = |n: u64| (n * SECTOR) as usize;
        img[at(32)..at(32) + 20].copy_from_slice(MAGIC);
        img[at(32) + 20..at(32) + 24].copy_from_slice(&33u32.to_le_bytes());
        img[at(32) + 24..at(32) + 28].copy_from_slice(&(root.len() as u32).to_le_bytes());
        img[at(33)..at(33) + root.len()].copy_from_slice(&root);
        img[at(34)..at(34) + xex.len()].copy_from_slice(xex);
        img
    }

    /// A disc image is extracted into the app's folder, which then holds the game (the first run
    /// prepares it from there).
    #[test]
    fn a_disc_image_is_installed_into_the_apps_folder() {
        let dir = std::env::temp_dir().join(format!("lma-install-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("LMA.iso"), image(b"XEX2 test")).unwrap();
        let state = Mutex::new(State::Idle);
        run(&dir.join("LMA.iso"), &dir.join("game"), None, &state);
        assert_eq!(*state.lock().unwrap(), State::Done(dir.join("game")));
        assert_eq!(std::fs::read(dir.join("game").join("default.xex")).unwrap(), b"XEX2 test");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn something_else_fails_with_a_reason_and_asks_again() {
        let dir = std::env::temp_dir().join(format!("lma-install-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("photo.jpg"), vec![0u8; 4096]).unwrap();
        let state = Mutex::new(State::Idle);
        run(&dir.join("photo.jpg"), &dir.join("out"), None, &state);
        let s = state.lock().unwrap().clone();
        assert!(matches!(&s, State::Failed(e) if e.contains("not an Xbox 360 disc image")), "{s:?}");
        assert!(note(&s).contains("Drop your LMA Manager 2007 disc"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// A game folder without the game's sound banks gets no audio cache (an empty one would hide
    /// them from native audio later), and the install still succeeds.
    #[test]
    fn no_sound_banks_no_audio_cache() {
        let dir = std::env::temp_dir().join(format!("lma-install-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("default.xex"), b"XEX2").unwrap();
        let cache = dir.join("audio-cache");
        let state = Mutex::new(State::Idle);
        run(&dir, &dir.join("unused"), Some(&cache), &state);
        assert_eq!(*state.lock().unwrap(), State::Done(dir.clone()));
        assert!(!cache.exists() && !cache.with_extension("part").exists(), "no empty cache left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_reads_in_percent_and_megabytes() {
        let n = note(&State::Working { from: "LMA.iso".into(), done: 3_500_000_000, total: 7_000_000_000 });
        assert!(n.starts_with("Installing from LMA.iso\n50%  (3500 of 7000 MB)"), "{n}");
        assert!(note(&State::Idle).contains("Welcome"));
    }
}
