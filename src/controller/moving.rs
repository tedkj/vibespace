//! "Move to another drive and leave a link" in the window (SPEC.md §6): the destination picker,
//! the plan dialog (worked out on a thread), the move and Move back on a thread with progress,
//! and at start the journals, with the one safe action for a move that did not finish. Every
//! answer carries the move's seq; one for another seq does nothing.

use super::{App, Msg};
use crate::relocate::journal::{Phase, Record, Unreadable};
use crate::relocate::run::{self, Failed, Fix, Progress, Side};
use crate::relocate::{self, Plan, Request, same_path};
use crate::{MoveFact, ToastKind, format, tree};
use slint::{ModelRc, SharedString, VecModel};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Where the dialog is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    #[default]
    Closed,
    /// The plan is being worked out.
    Checking,
    /// The plan is shown: Move (or Move back) or Cancel.
    Plan,
    /// Moving; `finishing` in the progress once it can no longer be cancelled.
    Working,
    /// A move that did not finish: its one safe action, or Later.
    Unfinished,
    /// That action under way.
    Recovering,
    /// That action could not go on: why, and where each side is; Forget this record, or Later.
    Stuck,
    /// Forget this record? (what stays as it is listed).
    Forget,
    /// Forgetting it.
    Forgetting,
}

#[derive(Clone, Debug)]
pub enum What {
    Move(Request),
    Back(Box<Record>),
}

#[derive(Default)]
pub struct MoveState {
    pub seq: u64,
    pub stage: Stage,
    pub what: Option<What>,
    pub plan: Option<Plan>,
    /// Cancel: read while copying only.
    pub stop: Arc<AtomicBool>,
    /// Stop: the check before the original goes (its own flag, R-SPACE-MV4-2).
    pub halt: Arc<AtomicBool>,
    pub progress: Progress,
    /// The folder's parent in the tree (read again after), and the tree it is in.
    pub parent: Option<(u64, u32)>,
    /// The journals as last read: done ones offer Move back.
    pub records: Vec<Record>,
    pub unfinished: Option<Record>,
    /// Why the unfinished move cannot go on, and where each side is.
    pub stuck: Option<(String, Vec<Side>)>,
    /// "Later" was picked: not offered again until the next start.
    pub later: bool,
    pub reading: usize,
    /// Work on a thread (a plan, a move, a recovery).
    pub busy: bool,
    /// Unreadable journals were told this session.
    pub told_bad: bool,
}

/// A path for the dialog: middle-shortened to `room` characters, with a break opportunity
/// (U+200B) after each separator so a long one wraps at its folders, never inside a name.
pub fn wrap_path(p: &Path, room: usize) -> String {
    breakable(&format::middle(&relocate::show(p), room))
}

/// Text with a break opportunity after each `\` (paths inside sentences wrap at separators).
pub fn breakable(s: &str) -> String {
    s.replace('\\', "\\\u{200B}")
}

impl App {
    /// The done records of moves of `path`, oldest first (a folder moved, its link removed by
    /// hand and moved again has two). Which one the link leads to is read on the plan's thread
    /// (`run::record_for_link`).
    fn records_for(&self, path: &Path) -> Vec<Record> {
        self.st.borrow().moving.records.iter().filter(|r| r.phase == crate::relocate::journal::Phase::Done && same_path(&r.source(), path)).cloned().collect()
    }

    /// The move that did not finish whose folder is `path` (now its link), if any.
    fn unfinished_for(&self, path: &Path) -> Option<Record> {
        self.st.borrow().moving.records.iter().find(|r| run::fix_of(r).is_some() && same_path(&r.source(), path)).cloned()
    }

    /// The link's row leads back to its move that did not finish (R-SPACE-MV4-4: after Later,
    /// or a Stop, the link itself is where the user looks).
    pub(super) fn unfinished_offer(&self) -> bool {
        let st = self.st.borrow();
        let busy = st.running.is_some() || st.recycling || st.moving.stage != Stage::Closed || st.moving.busy;
        drop(st);
        !busy && self.cursor_link().is_some_and(|p| self.unfinished_for(&p).is_some())
    }

    /// Show the unfinished move…: that move's dialog.
    pub(super) fn ask_show_move(self: &Rc<Self>) {
        if !self.unfinished_offer() {
            return;
        }
        if let Some(rec) = self.cursor_link().and_then(|p| self.unfinished_for(&p)) {
            self.show_unfinished(rec);
        }
    }

    /// The cursor's path when it is a link.
    fn cursor_link(&self) -> Option<std::path::PathBuf> {
        let v = self.view.borrow();
        let (Some(c), Some(tree)) = (v.cursor, v.tree.as_ref()) else { return None };
        let t = tree.read().unwrap();
        (c != 0 && t.node(c).is_dir() && t.node(c).has(tree::LINK)).then(|| t.path(c))
    }

    /// What the context menu offers for the cursor: (Move, Move back).
    pub(super) fn move_offers(&self) -> (bool, bool) {
        let st = self.st.borrow();
        let busy = st.running.is_some() || st.recycling || st.moving.stage != Stage::Closed || st.moving.busy;
        drop(st);
        let v = self.view.borrow();
        let (Some(c), Some(tree)) = (v.cursor, v.tree.as_ref()) else { return (false, false) };
        let t = tree.read().unwrap();
        let n = t.node(c);
        if c == 0 || !n.is_dir() {
            return (false, false);
        }
        if n.has(tree::LINK) {
            let p = t.path(c);
            drop(t);
            drop(v);
            return (false, !busy && !self.records_for(&p).is_empty());
        }
        (!busy, false)
    }

    /// Move to another drive…: the destination, then the plan (on a thread).
    pub(super) fn ask_move(self: &Rc<Self>) {
        if !self.move_offers().0 {
            return;
        }
        let (source, parent, tree_gen) = {
            let v = self.view.borrow();
            let Some(c) = v.cursor else { return };
            let Some(tree) = v.tree.as_ref() else { return };
            let t = tree.read().unwrap();
            (t.path(c), t.node(c).parent, self.st.borrow().tree_gen)
        };
        let Some(dest) = (self.pick_dest)() else { return };
        self.st.borrow_mut().moving.parent = Some((tree_gen, parent));
        self.start_plan(What::Move(Request { source, dest }), Vec::new());
    }

    /// Move back…: the plan of moving the link's folder back.
    pub(super) fn ask_move_back(self: &Rc<Self>) {
        if !self.move_offers().1 {
            return;
        }
        let (path, parent, tree_gen) = {
            let v = self.view.borrow();
            let Some(c) = v.cursor else { return };
            let Some(tree) = v.tree.as_ref() else { return };
            let t = tree.read().unwrap();
            (t.path(c), t.node(c).parent, self.st.borrow().tree_gen)
        };
        let records = self.records_for(&path);
        let Some(newest) = records.last().cloned() else { return };
        self.st.borrow_mut().moving.parent = Some((tree_gen, parent));
        self.start_plan(What::Back(Box::new(newest)), records);
    }

    /// The plan, on a thread. A move back's record is picked there from `records` (by where
    /// the link leads), and comes back with the plan.
    fn start_plan(self: &Rc<Self>, what: What, records: Vec<Record>) {
        let (seq, stop) = {
            let mut st = self.st.borrow_mut();
            let m = &mut st.moving;
            m.seq += 1;
            m.stage = Stage::Checking;
            m.what = Some(what.clone());
            m.plan = None;
            m.stop = Arc::new(AtomicBool::new(false));
            m.progress = Progress::default();
            m.busy = true;
            (m.seq, m.stop.clone())
        };
        let (fs, env, send) = (self.relocate_fs.clone(), self.relocate_env.clone(), self.send());
        std::thread::spawn(move || {
            let halt = || stop.load(Ordering::Relaxed);
            let (plan, record) = match &what {
                What::Move(r) => (relocate::plan(fs.as_ref(), &env, r, &halt), None),
                What::Back(rec) => {
                    let rec = run::record_for_link(fs.as_ref(), &records, &rec.source()).unwrap_or_else(|| (**rec).clone());
                    (relocate::plan_back(fs.as_ref(), &env, &rec, &halt), Some(Box::new(rec)))
                }
            };
            send(Msg::MovePlanned { seq, plan: Box::new(plan), record });
        });
        self.push_move();
        self.push_actions();
    }

    pub(super) fn move_planned(self: &Rc<Self>, seq: u64, plan: Plan, record: Option<Record>) {
        {
            let mut st = self.st.borrow_mut();
            let m = &mut st.moving;
            // A plan for an older seq (cancelled, or another asked since) does nothing.
            if seq != m.seq {
                return;
            }
            m.busy = false;
            m.stage = Stage::Plan;
            m.plan = Some(plan);
            if let Some(r) = record {
                m.what = Some(What::Back(Box::new(r)));
            }
        }
        self.push_move();
    }

    /// The dialog's main button: Move, Move back, Put it back or Finish.
    pub(super) fn move_confirmed(self: &Rc<Self>) {
        let stage = self.st.borrow().moving.stage;
        match stage {
            Stage::Plan => {
                let other = { self.st.borrow().moving.plan.as_ref().filter(|p| !p.ok()).and_then(|p| p.other_move.clone()) };
                match other {
                    Some(rec) => self.show_unfinished(rec),
                    None => self.start_move(),
                }
            }
            Stage::Unfinished => self.start_recovery(),
            Stage::Stuck => {
                self.st.borrow_mut().moving.stage = Stage::Forget;
                self.push_move();
            }
            Stage::Forget => self.start_forget(),
            _ => {}
        }
    }

    /// A plan refused because of a move that did not finish: that move's dialog, with its one
    /// safe action (Later only hid it until the next start; R-SPACE-MV3-4).
    fn show_unfinished(self: &Rc<Self>, rec: Record) {
        {
            let mut st = self.st.borrow_mut();
            let m = &mut st.moving;
            m.seq += 1;
            m.stage = Stage::Unfinished;
            m.what = None;
            m.plan = None;
            m.parent = None;
            m.stuck = None;
            m.unfinished = Some(rec);
        }
        self.push_move();
        self.push_actions();
    }

    fn start_move(self: &Rc<Self>) {
        let (seq, stop, what) = {
            let mut st = self.st.borrow_mut();
            let m = &mut st.moving;
            if !m.plan.as_ref().is_some_and(Plan::ok) {
                return;
            }
            let Some(what) = m.what.clone() else { return };
            m.stage = Stage::Working;
            m.busy = true;
            m.halt = Arc::new(AtomicBool::new(false));
            m.progress = Progress { total: m.plan.as_ref().map_or(0, |p| p.bytes), ..Progress::default() };
            (m.seq, m.stop.clone(), what)
        };
        let halt = self.st.borrow().moving.halt.clone();
        let (fs, env, send) = (self.relocate_fs.clone(), self.relocate_env.clone(), Arc::new(self.send()));
        std::thread::spawn(move || {
            let beat = {
                let send = send.clone();
                move |p: Progress| send(Msg::MoveProgress { seq, p })
            };
            let (back, result) = match &what {
                What::Move(r) => (false, run::move_folder_with(fs.as_ref(), &env, r, &stop, &halt, &beat).map(|m| (m.record, m.notes))),
                What::Back(rec) => (true, run::move_back_with(fs.as_ref(), &env, rec, &stop, &halt, &beat).map(|n| ((**rec).clone(), n))),
            };
            send(Msg::MoveDone { seq, back, result });
        });
        self.push_move();
        self.push_actions();
    }

    pub(super) fn move_progress(self: &Rc<Self>, seq: u64, p: Progress) {
        {
            let mut st = self.st.borrow_mut();
            if seq != st.moving.seq || !matches!(st.moving.stage, Stage::Working | Stage::Recovering) {
                return;
            }
            st.moving.progress = p;
        }
        self.push_move_progress();
    }

    pub(super) fn move_done(self: &Rc<Self>, seq: u64, back: bool, result: Result<(Record, Vec<String>), Failed>) {
        let parent = {
            let mut st = self.st.borrow_mut();
            if seq != st.moving.seq {
                return;
            }
            let m = &mut st.moving;
            m.busy = false;
            m.stage = Stage::Closed;
            m.what = None;
            m.plan = None;
            m.parent.take()
        };
        self.close_move_dialog();
        match result {
            Ok((rec, notes)) => {
                let short = |p: &Path| format::middle(&relocate::show(p), 48);
                let (src, dst) = (short(&rec.source()), short(&rec.target()));
                let size = format::bytes(rec.bytes);
                let text = if back { format!("Moved {src} back from {dst} ({size})") } else { format!("Moved {size} to {dst}: {src} now leads there") };
                self.toast(text, ToastKind::Ok);
                for n in notes {
                    self.toast(capital(&n), ToastKind::Info);
                }
            }
            Err(f) => self.toast(f.why, ToastKind::Error),
        }
        // The folder's place in the tree is read again (it is a link now, or a folder again).
        if let Some((tree_gen, p)) = parent
            && tree_gen == self.st.borrow().tree_gen
            && p != tree::NONE
        {
            let alive = self.view.borrow().tree.as_ref().is_some_and(|t| {
                let t = t.read().unwrap();
                (p as usize) < t.nodes.len() && !t.node(p).has(tree::GONE)
            });
            if alive {
                self.start_rescan(vec![p]);
            }
        }
        self.read_journals();
        self.push_actions();
    }

    /// Cancel (or Escape, or Later).
    pub(super) fn move_cancelled(self: &Rc<Self>) {
        let stage = self.st.borrow().moving.stage;
        match stage {
            Stage::Checking | Stage::Plan => {
                {
                    let mut st = self.st.borrow_mut();
                    let m = &mut st.moving;
                    m.stop.store(true, Ordering::Relaxed);
                    m.seq += 1; // a plan still on its way does nothing
                    m.busy = false;
                    m.stage = Stage::Closed;
                    m.what = None;
                    m.plan = None;
                    m.parent = None;
                }
                self.close_move_dialog();
                self.push_actions();
            }
            Stage::Working | Stage::Recovering => {
                // Cancel while copying; Stop while the check before the original goes is under
                // way (a whole read of both sides: Stop keeps both), each its own flag. Nothing
                // else can be stopped (R-SPACE-MV4-1, -2).
                let st = self.st.borrow();
                let m = &st.moving;
                if stoppable(&m.progress) {
                    m.halt.store(true, Ordering::Relaxed);
                } else if m.stage == Stage::Working && !m.progress.finishing {
                    m.stop.store(true, Ordering::Relaxed);
                }
                drop(st);
                self.push_move_progress();
            }
            Stage::Unfinished | Stage::Stuck => {
                {
                    let mut st = self.st.borrow_mut();
                    st.moving.later = true;
                    st.moving.stage = Stage::Closed;
                    st.moving.unfinished = None;
                    st.moving.stuck = None;
                }
                self.close_move_dialog();
                self.push_actions();
            }
            // Cancel on the confirm goes back to what it asked about.
            Stage::Forget => {
                self.st.borrow_mut().moving.stage = Stage::Stuck;
                self.push_move();
            }
            Stage::Forgetting | Stage::Closed => {}
        }
    }

    fn close_move_dialog(&self) {
        if let Some(ui) = self.ui() {
            ui.set_move_open(false);
        }
    }

    // ---------- journals, and a move that did not finish ----------

    /// Read the journals on a thread (at start, and after every move).
    pub(super) fn read_journals(self: &Rc<Self>) {
        let Some(j) = self.relocate_env.journal.clone() else { return };
        self.st.borrow_mut().moving.reading += 1;
        let send = self.send();
        std::thread::spawn(move || {
            let (records, bad) = j.read_all();
            // A move another window is working on right now is not "a move that did not finish".
            let busy = records.iter().filter(|r| run::fix_of(r).is_some() && j.busy(&r.id)).map(|r| r.id.clone()).collect();
            send(Msg::Journals { records, bad, busy });
        });
    }

    pub(super) fn journals_read(self: &Rc<Self>, records: Vec<Record>, bad: Vec<Unreadable>, busy: Vec<String>) {
        let offer = {
            let mut st = self.st.borrow_mut();
            let m = &mut st.moving;
            m.reading = m.reading.saturating_sub(1);
            m.records = records;
            let open = m.stage != Stage::Closed || m.busy;
            if open || m.later { None } else { m.records.iter().find(|r| run::fix_of(r).is_some() && !busy.contains(&r.id)).cloned() }
        };
        // Told once a session: an unreadable journal stays as it is (what it says is not known).
        let tell = !std::mem::replace(&mut self.st.borrow_mut().moving.told_bad, true);
        if tell {
            for b in bad {
                self.toast(format!("A move's journal could not be read ({}): {}", b.why, relocate::show(&b.file)), ToastKind::Error);
            }
        }
        if let Some(rec) = offer {
            {
                let mut st = self.st.borrow_mut();
                st.moving.seq += 1;
                st.moving.stage = Stage::Unfinished;
                st.moving.unfinished = Some(rec);
            }
            self.push_move();
        }
        self.push_actions();
    }

    fn start_recovery(self: &Rc<Self>) {
        let (seq, rec, stop) = {
            let mut st = self.st.borrow_mut();
            let m = &mut st.moving;
            let Some(rec) = m.unfinished.clone() else { return };
            m.stage = Stage::Recovering;
            m.busy = true;
            m.halt = Arc::new(AtomicBool::new(false));
            m.progress = Progress::default();
            (m.seq, rec, m.halt.clone())
        };
        let (fs, env, send) = (self.relocate_fs.clone(), self.relocate_env.clone(), Arc::new(self.send()));
        std::thread::spawn(move || {
            let beat = {
                let send = send.clone();
                move |p: Progress| send(Msg::MoveProgress { seq, p })
            };
            let result = run::recover_with(fs.as_ref(), &env, &rec, &stop, &beat);
            // It cannot go on: its journal as it is now, and where each side is (looked at
            // here, off the window's thread). Another window's lock, or a Stop, is no such case.
            let stuck = match &result {
                Err(e) if e != run::BUSY && !e.starts_with(run::STOPPED) => env.journal.as_ref().and_then(|j| j.read(&rec.id).ok().flatten()).map(|now| {
                    let sides = run::sides(fs.as_ref(), &now);
                    Box::new((now, sides))
                }),
                _ => None,
            };
            send(Msg::Recovered { seq, result, stuck });
        });
        self.push_move();
    }

    pub(super) fn recovered(self: &Rc<Self>, seq: u64, result: Result<String, String>, stuck: Option<(Record, Vec<Side>)>) {
        if self.st.borrow().moving.seq != seq {
            return;
        }
        // It cannot go on: the dialog stays, saying why and where each side is, and offers to
        // forget the record (R-SPACE-MV2-6).
        if let (Err(why), Some((now, sides))) = (&result, stuck) {
            {
                let mut st = self.st.borrow_mut();
                let m = &mut st.moving;
                m.busy = false;
                m.stage = Stage::Stuck;
                m.unfinished = Some(now);
                m.stuck = Some((why.clone(), sides));
            }
            self.push_move();
            self.read_journals();
            return;
        }
        {
            let mut st = self.st.borrow_mut();
            let m = &mut st.moving;
            m.busy = false;
            m.stage = Stage::Closed;
            m.unfinished = None;
        }
        self.close_move_dialog();
        match result {
            Ok(s) => self.toast(s, ToastKind::Ok),
            // Stopped: offered again at once (the journal read below), as after a Stop in the
            // move itself (R-SPACE-MV4-4). Another window's lock: it is not offered while held.
            Err(e) if e == run::BUSY || e.starts_with(run::STOPPED) => self.toast(e, ToastKind::Error),
            Err(e) => {
                // Not offered again this session: it would only say the same.
                self.st.borrow_mut().moving.later = true;
                self.toast(format!("{e}. The journal is kept"), ToastKind::Error)
            }
        }
        // Whatever the folder's place in the tree, read the scan again where it shows.
        self.read_journals();
        self.push_actions();
    }

    /// Forget the record of a move that cannot go on: only its journal goes (on a thread).
    fn start_forget(self: &Rc<Self>) {
        let (seq, rec) = {
            let mut st = self.st.borrow_mut();
            let m = &mut st.moving;
            let Some(rec) = m.unfinished.clone() else { return };
            m.stage = Stage::Forgetting;
            m.busy = true;
            (m.seq, rec)
        };
        let (env, send) = (self.relocate_env.clone(), self.send());
        std::thread::spawn(move || send(Msg::Forgot { seq, result: run::forget(&env, &rec) }));
        self.push_move();
    }

    pub(super) fn forgot(self: &Rc<Self>, seq: u64, result: Result<String, String>) {
        {
            let mut st = self.st.borrow_mut();
            if seq != st.moving.seq {
                return;
            }
            let m = &mut st.moving;
            m.busy = false;
            m.stage = Stage::Closed;
            m.unfinished = None;
            m.stuck = None;
            if result.is_err() {
                m.later = true;
            }
        }
        self.close_move_dialog();
        match result {
            Ok(s) => self.toast(s, ToastKind::Ok),
            Err(e) => self.toast(e, ToastKind::Error),
        }
        self.read_journals();
        self.push_actions();
    }

    // ---------- what the dialog shows ----------

    /// A path in the dialog wraps at its separators; past this many characters it is
    /// shortened in the middle (three lines of the value column).
    const ROOM: usize = 150;

    pub(super) fn push_move(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.st.borrow();
        let m = &st.moving;
        let fact = |l: &str, v: String| MoveFact { label: l.into(), value: v.into() };
        let strings = |v: Vec<String>| ModelRc::new(VecModel::from(v.into_iter().map(SharedString::from).collect::<Vec<_>>()));
        let back = matches!(m.what, Some(What::Back(_)));
        let (mut facts, mut warnings, mut refusals) = (Vec::new(), Vec::new(), Vec::new());
        let (mut link, mut note) = (String::new(), String::new());
        let (state, title, go, can_go, cancel) = match m.stage {
            Stage::Closed => {
                drop(st);
                return;
            }
            Stage::Checking => {
                match &m.what {
                    Some(What::Move(r)) => {
                        facts.push(fact("FROM", wrap_path(&r.source, Self::ROOM)));
                        facts.push(fact("INTO", wrap_path(&r.dest, Self::ROOM)));
                    }
                    // Which copy it comes from is read with the plan (where the link leads).
                    Some(What::Back(rec)) => facts.push(fact("TO", wrap_path(&rec.source(), Self::ROOM))),
                    None => {}
                }
                note = "Checking… (reading the folder and asking the drives)".into();
                (0, if back { "Move back" } else { "Move to another drive" }, if back { "Move back" } else { "Move" }, false, "Cancel")
            }
            Stage::Plan | Stage::Working => {
                if let Some(p) = &m.plan {
                    facts.push(fact("FROM", wrap_path(&p.from, Self::ROOM)));
                    facts.push(fact("TO", wrap_path(&p.to, Self::ROOM)));
                    facts.push(fact(
                        "SIZE",
                        format!("{} · {} · {}", format::bytes(p.bytes), format::plural(p.files, "file", "files"), format::plural(p.folders, "folder", "folders")),
                    ));
                    let drive = |r: &str| r.trim_end_matches('\\').to_string();
                    if let Some(after) = p.free_after() {
                        let v = if after < 0 { format!("{} short", format::bytes(after.unsigned_abs() as u64)) } else { format::bytes(after as u64) };
                        facts.push(fact(&format!("{} FREE AFTER", drive(&p.to_root)), v));
                    }
                    if !p.from_root.is_empty() {
                        facts.push(fact(&format!("{} GAINS", drive(&p.from_root)), format!("{} (on disk)", format::bytes(p.alloc))));
                    }
                    link = if back {
                        format!("It becomes a folder of its own again at its old path, and the copy on {} goes.", drive(&p.from_root))
                    } else {
                        "Its old path becomes a link to the new place: programs still find it there.".to_string()
                    };
                    warnings = p.warnings.iter().map(|w| breakable(w)).collect();
                    refusals = p.refusals.iter().map(|r| breakable(r)).collect();
                }
                if m.stage == Stage::Working {
                    (2, if back { "Moving back" } else { "Moving" }, if back { "Move back" } else { "Move" }, false, if m.progress.finishing || m.stop.load(Ordering::Relaxed) { "Finishing…" } else { "Cancel" })
                } else {
                    let ok = m.plan.as_ref().is_some_and(Plan::ok);
                    // Refused because of a move that did not finish: the button leads there.
                    let other = !ok && m.plan.as_ref().is_some_and(|p| p.other_move.is_some());
                    note = if ok && back {
                        "Every file is copied back, then read back and checked; only then does the link go and the copy is removed.".into()
                    } else if ok {
                        "Every file is copied, then read back and checked; only then is the original replaced by the link and removed.".into()
                    } else {
                        "Nothing can be moved:".into()
                    };
                    let go = if other {
                        "Show that move"
                    } else if back {
                        "Move back"
                    } else {
                        "Move"
                    };
                    (1, if back { "Move back" } else { "Move to another drive" }, go, ok || other, "Cancel")
                }
            }
            Stage::Unfinished | Stage::Recovering => {
                let rec = m.unfinished.as_ref();
                let fix = rec.and_then(run::fix_of);
                if let Some(r) = rec {
                    facts.push(fact("FOLDER", wrap_path(&r.source(), Self::ROOM)));
                    facts.push(fact("COPY", wrap_path(&r.target(), Self::ROOM)));
                    facts.push(fact("SIZE", format!("{} · {}", format::bytes(r.bytes), format::plural(r.files, "file", "files"))));
                }
                // Each says only what the action will do (R-SPACE-MV3-5).
                note = match rec.map(|r| r.phase) {
                    Some(Phase::Linking) => "The copy was made. Finish puts the link in its place if it is missing, checks the copy holds the original, and removes the original only if it does; otherwise both are kept.".into(),
                    Some(Phase::Removing) => "It was moved and checked; removing the original did not finish. Finish checks the copy against what is left of the original, then removes it.".into(),
                    Some(Phase::BackSwapped) => "It was copied back and checked; the last steps did not finish. Finish tries them again.".into(),
                    Some(Phase::BackCopying | Phase::BackAside) => "VibeSpace stopped in the middle of moving it back. Put it back leaves it moved: the link stays, and the folder copied back is removed.".into(),
                    _ => "VibeSpace stopped in the middle of it. Put it back puts the original back in its place and removes the unfinished copy; if programs may have written into the copy, both are kept.".into(),
                };
                let go = if fix == Some(Fix::Finish) { "Finish" } else { "Put it back" };
                (3, "A move did not finish", go, m.stage == Stage::Unfinished, "Later")
            }
            Stage::Stuck | Stage::Forget | Stage::Forgetting => {
                if let Some((why, sides)) = &m.stuck {
                    facts.extend(sides.iter().map(|s| fact(&s.label, format!("{} · {}", wrap_path(&s.path, Self::ROOM), s.state))));
                    if m.stage == Stage::Stuck {
                        refusals.push(breakable(why));
                    }
                }
                if m.stage == Stage::Stuck {
                    note = "The places are as listed above. VibeSpace can forget its record of this move: every place stays as it is, and the move is no longer offered.".into();
                    (4, "A move cannot go on", "Forget this record…", true, "Later")
                } else {
                    note = "Only VibeSpace's record of this move goes. Nothing on the disk is touched: every place above stays as it is. Without the record VibeSpace can no longer move it back or finish it: what is listed is yours to keep, undo or remove by hand.".into();
                    (5, "Forget this record?", "Forget", m.stage == Stage::Forget, "Cancel")
                }
            }
        };
        let working = matches!(m.stage, Stage::Working | Stage::Recovering | Stage::Forgetting);
        let can_cancel = match m.stage {
            Stage::Working => (stoppable(&m.progress) && !m.halt.load(Ordering::Relaxed)) || (!m.progress.finishing && !m.stop.load(Ordering::Relaxed)),
            Stage::Recovering => stoppable(&m.progress) && !m.halt.load(Ordering::Relaxed),
            Stage::Forgetting => false,
            _ => true,
        };
        drop(st);
        ui.set_move_state(state);
        ui.set_move_title(title.into());
        ui.set_move_facts(ModelRc::new(VecModel::from(facts)));
        ui.set_move_link(link.into());
        ui.set_move_warnings(strings(warnings));
        ui.set_move_refusals(strings(refusals));
        ui.set_move_note(note.into());
        ui.set_move_go(go.into());
        ui.set_move_can_go(can_go && !working);
        ui.set_move_cancel(cancel.into());
        ui.set_move_can_cancel(can_cancel);
        ui.set_move_open(true);
        self.push_move_progress();
    }

    fn push_move_progress(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.st.borrow();
        let m = &st.moving;
        let p = &m.progress;
        let (stopped, halted) = (m.stop.load(Ordering::Relaxed), m.halt.load(Ordering::Relaxed));
        let checking = matches!(m.stage, Stage::Working | Stage::Recovering) && p.against.is_some();
        let removing = matches!(m.stage, Stage::Working | Stage::Recovering) && p.removing;
        let (value, text) = match m.stage {
            // The check passed: written to the disk, then the original goes. No Stop.
            _ if removing => (-1.0, "Writing the copy to the disk and removing the original…".to_string()),
            // The check before the original goes: a whole read of both sides, told as it goes.
            _ if checking => {
                let (done, files, at, bytes) = p.against.unwrap_or_default();
                // A move back reads the copy again against what its copy back read (RS-1), with
                // the copy renamed aside: the old path leads nowhere until it ends (RS2-5).
                let what = if matches!(m.what, Some(What::Back(_))) { "Reading the copy again (the folder is unavailable meanwhile)" } else { "Checking the copy against the original" };
                (
                    if bytes == 0 { 1.0 } else { (at as f64 / bytes as f64).min(1.0) as f32 },
                    format!("{what}: {done} of {} files, {} of {}", format::count(files), format::bytes(at), format::bytes(bytes)),
                )
            }
            Stage::Working if p.finishing => (-1.0, "Taking the original aside, linking, checking the link, removing the original…".to_string()),
            Stage::Working if m.stop.load(Ordering::Relaxed) => (-1.0, "Cancelling: removing the copy…".to_string()),
            Stage::Working if p.total == 0 => (-1.0, "Copying…".to_string()),
            Stage::Working => (
                ((p.copied + p.checked) as f64 / (2 * p.total) as f64).min(1.0) as f32,
                format!("Copied {}, checked {} of {}", format::bytes(p.copied), format::bytes(p.checked), format::bytes(p.total)),
            ),
            Stage::Recovering => (-1.0, "Working…".to_string()),
            _ => (-2.0, String::new()),
        };
        let file = if checking || (m.stage == Stage::Working && !p.finishing) { breakable(&p.file) } else { String::new() };
        let can_cancel = match m.stage {
            _ if removing => false,
            _ if checking => stoppable(p) && !halted,
            Stage::Working => !p.finishing && !stopped,
            Stage::Recovering | Stage::Forgetting => false,
            _ => true,
        };
        let cancel = match m.stage {
            _ if removing => "Finishing…",
            _ if checking && halted => "Stopping…",
            _ if checking && stoppable(p) => "Stop",
            _ if checking => "Finishing…",
            Stage::Working if p.finishing || stopped => "Finishing…",
            Stage::Unfinished | Stage::Stuck => "Later",
            _ => "Cancel",
        };
        drop(st);
        ui.set_move_progress(value);
        ui.set_move_progress_text(text.into());
        ui.set_move_file(file.into());
        ui.set_move_can_cancel(can_cancel);
        ui.set_move_cancel(cancel.into());
    }

    /// Tests: the journals the window knows.
    pub fn move_records(&self) -> Vec<Record> {
        self.st.borrow().moving.records.clone()
    }
}

/// The check before the original goes is under way and not yet complete: only then is there
/// something for Stop to stop (a late message saying n of n is not; R-SPACE-MV4-1).
fn stoppable(p: &Progress) -> bool {
    !p.removing && p.against.is_some_and(|(done, files, ..)| done < files)
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
