//! The launcher (#4): shown before the game when lma-app starts without arguments or automation
//! variables, so a player picks what to play without environment variables: first the version
//! (the original game or a mod space, each with its own saves and quick state), then how to start
//! it: continue from the quick save, boot fresh, or resume any save state. Keyboard (arrows / WASD,
//! Enter / Space), gamepad (d-pad or stick, A / START) and mouse (wheel, click) all work.

use bevy::prelude::*;
use std::path::{Path, PathBuf};

/// What the game thread needs to start, kept until the player chooses.
pub struct Pending {
    pub xex: String,
    pub mod_id: Option<String>,
    pub software: bool,
}

/// What an item does.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    /// Start the game: from this save state, or boot (None).
    Start(Option<PathBuf>),
    /// Switch between a window and borderless fullscreen (saved in the config).
    Fullscreen,
    /// Play this version: a mod space (`id`), or the original game (None). Saved in the config; the
    /// Start rows follow it (each version has its own quick state and states folder).
    Version { id: Option<String>, desc: String },
}

#[derive(Resource)]
pub struct Launcher {
    /// The version rows, in menu order (`versions`); `items` starts with them.
    pub versions: Vec<(String, Choice)>,
    pub items: Vec<(String, Choice)>,
    pub sel: usize,
    /// Some until the player has chosen; then the game is running.
    pub pending: Option<Pending>,
    /// Shown above the choices (e.g. the game wasn't found).
    pub note: String,
    /// The fullscreen setting, as saved in the config.
    pub fullscreen: bool,
    /// The folder holding the save states (`states/`; a mod's own are in `states/<id>/`).
    pub states_root: PathBuf,
}

impl Launcher {
    /// The items for the pending version: the version rows, then the ways to start it, then the
    /// window row. Called when the version changes; the selection goes to the first way to start.
    pub fn rebuild(&mut self) {
        let version = self.pending.as_ref().and_then(|p| p.mod_id.as_deref());
        let mut items = self.versions.clone();
        items.extend(starts(&self.states_root, version));
        items.push((String::new(), Choice::Fullscreen)); // labelled from the config when shown
        self.sel = items.iter().position(|(_, c)| matches!(c, Choice::Start(_))).unwrap_or(0);
        self.items = items;
    }
}

/// The valid mod spaces under `mods_root` as (id, "name version").
pub fn mod_spaces(mods_root: &Path) -> Vec<(String, String)> {
    runtime::mods::ModSpace::list(mods_root).into_iter().flatten().map(|m| (m.id, format!("{} {}", m.name, m.version).trim().to_string())).collect()
}

/// The versions to choose from. With `mods/menu.txt`: its rows, one per line `id|label|description`
/// (`-` for the original game; the description is optional), in that order, without the mod spaces
/// that don't load. Without it: every valid mod space, newest first (by its mod.toml's date), then
/// the original game.
pub fn versions(mods_root: &Path, valid: &[(String, String)]) -> Vec<(String, Choice)> {
    let row = |id: &str, label: &str, desc: &str| (label.to_string(), Choice::Version { id: (id != "-").then(|| id.to_string()), desc: desc.to_string() });
    if let Ok(text) = std::fs::read_to_string(mods_root.join("menu.txt")) {
        return text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .map(|l| {
                let mut f = l.splitn(3, '|').map(str::trim);
                (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""))
            })
            .filter(|(id, _, _)| *id == "-" || valid.iter().any(|(m, _)| m == id))
            .map(|(id, label, desc)| row(id, if label.is_empty() { id } else { label }, desc))
            .collect();
    }
    let mut mods: Vec<(std::time::SystemTime, &(String, String))> = valid
        .iter()
        .map(|m| (std::fs::metadata(mods_root.join(&m.0).join("mod.toml")).and_then(|d| d.modified()).unwrap_or(std::time::UNIX_EPOCH), m))
        .collect();
    mods.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1 .0.cmp(&a.1 .0))); // newest first
    let mut out: Vec<(String, Choice)> = mods.into_iter().map(|(_, (id, label))| row(id, label, &format!("mod space {id}"))).collect();
    out.push(row("-", "Original game 2006/07", "the game as it shipped"));
    out
}

/// Written into `states/` by `adopt_old_quick_state` on its first run.
pub const MOVED_MARKER: &str = ".quick-state-per-version";

/// A version's own states folder: `states/<id>/` for a mod space, `states/` for the original game.
pub fn states_dir(states_root: &Path, version: Option<&str>) -> PathBuf {
    version.map_or(states_root.to_path_buf(), |id| states_root.join(id))
}

/// The quick state of builds before versions had their own folders (`states/quick.state`) was
/// made by whichever version was played last (`last`, the config's): moved once into that
/// version's folder, so it stays that version's Continue and the original game's Continue can't
/// load a modded career. Only when the last version is a mod space without a quick state of its
/// own yet. Runs once per states folder (the marker file `MOVED_MARKER`), so a shared quick state
/// made later, the original game's own, is never moved into a version that has none. Returns a note.
pub fn adopt_old_quick_state(states_root: &Path, last: Option<&str>) -> Option<String> {
    let marker = states_root.join(MOVED_MARKER);
    if marker.exists() {
        return None;
    }
    let _ = std::fs::create_dir_all(states_root).and_then(|()| std::fs::write(&marker, "Since this file was written each version keeps its quick state in its own folder (states/<id>/);\nthe shared quick.state here is the original game's. Written once by lma-app (launcher::adopt_old_quick_state).\n"));
    let id = last?;
    let old = states_root.join("quick.state");
    let dir = states_root.join(id);
    let new = dir.join("quick.state");
    if !old.exists() || new.exists() {
        return None;
    }
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::rename(&old, &new)) {
        Ok(()) => Some(format!("the quick save is now {} (the version played last: {id})", new.display())),
        Err(e) => Some(format!("couldn't move {} to {}: {e}", old.display(), new.display())),
    }
}

#[derive(Component)]
pub struct LauncherText;

/// A held direction moves the selection once at once, then again every `EVERY` seconds after
/// `DELAY` (as a keyboard's repeat), so a long list can be run through without tapping.
#[derive(Default)]
pub struct Repeat {
    dir: i32,
    held: f32,
}

impl Repeat {
    const DELAY: f32 = 0.35;
    const EVERY: f32 = 0.07;

    /// The steps to move this frame with `dir` (-1 up, 0 none, 1 down) held for `dt` more seconds.
    pub fn step(&mut self, dir: i32, dt: f32) -> i32 {
        if dir != self.dir {
            (self.dir, self.held) = (dir, 0.0);
            return dir;
        }
        if dir == 0 {
            return 0;
        }
        let repeats = |t: f32| if t < Self::DELAY { 0 } else { ((t - Self::DELAY) / Self::EVERY) as i32 + 1 };
        let before = self.held;
        self.held += dt;
        dir * (repeats(self.held) - repeats(before))
    }
}

/// The launcher's font size for `lines` rows of at most `chars` characters in a `w` x `h` window
/// (logical pixels): 24 px when the box fits in 90% of the window, smaller when it doesn't (rows
/// 1.2 em apart and characters 0.6 em wide in bevy's monospaced default font, padding 1 em a side),
/// never below 11 px. Whole pixels: bevy keeps a glyph atlas per size.
pub fn font_for(lines: usize, chars: usize, w: f32, h: f32) -> f32 {
    let by_h = h * 0.9 / (lines.max(1) as f32 * 1.2 + 2.0);
    let by_w = w * 0.9 / (chars.max(1) as f32 * 0.6 + 2.0);
    by_h.min(by_w).floor().clamp(11.0, 24.0)
}

/// Whether to show the launcher: no game path given and nothing scripted or replayed.
pub fn wanted(arg_given: bool) -> bool {
    !arg_given && ["LMA_LOAD", "LMA_INPUT", "LMA_INPUT_LIVE", "LMA_REPLAY", "LMA_RECORD", "LMA_NO_LAUNCHER"].iter().all(|v| std::env::var_os(v).is_none())
}

/// The save states in `dir`, newest first, without the quick state.
fn states_in(dir: &Path) -> Vec<PathBuf> {
    let mut states: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "state") && p.file_name().is_some_and(|n| n != "quick.state"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    states.sort_by_key(|s| std::cmp::Reverse(s.0));
    states.into_iter().map(|(_, p)| p).collect()
}

/// The ways to start `version`: continue (its quick save) first when there is one, then a fresh
/// boot, then its other save states, newest first (at most 8). A mod space's rows come from its
/// own folder (`states/<id>/`) and then the shared `states/` (states saved before versions had
/// their own folders, marked "shared"); the original game's from `states/` only.
pub fn starts(states_root: &Path, version: Option<&str>) -> Vec<(String, Choice)> {
    let own = states_dir(states_root, version);
    let quick = own.join("quick.state");
    let mut out = Vec::new();
    if quick.exists() {
        out.push(("Continue (quick save)".to_string(), Choice::Start(Some(quick))));
    }
    out.push(("New boot".to_string(), Choice::Start(None)));
    let name = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut states: Vec<(String, PathBuf)> = states_in(&own).into_iter().map(|p| (format!("Load state: {}", name(&p)), p)).collect();
    if version.is_some() {
        states.extend(states_in(states_root).into_iter().map(|p| (format!("Load state: {} (shared)", name(&p)), p)));
    }
    out.extend(states.into_iter().take(8).map(|(label, p)| (label, Choice::Start(Some(p)))));
    out
}

/// Arrows / WASD / d-pad / left stick / wheel move (held: repeating), Enter / Space / A / START /
/// click choose.
#[allow(clippy::too_many_arguments)] // a bevy system: its parameters are what it reads
pub fn input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut wheel: MessageReader<bevy::input::mouse::MouseWheel>,
    gamepads: Query<&Gamepad>,
    mut l: ResMut<Launcher>,
    link: Res<crate::Link>,
    time: Res<Time>,
    mut windows: Query<&mut Window>,
    mut repeat: Local<Repeat>,
) {
    if l.pending.is_none() || l.items.is_empty() {
        return;
    }
    // LMA_LAUNCHER_PICK=<n>: choose item n after a second (checking the launcher without a person)
    let auto = std::env::var("LMA_LAUNCHER_PICK").ok().and_then(|v| v.parse::<usize>().ok()).filter(|&i| time.elapsed_secs() > 1.0 && matches!(l.items.get(i), Some((_, Choice::Start(_)))));
    if let Some(i) = auto {
        l.sel = i.min(l.items.len() - 1);
    }
    let pad = |b: GamepadButton| gamepads.iter().any(|g| g.just_pressed(b));
    let scroll: f32 = wheel.read().map(|w| w.y).sum();
    let n = l.items.len();
    // the stick counts once pushed past half way
    let stick = gamepads.iter().map(|g| g.left_stick().y).fold(0.0f32, |a, y| if y.abs() > a.abs() { y } else { a });
    let held = |key: [KeyCode; 2], b: GamepadButton| keys.any_pressed(key) || gamepads.iter().any(|g| g.pressed(b));
    let down = held([KeyCode::ArrowDown, KeyCode::KeyS], GamepadButton::DPadDown) || stick < -0.5;
    let up = held([KeyCode::ArrowUp, KeyCode::KeyW], GamepadButton::DPadUp) || stick > 0.5;
    let wheel = if scroll < 0.0 { 1 } else if scroll > 0.0 { -1 } else { 0 };
    let steps = repeat.step(down as i32 - up as i32, time.delta_secs()) + wheel;
    l.sel = (l.sel as i64 + steps as i64).rem_euclid(n as i64) as usize;
    if auto.is_some() || keys.any_just_pressed([KeyCode::Enter, KeyCode::Space]) || pad(GamepadButton::South) || pad(GamepadButton::Start) || mouse.just_pressed(MouseButton::Left) {
        match l.items[l.sel].1.clone() {
            Choice::Start(load) => {
                let p = l.pending.take().unwrap();
                crate::spawn_game(p.xex, p.mod_id, p.software, link.clone(), load.map(|p| p.to_string_lossy().into_owned()));
            }
            Choice::Version { id, .. } => {
                l.pending.as_mut().unwrap().mod_id = id.clone();
                let mut c = crate::config::Config::load();
                c.mod_space = id;
                if let Err(e) = c.save() {
                    l.note = format!("couldn't save the setting: {e}");
                }
                // the Start rows follow the version; on to the first of them (continue / new boot)
                l.rebuild();
            }
            Choice::Fullscreen => {
                let mut c = crate::config::Config::load();
                c.fullscreen = !l.fullscreen;
                l.fullscreen = c.fullscreen;
                if let Err(e) = c.save() {
                    l.note = format!("couldn't save the setting: {e}");
                }
                for mut w in &mut windows {
                    w.mode = crate::window_mode(c.fullscreen);
                }
            }
        }
    }
}

/// LMA_LAUNCHER_SHOT=<png> (checking the launcher's layout without a person, #655): once the
/// launcher has been shown for a second, the window as drawn is saved to <png> and the app quits.
pub fn shot(mut commands: Commands, l: Res<Launcher>, time: Res<Time>, mut wanted: Local<Option<Option<PathBuf>>>, mut taken: Local<bool>) {
    // the variable is read once, not every frame of the session
    let Some(path) = wanted.get_or_insert_with(|| std::env::var_os("LMA_LAUNCHER_SHOT").map(PathBuf::from)).clone() else { return };
    if *taken || l.pending.is_none() || time.elapsed_secs() < 1.0 {
        return;
    }
    *taken = true;
    use bevy::render::view::screenshot::{save_to_disk, Screenshot, ScreenshotCaptured};
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(PathBuf::from(path)))
        .observe(|_: On<ScreenshotCaptured>, mut exit: MessageWriter<AppExit>| {
            exit.write(AppExit::Success);
        });
}

/// `text` in lines of at most `width` characters, broken at spaces (a word longer than the width
/// stands alone), each with `indent` in front.
pub fn wrapped(text: &str, width: usize, indent: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(l) if l.chars().count() + 1 + word.chars().count() <= width => {
                l.push(' ');
                l.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines.iter().map(|l| format!("{indent}{l}\n")).collect()
}

/// The text of the menu: the versions (the chosen one ticked, its description under it), the ways
/// to start, the window row, where the chosen version keeps its careers, and the keys.
pub fn text(l: &Launcher, kind: crate::device::InputKind) -> String {
    let mut s = String::from("LMA Manager 2007\n\n");
    if !l.note.is_empty() {
        s.push_str(&l.note);
        s.push_str("\n\n");
    }
    let current = l.pending.as_ref().and_then(|p| p.mod_id.clone());
    let mut last: Option<u8> = None; // the kind of the row before: 0 version, 1 start, 2 window
    for (i, (label, choice)) in l.items.iter().enumerate() {
        let kind = match choice {
            Choice::Version { .. } => 0,
            Choice::Start(_) => 1,
            Choice::Fullscreen => 2,
        };
        if last != Some(kind) {
            s.push_str(match kind {
                0 => "Version\n",
                1 if last.is_some() => "\nStart\n",
                1 => "Start\n",
                _ => "\n",
            });
            last = Some(kind);
        }
        s.push_str(if i == l.sel { "> " } else { "  " });
        match choice {
            Choice::Version { id, desc } => {
                s.push_str(if *id == current { "[x] " } else { "[ ] " });
                s.push_str(label);
                s.push('\n');
                if !desc.is_empty() {
                    s.push_str(&wrapped(desc, 64, "      "));
                }
            }
            Choice::Fullscreen => s.push_str(if l.fullscreen { "Window: fullscreen (change)\n" } else { "Window: windowed (change)\n" }),
            Choice::Start(_) => {
                s.push_str(label);
                s.push('\n');
            }
        }
    }
    let saves = current.as_deref().map_or("userdata".to_string(), |id| format!("userdata/{id}"));
    s.push_str(&format!("\nCareers and settings of this version: {saves}\n{}", kind.hint()));
    s
}

pub fn show(l: Res<Launcher>, kind: Res<crate::device::InputKind>, mut text_node: Query<(&mut Text, &mut TextFont, &mut Node, &mut Visibility), With<LauncherText>>, windows: Query<&Window, With<bevy::window::PrimaryWindow>>) {
    let Ok((mut t, mut font, mut node, mut vis)) = text_node.single_mut() else { return };
    if l.pending.is_none() {
        *vis = Visibility::Hidden;
        return;
    }
    *vis = Visibility::Visible;
    let s = text(&l, *kind);
    // the box fits the window (centred by its parent node, main.rs setup)
    if let Ok(w) = windows.single() {
        let size = font_for(s.lines().count(), s.lines().map(|l| l.chars().count()).max().unwrap_or(0), w.width(), w.height());
        if !matches!(font.font_size, bevy::text::FontSize::Px(px) if px == size) {
            font.font_size = bevy::text::FontSize::Px(size);
            node.padding = UiRect::all(Val::Px(size));
        }
    }
    *t = Text::new(s);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lma-launcher-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A hold moves once, waits DELAY, then moves every EVERY; a new direction or a release starts
    /// over.
    #[test]
    fn a_held_direction_repeats() {
        let mut r = Repeat::default();
        let dt = 1.0 / 60.0;
        assert_eq!(r.step(1, dt), 1, "the press moves at once");
        let mut moved = 0;
        for _ in 0..20 {
            moved += r.step(1, dt); // up to ~0.35 s: no repeat yet
        }
        assert_eq!(moved, 0);
        for _ in 0..60 {
            moved += r.step(1, dt); // one more second: about 1 / 0.07 repeats
        }
        assert!((13..=16).contains(&moved), "{moved}");
        assert_eq!(r.step(-1, dt), -1, "turning moves at once");
        assert_eq!(r.step(0, dt), 0);
        assert_eq!(r.step(0, dt), 0);
        assert_eq!(r.step(1, dt), 1, "a new press after a release moves at once");
    }

    /// The menu's 24 px font is kept when the box fits and shrinks so that it fits a small window.
    #[test]
    fn the_font_fits_the_window() {
        assert_eq!(font_for(12, 30, 1280.0, 720.0), 24.0);
        // five versions with a description each, continue, new boot, eight states, the window row,
        // headings and the footer: 34 rows
        let f = font_for(34, 70, 1280.0, 720.0);
        assert!(f < 24.0 && (34.0 * 1.2 + 2.0) * f <= 720.0 * 0.9 + 0.01, "{f}");
        assert!(font_for(12, 60, 640.0, 720.0) < 24.0, "a narrow window: the width decides");
        assert_eq!(font_for(200, 30, 1280.0, 720.0), 11.0, "never below 11 px");
    }

    /// The original game's rows come from states/; a mod's from its own folder, then the shared
    /// ones; the quick state of each is its Continue.
    #[test]
    fn starts_follow_the_version() {
        let dir = temp("starts");
        assert_eq!(starts(&dir, None).len(), 1, "no states: a fresh boot only");
        std::fs::write(dir.join("old.state"), b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("new.state"), b"x").unwrap();
        std::fs::write(dir.join("quick.state"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        let labels = |v: Option<&str>| starts(&dir, v).into_iter().map(|(l, _)| l).collect::<Vec<_>>();
        assert_eq!(labels(None), ["Continue (quick save)", "New boot", "Load state: new", "Load state: old"]);
        // a mod with no states of its own: no Continue, the shared states marked
        assert_eq!(labels(Some("fc27")), ["New boot", "Load state: new (shared)", "Load state: old (shared)"]);
        std::fs::create_dir_all(dir.join("fc27")).unwrap();
        std::fs::write(dir.join("fc27/quick.state"), b"x").unwrap();
        std::fs::write(dir.join("fc27/mine.state"), b"x").unwrap();
        let rows = starts(&dir, Some("fc27"));
        assert_eq!(rows.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), ["Continue (quick save)", "New boot", "Load state: mine", "Load state: new (shared)", "Load state: old (shared)"]);
        assert_eq!(rows[0].1, Choice::Start(Some(dir.join("fc27/quick.state"))), "the mod's own quick state");
        std::fs::remove_dir_all(dir).ok();
    }

    /// The shared quick state of older builds goes to the version played last, on the first run
    /// only (the marker), and only if that was a mod space; a shared one made later is the
    /// original game's and stays, even for a version without a quick state of its own.
    #[test]
    fn the_old_quick_save_follows_the_last_version_once() {
        let dir = temp("adopt");
        std::fs::write(dir.join("quick.state"), b"x").unwrap();
        assert!(adopt_old_quick_state(&dir, Some("fc27")).unwrap().starts_with("the quick save is now"));
        assert!(dir.join("fc27/quick.state").exists() && !dir.join("quick.state").exists() && dir.join(MOVED_MARKER).exists());
        assert_eq!(starts(&dir, Some("fc27"))[0].0, "Continue (quick save)");
        assert_eq!(starts(&dir, None)[0].0, "New boot", "the original game has no Continue from it");
        std::fs::write(dir.join("quick.state"), b"y").unwrap(); // the original game's F5
        assert!(adopt_old_quick_state(&dir, Some("fc27final26")).is_none(), "moved once: a later shared one stays, even for a version without its own");
        assert!(dir.join("quick.state").exists() && !dir.join("fc27final26").exists());
        std::fs::remove_dir_all(dir).ok();
        // a folder whose first run has no old quick state is marked as well
        let dir = temp("adopt-fresh");
        assert!(adopt_old_quick_state(&dir, None).is_none());
        assert!(dir.join(MOVED_MARKER).exists());
        std::fs::write(dir.join("quick.state"), b"v").unwrap();
        assert!(adopt_old_quick_state(&dir, Some("fc27")).is_none(), "marked on the first run: the original game's F5 stays");
        assert!(dir.join("quick.state").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// menu.txt rows in its order with their descriptions, the original game as `-`, mod spaces
    /// that don't load left out, comments and blank lines skipped.
    #[test]
    fn versions_from_the_menu_file() {
        let dir = temp("menu");
        let valid = vec![("fc27b".to_string(), "x".to_string()), ("fc27a".to_string(), "y".to_string())];
        std::fs::write(dir.join("menu.txt"), "# newest first\nfc27b|FC 27, newest|real squads, modern money\n\ngone|a removed mod|x\nfc27a|FC 27, my career\n-|Original game 2006/07|as shipped\n").unwrap();
        let rows = versions(&dir, &valid);
        assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["FC 27, newest", "FC 27, my career", "Original game 2006/07"]);
        assert_eq!(rows[0].1, Choice::Version { id: Some("fc27b".into()), desc: "real squads, modern money".into() });
        assert_eq!(rows[1].1, Choice::Version { id: Some("fc27a".into()), desc: String::new() });
        assert_eq!(rows[2].1, Choice::Version { id: None, desc: "as shipped".into() });
        std::fs::remove_dir_all(dir).ok();
    }

    /// Without menu.txt: every mod space newest first, then the original game.
    #[test]
    fn versions_without_a_menu_file() {
        let dir = temp("nomenu");
        for id in ["older", "newer"] {
            std::fs::create_dir_all(dir.join(id)).unwrap();
            std::fs::write(dir.join(id).join("mod.toml"), "name = \"x\"").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let valid = vec![("newer".to_string(), "Newer 2".to_string()), ("older".to_string(), "Older 1".to_string())];
        let rows = versions(&dir, &valid);
        assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["Newer 2", "Older 1", "Original game 2006/07"]);
        assert!(matches!(&rows[0].1, Choice::Version { id: Some(id), .. } if id == "newer"));
        assert_eq!(rows[2].1, Choice::Version { id: None, desc: "the game as it shipped".into() });
        std::fs::remove_dir_all(dir).ok();
    }

    /// The menu: the chosen version ticked with its description under it, the Start rows after
    /// the versions, the window row apart, the careers folder of the version in the footer.
    #[test]
    fn the_text_lists_versions_then_starts() {
        let dir = temp("text");
        let mut l = Launcher {
            versions: vec![("FC 27".into(), Choice::Version { id: Some("fc27".into()), desc: "real squads".into() }), ("Original".into(), Choice::Version { id: None, desc: String::new() })],
            items: Vec::new(),
            sel: 0,
            pending: Some(Pending { xex: String::new(), mod_id: Some("fc27".into()), software: false }),
            note: String::new(),
            fullscreen: false,
            states_root: dir.clone(),
        };
        l.rebuild();
        assert_eq!(l.sel, 2, "the selection starts on the first way to start");
        let t = text(&l, crate::device::InputKind::Unknown);
        assert_eq!(t, "LMA Manager 2007\n\nVersion\n  [x] FC 27\n      real squads\n  [ ] Original\n\nStart\n> New boot\n\n  Window: windowed (change)\n\nCareers and settings of this version: userdata/fc27\nUp / Down move, Enter / A choose");
        assert!(text(&l, crate::device::InputKind::Keyboard).ends_with("Up / Down move, Enter choose"), "the hint follows the device");
        assert!(text(&l, crate::device::InputKind::PlayStation).ends_with("D-pad move, Cross choose"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn long_descriptions_wrap() {
        assert_eq!(wrapped("one two three four", 9, "  "), "  one two\n  three\n  four\n");
        assert_eq!(wrapped("", 9, "  "), "");
    }

    #[test]
    fn mod_spaces_listed_broken_ones_skipped() {
        let dir = temp("mods");
        for (id, toml) in [("b-2026", "name = \"Season 2026\"\nversion = \"1.1\""), ("a-retro", "name = \"Retro\""), ("broken", "base = \"Nope.mng\"")] {
            std::fs::create_dir_all(dir.join(id)).unwrap();
            std::fs::write(dir.join(id).join("mod.toml"), toml).unwrap();
        }
        assert_eq!(mod_spaces(&dir), [("a-retro".to_string(), "Retro".to_string()), ("b-2026".to_string(), "Season 2026 1.1".to_string())]);
        std::fs::remove_dir_all(dir).ok();
    }
}
