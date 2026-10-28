//! Keyboard bindings for controller port 1 (#4): a PC layout by default, changed per button in
//! the config file's `[keys]` table. A button listed there gets exactly the keys given; the others
//! keep their defaults. Keys use bevy's names (`KeyW`, `ArrowUp`, `Enter`, `F2`, `Numpad8`, ...),
//! or a single letter or digit:
//!
//! ```toml
//! [keys]
//! A = ["Enter", "J"]
//! B = ["Escape", "K"]
//! UP = ["I", "ArrowUp"]
//! ```
//!
//! Buttons: A B X Y START BACK UP DOWN LEFT RIGHT LB RB LT RT LS RS (LB/RB the shoulder buttons,
//! LT/RT the triggers pressed fully, LS/RS the stick clicks).

use bevy::input::keyboard::KeyCode;
use runtime::input::button;
use std::collections::BTreeMap;

/// Config names of the controller buttons a key can press.
pub const BUTTONS: [(&str, u16); 16] = [
    ("A", button::A),
    ("B", button::B),
    ("X", button::X),
    ("Y", button::Y),
    ("START", button::START),
    ("BACK", button::BACK),
    ("UP", button::DPAD_UP),
    ("DOWN", button::DPAD_DOWN),
    ("LEFT", button::DPAD_LEFT),
    ("RIGHT", button::DPAD_RIGHT),
    ("LB", button::LEFT_SHOULDER),
    ("RB", button::RIGHT_SHOULDER),
    ("LT", button::LEFT_TRIGGER),
    ("RT", button::RIGHT_TRIGGER),
    ("LS", button::LEFT_THUMB),
    ("RS", button::RIGHT_THUMB),
];

/// The default layout, not letters matching the pad: WASD / arrows move, Enter / Space confirm,
/// Esc / Backspace go back, Q / E switch tabs, 1 / 3 are the triggers (sub-pages; the maintainer, 11 Oct:
/// the keyboard had none), Z / C are the stick clicks (green L / R prompts).
pub const DEFAULT: [(KeyCode, u16); 22] = [
    (KeyCode::Enter, button::A),
    (KeyCode::Space, button::A),
    (KeyCode::Escape, button::B),
    (KeyCode::Backspace, button::B),
    (KeyCode::KeyF, button::X),
    (KeyCode::KeyR, button::Y),
    (KeyCode::Tab, button::START),
    (KeyCode::KeyV, button::BACK),
    (KeyCode::KeyW, button::DPAD_UP),
    (KeyCode::KeyA, button::DPAD_LEFT),
    (KeyCode::KeyS, button::DPAD_DOWN),
    (KeyCode::KeyD, button::DPAD_RIGHT),
    (KeyCode::ArrowUp, button::DPAD_UP),
    (KeyCode::ArrowDown, button::DPAD_DOWN),
    (KeyCode::ArrowLeft, button::DPAD_LEFT),
    (KeyCode::ArrowRight, button::DPAD_RIGHT),
    (KeyCode::KeyQ, button::LEFT_SHOULDER),
    (KeyCode::KeyE, button::RIGHT_SHOULDER),
    (KeyCode::Digit1, button::LEFT_TRIGGER),
    (KeyCode::Digit3, button::RIGHT_TRIGGER),
    (KeyCode::KeyZ, button::LEFT_THUMB),
    (KeyCode::KeyC, button::RIGHT_THUMB),
];

/// The app's own keys (save, load, turbo, frame cap, bug report), which can't press a controller button.
pub const RESERVED: [KeyCode; 5] = [KeyCode::F1, KeyCode::F3, KeyCode::F5, KeyCode::F9, KeyCode::F12];

/// Keys a binding may name. Ctrl and Cmd are left out: with them a key is an app hotkey.
#[rustfmt::skip]
const KEYS: &[KeyCode] = &[
    KeyCode::KeyA, KeyCode::KeyB, KeyCode::KeyC, KeyCode::KeyD, KeyCode::KeyE, KeyCode::KeyF, KeyCode::KeyG,
    KeyCode::KeyH, KeyCode::KeyI, KeyCode::KeyJ, KeyCode::KeyK, KeyCode::KeyL, KeyCode::KeyM, KeyCode::KeyN,
    KeyCode::KeyO, KeyCode::KeyP, KeyCode::KeyQ, KeyCode::KeyR, KeyCode::KeyS, KeyCode::KeyT, KeyCode::KeyU,
    KeyCode::KeyV, KeyCode::KeyW, KeyCode::KeyX, KeyCode::KeyY, KeyCode::KeyZ,
    KeyCode::Digit0, KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4,
    KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9,
    KeyCode::Numpad0, KeyCode::Numpad1, KeyCode::Numpad2, KeyCode::Numpad3, KeyCode::Numpad4,
    KeyCode::Numpad5, KeyCode::Numpad6, KeyCode::Numpad7, KeyCode::Numpad8, KeyCode::Numpad9,
    KeyCode::NumpadEnter, KeyCode::NumpadAdd, KeyCode::NumpadSubtract, KeyCode::NumpadMultiply,
    KeyCode::NumpadDivide, KeyCode::NumpadDecimal,
    KeyCode::F2, KeyCode::F4, KeyCode::F6, KeyCode::F7, KeyCode::F8, KeyCode::F10, KeyCode::F11,
    KeyCode::ArrowUp, KeyCode::ArrowDown, KeyCode::ArrowLeft, KeyCode::ArrowRight,
    KeyCode::Enter, KeyCode::Space, KeyCode::Escape, KeyCode::Backspace, KeyCode::Tab, KeyCode::Delete,
    KeyCode::Insert, KeyCode::Home, KeyCode::End, KeyCode::PageUp, KeyCode::PageDown,
    KeyCode::ShiftLeft, KeyCode::ShiftRight, KeyCode::AltLeft, KeyCode::AltRight,
    KeyCode::Comma, KeyCode::Period, KeyCode::Slash, KeyCode::Semicolon, KeyCode::Quote,
    KeyCode::BracketLeft, KeyCode::BracketRight, KeyCode::Backslash, KeyCode::Minus, KeyCode::Equal,
    KeyCode::Backquote,
];

/// A key by bevy's name (any case), or a single letter or digit.
pub fn key(name: &str) -> Option<KeyCode> {
    let n = name.trim();
    let n = match n.len() {
        1 if n.as_bytes()[0].is_ascii_alphabetic() => format!("Key{}", n.to_ascii_uppercase()),
        1 if n.as_bytes()[0].is_ascii_digit() => format!("Digit{n}"),
        _ => n.to_string(),
    };
    KEYS.iter().copied().find(|k| format!("{k:?}").eq_ignore_ascii_case(&n))
}

/// The keyboard map: the defaults with each button in `bindings` replaced by its keys. Also returns
/// what was ignored (unknown buttons and keys), for the log.
pub fn map(bindings: &BTreeMap<String, Vec<String>>) -> (Vec<(KeyCode, u16)>, Vec<String>) {
    let mut map: Vec<(KeyCode, u16)> = DEFAULT.to_vec();
    let mut ignored = Vec::new();
    for (name, keys) in bindings {
        let Some(&(_, b)) = BUTTONS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) else {
            ignored.push(format!("unknown button {name} (buttons: {})", BUTTONS.map(|b| b.0).join(" ")));
            continue;
        };
        map.retain(|&(_, mb)| mb != b);
        for k in keys {
            match key(k) {
                Some(code) => map.push((code, b)),
                None if RESERVED.iter().any(|r| format!("{r:?}").eq_ignore_ascii_case(k.trim())) => ignored.push(format!("{name}: {k} is an app key (F1/F3/F5/F9/F12)")),
                None => ignored.push(format!("{name}: unknown key {k}")),
            }
        }
    }
    (map, ignored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs.iter().map(|(b, ks)| (b.to_string(), ks.iter().map(|k| k.to_string()).collect())).collect()
    }

    #[test]
    fn key_names() {
        assert_eq!(key("KeyW"), Some(KeyCode::KeyW));
        assert_eq!(key("w"), Some(KeyCode::KeyW));
        assert_eq!(key("7"), Some(KeyCode::Digit7));
        assert_eq!(key("arrowup"), Some(KeyCode::ArrowUp));
        assert_eq!(key(" Numpad8 "), Some(KeyCode::Numpad8));
        assert_eq!(key("F5"), None, "reserved for save states");
        assert_eq!(key("ControlLeft"), None, "Ctrl makes app hotkeys");
        assert_eq!(key("Nope"), None);
    }

    #[test]
    fn no_bindings_is_the_default_layout() {
        let (map, ignored) = map(&BTreeMap::new());
        assert_eq!(map, DEFAULT.to_vec());
        assert!(ignored.is_empty());
    }

    /// The triggers: 1 and 3 by default (the menus' sub-pages; the maintainer, 11 Oct), bindable as LT / RT.
    #[test]
    fn triggers_are_on_1_and_3_and_bindable() {
        let (defaults, _) = map(&bind(&[]));
        let keys_of = |m: &Vec<(KeyCode, u16)>, b: u16| m.iter().filter(|x| x.1 == b).map(|x| x.0).collect::<Vec<_>>();
        assert_eq!(keys_of(&defaults, button::LEFT_TRIGGER), vec![KeyCode::Digit1]);
        assert_eq!(keys_of(&defaults, button::RIGHT_TRIGGER), vec![KeyCode::Digit3]);
        let (bound, ignored) = map(&bind(&[("lt", &["ShiftLeft"]), ("RT", &["ShiftRight", "0"])]));
        assert_eq!(keys_of(&bound, button::LEFT_TRIGGER), vec![KeyCode::ShiftLeft]);
        assert_eq!(keys_of(&bound, button::RIGHT_TRIGGER), vec![KeyCode::ShiftRight, KeyCode::Digit0]);
        assert!(ignored.is_empty(), "{ignored:?}");
    }

    #[test]
    fn a_listed_button_gets_exactly_its_keys() {
        let (map, ignored) = map(&bind(&[("a", &["J", "Enter"]), ("UP", &["I"])]));
        let keys_of = |b: u16| map.iter().filter(|m| m.1 == b).map(|m| m.0).collect::<Vec<_>>();
        assert_eq!(keys_of(button::A), vec![KeyCode::KeyJ, KeyCode::Enter], "Space no longer confirms");
        assert_eq!(keys_of(button::DPAD_UP), vec![KeyCode::KeyI]);
        // the rest keep their defaults
        assert_eq!(keys_of(button::B), vec![KeyCode::Escape, KeyCode::Backspace]);
        assert_eq!(keys_of(button::DPAD_DOWN), vec![KeyCode::KeyS, KeyCode::ArrowDown]);
        assert!(ignored.is_empty());
    }

    #[test]
    fn mistakes_are_reported_not_fatal() {
        let (map, ignored) = map(&bind(&[("JUMP", &["Space"]), ("B", &["Esc", "F5", "K"])]));
        assert_eq!(ignored.len(), 3, "{ignored:?}");
        for what in ["unknown button JUMP", "unknown key Esc", "F5 is an app key"] {
            assert!(ignored.iter().any(|i| i.contains(what)), "{what}: {ignored:?}");
        }
        // what could be read still applies
        assert_eq!(map.iter().filter(|m| m.1 == button::B).map(|m| m.0).collect::<Vec<_>>(), vec![KeyCode::KeyK]);
    }
}
