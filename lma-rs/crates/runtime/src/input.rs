//! Controller state seen by the game (XINPUT_GAMEPAD), fed by any host device.
//!
//! The front end (Bevy) maps keyboard, mouse and real gamepads onto `Pad` and calls
//! `Input::set`. Default keyboard layout mirrors the original pad so menus work without a
//! controller; it is user-rebindable in the launcher.

/// XINPUT button bits.
pub mod button {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
    /// Script and LMA_INPUT pseudo-buttons on two bits XInput leaves unused: the left / right
    /// trigger pressed fully (the game's menus switch sub-pages with them). `state_bytes`
    /// moves them to the trigger bytes; the guest never sees the bits.
    pub const LEFT_TRIGGER: u16 = 0x0400;
    pub const RIGHT_TRIGGER: u16 = 0x0800;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pad {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

/// Four controller ports; port 0 is the keyboard/mouse + first gamepad by default.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Input {
    pub pads: [Option<Pad>; 4],
    packet: [u32; 4],
}

impl Input {
    pub fn new() -> Self {
        // port 0 always connected so the game never stops at "connect a controller";
        // LMA_PADS=2..4 connects more (local multiplayer, #206)
        let n = std::env::var("LMA_PADS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 4);
        Self { pads: std::array::from_fn(|i| (i < n).then(Pad::default)), packet: [0; 4] }
    }

    pub fn set(&mut self, port: usize, pad: Pad) {
        if self.pads[port] != Some(pad) {
            self.packet[port] = self.packet[port].wrapping_add(1);
        }
        self.pads[port] = Some(pad);
    }

    /// XINPUT_STATE: packet number u32, then XINPUT_GAMEPAD (big-endian in guest memory).
    pub fn state_bytes(&self, port: usize) -> Option<[u8; 16]> {
        let p = self.pads.get(port).copied().flatten()?;
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&self.packet[port].to_be_bytes());
        let trig = button::LEFT_TRIGGER | button::RIGHT_TRIGGER;
        b[4..6].copy_from_slice(&(p.buttons & !trig).to_be_bytes());
        b[6] = if p.buttons & button::LEFT_TRIGGER != 0 { 255 } else { p.left_trigger };
        b[7] = if p.buttons & button::RIGHT_TRIGGER != 0 { 255 } else { p.right_trigger };
        b[8..10].copy_from_slice(&p.thumb_lx.to_be_bytes());
        b[10..12].copy_from_slice(&p.thumb_ly.to_be_bytes());
        b[12..14].copy_from_slice(&p.thumb_rx.to_be_bytes());
        b[14..16].copy_from_slice(&p.thumb_ry.to_be_bytes());
        Some(b)
    }
}

/// Default keyboard layout (host key name -> pad button). Mouse: left click = A,
/// right click = B, wheel = d-pad up/down; cursor movement drives menu focus in the app.
pub const DEFAULT_KEYBOARD: &[(&str, u16)] = &[
    ("Enter", button::A),
    ("Space", button::A),
    ("Escape", button::B),
    ("Backspace", button::B),
    ("KeyF", button::X),
    ("KeyR", button::Y),
    ("Tab", button::START),
    ("KeyV", button::BACK),
    ("KeyW", button::DPAD_UP),
    ("KeyA", button::DPAD_LEFT),
    ("KeyS", button::DPAD_DOWN),
    ("KeyD", button::DPAD_RIGHT),
    ("ArrowUp", button::DPAD_UP),
    ("ArrowDown", button::DPAD_DOWN),
    ("ArrowLeft", button::DPAD_LEFT),
    ("ArrowRight", button::DPAD_RIGHT),
    ("KeyQ", button::LEFT_SHOULDER),
    ("KeyE", button::RIGHT_SHOULDER),
    ("KeyZ", button::LEFT_THUMB),
    ("KeyC", button::RIGHT_THUMB),
];

/// Parse an `LMA_INPUT` script: `at:buttons:len` items separated by commas or newlines, each
/// pressing the hex button mask from `at` for `len` million instructions ("4900:10:100" = START
/// at 4.9B for 0.1B); `#` starts a comment. `@path` reads the script from a file (an lma-app
/// `LMA_RECORD` recording, for example). Returns controller 1's (clock, buttons from then on)
/// events sorted by clock; items for other controllers (`parse_script_ports`) are left out.
pub fn parse_script(s: &str) -> Result<Vec<(u64, u16)>, String> {
    Ok(parse_script_ports(s)?.into_iter().filter(|e| e.1 == 0).map(|(at, _, b)| (at, b)).collect())
}

/// `parse_script` for every controller: an item may name its controller as a fourth field,
/// `at:buttons:len:port` with port 1-4 (default 1), so a headless run can drive a two-player
/// career (#693). Returns (clock, port index 0-3, that port's buttons from then on), sorted.
pub fn parse_script_ports(s: &str) -> Result<Vec<(u64, usize, u16)>, String> {
    let text;
    let s = match s.trim().strip_prefix('@') {
        Some(path) => {
            text = std::fs::read_to_string(path).map_err(|e| format!("LMA_INPUT {path}: {e}"))?;
            text.as_str()
        }
        None => s,
    };
    let mut events = Vec::new();
    let items = s.lines().map(|l| l.split('#').next().unwrap_or("")).flat_map(|l| l.split(','));
    for item in items.map(str::trim).filter(|s| !s.is_empty()) {
        let bad = || format!("bad LMA_INPUT item {item:?} (want at:hexbuttons:len or at:hexbuttons:len:port, port 1-4)");
        let f: Vec<&str> = item.split(':').collect();
        let (at, b, len, port) = match f[..] {
            [at, b, len] => (at, b, len, 1),
            [at, b, len, port] => (at, b, len, port.parse::<usize>().ok().filter(|p| (1..=4).contains(p)).ok_or_else(bad)?),
            _ => return Err(bad()),
        };
        let (at, len): (u64, u64) = (at.parse().map_err(|_| bad())?, len.parse().map_err(|_| bad())?);
        events.push((at * 1_000_000, port - 1, u16::from_str_radix(b, 16).map_err(|_| bad())?));
        events.push(((at + len) * 1_000_000, port - 1, 0));
    }
    events.sort();
    Ok(events)
}

/// Buttons by name (A B X Y START BACK UP DOWN LEFT RIGHT LB RB LS RS, any case, joined with `+`)
/// or a hex mask written `0x...`.
pub fn parse_buttons(s: &str) -> Option<u16> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u16::from_str_radix(hex, 16).ok();
    }
    s.split('+').try_fold(0u16, |acc, name| {
        let b = match name.to_ascii_uppercase().as_str() {
            "A" => button::A,
            "B" => button::B,
            "X" => button::X,
            "Y" => button::Y,
            "START" => button::START,
            "BACK" => button::BACK,
            "UP" => button::DPAD_UP,
            "DOWN" => button::DPAD_DOWN,
            "LEFT" => button::DPAD_LEFT,
            "RIGHT" => button::DPAD_RIGHT,
            "LB" => button::LEFT_SHOULDER,
            "RB" => button::RIGHT_SHOULDER,
            "LS" => button::LEFT_THUMB,
            "RS" => button::RIGHT_THUMB,
            "LT" => button::LEFT_TRIGGER,
            "RT" => button::RIGHT_TRIGGER,
            _ => return None,
        };
        Some(acc | b)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script() {
        let ev = parse_script("5300:2:8, 4900:10:100").unwrap();
        assert_eq!(ev, vec![(4_900_000_000, 0x10), (5_000_000_000, 0), (5_300_000_000, 2), (5_308_000_000, 0)]);
        assert_eq!(parse_script("").unwrap(), vec![]);
        assert!(parse_script("4900:10").is_err());
        assert!(parse_script("4900:zz:8").is_err());
        // recordings: one item per line, comments
        let rec = "# lma-app recording\n4900:10:100\n5300:2:8 # A\n";
        assert_eq!(parse_script(rec).unwrap(), ev);
    }

    /// A fourth field names the controller (1-4); without it, controller 1. `parse_script` keeps
    /// to controller 1, so lma-app and lock-step runs see what they saw before (#693).
    #[test]
    fn script_ports() {
        let s = "4900:10:100, 5000:1000:8:2, 5000:2:8:1";
        let ev = parse_script_ports(s).unwrap();
        assert_eq!(ev, vec![(4_900_000_000, 0, 0x10), (5_000_000_000, 0, 0), (5_000_000_000, 0, 2), (5_000_000_000, 1, 0x1000), (5_008_000_000, 0, 0), (5_008_000_000, 1, 0)]);
        assert_eq!(parse_script(s).unwrap(), vec![(4_900_000_000, 0x10), (5_000_000_000, 0), (5_000_000_000, 2), (5_008_000_000, 0)]);
        assert!(parse_script_ports("5000:10:8:5").is_err(), "port 5");
        assert!(parse_script_ports("5000:10:8:0").is_err(), "ports count from 1");
        assert!(parse_script_ports("5000:10:8:2:9").is_err());
    }

    #[test]
    fn buttons() {
        assert_eq!(parse_buttons("start"), Some(button::START));
        assert_eq!(parse_buttons("A+down"), Some(button::A | button::DPAD_DOWN));
        assert_eq!(parse_buttons("0x1010"), Some(0x1010));
        assert_eq!(parse_buttons("Z"), None);
        assert_eq!(parse_buttons("lt+RT"), Some(button::LEFT_TRIGGER | button::RIGHT_TRIGGER));
    }

    /// The trigger pseudo-buttons reach the guest as full trigger values, not as button bits.
    #[test]
    fn trigger_pseudo_buttons() {
        let mut i = Input::default();
        i.pads[0] = Some(Pad { buttons: button::A | button::LEFT_TRIGGER, ..Default::default() });
        let b = i.state_bytes(0).unwrap();
        assert_eq!(u16::from_be_bytes([b[4], b[5]]), button::A);
        assert_eq!((b[6], b[7]), (255, 0));
        i.pads[0] = Some(Pad { buttons: button::RIGHT_TRIGGER, right_trigger: 7, ..Default::default() });
        let b = i.state_bytes(0).unwrap();
        assert_eq!((u16::from_be_bytes([b[4], b[5]]), b[6], b[7]), (0, 0, 255));
    }
}
