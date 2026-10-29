//! The player's settings, kept between runs (#4): where the game is, so lma-app starts without
//! arguments. A TOML file in the platform's config folder:
//! `~/Library/Application Support/lma-rs/config.toml` (macOS), `%APPDATA%\lma-rs\config.toml`
//! (Windows), `$XDG_CONFIG_HOME/lma-rs/config.toml` or `~/.config/lma-rs/config.toml` (others).
//!
//! ```toml
//! game_dir = "/Users/me/lma/LMA_full"
//! fullscreen = false
//! mod_space = "2025-26"   # left out: vanilla
//! render_every = 1        # left out: adaptive frame skip
//! turbo_until = 4700      # intro in turbo up to 4700 M instructions; 0 off
//! status_line = false     # the fps/debug line at the top (F2 toggles it; off unless chosen)
//! battery_saver = "on-battery"  # draw every 2nd frame: "off", "on-battery" (default), "always"
//!
//! [keys]                  # keyboard rebinding, see keys.rs
//! A = ["Enter", "J"]
//! ```

use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// The folder holding `default.xex` (the player's own copy of the game).
    pub game_dir: Option<PathBuf>,
    /// Borderless fullscreen instead of a window (the launcher toggles it).
    #[serde(default)]
    pub fullscreen: bool,
    /// The mod space last chosen in the launcher (None: vanilla).
    #[serde(default)]
    pub mod_space: Option<String>,
    /// Render and show every Nth guest frame (LMA_RENDER_EVERY wins); None: adaptive.
    #[serde(default)]
    pub render_every: Option<u64>,
    /// Run the intro in turbo up to this many million instructions (LMA_TURBO_UNTIL wins; 0 off).
    #[serde(default)]
    pub turbo_until: Option<u64>,
    /// Keyboard bindings for controller port 1, by button (`keys.rs`): `A = ["Enter", "J"]`.
    #[serde(default)]
    pub keys: std::collections::BTreeMap<String, Vec<String>>,
    /// Show the fps/debug status line over the game (F2's Status line, saved when toggled). Off by
    /// default: a player sees no technical text (#766); `LMA_STATUS_LINE=1` shows it for one run.
    /// The line still goes to the log every 5 s either way, for timing runs.
    #[serde(default)]
    pub status_line: bool,
    /// Wait for the display's refresh before showing a frame (F2's VSync; None: on). Off shows each
    /// frame at once, which can tear.
    #[serde(default)]
    pub vsync: Option<bool>,
    /// The window's size in logical pixels when windowed (F2's Window size; None: 1280x720).
    #[serde(default)]
    pub window_size: Option<[u32; 2]>,
    /// The game's sound level, 0-100 (F2's Volume; None: 100).
    #[serde(default)]
    pub volume: Option<u8>,
    /// Co-op controls (F2's Co-op): "keyboard_pad" (the keyboard is player 1, the pad player 2),
    /// "shared_pad" (one pad passed between the two: BACK + START or F4 switches); None: off.
    #[serde(default)]
    pub coop: Option<String>,
    /// F2's Battery saver (None: on battery).
    #[serde(default)]
    pub battery_saver: Option<BatterySaver>,
    /// Render at this many times 1280x720 (F2's Render scale, from the next start; None: 1;
    /// `LMA_RES_SCALE` wins, #797).
    #[serde(default)]
    pub render_scale: Option<u32>,
}

/// The battery saver (F2): while it is on, the game draws every second frame (30 a second; the
/// game itself runs at its full rate), which takes a match from about 2.1 cores to 1.3 on an M4.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BatterySaver {
    Off,
    /// Only while the computer runs on its battery.
    #[default]
    OnBattery,
    Always,
}

impl BatterySaver {
    pub const ALL: [BatterySaver; 3] = [BatterySaver::Off, BatterySaver::OnBattery, BatterySaver::Always];

    pub fn label(self) -> &'static str {
        match self {
            BatterySaver::Off => "off",
            BatterySaver::OnBattery => "on battery",
            BatterySaver::Always => "always",
        }
    }

    /// Whether it saves now, given whether the machine is on its battery.
    pub fn active(self, on_battery: bool) -> bool {
        match self {
            BatterySaver::Off => false,
            BatterySaver::OnBattery => on_battery,
            BatterySaver::Always => true,
        }
    }
}

/// The largest render scale F2 offers (3: 3840x2160).
pub const RENDER_SCALE_MAX: u32 = 3;

/// The window sizes F2 offers (16:9, like the game's 1280x720).
pub const WINDOW_SIZES: [[u32; 2]; 4] = [[1280, 720], [1600, 900], [1920, 1080], [2560, 1440]];

impl Config {
    pub fn vsync(&self) -> bool {
        self.vsync.unwrap_or(true)
    }

    pub fn window_size(&self) -> [u32; 2] {
        self.window_size.unwrap_or(WINDOW_SIZES[0])
    }

    pub fn volume(&self) -> u8 {
        self.volume.unwrap_or(100).min(100)
    }

    pub fn battery_saver(&self) -> BatterySaver {
        self.battery_saver.unwrap_or_default()
    }

    pub fn render_scale(&self) -> u32 {
        self.render_scale.unwrap_or(1).clamp(1, RENDER_SCALE_MAX)
    }

    /// Whether the status line shows at start: `LMA_STATUS_LINE` (1 on, 0 off) over the saved choice.
    pub fn status_line_shown(&self) -> bool {
        match std::env::var("LMA_STATUS_LINE").as_deref() {
            Ok("1") => true,
            Ok("0") => false,
            _ => self.status_line,
        }
    }
}

/// The config file's path on this platform, if a home or config folder is known.
pub fn path() -> Option<PathBuf> {
    let dir = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    dir.map(|d| d.join("lma-rs").join("config.toml"))
}

impl Config {
    /// The saved settings, or the defaults when there are none (or they don't parse).
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string(self).map_err(std::io::Error::other)?)
    }

    pub fn load() -> Self {
        path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        match path() {
            Some(p) => self.save_to(&p),
            None => Err(std::io::Error::other("no config folder")),
        }
    }
}

/// The game to run: a path given on the command line (remembered for next time), else the
/// remembered game folder, else `fallback`. Returns the `default.xex` path and the config to save
/// (Some when it changed).
pub fn resolve_xex(arg: Option<&str>, config: &Config, fallback: &str) -> (PathBuf, Option<Config>) {
    if let Some(a) = arg {
        let p = PathBuf::from(a);
        // a folder or the .xex itself
        let xex = if p.is_dir() { p.join("default.xex") } else { p };
        let dir = xex.parent().map(|d| d.to_path_buf()).filter(|d| !d.as_os_str().is_empty());
        let dir = dir.map(|d| std::fs::canonicalize(&d).unwrap_or(d));
        let changed = (dir.is_some() && dir != config.game_dir).then(|| Config { game_dir: dir, ..config.clone() });
        return (xex, changed);
    }
    match &config.game_dir {
        Some(d) => (d.join("default.xex"), None),
        None => (PathBuf::from(fallback), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("lma-config-{}", std::process::id()));
        let path = dir.join("lma-rs").join("config.toml");
        assert_eq!(Config::load_from(&path), Config::default(), "no file: defaults");
        let keys = [("A".to_string(), vec!["Enter".to_string(), "J".to_string()])].into();
        let c = Config { game_dir: Some(PathBuf::from("/games/LMA_full")), fullscreen: true, mod_space: Some("2025-26".into()), render_every: Some(2), turbo_until: Some(0), keys, status_line: true, vsync: Some(false), window_size: Some([1920, 1080]), volume: Some(40), coop: Some("shared_pad".into()), battery_saver: Some(BatterySaver::Always), render_scale: Some(2) };
        c.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path), c);
        // a file from before `fullscreen` (or `status_line`) existed still loads, the line hidden
        std::fs::write(&path, "game_dir = \"/games/LMA_full\"\n").unwrap();
        assert_eq!(Config::load_from(&path), Config { game_dir: Some(PathBuf::from("/games/LMA_full")), ..Default::default() });
        assert!(!Config::load_from(&path).status_line, "a player sees no debug line unless they chose it");
        let old = Config::load_from(&path);
        assert_eq!((old.vsync(), old.window_size(), old.volume()), (true, [1280, 720], 100), "a file from before #766's settings: vsync on, 1280x720, full volume");
        assert_eq!(old.battery_saver(), BatterySaver::OnBattery, "the saver works on battery unless turned off");
        std::fs::write(&path, "battery_saver = \"off\"\n").unwrap();
        assert_eq!(Config::load_from(&path).battery_saver(), BatterySaver::Off);
        std::fs::write(&path, "not toml [").unwrap();
        assert_eq!(Config::load_from(&path), Config::default(), "unreadable: defaults");
        std::fs::remove_dir_all(dir).ok();
    }

    /// The battery saver's three choices: off never saves, on battery follows the power source,
    /// always always saves; F2 shows each by its label, and they cycle in this order.
    #[test]
    fn the_battery_saver_follows_its_choice() {
        use BatterySaver::*;
        for (b, mains, battery) in [(Off, false, false), (OnBattery, false, true), (Always, true, true)] {
            assert_eq!((b.active(false), b.active(true)), (mains, battery), "{b:?}");
        }
        assert_eq!(BatterySaver::ALL.map(BatterySaver::label), ["off", "on battery", "always"]);
        assert_eq!(BatterySaver::default(), OnBattery);
        let s = toml::to_string(&Config { battery_saver: Some(OnBattery), ..Default::default() }).unwrap();
        assert!(s.contains("battery_saver = \"on-battery\""), "{s}");
    }

    #[test]
    fn argument_wins_and_is_remembered() {
        let remembered = Config { game_dir: Some(PathBuf::from("/old")), ..Default::default() };
        let (xex, save) = resolve_xex(Some("/new/LMA_full/default.xex"), &remembered, "fallback.xex");
        assert_eq!(xex, PathBuf::from("/new/LMA_full/default.xex"));
        assert_eq!(save.unwrap().game_dir, Some(PathBuf::from("/new/LMA_full")));
        // no argument: the remembered folder, nothing to save
        let (xex, save) = resolve_xex(None, &remembered, "fallback.xex");
        assert_eq!((xex, save), (PathBuf::from("/old/default.xex"), None));
        // nothing remembered: the fallback
        let (xex, _) = resolve_xex(None, &Config::default(), "fallback.xex");
        assert_eq!(xex, PathBuf::from("fallback.xex"));
        // the same folder again: nothing to save
        let (_, save) = resolve_xex(Some("/old/default.xex"), &remembered, "fallback.xex");
        assert!(save.is_none());
    }
}
