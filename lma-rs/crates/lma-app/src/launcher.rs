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
    /// No game set up yet: the launcher shows the first-run install (`install`) instead of the
    /// versions until the player's disc is in place.
    pub needs_game: bool,
}

impl Launcher {
    pub fn needs_game(&self) -> bool {
        self.needs_game && self.pending.is_some()
    }

    /// The first run's install is done: start from `xex`, offer the versions.
    pub fn game_found(&mut self, xex: String) {
        if let Some(p) = self.pending.as_mut() {
            p.xex = xex;
        }
        self.needs_game = false;
        self.note.clear();
        let mods = mod_spaces(Path::new("mods"));
        self.versions = versions(Path::new("mods"), &mods);
        self.rebuild();
    }

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

/// The version id of the original game with this port's conveniences (the maintainer, 22 Oct: "vanilla
/// (completely accurate), vanilla+ and modded"): the shipped game's data, with the extras the
/// mod spaces get too (pad icons that follow the device in use). Plain vanilla (`-`, id None)
/// stays exactly as shipped. Vanilla+ shares the original game's careers and states.
pub const PLUS: &str = "+";

/// Whether a version gets this port's conveniences: everything but the original game as shipped.
pub fn enhanced(version: Option<&str>) -> bool {
    version.is_some()
}

/// A version's mod space, if it has one (vanilla+ plays the original game's data).
pub fn mod_space_of(version: Option<&str>) -> Option<&str> {
    version.filter(|v| *v != PLUS)
}

/// The versions to choose from. With `mods/menu.txt`: its rows, one per line `id|label|description`
/// (`-` for the original game as shipped, `+` for it with this port's conveniences; the
/// description is optional), in that order, without the mod spaces that don't load. Without it:
/// every valid mod space, newest first (by its mod.toml's date), then the original game+, then the
/// original game.
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
            .filter(|(id, _, _)| *id == "-" || *id == PLUS || valid.iter().any(|(m, _)| m == id))
            .map(|(id, label, desc)| row(id, if label.is_empty() { id } else { label }, desc))
            .collect();
    }
    let mut mods: Vec<(std::time::SystemTime, &(String, String))> = valid
        .iter()
        .map(|m| (std::fs::metadata(mods_root.join(&m.0).join("mod.toml")).and_then(|d| d.modified()).unwrap_or(std::time::UNIX_EPOCH), m))
        .collect();
    mods.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1 .0.cmp(&a.1 .0))); // newest first
    let mut out: Vec<(String, Choice)> = mods.into_iter().map(|(_, (id, label))| row(id, label, &format!("mod space {id}"))).collect();
    out.push(row(PLUS, "Original game 2006/07+", "the original game, with controller icons that follow your pad or keyboard"));
    out.push(row("-", "Original game 2006/07", "exactly as it shipped"));
    out
}

/// Written into `states/` by `adopt_old_quick_state` on its first run.
pub const MOVED_MARKER: &str = ".quick-state-per-version";

/// A version's own states folder: `states/<id>/` for a mod space, `states/` for the original game.
pub fn states_dir(states_root: &Path, version: Option<&str>) -> PathBuf {
    mod_space_of(version).map_or(states_root.to_path_buf(), |id| states_root.join(id))
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
                crate::device::ENHANCED.store(enhanced(p.mod_id.as_deref()), std::sync::atomic::Ordering::Relaxed);
                let mod_id = mod_space_of(p.mod_id.as_deref()).map(String::from);
                crate::spawn_game(p.xex, mod_id, p.software, link.clone(), load.map(|p| p.to_string_lossy().into_owned()));
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

/// How a row of the menu is drawn (`rows`): the game-font launcher colours them; the plain text
/// shows them as they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Title,
    Note,
    Header,
    Item,
    Selected,
    Desc,
    Footer,
}

impl Row {
    /// The row's colour in the game-font launcher.
    pub fn colour(self) -> [u8; 3] {
        match self {
            Row::Title => [255, 210, 80],
            Row::Header => [120, 205, 145],
            Row::Selected => [255, 232, 130],
            Row::Desc => [168, 182, 174],
            Row::Footer => [130, 145, 138],
            Row::Note | Row::Item => [236, 238, 236],
        }
    }
}

/// The menu as rows: the versions (the chosen one ticked; a description only under the ticked
/// version and the one the cursor is on, so a long list of versions still fits), the ways to start,
/// the window row, where the chosen version keeps its careers, and the keys.
pub fn rows(l: &Launcher, kind: crate::device::InputKind) -> Vec<(String, Row)> {
    let mut r: Vec<(String, Row)> = vec![("LMA Manager 2007".into(), Row::Title), (String::new(), Row::Item)];
    if !l.note.is_empty() {
        r.extend(l.note.lines().map(|line| (line.to_string(), Row::Note)));
        r.push((String::new(), Row::Item));
    }
    // the first run's install screen is the note alone
    if l.needs_game() {
        while r.last().is_some_and(|(t, _)| t.is_empty()) {
            r.pop();
        }
        return r;
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
            match kind {
                0 => r.push(("Version".into(), Row::Header)),
                1 => {
                    if last.is_some() {
                        r.push((String::new(), Row::Item));
                    }
                    r.push(("Start".into(), Row::Header));
                }
                _ => r.push((String::new(), Row::Item)),
            }
            last = Some(kind);
        }
        let sel = i == l.sel;
        let (prefix, style) = if sel { ("> ", Row::Selected) } else { ("  ", Row::Item) };
        match choice {
            Choice::Version { id, desc } => {
                let ticked = *id == current;
                r.push((format!("{prefix}{} {label}", if ticked { "[x]" } else { "[ ]" }), style));
                if !desc.is_empty() && (ticked || sel) {
                    r.extend(wrapped(desc, 64, "      ").lines().map(|line| (line.to_string(), Row::Desc)));
                }
            }
            Choice::Fullscreen => r.push((format!("{prefix}Window: {} (change)", if l.fullscreen { "fullscreen" } else { "windowed" }), style)),
            Choice::Start(_) => r.push((format!("{prefix}{label}"), style)),
        }
    }
    let saves = mod_space_of(current.as_deref()).map_or("userdata".to_string(), |id| format!("userdata/{id}"));
    r.push((String::new(), Row::Item));
    r.push((format!("Careers and settings of this version: {saves}"), Row::Footer));
    r.push((kind.hint().to_string(), Row::Footer));
    r
}

/// The text of the menu (`rows`, one per line).
pub fn text(l: &Launcher, kind: crate::device::InputKind) -> String {
    rows(l, kind).into_iter().map(|(t, _)| t).collect::<Vec<_>>().join("\n")
}

/// The menu drawn in the game's face: each row in its colour (`Row::colour`), the title twice the
/// size, a soft bar behind the selected row. Returns RGBA, width, height.
pub fn draw_rows(f: &lma_data::font::Font, rows: &[(String, Row)]) -> (Vec<u8>, u32, u32) {
    let parts: Vec<(Vec<u8>, u32, u32, u32, Row)> = rows
        .iter()
        .map(|(t, row)| {
            let (rgba, w, h) = f.render(t, row.colour());
            (rgba, w, h, if *row == Row::Title { 2 } else { 1 }, *row)
        })
        .collect();
    let pad = 6;
    let width = parts.iter().map(|(_, w, _, k, _)| w * k).max().unwrap_or(1) + 2 * pad;
    let height: u32 = parts.iter().map(|(_, _, h, k, _)| h * k).sum::<u32>().max(1);
    let mut out = vec![0u8; (width * height * 4) as usize];
    let mut y0 = 0;
    for (rgba, w, h, k, row) in &parts {
        if *row == Row::Selected {
            for y in y0..y0 + h {
                for x in 0..width {
                    let o = ((y * width + x) * 4) as usize;
                    out[o..o + 4].copy_from_slice(&[255, 220, 110, 46]);
                }
            }
        }
        for y in 0..h * k {
            for x in 0..w * k {
                let i = (((y / k) * w + x / k) * 4) as usize;
                if rgba[i + 3] == 0 {
                    continue;
                }
                let o = (((y0 + y) * width + pad + x) * 4) as usize;
                out[o..o + 4].copy_from_slice(&rgba[i..i + 4]);
            }
        }
        y0 += h * k;
    }
    (out, width, height)
}

/// The launcher drawn in the game's own menu font (#766 c): the image `show` draws the menu into,
/// in place of the plain text, when the player's copy of the game has the font.
#[derive(Component)]
pub struct LauncherImage;

/// The face the launcher is drawn in, from the game's Fonts.mng.
pub const FACE: &str = "swiss_medium_15";

/// Where a game folder keeps its menu fonts.
pub fn fonts_mng(game_dir: &Path) -> PathBuf {
    game_dir.join("Common").join("GameData").join("Textures").join("Fonts.mng")
}

/// The game's face, read once per game folder (None: not there or unreadable, and the plain text
/// stays), and the image of the menu last drawn in it.
#[derive(Resource, Default)]
pub struct Face {
    tried: Option<PathBuf>,
    /// The folder had no Fonts.mng when tried (looked for again: a first-run install may add it).
    missing: bool,
    font: Option<lma_data::font::Font>,
    drawn: String,
    image: Handle<Image>,
}

impl Face {
    /// The face for the game at `xex`, read when the folder changes, or when its fonts were missing
    /// and have appeared since (a first-run install landing them); a bad file is read once.
    fn follow(&mut self, xex: &str) {
        let dir = Path::new(xex).parent().unwrap_or(Path::new(".")).to_path_buf();
        let path = fonts_mng(&dir);
        if self.tried.as_ref() == Some(&dir) && !(self.missing && path.is_file()) {
            return;
        }
        self.tried = Some(dir);
        self.missing = !path.is_file();
        self.font = if self.missing {
            None
        } else {
            std::fs::read(&path).map_err(|e| e.to_string()).and_then(|d| lma_data::font::Font::from_fonts_mng(&d, FACE).map_err(|e| e.to_string())).inspect_err(|e| warn!("launcher font: {}: {e}", path.display())).ok()
        };
        self.drawn.clear();
    }
}

/// The scale the menu image is shown at: the largest whole number that fits a `w` x `h` window
/// with a margin (crisp), or, when even 1x doesn't fit (a long list of versions in a small window),
/// the fraction that does (the maintainer, 26 Oct: the menu ran past the window).
pub fn image_scale(iw: u32, ih: u32, w: f32, h: f32) -> f32 {
    let fit = ((w * 0.9) / iw.max(1) as f32).min((h * 0.9) / ih.max(1) as f32);
    if fit >= 1.0 {
        fit.floor()
    } else {
        fit.max(0.05)
    }
}

#[allow(clippy::type_complexity)]
pub fn show(
    l: Res<Launcher>,
    kind: Res<crate::device::InputKind>,
    mut face: ResMut<Face>,
    mut images: ResMut<Assets<Image>>,
    mut text_node: Query<(&mut Text, &mut TextFont, &mut Node, &mut Visibility), (With<LauncherText>, Without<LauncherImage>)>,
    mut image_node: Query<(&mut ImageNode, &mut Node, &mut Visibility), (With<LauncherImage>, Without<LauncherText>)>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
) {
    let Ok((mut t, mut font, mut node, mut vis)) = text_node.single_mut() else { return };
    let mut img = image_node.single_mut().ok();
    let Some(p) = l.pending.as_ref() else {
        *vis = Visibility::Hidden;
        if let Some((_, _, v)) = img.as_mut() {
            **v = Visibility::Hidden;
        }
        return;
    };
    let menu = rows(&l, *kind);
    let s = text(&l, *kind);
    let face = &mut *face;
    face.follow(&p.xex);
    // the game's face when the player's copy has it (and the image node exists), else the text
    if let (Some(f), Some((inode, inode_node, ivis))) = (face.font.as_ref(), img.as_mut()) {
        *vis = Visibility::Hidden;
        **ivis = Visibility::Visible;
        if face.drawn != s {
            let (rgba, w, h) = draw_rows(f, &menu);
            use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
            let image = Image::new(Extent3d { width: w, height: h, depth_or_array_layers: 1 }, TextureDimension::D2, rgba, TextureFormat::Rgba8UnormSrgb, bevy::asset::RenderAssetUsages::default());
            face.image = images.add(image);
            inode.image = face.image.clone();
            face.drawn = s;
        }
        if let (Ok(win), Some(size)) = (windows.single(), images.get(&face.image).map(|i| i.size())) {
            let k = image_scale(size.x, size.y, win.width(), win.height());
            // the node's size includes its padding (the box around the menu, as the text has)
            let pad = 12.0 * k.max(1.0);
            let (w, h) = (Val::Px(size.x as f32 * k + 2.0 * pad), Val::Px(size.y as f32 * k + 2.0 * pad));
            if inode_node.width != w || inode_node.height != h {
                inode_node.width = w;
                inode_node.height = h;
                inode_node.padding = UiRect::all(Val::Px(pad));
            }
        }
        return;
    }
    if let Some((_, _, v)) = img.as_mut() {
        **v = Visibility::Hidden;
    }
    *vis = Visibility::Visible;
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

    /// The menu image is shown at the largest whole scale that fits the window, or scaled down to
    /// fit when it's bigger than the window.
    /// Over many menu and window sizes the scaled menu always fits in 90% of the window (the
    /// overflow the maintainer saw on 26 Oct), whole steps when it fits at 1x or more; a tiny window stops at
    /// the 0.05 floor.
    #[test]
    fn the_scaled_menu_always_fits_the_window() {
        for iw in [200u32, 640, 900, 1300, 2000] {
            for ih in [150u32, 480, 800, 1100] {
                for (w, h) in [(640.0f32, 360.0f32), (1280.0, 720.0), (1440.0, 900.0), (1920.0, 1080.0), (3024.0, 1964.0)] {
                    let k = image_scale(iw, ih, w, h);
                    assert!(iw as f32 * k <= 0.9 * w + 0.01 && ih as f32 * k <= 0.9 * h + 0.01, "{iw}x{ih} in {w}x{h}: x{k}");
                    if k >= 1.0 {
                        assert_eq!(k, k.floor(), "{iw}x{ih} in {w}x{h}: whole steps at 1x or more");
                    }
                }
            }
        }
        assert_eq!(image_scale(1000, 600, 1280.0, 720.0), 1.0, "1.08 fits: floored to 1x, not blurred up");
        assert_eq!(image_scale(4000, 4000, 100.0, 100.0), 0.05, "never below the floor");
    }

    #[test]
    fn the_image_scales_by_whole_steps() {
        assert_eq!(image_scale(400, 300, 1280.0, 720.0), 2.0);
        assert_eq!(image_scale(400, 300, 2560.0, 1440.0), 4.0);
        assert!((image_scale(900, 800, 1280.0, 720.0) - 0.81).abs() < 1e-3, "too big for the window: scaled down to fit");
        assert_eq!(image_scale(0, 0, 1280.0, 720.0), 648.0, "an empty image doesn't divide by zero");
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
        assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["Newer 2", "Older 1", "Original game 2006/07+", "Original game 2006/07"]);
        assert!(matches!(&rows[0].1, Choice::Version { id: Some(id), .. } if id == "newer"));
        assert!(matches!(&rows[2].1, Choice::Version { id: Some(id), .. } if id == PLUS));
        assert_eq!(rows[3].1, Choice::Version { id: None, desc: "exactly as it shipped".into() });
        std::fs::remove_dir_all(dir).ok();
    }

    /// Vanilla, vanilla+ and modded (the maintainer, 22 Oct): vanilla+ plays the original game's data and
    /// careers with the conveniences on; only vanilla is exactly as shipped.
    #[test]
    fn vanilla_plus_is_the_original_game_with_conveniences() {
        assert_eq!((enhanced(None), enhanced(Some(PLUS)), enhanced(Some("fc27"))), (false, true, true));
        assert_eq!((mod_space_of(None), mod_space_of(Some(PLUS)), mod_space_of(Some("fc27"))), (None, None, Some("fc27")));
        let root = Path::new("states");
        assert_eq!(states_dir(root, Some(PLUS)), states_dir(root, None), "vanilla+ shares the original game's states");
        let dir = temp("plusmenu");
        std::fs::write(dir.join("menu.txt"), "+|Original+|\n-|Original|\nnope|Missing|\n").unwrap();
        let ids: Vec<Option<String>> = versions(&dir, &[]).into_iter().map(|r| match r.1 {
            Choice::Version { id, .. } => id,
            _ => unreachable!(),
        }).collect();
        assert_eq!(ids, [Some(PLUS.to_string()), None], "menu.txt may list + and -; a mod space that doesn't load is left out");
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
            needs_game: false,
        };
        l.rebuild();
        assert_eq!(l.sel, 2, "the selection starts on the first way to start");
        let t = text(&l, crate::device::InputKind::Unknown);
        assert_eq!(t, "LMA Manager 2007\n\nVersion\n  [x] FC 27\n      real squads\n  [ ] Original\n\nStart\n> New boot\n\n  Window: windowed (change)\n\nCareers and settings of this version: userdata/fc27\nUp / Down move, Enter / A choose");
        assert!(text(&l, crate::device::InputKind::Keyboard).ends_with("Up / Down move, Enter choose"), "the hint follows the device");
        assert!(text(&l, crate::device::InputKind::PlayStation).ends_with("D-pad move, Cross choose"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// With many versions only the ticked one and the one under the cursor show their description,
    /// so the menu stays short (the maintainer, 26 Oct: it ran past the window).
    #[test]
    fn descriptions_only_for_the_ticked_and_the_selected_version() {
        let dir = temp("rows");
        let v = |n: &str, id: Option<&str>| (n.to_string(), Choice::Version { id: id.map(String::from), desc: format!("about {n}") });
        let mut l = Launcher {
            versions: vec![v("A", Some("a")), v("B", Some("b")), v("C", Some("c")), v("Original", None)],
            items: Vec::new(),
            sel: 0,
            pending: Some(Pending { xex: String::new(), mod_id: Some("a".into()), software: false }),
            note: String::new(),
            fullscreen: false,
            states_root: dir.clone(),
            needs_game: false,
        };
        l.rebuild();
        l.sel = 2; // the cursor on version C
        let r = rows(&l, crate::device::InputKind::Keyboard);
        let descs: Vec<&str> = r.iter().filter(|(_, row)| *row == Row::Desc).map(|(t, _)| t.trim()).collect();
        assert_eq!(descs, ["about A", "about C"], "the ticked A and the selected C only");
        assert!(r.iter().any(|(t, row)| *row == Row::Selected && t == "> [ ] C"));
        assert_eq!(r[0], ("LMA Manager 2007".to_string(), Row::Title));
        std::fs::remove_dir_all(dir).ok();
    }

    /// A look at the menu in the game's face (the player's own Fonts.mng, LMA_GAME_DIR; LMA_MENU_DIR
    /// is a folder with mods/menu.txt): writes LMA_MENU_PNG. Run with --ignored.
    #[test]
    #[ignore]
    fn draw_the_menu_to_a_png() {
        let (Some(game), Some(out)) = (std::env::var_os("LMA_GAME_DIR"), std::env::var_os("LMA_MENU_PNG")) else { return };
        let menu_dir = std::env::var_os("LMA_MENU_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let f = lma_data::font::Font::from_fonts_mng(&std::fs::read(fonts_mng(Path::new(&game))).unwrap(), FACE).unwrap();
        let mods = menu_dir.join("mods");
        let mut l = Launcher {
            versions: versions(&mods, &mod_spaces(&mods)),
            items: Vec::new(),
            sel: 0,
            pending: Some(Pending { xex: String::new(), mod_id: None, software: false }),
            note: String::new(),
            fullscreen: false,
            states_root: temp("png"),
            needs_game: false,
        };
        l.pending.as_mut().unwrap().mod_id = match &l.versions[0].1 {
            Choice::Version { id, .. } => id.clone(),
            _ => None,
        };
        l.rebuild();
        let (rgba, w, h) = draw_rows(&f, &rows(&l, crate::device::InputKind::Keyboard));
        let mut png = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(out).unwrap()), w, h);
        png.set_color(png::ColorType::Rgba);
        png.set_depth(png::BitDepth::Eight);
        png.write_header().unwrap().write_image_data(&rgba).unwrap();
    }

    /// A tiny font like the game's (lma-data's font fixtures): 'A' 4x6 and 'B' 3x6, line height 8,
    /// every sheet pixel opaque.
    fn tiny_font() -> lma_data::font::Font {
        let (w, h) = (8u16, 6u16);
        let mut tm2 = b"TIM2".to_vec();
        tm2.extend_from_slice(&[4, 0, 1, 0]);
        tm2.resize(0x10, 0);
        let image = w as usize * h as usize;
        tm2.extend_from_slice(&((0x30 + image + 1024) as u32).to_le_bytes());
        tm2.extend_from_slice(&1024u32.to_le_bytes());
        tm2.extend_from_slice(&(image as u32).to_le_bytes());
        tm2.extend_from_slice(&0x30u16.to_le_bytes());
        tm2.extend_from_slice(&256u16.to_le_bytes());
        tm2.extend_from_slice(&[0, 1, 3, 5]);
        tm2.extend_from_slice(&w.to_le_bytes());
        tm2.extend_from_slice(&h.to_le_bytes());
        tm2.resize(0x40, 0);
        tm2.extend(std::iter::repeat_n(1u8, image));
        let mut clut = vec![0u8; 1024];
        for i in 0..256usize {
            let at = i / 32 * 32 + [0, 2, 1, 3][i % 32 / 8] * 8 + i % 8;
            clut[4 * at..4 * at + 4].copy_from_slice(&[255, 255, 255, if i > 0 { 128 } else { 0 }]);
        }
        tm2.extend(clut);
        let mut pcf = Vec::new();
        for v in [3u32, 2, 8, 8, 2, 0] {
            pcf.extend_from_slice(&v.to_le_bytes());
        }
        let mut map = [0xFFFFu16; 256];
        map[b'A' as usize] = 0;
        map[b'B' as usize] = 1;
        for m in map {
            pcf.extend_from_slice(&m.to_le_bytes());
        }
        for (x, y, gw, gh, c) in [(0u16, 0u16, 4u16, 6u16, b'A' as u16), (4, 0, 3, 6, b'B' as u16)] {
            for v in [x, y, gw, gh, c, 0xCD00] {
                pcf.extend_from_slice(&v.to_le_bytes());
            }
            pcf.extend_from_slice(&[0xF8, 7, 0, 0]);
        }
        lma_data::font::Font::parse("tiny", &pcf, &tm2).unwrap()
    }

    /// The drawn menu: the title twice the size, a soft bar across the selected row, each row in
    /// its colour.
    #[test]
    fn rows_draw_with_a_big_title_and_a_bar_behind_the_selection() {
        let f = tiny_font();
        let rows = vec![("A".to_string(), Row::Title), ("B".to_string(), Row::Item), ("AB".to_string(), Row::Selected)];
        let (rgba, w, h) = draw_rows(&f, &rows);
        let lh = f.line_height as u32;
        assert_eq!(h, 2 * lh + lh + lh, "the title's row is twice as tall");
        let px = |x: u32, y: u32| &rgba[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize];
        // the selected row: the bar right across, where no glyph is
        assert_eq!(px(w - 1, 3 * lh + 1), &[255, 220, 110, 46]);
        assert_eq!(px(w - 1, 2 * lh + 1)[3], 0, "no bar behind an ordinary row");
        // the title's glyph doubled: a pixel well inside 'A' at 2x is drawn in the title's colour
        let [r, g, b] = Row::Title.colour();
        assert_eq!(px(6 + 3, 3), &[r, g, b, 255]);
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
