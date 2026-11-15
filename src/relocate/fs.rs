//! Everything a move does to the disk goes through `Fs`: the real disk in the app; in the tests
//! the real disk with volume facts faked (two temp folders act as two drives), failures injected
//! by name, and a crash (a panic, so nothing after it runs: no clean-up, as when the power
//! goes) or a change of the world at any named point of a move.

use super::win::{self, Digest, Entry, Item, Volume};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub trait Fs: Send + Sync {
    fn item(&self, p: &Path, follow: bool) -> io::Result<Item>;
    fn final_path(&self, p: &Path) -> io::Result<PathBuf>;
    fn volume(&self, p: &Path) -> io::Result<Volume>;
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>>;
    /// The programs holding any of `files` open, a handle or a mapped view (pid, name); Err:
    /// not every file could be asked.
    fn holders(&self, files: &[PathBuf], stop: &dyn Fn() -> bool) -> Result<Vec<(u32, String)>, String>;
    fn may_rename(&self, dir: &Path) -> io::Result<()>;
    fn may_add(&self, dir: &Path) -> io::Result<()>;
    fn create_dir(&self, p: &Path) -> io::Result<()>;
    fn copy_dacl(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn copy_dir_attributes(&self, from: &Path, to: &Path);
    fn copy_dir_times(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// A folder's own permissions (SDDL); None: it has none.
    fn dacl(&self, p: &Path) -> io::Result<Option<String>>;
    fn set_dacl(&self, p: &Path, sddl: &str) -> io::Result<()>;
    /// The drive under `p` writes its cache to the disk.
    fn flush(&self, p: &Path) -> io::Result<()>;
    /// Ok(false): stopped by `step`.
    fn copy(&self, src: &Path, dst: &Path, step: &dyn Fn(u64) -> bool) -> io::Result<bool>;
    fn hash(&self, p: &Path, unbuffered: bool, stop: &dyn Fn() -> bool, read: &dyn Fn(u64)) -> io::Result<Option<Digest>>;
    /// Rename (never replacing), only while what `from` names is what `is` accepts, judged on
    /// the handle that renames it. Ok(false): it is not, and nothing was renamed.
    fn rename_if(&self, from: &Path, to: &Path, is: &dyn Fn(&Item) -> bool) -> io::Result<bool>;
    fn junction(&self, at: &Path, target: &Path) -> io::Result<()>;
    fn junction_target(&self, at: &Path) -> io::Result<Option<PathBuf>>;
    fn remove_dir(&self, p: &Path) -> io::Result<()>;
    fn remove_tree(&self, p: &Path) -> io::Result<()>;
    /// A named point of a move (the tests crash or change things there; the disk: nothing).
    fn checkpoint(&self, _at: &str) {}
}

/// The disk itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct Real;

impl Fs for Real {
    fn item(&self, p: &Path, follow: bool) -> io::Result<Item> {
        win::item(p, follow)
    }
    fn final_path(&self, p: &Path) -> io::Result<PathBuf> {
        win::final_path(p)
    }
    fn volume(&self, p: &Path) -> io::Result<Volume> {
        win::volume(p)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        win::list(dir)
    }
    fn holders(&self, files: &[PathBuf], stop: &dyn Fn() -> bool) -> Result<Vec<(u32, String)>, String> {
        win::holders(files, stop)
    }
    fn may_rename(&self, dir: &Path) -> io::Result<()> {
        win::may_rename(dir)
    }
    fn may_add(&self, dir: &Path) -> io::Result<()> {
        win::may_add(dir)
    }
    fn create_dir(&self, p: &Path) -> io::Result<()> {
        win::create_dir(p)
    }
    fn copy_dacl(&self, from: &Path, to: &Path) -> io::Result<()> {
        win::copy_dacl(from, to)
    }
    fn copy_dir_attributes(&self, from: &Path, to: &Path) {
        win::copy_dir_attributes(from, to)
    }
    fn copy_dir_times(&self, from: &Path, to: &Path) -> io::Result<()> {
        win::copy_dir_times(from, to)
    }
    fn dacl(&self, p: &Path) -> io::Result<Option<String>> {
        win::dacl(p)
    }
    fn set_dacl(&self, p: &Path, sddl: &str) -> io::Result<()> {
        win::set_dacl(p, sddl)
    }
    fn flush(&self, p: &Path) -> io::Result<()> {
        win::flush(p)
    }
    fn copy(&self, src: &Path, dst: &Path, step: &dyn Fn(u64) -> bool) -> io::Result<bool> {
        win::copy_file(src, dst, step)
    }
    fn hash(&self, p: &Path, unbuffered: bool, stop: &dyn Fn() -> bool, read: &dyn Fn(u64)) -> io::Result<Option<Digest>> {
        win::hash_file(p, unbuffered, stop, read)
    }
    fn rename_if(&self, from: &Path, to: &Path, is: &dyn Fn(&Item) -> bool) -> io::Result<bool> {
        win::rename_if(from, to, is)
    }
    fn junction(&self, at: &Path, target: &Path) -> io::Result<()> {
        win::create_junction(at, target)
    }
    fn junction_target(&self, at: &Path) -> io::Result<Option<PathBuf>> {
        win::junction_target(at)
    }
    fn remove_dir(&self, p: &Path) -> io::Result<()> {
        win::remove_dir(p)
    }
    fn remove_tree(&self, p: &Path) -> io::Result<()> {
        win::remove_tree(p)
    }
}

/// A failure injected by a file or folder name (case-blind).
#[derive(Clone, Debug)]
pub enum Fault {
    /// Copying this file fails with this OS error.
    FailCopy(i32),
    /// The copy of this file lands with its first byte flipped (size and time kept).
    Corrupt,
    /// Renaming this (from this name) fails with this OS error.
    FailRename(i32),
    /// Making a junction at this name fails.
    FailJunction(i32),
    /// Removing this (a folder tree, or one empty folder) fails.
    FailRemove(i32),
    /// Listing this folder fails.
    FailList(i32),
    /// Listed with these attributes added and this reparse tag (an online-only file, a special
    /// reparse point: what a temp folder cannot make).
    Attrs(u32, u32),
    /// Reading this file to hash it fails with this OS error (a bad sector).
    FailRead(i32),
    /// Carrying or putting back permissions onto this folder (or from it) fails.
    FailDacl(i32),
    /// Flushing the drive under this folder fails.
    FailFlush(i32),
    /// Carrying times onto this folder fails.
    FailTimes(i32),
}

/// A change to the world at a checkpoint, run once.
pub type Hook = Box<dyn FnOnce() + Send>;

/// The real disk with faked volumes and injected failures.
#[derive(Default)]
pub struct Faulty {
    faults: Mutex<Vec<(OsString, Fault)>>,
    /// Folders acting as drives of their own: (folder, its facts, its serial).
    drives: Mutex<Vec<(PathBuf, Volume, u64)>>,
    holders: Mutex<Vec<(u32, String)>>,
    /// Asking which programs hold files fails with this.
    holders_fail: Mutex<Option<String>>,
    crash: Mutex<Option<String>>,
    /// Folders acting as FAT/exFAT drives: modified times read back rounded up to this many
    /// ticks (exFAT: 10 ms, FAT32: 2 s), as such a drive stores them.
    rounds: Mutex<Vec<(PathBuf, i64)>>,
    hooks: Mutex<Vec<(String, Hook)>>,
    /// Every checkpoint passed, in order.
    pub passed: Mutex<Vec<String>>,
}

impl Faulty {
    pub fn new() -> Arc<Faulty> {
        Arc::new(Faulty::default())
    }
    pub fn fault(&self, name: &str, f: Fault) {
        self.faults.lock().unwrap().push((OsString::from(name), f));
    }
    pub fn clear(&self) {
        self.faults.lock().unwrap().clear();
        self.holders.lock().unwrap().clear();
        *self.holders_fail.lock().unwrap() = None;
    }
    /// Asking which programs hold files open fails (the answer would be incomplete).
    pub fn holders_fail(&self, why: &str) {
        *self.holders_fail.lock().unwrap() = Some(why.to_string());
    }
    /// `root` and everything under it is on a drive of its own (`serial`), with these facts. Given
    /// again for the same root, the new facts replace the old (another volume at that letter).
    pub fn drive(&self, root: &Path, v: Volume, serial: u64) {
        let mut d = self.drives.lock().unwrap();
        d.retain(|(r, _, _)| !r.as_os_str().eq_ignore_ascii_case(root.as_os_str()));
        d.push((root.to_path_buf(), v, serial));
    }
    /// Everything under `root` reads back with its modified time rounded up to `ticks` (a
    /// FAT/exFAT destination, on NTFS).
    pub fn round_times(&self, root: &Path, ticks: i64) {
        self.rounds.lock().unwrap().push((root.to_path_buf(), ticks));
    }
    fn rounded(&self, at: &Path, mtime: i64) -> i64 {
        let s = at.to_string_lossy().to_lowercase();
        let r = self.rounds.lock().unwrap();
        match r.iter().find(|(root, _)| {
            let root = root.to_string_lossy().to_lowercase();
            s == root || s.strip_prefix(&root).is_some_and(|rest| rest.starts_with('\\'))
        }) {
            Some((_, t)) if *t > 0 => mtime.div_euclid(*t) * t + if mtime.rem_euclid(*t) == 0 { 0 } else { *t },
            _ => mtime,
        }
    }
    /// The Restart Manager says these programs hold files open.
    pub fn hold(&self, pid: u32, name: &str) {
        self.holders.lock().unwrap().push((pid, name.to_string()));
    }
    /// Crash (a panic: nothing after it runs) when this checkpoint is reached; once.
    pub fn crash_at(&self, at: &str) {
        *self.crash.lock().unwrap() = Some(at.to_string());
    }
    /// Run `f` when this checkpoint is reached; once.
    pub fn on(&self, at: &str, f: Hook) {
        self.hooks.lock().unwrap().push((at.to_string(), f));
    }
    fn faults(&self, p: &Path) -> Vec<Fault> {
        let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        self.faults.lock().unwrap().iter().filter(|(n, _)| n.to_string_lossy().to_lowercase() == name).map(|(_, f)| f.clone()).collect()
    }
    /// An item at `at` as this drive's faked facts make it.
    fn faked(&self, at: &Path, it: &mut Item) {
        // By how it is spelled, else by where Windows ends up (an 8.3 short name). Not for a
        // link read as itself: it lives on its host's volume, never where it leads (RS3-3).
        let resolve = || if it.tag.is_some() { None } else { Real.final_path(at).ok().and_then(|f| self.drive_of(&f)) };
        let drive = self.drive_of(at).or_else(resolve);
        if let Some((_, serial)) = drive {
            it.ident.volume = serial;
            it.ident.guid = Some(format!("\\\\?\\Volume{{fake-{serial}}}"));
        }
    }
    fn drive_of(&self, p: &Path) -> Option<(Volume, u64)> {
        let s = p.to_string_lossy().to_lowercase();
        let d = self.drives.lock().unwrap();
        d.iter()
            .filter(|(r, _, _)| {
                let r = r.to_string_lossy().to_lowercase();
                s == r || s.strip_prefix(&r).is_some_and(|rest| rest.starts_with('\\'))
            })
            .max_by_key(|(r, _, _)| r.as_os_str().len())
            .map(|(_, v, s)| (v.clone(), *s))
    }
}

fn os(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

impl Fs for Faulty {
    fn item(&self, p: &Path, follow: bool) -> io::Result<Item> {
        let mut it = Real.item(p, follow)?;
        // Followed through a junction, the item is where the junction leads.
        let at = if follow { Real.final_path(p).unwrap_or_else(|_| p.to_path_buf()) } else { p.to_path_buf() };
        self.faked(&at, &mut it);
        it.mtime = self.rounded(&at, it.mtime);
        Ok(it)
    }
    fn final_path(&self, p: &Path) -> io::Result<PathBuf> {
        Real.final_path(p)
    }
    fn volume(&self, p: &Path) -> io::Result<Volume> {
        match self.drive_of(p) {
            Some((v, _)) => Ok(v),
            None => Real.volume(p),
        }
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<Entry>> {
        for f in self.faults(dir) {
            if let Fault::FailList(c) = f {
                return Err(os(c));
            }
        }
        let mut v = Real.list(dir)?;
        for e in &mut v {
            e.mtime = self.rounded(dir, e.mtime);
            for f in self.faults(Path::new(&String::from_utf16_lossy(&e.name))) {
                if let Fault::Attrs(a, tag) = f {
                    e.attrs |= a;
                    e.tag = tag;
                }
            }
        }
        Ok(v)
    }
    fn holders(&self, files: &[PathBuf], stop: &dyn Fn() -> bool) -> Result<Vec<(u32, String)>, String> {
        if let Some(why) = self.holders_fail.lock().unwrap().clone() {
            return Err(why);
        }
        let fake = self.holders.lock().unwrap().clone();
        if !fake.is_empty() {
            return Ok(fake);
        }
        Real.holders(files, stop)
    }
    fn may_rename(&self, dir: &Path) -> io::Result<()> {
        Real.may_rename(dir)
    }
    fn may_add(&self, dir: &Path) -> io::Result<()> {
        Real.may_add(dir)
    }
    fn create_dir(&self, p: &Path) -> io::Result<()> {
        Real.create_dir(p)
    }
    fn copy_dacl(&self, from: &Path, to: &Path) -> io::Result<()> {
        for f in self.faults(from).into_iter().chain(self.faults(to)) {
            if let Fault::FailDacl(c) = f {
                return Err(os(c));
            }
        }
        Real.copy_dacl(from, to)
    }
    fn copy_dir_attributes(&self, from: &Path, to: &Path) {
        Real.copy_dir_attributes(from, to)
    }
    fn copy_dir_times(&self, from: &Path, to: &Path) -> io::Result<()> {
        for f in self.faults(to) {
            if let Fault::FailTimes(c) = f {
                return Err(os(c));
            }
        }
        Real.copy_dir_times(from, to)
    }
    fn dacl(&self, p: &Path) -> io::Result<Option<String>> {
        Real.dacl(p)
    }
    fn set_dacl(&self, p: &Path, sddl: &str) -> io::Result<()> {
        for f in self.faults(p) {
            if let Fault::FailDacl(c) = f {
                return Err(os(c));
            }
        }
        Real.set_dacl(p, sddl)
    }
    fn flush(&self, p: &Path) -> io::Result<()> {
        for f in self.faults(p) {
            if let Fault::FailFlush(c) = f {
                return Err(os(c));
            }
        }
        Real.flush(p)
    }
    fn copy(&self, src: &Path, dst: &Path, step: &dyn Fn(u64) -> bool) -> io::Result<bool> {
        let faults = self.faults(src);
        for f in &faults {
            if let Fault::FailCopy(c) = f {
                return Err(os(*c));
            }
        }
        let r = Real.copy(src, dst, step)?;
        if r && faults.iter().any(|f| matches!(f, Fault::Corrupt)) {
            let mtime = Real.item(dst, false)?.mtime;
            let mut b = std::fs::read(dst)?;
            match b.first_mut() {
                Some(x) => *x ^= 0x55,
                None => b.push(0),
            }
            std::fs::write(dst, &b)?;
            set_mtime(dst, mtime)?;
        }
        Ok(r)
    }
    fn hash(&self, p: &Path, unbuffered: bool, stop: &dyn Fn() -> bool, read: &dyn Fn(u64)) -> io::Result<Option<Digest>> {
        for f in self.faults(p) {
            if let Fault::FailRead(c) = f {
                return Err(os(c));
            }
        }
        Real.hash(p, unbuffered, stop, read)
    }
    fn rename_if(&self, from: &Path, to: &Path, is: &dyn Fn(&Item) -> bool) -> io::Result<bool> {
        for f in self.faults(from) {
            if let Fault::FailRename(c) = f {
                return Err(os(c));
            }
        }
        // Judged with this drive's faked facts, as `item` reports them.
        Real.rename_if(from, to, &|i: &Item| {
            let mut i = i.clone();
            self.faked(from, &mut i);
            is(&i)
        })
    }
    fn junction(&self, at: &Path, target: &Path) -> io::Result<()> {
        for f in self.faults(at) {
            if let Fault::FailJunction(c) = f {
                return Err(os(c));
            }
        }
        Real.junction(at, target)
    }
    fn junction_target(&self, at: &Path) -> io::Result<Option<PathBuf>> {
        Real.junction_target(at)
    }
    fn remove_dir(&self, p: &Path) -> io::Result<()> {
        for f in self.faults(p) {
            if let Fault::FailRemove(c) = f {
                return Err(os(c));
            }
        }
        Real.remove_dir(p)
    }
    fn remove_tree(&self, p: &Path) -> io::Result<()> {
        for f in self.faults(p) {
            if let Fault::FailRemove(c) = f {
                return Err(os(c));
            }
        }
        Real.remove_tree(p)
    }
    fn checkpoint(&self, at: &str) {
        self.passed.lock().unwrap().push(at.to_string());
        let hook = {
            let mut h = self.hooks.lock().unwrap();
            h.iter().position(|(n, _)| n == at).map(|i| h.remove(i).1)
        };
        if let Some(f) = hook {
            f();
        }
        let crash = {
            let mut c = self.crash.lock().unwrap();
            if c.as_deref() == Some(at) { c.take() } else { None }
        };
        if crash.is_some() {
            panic!("simulated crash at {at}");
        }
    }
}

/// Stamp a file's modified time (FILETIME ticks).
pub fn set_mtime(path: &Path, ticks: i64) -> io::Result<()> {
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(((ticks - 116_444_736_000_000_000) * 100) as u64);
    f.set_times(std::fs::FileTimes::new().set_modified(t))
}
