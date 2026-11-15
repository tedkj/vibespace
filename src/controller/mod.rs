//! The window's brain. It lives on the UI thread and owns every decision; the .slint files only
//! lay out and draw. Slow work runs on other threads (the scan, the drives, the shell, the
//! Recycle Bin, saving) and comes back as messages through one channel: the app wakes the
//! event loop for each, the window tests drain it themselves (`settle`).
//!
//! The tree lives behind an `RwLock`: the scan's builder writes in short batches, the window
//! reads between them. A read guard is never held across a call that writes, nor nested. The
//! Recycle Bin's worker holds one while it reads the hard-link counts of what goes (X15R2BC-2):
//! nothing writes to that tree meanwhile (`recycling` holds rescans back; a new scan is a new
//! tree), and the window's reads go on beside it.

mod actions;
mod map;
mod moving;
mod view;

use crate::check::{Dot, Fetched, Local, Release, Rows};
use crate::drives::{Drive, Info};
use crate::scan::{self, Done, Event, Scan, Volume};
use crate::tree::{self, Metric, Tree};
use crate::types::TypeStat;
use crate::{AppWindow, CheckDot, CheckRow, DriveRow, MenuEntry, Prefs, RecentRow, Theme, ToastItem, ToastKind, format, prefs};
use slint::platform::Key;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
pub use moving::wrap_path;
pub use view::{FileModel, TreeModel, View};

/// The shell calls that leave the app (a test puts its own in).
pub trait Shell: Send + Sync {
    fn open(&self, path: &Path) -> Result<(), String>;
    fn reveal(&self, path: &Path) -> Result<(), String>;
    fn recycle(&self, paths: &[PathBuf]) -> Result<(), crate::shell::Refusal>;
}

/// The real one: Explorer, the default programs, the Recycle Bin (its prompts owned by the window).
pub struct RealShell {
    pub owner: Arc<std::sync::atomic::AtomicIsize>,
}

impl Shell for RealShell {
    fn open(&self, path: &Path) -> Result<(), String> {
        crate::shell::open(path)
    }
    fn reveal(&self, path: &Path) -> Result<(), String> {
        crate::shell::reveal(path)
    }
    fn recycle(&self, paths: &[PathBuf]) -> Result<(), crate::shell::Refusal> {
        crate::shell::recycle(paths, self.owner.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// Ask where to save a CSV: its suggested name and the folder to start in; None when cancelled.
pub type SaveCsv = Box<dyn Fn(&str, Option<PathBuf>) -> Option<PathBuf>>;

/// What the controller needs from outside.
pub struct Deps {
    pub prefs: prefs::Store<Prefs>,
    /// Called from any thread when a message is waiting (the app posts a `pump` to the loop).
    pub wake: Arc<dyn Fn() + Send + Sync>,
    pub pick_folder: Box<dyn Fn(Option<PathBuf>) -> Option<PathBuf>>,
    pub save_csv: SaveCsv,
    /// The user's list separator and decimal symbol, asked at each CSV export.
    pub csv_format: Box<dyn Fn() -> crate::csvformat::CsvFormat>,
    pub copy: Box<dyn Fn(&str) -> bool>,
    pub shell: Arc<dyn Shell>,
    pub drives: Arc<dyn Fn() -> Vec<Drive> + Send + Sync>,
    pub drive_info: Arc<dyn Fn(&str) -> Info + Send + Sync>,
    /// Move to another drive: the disk it works through, what it refuses and where its
    /// journals are, and the destination picker.
    pub relocate_fs: Arc<dyn crate::relocate::fs::Fs>,
    pub relocate_env: Arc<crate::relocate::Env>,
    pub pick_dest: Box<dyn Fn() -> Option<PathBuf>>,
    /// A web page in the browser (About's link).
    pub open_url: Box<dyn Fn(&str)>,
    /// The install check's offline half, on its worker: this copy's files, their signatures and
    /// hashes (`trust::inspect_from`; true: installed, so the uninstaller too).
    pub inspect: Fetcher<bool, Local>,
    /// The install check's online half, on its worker: the release list at a path on the site,
    /// waited for at most `release.wait`. Only `main` hands in the real one (the exe's
    /// `net::fetch_release_list`, in no test build); the tests hand in fakes.
    pub fetch: Fetcher<str, Fetched>,
    /// What the check holds the list to: the app, its version, the list's key, the code signer.
    pub release: Release,
}

/// A blocking call the check's worker makes.
pub type Fetcher<A, R> = Arc<dyn Fn(&A) -> R + Send + Sync>;

/// How often the window looks whether a running check has its answer.
const CHECK_POLL_MS: u64 = 50;

/// `fetch` on a thread of its own, waited for at most `deadline` (FETCH_DEADLINE, review RAB-3):
/// past it the answer is `TimedOut`, whatever the site or a proxy is doing, and the thread is
/// left to end by itself (the real fetcher's read loop stops at the same deadline, and WinHTTP's
/// own timeouts bound each step); its late answer goes nowhere.
fn fetch_within(fetch: Fetcher<str, Fetched>, path: String, deadline: Duration) -> Fetched {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch(&path));
    });
    match rx.recv_timeout(deadline) {
        Ok(f) => f,
        Err(RecvTimeoutError::Timeout) => Fetched::TimedOut,
        Err(RecvTimeoutError::Disconnected) => Fetched::Failed("the fetch stopped before it answered".into()),
    }
}

enum Msg {
    Scan { seq: u64, event: Event },
    Drives { seq: u64, list: Vec<Drive> },
    DriveInfo { seq: u64, root: String, info: Info },
    /// `tree`: which scan's nodes these are (a newer scan may have replaced it meanwhile).
    Recycled { tree: u64, nodes: Vec<u32>, result: Result<(), crate::shell::Refusal>, fates: Vec<Fate>, held: Vec<Vec<(u64, u64)>> },
    ShellDone(Result<(), String>),
    /// The CSV written (and what it left out, in words), or why not.
    Saved(Result<(PathBuf, String), String>),
    /// The prefs thread wrote a change.
    PrefsSaved,
    /// A move's plan, for its seq (a move back's: with the record it was worked out from).
    MovePlanned { seq: u64, plan: Box<crate::relocate::Plan>, record: Option<Box<crate::relocate::journal::Record>> },
    MoveProgress { seq: u64, p: crate::relocate::run::Progress },
    MoveDone { seq: u64, back: bool, result: Result<(crate::relocate::journal::Record, Vec<String>), crate::relocate::run::Failed> },
    /// The journals, the unreadable ones, and the ids another window is acting on right now.
    Journals { records: Vec<crate::relocate::journal::Record>, bad: Vec<crate::relocate::journal::Unreadable>, busy: Vec<String> },
    /// A recovery's answer; when it cannot go on, its journal as it is now and where each side is.
    Recovered { seq: u64, result: Result<String, String>, stuck: Option<Box<(crate::relocate::journal::Record, Vec<crate::relocate::run::Side>)>> },
    /// A record forgotten, or why not.
    Forgot { seq: u64, result: Result<String, String> },
}

/// A change to the prefs, kept by the store until it is written and put onto the newest prefs on
/// disk right before each write (an absolute setter: it may be applied more than once).
type PrefsChange = prefs::Change<Prefs>;

/// What became of an item for the Recycle Bin, by reading its exact path right before the
/// shell is asked and again after.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Fate {
    Moved,
    /// Not there when the bin was asked (deleted elsewhere since the scan): nothing was moved.
    AlreadyGone,
    Stayed,
    /// Not what the scan saw any more (another file under its name, another disk under its
    /// letter, written since): not sent, read again.
    Changed,
    /// A folder that leads to a drive (a mounted volume, a link to a drive's root): never sent.
    Drive,
    /// Its path could not be read (before the shell, so it was not sent; or after): what
    /// became of it is not known.
    Unknown(String),
}

/// What a menu row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Open,
    Reveal,
    CopyPath,
    CopyListing,
    SaveCsv,
    Rescan,
    Recycle,
    Move,
    MoveBack,
    ShowMove,
}

/// A scan under way: a whole one, or a rescan of some folders.
struct Running {
    scan: Scan,
    seq: u64,
    rescan: Option<Vec<u32>>,
}

struct State {
    prefs: Prefs,
    running: Option<Running>,
    scan_seq: u64,
    /// The last whole scan: how it ended, when.
    done: Option<Done>,
    finished_at: u32,
    volume: Volume,
    /// A rescan that ended with an error (the folder went).
    drives: Vec<(Drive, Option<Info>)>,
    drives_seq: u64,
    drives_pending: usize,
    tab: usize,
    anchor: Option<u32>,
    map: map::MapState,
    types: Vec<TypeStat>,
    hues: HashMap<u32, usize>,
    type_filter: Option<u32>,
    /// When the lists and the map last followed a scan under way.
    last_rows: Instant,
    last_map: Instant,
    last_stats: Instant,
    stats_stale: bool,
    menu: Vec<Option<Action>>,
    confirm: Vec<u32>,
    recycling: bool,
    shell_pending: usize,
    saving: bool,
    toasts: Vec<(i32, String, ToastKind)>,
    toast_seq: i32,
    /// Per toast: its newest timer's number (older ones do nothing) and its full time (ms).
    toast_time: HashMap<i32, (u64, u64)>,
    folder_seq: i32,
    scroll_seq: i32,
    file_scroll_seq: i32,
    /// The text in the folder field when nothing is scanned (what the user typed last).
    folder_text: String,
    /// The path the last whole scan was given (normalized), before a file became its folder:
    /// what goes back in the field when it cannot be read.
    asked: String,
    /// Bumped by every new tree: work on an older one (a Recycle Bin answer) is let go.
    tree_gen: u64,
    /// A scan cancelled from the window (its seq, and the folders when it was a rescan): it is
    /// over for the window at once; its own end (a worker may sit in a slow network call)
    /// only refreshes what is shown, unless it had read everything after all.
    cancelled: Option<(u64, Option<Vec<u32>>)>,
    /// Move to another drive (its dialog, its work, the journals).
    moving: moving::MoveState,
    /// The install check: its number (an answer for another one does nothing), and while it
    /// runs, where its answer comes from.
    check_seq: u64,
    check_rx: Option<Receiver<(u64, Rows)>>,
}

pub struct App {
    ui: Weak<AppWindow>,
    st: RefCell<State>,
    view: Rc<RefCell<View>>,
    tree_model: Rc<TreeModel>,
    file_model: Rc<FileModel>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    wake: Arc<dyn Fn() + Send + Sync>,
    store: prefs::Store<Prefs>,
    pick: Box<dyn Fn(Option<PathBuf>) -> Option<PathBuf>>,
    save_csv: SaveCsv,
    csv_format: Box<dyn Fn() -> crate::csvformat::CsvFormat>,
    copy: Box<dyn Fn(&str) -> bool>,
    shell: Arc<dyn Shell>,
    drives_fn: Arc<dyn Fn() -> Vec<Drive> + Send + Sync>,
    drive_info: Arc<dyn Fn(&str) -> Info + Send + Sync>,
    relocate_fs: Arc<dyn crate::relocate::fs::Fs>,
    relocate_env: Arc<crate::relocate::Env>,
    pick_dest: Box<dyn Fn() -> Option<PathBuf>>,
    open_url: Box<dyn Fn(&str)>,
    inspect: Fetcher<bool, Local>,
    fetch: Fetcher<str, Fetched>,
    release: Release,
    check_rows: Rc<VecModel<CheckRow>>,
    /// The toasts on screen: one model for the window's life, changed in place by id, so a
    /// toast that stays keeps its element (a screen reader announces it once, X15RFTP).
    toast_rows: Rc<VecModel<ToastItem>>,
    check_timer: Timer,
    ticker: Timer,
    /// The prefs thread: changes go in here; reading prefs.json is the disk too (a profile on a
    /// share that does not answer), never the window's thread. Exit waits for it (`close_prefs`).
    prefs_tx: RefCell<Option<Sender<PrefsChange>>>,
    prefs_thread: RefCell<Option<std::thread::JoinHandle<()>>>,
    prefs_pending: std::cell::Cell<usize>,
}

pub fn key_is(text: &str, k: Key) -> bool {
    let c: char = k.into();
    text.chars().eq(std::iter::once(c))
}

impl App {
    pub fn new(ui: &AppWindow, deps: Deps) -> Rc<App> {
        let (tx, rx) = channel();
        let prefs = deps.prefs.load();
        let tab = (prefs.tab as usize).min(2);
        let view = Rc::new(RefCell::new(View { metric: prefs.metric, tab, ..View::default() }));
        let tree_model = Rc::new(TreeModel::new(view.clone()));
        let file_model = Rc::new(FileModel::new(view.clone()));
        let app = Rc::new(App {
            ui: ui.as_weak(),
            st: RefCell::new(State {
                prefs,
                running: None,
                scan_seq: 0,
                done: None,
                finished_at: 0,
                volume: Volume::default(),
                drives: Vec::new(),
                drives_seq: 0,
                drives_pending: 0,
                tab,
                anchor: None,
                map: map::MapState::default(),
                types: Vec::new(),
                hues: HashMap::new(),
                type_filter: None,
                last_rows: Instant::now(),
                last_map: Instant::now(),
                last_stats: Instant::now(),
                stats_stale: true,
                menu: Vec::new(),
                confirm: Vec::new(),
                recycling: false,
                shell_pending: 0,
                saving: false,
                toasts: Vec::new(),
                toast_seq: 0,
                toast_time: HashMap::new(),
                folder_seq: 0,
                scroll_seq: 0,
                file_scroll_seq: 0,
                folder_text: String::new(),
                asked: String::new(),
                tree_gen: 0,
                cancelled: None,
                moving: moving::MoveState::default(),
                check_seq: 0,
                check_rx: None,
            }),
            view,
            tree_model,
            file_model,
            tx,
            rx,
            wake: deps.wake,
            store: deps.prefs,
            pick: deps.pick_folder,
            save_csv: deps.save_csv,
            csv_format: deps.csv_format,
            copy: deps.copy,
            shell: deps.shell,
            drives_fn: deps.drives,
            drive_info: deps.drive_info,
            relocate_fs: deps.relocate_fs,
            relocate_env: deps.relocate_env,
            pick_dest: deps.pick_dest,
            open_url: deps.open_url,
            inspect: deps.inspect,
            fetch: deps.fetch,
            release: deps.release,
            check_rows: Rc::new(VecModel::default()),
            toast_rows: Rc::new(VecModel::default()),
            check_timer: Timer::default(),
            ticker: Timer::default(),
            prefs_tx: RefCell::new(None),
            prefs_thread: RefCell::new(None),
            prefs_pending: std::cell::Cell::new(0),
        });
        app.start_prefs();
        app.wire(ui);
        ui.set_check_rows(ModelRc::from(app.check_rows.clone()));
        ui.set_toasts(ModelRc::from(app.toast_rows.clone()));
        app.show_rows(&Rows::idle());
        // About's version line. VibeSpace has no portable mode: it is always installed.
        let bits = if cfg!(target_pointer_width = "64") { "64-bit" } else { "32-bit" };
        ui.set_about_line(SharedString::from(format!("version {} · {bits} · installed for you", crate::VERSION)));
        app
    }

    fn ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    /// The first screen: the drives, the folders scanned lately.
    pub fn boot(self: &Rc<Self>) {
        self.load_drives();
        // A move that did not finish is offered as soon as its journal is read.
        self.read_journals();
        self.push_all();
        self.tell_garbled();
    }

    /// A prefs file that could not be parsed was set aside: at start (the window starts afresh),
    /// or since (once this window's prefs are written in its place): said once, in words true
    /// at that moment (R2-SPACE-5).
    fn tell_garbled(self: &Rc<Self>) {
        let kept = prefs::broken_path(self.store.path());
        let name = kept.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if self.store.take_broken() {
            self.toast(format!("The settings could not be read: they start afresh, and the old file is kept as {name}"), ToastKind::Error);
        }
        if self.store.take_garbled() {
            self.toast(format!("The settings file was damaged: it is kept as {name}, and this window's settings are saved in its place"), ToastKind::Error);
        }
    }

    // ---------- wiring ----------

    fn wire(self: &Rc<Self>, ui: &AppWindow) {
        ui.set_rows(ModelRc::from(self.tree_model.clone() as Rc<dyn Model<Data = crate::TreeRow>>));
        ui.set_file_rows(ModelRc::from(self.file_model.clone() as Rc<dyn Model<Data = crate::FileRow>>));
        let w = Rc::downgrade(self);
        macro_rules! on {
            ($setter:ident, |$a:ident $(, $arg:ident : $t:ty)*| $body:expr) => {{
                let w = w.clone();
                ui.$setter(move |$($arg: $t),*| {
                    if let Some($a) = w.upgrade() {
                        $body
                    }
                });
            }};
        }
        macro_rules! on_ret {
            ($setter:ident, $default:expr, |$a:ident $(, $arg:ident : $t:ty)*| $body:expr) => {{
                let w = w.clone();
                ui.$setter(move |$($arg: $t),*| match w.upgrade() {
                    Some($a) => $body,
                    None => $default,
                });
            }};
        }
        on!(on_toggle_theme, |a| a.toggle_theme());
        on_ret!(on_copy_text, false, |a, t: SharedString| a.copy_text(&t));
        on!(on_toast_action, |a, id: i32| a.dismiss_toast(id));
        on!(on_toast_dismissed, |a, id: i32| a.dismiss_toast(id));
        on!(on_dialogs_closed, |a| a.dialogs_closed());
        on!(on_check_install, |a| a.check_install());
        on!(on_open_site, |a| (a.open_url)(crate::check::SITE));
        on!(on_folder_accepted, |a, t: SharedString| a.folder_typed(&t));
        on!(on_folder_edited, |a, t: SharedString| a.st.borrow_mut().folder_text = t.trim().to_string());
        on!(on_browse, |a| a.browse());
        on!(on_scan_or_cancel, |a| a.scan_or_cancel());
        on!(on_show_drives, |a| a.show_drives());
        on!(on_metric_picked, |a, i: i32| a.set_metric(if i == 0 { Metric::Size } else { Metric::OnDisk }));
        on!(on_tab_picked, |a, i: i32| a.set_tab(i as usize));
        on!(on_scan_drive, |a, i: i32| a.scan_drive(i as usize));
        on!(on_scan_recent, |a, i: i32| a.scan_recent(i as usize));
        on!(on_row_pressed, |a, i: i32, c: bool, s: bool| a.row_pressed(i as usize, c, s));
        on!(on_row_toggle, |a, i: i32| a.row_toggle(i as usize));
        on!(on_row_double, |a, i: i32| a.row_double(i as usize));
        on!(on_row_context, |a, i: i32, x: f32, y: f32| a.row_context(i as usize, x, y));
        on_ret!(on_list_key, false, |a, t: SharedString, c: bool, s: bool, p: i32| a.list_key(&t, c, s, p.max(1) as usize));
        on!(on_type_clicked, |a, i: i32| a.type_clicked(i as usize));
        on!(on_file_pressed, |a, i: i32, c: bool, s: bool| a.file_pressed(i as usize, c, s));
        on!(on_file_double, |a, i: i32| a.file_double(i as usize));
        on!(on_file_context, |a, i: i32, x: f32, y: f32| a.file_context(i as usize, x, y));
        on_ret!(on_file_key, false, |a, t: SharedString, c: bool, s: bool, p: i32| a.file_key(&t, c, s, p.max(1) as usize));
        on!(on_act_open, |a| a.act(Action::Open));
        on!(on_act_reveal, |a| a.act(Action::Reveal));
        on!(on_act_copy, |a| a.act(Action::CopyPath));
        on!(on_act_rescan, |a| a.act(Action::Rescan));
        on!(on_act_recycle, |a| a.act(Action::Recycle));
        on!(on_act_more, |a, x: f32, y: f32| a.more_menu(x, y));
        on!(on_menu_picked, |a, i: i32| a.menu_picked(i as usize));
        on!(on_map_resized, |a, w: f32, h: f32| a.map_resized(w, h));
        on!(on_map_hovered, |a, x: f32, y: f32| a.map_hovered(x, y));
        on!(on_map_left, |a| a.map_left());
        on!(on_map_clicked, |a, x: f32, y: f32| a.map_clicked(x, y));
        on!(on_map_double, |a, x: f32, y: f32| a.map_double(x, y));
        on!(on_map_up, |a| a.map_up());
        on!(on_map_clear_filter, |a| a.set_type_filter(None));
        on!(on_recycle_confirmed, |a| a.recycle_confirmed());
        on!(on_recycle_cancelled, |a| a.recycle_cancelled());
        on!(on_move_confirmed, |a| a.move_confirmed());
        on!(on_move_cancelled, |a| a.move_cancelled());
        on_ret!(on_window_key, false, |a, t: SharedString, c: bool, s: bool| a.window_key(&t, c, s));
    }

    // ---------- messages ----------

    /// Handle every message waiting (the app calls this when woken).
    pub fn pump(self: &Rc<Self>) {
        while let Ok(m) = self.rx.try_recv() {
            self.handle(m);
        }
    }

    /// Tests: wait until nothing is under way and everything that came back is on screen.
    pub fn settle(self: &Rc<Self>) {
        let start = Instant::now();
        loop {
            self.pump();
            if !self.working() {
                return;
            }
            match self.rx.recv_timeout(Duration::from_secs(60)) {
                Ok(m) => self.handle(m),
                Err(_) => panic!("still working after {:?}", start.elapsed()),
            }
        }
    }

    pub fn working(&self) -> bool {
        let st = self.st.borrow();
        st.running.is_some() || st.drives_pending > 0 || st.recycling || st.shell_pending > 0 || st.saving || self.prefs_pending.get() > 0 || st.moving.busy || st.moving.reading > 0
    }

    fn handle(self: &Rc<Self>, m: Msg) {
        match m {
            Msg::Scan { seq, event } => self.scan_event(seq, event),
            Msg::Drives { seq, list } => self.drives_listed(seq, list),
            Msg::DriveInfo { seq, root, info } => self.drive_answered(seq, root, info),
            Msg::Recycled { tree, nodes, result, fates, held } => self.recycled(tree, nodes, result, fates, held),
            Msg::ShellDone(r) => {
                self.st.borrow_mut().shell_pending -= 1;
                if let Err(e) = r {
                    self.toast(e, ToastKind::Error);
                }
            }
            Msg::MovePlanned { seq, plan, record } => self.move_planned(seq, *plan, record.map(|r| *r)),
            Msg::MoveProgress { seq, p } => self.move_progress(seq, p),
            Msg::MoveDone { seq, back, result } => self.move_done(seq, back, result),
            Msg::Journals { records, bad, busy } => self.journals_read(records, bad, busy),
            Msg::Recovered { seq, result, stuck } => self.recovered(seq, result, stuck.map(|s| *s)),
            Msg::Forgot { seq, result } => self.forgot(seq, result),
            Msg::PrefsSaved => {
                self.prefs_pending.set(self.prefs_pending.get().saturating_sub(1));
                // The file may have turned garbled while the window was open (read just now).
                self.tell_garbled();
            }
            Msg::Saved(r) => {
                self.st.borrow_mut().saving = false;
                match r {
                    Ok((p, left)) => {
                        let p = tree::clean(&p.display().to_string());
                        self.toast(if left.is_empty() { format!("Saved {p}") } else { format!("Saved {p}: {left}") }, ToastKind::Ok)
                    }
                    Err(e) => self.toast(e, ToastKind::Error),
                }
            }
        }
    }

    fn send(&self) -> impl Fn(Msg) + Send + Sync + 'static {
        let tx = std::sync::Mutex::new(self.tx.clone());
        let wake = self.wake.clone();
        move |m| {
            let _ = tx.lock().unwrap().send(m);
            wake();
        }
    }

    // ---------- the drives ----------

    fn load_drives(self: &Rc<Self>) {
        let seq = {
            let mut st = self.st.borrow_mut();
            st.drives_seq += 1;
            st.drives_pending += 1;
            st.drives_seq
        };
        let send = self.send();
        let list = self.drives_fn.clone();
        std::thread::spawn(move || send(Msg::Drives { seq, list: list() }));
        self.push_drives();
    }

    fn drives_listed(self: &Rc<Self>, seq: u64, list: Vec<Drive>) {
        {
            let mut st = self.st.borrow_mut();
            st.drives_pending -= 1;
            if seq != st.drives_seq {
                return;
            }
            st.drives = list.iter().cloned().map(|d| (d, None)).collect();
            st.drives_pending += list.len();
        }
        self.push_status();
        // Each drive on its own thread: one that does not answer holds up no other.
        let send = Arc::new(self.send());
        for d in list {
            let (send, info) = (send.clone(), self.drive_info.clone());
            std::thread::spawn(move || {
                let i = info(&d.root);
                send(Msg::DriveInfo { seq, root: d.root, info: i });
            });
        }
        self.push_drives();
    }

    fn drive_answered(self: &Rc<Self>, seq: u64, root: String, info: Info) {
        {
            let mut st = self.st.borrow_mut();
            st.drives_pending -= 1;
            if seq != st.drives_seq {
                return;
            }
            if let Some(d) = st.drives.iter_mut().find(|d| d.0.root == root) {
                d.1 = Some(info);
            }
        }
        self.push_drives();
        self.push_status();
    }

    fn push_drives(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.st.borrow();
        if self.view.borrow().tree.is_some() {
            // Hidden behind the scan: no cards (nor their accessibility nodes).
            ui.set_drives(ModelRc::new(VecModel::<DriveRow>::default()));
            ui.set_recent(ModelRc::new(VecModel::<RecentRow>::default()));
            return;
        }
        let rows: Vec<DriveRow> = st
            .drives
            .iter()
            .map(|(d, i)| {
                let letter = d.root.trim_end_matches('\\');
                let kind = match d.kind {
                    crate::drives::Kind::Fixed => "local disk",
                    crate::drives::Kind::Removable => "removable",
                    crate::drives::Kind::Network => "network",
                    crate::drives::Kind::Optical => "optical",
                    crate::drives::Kind::Ram => "RAM disk",
                    crate::drives::Kind::Unknown => "drive",
                };
                match i {
                    None => DriveRow { root: d.root.as_str().into(), title: letter.into(), detail: kind.into(), used: 0.0, free: "Looking…".into(), state: 0, network: d.kind == crate::drives::Kind::Network },
                    Some(i) if !i.error.is_empty() => DriveRow {
                        root: d.root.as_str().into(),
                        title: letter.into(),
                        detail: kind.into(),
                        used: 0.0,
                        free: i.error.as_str().into(),
                        state: 2,
                        network: d.kind == crate::drives::Kind::Network,
                    },
                    Some(i) => {
                        let title = if i.label.is_empty() { letter.to_string() } else { format!("{letter}  {}", i.label) };
                        let used = if i.total > 0 { (i.total - i.free.min(i.total)) as f32 / i.total as f32 } else { 0.0 };
                        DriveRow {
                            root: d.root.as_str().into(),
                            title: title.into(),
                            detail: format!("{} · {kind}", if i.fs.is_empty() { "?" } else { &i.fs }).into(),
                            used,
                            free: format!("{} free of {}", format::bytes(i.free), format::bytes(i.total)).into(),
                            state: 1,
                            network: d.kind == crate::drives::Kind::Network,
                        }
                    }
                }
            })
            .collect();
        ui.set_drives(ModelRc::new(VecModel::from(rows)));
        ui.set_drives_looking(st.drives_pending > 0);
        let recent: Vec<RecentRow> = st.prefs.recent.iter().map(|p| RecentRow { path: tree::clean(p).into() }).collect();
        ui.set_recent(ModelRc::new(VecModel::from(recent)));
    }

    fn scan_drive(self: &Rc<Self>, i: usize) {
        let root = self.st.borrow().drives.get(i).map(|d| d.0.root.clone());
        if let Some(r) = root {
            self.start_scan(Path::new(&r));
        }
    }

    fn scan_recent(self: &Rc<Self>, i: usize) {
        let p = self.st.borrow().prefs.recent.get(i).cloned();
        if let Some(p) = p {
            self.start_scan(Path::new(&p));
        }
    }

    /// Back to the first screen: a scan under way stops, the scan is let go (its memory too).
    fn show_drives(self: &Rc<Self>) {
        self.stop_scan();
        self.view.borrow_mut().clear();
        {
            let mut st = self.st.borrow_mut();
            st.tree_gen += 1;
            st.cancelled = None; // a late answer from that scan is for no tree on screen
            st.done = None;
            st.map = map::MapState::default();
            st.types.clear();
            st.hues.clear();
            st.type_filter = None;
            st.folder_seq += 1;
        }
        self.tree_model.reset();
        self.file_model.reset();
        self.load_drives();
        self.push_all();
    }

    // ---------- scanning ----------

    /// A path from outside (the command line, a drop). A file stands for its folder: the scan
    /// looks, off the UI thread, as it does for a typed path.
    pub fn open_path(self: &Rc<Self>, p: PathBuf) {
        if let Some(p) = typed_path(p.as_os_str()) {
            self.start_scan(&p);
        }
    }

    /// Items dragged into the window from Explorer: VibeSpace scans one at a time, so the first
    /// is scanned as `open_path` scans one, and with more a toast says the others were left out
    /// (the toast items dropped on the exe get, `launch::left_out`).
    pub fn open_dropped(self: &Rc<Self>, items: Vec<PathBuf>) {
        let mut items = items.into_iter().map(PathBuf::into_os_string);
        let Some(first) = items.next() else { return };
        let rest: Vec<std::ffi::OsString> = items.collect();
        self.open_path(PathBuf::from(first));
        if !rest.is_empty() {
            self.toast(crate::launch::left_out(&rest), ToastKind::Info);
        }
    }

    /// What was typed (Enter or the Scan button): the same normaliser as a drop and the
    /// command line.
    fn folder_typed(self: &Rc<Self>, t: &str) {
        let Some(p) = typed_path(std::ffi::OsStr::new(t)) else { return };
        self.st.borrow_mut().folder_text = p.display().to_string();
        self.start_scan(&p);
    }

    fn browse(self: &Rc<Self>) {
        let root = self.view.borrow().tree.as_ref().map(|t| t.read().unwrap().root_path.clone());
        let vol = {
            let st = self.st.borrow();
            match &st.running {
                Some(r) if r.rescan.is_none() => r.scan.volume.lock().unwrap().clone(),
                _ => st.volume.clone(),
            }
        };
        let start = root.and_then(|r| browse_start(&r, &vol));
        if let Some(p) = (self.pick)(start) {
            self.start_scan(&p);
        }
    }

    fn scan_or_cancel(self: &Rc<Self>) {
        if self.st.borrow().running.is_some() {
            self.cancel_scan();
            return;
        }
        let text = self.ui().map(|u| u.get_folder().to_string()).unwrap_or_default();
        let typed = self.st.borrow().folder_text.clone();
        let target = if !typed.is_empty() { typed } else { text };
        // The same as Enter: one normaliser for what was typed.
        if typed_path(std::ffi::OsStr::new(&target)).is_none() {
            self.browse();
        } else {
            self.folder_typed(&target);
        }
    }

    fn notify(&self, seq: u64) -> Arc<dyn Fn(Event) + Send + Sync> {
        let send = self.send();
        Arc::new(move |event| send(Msg::Scan { seq, event }))
    }

    /// A whole new scan of `path`.
    pub fn start_scan(self: &Rc<Self>, path: &Path) {
        self.stop_scan();
        let seq = {
            let mut st = self.st.borrow_mut();
            st.scan_seq += 1;
            st.scan_seq
        };
        let (tree, scan) = scan::scan(path, self.notify(seq));
        {
            let mut st = self.st.borrow_mut();
            st.tree_gen += 1;
            st.cancelled = None; // an older scan's late answer is not for this tree
        }
        {
            let mut v = self.view.borrow_mut();
            v.clear();
            v.tree = Some(tree);
            v.expanded.insert(0);
            v.cursor = Some(0);
            v.sel.insert(0);
        }
        {
            let mut st = self.st.borrow_mut();
            st.running = Some(Running { scan, seq, rescan: None });
            st.done = None;
            st.volume = Volume::default();
            st.anchor = Some(0);
            st.map = map::MapState { px: st.map.px, logical: st.map.logical, scale: st.map.scale, ..map::MapState::default() };
            st.types.clear();
            st.hues.clear();
            st.type_filter = None;
            st.stats_stale = true;
            st.folder_text.clear();
            st.asked = scan::normalize(path).display().to_string();
            st.folder_seq += 1;
            st.last_rows = Instant::now() - Duration::from_secs(1);
            st.last_map = Instant::now() - Duration::from_secs(1);
        }
        self.start_ticker();
        self.fit_root_label();
        self.refresh_rows();
        self.file_model.reset();
        self.repaint_map();
        self.push_all();
        if let Some(ui) = self.ui() {
            ui.invoke_focus_list();
        }
    }

    /// Rescan folders of the current scan in place.
    fn start_rescan(self: &Rc<Self>, dirs: Vec<u32>) {
        let Some(tree) = self.view.borrow().tree.clone() else { return };
        if dirs.is_empty() || self.st.borrow().running.is_some() {
            return;
        }
        let seq = {
            let mut st = self.st.borrow_mut();
            st.scan_seq += 1;
            st.scan_seq
        };
        let scan = scan::rescan(tree, dirs.clone(), self.notify(seq));
        {
            let mut st = self.st.borrow_mut();
            st.running = Some(Running { scan, seq, rescan: Some(dirs) });
            st.stats_stale = true;
        }
        self.start_ticker();
        self.refresh_rows();
        self.push_all();
    }

    /// Cancel: stopped at once for the window, whatever the workers are still waiting on.
    fn cancel_scan(self: &Rc<Self>) {
        let r = self.st.borrow_mut().running.take();
        let Some(r) = r else { return };
        r.scan.cancel();
        self.ticker.stop();
        {
            let mut st = self.st.borrow_mut();
            st.cancelled = Some((r.seq, r.rescan.clone()));
            if r.rescan.is_none() {
                let volume = r.scan.volume.lock().unwrap().clone();
                st.volume = volume.clone();
                st.finished_at = format::now_unix();
                st.done = Some(Done { elapsed: r.scan.started.elapsed(), cancelled: true, volume, ..Done::default() });
            }
            st.stats_stale = true;
        }
        self.refresh_rows();
        self.refresh_stats();
        self.repaint_map();
        self.push_all();
    }

    /// Stop a scan under way for good (another one starts, or the drives view): its late
    /// messages are ignored.
    fn stop_scan(&self) {
        let mut st = self.st.borrow_mut();
        if let Some(r) = st.running.take() {
            r.scan.cancel();
        }
        self.ticker.stop();
    }

    fn start_ticker(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.ticker.start(TimerMode::Repeated, Duration::from_millis(250), move || {
            if let Some(a) = w.upgrade() {
                a.push_facts();
                a.push_status();
            }
        });
    }

    fn scan_event(self: &Rc<Self>, seq: u64, event: Event) {
        let current = self.st.borrow().running.as_ref().map(|r| r.seq) == Some(seq);
        if !current {
            // The last word of a scan the window already stopped: what it read is shown. A
            // Cancel that came too late (everything was read) ends it as if it had not come.
            let cancelled = self.st.borrow().cancelled.clone();
            if let Event::Done(done) = event
                && let Some((s, rescan)) = cancelled
                && s == seq
            {
                self.st.borrow_mut().cancelled = None;
                if !done.cancelled {
                    self.finish(done, rescan);
                    return;
                }
                self.st.borrow_mut().stats_stale = true;
                self.refresh_rows();
                self.refresh_stats();
                self.repaint_map();
                self.push_all();
            }
            return;
        }
        match event {
            Event::Progress => {
                let (rows, mapp, stats) = {
                    let mut st = self.st.borrow_mut();
                    let now = Instant::now();
                    let rows = now - st.last_rows >= Duration::from_millis(200);
                    let mapp = now - st.last_map >= Duration::from_millis(600);
                    let stats = st.tab != 0 && now - st.last_stats >= Duration::from_millis(1000);
                    if rows {
                        st.last_rows = now;
                    }
                    if mapp {
                        st.last_map = now;
                    }
                    if stats {
                        st.last_stats = now;
                    }
                    st.stats_stale = true;
                    (rows, mapp, stats)
                };
                if rows {
                    self.refresh_rows();
                }
                if stats {
                    self.refresh_stats();
                }
                if mapp {
                    self.repaint_map();
                }
                self.push_facts();
                self.push_status();
            }
            Event::Done(done) => self.scan_done(done),
        }
    }

    fn scan_done(self: &Rc<Self>, done: Done) {
        let rescan = {
            let mut st = self.st.borrow_mut();
            let r = st.running.take();
            r.and_then(|r| r.rescan)
        };
        self.ticker.stop();
        self.finish(done, rescan);
    }

    /// A scan's end (`rescan`: the folders a rescan read again).
    fn finish(self: &Rc<Self>, done: Done, rescan: Option<Vec<u32>>) {
        match rescan {
            Some(dirs) => {
                if let Some(e) = &done.root_error {
                    self.toast(e.clone(), ToastKind::Error);
                }
                // A rescanned folder that went (not found, its own folder answering) is taken
                // out (its parent shrinks); one that could not be read for another reason (a
                // share's hiccup) stays, marked: unknown is not gone.
                if !done.gone.is_empty() {
                    let tree = self.view.borrow().tree.clone();
                    if let Some(tree) = tree {
                        let mut t = tree.write().unwrap();
                        let went: Vec<u32> = dirs.into_iter().filter(|&d| done.gone.contains(&d) && !t.has_children(d) && d != 0).collect();
                        t.remove(&went);
                    }
                    self.fix_selection();
                }
            }
            None => {
                let failed = done.root_error.clone();
                {
                    let mut st = self.st.borrow_mut();
                    st.volume = done.volume.clone();
                    st.finished_at = format::now_unix();
                    st.done = Some(done.clone());
                }
                if let Some(e) = failed {
                    // Nothing was read: back to where the user came from, saying why.
                    self.toast(e, ToastKind::Error);
                    let typed = self.st.borrow().asked.clone();
                    self.show_drives();
                    self.st.borrow_mut().folder_text = typed.clone();
                    if let Some(ui) = self.ui() {
                        ui.set_folder(typed.into());
                    }
                    self.bump_folder();
                    return;
                }
                if !done.cancelled {
                    // Only a root a string can spell exactly: a lossy one would never scan again
                    // (a lone surrogate in a name; VibeRename RR-7). Such a root is not offered.
                    let path = self.view.borrow().tree.as_ref().and_then(|t| t.read().unwrap().root_path.to_str().map(str::to_string));
                    if let Some(p) = path {
                        self.remember(move |pr| {
                            pr.recent.retain(|r| !r.eq_ignore_ascii_case(&p));
                            pr.recent.insert(0, p.clone());
                            pr.recent.truncate(8);
                        });
                    }
                }
            }
        }
        self.st.borrow_mut().stats_stale = true;
        self.fit_root_label(); // a file given to scan became its folder
        self.refresh_rows();
        self.refresh_stats();
        self.repaint_map();
        self.push_all();
    }

    fn bump_folder(&self) {
        let seq = {
            let mut st = self.st.borrow_mut();
            st.folder_seq += 1;
            st.folder_seq
        };
        if let Some(ui) = self.ui() {
            ui.set_folder_seq(seq);
        }
    }

    // ---------- settings ----------

    fn set_metric(self: &Rc<Self>, m: Metric) {
        if self.view.borrow().metric == m {
            return;
        }
        self.view.borrow_mut().metric = m;
        self.remember(move |p| p.metric = m);
        self.st.borrow_mut().stats_stale = true;
        self.refresh_rows();
        self.refresh_stats();
        self.repaint_map();
        self.push_all();
    }

    fn set_tab(self: &Rc<Self>, i: usize) {
        let i = i.min(2);
        self.st.borrow_mut().tab = i;
        self.view.borrow_mut().tab = i;
        self.tree_model.reset();
        self.file_model.reset();
        self.remember(move |p| p.tab = i as u8);
        self.refresh_stats();
        self.push_all();
        if let Some(ui) = self.ui() {
            ui.set_tab(i as i32);
            ui.invoke_focus_list();
        }
        // Back in the tree, it opens to what was picked elsewhere (Largest files, the map).
        let cursor = self.view.borrow().cursor;
        if i == 0
            && let Some(c) = cursor
        {
            let sel = self.view.borrow().sel.clone();
            self.select(c, true);
            self.view.borrow_mut().sel = sel;
            self.tree_model.reset();
        }
    }

    fn toggle_theme(self: &Rc<Self>) {
        let Some(ui) = self.ui() else { return };
        let theme = ui.global::<Theme>();
        let dark = !theme.get_dark();
        theme.set_dark(dark);
        self.remember(move |p| p.theme = Some(if dark { "dark" } else { "light" }.into()));
        self.theme_changed();
    }

    /// The colours Rust draws (the map, the swatches) follow the theme now set.
    pub fn theme_changed(&self) {
        self.repaint_map();
        self.push_types();
    }

    /// Change a pref: in the window's own copy at once, and by the prefs thread onto the
    /// newest prefs on disk (another window's changes stay). `f` sets, never toggles.
    fn remember(self: &Rc<Self>, f: impl Fn(&mut Prefs) + Send + Sync + 'static) {
        f(&mut self.st.borrow_mut().prefs);
        let sent = self.prefs_tx.borrow().as_ref().is_some_and(|tx| tx.send(Arc::new(f)).is_ok());
        if sent {
            self.prefs_pending.set(self.prefs_pending.get() + 1);
        }
    }

    /// The prefs thread: the store keeps each change until it is written, and puts every
    /// unwritten one onto the file as read right before the write (another window's changes
    /// included), onto this window's own when the file is garbled or missing, and writes
    /// nothing while the file cannot be read.
    fn start_prefs(self: &Rc<Self>) {
        let (tx, rx) = channel::<PrefsChange>();
        let (store, send) = (self.store.clone(), self.send());
        let h = std::thread::spawn(move || {
            while let Ok(f) = rx.recv() {
                store.change(f);
                send(Msg::PrefsSaved);
            }
        });
        *self.prefs_tx.borrow_mut() = Some(tx);
        *self.prefs_thread.borrow_mut() = Some(h);
    }

    /// At exit (and when a test's window goes): the changes still queued go to the store first,
    /// which writes them onto the file as it is now. A file that still cannot be read gets
    /// nothing (the change is lost, the other instance's prefs are not).
    pub fn close_prefs(&self) {
        self.prefs_tx.borrow_mut().take();
        if let Some(h) = self.prefs_thread.borrow_mut().take() {
            let _ = h.join();
        }
    }

    // ---------- keys ----------

    fn window_key(self: &Rc<Self>, t: &str, ctrl: bool, _shift: bool) -> bool {
        if key_is(t, Key::Escape) {
            if self.st.borrow().running.is_some() {
                self.cancel_scan();
                return true;
            }
            if self.st.borrow().type_filter.is_some() {
                self.set_type_filter(None);
                return true;
            }
            return false;
        }
        if self.view.borrow().tree.is_none() {
            return false;
        }
        if key_is(t, Key::Backspace) {
            self.map_up();
            return true;
        }
        if key_is(t, Key::F5) {
            self.act(Action::Rescan);
            return true;
        }
        if key_is(t, Key::Delete) {
            self.act(Action::Recycle);
            return true;
        }
        if ctrl && (t == "c" || t == "C" || t == "\u{3}") {
            self.act(Action::CopyPath);
            return true;
        }
        false
    }

    // ---------- what the window shows ----------

    /// The window's width in logical pixels.
    fn window_width(&self) -> f32 {
        self.ui().map(|u| u.window().size().width as f32 / u.window().scale_factor()).unwrap_or(1180.0)
    }

    fn push_all(&self) {
        let Some(ui) = self.ui() else { return };
        {
            let st = self.st.borrow();
            let v = self.view.borrow();
            ui.set_has_scan(v.tree.is_some());
            ui.set_scanning(st.running.is_some());
            ui.set_metric(if v.metric == Metric::Size { 0 } else { 1 });
            ui.set_tab(st.tab as i32);
            let root = v.tree.as_ref().map(|t| t.read().unwrap().root_path.display().to_string());
            let shown = match root {
                Some(r) => tree::clean(&r),
                None => st.folder_text.clone(),
            };
            ui.set_window_title(if v.tree.is_some() { format!("{} · VibeSpace", format::middle(&shown, 60)).into() } else { "VibeSpace".into() });
            if ui.get_folder() != shown.as_str() {
                ui.set_folder(shown.into());
                ui.set_folder_seq(st.folder_seq);
            }
        }
        self.push_facts();
        self.push_status();
        self.push_actions();
        self.push_drives();
        self.push_types();
        self.push_toasts();
    }

    fn push_facts(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.st.borrow();
        let v = self.view.borrow();
        let Some(tree) = v.tree.as_ref() else { return };
        let t = tree.read().unwrap();
        let root = t.node(0);
        let (files, folders) = t.counts(0);
        let vol = match &st.running {
            Some(r) if st.volume.root.is_empty() => r.scan.volume.lock().unwrap().clone(),
            _ => st.volume.clone(),
        };
        let elapsed = match (&st.running, &st.done) {
            (Some(r), _) if r.rescan.is_none() => Some(r.scan.started.elapsed()),
            (_, Some(d)) => Some(d.elapsed),
            _ => None,
        };
        let facts = [
            ("TOTAL", format::bytes(root.size)),
            ("ON DISK", format::bytes(root.alloc)),
            ("FILES", format::count(files)),
            ("FOLDERS", format::count(folders)),
            ("FREE", vol.free.map(format::bytes).unwrap_or_else(|| "—".into())),
            ("TIME", elapsed.map(|e| format::took(e.as_millis())).unwrap_or_default()),
        ];
        // The path gets what the other facts leave (mono 14 px values, 12 px labels).
        let others: f32 = facts.iter().map(|(l, v)| (l.len() as f32 * 7.3).max(v.chars().count() as f32 * 8.5) + 20.0 + 8.0).sum();
        let room = self.window_width() - 28.0 - others - 20.0;
        ui.set_fact_path(format::middle(&tree::clean(&t.root_path.display().to_string()), (room / 8.5).max(0.0) as usize).into());
        ui.set_fact_total(facts[0].1.as_str().into());
        ui.set_fact_disk(facts[1].1.as_str().into());
        ui.set_fact_files(facts[2].1.as_str().into());
        ui.set_fact_folders(facts[3].1.as_str().into());
        ui.set_fact_free(facts[4].1.as_str().into());
        ui.set_fact_time(facts[5].1.as_str().into());
        // Partial is what the tree holds, not how it got there: a cancelled rescan leaves
        // folders not read too (and a later rescan of them that completes heals it).
        ui.set_fact_partial(st.done.as_ref().is_some_and(|d| d.cancelled) || (st.running.is_none() && t.skipped().pending > 0));
        // Progress against the drive's used space, when the scan is of the whole drive.
        let p = match &st.running {
            Some(r) if r.rescan.is_none() => {
                let used = vol.total.zip(vol.free).map(|(a, b)| a.saturating_sub(b)).unwrap_or(0);
                let whole = !vol.root.is_empty() && vol.root.trim_end_matches('\\').eq_ignore_ascii_case(t.root_path.to_string_lossy().trim_end_matches('\\'));
                if whole && used > 0 { (root.alloc as f64 / used as f64).min(1.0) as f32 } else { -1.0 }
            }
            Some(_) => -1.0,
            None => -2.0,
        };
        ui.set_progress(p);
    }

    fn push_status(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.st.borrow();
        let v = self.view.borrow();
        let mut warn = false;
        // A path in the status line: about a third of the line at most (the rest is the news).
        let path_room = ((self.window_width() - 400.0) / 6.6).max(20.0) as usize;
        let short = |p: &str| format::middle(p, path_room);
        let left = match v.tree.as_ref() {
            None => {
                if st.drives_pending > 0 {
                    "Looking at the drives…".to_string()
                } else {
                    format!("{} · pick one to scan, or Browse for a folder", format::plural(st.drives.len() as u64, "drive", "drives"))
                }
            }
            Some(tree) => {
                let t = tree.read().unwrap();
                if st.recycling {
                    "Moving to the Recycle Bin…".to_string()
                } else if let Some(r) = &st.running {
                    let what = r.scan.current();
                    match &r.rescan {
                        Some(_) => format!("Rescanning… {}", short(&what)),
                        None => format!("Reading {}", short(&if what.is_empty() { tree::clean(&t.root_path.display().to_string()) } else { what })),
                    }
                } else if let Some(d) = &st.done {
                    let s = t.skipped();
                    let mut parts = Vec::new();
                    if d.cancelled {
                        warn = true;
                        parts.push(format!("Cancelled after {}: the totals are partial ({} not read)", format::took(d.elapsed.as_millis()), format::plural(s.pending, "folder", "folders")));
                    } else {
                        parts.push(format!("Scanned {} at {} in {}", short(&tree::clean(&t.root_path.display().to_string())), format::clock(st.finished_at), format::took(d.elapsed.as_millis())));
                        if s.pending > 0 {
                            // A rescan was cancelled since.
                            warn = true;
                            parts.push(format!("the totals are partial ({} not read)", format::plural(s.pending, "folder", "folders")));
                        }
                    }
                    if s.no_access > 0 {
                        parts.push(format!("{} without access", format::plural(s.no_access, "folder", "folders")));
                    }
                    if s.unreadable > 0 {
                        parts.push(format!("{} could not be read", format::plural(s.unreadable, "folder", "folders")));
                    }
                    if s.online > 0 {
                        parts.push(format!("{} online only, not listed", format::plural(s.online, "folder", "folders")));
                    }
                    if st.volume.remote {
                        parts.push("a network drive: hard links count each time".into());
                    }
                    parts.join(" · ")
                } else {
                    String::new()
                }
            }
        };
        ui.set_status_left(left.into());
        ui.set_status_warn(warn);
        // The selection.
        let right = match v.tree.as_ref() {
            Some(tree) => {
                let t = tree.read().unwrap();
                let tops = view::tops(&t, &v.sel);
                if tops.is_empty() {
                    String::new()
                } else {
                    let total: u64 = tops.iter().map(|&n| t.node(n).value(v.metric)).sum();
                    format!("{} selected · {}", format::count(tops.len() as u64), format::bytes(total))
                }
            }
            None => String::new(),
        };
        ui.set_status_right(right.into());
    }

    // ---------- About's install check ----------

    /// "Check this install": one at a time, on a worker (it reads the files and may go online);
    /// the window looks for the answer every CHECK_POLL_MS meanwhile and goes on. Closing About
    /// leaves it running: the answer lands in the rows, which About shows when it opens again.
    fn check_install(self: &Rc<Self>) {
        let Some(ui) = self.ui() else { return };
        let seq = {
            let mut st = self.st.borrow_mut();
            if st.check_rx.is_some() {
                return;
            }
            st.check_seq += 1;
            st.check_seq
        };
        let (tx, rx) = channel();
        self.st.borrow_mut().check_rx = Some(rx);
        ui.set_checking(true);
        let (inspect, fetch, release) = (self.inspect.clone(), self.fetch.clone(), self.release.clone());
        // VibeSpace has no portable mode: it is always installed, so the uninstaller is checked too.
        let installed = true;
        std::thread::spawn(move || {
            let local = inspect(&installed);
            // The exe updated or replaced under this one while it ran: nothing to compare (FED3).
            let rows = if local.replaced {
                crate::check::replaced_rows(&release, &local)
            } else {
                let fetched = fetch_within(fetch, crate::check::list_path(release.app), release.wait);
                crate::check::rows(&release, &local, fetched)
            };
            let _ = tx.send((seq, rows));
        });
        let w = Rc::downgrade(self);
        self.check_timer.start(TimerMode::Repeated, Duration::from_millis(CHECK_POLL_MS), move || {
            if let Some(a) = w.upgrade() {
                a.poll_check();
            }
        });
    }

    /// The check's answer, once it is there: the rows, and the button back.
    fn poll_check(&self) {
        let got = {
            let st = self.st.borrow();
            let Some(rx) = st.check_rx.as_ref() else { return };
            match rx.try_recv() {
                Ok((seq, rows)) => Some((seq == st.check_seq).then_some(rows)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Some(Rows::stopped())),
            }
        };
        let Some(rows) = got else { return };
        self.check_timer.stop();
        self.st.borrow_mut().check_rx = None;
        if let Some(rows) = rows {
            self.show_rows(&rows);
        }
        if let Some(ui) = self.ui() {
            ui.set_checking(false);
            ui.set_checked(true);
        }
    }

    fn show_rows(&self, rows: &Rows) {
        let dot = |d: Dot| match d {
            Dot::Idle => CheckDot::Idle,
            Dot::Ok => CheckDot::Ok,
            Dot::Warn => CheckDot::Warn,
            Dot::Danger => CheckDot::Danger,
        };
        let all: Vec<CheckRow> = rows.all().iter().map(|(name, r)| CheckRow { name: SharedString::from(*name), text: SharedString::from(r.text.as_str()), dot: dot(r.dot) }).collect();
        self.check_rows.set_vec(all);
    }

    // ---------- toasts, clipboard ----------

    fn push_toasts(&self) {
        let items: Vec<ToastItem> = self.st.borrow().toasts.iter().map(|(id, text, kind)| ToastItem { id: *id, text: text.as_str().into(), kind: *kind, action: SharedString::new() }).collect();
        crate::sync_toasts(&self.toast_rows, items);
    }

    pub fn toast(self: &Rc<Self>, text: impl Into<String>, kind: ToastKind) {
        let id = {
            let mut st = self.st.borrow_mut();
            st.toast_seq += 1;
            let id = st.toast_seq;
            st.toasts.push((id, text.into(), kind));
            let n = st.toasts.len();
            if n > 4 {
                let gone: Vec<i32> = st.toasts.drain(..n - 4).map(|t| t.0).collect();
                for g in gone {
                    st.toast_time.remove(&g);
                }
            }
            id
        };
        self.push_toasts();
        self.arm_toast(id, if kind == ToastKind::Error { 10_000 } else { 4_000 });
    }

    /// A toast's timer. Behind a dialog a toast stays in sight (inert) but cannot be acted on, so
    /// its time does not run out while a dialog is open: `dialogs_closed` gives it its full time
    /// again (the family's rule, as in VibeRename). An older timer of a toast does nothing.
    fn arm_toast(self: &Rc<Self>, id: i32, ms: u64) {
        let seq = {
            let mut st = self.st.borrow_mut();
            let t = st.toast_time.entry(id).or_insert((0, ms));
            t.0 += 1;
            t.0
        };
        let me = Rc::downgrade(self);
        Timer::single_shot(Duration::from_millis(ms), move || {
            let Some(a) = me.upgrade() else { return };
            let current = a.st.borrow().toast_time.get(&id).is_some_and(|t| t.0 == seq);
            if current && !a.ui().is_some_and(|ui| ui.get_dialog_open()) {
                a.dismiss_toast(id);
            }
        });
    }

    fn dialogs_closed(self: &Rc<Self>) {
        if let Some(ui) = self.ui() {
            ui.invoke_focus_list();
        }
        let all: Vec<(i32, u64)> = self.st.borrow().toast_time.iter().map(|(id, t)| (*id, t.1)).collect();
        for (id, ms) in all {
            self.arm_toast(id, ms);
        }
    }

    fn dismiss_toast(&self, id: i32) {
        {
            let mut st = self.st.borrow_mut();
            st.toasts.retain(|t| t.0 != id);
            st.toast_time.remove(&id);
        }
        self.push_toasts();
    }

    fn copy_text(self: &Rc<Self>, t: &str) -> bool {
        let ok = (self.copy)(t);
        if !ok {
            self.toast("Could not copy: the clipboard is busy", ToastKind::Error);
        }
        ok
    }

    // ---------- for the tests ----------

    /// The tree, for tests and measurements.
    pub fn tree(&self) -> Option<Arc<RwLock<Tree>>> {
        self.view.borrow().tree.clone()
    }

    pub fn tree_rows(&self) -> Vec<crate::TreeRow> {
        let n = self.view.borrow().rows.len();
        (0..n).filter_map(|i| self.tree_model.row_data(i)).collect()
    }

    pub fn file_rows(&self) -> Vec<crate::FileRow> {
        let n = self.view.borrow().largest.len();
        (0..n).filter_map(|i| self.file_model.row_data(i)).collect()
    }

    /// The cursor's path.
    pub fn cursor_path(&self) -> Option<String> {
        let v = self.view.borrow();
        let c = v.cursor?;
        let t = v.tree.as_ref()?.read().unwrap();
        Some(t.display_path(c))
    }

    pub fn map_root_path(&self) -> Option<String> {
        let root = self.st.borrow().map.root;
        let v = self.view.borrow();
        let t = v.tree.as_ref()?.read().unwrap();
        Some(t.display_path(root))
    }

    /// The map's tiles: (path, x0, y0, x1, y1) in pixels, deepest last.
    pub fn map_tiles(&self) -> Vec<(String, i32, i32, i32, i32)> {
        let st = self.st.borrow();
        let v = self.view.borrow();
        let Some(tree) = v.tree.as_ref() else { return Vec::new() };
        let t = tree.read().unwrap();
        st.map.tiles.iter().map(|x| (if x.kind == crate::treemap::Kind::Rest { String::new() } else { t.display_path(x.node) }, x.x0, x.y0, x.x1, x.y1)).collect()
    }

    pub fn map_scale(&self) -> f32 {
        self.st.borrow().map.scale
    }
}

/// The one normaliser for a path from outside (typed, the Scan button, a drop, the command
/// line): spaces and quotes around it go (a pasted `"C:\x"`, and the `C:\x"` Windows makes of
/// a quoted argument ending in a backslash); nothing left is no path. Works on the units, so a
/// name Rust cannot spell stays exact. A file standing for its folder is the scan's own call.
pub fn typed_path(p: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let units: Vec<u16> = p.encode_wide().collect();
    let space = |u: &u16| char::from_u32(u32::from(*u)).is_some_and(char::is_whitespace);
    let quote = |u: &u16| *u == u16::from(b'"');
    let trim = |mut s: &[u16], f: &dyn Fn(&u16) -> bool| {
        while s.first().is_some_and(f) {
            s = &s[1..];
        }
        while s.last().is_some_and(f) {
            s = &s[..s.len() - 1];
        }
        s.to_vec()
    };
    let t = trim(&trim(&trim(&units, &space), &quote), &space);
    (!t.is_empty()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&t)))
}

/// Where the folder picker opens: the scan's folder, but only on a drive known to be local.
/// The picker resolves its start on the UI thread before its own loop runs, so a share that
/// does not answer (a UNC path or a mapped drive, or one whose kind is not known yet) would
/// freeze the window ~21 s (VibeBackup BA-1); it then opens where it was last instead.
fn browse_start(root: &Path, vol: &Volume) -> Option<PathBuf> {
    let local = !vol.root.is_empty() && !vol.remote && !root.as_os_str().to_string_lossy().starts_with("\\\\");
    local.then(|| root.to_path_buf())
}

/// A menu entry.
fn entry(label: &str, icon: SharedString, shortcut: &str, danger: bool, disabled: bool) -> MenuEntry {
    MenuEntry { label: label.into(), icon, shortcut: shortcut.into(), danger, separator: false, disabled }
}

fn separator() -> MenuEntry {
    MenuEntry { label: SharedString::new(), icon: SharedString::new(), shortcut: SharedString::new(), danger: false, separator: true, disabled: true }
}
