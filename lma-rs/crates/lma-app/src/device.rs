//! The input device in use (#694): the last source that pressed something, so the app's hints and
//! the game's button glyphs can follow it. `track` runs before the launcher and the game input each
//! update: a key or mouse press makes it the keyboard, a gamepad button or a stick past half way
//! makes it that pad's kind, which bevy's vendor id tells (Sony's DualShock / DualSense, Microsoft's
//! and most third-party Xbox pads, anything else a generic pad with Xbox labels).
use bevy::prelude::*;

/// What the person last used.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum InputKind {
    /// Nothing pressed yet: hints name both the key and the button.
    #[default]
    Unknown,
    Keyboard,
    Xbox,
    PlayStation,
    /// A gamepad of another make: Xbox labels and glyphs.
    Pad,
}

/// USB vendor ids: Sony and Microsoft.
const SONY: u16 = 0x054c;
const MICROSOFT: u16 = 0x045e;

impl InputKind {
    /// The kind of a gamepad from its USB ids (bevy's `Gamepad::vendor_id` / `product_id`).
    pub fn of_gamepad(vendor: Option<u16>, _product: Option<u16>) -> Self {
        match vendor {
            Some(SONY) => Self::PlayStation,
            Some(MICROSOFT) => Self::Xbox,
            _ => Self::Pad,
        }
    }

    /// The launcher's last line: how to move and how to choose.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Unknown => "Up / Down move, Enter / A choose",
            Self::Keyboard => "Up / Down move, Enter choose",
            Self::Xbox | Self::Pad => "D-pad move, A choose",
            Self::PlayStation => "D-pad move, Cross choose",
        }
    }

    /// The name of the game's A / B / X / Y buttons on this device, in that order.
    pub fn face_buttons(self) -> [&'static str; 4] {
        match self {
            Self::PlayStation => ["Cross", "Circle", "Square", "Triangle"],
            _ => ["A", "B", "X", "Y"],
        }
    }
}

/// Follow the last input: keys and mouse buttons just pressed, gamepad buttons just pressed, the
/// left stick past half way.
pub fn track(keys: Res<ButtonInput<KeyCode>>, mouse: Res<ButtonInput<MouseButton>>, gamepads: Query<&Gamepad>, mut kind: ResMut<InputKind>) {
    let mut next = None;
    if keys.get_just_pressed().next().is_some() || mouse.get_just_pressed().next().is_some() {
        next = Some(InputKind::Keyboard);
    }
    for gp in &gamepads {
        let stick = gp.left_stick();
        if gp.get_just_pressed().next().is_some() || stick.x.abs() > 0.5 || stick.y.abs() > 0.5 {
            next = Some(InputKind::of_gamepad(gp.vendor_id(), gp.product_id()));
        }
    }
    if let Some(k) = next {
        if *kind != k {
            *kind = k;
        }
    }
}

/// The game's button glyphs follow the device (#694): a PlayStation pad installs the DualSense set
/// (`LMA_GLYPHS_PS5=<dir>`, else `assets/glyphs/ps5` next to the executable, else
/// `lma-rs/assets/glyphs/ps5` under the working directory), anything else removes it. The renderer
/// decodes the glyph textures again on the next frame (`runtime::tex_override`). With
/// `LMA_TEX_OVERRIDES` set the table is the user's and this leaves it alone.
/// Whether the running version gets this port's conveniences (the pad icons following the device):
/// set by the launcher when the game starts; false only for the original game exactly as shipped
/// (`launcher::enhanced`). Runs started with arguments keep the default, on.
pub static ENHANCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn glyphs(kind: Res<InputKind>, mut last: Local<Option<(InputKind, bool)>>) {
    let enhanced = ENHANCED.load(std::sync::atomic::Ordering::Relaxed);
    if *last == Some((*kind, enhanced)) || std::env::var_os("LMA_TEX_OVERRIDES").is_some() {
        return;
    }
    *last = Some((*kind, enhanced));
    // the original game as shipped keeps its own (Xbox) icons
    let set = match *kind {
        InputKind::PlayStation if enhanced => Some("ps5"),
        InputKind::Keyboard if enhanced => Some("keyboard"),
        _ => None,
    };
    let dir = set.and_then(glyphs_dir);
    match runtime::tex_override::set(dir.as_deref()) {
        Ok(n) if n > 0 => eprintln!("glyphs: {} set, {n} images", set.unwrap_or("")),
        Ok(_) => {}
        Err(e) => eprintln!("glyphs: {e}"),
    }
}

/// Where the DualSense glyph PNGs are, if anywhere.
/// A glyph set's folder (`ps5`, `keyboard`): `LMA_GLYPHS_PS5` / `LMA_GLYPHS_KEYBOARD`, then
/// `assets/glyphs/<set>` beside the executable or the file a symlink to it names (a release),
/// then the checkout's.
pub(crate) fn glyphs_dir(set: &str) -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = std::env::var_os(format!("LMA_GLYPHS_{}", set.to_uppercase())).map(std::path::PathBuf::from).into_iter().collect();
    // the folder the exe was started from and, through a symlink (the maintainer's launchers link to
    // builds/<hash>/lma-app; macOS reports the link's path), the one it really is in
    let exe = std::env::current_exe().ok();
    for e in [exe.clone(), exe.and_then(|e| std::fs::canonicalize(e).ok())].into_iter().flatten() {
        if let Some(d) = e.parent() {
            candidates.push(d.join("assets").join("glyphs").join(set));
            // LMA.app (macOS): Contents/MacOS/lma-app, the release's assets in Contents/Resources
            if let Some(contents) = d.parent() {
                candidates.push(contents.join("Resources").join("assets").join("glyphs").join(set));
            }
        }
    }
    candidates.push(std::path::PathBuf::from("lma-rs/assets/glyphs").join(set));
    candidates.into_iter().find(|p| p.is_dir())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_vendor_ids_and_their_hints() {
        assert_eq!(InputKind::of_gamepad(Some(0x054c), Some(0x0ce6)), InputKind::PlayStation, "DualSense");
        assert_eq!(InputKind::of_gamepad(Some(0x054c), Some(0x05c4)), InputKind::PlayStation, "DualShock 4");
        assert_eq!(InputKind::of_gamepad(Some(0x045e), Some(0x0b12)), InputKind::Xbox, "Xbox Series pad");
        assert_eq!(InputKind::of_gamepad(Some(0x2dc8), None), InputKind::Pad, "8BitDo: Xbox labels");
        assert_eq!(InputKind::of_gamepad(None, None), InputKind::Pad);
        assert_eq!(InputKind::default().hint(), "Up / Down move, Enter / A choose");
        assert_eq!(InputKind::Keyboard.hint(), "Up / Down move, Enter choose");
        assert_eq!(InputKind::PlayStation.hint(), "D-pad move, Cross choose");
        assert_eq!(InputKind::Pad.hint(), InputKind::Xbox.hint());
        assert_eq!(InputKind::PlayStation.face_buttons(), ["Cross", "Circle", "Square", "Triangle"]);
        assert_eq!(InputKind::Xbox.face_buttons(), ["A", "B", "X", "Y"]);
    }
}
