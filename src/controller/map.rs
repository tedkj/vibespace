//! The map in the window: which folder it draws, the image at the window's own pixel size,
//! hover and selection outlines, the legend, and clicks back into the tree.

use super::App;
use super::view::rgb;
use crate::treemap::{self, Kind, Tile, TreeSource};
use crate::tree::{self, Metric, NONE};
use crate::{LegendItem, Mark, format};
use slint::{ComponentHandle, Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, VecModel};
use std::rc::Rc;

#[derive(Clone, Debug)]
pub struct MapState {
    /// The folder drawn.
    pub root: u32,
    /// The image in physical pixels; the area in logical ones; their ratio.
    pub px: (i32, i32),
    pub logical: (f32, f32),
    pub scale: f32,
    pub tiles: Vec<Tile>,
    pub hover: Option<usize>,
}

impl Default for MapState {
    fn default() -> Self {
        MapState { root: 0, px: (0, 0), logical: (0.0, 0.0), scale: 1.0, tiles: Vec::new(), hover: None }
    }
}

/// A mono 12 px character's advance (Cascadia Mono): the legend keeps what fits.
const MONO_ADVANCE: f32 = 7.3;

fn mark(t: &Tile, scale: f32) -> Mark {
    // Inside the tile, without its gap.
    Mark {
        x: t.x0 as f32 / scale,
        y: t.y0 as f32 / scale,
        w: ((t.x1 - t.x0 - 1).max(1)) as f32 / scale,
        h: ((t.y1 - t.y0 - 1).max(1)) as f32 / scale,
        shown: true,
    }
}

fn hidden() -> Mark {
    Mark { x: 0.0, y: 0.0, w: 0.0, h: 0.0, shown: false }
}

impl App {
    pub(super) fn map_resized(self: &Rc<Self>, w: f32, h: f32) {
        let scale = self.ui().map(|u| u.window().scale_factor()).unwrap_or(1.0);
        let px = ((w * scale).floor() as i32, (h * scale).floor() as i32);
        let changed = {
            let mut st = self.st.borrow_mut();
            let c = st.map.px != px || st.map.scale != scale;
            st.map.px = px;
            st.map.logical = (w, h);
            st.map.scale = scale;
            c
        };
        if changed {
            self.repaint_map();
            // The window's width moved: the paths fitted to it are fitted again.
            self.push_facts();
            self.push_status();
            self.fit_root_label();
        }
    }

    /// Lay out and draw the map again (the data, the size or the colours changed).
    pub(super) fn repaint_map(&self) {
        let Some(ui) = self.ui() else { return };
        let dark = ui.global::<crate::Theme>().get_dark();
        let (root, (w, h), scale, filter, hues) = {
            let st = self.st.borrow();
            (st.map.root, st.map.px, st.map.scale, st.type_filter, st.hues.clone())
        };
        let v = self.view.borrow();
        let Some(tree) = v.tree.clone() else {
            drop(v);
            self.st.borrow_mut().map.tiles.clear();
            ui.set_map_image(Image::default());
            return;
        };
        let metric = v.metric;
        drop(v);
        let t = tree.read().unwrap();
        let root = if (root as usize) < t.nodes.len() && !t.node(root).has(tree::GONE) { root } else { 0 };
        let tiles = if w > 0 && h > 0 { treemap::layout(&TreeSource { tree: &t, metric }, root, w, h) } else { Vec::new() };
        let pal = treemap::palette(dark);
        let px = treemap::paint(&tiles, w, h, &pal, &|e| hues.get(&e).copied(), filter);
        if w > 0 && h > 0 {
            let mut buf = SharedPixelBuffer::<Rgb8Pixel>::new(w as u32, h as u32);
            buf.make_mut_bytes().copy_from_slice(&px);
            ui.set_map_image(Image::from_rgb8(buf));
            ui.set_map_image_w(w as f32 / scale);
            ui.set_map_image_h(h as f32 / scale);
        }
        let total = t.node(root).value(metric);
        let total_text = format!("{} {}", format::bytes(total), if metric == Metric::OnDisk { "on disk" } else { "in size" });
        let room = self.st.borrow().map.logical.0 - 6.0 - 28.0 - 8.0 - 8.0 - 12.0 - total_text.chars().count() as f32 * MONO_ADVANCE;
        ui.set_map_title(format::middle(&t.display_path(root), (room / MONO_ADVANCE).max(0.0) as usize).into());
        ui.set_map_total(total_text.into());
        ui.set_map_can_up(root != 0);
        let scanning = self.st.borrow().running.is_some();
        let empty = if !tiles.is_empty() {
            String::new()
        } else if scanning && (t.node(root).has(tree::PENDING) || total == 0) {
            "Reading…".into()
        } else if t.node(root).has(tree::PENDING) {
            // Nothing is reading it: a cancel left it (SPA-2's honest states, here too).
            "Not read: the scan was cancelled".into()
        } else if total == 0 {
            if t.node(root).is_dir() { "Nothing here takes any room".into() } else { "This file takes no room".into() }
        } else {
            String::new()
        };
        ui.set_map_empty_text(empty.into());
        // The legend: folders, other files, then the hues, as many as fit.
        let mut items = vec![LegendItem { name: "folders".into(), swatch: rgb(pal.folder[0]) }, LegendItem { name: "other files".into(), swatch: rgb(pal.file) }];
        let mut slots: Vec<(usize, u32)> = hues.iter().map(|(&e, &i)| (i, e)).collect();
        slots.sort();
        let room = self.st.borrow().map.logical.0 - 18.0;
        let width = |s: &str| 10.0 + 5.0 + MONO_ADVANCE * s.chars().count() as f32 + 12.0;
        let mut used: f32 = items.iter().map(|i| width(&i.name)).sum();
        for (i, e) in slots {
            let name = format!(".{}", t.exts[e as usize]);
            let w = width(&name);
            if used + w > room {
                break;
            }
            used += w;
            items.push(LegendItem { name: name.into(), swatch: rgb(pal.hues[i]) });
        }
        ui.set_legend(ModelRc::new(VecModel::from(items)));
        let filter_text = match filter {
            Some(e) => {
                let s = self.st.borrow().types.iter().find(|s| s.ext == e).copied();
                let name = if e == 0 { "files without an extension".to_string() } else { format!(".{} files", t.exts[e as usize]) };
                match s {
                    Some(s) => format!("Showing {name}: {} in {}", format::bytes(s.value(metric)), format::plural(s.count, "file", "files")),
                    None => format!("Showing {name}"),
                }
            }
            None => String::new(),
        };
        ui.set_map_filter_text(filter_text.into());
        drop(t);
        {
            let mut st = self.st.borrow_mut();
            st.map.root = root;
            st.map.tiles = tiles;
            st.map.hover = None;
        }
        ui.set_map_hover_mark(hidden());
        ui.set_map_hover_text(Default::default());
        ui.set_map_hover_size(Default::default());
        self.push_select_mark();
    }

    /// The selected tile's outline (when the cursor's node is drawn).
    pub(super) fn push_select_mark(&self) {
        let Some(ui) = self.ui() else { return };
        let cursor = self.view.borrow().cursor;
        let st = self.st.borrow();
        let m = match cursor {
            Some(c) if c != st.map.root => st.map.tiles.iter().find(|t| t.node == c && t.kind != Kind::Rest).map(|t| mark(t, st.map.scale)).unwrap_or_else(hidden),
            _ => hidden(),
        };
        ui.set_map_select_mark(m);
    }

    /// The map follows the cursor: a folder is drawn itself, a file inside its folder.
    pub(super) fn follow_cursor(&self) {
        let Some(c) = self.view.borrow().cursor else { return };
        let root = self.with_tree(|t| if t.node(c).is_dir() && !t.node(c).has(tree::LINK) { c } else if t.node(c).parent == NONE { 0 } else { t.node(c).parent });
        let Some(root) = root else { return };
        let changed = {
            let mut st = self.st.borrow_mut();
            let c = st.map.root != root;
            st.map.root = root;
            c
        };
        if changed {
            self.repaint_map();
        } else {
            self.push_select_mark();
        }
    }

    fn tile_at(&self, x: f32, y: f32) -> Option<Tile> {
        let st = self.st.borrow();
        let (px, py) = ((x * st.map.scale).floor() as i32, (y * st.map.scale).floor() as i32);
        treemap::hit(&st.map.tiles, px, py).map(|i| st.map.tiles[i])
    }

    pub(super) fn map_hovered(self: &Rc<Self>, x: f32, y: f32) {
        let Some(ui) = self.ui() else { return };
        let (idx, scale) = {
            let st = self.st.borrow();
            let (px, py) = ((x * st.map.scale).floor() as i32, (y * st.map.scale).floor() as i32);
            (treemap::hit(&st.map.tiles, px, py), st.map.scale)
        };
        if idx == self.st.borrow().map.hover {
            return;
        }
        self.st.borrow_mut().map.hover = idx;
        let Some(i) = idx else {
            self.map_left();
            return;
        };
        let tile = self.st.borrow().map.tiles[i];
        let root = self.st.borrow().map.root;
        let metric = self.view.borrow().metric;
        let text = self.with_tree(|t| {
            let whole = t.node(root).value(metric);
            let share = format::pct(tile.value, whole);
            match tile.kind {
                Kind::Rest => (format!("{} smaller items in {}", format::count(tile.ext as u64), t.display_path(tile.node)), format!("{} · {share}", format::bytes(tile.value))),
                _ => (t.display_path(tile.node), format!("{} · {share}", format::bytes(tile.value))),
            }
        });
        if let Some((path, size)) = text {
            let room = self.st.borrow().map.logical.0 - 24.0 - 12.0 - size.chars().count() as f32 * MONO_ADVANCE;
            ui.set_map_hover_text(crate::format::middle(&path, (room / MONO_ADVANCE).max(0.0) as usize).into());
            ui.set_map_hover_size(size.into());
        }
        ui.set_map_hover_mark(mark(&tile, scale));
    }

    pub(super) fn map_left(&self) {
        let Some(ui) = self.ui() else { return };
        self.st.borrow_mut().map.hover = None;
        ui.set_map_hover_mark(hidden());
        ui.set_map_hover_text(Default::default());
        ui.set_map_hover_size(Default::default());
    }

    /// A click selects the tile in the tree; the map stays where it is.
    pub(super) fn map_clicked(self: &Rc<Self>, x: f32, y: f32) {
        let Some(t) = self.tile_at(x, y) else { return };
        self.select(t.node, true);
        if let Some(ui) = self.ui() {
            ui.invoke_focus_list();
        }
    }

    /// A double-click zooms in: a folder fills the map; a file, its folder.
    pub(super) fn map_double(self: &Rc<Self>, x: f32, y: f32) {
        let Some(t) = self.tile_at(x, y) else { return };
        let target = if t.kind == Kind::File { self.with_tree(|tr| tr.node(t.node).parent).unwrap_or(0) } else { t.node };
        self.select(target, false);
    }

    /// Backspace: the map's folder's parent (and the tree's cursor with it).
    pub(super) fn map_up(self: &Rc<Self>) {
        let root = self.st.borrow().map.root;
        if root == 0 {
            return;
        }
        let parent = self.with_tree(|t| t.node(root).parent).unwrap_or(NONE);
        if parent != NONE {
            self.select(parent, false);
        }
    }
}
