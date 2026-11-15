//! What the lists show: the folder tree flattened to the rows on screen (only expanded folders'
//! children), the largest files, the types; the selection and the keys that move it.

use super::{Action, App, key_is};
use crate::tree::{self, Metric, NONE, Tree};
use crate::{FileRow, TreeRow, TypeRow, format, types};
use slint::platform::Key;
use slint::{Color, ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;
use std::sync::{Arc, RwLock};

/// The lists' shared state: the tree, the rows on screen, the selection.
#[derive(Default)]
pub struct View {
    pub tree: Option<Arc<RwLock<Tree>>>,
    pub metric: Metric,
    /// The folder tree as rows: (node, depth).
    pub rows: Vec<(u32, u16)>,
    pub expanded: HashSet<u32>,
    pub sel: BTreeSet<u32>,
    pub cursor: Option<u32>,
    /// The largest files, largest first.
    pub largest: Vec<u32>,
    pub scanning: bool,
    /// The root row's name: its path, shortened in the middle to the name column's width.
    pub root_label: String,
    /// The lists' pane width (logical px): what the folder paths of Largest files are fitted to.
    pub pane: f32,
    /// The tab on screen: the hidden lists report no rows (no rows built, no accessibility
    /// nodes for what cannot be seen).
    pub tab: usize,
}

impl View {
    pub fn clear(&mut self) {
        *self = View { metric: self.metric, tab: self.tab, ..View::default() };
    }

    pub fn row_of(&self, node: u32) -> Option<usize> {
        self.rows.iter().position(|r| r.0 == node)
    }
}

/// The folder tree in rows: depth first, each expanded folder's children largest first.
pub fn flatten(t: &Tree, expanded: &HashSet<u32>, m: Metric) -> Vec<(u32, u16)> {
    let mut out = Vec::new();
    let mut stack: Vec<(u32, u16)> = vec![(0, 0)];
    while let Some((n, d)) = stack.pop() {
        out.push((n, d));
        if expanded.contains(&n) && t.node(n).is_dir() && !t.node(n).has(tree::LINK) {
            let kids = t.sorted_children(n, m);
            for &c in kids.iter().rev() {
                stack.push((c, d + 1));
            }
        }
    }
    out
}

/// The selected items that are not inside another selected one, nor the root, nor gone.
pub fn tops(t: &Tree, sel: &BTreeSet<u32>) -> Vec<u32> {
    sel.iter()
        .copied()
        .filter(|&n| (n as usize) < t.nodes.len() && !t.node(n).has(tree::GONE))
        .filter(|&n| {
            let mut p = t.node(n).parent;
            while p != NONE {
                if sel.contains(&p) {
                    return false;
                }
                p = t.node(p).parent;
            }
            true
        })
        .collect()
}

/// What a row's state is, in words, and whether it warns.
fn tag(t: &Tree, n: u32, scanning: bool) -> (&'static str, i32) {
    let x = t.node(n);
    if x.has(tree::NO_ACCESS) {
        ("no access", 1)
    } else if x.has(tree::UNREADABLE) {
        ("could not be read in full", 1)
    } else if x.has(tree::PENDING) {
        if scanning { ("reading…", 0) } else { ("not read", 1) }
    } else if x.has(tree::LINK) {
        if x.is_dir() { ("link, not followed", 0) } else { ("link", 0) }
    } else if x.has(tree::HARDLINK) {
        ("hard link, counted once", 0)
    } else if x.has(tree::ONLINE) {
        if x.is_dir() { ("online only, not listed", 0) } else { ("online only", 0) }
    } else if x.has(tree::COMPRESSED) {
        ("compressed", 0)
    } else if x.has(tree::SPARSE) {
        ("sparse", 0)
    } else {
        ("", 0)
    }
}

pub fn tree_row(t: &Tree, v: &View, n: u32, depth: u16) -> TreeRow {
    let x = t.node(n);
    let m = v.metric;
    let value = x.value(m);
    let parent = if x.parent == NONE { value } else { t.node(x.parent).value(m) };
    let share = if parent > 0 { value as f32 / parent as f32 } else { 0.0 };
    let (files, folders) = t.counts(n);
    let is_dir = x.is_dir();
    let (tg, level) = tag(t, n, v.scanning);
    let name = if n == 0 && !v.root_label.is_empty() { v.root_label.clone() } else { t.name(n) };
    // A folder not read (no access, cancelled before it) has no known size: a dash, not a 0.
    let unknown = is_dir && x.flags & (tree::NO_ACCESS | tree::PENDING) != 0 && !t.has_children(n) && !(v.scanning && x.has(tree::PENDING));
    let counted = is_dir && !x.has(tree::LINK) && !unknown;
    let size = if unknown { "—".to_string() } else { format::bytes(value) };
    let pct = if x.parent == NONE { "100 %".to_string() } else { format::pct(value, parent) };
    let mut desc = vec![if n == 0 { format!("the scanned folder {}", t.name(0)) } else if is_dir { "folder".to_string() } else { "file".to_string() }];
    desc.push(format!("{size} {}", if m == Metric::OnDisk { "on disk" } else { "in size" }));
    if x.parent != NONE {
        desc.push(format!("{pct} of its folder"));
    }
    if counted {
        desc.push(format::plural(files, "file", "files"));
        desc.push(format::plural(folders, "folder", "folders"));
    }
    if !tg.is_empty() {
        desc.push(tg.to_string());
    }
    TreeRow {
        name: name.into(),
        size: size.into(),
        share,
        pct: pct.into(),
        files: if counted { format::count(files).into() } else { SharedString::new() },
        folders: if counted { format::count(folders).into() } else { SharedString::new() },
        modified: format::date(x.modified).into(),
        depth: depth as i32,
        kind: if x.has(tree::LINK) { 2 } else if is_dir { 1 } else { 0 },
        expandable: is_dir && !x.has(tree::LINK) && t.has_children(n),
        expanded: v.expanded.contains(&n),
        tag: tg.into(),
        tag_level: level,
        selected: v.sel.contains(&n),
        cursor: v.cursor == Some(n),
        description: desc.join(", ").into(),
    }
}

pub struct TreeModel {
    view: Rc<RefCell<View>>,
    notify: ModelNotify,
}

impl TreeModel {
    pub fn new(view: Rc<RefCell<View>>) -> TreeModel {
        TreeModel { view, notify: ModelNotify::default() }
    }
    pub fn reset(&self) {
        self.notify.reset();
    }
    pub fn changed(&self, i: usize) {
        self.notify.row_changed(i);
    }
}

impl Model for TreeModel {
    type Data = TreeRow;
    fn row_count(&self) -> usize {
        let v = self.view.borrow();
        if v.tab == 0 { v.rows.len() } else { 0 }
    }
    fn row_data(&self, i: usize) -> Option<TreeRow> {
        let v = self.view.borrow();
        let &(n, d) = v.rows.get(i)?;
        let t = v.tree.as_ref()?.read().unwrap();
        if n as usize >= t.nodes.len() {
            return None;
        }
        Some(tree_row(&t, &v, n, d))
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

pub fn file_row(t: &Tree, v: &View, n: u32) -> FileRow {
    let x = t.node(n);
    let value = x.value(v.metric);
    let folder = if x.parent == NONE { String::new() } else { t.display_path(x.parent) };
    let (tg, _) = tag(t, n, v.scanning);
    // The folder gets what the name and the columns leave (as lists.slint lays them out).
    let name = t.name(n);
    let fixed = 20.0 + 16.0 + 8.0 + 8.0 + 68.0 + 8.0 + if v.pane >= 520.0 { 116.0 + 8.0 } else { 0.0 } + if tg.is_empty() { 0.0 } else { tg.len() as f32 * 6.3 + 8.0 };
    let name_px = (name.chars().count() as f32 * 6.3).min((v.pane - fixed).max(0.0)).max(120.0_f32.min(name.chars().count() as f32 * 6.3));
    let shown_folder = format::middle(&folder, ((v.pane - fixed - name_px) / 7.3).max(0.0) as usize);
    let size = format::bytes(value);
    let modified = format::stamp(x.modified);
    let mut desc = format!("{size} {}, in {folder}", if v.metric == Metric::OnDisk { "on disk" } else { "in size" });
    if !tg.is_empty() {
        desc.push_str(", ");
        desc.push_str(tg);
    }
    FileRow {
        name: name.into(),
        folder: shown_folder.into(),
        size: size.into(),
        modified: modified.into(),
        selected: v.sel.contains(&n),
        cursor: v.cursor == Some(n),
        tag: tg.into(),
        description: desc.into(),
    }
}

pub struct FileModel {
    view: Rc<RefCell<View>>,
    notify: ModelNotify,
}

impl FileModel {
    pub fn new(view: Rc<RefCell<View>>) -> FileModel {
        FileModel { view, notify: ModelNotify::default() }
    }
    pub fn reset(&self) {
        self.notify.reset();
    }
}

impl Model for FileModel {
    type Data = FileRow;
    fn row_count(&self) -> usize {
        let v = self.view.borrow();
        if v.tab == 2 { v.largest.len() } else { 0 }
    }
    fn row_data(&self, i: usize) -> Option<FileRow> {
        let v = self.view.borrow();
        let &n = v.largest.get(i)?;
        let t = v.tree.as_ref()?.read().unwrap();
        if n as usize >= t.nodes.len() || t.node(n).has(tree::GONE) {
            return None;
        }
        Some(file_row(&t, &v, n))
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

/// The largest files the Largest tab lists.
pub const LARGEST: usize = 1000;
/// The extensions with a hue of their own on the map.
pub const HUES: usize = 8;

impl App {
    // ---------- rows ----------

    /// The root row's path, fitted to the name column (the same widths as tree.slint).
    pub(super) fn fit_root_label(&self) {
        let label = {
            let v = self.view.borrow();
            let Some(tree) = v.tree.as_ref() else { return };
            let path = tree.read().unwrap().name(0);
            let pane = (self.window_width() - 1.0) * 0.55;
            let mut fixed = 18.0 + 16.0 + 6.0 + 16.0 + 6.0 + (68.0 + 6.0) + (98.0 + 6.0) + (68.0 + 6.0);
            if pane >= 560.0 {
                fixed += 74.0;
            }
            if pane >= 640.0 {
                fixed += 86.0;
            }
            format::middle(&path, ((pane - fixed) / 6.3).max(0.0) as usize)
        };
        let pane = (self.window_width() - 1.0) * 0.55;
        {
            let mut v = self.view.borrow_mut();
            v.root_label = label;
            v.pane = pane;
        }
        self.file_model.reset();
        self.tree_model.changed(0);
    }

    /// Rebuild the folder rows from the tree (sizes moved, folders came, things went).
    pub(super) fn refresh_rows(&self) {
        let scanning = self.st.borrow().running.is_some();
        {
            let mut v = self.view.borrow_mut();
            v.scanning = scanning;
            let rows = match v.tree.as_ref() {
                Some(tree) => {
                    let t = tree.read().unwrap();
                    flatten(&t, &v.expanded, v.metric)
                }
                None => Vec::new(),
            };
            v.rows = rows;
        }
        self.tree_model.reset();
        self.push_cursor();
    }

    /// The types and the largest files (a pass over every node).
    pub(super) fn refresh_stats(&self) {
        let (tab, stale) = {
            let st = self.st.borrow();
            (st.tab, st.stats_stale)
        };
        if !stale && tab == 0 {
            return;
        }
        let (stats, largest) = {
            let v = self.view.borrow();
            match v.tree.as_ref() {
                Some(tree) => {
                    let t = tree.read().unwrap();
                    (types::by_type(&t, v.metric), types::largest(&t, LARGEST, v.metric))
                }
                None => (Vec::new(), Vec::new()),
            }
        };
        {
            let mut st = self.st.borrow_mut();
            let m = self.view.borrow().metric;
            st.hues = types::hue_slots(&stats, m, HUES);
            st.types = stats;
            st.stats_stale = false;
        }
        self.view.borrow_mut().largest = largest;
        self.file_model.reset();
        self.push_types();
        self.push_cursor();
    }

    pub(super) fn push_types(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.st.borrow();
        let v = self.view.borrow();
        let Some(tree) = v.tree.as_ref() else {
            ui.set_type_rows(ModelRc::new(VecModel::<TypeRow>::default()));
            return;
        };
        let t = tree.read().unwrap();
        let total = t.node(0).value(v.metric);
        let pal = crate::treemap::palette(ui.global::<crate::Theme>().get_dark());
        let rows: Vec<TypeRow> = st
            .types
            .iter()
            .map(|s| {
                let name = if s.ext == 0 { "(no extension)".to_string() } else { format!(".{}", t.exts[s.ext as usize]) };
                let value = s.value(v.metric);
                let swatch = match st.hues.get(&s.ext) {
                    Some(&i) => rgb(pal.hues[i]),
                    None => rgb(pal.file),
                };
                let largest = if s.largest == NONE { String::new() } else { t.name(s.largest) };
                TypeRow {
                    ext: name.as_str().into(),
                    size: format::bytes(value).into(),
                    share: if total > 0 { value as f32 / total as f32 } else { 0.0 },
                    pct: format::pct(value, total).into(),
                    count: format::count(s.count).into(),
                    largest: largest.as_str().into(),
                    swatch,
                    selected: st.type_filter == Some(s.ext),
                    description: format!("{}, {} files, {} of the scan, largest {}", format::bytes(value), format::count(s.count), format::pct(value, total), largest).into(),
                }
            })
            .collect();
        ui.set_type_rows(ModelRc::new(VecModel::from(rows)));
    }

    /// The cursor's row and the actions it allows; the list scrolls to it.
    pub(super) fn push_cursor(&self) {
        let Some(ui) = self.ui() else { return };
        let (idx, fidx) = {
            let v = self.view.borrow();
            let i = v.cursor.and_then(|c| v.row_of(c)).map(|i| i as i32).unwrap_or(-1);
            let f = v.cursor.and_then(|c| v.largest.iter().position(|&x| x == c)).map(|i| i as i32).unwrap_or(-1);
            (i, f)
        };
        ui.set_cursor_index(idx);
        ui.set_file_cursor(fidx);
        self.push_actions();
        self.push_status();
    }

    fn scroll_to_cursor(&self) {
        let Some(ui) = self.ui() else { return };
        let (a, b) = {
            let mut st = self.st.borrow_mut();
            st.scroll_seq += 1;
            st.file_scroll_seq += 1;
            (st.scroll_seq, st.file_scroll_seq)
        };
        ui.set_scroll_seq(a);
        ui.set_file_scroll_seq(b);
    }

    pub(super) fn push_actions(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.st.borrow();
        let v = self.view.borrow();
        let Some(tree) = v.tree.as_ref() else {
            ui.set_can_open(false);
            ui.set_can_recycle(false);
            ui.set_can_rescan(false);
            return;
        };
        let t = tree.read().unwrap();
        let busy = st.running.is_some() || st.recycling || st.moving.busy;
        ui.set_can_open(v.cursor.is_some());
        ui.set_can_rescan(!busy && v.cursor.is_some());
        let tops: Vec<u32> = tops(&t, &v.sel).into_iter().filter(|&n| n != 0).collect();
        let (ok, tip) = if st.recycling {
            (false, "Moving to the Recycle Bin…".to_string())
        } else if st.running.is_some() {
            (false, "Not while a scan is under way".into())
        } else if !st.volume.fixed && st.volume.remote {
            (false, "A network drive has no Recycle Bin: VibeSpace never deletes for good".into())
        } else if !st.volume.fixed && !st.volume.root.is_empty() {
            (false, "This drive has no Recycle Bin: VibeSpace never deletes for good".into())
        } else if tops.is_empty() && v.sel.contains(&0) {
            (false, "The scanned folder itself stays: scan its parent to remove it".into())
        } else if tops.is_empty() {
            (false, "Select something first".into())
        } else {
            (true, "Move to the Recycle Bin (Delete)".into())
        };
        ui.set_can_recycle(ok);
        ui.set_recycle_tip(tip.into());
    }

    // ---------- selection ----------

    /// Select one node: the tree opens to it and scrolls there, the map follows (a folder is
    /// drawn, a file is shown in its folder) unless the click came from the map.
    pub(super) fn select(self: &std::rc::Rc<Self>, n: u32, from_map: bool) {
        {
            let mut v = self.view.borrow_mut();
            let Some(tree) = v.tree.clone() else { return };
            let t = tree.read().unwrap();
            if n as usize >= t.nodes.len() || t.node(n).has(tree::GONE) {
                return;
            }
            let mut p = t.node(n).parent;
            while p != NONE {
                v.expanded.insert(p);
                p = t.node(p).parent;
            }
            v.sel.clear();
            v.sel.insert(n);
            v.cursor = Some(n);
        }
        self.st.borrow_mut().anchor = Some(n);
        self.refresh_rows();
        self.scroll_to_cursor();
        if from_map {
            self.push_select_mark();
        } else {
            self.follow_cursor();
        }
        self.file_model.reset();
    }

    /// After things went (recycled, rescanned away): the selection keeps what is still there,
    /// the cursor goes to the nearest live ancestor.
    pub(super) fn fix_selection(self: &std::rc::Rc<Self>) {
        let mut v = self.view.borrow_mut();
        let Some(tree) = v.tree.clone() else { return };
        let t = tree.read().unwrap();
        v.sel.retain(|&n| !t.node(n).has(tree::GONE));
        if let Some(mut c) = v.cursor {
            while c != NONE && t.node(c).has(tree::GONE) {
                c = t.node(c).parent;
            }
            let c = if c == NONE { 0 } else { c };
            v.cursor = Some(c);
            if v.sel.is_empty() {
                v.sel.insert(c);
            }
        }
        v.largest.retain(|&n| !t.node(n).has(tree::GONE));
        drop(t);
        drop(v);
        let mut st = self.st.borrow_mut();
        let root = st.map.root;
        let t = tree.read().unwrap();
        let mut r = root;
        while r != NONE && t.node(r).has(tree::GONE) {
            r = t.node(r).parent;
        }
        st.map.root = if r == NONE { 0 } else { r };
    }

    fn select_rows(self: &std::rc::Rc<Self>, list: &[u32], i: usize, ctrl: bool, shift: bool) {
        let Some(&n) = list.get(i) else { return };
        {
            let mut v = self.view.borrow_mut();
            let anchor = self.st.borrow().anchor;
            if shift && let Some(a) = anchor.and_then(|a| list.iter().position(|&x| x == a)) {
                let (lo, hi) = (a.min(i), a.max(i));
                if !ctrl {
                    v.sel.clear();
                }
                for &x in &list[lo..=hi] {
                    v.sel.insert(x);
                }
            } else if ctrl {
                if !v.sel.remove(&n) {
                    v.sel.insert(n);
                }
                self.st.borrow_mut().anchor = Some(n);
            } else {
                v.sel.clear();
                v.sel.insert(n);
                self.st.borrow_mut().anchor = Some(n);
            }
            v.cursor = Some(n);
        }
        self.tree_model.reset();
        self.file_model.reset();
        self.push_cursor();
        self.follow_cursor();
    }

    pub(super) fn row_pressed(self: &std::rc::Rc<Self>, i: usize, ctrl: bool, shift: bool) {
        let list: Vec<u32> = self.view.borrow().rows.iter().map(|r| r.0).collect();
        self.select_rows(&list, i, ctrl, shift);
    }

    pub(super) fn file_pressed(self: &std::rc::Rc<Self>, i: usize, ctrl: bool, shift: bool) {
        let list = self.view.borrow().largest.clone();
        self.select_rows(&list, i, ctrl, shift);
    }

    pub(super) fn row_toggle(self: &std::rc::Rc<Self>, i: usize) {
        let n = self.view.borrow().rows.get(i).map(|r| r.0);
        if let Some(n) = n {
            self.toggle(n, None);
        }
    }

    /// Open or close a folder in the tree (`to`: Some(true) opens, Some(false) closes).
    fn toggle(self: &std::rc::Rc<Self>, n: u32, to: Option<bool>) {
        {
            let mut v = self.view.borrow_mut();
            let open = to.unwrap_or(!v.expanded.contains(&n));
            if open {
                v.expanded.insert(n);
            } else {
                v.expanded.remove(&n);
                // A cursor inside what closes comes up to it.
                let inside = match (v.cursor, v.tree.as_ref()) {
                    (Some(c), Some(t)) => c != n && t.read().unwrap().is_within(c, n),
                    _ => false,
                };
                if inside {
                    v.cursor = Some(n);
                    v.sel.clear();
                    v.sel.insert(n);
                    self.st.borrow_mut().anchor = Some(n);
                }
            }
        }
        self.refresh_rows();
        self.follow_cursor();
    }

    pub(super) fn row_double(self: &std::rc::Rc<Self>, i: usize) {
        let n = self.view.borrow().rows.get(i).map(|r| r.0);
        let Some(n) = n else { return };
        // The row is the one acted on (a screen reader's default action has no press first).
        if self.view.borrow().cursor != Some(n) {
            self.row_pressed(i, false, false);
        }
        let is_dir = self.with_tree(|t| t.node(n).is_dir()).unwrap_or(false);
        if is_dir {
            self.toggle(n, None);
        } else {
            self.act(Action::Open);
        }
    }

    pub(super) fn file_double(self: &std::rc::Rc<Self>, i: usize) {
        let n = self.view.borrow().largest.get(i).copied();
        let Some(n) = n else { return };
        if self.view.borrow().cursor != Some(n) {
            self.file_pressed(i, false, false);
        }
        self.act(Action::Open);
    }

    pub(super) fn with_tree<R>(&self, f: impl FnOnce(&Tree) -> R) -> Option<R> {
        let tree = self.view.borrow().tree.clone()?;
        let t = tree.read().unwrap();
        Some(f(&t))
    }

    /// Move the cursor to row `i` of `list` (with Shift: extend the selection).
    fn move_to(self: &std::rc::Rc<Self>, list: &[u32], i: usize, shift: bool) {
        if list.is_empty() {
            return;
        }
        let i = i.min(list.len() - 1);
        self.select_rows(list, i, false, shift);
        self.scroll_to_cursor();
    }

    pub(super) fn list_key(self: &std::rc::Rc<Self>, t: &str, ctrl: bool, shift: bool, page: usize) -> bool {
        let list: Vec<u32> = self.view.borrow().rows.iter().map(|r| r.0).collect();
        let cur = self.view.borrow().cursor.and_then(|c| list.iter().position(|&x| x == c));
        if self.move_keys(&list, cur, t, shift, page) {
            return true;
        }
        let Some(i) = cur else { return false };
        let n = list[i];
        let (is_dir, open, has_kids, parent) = self.with_tree(|tr| (tr.node(n).is_dir() && !tr.node(n).has(tree::LINK), self.view.borrow().expanded.contains(&n), tr.has_children(n), tr.node(n).parent)).unwrap_or((false, false, false, NONE));
        if key_is(t, Key::RightArrow) {
            if is_dir && has_kids && !open {
                self.toggle(n, Some(true));
            } else if is_dir && open && i + 1 < list.len() {
                let list: Vec<u32> = self.view.borrow().rows.iter().map(|r| r.0).collect();
                self.move_to(&list, i + 1, false);
            }
            return true;
        }
        if key_is(t, Key::LeftArrow) {
            if is_dir && open {
                self.toggle(n, Some(false));
            } else if parent != NONE {
                let list: Vec<u32> = self.view.borrow().rows.iter().map(|r| r.0).collect();
                if let Some(p) = list.iter().position(|&x| x == parent) {
                    self.move_to(&list, p, false);
                }
            }
            return true;
        }
        if key_is(t, Key::Return) {
            if is_dir {
                self.toggle(n, None);
            } else {
                self.act(Action::Open);
            }
            return true;
        }
        let _ = ctrl;
        false
    }

    pub(super) fn file_key(self: &std::rc::Rc<Self>, t: &str, _ctrl: bool, shift: bool, page: usize) -> bool {
        let list = self.view.borrow().largest.clone();
        let cur = self.view.borrow().cursor.and_then(|c| list.iter().position(|&x| x == c));
        if self.move_keys(&list, cur, t, shift, page) {
            return true;
        }
        if key_is(t, Key::Return) && cur.is_some() {
            self.act(Action::Open);
            return true;
        }
        false
    }

    /// Up, Down, Page Up, Page Down, Home, End over a list.
    fn move_keys(self: &std::rc::Rc<Self>, list: &[u32], cur: Option<usize>, t: &str, shift: bool, page: usize) -> bool {
        if list.is_empty() {
            return false;
        }
        let last = list.len() - 1;
        let to = if key_is(t, Key::DownArrow) {
            Some(cur.map_or(0, |i| (i + 1).min(last)))
        } else if key_is(t, Key::UpArrow) {
            Some(cur.map_or(0, |i| i.saturating_sub(1)))
        } else if key_is(t, Key::PageDown) {
            Some(cur.map_or(0, |i| (i + page).min(last)))
        } else if key_is(t, Key::PageUp) {
            Some(cur.map_or(0, |i| i.saturating_sub(page)))
        } else if key_is(t, Key::Home) {
            Some(0)
        } else if key_is(t, Key::End) {
            Some(last)
        } else {
            None
        };
        match to {
            Some(i) => {
                self.move_to(list, i, shift);
                true
            }
            None => false,
        }
    }

    // ---------- types ----------

    pub(super) fn type_clicked(self: &std::rc::Rc<Self>, i: usize) {
        let ext = self.st.borrow().types.get(i).map(|s| s.ext);
        let Some(ext) = ext else { return };
        let now = self.st.borrow().type_filter;
        self.set_type_filter(if now == Some(ext) { None } else { Some(ext) });
    }

    pub(super) fn set_type_filter(self: &std::rc::Rc<Self>, f: Option<u32>) {
        self.st.borrow_mut().type_filter = f;
        self.push_types();
        self.repaint_map();
    }
}

pub fn rgb(c: [u8; 3]) -> Color {
    Color::from_rgb_u8(c[0], c[1], c[2])
}
