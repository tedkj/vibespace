//! The journal of each move: `%LOCALAPPDATA%\com.tedkj.vibespace\moves\<id>.json`, written
//! whole or not at all (a temp file flushed to the disk, then renamed over), before every step
//! that changes something. It names both places, their identities and the phase, so a crash at
//! any moment leaves a way back; once a move is done it stays as the record that makes "Move
//! back" possible. `<id>` (6 hex) is the owner id in every name a move makes.

use super::win::Identity;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Copying and checking into the destination (the original untouched).
    Copying,
    /// Taking the original aside and comparing it (rolled back on any doubt; no link exists).
    Aside,
    /// The copy is checked and the original aside compared: the link is being made and proven.
    /// From here the move only goes forward (programs may write through the link).
    Linking,
    /// Proven: the original aside is being removed.
    Removing,
    /// Moved: the record Move back works from.
    Done,
    /// Move back: copying the copy back beside the link (the link untouched).
    BackCopying,
    /// Move back: taking the copy aside and comparing it.
    BackAside,
    /// Move back: the link goes and the copied-back folder takes the old name.
    BackSwapped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub phase: Phase,
    /// The folder's own path (a link once moved).
    pub source: String,
    /// Where it went.
    pub target: String,
    pub source_ident: Identity,
    pub target_ident: Option<Identity>,
    /// Move back's copy beside the link.
    #[serde(default)]
    pub staging_ident: Option<Identity>,
    /// The folder's own permissions before the move (SDDL, the DACL only): Move back puts
    /// them back.
    #[serde(default)]
    pub dacl: Option<String>,
    pub files: u64,
    pub bytes: u64,
    /// When the move started (unix seconds).
    pub at: u64,
    /// Where the copy went as Windows ended up at it when the move was planned (RIN3-9): Move
    /// back and recovery act only while `target` still leads there. Empty: a journal from
    /// before.
    #[serde(default)]
    pub target_real: String,
}

fn suffixed(p: &str, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", p.trim_end_matches('\\')))
}

impl Record {
    pub fn source(&self) -> PathBuf {
        PathBuf::from(&self.source)
    }
    pub fn target(&self) -> PathBuf {
        PathBuf::from(&self.target)
    }
    /// The original, taken aside beside itself.
    pub fn aside(&self) -> PathBuf {
        suffixed(&self.source, &format!(".vsmove-{}", self.id))
    }
    /// Move back's copy, beside the link.
    pub fn staging(&self) -> PathBuf {
        suffixed(&self.source, &format!(".vsback-{}", self.id))
    }
    /// Move back: the copy, taken aside beside itself.
    pub fn target_aside(&self) -> PathBuf {
        suffixed(&self.target, &format!(".vsmove-{}-old", self.id))
    }
}

/// The folder of journals.
#[derive(Clone, Debug)]
pub struct Journal {
    pub dir: PathBuf,
}

/// A move's lock, held while it lives (see `Journal::lock`).
#[derive(Debug)]
pub struct Lock {
    _file: std::fs::File,
}

/// A journal file that is there but could not be read as one.
#[derive(Clone, Debug, PartialEq)]
pub struct Unreadable {
    pub file: PathBuf,
    pub why: String,
}

impl Journal {
    pub fn new(dir: PathBuf) -> Journal {
        Journal { dir }
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Whole or not at all, and on the disk before it returns.
    pub fn write(&self, r: &Record) -> io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(&r.id);
        let tmp = self.dir.join(format!("{}.{}.tmp", r.id, std::process::id()));
        let body = serde_json::to_vec_pretty(r).map_err(io::Error::other)?;
        let w = (|| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &path)
        })();
        if w.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        w
    }

    pub fn remove(&self, id: &str) -> io::Result<()> {
        match std::fs::remove_file(self.path(id)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    /// The journal of `id` as it is on the disk now. Ok(None): there is none (the move was put
    /// back or moved back).
    pub fn read(&self, id: &str) -> Result<Option<Record>, String> {
        match read_capped(&self.path(id)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(super::win::describe(&e)),
            Ok(b) => parse(&b).and_then(|r| if r.id == id { Ok(Some(r)) } else { Err("its name and its id differ".into()) }),
        }
    }

    /// The one lock of move `id`: `<id>.lock` beside its journal, opened exclusively and gone
    /// when closed (also when the process ends). Whoever holds it is the only one acting on the
    /// move: a move, a move back or a recovery, in this window or another. Ok(None): another
    /// holds it (asked for a quarter of a second, so a brief look by another window's start
    /// does not count).
    pub fn lock(&self, id: &str) -> io::Result<Option<Lock>> {
        use std::os::windows::fs::OpenOptionsExt;
        const DELETE_ON_CLOSE: u32 = 0x0400_0000;
        const ACCESS: u32 = 0x8000_0000 | 0x4000_0000 | 0x0001_0000; // read, write, delete
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{id}.lock"));
        for _ in 0..10 {
            match std::fs::OpenOptions::new().read(true).write(true).access_mode(ACCESS).create(true).truncate(false).share_mode(0).custom_flags(DELETE_ON_CLOSE).open(&path) {
                Ok(f) => return Ok(Some(Lock { _file: f })),
                // Held (32), or a lock just closed whose file is still being deleted (5).
                Err(e) if matches!(e.raw_os_error(), Some(32 | 5)) => std::thread::sleep(std::time::Duration::from_millis(25)),
                Err(e) => return Err(e),
            }
        }
        match std::fs::OpenOptions::new().read(true).write(true).access_mode(ACCESS).create(true).truncate(false).share_mode(0).custom_flags(DELETE_ON_CLOSE).open(&path) {
            Ok(f) => Ok(Some(Lock { _file: f })),
            Err(e) if e.raw_os_error() == Some(32) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Someone (another window) is acting on move `id` right now.
    pub fn busy(&self, id: &str) -> bool {
        matches!(self.lock(id), Ok(None))
    }

    /// Every journal, oldest first, and the files that could not be read as one (never
    /// removed or replaced: what they say is not known).
    pub fn read_all(&self) -> (Vec<Record>, Vec<Unreadable>) {
        let mut out = Vec::new();
        let mut bad = Vec::new();
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return (out, bad),
            Err(e) => {
                bad.push(Unreadable { file: self.dir.clone(), why: super::win::describe(&e) });
                return (out, bad);
            }
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            match read_capped(&p).map_err(|e| super::win::describe(&e)).and_then(|b| parse(&b)) {
                Ok(r) if p.file_stem().is_some_and(|s| s.to_string_lossy() == r.id) => out.push(r),
                Ok(_) => bad.push(Unreadable { file: p, why: "its name and its id differ".into() }),
                Err(why) => bad.push(Unreadable { file: p, why }),
            }
        }
        out.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
        (out, bad)
    }

    /// A new owner id: 6 hex, no journal of that id yet.
    pub fn new_id(&self, source: &Path) -> String {
        use std::hash::{BuildHasher, Hasher};
        for n in 0u32.. {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write(source.as_os_str().as_encoded_bytes());
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
            h.write_u32(std::process::id());
            h.write_u32(n);
            let id = format!("{:06x}", h.finish() & 0xFF_FFFF);
            if !self.path(&id).exists() {
                return id;
            }
        }
        unreachable!()
    }
}

/// The most a journal file is read (IN-VibeSpace-7): its fields, two paths and a folder's
/// permissions take a few KB. One larger is no journal of ours, and is never read whole.
pub const MAX_JOURNAL: u64 = 1 << 20;

/// A journal file's bytes, at most MAX_JOURNAL and one more: `parse` refuses one that long.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut b = Vec::new();
    std::fs::File::open(path)?.take(MAX_JOURNAL + 1).read_to_end(&mut b)?;
    Ok(b)
}

/// A journal's bytes as a record: JSON with the fields, an id of 6 hex digits and plain absolute
/// paths. Anything else is no journal of ours.
pub fn parse(b: &[u8]) -> Result<Record, String> {
    if b.len() as u64 > MAX_JOURNAL {
        return Err("it is larger than any journal VibeSpace writes".into());
    }
    let r: Record = serde_json::from_slice(b).map_err(|e| format!("not a journal ({e})"))?;
    if r.id.len() != 6 || !r.id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("its id is not one VibeSpace makes".into());
    }
    for p in [&r.source, &r.target] {
        if !plain_folder(p) {
            return Err(format!("a path in it is not a folder VibeSpace moves: {}", crate::tree::clean(p)));
        }
    }
    if r.dacl.as_ref().is_some_and(|d| d.is_empty() || d.chars().any(|c| c.is_control())) {
        return Err("its permissions are not ones VibeSpace records".into());
    }
    Ok(r)
}

/// `X:\name\...`: a drive letter, then one or more names, none of them empty, `.` or `..`
/// (judged name by name: `..cache`, `x.` and `a b ` are names like any other), no control
/// characters.
fn plain_folder(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() < 4 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' || p.chars().any(|c| c.is_control()) {
        return false;
    }
    p[3..].split('\\').all(|n| !n.is_empty() && n != "." && n != "..")
}
