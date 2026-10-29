//! Scripted input for lma-app, so the live window can be driven and checked without OS-level
//! keystrokes or screen capture.
//!
//! LMA_INPUT="at:buttons:len,..." works as in lma-cli (hex button mask from `at` for `len`
//! million instructions). LMA_INPUT_LIVE=1 also reads commands from stdin, one per line, run in
//! order; each prints `lma: ok <command> @ <M ticks>` (or `lma: error ...`) to stdout when done.
//! Live commands count guest time in M ticks of the 50 MHz timebase: real time (1 M = 20 ms),
//! or the instruction count under LMA_DETERMINISTIC=1 (as before):
//!
//!   press START [len]        press for len M ticks (default 8), then release for 4 M
//!   hold A [len]             like press; with no len, hold until `release` and go on at once
//!   release                  release every held button
//!   port 2                   later press/hold/release drive controller port 2 (1-4; needs
//!                            LMA_PADS for ports above 1, #206)
//!   wait 200                 let 200 M ticks pass (4 s in real time)
//!   wait 25s                 the same in seconds of guest time (25s = 1250 M ticks; decimals
//!                            allowed: 0.5s)
//!   save [path] / load [path]  save state (default: the version's quick state, states/quick.state)
//!   screenshot path.png      write the next drawn frame (game resolution, not the window)
//!   turbo [on|off]           toggle or set turbo (no rendering)
//!   status                   print the status line
//!   quit                     exit the app
//!
//! Buttons: A B X Y START BACK UP DOWN LEFT RIGHT LB RB LS RS, joined with `+`, or `0x` hex.

use runtime::input::parse_buttons;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

const M: u64 = 1_000_000;
/// Default press length and the released gap after a press, in M instructions (a few frames).
const PRESS: u64 = 8;
const GAP: u64 = 4;

/// Commands that need the machine or the app; button commands are handled by `Driver`.
pub enum Cmd {
    Hold { buttons: u16, len: Option<u64> },
    Release,
    Wait(u64),
    Save(PathBuf),
    Load(PathBuf),
    Screenshot(PathBuf),
    Turbo(Option<bool>),
    Port(usize),
    Status,
    /// A bug report, as F12 / Ctrl+B.
    Report,
    Quit,
}

/// Where the quick state lives: `states/` until the game thread sets the version's own folder
/// (`states/<mod id>/`, main.rs `spawn_game`), so F5 / F9 and `save` / `load` without a path never
/// mix one version's state into another's.
static STATES_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn set_states_dir(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = STATES_DIR.set(dir);
}

pub fn states_dir() -> PathBuf {
    STATES_DIR.get().cloned().unwrap_or_else(|| PathBuf::from("states"))
}

pub fn quick_state() -> PathBuf {
    states_dir().join("quick.state")
}

/// Parse one command line; `None` for blank lines and `#` comments.
pub fn parse(line: &str) -> Result<Option<Cmd>, String> {
    let words: Vec<&str> = line.split('#').next().unwrap_or("").split_whitespace().collect();
    let Some((&verb, args)) = words.split_first() else { return Ok(None) };
    let num = |i: usize| -> Result<Option<u64>, String> {
        args.get(i).map(|a| a.parse().map_err(|_| format!("not a number: {a}"))).transpose()
    };
    let path = |i: usize| args.get(i).map(PathBuf::from);
    let cmd = match verb.to_ascii_lowercase().as_str() {
        "press" | "hold" => {
            let b = args.first().ok_or("missing button")?;
            let buttons = parse_buttons(b).ok_or_else(|| format!("unknown button: {b}"))?;
            let len = num(1)?.or(if verb.eq_ignore_ascii_case("press") { Some(PRESS) } else { None });
            Cmd::Hold { buttons, len }
        }
        "release" => Cmd::Release,
        "wait" => Cmd::Wait(match args.first() {
            // seconds of the 50 MHz timebase: 50 M ticks each
            Some(a) if a.ends_with('s') => {
                let secs: f64 = a[..a.len() - 1].parse().ok().filter(|s: &f64| s.is_finite() && *s >= 0.0).ok_or_else(|| format!("not a number of seconds: {a}"))?;
                (secs * 50.0).round() as u64
            }
            _ => num(0)?.ok_or("missing M ticks (or seconds: 25s)")?,
        }),
        "save" => Cmd::Save(path(0).unwrap_or_else(quick_state)),
        "load" => Cmd::Load(path(0).unwrap_or_else(quick_state)),
        "screenshot" => Cmd::Screenshot(path(0).ok_or("missing path")?),
        "turbo" => Cmd::Turbo(match args.first().copied() {
            None => None,
            Some("on") => Some(true),
            Some("off") => Some(false),
            Some(a) => return Err(format!("turbo takes on or off, not {a}")),
        }),
        "port" => match num(0)? {
            Some(n @ 1..=4) => Cmd::Port(n as usize - 1),
            _ => return Err("port takes 1 to 4".into()),
        },
        "status" => Cmd::Status,
        "report" => Cmd::Report,
        "quit" | "exit" => Cmd::Quit,
        v => return Err(format!("unknown command: {v}")),
    };
    Ok(Some(cmd))
}

/// LMA_RECORD=path: the player's own button presses (gamepad and keyboard, not script input),
/// written as LMA_INPUT items in M ticks of guest time, one per line and flushed as they happen.
/// The state they start from is saved next to it as `path.state`; replay with
/// `LMA_LOAD=path.state LMA_INPUT=@path`.
pub struct Recorder {
    out: std::io::BufWriter<std::fs::File>,
    held: u16,
    since: u64,
    /// Guest time of the last update, so a button still held at exit can be closed.
    last: u64,
}

impl Recorder {
    pub fn create(path: &str, state: &str, deterministic: bool) -> std::io::Result<Self> {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        let clock = if deterministic { "instruction count (LMA_DETERMINISTIC=1)" } else { "real time, 1 M = 20 ms" };
        writeln!(out, "# lma-app recording: at:buttons:len in M ticks of guest time ({clock})")?;
        writeln!(out, "# replay: LMA_LOAD={state} LMA_INPUT=@{path}")?;
        writeln!(out, "# build {} (exact replays: LMA_REPLAY with this build's binaries)", runtime::BUILD)?;
        out.flush()?;
        Ok(Recorder { out, held: 0, since: 0, last: 0 })
    }

    /// Called with the pad's buttons at guest time `now`; writes the previous mask when it changes.
    pub fn update(&mut self, now: u64, buttons: u16) {
        use std::io::Write;
        self.last = self.last.max(now);
        if buttons == self.held {
            return;
        }
        if self.held != 0 {
            // guest time can jump back (a state loaded mid-session): never underflow
            let (at, end) = (self.since / M, now / M);
            let _ = writeln!(self.out, "{at}:{:x}:{}", self.held, end.saturating_sub(at).max(1));
            let _ = self.out.flush();
        }
        (self.held, self.since) = (buttons, now);
    }
}

impl Drop for Recorder {
    /// A button still held when the app closes is written too (up to the last update).
    fn drop(&mut self) {
        if self.held != 0 {
            let last = self.last;
            self.update(last.max(self.since + M), 0);
        }
    }
}

pub fn ok(line: &str, clock: u64) {
    println!("lma: ok {line} @ {} M", clock / M);
}

pub fn error(line: &str, e: impl std::fmt::Display) {
    println!("lma: error {line}: {e}");
}

/// The LMA_INPUT timeline plus the live command queue, advanced by the game thread.
pub struct Driver {
    script: Vec<(u64, u16)>,
    next: usize,
    script_buttons: u16,
    live: Option<Receiver<(String, Cmd)>>,
    queue: VecDeque<(String, Cmd)>,
    held: u16,
    release_at: Option<u64>,
    /// The front command waits until this clock; its line is reported then.
    busy: Option<(u64, String)>,
    /// The controller port (0-3) the live commands drive.
    port: usize,
}

impl Driver {
    pub fn from_env() -> Result<Self, String> {
        let script = runtime::input::parse_script(&std::env::var("LMA_INPUT").unwrap_or_default())?;
        let live = std::env::var("LMA_INPUT_LIVE").is_ok_and(|v| v != "0").then(spawn_stdin);
        Ok(Self { script, next: 0, script_buttons: 0, live, queue: VecDeque::new(), held: 0, release_at: None, busy: None, port: 0 })
    }

    /// Buttons held by the script and live commands at `clock`.
    pub fn buttons(&mut self, clock: u64) -> u16 {
        while let Some(&(_, b)) = self.script.get(self.next).filter(|e| e.0 <= clock) {
            self.script_buttons = b;
            self.next += 1;
        }
        if self.release_at.is_some_and(|t| t <= clock) {
            self.held = 0;
            self.release_at = None;
        }
        self.script_buttons | self.held
    }

    /// The controller port the live commands drive (0 = port 1, with the keyboard and LMA_INPUT).
    pub fn port(&self) -> usize {
        self.port
    }

    /// Where to stop the next run so scripted events land on time.
    pub fn stop_at(&self, clock: u64, default: u64) -> u64 {
        let script = self.script.get(self.next).map(|e| e.0);
        [script, self.release_at, self.busy.as_ref().map(|b| b.0)].into_iter().flatten().filter(|&t| t > clock).fold(default, u64::min)
    }

    /// Run the button commands that are due and return the next one that needs the machine.
    /// `blocked` (e.g. a screenshot still waiting for a frame) holds the queue.
    pub fn next(&mut self, clock: u64, blocked: bool) -> Option<(String, Cmd)> {
        if let Some(rx) = &self.live {
            self.queue.extend(rx.try_iter());
        }
        if let Some((until, line)) = &self.busy {
            if clock < *until {
                return None;
            }
            ok(line, clock);
            self.busy = None;
        }
        if blocked {
            return None;
        }
        loop {
            let (line, cmd) = self.queue.pop_front()?;
            match cmd {
                Cmd::Hold { buttons, len: Some(len) } => {
                    self.held |= buttons;
                    self.release_at = Some(clock + len * M);
                    self.busy = Some((clock + (len + GAP) * M, line));
                    return None;
                }
                Cmd::Hold { buttons, len: None } => {
                    self.held |= buttons;
                    self.release_at = None;
                    ok(&line, clock);
                }
                Cmd::Release => {
                    self.held = 0;
                    self.release_at = None;
                    ok(&line, clock);
                }
                Cmd::Wait(n) => {
                    self.busy = Some((clock + n * M, line));
                    return None;
                }
                Cmd::Port(p) => {
                    (self.port, self.held, self.release_at) = (p, 0, None);
                    ok(&line, clock);
                }
                cmd => return Some((line, cmd)),
            }
        }
    }
}

/// Read commands from stdin on their own thread.
fn spawn_stdin() -> Receiver<(String, Cmd)> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            let line = line.trim().to_string();
            match parse(&line) {
                Ok(Some(cmd)) => {
                    if tx.send((line, cmd)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => error(&line, e),
            }
        }
    });
    rx
}

/// LMA_FRAME_DIR (#205): every frame the game presents, at a quarter of its size, as PNGs named by
/// guest frame number. A thread encodes them, so the game only pays for a copy.
pub struct FrameDump(std::sync::mpsc::Sender<(u64, u32, u32, Vec<u8>)>);

impl FrameDump {
    pub fn start(dir: PathBuf) -> Self {
        let (tx, rx) = channel::<(u64, u32, u32, Vec<u8>)>();
        std::thread::spawn(move || {
            for (n, w, h, px) in rx {
                let (w, h, px) = shrink(w, h, &px, 4);
                if let Err(e) = write_png(&dir.join(format!("{n:07}.png")), w, h, &px) {
                    eprintln!("LMA_FRAME_DIR {}: {e}", dir.display());
                    return;
                }
            }
        });
        Self(tx)
    }

    pub fn send(&self, frame: &(u64, u32, u32, Vec<u8>)) {
        let _ = self.0.send(frame.clone());
    }
}

/// An RGBA image `f` times smaller on each side, each pixel the average of an f x f box.
pub fn shrink(w: u32, h: u32, px: &[u8], f: u32) -> (u32, u32, Vec<u8>) {
    let (sw, sh) = ((w / f).max(1), (h / f).max(1));
    let mut out = Vec::with_capacity((sw * sh * 4) as usize);
    for y in 0..sh {
        for x in 0..sw {
            let mut sum = [0u32; 4];
            let mut n = 0;
            for yy in y * f..((y + 1) * f).min(h) {
                for xx in x * f..((x + 1) * f).min(w) {
                    let i = ((yy * w + xx) * 4) as usize;
                    for c in 0..4 {
                        sum[c] += px[i + c] as u32;
                    }
                    n += 1;
                }
            }
            out.extend(sum.iter().map(|s| (s / n.max(1)) as u8));
        }
    }
    (sw, sh, out)
}

/// Write an RGBA frame as PNG, creating the folder.
pub fn write_png(path: &std::path::Path, w: u32, h: u32, px: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut wr| wr.write_image_data(px)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrink_averages_boxes() {
        // 4x2 image, 2x2 boxes: left box black/white (128), right box all 200
        let mut px = vec![0u8; 4 * 2 * 4];
        for (i, v) in [0u8, 255, 200, 200, 255, 0, 200, 200].iter().enumerate() {
            px[i * 4..i * 4 + 4].copy_from_slice(&[*v, *v, *v, 255]);
        }
        let (w, h, out) = shrink(4, 2, &px, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![127, 127, 127, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn report_is_a_command() {
        assert!(matches!(parse("report"), Ok(Some(Cmd::Report))));
        assert!(matches!(parse("  REPORT  # the black box"), Ok(Some(Cmd::Report))));
    }

    #[test]
    fn recorder_replays() {
        let dir = std::env::temp_dir().join(format!("lma-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rec.txt").to_string_lossy().into_owned();
        let mut r = Recorder::create(&path, "start.state", false).unwrap();
        // A held 4900-5000 M, then A+B until 5003 M, then nothing
        for (t, b) in [(4_900, 0x1000), (4_950, 0x1000), (5_000, 0x3000), (5_003, 0), (5_010, 0), (5_020, 0x10), (5_025, 0x10)] {
            r.update(t * M, b);
        }
        drop(r); // START still held: written up to the last update
        let ev = runtime::input::parse_script(&format!("@{path}")).unwrap();
        assert_eq!(ev, vec![(4_900 * M, 0x1000), (5_000 * M, 0), (5_000 * M, 0x3000), (5_003 * M, 0), (5_020 * M, 0x10), (5_025 * M, 0)]);
        std::fs::remove_dir_all(dir).ok();
    }

    fn driver(cmds: &[&str]) -> Driver {
        let queue = cmds.iter().map(|l| (l.to_string(), parse(l).unwrap().unwrap())).collect();
        Driver { script: vec![], next: 0, script_buttons: 0, live: None, queue, held: 0, release_at: None, busy: None, port: 0 }
    }

    #[test]
    fn port_switches_the_controller_the_presses_drive() {
        assert!(parse("port 5").is_err() && parse("port").is_err());
        let mut d = driver(&["port 2", "press A 2", "port 1", "press B 2"]);
        assert!(d.next(0, false).is_none());
        assert_eq!((d.port(), d.buttons(0)), (1, runtime::input::button::A));
        // after A's press and gap: back on port 1, B down
        assert!(d.next((2 + GAP) * M, false).is_none());
        assert_eq!((d.port(), d.buttons((2 + GAP) * M)), (0, runtime::input::button::B));
    }

    #[test]
    fn press_holds_then_releases_before_the_next_command() {
        let mut d = driver(&["press START", "save"]);
        assert!(d.next(0, false).is_none());
        assert_eq!(d.buttons(0), runtime::input::button::START);
        assert_eq!(d.stop_at(0, u64::MAX), PRESS * M);
        assert_eq!(d.buttons(PRESS * M), 0);
        assert!(d.next(PRESS * M, false).is_none(), "still in the released gap");
        assert!(matches!(d.next((PRESS + GAP) * M, false), Some((_, Cmd::Save(_)))));
    }

    #[test]
    fn hold_without_length_continues_at_once() {
        let mut d = driver(&["hold A+UP", "status"]);
        assert!(matches!(d.next(0, false), Some((_, Cmd::Status))));
        assert_eq!(d.buttons(5 * M), runtime::input::button::A | runtime::input::button::DPAD_UP);
    }

    /// `wait N` then `quit` (tools/pgo_build.py's live training): quit comes due N M ticks after
    /// the wait started, not before (#360).
    #[test]
    fn wait_then_quit() {
        let mut d = driver(&["wait 3000", "quit"]);
        assert!(d.next(1_000 * M, false).is_none(), "the wait starts at 1000 M");
        assert_eq!(d.stop_at(1_000 * M, u64::MAX), 4_000 * M);
        assert!(d.next(3_999 * M, false).is_none());
        assert!(matches!(d.next(4_000 * M, false), Some((_, Cmd::Quit))));
    }

    #[test]
    fn wait_in_seconds() {
        assert!(matches!(parse("wait 25s"), Ok(Some(Cmd::Wait(1250)))));
        assert!(matches!(parse("wait 0.5s"), Ok(Some(Cmd::Wait(25)))));
        assert!(matches!(parse("wait 200"), Ok(Some(Cmd::Wait(200)))));
        assert!(parse("wait xs").is_err());
        assert!(parse("wait -1s").is_err());
    }

    #[test]
    fn parse_errors() {
        assert!(parse("press Z").is_err());
        assert!(parse("wait").is_err());
        assert!(parse("dance").is_err());
        assert!(parse("  # comment").unwrap().is_none());
    }
}
