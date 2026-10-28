//! Saved-game content packages (XamContentCreateEx / XamEnumerate / XamContentDelete).
//!
//! A package's files live wherever its root (e.g. `save:`) maps in the VFS, so the game reads and
//! writes them as plain files. What the console also keeps is the list of packages on the device:
//! "load game" enumerates it. That list is `content.idx` in the user folder, one package per line:
//! `type<TAB>file name<TAB>display name`.

use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Package {
    /// XCONTENTTYPE_* (1 = saved game).
    pub kind: u32,
    /// `szFileName`: the package's name on the device.
    pub file: String,
    /// `szDisplayName`, shown by the game's load menu.
    pub display: String,
}

pub fn load(index: &Path) -> Vec<Package> {
    let Ok(text) = std::fs::read_to_string(index) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let kind = f.next()?.parse().ok()?;
            let file = f.next()?.to_string();
            let display = f.next().unwrap_or("").to_string();
            Some(Package { kind, file, display })
        })
        .collect()
}

fn store(index: &Path, list: &[Package]) -> std::io::Result<()> {
    if let Some(dir) = index.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = list.iter().map(|p| format!("{}\t{}\t{}\n", p.kind, p.file, p.display.replace(['\t', '\n'], " "))).collect();
    std::fs::write(index, text)
}

/// Remember a package (a newer display name replaces the old one; opening a package to load it
/// passes no display name, which keeps the one it has).
pub fn add(index: &Path, mut p: Package) -> std::io::Result<()> {
    let mut list = load(index);
    match list.iter_mut().find(|q| q.kind == p.kind && q.file.eq_ignore_ascii_case(&p.file)) {
        Some(q) if p.display.is_empty() => {
            p.display = q.display.clone();
            if *q == p {
                return Ok(());
            }
            *q = p
        }
        Some(q) if *q == p => return Ok(()),
        Some(q) => *q = p,
        None => list.push(p),
    }
    store(index, &list)
}

/// Forget a package (its files stay on disk).
pub fn remove(index: &Path, kind: u32, file: &str) -> std::io::Result<()> {
    let mut list = load(index);
    let n = list.len();
    list.retain(|q| !(q.kind == kind && q.file.eq_ignore_ascii_case(file)));
    if list.len() == n { Ok(()) } else { store(index, &list) }
}

/// The folder (relative to the user folder) holding a package's files.
pub fn folder(file: &str) -> String {
    let name: String = file.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    format!("content/{}", if name.is_empty() { "_" } else { &name })
}

/// Saves made before packages had their own folders: the files of the first career slot (`SG01`)
/// sat directly in `<user>/save/`. Offered to "load game" until imported.
const LEGACY_PACKAGE: &str = "SG01";
const LEGACY_MARK: &str = "content/.legacy-save-imported";

pub fn legacy(user: &Path, listed: &[Package]) -> Option<Package> {
    let unimported = !user.join(LEGACY_MARK).exists() && user.join("save").join("Career").is_file();
    let listed = listed.iter().any(|p| p.kind == 1 && p.file.eq_ignore_ascii_case(LEGACY_PACKAGE));
    (unimported && !listed).then(|| Package { kind: 1, file: LEGACY_PACKAGE.into(), display: "Career 01: earlier save".into() })
}

/// Opening the first career slot at root `save` for the first time: copy the legacy save into
/// the package's folder (the old files stay where they were).
pub fn import_legacy(user: &Path, root: &str, file: &str) -> std::io::Result<()> {
    let (from, to) = (user.join("save"), user.join(folder(file)));
    if !root.eq_ignore_ascii_case("save") || !file.eq_ignore_ascii_case(LEGACY_PACKAGE) || user.join(LEGACY_MARK).exists() || to.exists() || !from.is_dir() {
        return Ok(());
    }
    copy_dir(&from, &to)?;
    std::fs::write(user.join(LEGACY_MARK), b"save/ copied into content/SG01\n")
}

pub(crate) fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() { copy_dir(&e.path(), &dest)? } else { std::fs::copy(e.path(), dest).map(|_| ())? }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_replace_remove() {
        let dir = std::env::temp_dir().join(format!("lma-content-test-{}", std::process::id()));
        let idx = dir.join("content.idx");
        assert!(load(&idx).is_empty());
        add(&idx, Package { kind: 1, file: "Career".into(), display: "Arsenal".into() }).unwrap();
        add(&idx, Package { kind: 1, file: "Options".into(), display: "".into() }).unwrap();
        add(&idx, Package { kind: 1, file: "career".into(), display: "Arsenal 2".into() }).unwrap();
        let l = load(&idx);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].display, "Arsenal 2");
        remove(&idx, 1, "OPTIONS").unwrap();
        assert_eq!(load(&idx).len(), 1);
        // opening it again to load it passes no display name: the name stays
        add(&idx, Package { kind: 1, file: "CAREER".into(), display: "".into() }).unwrap();
        assert_eq!(load(&idx)[0].display, "Arsenal 2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_save_is_listed_then_imported_once() {
        let user = std::env::temp_dir().join(format!("lma-content-legacy-{}", std::process::id()));
        std::fs::create_dir_all(user.join("save")).unwrap();
        std::fs::write(user.join("save/Career"), b"career").unwrap();
        let l = legacy(&user, &[]).unwrap();
        assert_eq!((l.kind, l.file.as_str()), (1, "SG01"));
        import_legacy(&user, "save", "SG02").unwrap(); // another slot: nothing happens
        assert!(!user.join("content").exists());
        import_legacy(&user, "save", "SG01").unwrap();
        assert_eq!(std::fs::read(user.join("content/SG01/Career")).unwrap(), b"career");
        assert!(user.join("save/Career").exists(), "the old files stay");
        assert!(legacy(&user, &[]).is_none(), "imported: not offered again");
        std::fs::write(user.join("content/SG01/Career"), b"newer").unwrap();
        import_legacy(&user, "save", "SG01").unwrap(); // never overwrites
        assert_eq!(std::fs::read(user.join("content/SG01/Career")).unwrap(), b"newer");
        let _ = std::fs::remove_dir_all(&user);
    }
}
