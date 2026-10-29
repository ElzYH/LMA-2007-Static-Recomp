//! LMA Manager 2007 native port: Bevy front end.
//!
//!   lma-app [path/to/default.xex] [--mod <name>] [--software]   (or LMA_MOD=<name>; see runtime::mods)
//!
//! The recompiled/interpreted game runs on a background thread; every presented frame is shown
//! here, and keyboard, mouse and gamepads (Xbox, PlayStation, generic) drive the game's
//! controller port 1. Runs on Windows, macOS (Intel and Apple Silicon) and Linux.
//!
//! F5 saves a state to `states/quick.state` (a mod space: `states/<id>/quick.state`), F9 loads it,
//! F1 toggles turbo (no rendering).
//! Ctrl/Cmd + S, L and T do the same, for keyboards (Macs) whose F keys are media keys.
//! The intro runs in turbo until LMA_TURBO_UNTIL million instructions (default 4700, just
//! before the title screen; 0 disables). LMA_LOAD=<state> starts from a save state.
//! LMA_INPUT and LMA_INPUT_LIVE=1 script the controller from the environment or stdin, with
//! save/load/screenshot/report commands (see `script.rs`). LMA_RECORD=<path> records your own presses
//! for replay (`LMA_LOAD=<path>.state LMA_INPUT=@<path>`).
//!
//! Pixels are drawn on the GPU (wgpu: Vulkan, Metal or DX12). `--software` or LMA_BACKEND=software
//! uses the CPU rasteriser instead, which is also the automatic fallback when no GPU adapter is found.

mod audio_out;
mod blackbox;
mod bundle;
mod config;
mod device;
mod install;
mod keys;
mod launcher;
mod menu_sound;
mod power;
mod prepare;
mod script;

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use runtime::input::{button, Pad};
use std::sync::{Arc, Mutex};

/// LMA_SHARED_GPU=1: the device the window (bevy) and the game's renderer share (#6).
static SHARED_GPU: std::sync::OnceLock<runtime::gpu_wgpu::GpuContext> = std::sync::OnceLock::new();
/// The screen image, for `copy_gpu_frame` in the render world.
static SCREEN_ID: std::sync::OnceLock<AssetId<Image>> = std::sync::OnceLock::new();

/// LMA_SHARED_GPU=1, render world: the newest frame the renderer composed on the shared device
/// (`runtime::gpu_wgpu::GPU_FRAME`) copied into the screen texture, GPU to GPU: no read-back, no
/// upload (#6). The copy is queued after the compose on the one queue.
fn copy_gpu_frame(images: Res<bevy::render::render_asset::RenderAssets<bevy::render::texture::GpuImage>>, device: Res<bevy::render::renderer::RenderDevice>, queue: Res<bevy::render::renderer::RenderQueue>) {
    let Some(id) = SCREEN_ID.get() else { return };
    let Some((buf, w, h)) = runtime::gpu_wgpu::GPU_FRAME.lock().unwrap().take() else { return };
    if let Some(img) = images.get(*id) {
        let size = img.texture_descriptor.size;
        if (size.width, size.height) == (w, h) {
            use bevy::render::render_resource::{TexelCopyBufferInfo, TexelCopyBufferLayout};
            let mut enc = device.wgpu_device().create_command_encoder(&Default::default());
            enc.copy_buffer_to_texture(
                TexelCopyBufferInfo { buffer: &buf, layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) } },
                img.texture.as_image_copy(),
                Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            queue.submit([enc.finish()]);
        }
    }
    runtime::gpu_wgpu::give_gpu_frame_buf(buf);
    static SHOWN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n == 1 || n % 600 == 0 {
        info!("LMA_SHARED_GPU: {n} frames shown from the GPU (no read-back)");
    }
}

/// LMA_WINDOW_SHOT=<png> (checks without a person, #700): the window as drawn, saved once
/// LMA_WINDOW_SHOT_AT seconds in (default 20), through bevy's screenshot (no OS capture).
fn window_shot(mut commands: Commands, time: Res<Time>, mut taken: Local<bool>) {
    static WANTED: std::sync::OnceLock<Option<(std::path::PathBuf, f32)>> = std::sync::OnceLock::new();
    let Some((path, at)) = WANTED.get_or_init(|| std::env::var_os("LMA_WINDOW_SHOT").map(|p| (p.into(), std::env::var("LMA_WINDOW_SHOT_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(20.0)))) else { return };
    if *taken || time.elapsed_secs() < *at {
        return;
    }
    *taken = true;
    use bevy::render::view::screenshot::{save_to_disk, Screenshot};
    commands.spawn(Screenshot::primary_window()).observe(save_to_disk(path.clone()));
}

const W: u32 = 1280;
const H: u32 = 720;

#[derive(Default)]
struct Shared {
    frame: Option<(u64, u32, u32, Vec<u8>)>,
    pad: Pad,
    /// Controller ports 2-4: the second, third and fourth gamepad (local multiplayer, #206).
    /// `None` while that gamepad isn't connected.
    others: [Option<Pad>; 3],
    /// Buttons pressed since the game thread last read the pad, so short taps are never lost.
    latched: u16,
    status: String,
    /// Requests from the window to the game thread.
    save: bool,
    load: bool,
    turbo_toggle: bool,
    /// F3 / Ctrl+F: next frame cap.
    cap_cycle: bool,
    /// F12 / Ctrl+B: a bug report (the maintainer, 22 Oct): state, screenshot and report.txt in reports/.
    report: bool,
    /// The settings menu (F2): a frame cap to switch to, and the cap in force (for the menu).
    cap_set: Option<u32>,
    cap: u32,
    /// F2's Battery saver, changed.
    battery_set: Option<config::BatterySaver>,
}

/// The app icon: LMA's own (the maintainer, 26 Oct: "the icon from LMA, don't generate one"), the title
/// icon in the player's default.xex (`xex::Xex::title_icon_png`), read at run time so nothing of
/// the game ships; none of our own (the maintainer, 28 Oct: "remove the AI icon, just use the game file
/// one"), so before a game is set up the system's default. Set on the window
/// once winit has created it (Windows and Linux: title bar, taskbar) and, on macOS, as the Dock
/// icon. Bevy has no icon setting; winit's windows live in a main-thread thread-local, hence the
/// NonSendMarker.
fn set_window_icon(windows: Query<Entity, With<bevy::window::PrimaryWindow>>, mut done: Local<bool>, _main_thread: bevy::ecs::system::NonSendMarker) {
    if *done {
        return;
    }
    let Ok(entity) = windows.single() else { return };
    *done = true;
    let Some(game) = game_xex().and_then(|p| std::fs::read(p).ok()).and_then(|d| xex::Xex::parse(&d).ok()).and_then(|x| x.title_icon_png().map(<[u8]>::to_vec)) else { return };
    let png = game.as_slice();
    #[cfg(target_os = "macos")]
    set_dock_icon(png);
    let Some((rgba, w, h)) = png_rgba(png) else {
        warn!("window icon: can't decode it");
        return;
    };
    bevy::winit::WINIT_WINDOWS.with_borrow(|ws| {
        let Some(window) = ws.get_window(entity) else { return };
        match winit::window::Icon::from_rgba(rgba, w, h) {
            Ok(icon) => window.set_window_icon(Some(icon)),
            Err(e) => warn!("window icon: {e}"),
        }
    });
}

/// The game's default.xex: a `.xex` given as the argument, else the configured game folder's.
fn game_xex() -> Option<std::path::PathBuf> {
    if let Some(a) = std::env::args().skip(1).find(|a| a.to_ascii_lowercase().ends_with(".xex")) {
        return Some(a.into());
    }
    config::Config::load().game_dir.map(|d| d.join("default.xex")).filter(|p| p.is_file())
}

/// A PNG as 8-bit RGBA (RGB, grey and palette images expanded; an opaque alpha added).
fn png_rgba(png: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((rgba, w, h))
}

/// macOS: the Dock icon from PNG bytes (the bundle's own icon shows until this runs).
#[cfg(target_os = "macos")]
fn set_dock_icon(png: &[u8]) {
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};
    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(png);
    let Some(image) = NSImage::initWithData(mtm.alloc(), &data) else { return };
    // SAFETY: on the main thread (the marker), with a valid image
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}

/// The window can't be seen (minimised, fully covered, on another Space): the game keeps running
/// and sounding but draws nothing (the maintainer, 26 Oct, battery: drawing is ~65% of a match's CPU on the
/// M4, 1.76 -> 0.62 cores; a menu 0.37 -> 0.27). Set from winit's occlusion events.
static WINDOW_HIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Also slows Bevy's own loop while hidden: nothing is shown, and without vsync pacing a hidden
/// window's continuous loop runs flat out. Four updates a second still see it come back.
fn follow_occlusion(mut occluded: MessageReader<bevy::window::WindowOccluded>, mut winit: ResMut<bevy::winit::WinitSettings>) {
    for e in occluded.read() {
        WINDOW_HIDDEN.store(e.occluded, std::sync::atomic::Ordering::Relaxed);
        *winit = if e.occluded {
            let slow = bevy::winit::UpdateMode::reactive_low_power(std::time::Duration::from_millis(250));
            bevy::winit::WinitSettings { focused_mode: slow, unfocused_mode: slow }
        } else {
            bevy::winit::WinitSettings::game()
        };
    }
}

/// The in-window settings menu (F2): which item is selected, and the player's settings it changes
/// (saved in the config as they change, #766).
#[derive(Resource)]
struct Menu {
    open: bool,
    sel: usize,
    status_line: bool,
    vsync: bool,
    fullscreen: bool,
    /// The windowed size, as an index into `config::WINDOW_SIZES`.
    size: usize,
    /// The game's sound level, 0-100, in steps of 10.
    volume: u8,
    coop: Coop,
    battery: config::BatterySaver,
    /// Render scale for the next start (1 = 1280x720); this run keeps the one it started with.
    scale: u32,
    /// Just closed: the Enter/Esc that closed it must not reach the game as A/B (`read_input`)
    /// while it is still held.
    just_closed: bool,
}

impl Default for Menu {
    fn default() -> Self {
        Menu { open: false, sel: 0, status_line: false, vsync: true, fullscreen: false, size: 0, volume: 100, coop: Coop::Off, battery: config::BatterySaver::OnBattery, scale: 1, just_closed: false }
    }
}

/// The F2 menu's rows, in order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuItem {
    FrameCap,
    Battery,
    VSync,
    Window,
    WindowSize,
    RenderScale,
    Volume,
    Coop,
    StatusLine,
    Close,
}

const MENU_ITEMS: [MenuItem; 10] = [MenuItem::FrameCap, MenuItem::Battery, MenuItem::VSync, MenuItem::Window, MenuItem::WindowSize, MenuItem::RenderScale, MenuItem::Volume, MenuItem::Coop, MenuItem::StatusLine, MenuItem::Close];

/// Co-op controls (the maintainer, 22 Oct: "play co-op with one controller, or keyboard and controller").
/// The game's two-player career takes turns (each manager plays his own days), so one pad can be
/// passed between the players.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Coop {
    /// The keyboard, mouse and first pad are player 1; more pads are players 2-4 (as before).
    #[default]
    Off,
    /// The keyboard and mouse are player 1, the first pad player 2, more pads players 3-4.
    KeyboardAndPad,
    /// Player 2 is always connected; the keyboard and first pad drive whichever player's turn it
    /// is (`Turn`), switched with BACK + START on the pad or F4.
    SharedPad,
}

impl Coop {
    const ALL: [Coop; 3] = [Coop::Off, Coop::KeyboardAndPad, Coop::SharedPad];

    fn from_config(v: Option<&str>) -> Self {
        match v {
            Some("keyboard_pad") => Coop::KeyboardAndPad,
            Some("shared_pad") => Coop::SharedPad,
            _ => Coop::Off,
        }
    }

    fn config(self) -> Option<String> {
        match self {
            Coop::Off => None,
            Coop::KeyboardAndPad => Some("keyboard_pad".into()),
            Coop::SharedPad => Some("shared_pad".into()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Coop::Off => "off",
            Coop::KeyboardAndPad => "keyboard P1, pad P2",
            Coop::SharedPad => "one pad, passed",
        }
    }
}

/// Whose turn the shared pad is on (`Coop::SharedPad`): false player 1, true player 2.
#[derive(Resource, Default)]
struct Turn(bool);

/// Port 1's pad and ports 2-4's (None: not connected) from the keyboard + mouse (`keyboard`)
/// and each gamepad in order (`pads`), for the co-op mode and whose turn it is.
fn route(coop: Coop, keyboard: Pad, pads: &[Pad], player2: bool) -> (Pad, [Option<Pad>; 3]) {
    let merge = |a: Pad, b: Pad| Pad {
        buttons: a.buttons | b.buttons,
        left_trigger: a.left_trigger.max(b.left_trigger),
        right_trigger: a.right_trigger.max(b.right_trigger),
        thumb_lx: a.thumb_lx.saturating_add(b.thumb_lx),
        thumb_ly: a.thumb_ly.saturating_add(b.thumb_ly),
        thumb_rx: a.thumb_rx.saturating_add(b.thumb_rx),
        thumb_ry: a.thumb_ry.saturating_add(b.thumb_ry),
    };
    let pad = |i: usize| pads.get(i).copied();
    match coop {
        Coop::Off => (pad(0).map_or(keyboard, |p| merge(keyboard, p)), [pad(1), pad(2), pad(3)]),
        // player 2 stays connected with no pad yet, so the game keeps its second manager
        Coop::KeyboardAndPad => (keyboard, [Some(pad(0).unwrap_or_default()), pad(1), pad(2)]),
        Coop::SharedPad => {
            let both = pad(0).map_or(keyboard, |p| merge(keyboard, p));
            let (p1, p2) = if player2 { (Pad::default(), both) } else { (both, Pad::default()) };
            (p1, [Some(p2), pad(1), pad(2)])
        }
    }
}

impl Menu {
    /// The menu as the saved settings leave it; the volume is applied to the output at once.
    fn from_config(cfg: &config::Config) -> Self {
        let size = config::WINDOW_SIZES.iter().position(|&s| s == cfg.window_size()).unwrap_or(0);
        audio_out::VOLUME.store(cfg.volume() as u32, std::sync::atomic::Ordering::Relaxed);
        Menu { status_line: cfg.status_line_shown(), vsync: cfg.vsync(), fullscreen: cfg.fullscreen, size, volume: cfg.volume(), coop: Coop::from_config(cfg.coop.as_deref()), battery: cfg.battery_saver(), scale: cfg.render_scale(), ..Menu::default() }
    }

    /// Change `item` by `step` (-1 / +1; Enter is +1). Returns whether a setting changed.
    fn change(&mut self, item: MenuItem, step: i32) -> bool {
        match item {
            MenuItem::Battery => {
                let all = config::BatterySaver::ALL;
                let i = all.iter().position(|&b| b == self.battery).unwrap_or(1) as i32;
                self.battery = all[(i + step).rem_euclid(all.len() as i32) as usize];
            }
            MenuItem::VSync => self.vsync = !self.vsync,
            MenuItem::Window => self.fullscreen = !self.fullscreen,
            MenuItem::WindowSize => self.size = (self.size as i32 + step).rem_euclid(config::WINDOW_SIZES.len() as i32) as usize,
            MenuItem::RenderScale => self.scale = (self.scale as i32 - 1 + step).rem_euclid(config::RENDER_SCALE_MAX as i32) as u32 + 1,
            MenuItem::Volume => self.volume = (self.volume as i32 + 10 * step).clamp(0, 100) as u8,
            MenuItem::Coop => {
                let i = Coop::ALL.iter().position(|c| *c == self.coop).unwrap_or(0) as i32;
                self.coop = Coop::ALL[(i + step).rem_euclid(Coop::ALL.len() as i32) as usize];
            }
            MenuItem::StatusLine => self.status_line = !self.status_line,
            MenuItem::FrameCap | MenuItem::Close => return false,
        }
        true
    }

    /// The settings into the config file (the rest of it as it was).
    fn save(&self) {
        let mut c = config::Config::load();
        c.status_line = self.status_line;
        c.vsync = Some(self.vsync);
        c.fullscreen = self.fullscreen;
        c.window_size = Some(config::WINDOW_SIZES[self.size]);
        c.volume = Some(self.volume);
        c.coop = self.coop.config();
        c.battery_saver = Some(self.battery);
        c.render_scale = Some(self.scale);
        if let Err(e) = c.save() {
            warn!("could not save the settings: {e}");
        }
    }

    /// The menu's text.
    fn text(&self, cap: u32) -> String {
        let mark = |item: MenuItem| if MENU_ITEMS[self.sel] == item { "> " } else { "  " };
        let onoff = |b: bool| if b { "on" } else { "off" };
        let [w, h] = config::WINDOW_SIZES[self.size];
        format!(
            "Settings (F2)\n\n{}Frame cap      < {} >\n{}Battery saver  < {} >\n{}VSync          < {} >\n{}Window         < {} >\n{}Window size    < {w} x {h} >\n{}Render scale   < {} >{}\n{}Volume         < {}% >\n{}Co-op          < {} >\n{}Status line    < {} >\n{}Close\n\nUp/Down select, Left/Right change, Enter/Esc close",
            mark(MenuItem::FrameCap),
            cap_label(cap),
            mark(MenuItem::Battery),
            self.battery.label(),
            mark(MenuItem::VSync),
            onoff(self.vsync),
            mark(MenuItem::Window),
            if self.fullscreen { "fullscreen" } else { "windowed" },
            mark(MenuItem::WindowSize),
            mark(MenuItem::RenderScale),
            format!("{}x ({}p)", self.scale, 720 * self.scale),
            if self.scale == runtime::gpu_wgpu::res_scale() { "" } else { "  (on next start)" },
            mark(MenuItem::Volume),
            self.volume,
            mark(MenuItem::Coop),
            self.coop.label(),
            mark(MenuItem::StatusLine),
            onoff(self.status_line),
            mark(MenuItem::Close),
        )
    }
}

/// The swap chain's present mode for the VSync setting: on waits for the display's refresh (no
/// tearing), off shows each frame at once (falling back to vsync where the device can't).
fn present_mode(vsync: bool) -> bevy::window::PresentMode {
    if vsync { bevy::window::PresentMode::AutoVsync } else { bevy::window::PresentMode::AutoNoVsync }
}

#[derive(Component)]
struct MenuText;

/// Frame caps the player can choose (F3 cycles them): presentation only, game timing never
/// changes. The guest makes at most 60 frames a second, so 90 and 120 show 60 until frame
/// interpolation lands (design on #6 first).
const CAPS: [u32; 4] = [30, 60, 90, 120];

fn settings_path(userdata: &std::path::Path) -> std::path::PathBuf {
    userdata.join("lma-app-settings.txt")
}

/// The frame cap: LMA_FPS_CAP, else the saved setting, else 60.
fn load_cap(userdata: &std::path::Path) -> u32 {
    let saved = std::fs::read_to_string(settings_path(userdata))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("fps_cap=").and_then(|v| v.trim().parse().ok())));
    let cap = std::env::var("LMA_FPS_CAP").ok().and_then(|v| v.parse().ok()).or(saved).unwrap_or(60);
    if CAPS.contains(&cap) { cap } else { 60 }
}

fn save_cap(userdata: &std::path::Path, cap: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(userdata)?;
    std::fs::write(settings_path(userdata), format!("fps_cap={cap}
"))
}

fn cap_label(cap: u32) -> String {
    if cap > 60 { format!("cap {cap} (60 until interpolation)") } else { format!("cap {cap}") }
}

#[derive(Resource, Clone)]
struct Link(Arc<Mutex<Shared>>);

/// The keyboard's controller map (defaults plus the config's `[keys]`).
#[derive(Resource)]
struct KeyMap(Vec<(KeyCode, u16)>);

#[derive(Resource)]
struct Screen(Handle<Image>);

#[derive(Component)]
struct StatusText;

/// What PLAY.bat used to set (the maintainer, 22 Oct: the game starts from the exe alone, no .bat): the
/// folder beside the exe when it holds the release's `mods/` and no argument was given (a shortcut
/// or another folder's prompt still finds mods, saves and states; a relative path argument keeps
/// meaning what it did), up to 4 game threads (one core left for the rest on a small laptop),
/// resolve reuse off. A variable already set wins, so scripts and the settings keep control.
fn release_defaults() {
    // inside LMA.app (macOS): the config folder, with a link to the bundle's mods (bundle.rs)
    if !bundle::enter() {
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf())) {
            if std::env::args_os().len() == 1 && dir.join("mods").is_dir() {
                let _ = std::env::set_current_dir(&dir);
            }
        }
    }
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1)).clamp(1, 4).to_string();
    for (k, v) in [("LMA_THREADS", threads.as_str()), ("LMA_RESOLVE_REUSE", "0")] {
        if std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
        }
    }
}

fn main() {
    release_defaults();
    install::use_audio_cache();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mod_id = runtime::mods::selected(&mut args);
    let software = take_flag(&mut args, "--software") || std::env::var("LMA_BACKEND").as_deref() == Ok("software");
    // the game folder: an argument (remembered for next time), else the remembered one (#4)
    let show_launcher = launcher::wanted(!args.is_empty());
    // the downloaded app (no game code in it): once the game is built for this computer, every
    // start goes straight to it (prepare.rs)
    if prepare::is_shell() {
        if let Some(exe) = prepare::built_game() {
            let e = prepare::handover(&exe);
            eprintln!("lma-app: couldn't start the built game {}: {e}", exe.display());
        }
    }
    let cfg = config::Config::load();
    // F2's render scale, read once by the backend (`runtime::gpu_wgpu::res_scale`) before any
    // surface exists; set before other threads start
    if std::env::var_os("LMA_RES_SCALE").is_none() && cfg.render_scale() > 1 {
        unsafe { std::env::set_var("LMA_RES_SCALE", cfg.render_scale().to_string()) };
    }
    let (map, ignored) = keys::map(&cfg.keys);
    for i in &ignored {
        eprintln!("lma: [keys] in the config: {i} (ignored)");
    }
    let key_map = KeyMap(map);
    let (xex_path, changed) = config::resolve_xex(args.first().map(|s| s.as_str()), &cfg, "../lab/LMA_full/default.xex");
    if let Some(c) = changed {
        if let Err(e) = c.save() {
            eprintln!("lma-app: couldn't remember the game folder: {e}");
        }
    }
    let xex_path = xex_path.to_string_lossy().into_owned();
    let link = Link(Arc::new(Mutex::new(Shared::default())));
    // the quick save of builds before versions had their own folders belongs to the version played
    // last, not to the original game (#673): moved there once
    if let Some(note) = launcher::adopt_old_quick_state(std::path::Path::new("states"), launcher::mod_space_of(cfg.mod_space.as_deref())) {
        eprintln!("lma-app: {note}");
    }
    let prepare_state = prepare::Prepare::default();
    // the launcher picks how to start (continue, boot, a save state); scripts start at once
    let launcher = if show_launcher {
        let installed = std::path::Path::new(&xex_path).exists();
        // the game is installed but this downloaded app hasn't built it yet: carry on with that
        // (the launcher shows its progress instead of the versions)
        if installed && prepare::is_shell() {
            prepare::start(&prepare_state, std::path::PathBuf::from(&xex_path));
        }
        let needs_game = !installed || prepare::is_shell();
        // the first run: the launcher asks for the player's disc (install.rs)
        let note = if needs_game { install::note(&install::State::Idle) } else { String::new() };
        let mods = launcher::mod_spaces(std::path::Path::new("mods"));
        // the last version chosen, unless --mod / LMA_MOD says otherwise (and it still exists)
        let mod_id = mod_id.or(cfg.mod_space.clone()).filter(|id| id == launcher::PLUS || mods.iter().any(|(m, _)| m == id));
        // the versions: mods/menu.txt's rows, else every mod space (newest first) and the original
        // game; the ways to start follow the chosen one (launcher::Launcher::rebuild)
        let versions = if note.is_empty() { launcher::versions(std::path::Path::new("mods"), &mods) } else { Vec::new() };
        let mut l = launcher::Launcher { versions, items: Vec::new(), sel: 0, pending: Some(launcher::Pending { xex: xex_path, mod_id, software }), note, fullscreen: cfg.fullscreen, states_root: "states".into(), needs_game };
        if l.note.is_empty() {
            l.rebuild();
        }
        l
    } else {
        spawn_game(xex_path, mod_id, software, link.clone(), None);
        launcher::Launcher { versions: Vec::new(), items: Vec::new(), sel: 0, pending: None, note: String::new(), fullscreen: cfg.fullscreen, states_root: "states".into(), needs_game: false }
    };

    // pipelines of earlier sessions are built at start (#304), listed next to the config file;
    // LMA_PIPELINE_CACHE=<file> elsewhere, or empty to turn it off
    if std::env::var_os("LMA_PIPELINE_CACHE").is_none() {
        if let Some(dir) = config::path().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            if std::fs::create_dir_all(&dir).is_ok() {
                std::env::set_var("LMA_PIPELINE_CACHE", dir.join("wgpu-pipelines.txt"));
            }
        }
    }
    keep_display_awake();
    // LMA_SHARED_GPU=1 (#6, opt-in): one wgpu device for the game's renderer and the window (bevy),
    // created here with the features the renderer needs, so a frame can be shown without the CPU
    // read-back; without it bevy and the renderer make a device each
    let render_creation = match (software, std::env::var("LMA_SHARED_GPU").as_deref() == Ok("1")) {
        (false, true) => match runtime::gpu_wgpu::create_context() {
            Ok(ctx) => {
                use bevy::render::renderer::{RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue, WgpuWrapper};
                let info = ctx.adapter.get_info();
                let rc = bevy::render::settings::RenderCreation::manual(
                    RenderDevice::from(ctx.device.clone()),
                    RenderQueue(Arc::new(WgpuWrapper::new(ctx.queue.clone()))),
                    RenderAdapterInfo(WgpuWrapper::new(info)),
                    RenderAdapter(Arc::new(WgpuWrapper::new(ctx.adapter.clone()))),
                    RenderInstance(Arc::new(WgpuWrapper::new(ctx.instance.clone()))),
                );
                let _ = SHARED_GPU.set(ctx);
                Some(rc)
            }
            Err(e) => {
                warn!("LMA_SHARED_GPU: {e}; the window makes its own device");
                None
            }
        },
        _ => None,
    };
    let mut app = App::new();
    let plugins = DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "LMA Manager 2007 (native port)".into(),
                resolution: (cfg.window_size()[0], cfg.window_size()[1]).into(),
                mode: window_mode(cfg.fullscreen),
                present_mode: present_mode(cfg.vsync()),
                ..default()
            }),
            ..default()
        });
    match render_creation {
        Some(rc) => app.add_plugins(plugins.set(bevy::render::RenderPlugin { render_creation: rc, ..default() })),
        None => app.add_plugins(plugins),
    };
    if SHARED_GPU.get().is_some() {
        runtime::gpu_wgpu::SHARED_PRESENT.store(true, std::sync::atomic::Ordering::Relaxed);
        app.sub_app_mut(bevy::render::RenderApp).add_systems(bevy::render::Render, copy_gpu_frame.in_set(bevy::render::RenderSystems::PrepareResources));
    }
    app.add_observer(detach_from_monitor)
        .insert_resource(bevy::render::error_handler::RenderErrorHandler(on_render_error))
        .insert_resource(link)
        .insert_resource(launcher)
        .init_resource::<launcher::Face>()
        .init_resource::<menu_sound::MenuSounds>()
        .init_non_send::<menu_sound::MenuStream>()
        .insert_resource(key_map)
        .add_systems(Startup, setup)
        .add_systems(Update, (set_window_icon, follow_occlusion))
        .insert_resource(Menu::from_config(&cfg))
        .init_resource::<Turn>()
        .insert_resource(device::InputKind::default())
        .insert_resource(install::Install::default())
        .insert_resource(prepare_state)
        .add_systems(Update, (install::on_drop, install::browse, install::follow, prepare::follow, device::track, device::glyphs, launcher::input, menu_sound::follow, launcher::show, menu_input, read_input, show_frame).chain())
        .add_systems(Update, (launcher::shot, window_shot));
    // LMA_LOSE_DEVICE=<frames> (#302, testing): destroy the window's GPU device every that many
    // frames, as a driver reset would
    if let Some(n) = std::env::var("LMA_LOSE_DEVICE").ok().and_then(|v| v.parse::<u32>().ok()) {
        app.sub_app_mut(bevy::render::RenderApp).add_systems(bevy::render::Render, move |device: Res<bevy::render::renderer::RenderDevice>, mut frames: Local<u32>| {
            *frames += 1;
            if *frames % n.max(1) == 0 {
                warn!("LMA_LOSE_DEVICE: destroying the window's GPU device");
                device.wgpu_device().destroy();
                // the loss is reported when the device is next polled
                let _ = device.poll(bevy::render::render_resource::PollType::wait_indefinitely());
            }
        });
    }
    app.run();
}

/// The window mode for the player's fullscreen setting.
fn window_mode(fullscreen: bool) -> bevy::window::WindowMode {
    if fullscreen { bevy::window::WindowMode::BorderlessFullscreen(bevy::window::MonitorSelection::Current) } else { bevy::window::WindowMode::Windowed }
}

/// bevy despawns a monitor's windows with it (OnMonitor is a linked relationship): when the
/// display slept or was unplugged, the window closed and the game exited. Windows here don't
/// follow their monitor.
fn detach_from_monitor(on: On<Insert, bevy::window::OnMonitor>, mut commands: Commands) {
    commands.entity(on.entity).remove::<bevy::window::OnMonitor>();
}

/// Keep the display (and the machine) awake while the game runs, as games do: a match or the
/// highlights are watched with a controller, which the OS idle timer may not count as activity.
fn keep_display_awake() {
    #[cfg(target_os = "macos")]
    {
        // caffeinate holds the power assertions until this process exits
        let pid = std::process::id().to_string();
        if let Err(e) = std::process::Command::new("/usr/bin/caffeinate").args(["-d", "-i", "-w", &pid]).spawn() {
            warn!("caffeinate: {e}");
        }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn SetThreadExecutionState(flags: u32) -> u32;
        }
        // ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED, for the main thread's lifetime
        // SAFETY: plain Win32 call with constant flags
        unsafe { SetThreadExecutionState(0x8000_0000 | 0x2 | 0x1) };
    }
}

/// Run the game on its own thread, publishing frames and reading the controller state.
/// `load`: a save state to start from (the launcher's choice); else LMA_LOAD if set.
fn spawn_game(xex_path: String, mod_id: Option<String>, software: bool, link: Link, load: Option<String>) {
    std::thread::spawn(move || {
        let set_status = |s: String| link.0.lock().unwrap().status = s;
        let bytes = match std::fs::read(&xex_path) {
            Ok(b) => b,
            Err(e) => return set_status(format!("cannot read {xex_path}: {e}")),
        };
        let x = match xex::Xex::parse(&bytes) {
            Ok(x) => x,
            Err(e) => return set_status(format!("bad xex: {e}")),
        };
        let game_dir = std::path::Path::new(&xex_path).parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
        let mut userdata = std::env::current_dir().unwrap_or_default().join("userdata");
        // recordings keep the user folder as it was at the start; replays run on a copy of it
        if let Ok(rec) = std::env::var("LMA_RECORD") {
            if let Err(e) = runtime::journal::snapshot_userdata(&userdata, &rec) {
                return set_status(format!("LMA_RECORD {rec}: can't keep the user folder: {e}"));
            }
        } else if let Ok(rec) = std::env::var("LMA_REPLAY") {
            match runtime::journal::replay_userdata(&rec) {
                Ok(Some(dir)) => userdata = dir,
                Ok(None) => warn!("replay: {rec}.userdata missing (older recording): using ./userdata, exact only if it holds the same files"),
                Err(e) => return set_status(format!("LMA_REPLAY {rec}: {e}")),
            }
        }
        let report_mod = mod_id.clone().unwrap_or_else(|| "original game".into());
        let space = match mod_id.map(|id| runtime::mods::ModSpace::load(std::path::Path::new("mods"), &id)).transpose() {
            Ok(s) => s,
            Err(e) => return set_status(e),
        };
        let user_dir = space.as_ref().map_or(userdata.clone(), |s| s.user_dir(&userdata));
        // F5 / F9 and the launcher's Continue: each version's own quick state (states/<id>/)
        script::set_states_dir(launcher::states_dir(std::path::Path::new("states"), space.as_ref().map(|s| s.id.as_str())));
        let mut m = runtime::machine::Machine::new(&x, &game_dir, &user_dir);
        if let Some(s) = &space {
            let text = x.sections().into_iter().find(|s| s.0 == ".text").map_or((x.image_base, x.image.len() as u32), |s| (s.1, s.2));
            match s.install(&mut m, &game_dir).and_then(|mut notes| {
                notes.extend(s.patch_image(&mut m.mem, text)?);
                m.after_load_writes = s.data_words(&mut m.mem);
                Ok(notes)
            }) {
                Ok(notes) => info!("mod {}: {} {} (base {}), saves in {}; {}", s.id, s.name, s.version, s.base, user_dir.display(), notes.join("; ")),
                Err(e) => return set_status(e),
            }
        }
        m.install_blocks(game_blocks::PARTS);
        // LMA_WATCH=<hex address>: log changes to that word at kernel calls (as lma-cli)
        m.watch = std::env::var("LMA_WATCH").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
        // The game's sound (#199) on the default output, for as long as this thread runs; on unless
        // LMA_AUDIO=0. The game needs its sound engine: without it Football One's Start Highlights
        // waits forever for a commentary cue (#280). LMA_AUDIO_MUTE=1 runs and mixes it all but
        // plays nothing (measurements).
        m.set_audio(std::env::var("LMA_AUDIO").as_deref() != Ok("0"));
        if !m.audio.enabled {
            warn!("audio off (LMA_AUDIO=0): the game runs no sound engine, and Football One highlights will hang (#280)");
        }
        let mute = std::env::var("LMA_AUDIO_MUTE").as_deref() == Ok("1");
        let _sound = if !m.audio.enabled || mute { None } else if runtime::native_audio::replaces_mixer() { audio_out::start_native() } else { audio_out::start(m.audio.out.clone(), m.audio.underruns.clone()) };
        // frame skip: rasterise and show every Nth guest frame (LMA_RENDER_EVERY, else the config's
        // render_every). Without it, real time skips adaptively (#56), so the game keeps its own
        // frame rate however slow rendering is; with LMA_DETERMINISTIC=1 the default stays a fixed 3.
        let settings = config::Config::load();
        let fixed = std::env::var("LMA_RENDER_EVERY").ok().and_then(|v| v.parse().ok()).or(settings.render_every);
        let every = fixed.unwrap_or(if m.deterministic { 3u64 } else { 1 }).max(1);
        m.gpu.render_every = every;
        m.gpu.present_every = every;
        m.adaptive_skip = fixed.is_none() && !m.deterministic;
        // installed before LMA_LOAD, which restores the EDRAM into the active backend
        let backend = if software {
            "software".to_string()
        } else {
            match SHARED_GPU.get().map_or_else(runtime::gpu_wgpu::WgpuBackend::new, |ctx| runtime::gpu_wgpu::WgpuBackend::from_context(ctx.clone())) {
                Ok(gpu) => {
                    let name = format!("wgpu ({})", gpu.adapter_name());
                    // the backend on a render thread in real time (#95); LMA_RENDER_THREAD=0: inline
                    m.gpu.renderer = if std::env::var("LMA_RENDER_THREAD").as_deref() == Ok("0") { Box::new(gpu) } else { Box::new(runtime::threaded::threaded(gpu, &m.mem)) };
                    name
                }
                Err(e) => {
                    warn!("no GPU backend ({e}); using the software renderer");
                    "software (no GPU)".to_string()
                }
            }
        };
        info!("renderer: {backend}, build {}", runtime::BUILD);
        if let Some(path) = load.or_else(|| std::env::var("LMA_LOAD").ok()) {
            if let Err(e) = m.load_state(std::path::Path::new(&path)) {
                set_status(format!("could not load {path}: {e}"));
            }
        }
        // LMA_POKE="addr:hexbytes,...": guest memory edits on the loaded state, as in lma-cli
        if let Err(e) = m.apply_pokes() {
            return set_status(e);
        }
        let mut driver = match script::Driver::from_env() {
            Ok(d) => d,
            Err(e) => return set_status(e),
        };
        // LMA_RECORD=path records the player's presses, from a state saved alongside
        let mut recorder = None;
        // the recording a journal is written for, and how many state loads it has seen (#108)
        let mut record_path: Option<String> = None;
        let mut record_loads = 0u32;
        if let Ok(path) = std::env::var("LMA_RECORD") {
            let state = format!("{path}.state");
            if let Some(dir) = std::path::Path::new(&path).parent().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(dir);
            }
            let started = m.save_state(std::path::Path::new(&state)).and_then(|_| script::Recorder::create(&path, &state, m.deterministic));
            match started {
                Ok(r) => {
                    info!("recording input to {path} (start state {state})");
                    recorder = Some(r);
                    // real time: also journal what host timing decides, so it replays exactly (#108)
                    if !m.deterministic {
                        match runtime::journal::Journal::record(&format!("{path}.journal")) {
                            Ok(j) => {
                                m.journal = Some(j);
                                record_path = Some(path.clone());
                            }
                            Err(e) => error!("LMA_RECORD {path}.journal: {e}; recording without a journal"),
                        }
                    }
                }
                // never stop the game over it: play on without recording
                Err(e) => error!("LMA_RECORD {path}: {e}; playing without recording"),
            }
        }
        // LMA_REPLAY=path replays an LMA_RECORD session exactly: its start state, then the journal
        // (#108); the gamepad is ignored
        let mut replay: Option<(bool, Pad)> = None;
        if let Ok(path) = std::env::var("LMA_REPLAY") {
            let loaded = m.load_state(std::path::Path::new(&format!("{path}.state"))).map_err(|e| e.to_string());
            match loaded.and_then(|_| runtime::journal::Journal::replay(&format!("{path}.journal")).map_err(|e| e.to_string())) {
                Ok(j) => {
                    info!("replaying {path}");
                    if let Some(w) = j.build_warning() {
                        warn!("replay: {w}");
                        println!("lma: replay WARNING {w}");
                    }
                    m.deterministic = false;
                    m.journal = Some(j);
                    replay = Some((true, Pad::default()));
                }
                Err(e) => return set_status(format!("LMA_REPLAY {path}: {e}")),
            }
        }
        let turbo_until = std::env::var("LMA_TURBO_UNTIL").ok().and_then(|v| v.parse::<u64>().ok()).or(settings.turbo_until).unwrap_or(4700) * 1_000_000;
        // turbo through the intro once; afterwards only F1 / `turbo` change it
        let mut intro = m.clock < turbo_until;
        let mut turbo = intro;
        let mut note = String::new();
        let mut shot: Option<(String, std::path::PathBuf)> = None;
        let started = std::time::Instant::now();
        let mut last_log = 0u64;
        // guest instructions run since launch (the clock jumps on state loads)
        let mut executed = 0u64;
        // guest frame rate over the last 2 s: average and 1% low (99th-percentile frame time)
        let (mut fps_text, mut last_fps) = (String::from("- fps"), 0u64);
        let mut cap = load_cap(&userdata);
        // the battery saver (F2, and the 30 cap): every 2nd guest frame drawn and shown while it is
        // on, the power source checked every 5 s; never in deterministic runs, replays or with a
        // fixed frame skip (LMA_RENDER_EVERY, the config's render_every). While it is on the adaptive
        // skip is off: a machine that would need 1 in 3 stays at 1 in 2 (the default allows no skip).
        let mut saver = settings.battery_saver();
        let mut saving = false;
        // saving because of the battery (not the 30 cap): the note follows only this
        let mut saving_battery = false;
        let mut power_checked: Option<std::time::Instant> = None;
        let mut render_last = (0u64, 0u64);
        // LMA_FRAME_PROFILE=1: both threads' time per guest frame in the log every 2 s (#6)
        runtime::prof::ON.store(std::env::var("LMA_FRAME_PROFILE").as_deref() == Ok("1"), std::sync::atomic::Ordering::Relaxed);
        let mut prof_last = (runtime::prof::snapshot(), 0u64, std::time::Instant::now());
        let mut last_shown = std::time::Instant::now();
        // the newest frame not yet shown (the cap holds it until its slot)
        let mut held = None;
        // LMA_FRAME_DIR=<dir> (#205): every presented frame as a small PNG, to find one-frame flashes
        let frame_dump = std::env::var_os("LMA_FRAME_DIR").map(|d| script::FrameDump::start(d.into()));
        // the black box (LMA_BLACKBOX=<min>, 0 = off): the last minutes kept for a bug report
        let mut blackbox = if replay.is_none() { blackbox::BlackBox::from_env() } else { None };
        let mut shown_times: Vec<std::time::Instant> = Vec::new(); // frames handed to the window
        loop {
            let (save, load, toggle, cycle, cap_set, report, battery_set) = {
                let mut sh = link.0.lock().unwrap();
                sh.cap = cap;
                (std::mem::take(&mut sh.save), std::mem::take(&mut sh.load), std::mem::take(&mut sh.turbo_toggle), std::mem::take(&mut sh.cap_cycle), sh.cap_set.take(), std::mem::take(&mut sh.report), sh.battery_set.take())
            };
            // the black box: a sound started over and over writes a report by itself (once a session)
            let auto = if blackbox.is_some() { runtime::events::take_repeat() } else { None };
            if let Some(why) = &auto {
                warn!("black box: {why}");
            }
            if report || auto.is_some() {
                match bug_report(&mut m, &report_mod, &fps_text, blackbox.as_mut(), auto.as_deref()) {
                    Ok(dir) => {
                        note = match &auto {
                            Some(why) => format!("{why}: bug report saved: {}", dir.display()),
                            None => format!("bug report saved: {}", dir.display()),
                        };
                        info!("{note}");
                        shot = Some(("bug report".into(), dir.join("screen.png")));
                    }
                    Err(e) => note = format!("bug report failed: {e}"),
                }
            }
            let new_cap = match (cycle, cap_set) {
                (_, Some(c)) if CAPS.contains(&c) && c != cap => Some(c),
                (true, _) => Some(CAPS[(CAPS.iter().position(|&c| c == cap).unwrap_or(1) + 1) % CAPS.len()]),
                _ => None,
            };
            if let Some(b) = battery_set {
                saver = b;
                power_checked = None;
            }
            if let Some(c) = new_cap {
                cap = c;
                power_checked = None;
                note = match save_cap(&userdata, cap) {
                    Ok(()) => format!("frame {}", cap_label(cap)),
                    Err(e) => format!("frame {} (not saved: {e})", cap_label(cap)),
                };
            }
            if power_checked.is_none_or(|t| t.elapsed().as_secs() >= 5) {
                power_checked = Some(std::time::Instant::now());
                let battery = saver.active(saver == config::BatterySaver::OnBattery && power::on_battery());
                let want = fixed.is_none() && !m.deterministic && replay.is_none() && (battery || cap == 30);
                if want != saving {
                    saving = want;
                    let e = if saving { 2 } else { every };
                    m.gpu.render_every = e;
                    m.gpu.present_every = e;
                    m.adaptive_skip = !saving && fixed.is_none() && !m.deterministic;
                    if let Some(j) = m.journal.as_mut().filter(|j| !j.is_replay()) {
                        j.record_skip(m.clock, e);
                    }
                }
                if (want && battery) != saving_battery {
                    saving_battery = want && battery;
                    note = if saving_battery { "battery saver: 30 frames a second (F2)" } else { "battery saver off" }.into();
                }
            }
            if toggle {
                turbo = !turbo;
                intro = false;
            }
            if intro && m.clock >= turbo_until {
                intro = false;
                turbo = false; // intro done: start drawing
                info!("turbo intro done at {} M instr after {:.2}s", m.clock / 1_000_000, started.elapsed().as_secs_f64());
            }
            if save {
                note = save_state(&mut m, &script::quick_state()).unwrap_or_else(|e| e);
            }
            if SAVE_AND_QUIT.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::create_dir_all("states");
                let r = save_state(&mut m, std::path::Path::new(LOST_STATE)).unwrap_or_else(|e| e);
                println!("lma: window renderer lost: {r}");
                std::process::exit(1);
            }
            if load {
                note = load_recorded(&mut m, &script::quick_state(), record_path.as_deref(), &mut record_loads).unwrap_or_else(|e| e);
            }
            while let Some((line, cmd)) = driver.next(m.now(), shot.is_some()) {
                let done = match cmd {
                    script::Cmd::Save(p) => save_state(&mut m, &p).inspect(|n| note = n.clone()),
                    script::Cmd::Load(p) => load_recorded(&mut m, &p, record_path.as_deref(), &mut record_loads).inspect(|n| note = n.clone()),
                    script::Cmd::Turbo(on) => {
                        turbo = on.unwrap_or(!turbo);
                        intro = false;
                        Ok(String::new())
                    }
                    script::Cmd::Screenshot(p) => {
                        if turbo {
                            println!("lma: note: turbo draws no frames; the screenshot waits for `turbo off`");
                        }
                        shot = Some((line.clone(), p));
                        continue;
                    }
                    script::Cmd::Status => {
                        println!("lma: {}", link.0.lock().unwrap().status);
                        continue;
                    }
                    // written at the top of the next turn, as F12
                    script::Cmd::Report => {
                        link.0.lock().unwrap().report = true;
                        Ok(String::new())
                    }
                    script::Cmd::Quit => {
                        // LMA_KERNEL_LOG=<path>: the kernel call log, as lma-cli's last_run.log
                        if let Some(path) = std::env::var_os("LMA_KERNEL_LOG") {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                                let _ = writeln!(f, "{}", m.log.join("\n"));
                            }
                        }
                        // LMA_AUDIO_WAV=<path> (#199): everything the game submitted, as in lma-cli
                        if let (Some(path), Some(c)) = (std::env::var_os("LMA_AUDIO_WAV"), &m.audio.capture) {
                            let _ = std::fs::write(&path, runtime::audio::wav(c));
                        }
                        if m.audio.enabled {
                            println!("lma: audio {} frames, {} not silent, channel peaks {:?}, {} samples NaN or infinite", m.audio.frames, m.audio.loud, m.audio.ch_peak, m.audio.non_finite);
                            for (i, c) in m.audio.xma_hw.iter().enumerate().filter(|(_, c)| c.packets > 0) {
                                println!("lma: xma {i}: {:?} (channels, Hz), {} packets -> {} samples, peak {}", c.format, c.packets, c.samples, c.peak);
                            }
                        }
                        script::ok(&line, m.now());
                        if let Some(j) = m.journal.as_mut() {
                            j.record_end(m.clock); // the recorded time holds up to here in a replay (#296)
                        }
                        // LMA_QUIT_ATEXIT=1 (tools/pgo_build.py's live training): leave through the
                        // C runtime's exit, which runs atexit handlers such as the PGO profile
                        // writer; std::process::exit on Windows is ExitProcess and skips them,
                        // so a profiled run that quits writes nothing (#360)
                        if std::env::var("LMA_QUIT_ATEXIT").as_deref() == Ok("1") {
                            extern "C" {
                                fn exit(code: i32) -> !;
                            }
                            // SAFETY: the C runtime's exit; nothing here relies on destructors running
                            unsafe { exit(0) }
                        }
                        std::process::exit(0);
                    }
                    script::Cmd::Hold { .. } | script::Cmd::Release | script::Cmd::Wait(_) | script::Cmd::Port(_) => unreachable!("handled by the driver"),
                };
                match done {
                    Ok(_) => script::ok(&line, m.now()),
                    Err(e) => script::error(&line, e),
                }
            }
            // nothing is drawn in turbo, or while the window can't be seen (the game and its sound run
            // on); skip the costly front buffer reads too
            let draw = !turbo && !WINDOW_HIDDEN.load(std::sync::atomic::Ordering::Relaxed);
            m.gpu.render = draw;
            m.gpu.present_all = draw;
            let (pad, others) = {
                let mut sh = link.0.lock().unwrap();
                let mut pad = sh.pad;
                pad.buttons |= std::mem::take(&mut sh.latched);
                (pad, sh.others)
            };
            if let Some(r) = recorder.as_mut() {
                r.update(m.now(), pad.buttons);
            }
            if let Some(b) = blackbox.as_mut() {
                b.tick(&mut m, pad.buttons);
            }
            let scripted = driver.buttons(m.now());
            // ports 2-4: the extra gamepads, plus the live script after `port N` (a port opened
            // with LMA_PADS stays connected)
            for port in 1..4 {
                let script = if driver.port() == port { scripted } else { 0 };
                if others[port - 1].is_some() || m.input.pads[port].is_some() {
                    let p = others[port - 1].unwrap_or_default();
                    m.input.set(port, Pad { buttons: p.buttons | script, ..p });
                }
            }
            let pad = if driver.port() == 0 { Pad { buttons: pad.buttons | scripted, ..pad } } else { pad };
            let mut limit = m.clock + 5_000_000;
            if let Some((render, replay_pad)) = replay.as_mut() {
                // replay: the recorded pad and rendering switch, changed exactly where they were
                // pad changes and state loads due here, in recorded order
                while let Some(ui) = m.journal.as_mut().unwrap().replay_ui(m.clock) {
                    match ui {
                        runtime::journal::Ui::Pad(r, p) => (*render, *replay_pad) = (r, p),
                        runtime::journal::Ui::Load(n) => {
                            let path = format!("{}.load{n}.state", std::env::var("LMA_REPLAY").unwrap_or_default());
                            if let Err(e) = m.load_state(std::path::Path::new(&path)) {
                                error!("replay: {path}: {e}");
                            }
                        }
                    }
                }
                let clock = m.clock;
                let j = m.journal.as_mut().unwrap();
                // on to the next pad change, or the end mark of the last save or the stop (the
                // recorded time holds up to it, #296)
                limit = limit.min(j.next_pad_clock().or(j.end_clock().filter(|&e| e > clock)).unwrap_or(u64::MAX));
                let done = j.played_out(clock).then_some(j.diverged);
                m.gpu.render = *render;
                m.gpu.present_all = *render;
                m.input.set(0, *replay_pad);
                if let Some(diverged) = done {
                    // the journal is played out: carry on live from here (gamepad, real time)
                    info!("replay finished at {} M instr, {diverged} divergences", m.clock / 1_000_000);
                    println!("lma: replay finished at {} M instr, {diverged} divergences", m.clock / 1_000_000);
                    m.end_journal();
                    replay = None;
                }
            } else {
                m.input.set(0, pad);
                if let Some(j) = m.journal.as_mut() {
                    j.record_pad(m.clock, m.gpu.render, pad);
                }
            }
            let before = m.clock;
            // the script runs on guest time (#56); on the instruction clock run() can stop exactly
            runtime::zone!("guest run");
            let exit = m.run(if m.deterministic { driver.stop_at(m.clock, limit) } else { limit });
            if let Some(j) = m.journal.as_mut() {
                j.flush();
            }
            executed += m.clock.saturating_sub(before);
            // the kernel-call log (one line per call, for lma-cli's last_run.log) would grow ~6 MB
            // a minute in play: keep the newest lines only
            if m.log.len() > 20_000 {
                let old: Vec<String> = m.log.drain(..10_000).collect();
                // LMA_KERNEL_LOG=<path>: the whole log, the lines dropped here first
                if let Some(path) = std::env::var_os("LMA_KERNEL_LOG") {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                        let _ = writeln!(f, "{}", old.join("\n"));
                    }
                }
            }
            // the frame cap: the newest frame waits for its slot (an older one waiting is replaced),
            // so a game faster than the cap still shows a steady cap; slots advance by whole
            // periods so the cadence doesn't drift with the loop's granularity
            if let Some(f) = m.gpu.latest_frame.take() {
                if let Some(d) = &frame_dump {
                    d.send(&f);
                }
                held = Some(f);
            }
            let period = std::time::Duration::from_secs_f64(1.0 / cap.min(60) as f64);
            let now = std::time::Instant::now();
            let frame = if held.is_some() && now >= last_shown + period.mul_f64(0.95) {
                last_shown = if now > last_shown + period * 2 { now } else { last_shown + period };
                held.take()
            } else {
                None
            };
            if frame.is_some() {
                shown_times.push(std::time::Instant::now());
            }
            // a frame kept on the GPU (LMA_SHARED_GPU=1) has no bytes: the shot takes the next read-back
            if let (Some((line, path)), Some((_, w, h, px))) = (&shot, &frame) {
                if !px.is_empty() {
                    match script::write_png(path, *w, *h, px) {
                        Ok(()) => script::ok(line, m.now()),
                        Err(e) => script::error(line, e),
                    }
                    shot = None;
                }
            }
            runtime::gpu_wgpu::READBACK.store(shot.is_some(), std::sync::atomic::Ordering::Relaxed);
            let elapsed = started.elapsed().as_secs();
            if elapsed >= last_fps + 2 {
                last_fps = elapsed;
                // what the player sees (frames handed to the window: the adaptive skip presents
                // every Nth guest frame), then the game's own frame rate
                let shown = match runtime::gpu::frame_stats(&shown_times) {
                    Some((_, avg, low, _)) => format!("{avg:.0} fps (1% low {low:.0})"),
                    None => "- fps".into(),
                };
                let game = runtime::gpu::frame_stats(&m.gpu.swap_times).map_or("-".into(), |(_, avg, _, _)| format!("{avg:.0}"));
                // the render thread's cost per frame it presented (a 60 fps frame has ~16 ms)
                let busy = runtime::threaded::RENDER_BUSY_NS.load(std::sync::atomic::Ordering::Relaxed);
                let presents = runtime::threaded::RENDER_PRESENTS.load(std::sync::atomic::Ordering::Relaxed);
                let render = match presents - render_last.1 {
                    0 => String::new(),
                    n => format!(" | render {:.1} ms/frame", (busy - render_last.0) as f64 / n as f64 / 1e6),
                };
                render_last = (busy, presents);
                if runtime::prof::ON.load(std::sync::atomic::Ordering::Relaxed) {
                    let now = (runtime::prof::snapshot(), m.gpu.stats.frames, std::time::Instant::now());
                    info!("{}", runtime::prof::report(&prof_last.0, &now.0, now.1 - prof_last.1, (now.2 - prof_last.2).as_secs_f64()));
                    prof_last = now;
                }
                // how far the audio callback fell behind in this interval (on time: under ~6 ms), and how
                // much sound the output has in hand (~250 ms target)
                let late = if m.audio.enabled { format!(" | audio late {} ms, queued {} ms", m.audio.take_late_ms(), m.audio.out.lock().unwrap().len() / 48) } else { String::new() };
                fps_text = format!("{shown} | game {game} fps{render}{late}");
                for times in [&mut m.gpu.swap_times, &mut shown_times] {
                    let last = times.last().copied();
                    times.clear();
                    times.extend(last);
                }
            }
            {
                let mut sh = link.0.lock().unwrap();
                if frame.is_some() {
                    sh.frame = frame;
                }
                let secs = started.elapsed().as_secs_f64();
                // LMA_AUDIO (#199): the audio invariants, when any has fired
                let note = match m.audio.errors() {
                    e if m.audio.enabled && e[0] + e[3] + e[4] + e[5] > 0 => format!("{note} | audio: {} stale blocks, {} decode errors, {} underruns, {} frames dropped", e[0], e[3], e[4], e[5]),
                    _ => note.to_string(),
                };
                sh.status = format!(
                    "{} ({}) | {:.0} M instr ({:.1} M/s) | {} frames | {} draws | {}{} | F5/^S save  F9/^L load  F1/^T turbo  F3/^F cap  F2 settings {}",
                    fps_text,
                    cap_label(cap),
                    m.clock as f64 / 1e6,
                    executed as f64 / secs / 1e6,
                    m.gpu.stats.frames,
                    m.gpu.stats.draws,
                    backend,
                    if turbo { " | TURBO (not drawing)" } else if WINDOW_HIDDEN.load(std::sync::atomic::Ordering::Relaxed) { " | window hidden (not drawing)" } else { "" },
                    note
                );
                // the status line also goes to the log every 5 s, for timing runs without a screen
                let secs = secs as u64;
                if secs >= last_log + 5 {
                    last_log = secs;
                    info!("{}s: {}", secs, sh.status);
                }
            }
            if !matches!(exit, runtime::machine::Exit::Limit) {
                // log it and tell a script driver (it would otherwise wait forever for its "ok")
                error!("game stopped: {exit:?} at {} M instr", m.clock / 1_000_000);
                println!("lma: error game stopped: {exit:?}");
                set_status(format!("game stopped: {exit:?}"));
                return;
            }
        }
    });
}

/// Remove `flag` from the arguments, returning whether it was there.
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let n = args.len();
    args.retain(|a| a != flag);
    args.len() != n
}

/// Window renderer losses so far (#302); after `MAX_RECOVERIES` the game saves a state and quits.
static RENDER_LOSSES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const MAX_RECOVERIES: u32 = 3;
/// Set when the window renderer gave up: the game thread saves `LOST_STATE` and exits.
static SAVE_AND_QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const LOST_STATE: &str = "states/device_lost.state";

/// The window renderer's errors (#302). Bevy's default quits on any of them, so a lost device (a
/// driver reset, another process hogging the GPU) ended the session with unsaved progress. A lost
/// device now rebuilds the renderer (the game keeps running on its own thread, and the next frame
/// is uploaded afresh); after `MAX_RECOVERIES`, or on any other error, the game saves a state to
/// `LOST_STATE` and quits.
fn on_render_error(error: &bevy::render::error_handler::RenderError, _: &mut World, render_world: &mut World) -> bevy::render::error_handler::RenderErrorPolicy {
    use bevy::render::error_handler::{ErrorType, RenderErrorPolicy};
    use std::sync::atomic::Ordering::Relaxed;
    if matches!(error.ty, ErrorType::DeviceLost) && RENDER_LOSSES.fetch_add(1, Relaxed) < MAX_RECOVERIES {
        warn!("window renderer lost ({}), recreating it", error.description);
        // bevy 0.19 keeps the sprite renderer's buffers and bind groups from the lost device
        // (a "Buffer does not exist" panic on the next frame): start them afresh
        use bevy::sprite_render::{ImageBindGroups, SpriteBatches, SpriteMeta};
        render_world.insert_resource(SpriteMeta::default());
        render_world.insert_resource(ImageBindGroups::default());
        render_world.insert_resource(SpriteBatches::default());
        return RenderErrorPolicy::Recover(default());
    }
    // (asked again every frame until the game thread has saved and quit)
    if !SAVE_AND_QUIT.swap(true, Relaxed) {
        error!("window renderer failed ({:?}: {}): saving {LOST_STATE} and quitting", error.ty, error.description);
    }
    RenderErrorPolicy::StopRendering
}

/// A bug report (F12 / Ctrl+B, the maintainer 22 Oct): `reports/<date>-<time>/` beside the game with the
/// state at that moment (loads with F9-style `LMA_LOAD`), a screenshot (written with the next
/// frame) and report.txt (version, mod, system, game clock, fps). Nothing leaves the PC: the player
/// sends the folder. The state holds game memory, so it never goes on a GitHub issue. With the black
/// box on it also holds the minutes before (`before.state` + `before.input`), events.txt (every
/// sound started, repeats flagged) and, for a report the app wrote by itself, why.
fn bug_report(m: &mut runtime::machine::Machine, mod_label: &str, fps: &str, blackbox: Option<&mut blackbox::BlackBox>, auto: Option<&str>) -> Result<std::path::PathBuf, String> {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // civil date from days since 1970 (UTC), as the state and log names use no local clock
    let z = days as i64 + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, mo) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + i64::from(mo <= 2);
    let dir = std::path::PathBuf::from("reports").join(format!("{y:04}{mo:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem / 60 % 60, rem % 60));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // in the background as the black box's (a full save stalled the game ~4 s, 29 Oct); the
    // thread finishes in ~2 s, so a report made just before quitting can lack game.state
    let path = dir.join("game.state");
    drop(m.save_state_background(path.clone()));
    if let Some(j) = m.journal.as_mut() {
        j.record_end(m.clock);
    }
    let state = format!("{} (written in the background)", path.display());
    let before = blackbox.map(|b| b.add_to_report(&dir)).unwrap_or_default();
    std::fs::write(dir.join("events.txt"), runtime::events::dump()).map_err(|e| format!("events.txt: {e}"))?;
    let what = match auto {
        Some(why) => format!("written by the app itself: {why}"),
        None => "(write it here)".into(),
    };
    let report = format!(
        "LMA Manager 2007 native port, bug report\n\
         What happened: {what}\n\n\n\
         version: lma-app {} (build {})\nversion played: {mod_label}\nsystem: {} {}\ngame clock: {} M instructions\nfps: {fps}\nstate: {state}\n{before}",
        env!("CARGO_PKG_VERSION"),
        runtime::BUILD,
        std::env::consts::OS,
        std::env::consts::ARCH,
        m.clock / 1_000_000,
    );
    std::fs::write(dir.join("report.txt"), report).map_err(|e| format!("report.txt: {e}"))?;
    Ok(dir)
}

fn save_state(m: &mut runtime::machine::Machine, path: &std::path::Path) -> Result<String, String> {
    match m.save_state(path) {
        Ok(n) => {
            // a replay of this session runs on the recorded time up to here, so the state checks (#296)
            if let Some(j) = m.journal.as_mut() {
                j.record_end(m.clock);
            }
            Ok(format!("saved {} ({:.0} MB)", path.display(), n as f64 / 1e6))
        }
        Err(e) => Err(format!("save failed: {e}")),
    }
}

/// Load a state; while recording with a journal, also copy it next to the recording and note the
/// load in the journal, so the session still replays exactly (#108).
fn load_recorded(m: &mut runtime::machine::Machine, path: &std::path::Path, record: Option<&str>, loads: &mut u32) -> Result<String, String> {
    let Some(rec) = record.filter(|_| m.journal.as_ref().is_some_and(|j| !j.is_replay())) else {
        return load_state(m, path);
    };
    // copy first (the state file could change later), journal the load only once it succeeded
    let copy = format!("{rec}.load{}.state", *loads + 1);
    std::fs::copy(path, &copy).map_err(|e| format!("load failed: copying {} for the recording: {e}", path.display()))?;
    let clock = m.clock;
    match load_state(m, path) {
        Ok(note) => {
            *loads += 1;
            m.journal.as_mut().unwrap().record_load(clock, *loads);
            Ok(note)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&copy);
            Err(e)
        }
    }
}

fn load_state(m: &mut runtime::machine::Machine, path: &std::path::Path) -> Result<String, String> {
    match m.load_state(path) {
        Ok(()) => Ok(format!("loaded {}", path.display())),
        Err(e) => Err(format!("load failed: {e}")),
    }
}

fn setup(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let image = Image::new_fill(
        // at LMA_RES_SCALE the picture has more pixels; the sprite keeps the game's 16:9 size
        Extent3d { width: W * runtime::gpu_wgpu::res_scale(), height: H * runtime::gpu_wgpu::res_scale(), depth_or_array_layers: 1 },
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    let handle = images.add(image);
    commands.spawn(Camera2d);
    commands.spawn((Sprite { image: handle.clone(), custom_size: Some(Vec2::new(W as f32, H as f32)), ..default() }, Transform::default()));
    let _ = SCREEN_ID.set(handle.id());
    commands.insert_resource(Screen(handle));
    commands.spawn((
        Text::new("starting..."),
        TextFont { font_size: bevy::text::FontSize::Px(14.0), ..default() },
        TextColor(Color::srgb(1.0, 1.0, 0.4)),
        Node { position_type: PositionType::Absolute, left: Val::Px(6.0), bottom: Val::Px(4.0), ..default() },
        StatusText,
    ));
    commands.spawn((
        Text::new(""),
        TextFont { font_size: bevy::text::FontSize::Px(22.0), ..default() },
        TextColor(Color::srgb(1.0, 1.0, 1.0)),
        // placed relative to the window, so it stays centred-ish when the window is resized
        Node { position_type: PositionType::Absolute, left: Val::Percent(33.0), top: Val::Percent(33.0), padding: UiRect::all(Val::Px(18.0)), ..default() },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.8)),
        Visibility::Hidden,
        MenuText,
    ));
    // the launcher's box, centred in the window; launcher::show sizes its font to fit
    commands
        .spawn(Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), justify_content: JustifyContent::Center, align_items: AlignItems::Center, ..default() })
        .with_children(|p| {
            p.spawn((
                Text::new(""),
                TextFont { font_size: bevy::text::FontSize::Px(24.0), ..default() },
                TextColor(Color::srgb(1.0, 1.0, 1.0)),
                Node { padding: UiRect::all(Val::Px(24.0)), ..default() },
                BackgroundColor(Color::srgba(0.05, 0.12, 0.08, 0.92)),
                Visibility::Hidden,
                launcher::LauncherText,
            ));
            // the same menu drawn in the game's own font, shown instead when the player's copy has it
            p.spawn((ImageNode::default(), Node::default(), BackgroundColor(Color::srgba(0.05, 0.12, 0.08, 0.92)), Visibility::Hidden, launcher::LauncherImage));
        });
}

/// F2 opens and closes the settings menu; while it is open the arrows and Enter/Esc drive it and
/// the game gets no input (`read_input`).
fn menu_input(keys: Res<ButtonInput<KeyCode>>, mut menu: ResMut<Menu>, link: Res<Link>, mut windows: Query<&mut Window, With<bevy::window::PrimaryWindow>>) {
    const ITEMS: usize = MENU_ITEMS.len();
    // the game gets its input back only once the key that closed the menu is released
    menu.just_closed = menu.just_closed && keys.any_pressed([KeyCode::Escape, KeyCode::Enter, KeyCode::Space, KeyCode::F2]);
    if keys.just_pressed(KeyCode::F2) {
        menu.just_closed = menu.open;
        menu.open = !menu.open;
        menu.sel = 0;
        // the launcher toggles fullscreen too: show the window as it is
        if let Ok(w) = windows.single() {
            menu.fullscreen = !matches!(w.mode, bevy::window::WindowMode::Windowed);
        }
        return;
    }
    if !menu.open {
        return;
    }
    if keys.just_pressed(KeyCode::ArrowDown) {
        menu.sel = (menu.sel + 1) % ITEMS;
    }
    if keys.just_pressed(KeyCode::ArrowUp) {
        menu.sel = (menu.sel + ITEMS - 1) % ITEMS;
    }
    let step = keys.just_pressed(KeyCode::ArrowRight) as i32 - keys.just_pressed(KeyCode::ArrowLeft) as i32;
    let enter = keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::Space);
    let item = MENU_ITEMS[menu.sel];
    match item {
        MenuItem::FrameCap if step != 0 || enter => {
            let mut sh = link.0.lock().unwrap();
            let cur = sh.cap_set.unwrap_or(sh.cap);
            let i = CAPS.iter().position(|&c| c == cur).unwrap_or(1) as i32;
            let step = if step == 0 { 1 } else { step };
            sh.cap_set = Some(CAPS[(i + step).rem_euclid(CAPS.len() as i32) as usize]);
        }
        MenuItem::Close if enter => {
            menu.open = false;
            menu.just_closed = true;
        }
        MenuItem::Close | MenuItem::FrameCap => {}
        _ if step != 0 || enter => {
            if menu.change(item, if step == 0 { 1 } else { step }) {
                // applied at once, and remembered for the next start (#766)
                audio_out::VOLUME.store(menu.volume as u32, std::sync::atomic::Ordering::Relaxed);
                if item == MenuItem::Battery {
                    link.0.lock().unwrap().battery_set = Some(menu.battery);
                }
                if let Ok(mut w) = windows.single_mut() {
                    w.present_mode = present_mode(menu.vsync);
                    w.mode = window_mode(menu.fullscreen);
                    if !menu.fullscreen {
                        let [width, height] = config::WINDOW_SIZES[menu.size];
                        w.resolution.set(width as f32, height as f32);
                    }
                }
                menu.save();
            }
        }
        _ => {}
    }
    if keys.just_pressed(KeyCode::Escape) {
        menu.open = false;
        menu.just_closed = true;
    }
}

#[allow(clippy::type_complexity)]
fn show_frame(
    link: Res<Link>,
    screen: Res<Screen>,
    menu: Res<Menu>,
    mut images: ResMut<Assets<Image>>,
    mut text: Query<&mut Text, (With<StatusText>, Without<MenuText>)>,
    mut menu_text: Query<(&mut Text, &mut Visibility), With<MenuText>>,
) {
    let (frame, status, cap) = {
        let mut sh = link.0.lock().unwrap();
        (sh.frame.take(), sh.status.clone(), sh.cap_set.unwrap_or(sh.cap))
    };
    if let Ok(mut t) = text.single_mut() {
        **t = if menu.status_line { status } else { String::new() };
    }
    if let Ok((mut t, mut vis)) = menu_text.single_mut() {
        *vis = if menu.open { Visibility::Visible } else { Visibility::Hidden };
        if menu.open {
            **t = menu.text(cap);
        }
    }
    if let Some((_, w, h, px)) = frame {
        // empty: the frame is on the GPU and `copy_gpu_frame` puts it in the screen texture
        if w > 0 && h > 0 && px.len() == (w * h * 4) as usize {
            if let Some(mut img) = images.get_mut(&screen.0) {
                // a picture of another size (the software present at 1x under LMA_RES_SCALE)
                if (img.width(), img.height()) != (w, h) {
                    img.resize(Extent3d { width: w, height: h, depth_or_array_layers: 1 });
                }
                // copy into the image's own buffer and hand the frame's back to the renderer: no
                // fresh 3.7 MB allocation per frame on either side (#304, page faults)
                match img.data.take() {
                    Some(mut old) => {
                        old.clear();
                        old.extend_from_slice(&px);
                        img.data = Some(old);
                        runtime::gpu_wgpu::give_frame_buf(px);
                    }
                    None => img.data = Some(px),
                }
            }
        }
    }
}

/// Keyboard + mouse + gamepads -> one Xbox controller.
#[allow(clippy::too_many_arguments)]
fn read_input(keys: Res<ButtonInput<KeyCode>>, mouse: Res<ButtonInput<MouseButton>>, gamepads: Query<&Gamepad>, link: Res<Link>, menu: Res<Menu>, key_map: Res<KeyMap>, mut turn: ResMut<Turn>, mut windows: Query<&mut Window, With<bevy::window::PrimaryWindow>>) {
    if menu.open || menu.just_closed {
        // the settings menu has the keyboard (including the key that closed it): the game sees a
        // released controller
        let mut sh = link.0.lock().unwrap();
        sh.pad = Pad::default();
        return;
    }
    let mut pad = Pad::default();
    // Cmd / Ctrl + key are the app's hotkeys (save, load, turbo): not the controller
    let cmd = keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight, KeyCode::SuperLeft, KeyCode::SuperRight]);
    for &(k, b) in &key_map.0 {
        if keys.pressed(k) && !cmd {
            pad.buttons |= b;
        }
    }
    if mouse.pressed(MouseButton::Left) {
        pad.buttons |= button::A;
    }
    if mouse.pressed(MouseButton::Right) {
        pad.buttons |= button::B;
    }
    let buttons = [
        (GamepadButton::South, button::A),
        (GamepadButton::East, button::B),
        (GamepadButton::West, button::X),
        (GamepadButton::North, button::Y),
        (GamepadButton::Start, button::START),
        (GamepadButton::Select, button::BACK),
        (GamepadButton::DPadUp, button::DPAD_UP),
        (GamepadButton::DPadDown, button::DPAD_DOWN),
        (GamepadButton::DPadLeft, button::DPAD_LEFT),
        (GamepadButton::DPadRight, button::DPAD_RIGHT),
        (GamepadButton::LeftTrigger, button::LEFT_SHOULDER),
        (GamepadButton::RightTrigger, button::RIGHT_SHOULDER),
        (GamepadButton::LeftThumb, button::LEFT_THUMB),
        (GamepadButton::RightThumb, button::RIGHT_THUMB),
    ];
    let axis = |v: Option<f32>| (v.unwrap_or(0.0).clamp(-1.0, 1.0) * 32767.0) as i16;
    let trig = |v: Option<f32>| (v.unwrap_or(0.0).clamp(0.0, 1.0) * 255.0) as u8;
    let add = |pad: &mut Pad, gp: &Gamepad| {
        for (gb, b) in buttons {
            if gp.pressed(gb) {
                pad.buttons |= b;
            }
        }
        pad.thumb_lx = pad.thumb_lx.saturating_add(axis(gp.get(GamepadAxis::LeftStickX)));
        pad.thumb_ly = pad.thumb_ly.saturating_add(axis(gp.get(GamepadAxis::LeftStickY)));
        pad.thumb_rx = pad.thumb_rx.saturating_add(axis(gp.get(GamepadAxis::RightStickX)));
        pad.thumb_ry = pad.thumb_ry.saturating_add(axis(gp.get(GamepadAxis::RightStickY)));
        pad.left_trigger = pad.left_trigger.max(trig(gp.get(GamepadButton::LeftTrigger2)));
        pad.right_trigger = pad.right_trigger.max(trig(gp.get(GamepadButton::RightTrigger2)));
    };
    let mut pads: Vec<Pad> = gamepads
        .iter()
        .take(4)
        .map(|gp| {
            let mut p = Pad::default();
            add(&mut p, gp);
            p
        })
        .collect();
    // one pad passed between two managers: BACK + START (or F4) hands it over; the chord itself
    // never reaches the game
    if menu.coop == Coop::SharedPad {
        let chord = button::BACK | button::START;
        let pad_switch = gamepads.iter().next().is_some_and(|gp| {
            gp.pressed(GamepadButton::Select) && gp.pressed(GamepadButton::Start) && (gp.just_pressed(GamepadButton::Select) || gp.just_pressed(GamepadButton::Start))
        });
        if pad_switch || keys.just_pressed(KeyCode::F4) {
            turn.0 = !turn.0;
            info!("co-op: player {}'s turn", if turn.0 { 2 } else { 1 });
        }
        if let Some(p) = pads.first_mut().filter(|p| p.buttons & chord == chord) {
            p.buttons &= !chord;
        }
    }
    // whose turn it is, in the title while the pad is shared
    let title = match menu.coop {
        Coop::SharedPad => format!("LMA Manager 2007 (native port) - player {}'s turn", if turn.0 { 2 } else { 1 }),
        _ => "LMA Manager 2007 (native port)".to_string(),
    };
    if let Ok(mut w) = windows.single_mut() {
        if w.title != title {
            w.title = title;
        }
    }
    let (pad, others) = route(menu.coop, pad, &pads, turn.0);
    let mut sh = link.0.lock().unwrap();
    sh.others = others;
    let cmd = keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight, KeyCode::SuperLeft, KeyCode::SuperRight]);
    let hotkey = |f: KeyCode, letter: KeyCode| keys.just_pressed(f) || (cmd && keys.just_pressed(letter));
    sh.save |= hotkey(KeyCode::F5, KeyCode::KeyS);
    sh.load |= hotkey(KeyCode::F9, KeyCode::KeyL);
    sh.turbo_toggle |= hotkey(KeyCode::F1, KeyCode::KeyT);
    sh.cap_cycle |= hotkey(KeyCode::F3, KeyCode::KeyF);
    sh.report |= hotkey(KeyCode::F12, KeyCode::KeyB);
    // latch new presses until the game thread has picked them up
    sh.latched |= pad.buttons & !sh.pad.buttons;
    sh.pad = pad;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Icons decode to RGBA whatever their PNG colour type: the game's title icon is RGB (an opaque
    /// alpha added).
    #[test]
    fn icons_decode_to_rgba() {
        let mut rgb = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut rgb, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&[10, 20, 30, 40, 50, 60]).unwrap();
        }
        assert_eq!(png_rgba(&rgb), Some((vec![10, 20, 30, 255, 40, 50, 60, 255], 2, 1)));
    }

    /// Co-op (the maintainer, 22 Oct): off as before (keyboard + first pad are player 1); keyboard and pad
    /// split; one pad passed by turns, player 2 connected throughout.
    #[test]
    fn coop_routes_keyboard_and_pads_to_ports() {
        let kb = Pad { buttons: button::A, ..Default::default() };
        let gp = Pad { buttons: button::B, thumb_lx: 100, ..Default::default() };
        let gp2 = Pad { buttons: button::Y, ..Default::default() };
        let both = Pad { buttons: button::A | button::B, thumb_lx: 100, ..Default::default() };
        assert_eq!(route(Coop::Off, kb, &[gp, gp2], false), (both, [Some(gp2), None, None]));
        assert_eq!(route(Coop::Off, kb, &[], false), (kb, [None, None, None]), "no pad: player 2 not connected");
        assert_eq!(route(Coop::KeyboardAndPad, kb, &[gp, gp2], false), (kb, [Some(gp), Some(gp2), None]));
        assert_eq!(route(Coop::KeyboardAndPad, kb, &[], false), (kb, [Some(Pad::default()), None, None]), "player 2 waits for a pad, still connected");
        assert_eq!(route(Coop::SharedPad, kb, &[gp], false), (both, [Some(Pad::default()), None, None]));
        assert_eq!(route(Coop::SharedPad, kb, &[gp], true), (Pad::default(), [Some(both), None, None]), "player 2's turn: the pad and keyboard drive port 2");
        let mut m = Menu::default();
        for want in [Coop::KeyboardAndPad, Coop::SharedPad, Coop::Off] {
            m.change(MenuItem::Coop, 1);
            assert_eq!(m.coop, want);
        }
        assert_eq!(Coop::from_config(Coop::SharedPad.config().as_deref()), Coop::SharedPad);
        assert_eq!(Coop::from_config(None), Coop::Off);
    }

    /// F2's settings (#766): each row changes its setting, the volume in steps of 10 within 0-100,
    /// the window size around the list; the text shows them all.
    #[test]
    fn settings_menu_changes_and_shows_its_rows() {
        let mut m = Menu::default();
        assert!(m.change(MenuItem::VSync, 1) && !m.vsync);
        assert!(m.change(MenuItem::Window, -1) && m.fullscreen);
        assert!(m.change(MenuItem::WindowSize, -1));
        assert_eq!(config::WINDOW_SIZES[m.size], [2560, 1440], "left from the first size wraps to the last");
        assert!(m.change(MenuItem::RenderScale, -1));
        assert_eq!(m.scale, config::RENDER_SCALE_MAX, "left from 1x wraps to the largest");
        assert!(m.change(MenuItem::RenderScale, 1) && m.scale == 1);
        assert!(m.change(MenuItem::RenderScale, 1) && m.scale == 2);
        assert!(m.change(MenuItem::Volume, -1) && m.volume == 90);
        for _ in 0..12 {
            m.change(MenuItem::Volume, -1);
        }
        assert_eq!(m.volume, 0, "never below 0");
        assert!(m.change(MenuItem::Battery, 1) && m.battery == config::BatterySaver::Always);
        assert!(m.change(MenuItem::Battery, 1) && m.battery == config::BatterySaver::Off, "round the three choices");
        assert!(!m.change(MenuItem::Close, 1) && !m.change(MenuItem::FrameCap, 1), "the cap and Close are handled apart");
        m.sel = MENU_ITEMS.iter().position(|&i| i == MenuItem::Volume).unwrap();
        let t = m.text(60);
        for line in ["  Battery saver  < off >", "  VSync          < off >", "  Window         < fullscreen >", "  Window size    < 2560 x 1440 >", "  Render scale   < 2x (1440p) >  (on next start)", "> Volume         < 0% >", "  Status line    < off >"] {
            assert!(t.contains(line), "{line:?} in\n{t}");
        }
    }

    #[test]
    fn window_outlives_its_monitor() {
        let mut world = World::new();
        world.add_observer(detach_from_monitor);
        let monitor = world.spawn_empty().id();
        let window = world.spawn(bevy::window::OnMonitor(monitor)).id();
        world.flush();
        world.despawn(monitor);
        assert!(world.get_entity(window).is_ok(), "the window went with its monitor");
        // without the observer it would have (bevy's linked relationship)
        let mut plain = World::new();
        let monitor = plain.spawn_empty().id();
        let window = plain.spawn(bevy::window::OnMonitor(monitor)).id();
        plain.despawn(monitor);
        assert!(plain.get_entity(window).is_err());
    }

    /// A lost window device is recovered `MAX_RECOVERIES` times, each with the sprite renderer's
    /// state reset; the next loss, or any other error, asks the game thread to save and quit (#302).
    #[test]
    fn device_losses_recover_then_save_and_quit() {
        use bevy::render::error_handler::{ErrorType, RenderError, RenderErrorPolicy};
        use std::sync::atomic::Ordering::Relaxed;
        let lost = || RenderError { ty: ErrorType::DeviceLost, description: String::new(), source: None };
        let (mut main_world, mut render_world) = (World::new(), World::new());
        for _ in 0..MAX_RECOVERIES {
            assert!(matches!(on_render_error(&lost(), &mut main_world, &mut render_world), RenderErrorPolicy::Recover(_)));
            assert!(!SAVE_AND_QUIT.load(Relaxed));
        }
        assert!(render_world.contains_resource::<bevy::sprite_render::SpriteMeta>(), "sprite state reset for the new device");
        assert!(matches!(on_render_error(&lost(), &mut main_world, &mut render_world), RenderErrorPolicy::StopRendering));
        assert!(SAVE_AND_QUIT.load(Relaxed));
        // asked again every frame until the game thread quits: still stopped, nothing recovers
        assert!(matches!(on_render_error(&lost(), &mut main_world, &mut render_world), RenderErrorPolicy::StopRendering));
    }
}
