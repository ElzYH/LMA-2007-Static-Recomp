//! Guest file system: disc paths map to the extracted game folder (read-only),
//! cache/partition/save paths map to a writable per-user folder. Overlays replace single disc
//! files with bytes built in memory (a mod space's database), leaving the game folder untouched.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const STATUS_SUCCESS: u32 = 0;
pub const STATUS_NO_SUCH_FILE: u32 = 0xC000_000F;
pub const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
pub const STATUS_END_OF_FILE: u32 = 0xC000_0011;
pub const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
pub const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
pub const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;

pub struct OpenFile {
    pub guest_path: String,
    pub host: PathBuf,
    pub file: Option<File>, // None for directories and overlays
    pub mem: Option<Arc<Vec<u8>>>,
    pub pos: u64,
    pub dir_index: usize,
}

/// Open guest files for a save state: (handle, guest path, host path, position, dir index, is file).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VfsState {
    files: Vec<(u32, String, PathBuf, u64, usize, bool)>,
    next: u32,
}

pub struct Vfs {
    game: PathBuf,
    user: PathBuf,
    handles: HashMap<u32, OpenFile>,
    next: u32,
    /// Disc files replaced in memory, keyed by their resolved host path.
    overlay: HashMap<PathBuf, Arc<Vec<u8>>>,
    /// Content packages mounted by XamContentCreateEx: guest root (lower case, no colon) ->
    /// folder relative to the user folder. Each package keeps its own files.
    mounts: Vec<(String, String)>,
}

impl Vfs {
    pub fn save_state(&self) -> VfsState {
        let mut files: Vec<_> = self.handles.iter().map(|(&h, f)| (h, f.guest_path.clone(), f.host.clone(), f.pos, f.dir_index, f.file.is_some() || f.mem.is_some())).collect();
        files.sort_unstable_by_key(|f| f.0); // the same bytes for the same open files (a hash map's order differs per process)
        VfsState { files, next: self.next }
    }

    /// Reopen the files a save state had open (game files read-only, user files read/write).
    /// Paths are resolved again from the guest path: the host path stored in the state belongs
    /// to the machine and folders that saved it.
    pub fn load_state(&mut self, s: VfsState) -> std::io::Result<()> {
        self.handles.clear();
        self.next = s.next;
        for (h, guest_path, _saved_host, pos, dir_index, is_file) in s.files {
            let (host, writable) = self.resolve(&guest_path);
            let mem = self.overlay.get(&host).cloned();
            let file = if is_file && mem.is_none() {
                let f = std::fs::OpenOptions::new().read(true).write(writable).open(&host);
                Some(f.map_err(|e| std::io::Error::new(e.kind(), format!("{guest_path} ({}): {e}", host.display())))?)
            } else {
                None
            };
            self.handles.insert(h, OpenFile { guest_path, host, file, mem, pos, dir_index });
        }
        Ok(())
    }

    pub fn new(game: impl Into<PathBuf>, user: impl Into<PathBuf>) -> Self {
        Self { game: game.into(), user: user.into(), handles: HashMap::new(), next: 0x1000, overlay: HashMap::new(), mounts: Vec::new() }
    }

    /// The user folder (saves, cache, the content index).
    /// The game's folder.
    pub fn game_dir(&self) -> &Path {
        &self.game
    }

    pub fn user_dir(&self) -> &Path {
        &self.user
    }

    /// Mount a content package: guest paths under `root:` go to `folder` (relative to the user
    /// folder) until `unmount`. Mounting a root again replaces the package.
    pub fn mount(&mut self, root: &str, folder: &str) {
        let root = root.trim_end_matches(':').to_ascii_lowercase();
        self.mounts.retain(|(r, _)| *r != root);
        self.mounts.push((root, folder.to_string()));
    }

    /// Mounted packages for a save state (appended to it; restore before `load_state`).
    pub fn mounts(&self) -> &[(String, String)] {
        &self.mounts
    }

    pub fn set_mounts(&mut self, mounts: Vec<(String, String)>) {
        self.mounts = mounts;
    }

    pub fn unmount(&mut self, root: &str) {
        let root = root.trim_end_matches(':').to_ascii_lowercase();
        self.mounts.retain(|(r, _)| *r != root);
    }

    /// Serve `data` whenever the game opens the disc file `guest` (any case or spelling).
    pub fn add_overlay(&mut self, guest: &str, data: Vec<u8>) {
        let (host, _) = self.resolve(guest);
        self.overlay.insert(host, Arc::new(data));
    }

    /// Resolve a guest path to (host path, writable). Lookup is case-insensitive.
    pub fn resolve(&self, guest: &str) -> (PathBuf, bool) {
        let g = guest.replace('/', "\\");
        let lower = g.to_ascii_lowercase();
        let disc = ["game:", "d:", "\\device\\cdrom0", "\\??\\game:", "\\??\\d:"];
        for p in disc {
            if let Some(rest) = lower.strip_prefix(p) {
                let rest = &g[g.len() - rest.len()..];
                return (case_insensitive(&self.game, rest), false);
            }
        }
        // a mounted content package: its own folder
        let bare = lower.strip_prefix("\\??\\").unwrap_or(&lower);
        for (root, folder) in &self.mounts {
            if let Some(rest) = bare.strip_prefix(root.as_str()).and_then(|r| r.strip_prefix(':')) {
                let rest = &g[g.len() - rest.len()..];
                return (self.user.join(folder).join(rest.trim_start_matches('\\').replace('\\', "/")), true);
            }
        }
        // everything else (cache:, \Device\Harddisk0\..., partitions, saves) lives in the user folder
        let rel: String = g.trim_start_matches('\\').replace(':', "").to_string();
        (self.user.join(rel.replace('\\', "/")), true)
    }

    pub fn open(&mut self, guest: &str, write: bool, create: bool) -> Result<u32, u32> {
        let (host, writable) = self.resolve(guest);
        if write && !writable {
            return Err(STATUS_ACCESS_DENIED);
        }
        let mem = self.overlay.get(&host).cloned();
        let file = if mem.is_some() || host.is_dir() {
            None
        } else if host.is_file() || create {
            if create {
                if let Some(parent) = host.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
            let f = OpenOptions::new().read(true).write(write).create(create && write).open(&host);
            match f {
                Ok(f) => Some(f),
                Err(_) if create && !write => {
                    let _ = std::fs::create_dir_all(&host);
                    None
                }
                Err(_) => return Err(STATUS_OBJECT_NAME_NOT_FOUND),
            }
        } else if create {
            let _ = std::fs::create_dir_all(&host);
            None
        } else {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        };
        let h = self.next;
        self.next += 4;
        self.handles.insert(h, OpenFile { guest_path: guest.to_string(), host, file, mem, pos: 0, dir_index: 0 });
        Ok(h)
    }

    pub fn get(&mut self, h: u32) -> Option<&mut OpenFile> {
        self.handles.get_mut(&h)
    }

    pub fn close(&mut self, h: u32) -> bool {
        self.handles.remove(&h).is_some()
    }

    pub fn read(&mut self, h: u32, buf: &mut [u8], offset: Option<u64>) -> Result<usize, u32> {
        let f = self.handles.get_mut(&h).ok_or(STATUS_INVALID_HANDLE)?;
        let pos = offset.unwrap_or(f.pos);
        if let Some(mem) = &f.mem {
            let start = (pos as usize).min(mem.len());
            let n = buf.len().min(mem.len() - start);
            buf[..n].copy_from_slice(&mem[start..start + n]);
            f.pos = pos + n as u64;
            return if n == 0 && !buf.is_empty() { Err(STATUS_END_OF_FILE) } else { Ok(n) };
        }
        let file = f.file.as_mut().ok_or(STATUS_INVALID_HANDLE)?;
        file.seek(SeekFrom::Start(pos)).map_err(|_| STATUS_END_OF_FILE)?;
        let mut n = 0;
        while n < buf.len() {
            match file.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => break,
            }
        }
        f.pos = pos + n as u64;
        if n == 0 && !buf.is_empty() { Err(STATUS_END_OF_FILE) } else { Ok(n) }
    }

    pub fn write(&mut self, h: u32, data: &[u8], offset: Option<u64>) -> Result<usize, u32> {
        let f = self.handles.get_mut(&h).ok_or(STATUS_INVALID_HANDLE)?;
        let file = f.file.as_mut().ok_or(STATUS_INVALID_HANDLE)?;
        let pos = offset.unwrap_or(f.pos);
        file.seek(SeekFrom::Start(pos)).map_err(|_| STATUS_ACCESS_DENIED)?;
        file.write_all(data).map_err(|_| STATUS_ACCESS_DENIED)?;
        f.pos = pos + data.len() as u64;
        Ok(data.len())
    }

    pub fn size(&mut self, h: u32) -> Option<(u64, bool)> {
        let f = self.handles.get(&h)?;
        if let Some(mem) = &f.mem {
            return Some((mem.len() as u64, false));
        }
        let md = std::fs::metadata(&f.host).ok()?;
        Some((if md.is_dir() { 0 } else { md.len() }, md.is_dir()))
    }

    pub fn stat(&self, guest: &str) -> Option<(u64, bool)> {
        let host = self.resolve(guest).0;
        if let Some(mem) = self.overlay.get(&host) {
            return Some((mem.len() as u64, false));
        }
        let md = std::fs::metadata(host).ok()?;
        Some((if md.is_dir() { 0 } else { md.len() }, md.is_dir()))
    }
}

/// Join `rel` (backslash separated) onto `root`, matching each component case-insensitively
/// so the game's upper-case paths (`\COMMON\GAMEDATA\...`) find `Common/GameData/...`.
fn case_insensitive(root: &Path, rel: &str) -> PathBuf {
    let mut cur = root.to_path_buf();
    for comp in rel.split('\\').filter(|c| !c.is_empty()) {
        let found = std::fs::read_dir(&cur).ok().and_then(|rd| {
            rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(comp)).map(|e| e.path())
        });
        cur = found.unwrap_or_else(|| cur.join(comp));
    }
    cur
}

impl Vfs {
    /// Next directory entry matching `mask` (`*` wildcards): (name, size, is_dir).
    pub fn next_entry(&mut self, h: u32, mask: &str, restart: bool) -> Result<Option<(String, u64, bool)>, u32> {
        let f = self.handles.get_mut(&h).ok_or(STATUS_INVALID_HANDLE)?;
        if restart {
            f.dir_index = 0;
        }
        let mut entries: Vec<_> = std::fs::read_dir(&f.host)
            .map_err(|_| STATUS_INVALID_HANDLE)?
            .flatten()
            .map(|e| {
                let md = e.metadata().ok();
                let size = self.overlay.get(&e.path()).map_or(md.as_ref().map_or(0, |m| m.len()), |mem| mem.len() as u64);
                (e.file_name().to_string_lossy().into_owned(), size, md.is_some_and(|m| m.is_dir()))
            })
            .filter(|(n, _, _)| wildcard(mask, n))
            .collect();
        entries.sort();
        let e = entries.into_iter().nth(f.dir_index);
        f.dir_index += 1;
        Ok(e)
    }
}

fn wildcard(mask: &str, name: &str) -> bool {
    let (m, n) = (mask.to_ascii_lowercase(), name.to_ascii_lowercase());
    if m.is_empty() || m == "*" || m == "*.*" {
        return true;
    }
    match m.split_once('*') {
        None => m == n,
        Some((pre, post)) => n.starts_with(pre) && n.ends_with(post) && n.len() >= pre.len() + post.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_state_loads_with_the_game_in_another_folder() {
        let root = std::env::temp_dir().join(format!("lma-vfs-test-{}", std::process::id()));
        let (a, b, user) = (root.join("a/LMA_full"), root.join("b/Game"), root.join("user"));
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir.join("Common/GameData")).unwrap();
            std::fs::write(dir.join("Common/GameData/Export.mng"), b"0123456789").unwrap();
        }
        let mut vfs = Vfs::new(&a, &user);
        let h = vfs.open("d:\\COMMON\\GAMEDATA\\EXPORT.MNG", false, false).unwrap();
        vfs.read(h, &mut [0; 4], None).unwrap();
        let state = vfs.save_state();
        std::fs::remove_dir_all(root.join("a")).unwrap();

        let mut moved = Vfs::new(&b, &user);
        moved.load_state(state).unwrap();
        let mut buf = [0; 3];
        assert_eq!(moved.read(h, &mut buf, None), Ok(3));
        assert_eq!(&buf, b"456");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn mounted_packages_resolve_to_their_own_folders() {
        let root = std::env::temp_dir().join(format!("lma-vfs-mount-{}", std::process::id()));
        let user = root.join("user");
        let mut vfs = Vfs::new(root.join("game"), &user);
        // unmounted: the legacy layout, root name as a folder
        assert_eq!(vfs.resolve("save:\\Career").0, user.join("save/Career"));
        vfs.mount("save", "content/SG01");
        assert_eq!(vfs.resolve("save:\\Career").0, user.join("content/SG01/Career"));
        assert_eq!(vfs.resolve("SAVE:\\Career").0, user.join("content/SG01/Career"), "root names are case-insensitive");
        assert_eq!(vfs.resolve("\\??\\save:\\Career").0, user.join("content/SG01/Career"));
        assert!(vfs.resolve("save:\\Career").1, "writable");
        vfs.mount("save:", "content/SG02"); // mounting again replaces the package
        assert_eq!(vfs.resolve("save:\\Career").0, user.join("content/SG02/Career"));
        assert_eq!(vfs.mounts().len(), 1);
        assert_eq!(vfs.resolve("cache:\\x").0, user.join("cache/x"), "other roots are untouched");
        vfs.unmount("save");
        assert_eq!(vfs.resolve("save:\\Career").0, user.join("save/Career"));
        // a file written through the mount lands in the package folder
        vfs.mount("save", "content/SG03");
        let h = vfs.open("save:\\Career", true, true).unwrap();
        vfs.write(h, b"slot 3", None).unwrap();
        vfs.close(h);
        assert_eq!(std::fs::read(user.join("content/SG03/Career")).unwrap(), b"slot 3");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn overlay_replaces_a_disc_file_in_memory() {
        let root = std::env::temp_dir().join(format!("lma-vfs-overlay-{}", std::process::id()));
        let game = root.join("game");
        std::fs::create_dir_all(game.join("Common/GameData/Misc")).unwrap();
        std::fs::write(game.join("Common/GameData/Misc/Export.mng"), b"disc").unwrap();
        let mut vfs = Vfs::new(&game, root.join("user"));
        vfs.add_overlay("d:\\Common\\GameData\\Misc\\Export.mng", b"modded!".to_vec());
        let guest = "d:\\COMMON\\GAMEDATA\\MISC\\EXPORT.MNG";
        let h = vfs.open(guest, false, false).unwrap();
        let mut buf = [0; 16];
        assert_eq!(vfs.read(h, &mut buf, None), Ok(7));
        assert_eq!(&buf[..7], b"modded!");
        assert_eq!(vfs.read(h, &mut buf, None), Err(STATUS_END_OF_FILE));
        assert_eq!(vfs.read(h, &mut buf[..3], Some(2)), Ok(3));
        assert_eq!(&buf[..3], b"dde");
        assert_eq!(vfs.size(h), Some((7, false)));
        assert_eq!(vfs.stat(guest), Some((7, false)));
        assert_eq!(vfs.open(guest, true, false), Err(STATUS_ACCESS_DENIED));
        let dir = vfs.open("d:\\Common\\GameData\\Misc", false, false).unwrap();
        assert_eq!(vfs.next_entry(dir, "*.mng", true), Ok(Some(("Export.mng".into(), 7, false))));
        // a save state taken with the overlay reopens it
        let state = vfs.save_state();
        vfs.load_state(state).unwrap();
        assert_eq!(vfs.read(h, &mut buf[..2], Some(0)), Ok(2));
        assert_eq!(&buf[..2], b"mo");
        assert_eq!(std::fs::read(game.join("Common/GameData/Misc/Export.mng")).unwrap(), b"disc");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
