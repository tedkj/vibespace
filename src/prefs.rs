//! A small JSON file the app remembers things in (theme, window choices, recent entries).
//! Missing means defaults; garbled means defaults too, but the file is first set aside as
//! `<name>.broken` (replacing an older copy; never written over: VibeBackup BA-3); a file that
//! is there but cannot be read right now is never overwritten. The window's changes are kept as
//! changes, not as a merged value: every write reads the file afresh and puts the unwritten
//! changes onto what it read, so another instance's prefs stay, even when this window started on
//! defaults because the file could not be read then (the family's R2-TRACE-1 fix). Writes happen
//! off the UI thread, whole or not at all.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

thread_local! {
    /// prefs.json reads made on this thread (the tests check the window's thread makes none
    /// after start).
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times this thread has read a prefs file.
pub fn reads_here() -> usize {
    READS.with(|r| r.get())
}

fn counted_read(path: &Path) -> std::io::Result<Vec<u8>> {
    READS.with(|r| r.set(r.get() + 1));
    std::fs::read(path)
}

/// One change of this window's to the prefs (set a field, remember an entry), applied onto the
/// newest prefs right before each write. An absolute setter, never a toggle: it is applied again
/// onto each fresh read until it is written.
pub type Change<T> = Arc<dyn Fn(&mut T) + Send + Sync>;

pub struct Store<T> {
    path: PathBuf,
    /// This window's changes that are not in the file yet, oldest first.
    changes: Arc<Mutex<Vec<Change<T>>>>,
    /// This window's prefs as last loaded or written: what the changes go onto when the file is
    /// missing or garbled.
    own: Arc<Mutex<T>>,
    write: Arc<Mutex<()>>,
    /// `load` found the file garbled and set it aside (told at start).
    broken: Arc<AtomicBool>,
    /// A write found the file garbled and set it aside; the good one is not written yet.
    aside: Arc<AtomicBool>,
    /// A garbled file was set aside and this window's prefs written in its place, and nobody
    /// has told the user yet.
    garbled_new: Arc<AtomicBool>,
}

impl<T> Clone for Store<T> {
    fn clone(&self) -> Self {
        Store {
            path: self.path.clone(),
            changes: self.changes.clone(),
            own: self.own.clone(),
            write: self.write.clone(),
            broken: self.broken.clone(),
            aside: self.aside.clone(),
            garbled_new: self.garbled_new.clone(),
        }
    }
}

/// A lock that a panic elsewhere does not turn into a second one (the test rigs flush in Drop).
fn lock<X>(m: &Mutex<X>) -> MutexGuard<'_, X> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where a garbled file is kept: `prefs.json.broken`.
pub fn broken_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".broken");
    path.with_file_name(name)
}

/// Whole or not at all: a crash or a power cut mid-write leaves the old file. The temp file sits
/// beside the target and is this process's own (named by its id, and made create-new), so
/// neither a second instance writing at the same moment nor a file of the user's that happens to
/// have its name is ever written into or removed: a name already taken moves on to the next.
/// It is on the disk (`sync_all`, FlushFileBuffers) before it takes the name, so a power cut
/// right after cannot leave the name on zeros (FS10), and it is removed on every error. std's
/// rename replaces with POSIX semantics on NTFS: a reader holding the file open (another
/// instance, a scanner) does not block it, where MoveFileExW is refused. The new file takes the
/// folder's attributes and ACL, as before.
pub fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let pid = std::process::id();
    let mut n = 0;
    let (tmp, mut file) = loop {
        let tmp = path.with_extension(if n == 0 { format!("{pid}.tmp") } else { format!("{pid}-{n}.tmp") });
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 20 => n += 1,
            Err(e) => return Err(e),
        }
    };
    let r = file.write_all(body).and_then(|_| file.sync_all());
    drop(file);
    let r = r.and_then(|_| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

impl<T: Serialize + DeserializeOwned + Default + Clone + Send + 'static> Store<T> {
    pub fn new(path: PathBuf) -> Store<T> {
        Store {
            path,
            changes: Default::default(),
            own: Default::default(),
            write: Default::default(),
            broken: Default::default(),
            aside: Default::default(),
            garbled_new: Default::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The prefs; defaults when missing, garbled or unreadable now. A garbled file is set aside
    /// first (`take_broken` tells); when that rename fails it counts as unreadable right now,
    /// so nothing writes over it.
    pub fn load(&self) -> T {
        let v = match counted_read(&self.path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|_| {
                if std::fs::rename(&self.path, broken_path(&self.path)).is_ok() {
                    self.broken.store(true, Ordering::SeqCst);
                }
                T::default()
            }),
            Err(_) => T::default(),
        };
        *lock(&self.own) = v.clone();
        v
    }

    /// Whether `load` set a garbled file aside since this was last asked (the window says so
    /// at start).
    pub fn take_broken(&self) -> bool {
        self.broken.swap(false, Ordering::SeqCst)
    }

    /// Whether a garbled file was set aside and this window's prefs written in its place since
    /// this was last asked: the window says so once, and only then (both are true by then).
    pub fn take_garbled(&self) -> bool {
        self.garbled_new.swap(false, Ordering::SeqCst)
    }

    /// One change of this window's: kept until it is in the file, and written now (`flush`).
    /// Call it off the UI thread (it reads the file).
    pub fn change(&self, f: Change<T>) {
        lock(&self.changes).push(f);
        self.flush();
    }

    /// Write the unwritten changes: read the file afresh, put every change onto it in order,
    /// write the result, and only then forget the changes. While the file cannot be read nothing
    /// is written and the changes wait for the next write (one still waiting at exit is lost:
    /// never this window's defaults over a file it could not read). A garbled file is set aside
    /// first (never written over), and a garbled or missing one is replaced by this window's
    /// own prefs with the changes.
    pub fn flush(&self) {
        let _w = lock(&self.write);
        let n = lock(&self.changes).len();
        if n == 0 {
            return;
        }
        let base = match counted_read(&self.path) {
            Ok(b) => match serde_json::from_slice::<T>(&b) {
                Ok(v) => Some(v),
                // Garbled: set aside (replacing an older copy), never written over.
                Err(_) if std::fs::rename(&self.path, broken_path(&self.path)).is_ok() => {
                    self.aside.store(true, Ordering::SeqCst);
                    Some(lock(&self.own).clone())
                }
                Err(_) => None,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(lock(&self.own).clone()),
            Err(_) => None,
        };
        let Some(mut v) = base else { return };
        let todo: Vec<Change<T>> = lock(&self.changes)[..n].to_vec();
        for f in &todo {
            f(&mut v);
        }
        let ok = serde_json::to_vec_pretty(&v).map(|b| write_atomic(&self.path, &b).is_ok()).unwrap_or(false);
        if ok {
            lock(&self.changes).drain(..n);
            *lock(&self.own) = v;
            // Set aside and replaced: now it is true to say so.
            if self.aside.swap(false, Ordering::SeqCst) {
                self.garbled_new.store(true, Ordering::SeqCst);
            }
        }
    }
}
