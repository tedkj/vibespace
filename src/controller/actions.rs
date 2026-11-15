//! What the selection can do: open, show in Explorer, copy the path, rescan, move to the
//! Recycle Bin (asked first, with the total; the tree updates without a rescan), and the
//! exports (the folder's listing as text, the scan's top folders and files as CSV).

use super::view::tops;
use super::{Action, App, Fate, Msg, entry, separator};
use crate::csvformat::CsvFormat;
use crate::tree::{self, Metric, NONE, Tree};
use crate::{Icons, ToastKind, format};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::path::PathBuf;
use std::rc::Rc;

/// An item for the Recycle Bin as the scan saw it.
struct Check {
    /// What the shell is given (it cannot parse the verbatim form).
    plain: PathBuf,
    /// The `\\?\` form: what is read.
    exact: Vec<u16>,
    id: u64,
    modified: u32,
    size: Option<u64>,
    dir: bool,
}

/// The counted files at or under `n` (`exact`: its `\\?\` path) that have a name the tree does
/// not know of (`known_names`: not in the scan, not sent to the bin from it), as (id, size on
/// disk): emptying the bin frees nothing for one of them unless the scan still holds a name of
/// it (X15WFS-4, X15R2BC-1). On the worker, before the bin renames them (X15R2BC-2): one path
/// buffer for the walk, and one handle and one query per counted file.
fn named_outside(t: &Tree, n: u32, exact: &[u16]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut path = exact.to_vec();
    let mut stack = vec![(n, None::<usize>)];
    while let Some((x, parent_len)) = stack.pop() {
        if let Some(len) = parent_len {
            path.truncate(len);
            if path.last() != Some(&(b'\\' as u16)) {
                path.push(b'\\' as u16);
            }
            t.push_name(x, &mut path);
        }
        let y = t.node(x);
        if y.is_dir() {
            let len = path.len();
            stack.extend(t.children(x).map(|c| (c, Some(len))));
        } else if !y.has(tree::HARDLINK | tree::LINK | tree::ONLINE) && y.alloc > 0 && y.id != 0 && y.id != u64::MAX && crate::scan::links(&path) > t.known_names(y.id) {
            out.push((y.id, y.alloc));
        }
    }
    out
}

/// Right before the shell: Err is the item's fate without the shell (it is not sent).
fn before(c: &Check, serial: u64) -> Result<(), Fate> {
    use crate::scan::{error_text, folder_answers, ident, parent_of};
    match ident(&c.exact) {
        Ok(i) => as_seen(c, &i, serial),
        Err(e) if matches!(e, 2 | 3) && folder_answers(parent_of(&c.exact)) => Err(Fate::AlreadyGone),
        Err(e) => Err(Fate::Unknown(error_text(e))),
    }
}

/// The item as it is now (`now`) against what the scan saw: Ok only while it is provably that
/// item.
fn as_seen(c: &Check, now: &crate::scan::Ident, serial: u64) -> Result<(), Fate> {
    if now.drive {
        Err(Fate::Drive)
    } else if !identified(c, now, serial) {
        Err(Fate::Unknown(UNIDENTIFIED.into()))
    } else if changed(c, now, serial) || now.online {
        // Online only now (OneDrive freed its space since the scan): the scan never offers one
        // (FS4), so it changed; read again, it shows as online only.
        Err(Fate::Changed)
    } else {
        Ok(())
    }
}

/// After the shell: gone only when not found with its folder answering.
fn after(c: &Check) -> Fate {
    use crate::scan::{error_text, folder_answers, ident, parent_of};
    match ident(&c.exact) {
        Ok(_) => Fate::Stayed,
        Err(e) if matches!(e, 2 | 3) && folder_answers(parent_of(&c.exact)) => Fate::Moved,
        Err(e) => Fate::Unknown(error_text(e)),
    }
}

/// Why an item whose identity is not known on both sides is not sent (its toast: "… could not
/// be checked (…): left in the list").
const UNIDENTIFIED: &str = "Its file id or volume serial is not known";

fn known(x: u64) -> bool {
    x != 0 && x != u64::MAX
}

/// The volume serial and the file id are known both as the scan saw them and as they are now:
/// a zero (or all-ones) id or serial is unknown, never "the same", and nothing else stands in
/// for it (a file replaced under its name may keep its size and time; SOL-SPACE-2).
fn identified(c: &Check, now: &crate::scan::Ident, serial: u64) -> bool {
    known(serial) && known(now.serial) && known(c.id) && known(now.id)
}

/// Whether the path names something other than what the scan saw (both `identified`): another
/// volume, another file id, a file whose time or size moved. A folder is its id (its time
/// moves whenever something in it does).
fn changed(c: &Check, now: &crate::scan::Ident, serial: u64) -> bool {
    if serial != now.serial || now.dir != c.dir || c.id != now.id {
        return true;
    }
    if !c.dir && c.modified != now.modified {
        return true;
    }
    c.size.is_some_and(|s| s != now.size)
}

/// Names for a toast: the one name, or how many.
fn some(names: &[String]) -> String {
    if names.len() == 1 { names[0].clone() } else { format!("{} items", format::count(names.len() as u64)) }
}

/// What a Delete of the selected items would do, as the scan knows it (FS4).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Doomed {
    /// What is offered to the Recycle Bin.
    pub go: Vec<u32>,
    /// Left out: online only (in the cloud, not on this PC), or holding something that is.
    /// Deleting it frees nothing here, never reaches this PC's Recycle Bin and deletes it from
    /// the cloud on every device.
    pub online: Vec<u32>,
    /// Of `go`, how many are kept in step with the cloud (OneDrive): deleting them here deletes
    /// them from the cloud and every other device too.
    pub synced: u64,
    /// Folders in `go` (themselves or under them) whose contents the scan does not know: no
    /// access, not read in full, not read.
    pub unread: u64,
}

/// Split `items` (the selection's tops) for a Delete. `cloud_roots`: OneDrive's folders (an
/// item in one, or holding one, is synced even where the scan saw no placeholder).
pub(super) fn doomed(t: &Tree, items: &[u32], cloud_roots: &[PathBuf]) -> Doomed {
    let mut d = Doomed::default();
    for &n in items {
        let under = t.subtree(n);
        if under.iter().any(|&x| t.node(x).has(tree::ONLINE)) {
            d.online.push(n);
            continue;
        }
        let mut synced = under.iter().any(|&x| t.node(x).has(tree::CLOUD));
        let mut p = t.node(n).parent;
        while !synced && p != NONE {
            synced = t.node(p).has(tree::CLOUD);
            p = t.node(p).parent;
        }
        if !synced && !cloud_roots.is_empty() {
            let path = t.path(n);
            synced = cloud_roots.iter().any(|r| crate::relocate::within(&path, r) || crate::relocate::within(r, &path));
        }
        d.synced += synced as u64;
        d.unread += under.iter().filter(|&&x| t.node(x).is_dir() && t.node(x).has(tree::NO_ACCESS | tree::UNREADABLE | tree::PENDING)).count() as u64;
        d.go.push(n);
    }
    d
}

/// "a", "a and b", "a, b and c", "a, b and 3 more".
fn list_names(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        2 => format!("{} and {}", names[0], names[1]),
        3 => format!("{}, {} and {}", names[0], names[1], names[2]),
        n => format!("{}, {} and {} more", names[0], names[1], format::count(n as u64 - 2)),
    }
}

/// Why online-only items stay out of a Delete, in words (`n` of them).
fn online_why(n: usize) -> String {
    let (is, it) = if n == 1 { ("is", "it") } else { ("are", "them") };
    format!("{is} online only: in the cloud (OneDrive), not on this PC. Deleting {it} would free no space here and would delete {it} from the cloud on every device, so VibeSpace leaves {it} alone")
}

/// Folders and files the CSV lists, each.
pub const CSV_TOP: usize = 1000;

impl App {
    /// The folder an action on the cursor means: itself, or the file's folder.
    fn cursor_folder(&self) -> Option<u32> {
        let c = self.view.borrow().cursor?;
        self.with_tree(|t| if t.node(c).is_dir() && !t.node(c).has(tree::LINK) { c } else if t.node(c).parent == NONE { 0 } else { t.node(c).parent })
    }

    pub(super) fn act(self: &Rc<Self>, a: Action) {
        let Some(tree) = self.view.borrow().tree.clone() else { return };
        let cursor = self.view.borrow().cursor;
        match a {
            Action::Open | Action::Reveal => {
                let Some(c) = cursor else { return };
                let path = tree.read().unwrap().path(c);
                self.st.borrow_mut().shell_pending += 1;
                let (shell, send) = (self.shell.clone(), self.send());
                // The shell may take its time (a network path): never on the UI thread.
                std::thread::spawn(move || {
                    let r = if a == Action::Open { shell.open(&path) } else { shell.reveal(&path) };
                    send(Msg::ShellDone(r));
                });
            }
            Action::CopyPath => {
                let paths: Vec<String> = {
                    let t = tree.read().unwrap();
                    let sel = self.view.borrow().sel.clone();
                    // The real paths (cleaned only where they are shown): pasted elsewhere,
                    // they must still reach the files.
                    let mut v: Vec<String> = sel.iter().filter(|&&n| !t.node(n).has(tree::GONE)).map(|&n| tree::raw_path(&t, n)).collect();
                    if v.is_empty()
                        && let Some(c) = cursor
                    {
                        v.push(tree::raw_path(&t, c));
                    }
                    v
                };
                if paths.is_empty() {
                    return;
                }
                if self.copy_text(&paths.join("\n")) {
                    self.toast(if paths.len() == 1 { "Copied the path".to_string() } else { format!("Copied {} paths", format::count(paths.len() as u64)) }, ToastKind::Ok);
                }
            }
            Action::CopyListing => {
                let Some(d) = self.cursor_folder() else { return };
                let m = self.view.borrow().metric;
                let text = listing(&tree.read().unwrap(), d, m);
                if self.copy_text(&text) {
                    self.toast("Copied the folder's listing", ToastKind::Ok);
                }
            }
            Action::SaveCsv => self.save_csv(),
            Action::Rescan => {
                if self.st.borrow().running.is_some() || self.st.borrow().recycling {
                    return;
                }
                if let Some(d) = self.cursor_folder() {
                    self.start_rescan(vec![d]);
                }
            }
            Action::Recycle => self.ask_recycle(),
            Action::Move => self.ask_move(),
            Action::MoveBack => self.ask_move_back(),
            Action::ShowMove => self.ask_show_move(),
        }
    }

    // ---------- menus ----------

    fn open_menu(&self, items: Vec<(Option<Action>, crate::MenuEntry)>, x: f32, y: f32) {
        let Some(ui) = self.ui() else { return };
        let entries: Vec<crate::MenuEntry> = items.iter().map(|i| i.1.clone()).collect();
        self.st.borrow_mut().menu = items.into_iter().map(|i| i.0).collect();
        ui.set_menu_entries(ModelRc::new(VecModel::from(entries)));
        ui.invoke_open_menu(x, y);
    }

    pub(super) fn more_menu(self: &Rc<Self>, x: f32, y: f32) {
        let Some(ui) = self.ui() else { return };
        let ic = ui.global::<Icons>();
        let has = self.view.borrow().tree.is_some();
        let saving = self.st.borrow().saving;
        let items = vec![
            (Some(Action::CopyListing), entry("Copy this folder's listing", ic.get_copy(), "", false, !has)),
            (Some(Action::SaveCsv), entry("Save the 1 000 largest as CSV…", ic.get_download(), "", false, !has || saving)),
        ];
        self.open_menu(items, x, y);
    }

    fn context_menu(self: &Rc<Self>, x: f32, y: f32) {
        let Some(ui) = self.ui() else { return };
        let ic = ui.global::<Icons>();
        let can_recycle = ui.get_can_recycle();
        let can_rescan = ui.get_can_rescan();
        let (can_move, can_back) = self.move_offers();
        let items = vec![
            (Some(Action::Open), entry("Open", ic.get_external(), "Enter", false, false)),
            (Some(Action::Reveal), entry("Show in Explorer", ic.get_folder_open(), "", false, false)),
            (Some(Action::CopyPath), entry("Copy path", ic.get_copy(), "Ctrl+C", false, false)),
            (Some(Action::CopyListing), entry("Copy the folder's listing", ic.get_copy(), "", false, false)),
            (None, separator()),
            (Some(Action::Rescan), entry("Rescan this folder", ic.get_refresh(), "F5", false, !can_rescan)),
            if can_back {
                (Some(Action::MoveBack), entry("Move back…", ic.get_link(), "", false, false))
            } else if self.unfinished_offer() {
                (Some(Action::ShowMove), entry("Show the unfinished move…", ic.get_refresh(), "", false, false))
            } else {
                (Some(Action::Move), entry("Move to another drive…", ic.get_drive(), "", false, !can_move))
            },
            (None, separator()),
            (Some(Action::Recycle), entry("Move to Recycle Bin", ic.get_trash(), "Del", true, !can_recycle)),
        ];
        self.open_menu(items, x, y);
    }

    /// A right-click: the row is selected first (unless it already is), then the menu.
    pub(super) fn row_context(self: &Rc<Self>, i: usize, x: f32, y: f32) {
        let n = self.view.borrow().rows.get(i).map(|r| r.0);
        let Some(n) = n else { return };
        if !self.view.borrow().sel.contains(&n) {
            self.row_pressed(i, false, false);
        } else {
            self.view.borrow_mut().cursor = Some(n);
            self.tree_model.reset();
            self.push_cursor();
        }
        self.context_menu(x, y);
    }

    pub(super) fn file_context(self: &Rc<Self>, i: usize, x: f32, y: f32) {
        let n = self.view.borrow().largest.get(i).copied();
        let Some(n) = n else { return };
        if !self.view.borrow().sel.contains(&n) {
            self.file_pressed(i, false, false);
        }
        self.context_menu(x, y);
    }

    pub(super) fn menu_picked(self: &Rc<Self>, i: usize) {
        let a = self.st.borrow().menu.get(i).copied().flatten();
        if let Some(a) = a {
            self.act(a);
        }
    }

    // ---------- the Recycle Bin ----------

    fn ask_recycle(self: &Rc<Self>) {
        let Some(ui) = self.ui() else { return };
        if !ui.get_can_recycle() {
            let tip = ui.get_recycle_tip().to_string();
            if self.view.borrow().tree.is_some() && !tip.is_empty() && !tip.starts_with("Move") {
                self.toast(tip, ToastKind::Info);
            }
            return;
        }
        let Some(tree) = self.view.borrow().tree.clone() else { return };
        let t = tree.read().unwrap();
        let sel = self.view.borrow().sel.clone();
        let tops: Vec<u32> = tops(&t, &sel).into_iter().filter(|&n| n != 0).collect();
        if tops.is_empty() {
            return;
        }
        // FS4: online-only items (and folders holding one) never go: deleting them frees
        // nothing here, they never reach this PC's bin, and the cloud deletes them everywhere.
        let Doomed { go: items, online, synced, unread } = doomed(&t, &tops, &self.relocate_env.onedrive);
        let left: Vec<String> = online.iter().map(|&n| t.name(n)).collect();
        if items.is_empty() {
            drop(t);
            self.toast(format!("{} {}", list_names(&left), online_why(left.len())), ToastKind::Info);
            return;
        }
        let (size, alloc) = items.iter().fold((0u64, 0u64), |a, &n| (a.0 + t.node(n).size, a.1 + t.node(n).alloc));
        let (files, folders) = items.iter().fold((0u64, 0u64), |a, &n| {
            let (f, d) = t.counts(n);
            if t.node(n).is_dir() { (a.0 + f, a.1 + d + 1) } else { (a.0 + 1, a.1) }
        });
        let names: Vec<String> = items.iter().map(|&n| t.name(n)).collect();
        let what = list_names(&names);
        let mut counts = Vec::new();
        if folders > 0 {
            counts.push(format::plural(folders, "folder", "folders"));
        }
        counts.push(format::plural(files, "file", "files"));
        let total = format!("{} on disk · {} · {}", format::bytes(alloc), format::bytes(size), counts.join(", "));
        let mut note = String::new();
        if unread > 0 {
            // A folder the scan could not read holds more than these totals say: never a
            // made-up "0 files" (FS4).
            note.push_str(&format!(
                "{} could not be read, so what is in {} is not known and the totals above may be low. ",
                format::plural(unread, "folder", "folders"),
                if unread == 1 { "it" } else { "them" }
            ));
        }
        note.push_str(&if synced == 0 {
            "Nothing is deleted for good: it can be put back from the Recycle Bin, and the space comes back once the bin is emptied.".to_string()
        } else {
            let (who, it) = if items.len() == 1 {
                ("This is".to_string(), "it")
            } else if synced == items.len() as u64 {
                ("These are".to_string(), "them")
            } else {
                (format!("{} of these are", format::count(synced)), "them")
            };
            format!(
                "{who} kept in step with the cloud (OneDrive): deleting {it} here deletes {it} from the cloud and every other device too. {} can still be put back from this PC's Recycle Bin, and the space comes back once the bin is emptied.",
                if it == "it" { "It" } else { "They" }
            )
        });
        if !left.is_empty() {
            note.push_str(&format!(" Left out: {}, which {}.", list_names(&left), online_why(left.len())));
        }
        drop(t);
        self.st.borrow_mut().confirm = items;
        ui.set_recycle_what(what.into());
        ui.set_recycle_total(total.into());
        ui.set_recycle_note(note.into());
        ui.set_recycle_open(true);
    }

    pub(super) fn recycle_cancelled(self: &Rc<Self>) {
        self.st.borrow_mut().confirm.clear();
        if let Some(ui) = self.ui() {
            ui.set_recycle_open(false);
        }
    }

    pub(super) fn recycle_confirmed(self: &Rc<Self>) {
        let nodes = std::mem::take(&mut self.st.borrow_mut().confirm);
        if let Some(ui) = self.ui() {
            ui.set_recycle_open(false);
        }
        let Some(tree) = self.view.borrow().tree.clone() else { return };
        if nodes.is_empty() || self.st.borrow().running.is_some() {
            return;
        }
        // The shell gets the plain path (it cannot parse the verbatim form); the item is read
        // through its exact one (a plain `foo.` is `foo` to Win32: "not found" for a file that
        // is still there).
        let checks = {
            let t = tree.read().unwrap();
            let checks: Vec<Check> = nodes
                .iter()
                .map(|&n| {
                    let x = t.node(n);
                    Check {
                        plain: t.path(n),
                        exact: crate::scan::long(&t.path_units(n)),
                        id: x.id,
                        modified: x.modified,
                        // A hard-link name shows 0: its size is the counted name's.
                        size: (!x.is_dir() && !x.has(tree::HARDLINK)).then_some(x.size),
                        dir: x.is_dir(),
                    }
                })
                .collect();
            // Hard links are told apart only where ids are trusted, and a scan of a whole volume
            // holds every name a file there can have (X15R2BC-2): only then is nothing read.
            let links = t.seen.on.then(|| t.root_path.clone());
            (checks, self.st.borrow().volume.serial, links)
        };
        let (checks, serial, links_root) = checks;
        self.st.borrow_mut().recycling = true;
        self.push_actions();
        self.push_status();
        let tree_gen = self.st.borrow().tree_gen;
        let (shell, send) = (self.shell.clone(), self.send());
        std::thread::spawn(move || {
            // Right before the shell: each item is read again; only what is still what the scan
            // saw goes. After: an item went when its path is not found with its folder
            // answering (a permission error, a drive that went, is no proof either way).
            let mut fates: Vec<Option<Fate>> = checks.iter().map(|c| before(c, serial).err()).collect();
            let go: Vec<usize> = (0..checks.len()).filter(|&i| fates[i].is_none()).collect();
            // Read before they go (the bin renames them): files with a name outside the scan.
            let mut held = vec![Vec::new(); checks.len()];
            if links_root.is_some_and(|r| !go.is_empty() && !crate::scan::volume_root(&r)) {
                let t = tree.read().unwrap();
                for &i in &go {
                    held[i] = named_outside(&t, nodes[i], &checks[i].exact);
                }
            }
            let result = if go.is_empty() { Ok(()) } else { shell.recycle(&go.iter().map(|&i| checks[i].plain.clone()).collect::<Vec<_>>()) };
            for i in go {
                fates[i] = Some(after(&checks[i]));
            }
            let fates = fates.into_iter().map(|f| f.unwrap_or(Fate::Stayed)).collect();
            send(Msg::Recycled { tree: tree_gen, nodes, result, fates, held });
        });
    }

    pub(super) fn recycled(self: &Rc<Self>, tree_gen: u64, nodes: Vec<u32>, result: Result<(), crate::shell::Refusal>, fates: Vec<Fate>, held: Vec<Vec<(u64, u64)>>) {
        // Refused before anything moved: a folder that stayed lost nothing (R2-SPACE-4).
        let nothing_went = result.as_ref().is_err_and(|r| r.nothing_went);
        self.st.borrow_mut().recycling = false;
        let Some(tree) = self.view.borrow().tree.clone() else { return };
        if tree_gen != self.st.borrow().tree_gen {
            // Another scan is on screen now: its nodes are not these. Say what went, nothing more.
            let n = fates.iter().filter(|&f| *f == Fate::Moved).count() as u64;
            if n > 0 {
                self.toast(format!("Moved {} to the Recycle Bin", format::plural(n, "item", "items")), ToastKind::Ok);
            }
            self.push_all();
            return;
        }
        let (mut moved, mut already, mut already_names, mut stayed) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut changed, mut drives, mut unknown, mut rescan) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut held_n, mut held_alloc) = (0u64, 0u64);
        let mut named = Vec::new();
        let freed = {
            let mut t = tree.write().unwrap();
            for (i, (&n, f)) in nodes.iter().zip(&fates).enumerate() {
                if n as usize >= t.nodes.len() || t.node(n).has(tree::GONE) {
                    continue;
                }
                let dir = t.node(n).is_dir();
                match f {
                    Fate::Moved => {
                        moved.push(n);
                        named.extend(held.get(i).into_iter().flatten().copied());
                    }
                    Fate::AlreadyGone => {
                        already.push(n);
                        already_names.push(t.name(n));
                    }
                    Fate::Stayed | Fate::Unknown(_) => {
                        match f {
                            Fate::Unknown(e) => unknown.push((t.name(n), e.clone())),
                            _ => stayed.push(t.name(n)),
                        }
                        // Part of a folder may have gone: read it again rather than guess.
                        if dir && !nothing_went {
                            rescan.push(n);
                        }
                    }
                    Fate::Changed => {
                        changed.push(t.name(n));
                        let p = t.node(n).parent;
                        rescan.push(if dir || p == NONE { n } else { p });
                    }
                    Fate::Drive => drives.push(t.name(n)),
                }
            }
            // What the tree lost, not what the items showed: a hard-linked file's other name
            // takes its size over (emptying the bin frees nothing for it).
            t.remove(&already);
            let before = t.node(0).alloc;
            t.recycle(&moved);
            // A file with a name outside the scan stays on the disk under that name. Only one
            // the tree lost counts: one whose name in the scan took it over (`forget`) is not in
            // what the tree lost in the first place (X15R2BC-1).
            let mut once = std::collections::HashSet::new();
            for (id, alloc) in named {
                if !t.seen.contains(id) && once.insert(id) {
                    held_n += 1;
                    held_alloc += alloc;
                }
            }
            let freed = before.saturating_sub(t.node(0).alloc).saturating_sub(held_alloc);
            // Each folder read again once, the outermost of nested ones.
            let set: std::collections::HashSet<u32> = rescan.iter().copied().filter(|&d| !t.node(d).has(tree::GONE)).collect();
            rescan = set
                .iter()
                .copied()
                .filter(|&d| {
                    let mut p = t.node(d).parent;
                    while p != NONE {
                        if set.contains(&p) {
                            return false;
                        }
                        p = t.node(p).parent;
                    }
                    true
                })
                .collect();
            rescan.sort_unstable();
            freed
        };
        self.fix_selection();
        self.st.borrow_mut().stats_stale = true;
        let moved = moved.len() as u64;
        if moved > 0 {
            let what = if moved == 1 { "1 item".to_string() } else { format!("{} items", format::count(moved)) };
            let mut text = format!("Moved {what} to the Recycle Bin: {} on disk, back once the bin is emptied", format::bytes(freed));
            if held_n > 0 {
                let (has, its) = if held_n == 1 { ("has", "its") } else { ("have", "their") };
                text.push_str(&format!(
                    ". {} also {has} a name outside the scanned folder (hard links): {its} {} stay in use",
                    format::plural(held_n, "file", "files"),
                    format::bytes(held_alloc)
                ));
            }
            self.toast(text, ToastKind::Ok);
        }
        if !already_names.is_empty() {
            let names = if already_names.len() == 1 { format!("{} was", already_names[0]) } else { format!("{} items were", already_names.len()) };
            self.toast(format!("{names} already gone (deleted since the scan): taken off the list, nothing was moved"), ToastKind::Info);
        }
        if !changed.is_empty() {
            self.toast(format!("{} changed since the scan: not moved, read again", some(&changed)), ToastKind::Info);
        }
        if !drives.is_empty() {
            self.toast(format!("{} leads to a drive: VibeSpace never removes a drive", some(&drives)), ToastKind::Error);
        }
        if !unknown.is_empty() {
            let names: Vec<String> = unknown.iter().map(|u| u.0.clone()).collect();
            self.toast(format!("{} could not be checked ({}): left in the list", some(&names), unknown[0].1.to_lowercase()), ToastKind::Error);
        }
        if !stayed.is_empty() {
            let why = result.err().map_or_else(|| "it is in use or protected".into(), |r| r.why);
            self.toast(format!("{} stayed where it was: {why}", some(&stayed)), ToastKind::Error);
        }
        self.refresh_rows();
        self.refresh_stats();
        self.repaint_map();
        self.push_all();
        if !rescan.is_empty() {
            self.start_rescan(rescan);
        }
    }

    // ---------- exports ----------

    fn save_csv(self: &Rc<Self>) {
        let Some(tree) = self.view.borrow().tree.clone() else { return };
        if self.st.borrow().saving {
            return;
        }
        let (name, start) = {
            let t = tree.read().unwrap();
            let r = t.root_path.display().to_string();
            let base: String = r.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect::<String>().trim_matches('-').to_string();
            (format!("VibeSpace {}.csv", if base.is_empty() { "scan".into() } else { base }), None)
        };
        let Some(path) = (self.save_csv)(&name, start) else { return };
        let m = self.view.borrow().metric;
        let fmt = (self.csv_format)();
        let (body, left) = {
            let t = tree.read().unwrap();
            (csv(&t, m, CSV_TOP, &fmt), left_out(&t, CSV_TOP))
        };
        self.st.borrow_mut().saving = true;
        let send = self.send();
        std::thread::spawn(move || {
            let r = crate::prefs::write_atomic(&path, body.as_bytes()).map(|_| (path.clone(), left)).map_err(|e| format!("Could not save {}: {e}", path.display()));
            send(Msg::Saved(r));
        });
    }
}

/// What the CSV leaves out, in words ("" when nothing): the folders and files past the
/// largest `top`, and the folders the scan did not read.
pub fn left_out(t: &Tree, top: usize) -> String {
    let (mut dirs, mut files) = (0u64, 0u64);
    for (_, n) in t.live() {
        if n.is_dir() { dirs += 1 } else { files += 1 }
    }
    let mut parts = Vec::new();
    let top_n = format::count(top as u64);
    if dirs > top as u64 {
        parts.push(format!("the {top_n} largest of {} folders", format::count(dirs)));
    }
    if files > top as u64 {
        parts.push(format!("the {top_n} largest of {} files", format::count(files)));
    }
    let s = t.skipped();
    let unread = s.no_access + s.unreadable + s.pending + s.online;
    if unread > 0 {
        parts.push(format!("{} not read", format::plural(unread, "folder", "folders")));
    }
    parts.join(", ")
}

/// What a listing says after a folder's name when the scan could not read it all.
fn note(x: &tree::Node) -> &'static str {
    if !x.is_dir() {
        ""
    } else if x.has(tree::NO_ACCESS) {
        " (no access)"
    } else if x.has(tree::UNREADABLE) {
        " (could not be read in full)"
    } else if x.has(tree::PENDING) {
        " (not read)"
    } else if x.has(tree::ONLINE) {
        " (online only, not listed)"
    } else {
        ""
    }
}

/// A folder's listing as text: its total, then its children largest first.
pub fn listing(t: &Tree, d: u32, m: Metric) -> String {
    let n = t.node(d);
    let (files, folders) = t.counts(d);
    let mut out = format!(
        "{}\n{} · {} on disk · {} · {}\n\n",
        t.display_path(d),
        format::bytes(n.size),
        format::bytes(n.alloc),
        format::plural(files, "file", "files"),
        format::plural(folders, "folder", "folders")
    );
    out.push_str(if m == Metric::OnDisk { "On disk\tSize\tShare\tFiles\tName\n" } else { "Size\tOn disk\tShare\tFiles\tName\n" });
    let whole = n.value(m);
    for c in t.sorted_children(d, m) {
        let x = t.node(c);
        let (a, b) = if m == Metric::OnDisk { (x.alloc, x.size) } else { (x.size, x.alloc) };
        let (f, _) = t.counts(c);
        let name = if x.is_dir() { format!("{}\\{}", t.name(c), note(x)) } else { t.name(c) };
        // A folder nothing is known of has no size: a dash, never a made-up 0 (as the tree).
        let unknown = x.is_dir() && x.flags & (tree::NO_ACCESS | tree::PENDING) != 0 && !t.has_children(c);
        let size = |v: u64| if unknown { "—".to_string() } else { format::bytes(v) };
        let share = if unknown { "—".to_string() } else { format::pct(x.value(m), whole) };
        let files = if x.is_dir() && !unknown { format::count(f) } else { String::new() };
        out.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", size(a), size(b), share, files, name));
    }
    out
}

pub const CSV_HEADER: [&str; 7] = ["kind", "path", "size_bytes", "on_disk_bytes", "files", "folders", "modified"];

/// The scan's largest folders and files as CSV, exact bytes: UTF-8 with a BOM, in the user's
/// list separator (`f`); a path holding it is quoted, one a spreadsheet would run as a formula
/// gets an apostrophe.
pub fn csv(t: &Tree, m: Metric, top: usize, f: &CsvFormat) -> String {
    let mut out = String::from("\u{FEFF}");
    out.push_str(&CSV_HEADER.join(&f.sep));
    out.push('\n');
    let mut row = |cells: [String; 7]| {
        out.push_str(&cells.join(&f.sep));
        out.push('\n');
    };
    let mut dirs: Vec<u32> = t.dirs.iter().map(|d| d.node).filter(|&n| !t.node(n).has(tree::GONE)).collect();
    dirs.sort_unstable_by(|&a, &b| t.compare(a, b, m));
    dirs.truncate(top);
    let iso = |u: u32| format::stamp(u);
    for d in dirs {
        let x = t.node(d);
        let (files, folders) = t.counts(d);
        row(["folder".into(), f.field(&t.display_path(d), true), x.size.to_string(), x.alloc.to_string(), files.to_string(), folders.to_string(), iso(x.modified)]);
    }
    for n in crate::types::largest(t, top, m) {
        let x = t.node(n);
        row(["file".into(), f.field(&t.display_path(n), true), x.size.to_string(), x.alloc.to_string(), String::new(), String::new(), iso(x.modified)]);
    }
    out
}
