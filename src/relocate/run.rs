//! The moves themselves (SPEC.md §6.3 to 6.5). Every step only after the one before succeeded,
//! the journal written before each step that changes something, and every failure handled by
//! the same code a crash is recovered with (`put_back`, `finish`): one mechanism, so the way
//! back is the tested way back. Nothing is removed without reading first that it is what the
//! journal says it made or moved (its identity), and every recovery looks at everything it will
//! touch before it touches anything; a link goes last. One lock per move (`Journal::lock`): a
//! move, a move back and a recovery of it never run at once, in one window or two.
//!
//! Once the link exists, programs may write through it into the copy: from then on the move
//! only goes forward (the copy is the data), unless the copy is provably as it was copied. When
//! in doubt nothing is deleted: a copy that failed its check never has the original removed
//! behind it, and a compare that cannot tell keeps everything.

use super::fs::Fs;
use super::journal::{Journal, Lock, Phase, Record};
use super::win::{self, Digest, TAG_MOUNT_POINT};
use super::{Env, Item, Plan, Request, same_path, show};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Said when another window holds a move's lock.
pub const BUSY: &str = "This move is being handled in another VibeSpace window";

/// How a Stop during the check before the original goes begins (R-SPACE-MV3-3).
pub const STOPPED: &str = "Stopped while checking the copy against the original";

/// Where a move is, for the window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    /// Past the copy: the rest is quick and cannot be cancelled.
    pub finishing: bool,
    pub copied: u64,
    pub checked: u64,
    pub total: u64,
    /// The file at hand, relative, as shown.
    pub file: String,
    /// Checking the copy against the original before it goes (a whole read of both sides,
    /// R-SPACE-MV3-3): files done, files, bytes done, bytes. It can be stopped.
    pub against: Option<(u64, u64, u64, u64)>,
    /// The check passed: the copy is written to the disk and the original removed. Nothing
    /// can be stopped from here (R-SPACE-MV4-1).
    pub removing: bool,
}

/// A move or a move back that did not happen (or not all of it).
#[derive(Clone, Debug, PartialEq)]
pub struct Failed {
    pub why: String,
    /// The journal is kept: the way back did not finish and is offered again.
    pub kept: bool,
}

/// What a move did.
#[derive(Clone, Debug, PartialEq)]
pub struct Moved {
    pub record: Record,
    /// Said along with the result: things left over, or not carried.
    pub notes: Vec<String>,
}

/// How a put back ended: put back as it was, or finished forward instead (the original gone:
/// the link stays; the copy written to through the link: both kept, the original aside).
#[derive(Clone, Debug, PartialEq)]
pub enum Back {
    Put(Vec<String>),
    Finished(Vec<String>),
}

fn journal(env: &Env) -> Result<&Journal, Failed> {
    env.journal.as_ref().ok_or_else(|| Failed { why: "Nothing was moved: VibeSpace has no folder for its journals".into(), kept: false })
}

/// The move's lock, or why not.
fn lock(j: &Journal, id: &str) -> Result<Lock, String> {
    match j.lock(id) {
        Ok(Some(l)) => Ok(l),
        Ok(None) => Err(BUSY.into()),
        Err(e) => Err(format!("Its lock could not be taken ({})", win::describe(&e))),
    }
}

/// Progress, at most every 100 ms.
struct Beat<'a> {
    f: &'a dyn Fn(Progress),
    last: std::cell::Cell<Instant>,
    p: std::cell::RefCell<Progress>,
}

impl<'a> Beat<'a> {
    fn new(f: &'a dyn Fn(Progress), total: u64) -> Beat<'a> {
        let b = Beat::with(f, Progress { total, ..Progress::default() });
        b.send(true);
        b
    }
    /// Starting from `p`, nothing sent yet.
    fn with(f: &'a dyn Fn(Progress), p: Progress) -> Beat<'a> {
        Beat { f, last: std::cell::Cell::new(Instant::now() - Duration::from_secs(1)), p: std::cell::RefCell::new(p) }
    }
    fn send(&self, now: bool) {
        if now || self.last.get().elapsed() >= Duration::from_millis(100) {
            self.last.set(Instant::now());
            (self.f)(self.p.borrow().clone());
        }
    }
    fn set(&self, f: impl FnOnce(&mut Progress), now: bool) {
        f(&mut self.p.borrow_mut());
        self.send(now);
    }
}

/// How a long read is watched: Stop, and where its progress goes (none: not shown).
struct Watch<'a> {
    stop: &'a dyn Fn() -> bool,
    beat: Option<&'a Beat<'a>>,
}

fn never() -> bool {
    false
}

/// Not watched: a read nobody can stop or see (a quick one, or one with no window behind it).
fn quiet() -> Watch<'static> {
    Watch { stop: &never, beat: None }
}

/// What the copy saw of each item: the original as it was copied (size, times, id), the copy as
/// it landed, and each file's hash.
#[derive(Clone, Debug, Default)]
pub struct Copied {
    pub source: Vec<Item>,
    pub copy: Vec<Item>,
    digest: Vec<Option<Digest>>,
}

fn as_item(rel: &[u16], i: &win::Item) -> Item {
    Item { rel: rel.to_vec(), dir: i.dir, size: if i.dir { 0 } else { i.size }, mtime: i.mtime, ctime: i.ctime, id: i.id() }
}

/// Copy `items` from `from` into `to` (which exists), each file checked: both sides hashed (the
/// copy read past the cache), the original's size, times and id unchanged from before its copy
/// to after its hash. Folders get their attributes as they are made and their times once
/// everything is in them (`to` too).
fn copy_all(fs: &dyn Fs, items: &[Item], from: &Path, to: &Path, stop: &AtomicBool, beat: &Beat) -> Result<Copied, String> {
    let halt = || stop.load(Ordering::Relaxed);
    let mut out = Copied::default();
    let (mut copied, mut checked) = (0u64, 0u64);
    for it in items {
        if halt() {
            return Err("Cancelled".into());
        }
        let (s, d) = (it.path(from), it.path(to));
        if it.dir {
            fs.create_dir(&d).map_err(|e| format!("{} could not be made ({})", show(&d), win::describe(&e)))?;
            fs.copy_dir_attributes(&s, &d);
            out.source.push(it.clone());
            out.copy.push(it.clone());
            out.digest.push(None);
            continue;
        }
        let name = it.name();
        let before = fs.item(&s, false).map_err(|e| format!("{} could not be read ({})", name, win::describe(&e)))?;
        // A big file is shown as it starts and as it ends (it takes a while); small ones at
        // the beat's pace.
        let big = before.size >= 1 << 20;
        beat.set(|p| p.file = name.clone(), big);
        let base = copied;
        let go = fs
            .copy(&s, &d, &|n| {
                if n != u64::MAX {
                    beat.set(|p| p.copied = base + n.min(before.size), false);
                }
                !halt()
            })
            .map_err(|e| format!("{} could not be copied ({})", name, win::describe(&e)))?;
        if !go {
            return Err("Cancelled".into());
        }
        copied += before.size;
        beat.set(|p| p.copied = copied, big);
        fs.checkpoint(&format!("copy {name}"));
        let a = fs.hash(&s, false, &halt, &|_| {}).map_err(|e| format!("{} could not be read back ({})", name, win::describe(&e)))?;
        let cbase = checked;
        let b = fs.hash(&d, true, &halt, &|n| beat.set(|p| p.checked = cbase + n.min(before.size), false)).map_err(|e| format!("The copy of {} could not be read back ({})", name, win::describe(&e)))?;
        let (Some(a), Some(b)) = (a, b) else { return Err("Cancelled".into()) };
        let after = fs.item(&s, false).map_err(|e| format!("{} could not be read ({})", name, win::describe(&e)))?;
        if !same_file(&as_item(&it.rel, &after), &as_item(&it.rel, &before), true) {
            return Err(format!("{name} changed while it was copied"));
        }
        if a != b {
            return Err(format!("The copy of {name} does not read back the same as the original"));
        }
        let landed = fs.item(&d, false).map_err(|e| format!("The copy of {} could not be read ({})", name, win::describe(&e)))?;
        checked += before.size;
        beat.set(|p| (p.copied, p.checked) = (copied, checked), false);
        out.source.push(as_item(&it.rel, &before));
        out.copy.push(as_item(&it.rel, &landed));
        out.digest.push(Some(a));
    }
    // Times last: every new entry moves its folder's modified time.
    for it in items.iter().rev().filter(|i| i.dir) {
        fs.copy_dir_times(&it.path(from), &it.path(to)).map_err(|e| format!("The times of {} could not be carried ({})", it.name(), win::describe(&e)))?;
    }
    fs.copy_dir_times(from, to).map_err(|e| format!("The folder's times could not be carried ({})", win::describe(&e)))?;
    beat.set(|p| (p.copied, p.checked) = (copied, checked), true);
    Ok(out)
}

/// One file as it was and as it is: the same size and (with `times`) modified time, and the
/// same change time and file id wherever both are known (a rewrite that keeps size and modified
/// time still moves the change time; a file deleted and made again has another id). Across
/// volumes `times` is false: FAT32 stores modified times to 2 s and exFAT to 10 ms (measured on
/// the real D:), so there the bytes are compared instead (R-SPACE-MV3-1).
fn same_file(now: &Item, was: &Item, times: bool) -> bool {
    let known = |a: i64, b: i64| a == 0 || b == 0 || a == b;
    now.size == was.size && (!times || now.mtime == was.mtime) && known(now.ctime, was.ctime) && known(now.id as i64, was.id as i64)
}

/// Why a compare did not find a folder as it should be: a difference seen (evidence that
/// something wrote into it), or no answer (it could not be read or asked: evidence of nothing).
#[derive(Clone, Debug, PartialEq)]
enum Odd {
    Differs(String),
    Unknown(String),
    /// Stopped by the user before it could tell.
    Stopped,
}

impl Odd {
    fn why(self) -> String {
        match self {
            Odd::Differs(s) | Odd::Unknown(s) => s,
            Odd::Stopped => STOPPED.into(),
        }
    }
}

/// The folder at `dir` must hold exactly `want`: the same names and kinds, each file the same
/// (`same_file`, modified times only with `times`: on the same volume), nothing more; and no
/// program may have any file in it open (a handle or a mapped view: a mapping does not stop a
/// rename, so it is asked here).
fn compare(fs: &dyn Fs, dir: &Path, want: &[Item], times: bool) -> Result<(), Odd> {
    let w = listing(fs, dir, want, true, times)?;
    match super::in_use(fs, dir, &w.items, &|| false) {
        Ok(None) => Ok(()),
        Ok(Some(n)) => Err(Odd::Differs(format!("Files in it are in use by {n}"))),
        Err(e) => Err(Odd::Unknown(format!("Could not ask which programs have files in it open: {e}"))),
    }
}

/// The folder at `dir` holds every item of `want` (the same kind; a file the same by
/// `same_file`), and with `exact` nothing more. What was listed, for the holders' question.
fn listing(fs: &dyn Fs, dir: &Path, want: &[Item], exact: bool, times: bool) -> Result<super::Walk, Odd> {
    let w = super::walk(fs, dir, &|| false).map_err(|e| Odd::Unknown(format!("It could not be read again: {e}")))?;
    if !w.links.is_empty() || !w.special.is_empty() || w.online > 0 {
        return Err(Odd::Differs("Something was added to it while it was copied".into()));
    }
    let now: std::collections::HashMap<&[u16], &Item> = w.items.iter().map(|i| (i.rel.as_slice(), i)).collect();
    let wanted: std::collections::HashSet<&[u16]> = want.iter().map(|i| i.rel.as_slice()).collect();
    if let Some(extra) = w.items.iter().find(|i| exact && !wanted.contains(i.rel.as_slice())) {
        return Err(Odd::Differs(format!("{} was added to it while it was copied", extra.name())));
    }
    for it in want {
        let Some(n) = now.get(it.rel.as_slice()) else { return Err(Odd::Differs(format!("{} went while it was copied", it.name()))) };
        if n.dir != it.dir || (!it.dir && !same_file(n, it, times)) {
            return Err(Odd::Differs(format!("{} changed while it was copied", it.name())));
        }
    }
    Ok(w)
}

/// Before the original goes (R-SPACE-MV2, gap 7): the copy must hold what is left of the
/// original aside, every file by name, kind, size and content (hashed on both sides; modified
/// times are no evidence across volumes, R-SPACE-MV3-1). The copy may hold more: a file added
/// through the link is the link in use, and a removal cut short leaves the aside part removed
/// whatever the journal says (R-SPACE-MV3-2); removing the aside can only lose what is in it,
/// and every byte of that is checked. Err: a difference (Differs) or no answer (Unknown), in
/// words that keep both and name where the original is.
fn copy_holds_the_original(fs: &dyn Fs, rec: &Record, w: &Watch) -> Result<(), Odd> {
    let aside = rec.aside();
    match holds(fs, &aside, &rec.target(), w, ("the original", "the copy")) {
        Ok(()) => Ok(()),
        Err(Odd::Stopped) => Err(Odd::Unknown(format!("{STOPPED}: both are kept, the copy in use and the original at {}", show(&aside)))),
        Err(Odd::Differs(why)) => Err(Odd::Differs(format!(
            "The copy and the original differ ({}): both are kept, the copy in use and the original at {}",
            lower(&why).replace(" while it was copied", ""),
            show(&aside)
        ))),
        Err(Odd::Unknown(why)) => Err(Odd::Unknown(format!(
            "VibeSpace could not tell whether the copy holds the original ({}): both are kept, the copy in use and the original at {}",
            lower(&why),
            show(&aside)
        ))),
    }
}

/// `keep` holds every entry of `gone` (the side about to be removed): by name, kind, size and
/// content, never by modified time (across volumes); `keep` may hold more. `names`: what `gone`
/// and `keep` are called in the words.
fn holds(fs: &dyn Fs, gone: &Path, keep: &Path, w: &Watch, (g, k): (&str, &str)) -> Result<(), Odd> {
    let walked = super::walk(fs, gone, &|| false).map_err(|e| Odd::Unknown(format!("{} could not be read: {e}", show(gone))))?;
    // What the walk sets apart (a link, a special or online-only file) cannot be compared: it
    // would go with `gone` unchecked (R-SPACE-MV4-6).
    if let Some(n) = walked.links.iter().chain(&walked.special).next() {
        return Err(Odd::Differs(format!("{n} is in {g} and cannot be checked against {k}")));
    }
    if walked.online > 0 {
        return Err(Odd::Differs(format!("{} online-only files are in {g} and cannot be checked against {k}", walked.online)));
    }
    let reference: Vec<Item> = walked.items.into_iter().map(|i| Item { ctime: 0, id: 0, ..i }).collect();
    listing(fs, keep, &reference, false, false).and_then(|_| same_bytes(fs, gone, keep, &reference, w))
}

/// Before the copy aside goes in a recovery's Finish of a move back (SOL-SPACE-1): the folder
/// copied back, now at the old path, must hold what is left of the copy aside, every file by
/// content (`holds`); the journal may have waited days, and a file added to or changed in the
/// copy aside meanwhile, or gone from the folder copied back, has no other copy. Err: both are
/// kept and named, the journal too.
/// `folder`: where the folder copied back is now (its own name, or the old path after a crash
/// past the rename, RS-2).
fn folder_holds_the_copy(fs: &dyn Fs, rec: &Record, folder: &Path) -> Result<(), String> {
    let taside = rec.target_aside();
    let both = format!("both are kept, the folder copied back at {} and the copy at {}", show(folder), show(&taside));
    match holds(fs, &taside, folder, &quiet(), ("the copy", "the folder copied back")) {
        Ok(()) => Ok(()),
        Err(Odd::Differs(why)) => Err(format!("The folder copied back and the copy differ ({}): {both}", lower(&why).replace(" while it was copied", ""))),
        Err(odd) => Err(format!("VibeSpace could not tell whether the folder copied back holds the copy ({}): {both}", lower(&odd.why()))),
    }
}

/// Said when the move back's read of the copy aside is stopped (RS-1).
const BACK_STOPPED: &str = "Stopped while reading the copy again";

/// Before a move back journals BackSwapped (RS-1): every file of the copy aside reads as the
/// copy back hashed it. The copy back may take hours with the link up, and a write through a
/// mapped view keeps the size and may keep every time: the compare cannot see it, the bytes do.
/// One side is enough: the folder copied back read the same as these hashes. Told and stopped
/// as the forward check is (`against`).
fn reads_as_copied(fs: &dyn Fs, dir: &Path, copied: &Copied, w: &Watch) -> Result<(), Odd> {
    let files: Vec<(&Item, Digest)> = copied.source.iter().zip(&copied.digest).filter_map(|(i, d)| d.map(|d| (i, d))).collect();
    let (count, bytes) = (files.len() as u64, files.iter().map(|(i, _)| i.size).sum::<u64>());
    let tell = |done: u64, at: u64, name: Option<String>, now: bool| {
        if let Some(b) = w.beat {
            b.set(
                |p| {
                    p.against = Some((done, count, at.min(bytes), bytes));
                    if let Some(n) = name {
                        p.file = n;
                    }
                },
                now,
            );
        }
    };
    let mut at = 0u64;
    for (done, (it, want)) in files.iter().enumerate() {
        let (p, name) = (it.path(dir), it.name());
        tell(done as u64, at, Some(name.clone()), it.size >= 1 << 20);
        fs.checkpoint(&format!("reread {name}"));
        match fs.hash(&p, false, w.stop, &|n| tell(done as u64, at + n.min(it.size), None, false)) {
            Ok(Some(h)) if h == *want => {}
            Ok(Some(_)) => return Err(Odd::Differs(format!("{name} changed while it was copied"))),
            Ok(None) => return Err(Odd::Stopped),
            Err(e) => return Err(Odd::Unknown(format!("{name} could not be read again ({})", win::describe(&e)))),
        }
        at += it.size;
    }
    tell(count, bytes, None, true);
    Ok(())
}

/// How the folder a path is in answers (`parent_answers`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parent {
    /// On the volume the copy was on: a path not found there is gone for certain.
    Answers,
    /// Not reached (a drive not there, a link that leads nowhere, a share that dropped), or the
    /// copy's volume not known.
    Unreached,
    /// A folder, but on another volume (another drive took the letter, RS3-4).
    OtherVolume,
    /// A folder, but its serial is 0: which drive it is on is not known, neither the copy's nor
    /// another (RL-3).
    Unknown,
    /// A folder, but the copy's serial in the journal is 0: which drive the copy was on is not
    /// known (RL2-2).
    CopyUnknown,
}

/// The folder `p` is in, opened through any link (a junction or mount point that leads nowhere
/// does not answer), and whether it is on the volume `on` is on: only then is `p` not found gone
/// for certain; otherwise it is unknown, never gone (RS-3, RS2-1).
fn parent_answers(fs: &dyn Fs, p: &Path, on: Option<&win::Identity>) -> Parent {
    let (Some(on), Some(d)) = (on, p.parent()) else { return Parent::Unreached };
    match fs.item(d, true) {
        Ok(i) if i.dir && i.ident.same_volume(on) => Parent::Answers,
        Ok(i) if i.dir && on.volume == 0 => Parent::CopyUnknown,
        Ok(i) if i.dir && i.ident.volume == 0 => Parent::Unknown,
        Ok(i) if i.dir => Parent::OtherVolume,
        _ => Parent::Unreached,
    }
}

/// What `finish` does with the original aside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Original {
    /// Never removed (the copy failed its check).
    Keep,
    /// Removed once the copy is checked to hold it (`copy_holds_the_original`).
    Remove,
    /// Removed: the caller checked just now.
    Checked,
}

/// Every file of `items` reads the same in `copy` as in `original` (both hashed: the copy past
/// the cache). What a compare of names, sizes and times cannot see (a mapped write, a tool that
/// sets the times back) is seen here.
fn same_bytes(fs: &dyn Fs, original: &Path, copy: &Path, items: &[Item], w: &Watch) -> Result<(), Odd> {
    // A file a program holds open without sharing (a database, a disk image) cannot be read:
    // the program is named, so the user knows what to close (R-SPACE-MV3-6).
    let unreadable = |what: String, p: &Path, e: std::io::Error| {
        if matches!(e.raw_os_error(), Some(32 | 33))
            && let Ok(h) = fs.holders(&[p.to_path_buf()], &|| false)
            && !h.is_empty()
        {
            let mut who: Vec<String> = h.into_iter().map(|(pid, name)| if name.is_empty() { format!("process {pid}") } else { name }).collect();
            who.sort();
            who.dedup();
            return Odd::Unknown(format!("{what} is held open by {}: once it is closed, VibeSpace checks again", who.join(", ")));
        }
        Odd::Unknown(format!("{what} could not be read ({})", win::describe(&e)))
    };
    // Told as it goes: files and bytes (each file's bytes counted half on each side).
    let files: Vec<&Item> = items.iter().filter(|i| !i.dir).collect();
    let (count, bytes) = (files.len() as u64, files.iter().map(|i| i.size).sum::<u64>());
    let tell = |done: u64, at: u64, name: Option<String>, now: bool| {
        if let Some(b) = w.beat {
            b.set(
                |p| {
                    p.against = Some((done, count, at.min(bytes), bytes));
                    if let Some(n) = name {
                        p.file = n;
                    }
                },
                now,
            );
        }
    };
    let mut at = 0u64;
    for (done, it) in files.iter().enumerate() {
        let (pa, pb, name) = (it.path(original), it.path(copy), it.name());
        tell(done as u64, at, Some(name.clone()), it.size >= 1 << 20);
        fs.checkpoint(&format!("compare {name}"));
        let a = fs.hash(&pa, false, w.stop, &|n| tell(done as u64, at + n.min(it.size) / 2, None, false)).map_err(|e| unreadable(name.clone(), &pa, e))?;
        let b = fs.hash(&pb, true, w.stop, &|n| tell(done as u64, at + it.size / 2 + n.min(it.size) / 2, None, false)).map_err(|e| unreadable(format!("The copy of {name}"), &pb, e))?;
        let (Some(a), Some(b)) = (a, b) else { return Err(Odd::Stopped) };
        if a != b {
            return Err(Odd::Differs(format!("{name} changed while it was copied")));
        }
        at += it.size;
    }
    tell(count, bytes, None, true);
    Ok(())
}

/// The link must lead to the copy, and a sample read through it must hash as checked: the 8
/// largest files and up to 24 more spread over the rest, `PROOF_BYTES` at most.
fn prove(fs: &dyn Fs, link: &Path, target_ident: &win::Identity, items: &[Item], copied: &Copied) -> Result<(), String> {
    match fs.item(link, true) {
        Ok(i) if i.ident.same(target_ident) => {}
        Ok(_) => return Err("The link does not lead to the copy".into()),
        Err(e) => return Err(format!("The link could not be followed ({})", win::describe(&e))),
    }
    for i in sample(items) {
        let Some(want) = copied.digest.get(i).copied().flatten() else { continue };
        let p = items[i].path(link);
        match fs.hash(&p, false, &|| false, &|_| {}) {
            Ok(Some(h)) if h == want => {}
            Ok(_) => return Err(format!("{} does not read back the same through the link", items[i].name())),
            Err(e) => return Err(format!("{} could not be read through the link ({})", items[i].name(), win::describe(&e))),
        }
    }
    Ok(())
}

/// Which files the proof reads (indices into `items`).
pub fn sample(items: &[Item]) -> Vec<usize> {
    let files: Vec<usize> = (0..items.len()).filter(|&i| !items[i].dir).collect();
    let mut by_size = files.clone();
    by_size.sort_by(|&a, &b| items[b].size.cmp(&items[a].size).then(a.cmp(&b)));
    let mut budget = super::PROOF_BYTES;
    let mut out = Vec::new();
    let mut take = |i: usize, out: &mut Vec<usize>| {
        if items[i].size <= budget && !out.contains(&i) {
            budget -= items[i].size;
            out.push(i);
        }
    };
    for &i in by_size.iter().take(8) {
        take(i, &mut out);
    }
    let step = (files.len() / 24).max(1);
    for &i in files.iter().step_by(step).take(24) {
        take(i, &mut out);
    }
    out
}

/// A folder this move made goes, but only while it is what the journal says it made; an
/// unrecorded one only when it is empty (made right before a crash). Ok(None): gone; Ok(Some):
/// left, why.
fn remove_ours(fs: &dyn Fs, p: &Path, ident: Option<&win::Identity>) -> Result<Option<String>, String> {
    match fs.item(p, false) {
        Err(e) if win::is_gone(&e) => Ok(None),
        Err(e) => Err(format!("{} could not be read ({})", show(p), win::describe(&e))),
        Ok(i) if i.tag.is_none() && ident.is_some_and(|x| x.same(&i.ident)) => {
            fs.remove_tree(p).map_err(|e| format!("{} could not be removed ({})", show(p), win::describe(&e)))?;
            Ok(None)
        }
        Ok(i) if ident.is_none() && i.dir && i.tag.is_none() => match fs.remove_dir(p) {
            Ok(()) => Ok(None),
            Err(_) => Ok(Some(format!("{} was left: VibeSpace cannot tell it made it", show(p)))),
        },
        Ok(_) => Ok(Some(format!("{} was left: it is not what VibeSpace made", show(p)))),
    }
}

/// Is `link` our junction to `target`? Ok(None): nothing there. Err: something else is.
fn our_link(fs: &dyn Fs, link: &Path, target: &Path) -> Result<Option<bool>, String> {
    match fs.item(link, false) {
        Err(e) if win::is_gone(&e) => Ok(None),
        Err(e) => Err(format!("{} could not be read ({})", show(link), win::describe(&e))),
        Ok(i) if i.tag == Some(TAG_MOUNT_POINT) => match fs.junction_target(link) {
            Ok(Some(t)) if same_path(&t, target) => Ok(Some(true)),
            Ok(_) => Err(format!("{} is a link VibeSpace did not make: left as it is", show(link))),
            Err(e) => Err(format!("{} could not be read ({})", show(link), win::describe(&e))),
        },
        Ok(_) => Ok(Some(false)),
    }
}

fn write(j: &Journal, r: &Record) -> Result<(), String> {
    j.write(r).map_err(|e| format!("the journal could not be written ({})", win::describe(&e)))
}

/// What is aside: the original (Some(true)), nothing (Some(false)); Err: something else.
fn original_aside(fs: &dyn Fs, rec: &Record) -> Result<bool, String> {
    let aside = rec.aside();
    match fs.item(&aside, false) {
        Ok(i) if i.tag.is_none() && i.ident.same(&rec.source_ident) => Ok(true),
        Ok(_) => Err(format!("{} is not the original: left as it is", show(&aside))),
        Err(e) if win::is_gone(&e) => Ok(false),
        Err(e) => Err(format!("{} could not be read ({})", show(&aside), win::describe(&e))),
    }
}

/// Is this the original (this move's folder, not a link)?
fn is_original(rec: &Record) -> impl Fn(&win::Item) -> bool + '_ {
    |i| i.tag.is_none() && i.ident.same(&rec.source_ident)
}

/// Is this the copy this move made?
fn is_copy(rec: &Record) -> impl Fn(&win::Item) -> bool + '_ {
    |i| i.tag.is_none() && rec.target_ident.as_ref().is_some_and(|t| t.same(&i.ident))
}

/// Rename the original from aside back into its place, only while it is the original.
fn rename_back(fs: &dyn Fs, rec: &Record) -> Result<(), String> {
    let (src, aside) = (rec.source(), rec.aside());
    match fs.rename_if(&aside, &src, &is_original(rec)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("{} is not the original: left as it is", show(&aside))),
        Err(e) => Err(format!("{} could not be renamed back to {} ({})", show(&aside), show(&src), win::describe(&e))),
    }
}

/// Put a move that did not finish back as it was (phases Copying and Aside). Everything is
/// looked at first; then the original comes back from aside (our link, if one is up, going
/// right before it, and only when the original is there to take its place), then our copy goes
/// and the journal with it. `want`: the copy as it landed, when known (else the original aside
/// stands in for it, and every file is compared by content too). A link up with the original
/// gone finishes the move instead; a copy written to through it (the proof having failed) keeps
/// both, the copy in use and the original aside; a compare that cannot tell keeps everything
/// and the journal. Each step looks first, so it can run again after a partial run.
pub fn put_back(fs: &dyn Fs, j: &Journal, rec: &Record, want: Option<&[Item]>) -> Result<Back, String> {
    let (src, target, aside) = (rec.source(), rec.target(), rec.aside());
    let link = our_link(fs, &src, &target)?;
    if link == Some(false) {
        // Something real at the old path: the original (never taken aside, or put back
        // already), or not ours to touch.
        match fs.item(&src, false) {
            Ok(i) if i.ident.same(&rec.source_ident) => return drop_copy(fs, j, rec),
            Ok(_) => return Err(format!("{} is not the folder that was being moved: left as it is", show(&src))),
            Err(e) => return Err(format!("{} could not be read ({})", show(&src), win::describe(&e))),
        }
    }
    let original = original_aside(fs, rec)?;
    match (link, original) {
        (Some(true), false) => {
            // The link is the one way left to the data: it stays, and the move is finished.
            let mut done = rec.clone();
            done.phase = Phase::Done;
            write(j, &done)?;
            return Ok(Back::Finished(vec!["the original is already gone; the moved copy is in use".into()]));
        }
        (None, false) => return Err(format!("The original is neither at {} nor at {}: the copy at {} is kept", show(&src), show(&aside), show(&target))),
        (Some(true), true) => {
            // The proof failed and programs may have written into the copy through the link:
            // only a copy that is as it was copied is put back.
            let reference: Vec<Item> = match want {
                Some(w) => w.to_vec(),
                None => {
                    let w = super::walk(fs, &aside, &|| false).map_err(|e| format!("{} could not be read: {e}", show(&aside)))?;
                    w.items.into_iter().map(|i| Item { ctime: 0, id: 0, ..i }).collect()
                }
            };
            // In a recovery the original stands in for the copy as it landed, and the link may
            // have been up for days: what names, sizes and times cannot show, the bytes do.
            // Against the copy as it landed (same volume) times count; against the original
            // (another volume) the bytes do instead (R-SPACE-MV3-1).
            let checked = compare(fs, &target, &reference, want.is_some()).and_then(|()| if want.is_none() { same_bytes(fs, &aside, &target, &reference, &quiet()) } else { Ok(()) });
            match checked {
                Ok(()) => {}
                Err(Odd::Differs(why)) => {
                    // The copy holds new writes, and it failed its check: neither goes. The
                    // link stays (programs are on the copy), the original stays aside, named.
                    let mut notes = vec![format!(
                        "programs wrote into the copy through the link ({}): both are kept, the copy in use and the original at {}",
                        lower(&why).replace(" while it was copied", ""),
                        show(&aside)
                    )];
                    notes.extend(finish(fs, j, rec, Original::Keep, &quiet())?);
                    return Ok(Back::Finished(notes));
                }
                Err(odd @ (Odd::Unknown(_) | Odd::Stopped)) => {
                    let why = odd.why();
                    return Err(format!(
                        "VibeSpace could not tell whether programs wrote into the copy through the link ({}): both are kept, the original at {} and the copy at {}",
                        lower(&why),
                        show(&aside),
                        show(&target)
                    ));
                }
            }
            fs.remove_dir(&src).map_err(|e| format!("The link at {} could not be removed ({})", show(&src), win::describe(&e)))?;
            fs.checkpoint("unlinked");
            rename_back(fs, rec)?;
            fs.checkpoint("put back");
        }
        (None, true) => {
            rename_back(fs, rec)?;
            fs.checkpoint("put back");
        }
        (Some(false), _) => unreachable!("handled above"),
    }
    drop_copy(fs, j, rec)
}

/// The original is in its place: our copy goes, then the journal.
fn drop_copy(fs: &dyn Fs, j: &Journal, rec: &Record) -> Result<Back, String> {
    let mut notes = Vec::new();
    if let Some(n) = remove_ours(fs, &rec.target(), rec.target_ident.as_ref())? {
        notes.push(n);
    }
    j.remove(&rec.id).map_err(|e| format!("its journal could not be removed ({})", win::describe(&e)))?;
    Ok(Back::Put(notes))
}

/// Finish a move forward (phases Linking and Removing): the copy must be ours; our link is
/// made again where it is missing (the old path empty); the destination drive writes its cache
/// to the disk; then the original aside goes (only while it is the original, never with
/// `Original::Keep`, and with `Original::Remove` only once the copy is checked to hold it: else
/// both are kept and the journal stays as it is), and the record says done.
fn finish(fs: &dyn Fs, j: &Journal, rec: &Record, keep: Original, w: &Watch) -> Result<Vec<String>, String> {
    let (src, target, aside) = (rec.source(), rec.target(), rec.aside());
    match fs.item(&target, false) {
        Ok(i) if rec.target_ident.as_ref().is_some_and(|t| t.same(&i.ident)) => {}
        _ => return Err(format!("{} is not the copy VibeSpace made: left as it is", show(&target))),
    }
    let mut notes = Vec::new();
    let original = match original_aside(fs, rec) {
        Ok(o) => o,
        Err(e) => {
            notes.push(e);
            false
        }
    };
    match our_link(fs, &src, &target)? {
        Some(true) => {}
        None => {
            fs.junction(&src, &target).map_err(|e| format!("The link at {} could not be made ({})", show(&src), win::describe(&e)))?;
            fs.checkpoint("relinked");
            match fs.item(&src, true) {
                Ok(i) if rec.target_ident.as_ref().is_some_and(|t| t.same(&i.ident)) => {}
                _ => return Err(format!("The link at {} does not lead to the copy", show(&src))),
            }
        }
        Some(false) => return Err(format!("{} is no longer the link to {}: left as it is", show(&src), show(&target))),
    }
    if original && keep != Original::Keep {
        if keep == Original::Remove {
            // Cleared on the wire whatever it found, as in the move: a failed check leaves no
            // "Checking … k of n" and no Stop while the window learns where each side is
            // (R-SPACE-MV5-1).
            let checked = copy_holds_the_original(fs, rec, w);
            if let Some(b) = w.beat {
                b.set(|p| (p.against, p.removing) = (None, checked.is_ok()), true);
            }
            checked.map_err(Odd::why)?;
            fs.checkpoint("checked");
        }
        fs.flush(&target).map_err(|e| format!("{} could not write the copy to the disk for certain ({}): the original is kept at {}", drive_of(&target), win::describe(&e), show(&aside)))?;
        fs.checkpoint("flushed");
        fs.remove_tree(&aside).map_err(|e| format!("The old copy at {} could not be removed ({})", show(&aside), win::describe(&e)))?;
        fs.checkpoint("cleared");
    }
    let mut done = rec.clone();
    done.phase = Phase::Done;
    write(j, &done).map_err(|e| format!("The move is done, but {e}"))?;
    Ok(notes)
}

/// `D:` of a path, for messages ("the drive" when it has no letter).
fn drive_of(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.as_bytes().get(1) == Some(&b':') { s[..2].to_string() } else { "The drive".into() }
}

/// Put a move back that did not finish back (phases BackCopying and BackAside): the link must
/// still be ours; the copy comes back from aside; the copied-back folder goes.
pub fn put_back_back(fs: &dyn Fs, j: &Journal, rec: &Record) -> Result<Vec<String>, String> {
    let (src, target, taside) = (rec.source(), rec.target(), rec.target_aside());
    if our_link(fs, &src, &target)? != Some(true) {
        return Err(format!("{} is no longer the link to {}: left as it is", show(&src), show(&target)));
    }
    match fs.item(&target, false) {
        Ok(i) if rec.target_ident.as_ref().is_some_and(|t| t.same(&i.ident)) => {}
        Ok(_) => return Err(format!("{} is not the copy VibeSpace made: left as it is", show(&target))),
        Err(e) if win::is_gone(&e) => match fs.rename_if(&taside, &target, &is_copy(rec)) {
            Ok(true) => {}
            Ok(false) => return Err(format!("The copy is neither at {} nor at {}", show(&target), show(&taside))),
            Err(e) if win::is_gone(&e) => return Err(format!("The copy is neither at {} nor at {}", show(&target), show(&taside))),
            Err(e) => return Err(format!("{} could not be renamed back ({})", show(&taside), win::describe(&e))),
        },
        Err(e) => return Err(format!("{} could not be read ({})", show(&target), win::describe(&e))),
    }
    let mut notes = Vec::new();
    if let Some(n) = remove_ours(fs, &rec.staging(), rec.staging_ident.as_ref())? {
        notes.push(n);
    }
    let mut done = rec.clone();
    done.phase = Phase::Done;
    done.staging_ident = None;
    write(j, &done)?;
    Ok(notes)
}

/// Finish a move back whose link was going (phase BackSwapped): everything is looked at first
/// (a copy aside that cannot be read, its drive perhaps not there, stops it here: RS-3); (with
/// `check`: a recovery) the copied-back folder is checked to hold the copy aside
/// (`folder_holds_the_copy`) while it is still at its own name, where no program uses it
/// (RS-2); our link goes, the copied-back folder takes the old name (after a crash past that
/// rename it is checked there instead), the old path's drive writes its cache to the disk, the
/// copy aside goes, the record with it. The move back itself passes no `check`: it has just
/// copied every file back hashed on both sides, found the copy aside unchanged since (names,
/// sizes, times, ids, no holder) and read every file of it again against those hashes (RS-1).
pub fn finish_back(fs: &dyn Fs, j: &Journal, rec: &Record, check: bool) -> Result<Vec<String>, String> {
    let (src, target, staging, taside) = (rec.source(), rec.target(), rec.staging(), rec.target_aside());
    let ours = |i: &win::Item| rec.staging_ident.as_ref().is_some_and(|s| s.same(&i.ident));
    let link = our_link(fs, &src, &target)?;
    let staged = match fs.item(&staging, false) {
        Ok(i) if ours(&i) => true,
        Ok(_) => return Err(format!("{} is not the folder VibeSpace copied back: left as it is", show(&staging))),
        Err(e) if win::is_gone(&e) => false,
        Err(e) => return Err(format!("{} could not be read ({})", show(&staging), win::describe(&e))),
    };
    // The way out when the copy aside really is gone (DECISIONS "Known limits"), said with each
    // refusal below that may be that case (RS3-4). Without staging, only when the folder at the
    // old path is the one copied back (RL-4): else Forget would drop the only record of the data.
    let if_gone = || {
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if staged {
            format!(" If the copy is really gone, Forget this record, remove the link at {} if one is left, and rename {} to {name}.", show(&src), show(&staging))
        } else if fs.item(&src, false).is_ok_and(|i| ours(&i)) {
            format!(" If the copy is really gone, Forget this record: the folder is back at {}.", show(&src))
        } else {
            String::new()
        }
    };
    let aside = match fs.item(&taside, false) {
        Ok(i) => rec.target_ident.as_ref().is_some_and(|t| t.same(&i.ident)),
        // The one place a copy aside counts as gone: the removal below finds it gone only
        // after this, or after it was checked (or read again, RS-1) to be a duplicate.
        Err(e) if win::is_gone(&e) => match parent_answers(fs, &taside, rec.target_ident.as_ref()) {
            Parent::Answers => false,
            Parent::Unreached => return Err(format!("The copy at {} could not be read: the folder it is in cannot be reached (is its drive connected?): it is kept, and so is the journal.{}", show(&taside), if_gone())),
            Parent::OtherVolume => {
                let letter = drive_of(&taside);
                let took = if letter.ends_with(':') { format!(" (did another drive take {letter}?)") } else { String::new() };
                return Err(format!("The copy at {} could not be read: the folder it is in is on another drive than the copy was{took}: it is kept, and so is the journal.{}", show(&taside), if_gone()));
            }
            Parent::Unknown => return Err(format!("The copy at {} could not be read: the drive of the folder it is in is not known (a volume serial of 0): it is kept, and so is the journal.{}", show(&taside), if_gone())),
            Parent::CopyUnknown => return Err(format!("The copy at {} could not be read: the drive the copy was on is not known (a volume serial of 0 in the journal): it is kept, and so is the journal.{}", show(&taside), if_gone())),
        },
        Err(e) => return Err(format!("The copy at {} could not be read ({}; is its drive connected?): it is kept, and so is the journal", show(&taside), win::describe(&e))),
    };
    let early = check && aside && staged;
    if early {
        folder_holds_the_copy(fs, rec, &staging)?;
        fs.checkpoint("back-checked");
    }
    match (link, staged) {
        (Some(true), true) => {
            fs.remove_dir(&src).map_err(|e| format!("The link at {} could not be removed ({})", show(&src), win::describe(&e)))?;
            fs.checkpoint("unlinked");
        }
        (Some(true), false) => return Err(format!("The copied-back folder is not at {}: {} still leads to {}", show(&staging), show(&src), show(&target))),
        _ => {}
    }
    match fs.item(&src, false) {
        Ok(i) if ours(&i) => {}
        Ok(_) => return Err(format!("{} is not the folder VibeSpace copied back: left as it is", show(&src))),
        Err(e) if win::is_gone(&e) && staged => {
            match fs.rename_if(&staging, &src, &|i: &win::Item| i.tag.is_none() && ours(i)) {
                Ok(true) => {}
                Ok(false) => return Err(format!("{} is not the folder VibeSpace copied back: left as it is", show(&staging))),
                Err(e) => return Err(format!("{} could not be renamed to {} ({})", show(&staging), show(&src), win::describe(&e))),
            }
            fs.checkpoint("renamed");
        }
        Err(e) if win::is_gone(&e) => return Err(format!("The copied-back folder is neither at {} nor at {}", show(&src), show(&staging))),
        Err(e) => return Err(format!("{} could not be read ({})", show(&src), win::describe(&e))),
    }
    let mut notes = Vec::new();
    if aside {
        if check && !early {
            folder_holds_the_copy(fs, rec, &src)?;
            fs.checkpoint("back-checked");
        }
        fs.flush(&src).map_err(|e| format!("{} could not write the folder to the disk for certain ({}): the copy is kept at {}", drive_of(&src), win::describe(&e), show(&taside)))?;
        fs.checkpoint("back-flushed");
    }
    match remove_ours(fs, &taside, rec.target_ident.as_ref()) {
        Ok(Some(n)) => notes.push(n),
        Ok(None) => {}
        Err(e) => return Err(e),
    }
    j.remove(&rec.id).map_err(|e| format!("its journal could not be removed ({})", win::describe(&e)))?;
    Ok(notes)
}

/// The one safe action for an unfinished journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    PutBack,
    Finish,
}

pub fn fix_of(rec: &Record) -> Option<Fix> {
    match rec.phase {
        Phase::Copying | Phase::Aside | Phase::BackCopying | Phase::BackAside => Some(Fix::PutBack),
        Phase::Linking | Phase::Removing | Phase::BackSwapped => Some(Fix::Finish),
        Phase::Done => None,
    }
}

/// Recover an unfinished move (at start, from its journal). Under the move's lock, and on the
/// journal as it is now (the record handed in may be what a dialog read long ago). Ok: what was
/// done, in words.
pub fn recover(fs: &dyn Fs, env: &Env, rec: &Record) -> Result<String, String> {
    recover_with(fs, env, rec, &AtomicBool::new(false), &|_| {})
}

/// `recover`, its long read (the check before the original goes) told to `progress` and
/// stopped by `stop` (R-SPACE-MV3-3): a Stop keeps both and the journal, in words that begin
/// with `STOPPED`.
pub fn recover_with(fs: &dyn Fs, env: &Env, rec: &Record, stop: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<String, String> {
    let beat = Beat::with(progress, Progress { finishing: true, ..Progress::default() });
    let halt = || stop.load(Ordering::Relaxed);
    let w = Watch { stop: &halt, beat: Some(&beat) };
    let j = journal(env).map_err(|f| f.why)?;
    let _lock = lock(j, &rec.id)?;
    let (src, target) = (show(&rec.source()), show(&rec.target()));
    let Some(rec) = j.read(&rec.id).map_err(|e| format!("Its journal could not be read again ({e})"))? else {
        return Ok(format!("Nothing to do: the move of {src} was finished in another VibeSpace window"));
    };
    if let Some(why) = super::journal_refusal(fs, env, &rec) {
        return Err(format!("Nothing was done, and its journal is kept: {}", lower(&why)));
    }
    let with = |s: String, notes: Vec<String>| if notes.is_empty() { s } else { format!("{s} ({})", notes.join("; ")) };
    let finished = |n: Vec<String>| with(format!("Finished: {src} leads to {target}"), n);
    match rec.phase {
        Phase::Copying | Phase::Aside => put_back(fs, j, &rec, None).map(|b| match b {
            Back::Put(n) => with(format!("Put back: {src} is as it was"), n),
            Back::Finished(n) => finished(n),
        }),
        Phase::Linking | Phase::Removing => finish(fs, j, &rec, Original::Remove, &w).map(finished),
        Phase::BackCopying | Phase::BackAside => put_back_back(fs, j, &rec).map(|n| with(format!("Put back: {src} still leads to {target}"), n)),
        Phase::BackSwapped => finish_back(fs, j, &rec, true).map(|n| with(format!("Finished: {src} is back on its own drive"), n)),
        Phase::Done => Ok(format!("Nothing to do: the move of {src} was finished in another VibeSpace window")),
    }
}

/// Of the done records of moves of `link` (a folder moved, its link removed by hand, made again
/// and moved again has several), the one whose copy the link at `link` leads to now; else the
/// newest (its Move back plan then says where the link leads instead). Times cannot order them:
/// two moves in one second tie, a clock set back reverses them (R-SPACE-MV2-5). Reads the link:
/// a worker's thread only.
pub fn record_for_link(fs: &dyn Fs, records: &[Record], link: &Path) -> Option<Record> {
    let done: Vec<&Record> = records.iter().filter(|r| r.phase == Phase::Done && same_path(&r.source(), link)).collect();
    let leads = fs.junction_target(link).ok().flatten();
    leads.and_then(|t| done.iter().rev().find(|r| same_path(&r.target(), &t))).or(done.last()).map(|r| (*r).clone())
}

/// One place a move that cannot go on left something, for the window: what it is, where, and
/// what is there now.
#[derive(Clone, Debug, PartialEq)]
pub struct Side {
    pub label: String,
    pub path: std::path::PathBuf,
    pub state: String,
}

/// Where each side of a move is (R-SPACE-MV2-6), looked at now: the old path, the original
/// aside, the copy (a move back: also the folder copied back, the copy aside). A place that
/// holds nothing is shown only for the old path and the copy. Looks, changes nothing.
pub fn sides(fs: &dyn Fs, rec: &Record) -> Vec<Side> {
    let read = |p: &Path, what: &dyn Fn(&win::Item) -> String, always: bool| -> Option<String> {
        match fs.item(p, false) {
            Ok(i) => Some(what(&i)),
            Err(e) if win::is_gone(&e) => always.then(|| "nothing there".to_string()),
            Err(e) => Some(format!("could not be read ({})", win::describe(&e))),
        }
    };
    let back = matches!(rec.phase, Phase::BackCopying | Phase::BackAside | Phase::BackSwapped);
    let (src, target) = (rec.source(), rec.target());
    let folder = |i: &win::Item| match i.tag {
        Some(TAG_MOUNT_POINT) if fs.junction_target(&src).ok().flatten().is_some_and(|t| same_path(&t, &target)) => "the link to the copy".to_string(),
        Some(_) => "a link VibeSpace did not make".to_string(),
        None if !i.dir => "a file, not the folder that was being moved".to_string(),
        None if i.ident.same(&rec.source_ident) => "the folder that was being moved".to_string(),
        None if rec.staging_ident.as_ref().is_some_and(|s| s.same(&i.ident)) => "the folder VibeSpace copied back".to_string(),
        None => "another folder, not the one that was being moved".to_string(),
    };
    let copy = |i: &win::Item| if is_copy(rec)(i) { "the copy VibeSpace made".to_string() } else { "not the copy VibeSpace made".to_string() };
    // An aside with fewer files than were moved is part removed (a removal cut short): it is
    // what is left of the original, never "the original" (R-SPACE-MV3-5).
    let original = |i: &win::Item| {
        if !is_original(rec)(i) {
            return "not the original".to_string();
        }
        match super::walk(fs, &rec.aside(), &|| false) {
            Ok(w) => {
                let n = w.items.iter().filter(|i| !i.dir).count() as u64;
                if n < rec.files { format!("what is left of the original ({n} of {} files)", rec.files) } else { "the original, taken aside".to_string() }
            }
            Err(_) => "the original, taken aside (it could not be read in full)".to_string(),
        }
    };
    let staged = |i: &win::Item| if i.tag.is_none() && rec.staging_ident.as_ref().is_some_and(|s| s.same(&i.ident)) { "the folder VibeSpace copied back".to_string() } else { "not the folder VibeSpace copied back".to_string() };
    // Each place: its label, its path, what is found there in words, shown even when it holds nothing.
    type Place<'a, 'b> = (&'a str, std::path::PathBuf, &'b dyn Fn(&win::Item) -> String, bool);
    let mut places: Vec<Place<'_, '_>> = vec![("FOLDER", src.clone(), &folder, true)];
    if back {
        places.push(("COPIED BACK", rec.staging(), &staged, false));
        places.push(("COPY", target.clone(), &copy, true));
        places.push(("COPY ASIDE", rec.target_aside(), &copy, false));
    } else {
        places.push(("ORIGINAL", rec.aside(), &original, false));
        places.push(("COPY", target.clone(), &copy, true));
    }
    places.into_iter().filter_map(|(label, path, what, always)| read(&path, what, always).map(|state| Side { label: label.into(), path, state })).collect()
}

/// Forget the record of a move that cannot go on (R-SPACE-MV2-6): only its journal goes,
/// nothing on the disk is touched. Under the move's lock, and only while the journal is still
/// the one shown (`rec`). Ok: what was done, in words.
pub fn forget(env: &Env, rec: &Record) -> Result<String, String> {
    let j = journal(env).map_err(|f| f.why)?;
    let _lock = lock(j, &rec.id)?;
    let src = show(&rec.source());
    match j.read(&rec.id).map_err(|e| format!("Its journal could not be read again ({e}): nothing was forgotten"))? {
        None => Ok(format!("Nothing to forget: the move of {src} was finished in another VibeSpace window")),
        Some(now) if now != *rec => Err(format!("The record of the move of {src} changed since it was shown: nothing was forgotten")),
        Some(_) => {
            j.remove(&rec.id).map_err(|e| format!("The record of the move of {src} could not be removed ({})", win::describe(&e)))?;
            Ok(format!("Forgot the record of the move of {src}: nothing on the disk was changed"))
        }
    }
}

/// A failure during a move before its link exists: put back by the same code a crash is
/// recovered with.
fn rollback(fs: &dyn Fs, j: &Journal, rec: &Record, why: String, back: bool) -> Failed {
    let r = if back { put_back_back(fs, j, rec) } else { put_back(fs, j, rec, None).map(|b| match b { Back::Put(n) | Back::Finished(n) => n }) };
    match r {
        Ok(notes) if notes.is_empty() => Failed { why: format!("{why}: nothing changed"), kept: false },
        Ok(notes) => Failed { why: format!("{why}: nothing changed ({})", notes.join("; ")), kept: false },
        Err(e) => Failed { why: format!("{why}; putting it back did not finish ({e}): the journal keeps it, and VibeSpace offers it again"), kept: true },
    }
}

/// Who holds files in a folder that could not be taken aside, in words.
fn in_use(fs: &dyn Fs, root: &Path, items: &[Item], e: &std::io::Error) -> String {
    match super::in_use(fs, root, items, &|| false) {
        Ok(Some(names)) => format!("It could not be taken aside: files in it are in use by {names}"),
        _ if matches!(e.raw_os_error(), Some(5 | 32 | 33)) => "It could not be taken aside: a program has a file in it open (or is working inside it)".into(),
        _ => format!("It could not be taken aside ({})", win::describe(e)),
    }
}

/// Move `req.source` into `req.dest` and leave a link (SPEC.md §6.3). `cancel` is read while
/// copying only; the check before the original goes cannot be stopped here (`move_folder_with`).
pub fn move_folder(fs: &dyn Fs, env: &Env, req: &Request, cancel: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Moved, Failed> {
    move_folder_with(fs, env, req, cancel, &AtomicBool::new(false), progress)
}

/// `move_folder` with the check's own Stop: Cancel (`stop` while copying) and Stop (`check`
/// while checking the copy against the original) are two flags, so a Cancel that lands as the
/// copy ends is ignored as it always was, never read as a Stop (R-SPACE-MV4-2).
pub fn move_folder_with(fs: &dyn Fs, env: &Env, req: &Request, stop: &AtomicBool, check: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Moved, Failed> {
    let j = journal(env)?;
    // The plan is a plan: everything is looked at again, right now.
    let plan: Plan = super::plan(fs, env, req, &|| stop.load(Ordering::Relaxed));
    if let Some(r) = plan.refusals.first() {
        return Err(Failed { why: format!("Nothing was moved: {}", lower(r)), kept: false });
    }
    let (from, to) = (plan.from.clone(), plan.to.clone());
    let id = j.new_id(&from);
    let _lock = lock(j, &id).map_err(|e| Failed { why: format!("Nothing was moved: {}", lower(&e)), kept: false })?;
    let dacl = fs.dacl(&from).map_err(|e| Failed { why: format!("Nothing was moved: its permissions could not be read ({})", win::describe(&e)), kept: false })?;
    let beat = Beat::new(progress, plan.bytes);
    let mut rec = Record {
        id,
        phase: Phase::Copying,
        source: from.to_string_lossy().into_owned(),
        target: to.to_string_lossy().into_owned(),
        source_ident: plan.from_ident.clone(),
        target_ident: None,
        staging_ident: None,
        dacl,
        files: plan.files,
        bytes: plan.bytes,
        at: crate::format::now_unix() as u64,
        target_real: plan.to_real.to_string_lossy().into_owned(),
    };
    write(j, &rec).map_err(|e| Failed { why: format!("Nothing was moved: {e}"), kept: false })?;
    fs.checkpoint("journaled");
    // Asked again now that this move's journal is there: of two windows starting a move of
    // the same folder at once, at least one sees the other's (R-SPACE-MV2-1).
    if let Some((_, other)) = super::another_move(j, &from, Some(&plan.from_ident), &rec.id) {
        let _ = j.remove(&rec.id);
        return Err(Failed { why: format!("Nothing was moved: {}", lower(&other)), kept: false });
    }
    if let Err(e) = fs.create_dir(&to) {
        let _ = j.remove(&rec.id);
        return Err(Failed { why: format!("Nothing was moved: {} could not be made ({})", show(&to), win::describe(&e)), kept: false });
    }
    fs.checkpoint("created");
    // Its creation time now, before its identity (which holds it) is journaled; its modified
    // time again once everything is in it.
    if let Err(e) = fs.copy_dir_times(&from, &to) {
        rec.target_ident = fs.item(&to, false).ok().map(|i| i.ident);
        return Err(rollback(fs, j, &rec, format!("The folder's times could not be carried ({})", win::describe(&e)), false));
    }
    rec.target_ident = fs.item(&to, false).ok().map(|i| i.ident);
    if rec.target_ident.as_ref().is_none_or(|t| t.volume == 0) {
        return Err(rollback(fs, j, &rec, format!("{} could not be read back", show(&to)), false));
    }
    if let Err(e) = write(j, &rec) {
        return Err(rollback(fs, j, &rec, format!("Nothing was moved: {e}"), false));
    }
    // Its permissions go with it, or it does not go (a private folder must not land open to
    // every account on the destination).
    if (plan.to_fs.eq_ignore_ascii_case("NTFS") || plan.to_fs.eq_ignore_ascii_case("ReFS"))
        && let Err(e) = fs.copy_dacl(&from, &to)
    {
        return Err(rollback(fs, j, &rec, format!("Its permissions could not be carried to {} ({})", show(&to), win::describe(&e)), false));
    }
    fs.copy_dir_attributes(&from, &to);
    let copied = match copy_all(fs, &plan.items, &from, &to, stop, &beat) {
        Ok(c) => c,
        Err(why) => return Err(rollback(fs, j, &rec, why, false)),
    };
    fs.checkpoint("copied");
    beat.set(|p| p.finishing = true, true);
    rec.phase = Phase::Aside;
    if let Err(e) = write(j, &rec) {
        return Err(rollback(fs, j, &rec, e, false));
    }
    fs.checkpoint("aside-journaled");
    let aside = rec.aside();
    // Only the folder that was planned is taken aside, judged on the handle that renames it:
    // never another move's link or a folder made again under its name (R-SPACE-MV2-1).
    match fs.rename_if(&from, &aside, &|i: &win::Item| i.tag.is_none() && i.ident.same(&plan.from_ident)) {
        Ok(true) => {}
        Ok(false) => {
            // Nothing at the old path was touched: this move's copy goes, and its journal.
            let why = format!("{} is no longer the folder that was planned", show(&from));
            return Err(match drop_copy(fs, j, &rec) {
                Ok(Back::Put(n) | Back::Finished(n)) if n.is_empty() => Failed { why: format!("{why}: nothing changed"), kept: false },
                Ok(Back::Put(n) | Back::Finished(n)) => Failed { why: format!("{why}: nothing changed ({})", n.join("; ")), kept: false },
                Err(e) => Failed { why: format!("{why}; its copy could not be cleared ({e}): the journal keeps it, and VibeSpace offers it again"), kept: true },
            });
        }
        Err(e) => {
            let why = in_use(fs, &from, &plan.items, &e);
            return Err(rollback(fs, j, &rec, why, false));
        }
    }
    fs.checkpoint("aside");
    if let Err(odd) = compare(fs, &aside, &copied.source, true) {
        return Err(rollback(fs, j, &rec, odd.why(), false));
    }
    fs.checkpoint("compared");
    // From the link on, the move goes forward: the journal says so before the link exists.
    rec.phase = Phase::Linking;
    if let Err(e) = write(j, &rec) {
        return Err(rollback(fs, j, &rec, e, false));
    }
    fs.checkpoint("linking-journaled");
    if let Err(e) = fs.junction(&from, &to) {
        rec.phase = Phase::Aside;
        let _ = write(j, &rec);
        return Err(rollback(fs, j, &rec, format!("The link could not be made ({})", win::describe(&e)), false));
    }
    fs.checkpoint("linked");
    let mut notes = Vec::new();
    let target_ident = rec.target_ident.clone().unwrap_or_default();
    if let Err(why) = prove(fs, &from, &target_ident, &plan.items, &copied) {
        // Put back only a copy that is as it landed (`put_back` looks); else it is the data.
        rec.phase = Phase::Aside;
        let backed_off = write(j, &rec).is_ok();
        fs.checkpoint("backing-off");
        // The original is never removed behind a copy that failed its check (R-SPACE-MV2-3).
        let r = if backed_off { put_back(fs, j, &rec, Some(&copied.copy)) } else { finish(fs, j, &rec, Original::Keep, &quiet()).map(Back::Finished) };
        return match r {
            Ok(Back::Put(n)) if n.is_empty() => Err(Failed { why: format!("{why}: nothing changed"), kept: false }),
            Ok(Back::Put(n)) => Err(Failed { why: format!("{why}: nothing changed ({})", n.join("; ")), kept: false }),
            Ok(Back::Finished(n)) => {
                rec.phase = Phase::Done;
                Ok(Moved { record: rec, notes: [format!("the check through the link failed ({})", lower(&why))].into_iter().chain(n).collect() })
            }
            Err(e) => Err(Failed { why: format!("{why}; putting it back did not finish ({e}): the journal keeps it, and VibeSpace offers it again"), kept: true }),
        };
    }
    fs.checkpoint("proved");
    // Before the original goes, the copy must still hold it, every file by content. A real
    // difference (a file of the original changed or gone in the copy) keeps both, and the
    // record is done (as after a failed proof: Move back stays possible); a copy that cannot
    // be read keeps both and the journal at Linking (the next start checks again).
    let halt = || check.load(Ordering::Relaxed);
    let checked = copy_holds_the_original(fs, &rec, &Watch { stop: &halt, beat: Some(&beat) });
    // Cleared on the wire at once: from here nothing can be stopped (R-SPACE-MV4-1).
    beat.set(|p| (p.against, p.removing) = (None, checked.is_ok()), true);
    match checked {
        Ok(()) => {}
        Err(Odd::Differs(why)) => {
            let mut notes = vec![lower(&why)];
            return match finish(fs, j, &rec, Original::Keep, &quiet()) {
                Ok(n) => {
                    notes.extend(n);
                    rec.phase = Phase::Done;
                    Ok(Moved { record: rec, notes })
                }
                Err(e) => Err(Failed { why: format!("{why}; {}: the journal keeps it, and VibeSpace offers it again", lower(&e)), kept: true }),
            };
        }
        Err(odd) => return Err(Failed { why: format!("{}; the journal keeps it, and VibeSpace offers it again", odd.why()), kept: true }),
    }
    fs.checkpoint("checked");
    // A journal that cannot be written now (C: full) changes nothing: Linking finishes forward
    // at the next start just as Removing does.
    rec.phase = Phase::Removing;
    if let Err(e) = write(j, &rec) {
        notes.push(format!("{e}: the move is done and the copy is in use; VibeSpace finishes its record at the next start"));
    }
    fs.checkpoint("removing");
    match finish(fs, j, &rec, Original::Checked, &quiet()) {
        Ok(n) => {
            notes.extend(n);
            rec.phase = Phase::Done;
        }
        Err(e) => notes.push(format!("{}; VibeSpace finishes it at the next start", lower(&e))),
    }
    fs.checkpoint("done");
    Ok(Moved { record: rec, notes })
}

/// Move a moved folder back to where it was (SPEC.md §6.4), under its lock and from its journal
/// as it is now. Ok: notes. `cancel` is read while copying only (`move_back_with`).
pub fn move_back(fs: &dyn Fs, env: &Env, rec: &Record, cancel: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Vec<String>, Failed> {
    move_back_with(fs, env, rec, cancel, &AtomicBool::new(false), progress)
}

/// `move_back` with the Stop of its read of the copy aside (`check`, its own flag as in
/// `move_folder_with`): a Stop puts it back, still moved (RS-1).
pub fn move_back_with(fs: &dyn Fs, env: &Env, rec: &Record, stop: &AtomicBool, check: &AtomicBool, progress: &dyn Fn(Progress)) -> Result<Vec<String>, Failed> {
    let j = journal(env)?;
    let _lock = lock(j, &rec.id).map_err(|e| Failed { why: e, kept: false })?;
    let rec = match j.read(&rec.id) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(Failed { why: "Nothing was moved back: it was moved back in another VibeSpace window".into(), kept: false }),
        Err(e) => return Err(Failed { why: format!("Nothing was moved back: its journal could not be read again ({e})"), kept: false }),
    };
    if rec.phase != Phase::Done {
        return Err(Failed { why: "This move has not finished: it is offered at the start instead".into(), kept: true });
    }
    let plan = super::plan_back(fs, env, &rec, &|| stop.load(Ordering::Relaxed));
    if let Some(r) = plan.refusals.first() {
        return Err(Failed { why: format!("Nothing was moved back: {}", lower(r)), kept: false });
    }
    let beat = Beat::new(progress, plan.bytes);
    let mut rec = rec;
    let (src, target, staging, taside) = (rec.source(), rec.target(), rec.staging(), rec.target_aside());
    rec.phase = Phase::BackCopying;
    rec.staging_ident = None;
    write(j, &rec).map_err(|e| Failed { why: format!("Nothing was moved back: {e}"), kept: false })?;
    fs.checkpoint("back-journaled");
    if let Err(e) = fs.create_dir(&staging) {
        return Err(rollback(fs, j, &rec, format!("{} could not be made ({})", show(&staging), win::describe(&e)), true));
    }
    // Its creation time before its identity is journaled (see `move_folder`).
    let times = fs.copy_dir_times(&target, &staging);
    rec.staging_ident = fs.item(&staging, false).ok().map(|i| i.ident);
    if let Err(e) = times {
        return Err(rollback(fs, j, &rec, format!("The folder's times could not be carried ({})", win::describe(&e)), true));
    }
    if let Err(e) = write(j, &rec) {
        return Err(rollback(fs, j, &rec, e, true));
    }
    fs.checkpoint("staging");
    // The folder's own permissions as they were before the move, before anything goes in it.
    if let Some(d) = &rec.dacl
        && let Err(e) = fs.set_dacl(&staging, d)
    {
        return Err(rollback(fs, j, &rec, format!("Its permissions could not be put back ({})", win::describe(&e)), true));
    }
    fs.copy_dir_attributes(&target, &staging);
    let copied = match copy_all(fs, &plan.items, &target, &staging, stop, &beat) {
        Ok(c) => c,
        Err(why) => return Err(rollback(fs, j, &rec, why, true)),
    };
    fs.checkpoint("back-copied");
    beat.set(|p| p.finishing = true, true);
    rec.phase = Phase::BackAside;
    if let Err(e) = write(j, &rec) {
        return Err(rollback(fs, j, &rec, e, true));
    }
    fs.checkpoint("back-aside-journaled");
    // Only the copy this move made is taken aside, judged on the handle that renames it.
    match fs.rename_if(&target, &taside, &is_copy(&rec)) {
        Ok(true) => {}
        Ok(false) => return Err(rollback(fs, j, &rec, format!("{} is no longer the copy VibeSpace made", show(&target)), true)),
        Err(e) => {
            let why = in_use(fs, &target, &plan.items, &e);
            return Err(rollback(fs, j, &rec, why, true));
        }
    }
    fs.checkpoint("back-aside");
    if let Err(odd) = compare(fs, &taside, &copied.source, true) {
        return Err(rollback(fs, j, &rec, odd.why(), true));
    }
    fs.checkpoint("back-compared");
    // The compare saw names, sizes, times, ids and no holder; the bytes are read too before
    // anything can remove the copy aside (RS-1). A difference, a read that fails or a Stop
    // puts it back: the copy is the data, the folder copied back goes.
    let halt = || check.load(Ordering::Relaxed);
    let read = reads_as_copied(fs, &taside, &copied, &Watch { stop: &halt, beat: Some(&beat) });
    // Cleared on the wire whatever it found: from here nothing can be stopped (R-SPACE-MV5-1).
    beat.set(|p| p.against = None, true);
    if let Err(odd) = read {
        let why = if odd == Odd::Stopped { BACK_STOPPED.to_string() } else { odd.why() };
        return Err(rollback(fs, j, &rec, why, true));
    }
    fs.checkpoint("back-read");
    rec.phase = Phase::BackSwapped;
    if let Err(e) = write(j, &rec) {
        return Err(rollback(fs, j, &rec, e, true));
    }
    fs.checkpoint("swapped-journaled");
    // From here the way is forward: the copied-back folder is checked and the copy is aside.
    // Only a link that cannot be removed now turns it back.
    if let Err(e) = fs.remove_dir(&src) {
        let mut back = rec.clone();
        back.phase = Phase::BackAside;
        let _ = write(j, &back);
        return Err(rollback(fs, j, &back, format!("The link at {} could not be removed ({})", show(&src), win::describe(&e)), true));
    }
    fs.checkpoint("unlinked");
    let notes = finish_back(fs, j, &rec, false).map_err(|e| Failed { why: format!("{e}; the journal keeps it, and VibeSpace offers to finish it"), kept: true })?;
    fs.checkpoint("back-done");
    Ok(notes)
}

pub(super) fn lower(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // A word ("It", "The"), not a path ("C:\x") or a name ("VibeSpace").
        Some(f) if f.is_ascii_uppercase() && s.chars().nth(1).is_some_and(|x| x.is_ascii_lowercase()) && !["VibeSpace", "OneDrive", "Windows"].iter().any(|n| s.starts_with(n)) => {
            f.to_lowercase().collect::<String>() + c.as_str()
        }
        _ => s.to_string(),
    }
}
