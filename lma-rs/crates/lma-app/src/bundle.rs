//! A macOS app bundle (`LMA.app`, `tools/macos/bundle.sh`; the maintainer, 22 Oct: "a normal game app: a
//! shortcut on the desktop and everything runs"). The executable sits in `Contents/MacOS` and the
//! release's `mods/` and `assets/` in `Contents/Resources`. A bundle must stay read-only (it is
//! signed, and may live in /Applications), so the game's own files (saves, states, userdata,
//! reports, logs) go to the config folder, `~/Library/Application Support/lma-rs`, where the
//! launcher's settings already are. That folder becomes the working directory, with a `mods` link
//! to the bundle's, so the code that reads `mods/` and writes `states/` runs unchanged.

use std::path::{Path, PathBuf};

/// The bundle's `Contents/Resources` when `exe_dir` is an app bundle's `Contents/MacOS` holding a
/// release (its `Resources/mods`), else None.
pub fn resources(exe_dir: &Path) -> Option<PathBuf> {
    let contents = exe_dir.parent()?;
    if exe_dir.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    let res = contents.join("Resources");
    res.join("mods").is_dir().then_some(res)
}

/// Make `data` the game's working folder for the bundle whose resources are `res`: created if
/// needed, with `data/mods` a link to the bundle's mods (re-pointed when the app has moved or been
/// replaced). A real `mods` folder there is the player's own and is left alone.
pub fn prepare(data: &Path, res: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(data)?;
    let link = data.join("mods");
    let target = res.join("mods");
    match std::fs::read_link(&link) {
        Ok(old) if old == target => return Ok(()),
        Ok(_) => std::fs::remove_file(&link)?,
        Err(_) if link.exists() => return Ok(()),
        Err(_) => {}
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link)?;
    Ok(())
}

/// Called first in `main`: inside an app bundle, work in the config folder (see the module notes).
/// Returns whether the game runs from a bundle.
pub fn enter() -> bool {
    let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else { return false };
    let Some(res) = resources(&exe_dir) else { return false };
    let Some(data) = crate::config::path().and_then(|p| p.parent().map(Path::to_path_buf)) else { return false };
    if let Err(e) = prepare(&data, &res).and_then(|_| std::env::set_current_dir(&data)) {
        eprintln!("app bundle: can't use {}: {e}", data.display());
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lma-bundle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_bundle_is_contents_macos_beside_resources_with_mods() {
        let d = temp("res");
        let macos = d.join("LMA.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        assert_eq!(resources(&macos), None, "no Resources/mods: not a release bundle");
        std::fs::create_dir_all(d.join("LMA.app/Contents/Resources/mods")).unwrap();
        assert_eq!(resources(&macos), Some(d.join("LMA.app/Contents/Resources")));
        std::fs::create_dir_all(d.join("plain")).unwrap();
        assert_eq!(resources(&d.join("plain")), None, "a release folder is not a bundle");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn the_data_folder_links_the_bundles_mods_and_follows_a_moved_app() {
        let d = temp("data");
        let (a, b, data) = (d.join("a/Resources"), d.join("b/Resources"), d.join("data"));
        std::fs::create_dir_all(a.join("mods")).unwrap();
        std::fs::create_dir_all(b.join("mods")).unwrap();
        prepare(&data, &a).unwrap();
        assert_eq!(std::fs::read_link(data.join("mods")).unwrap(), a.join("mods"));
        prepare(&data, &b).unwrap();
        assert_eq!(std::fs::read_link(data.join("mods")).unwrap(), b.join("mods"), "the app moved: re-pointed");
        std::fs::remove_file(data.join("mods")).unwrap();
        std::fs::create_dir_all(data.join("mods")).unwrap();
        prepare(&data, &a).unwrap();
        assert!(std::fs::read_link(data.join("mods")).is_err(), "the player's own mods folder is kept");
        let _ = std::fs::remove_dir_all(&d);
    }
}
