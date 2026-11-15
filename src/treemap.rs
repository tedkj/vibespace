//! The map: a squarified treemap (Bruls, Huizing and van Wijk, 2000) of a folder, drawn by
//! Rust into one pixel buffer, so thousands of tiles cost the window one image.
//!
//! `squarify` is the pure layout: values fill a rectangle exactly, each tile's area in
//! proportion, rows laid along the shorter side so tiles stay close to square. Tiles share edge
//! coordinates (the same f64 for both neighbours), so snapping edges to whole pixels keeps them
//! adjacent: no overlap, no seam. `layout` nests folders two more levels deep inside a 1 px frame
//! of their own colour; tiles too small to see are gathered into one "smaller items" tile.
//! `paint` fills the buffer, leaving a 1 px gap right and below each tile.

use crate::tree::{Metric, Tree};

/// A rectangle by its edges.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn w(&self) -> f64 {
        self.x1 - self.x0
    }
    pub fn h(&self) -> f64 {
        self.y1 - self.y0
    }
    pub fn area(&self) -> f64 {
        self.w().max(0.0) * self.h().max(0.0)
    }
}

/// The squarified layout of `values` (largest first; zero and negative values get empty
/// tiles) in `r`.
pub fn squarify(values: &[f64], r: Rect) -> Vec<Rect> {
    let n = values.len();
    let mut out = vec![Rect { x0: r.x0, y0: r.y0, x1: r.x0, y1: r.y0 }; n];
    let total: f64 = values.iter().map(|v| v.max(0.0)).sum();
    if total <= 0.0 || r.w() <= 0.0 || r.h() <= 0.0 {
        return out;
    }
    let scale = r.area() / total;
    let areas: Vec<f64> = values.iter().map(|v| v.max(0.0) * scale).collect();
    // The last positive value: the row that holds it takes all that is left.
    let last = areas.iter().rposition(|&a| a > 0.0).unwrap_or(0);
    let mut rest = r;
    let mut i = 0;
    while i <= last {
        if areas[i] <= 0.0 {
            out[i] = Rect { x0: rest.x0, y0: rest.y0, x1: rest.x0, y1: rest.y0 };
            i += 1;
            continue;
        }
        let short = rest.w().min(rest.h());
        let worst = |sum: f64, lo: f64, hi: f64| {
            let s2 = sum * sum;
            let w2 = short * short;
            (w2 * hi / s2).max(s2 / (w2 * lo))
        };
        let (mut j, mut sum, mut lo, mut hi) = (i + 1, areas[i], areas[i], areas[i]);
        let mut cur = worst(sum, lo, hi);
        while j <= last {
            let a = areas[j];
            if a <= 0.0 {
                break;
            }
            let next = worst(sum + a, lo.min(a), hi.max(a));
            if next > cur {
                break;
            }
            sum += a;
            lo = lo.min(a);
            hi = hi.max(a);
            cur = next;
            j += 1;
        }
        let final_row = j > last;
        if rest.w() >= rest.h() {
            // A column on the left, as wide as its area needs (the last one: all that is left).
            let x1 = if final_row { rest.x1 } else { (rest.x0 + sum / rest.h()).min(rest.x1) };
            let mut acc = 0.0;
            let mut y = rest.y0;
            for k in i..j {
                acc += areas[k];
                let y1 = if k + 1 == j { rest.y1 } else { rest.y0 + rest.h() * (acc / sum) };
                out[k] = Rect { x0: rest.x0, y0: y, x1, y1 };
                y = y1;
            }
            rest.x0 = x1;
        } else {
            let y1 = if final_row { rest.y1 } else { (rest.y0 + sum / rest.w()).min(rest.y1) };
            let mut acc = 0.0;
            let mut x = rest.x0;
            for k in i..j {
                acc += areas[k];
                let x1 = if k + 1 == j { rest.x1 } else { rest.x0 + rest.w() * (acc / sum) };
                out[k] = Rect { x0: x, y0: rest.y0, x1, y1 };
                x = x1;
            }
            rest.y0 = y1;
        }
        i = j;
    }
    out
}

/// A tile of the drawn map, in whole pixels: it covers `[x0, x1) x [y0, y1)` (its last column
/// and row are the gap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    /// The node, or for a "smaller items" tile the folder they are in.
    pub node: u32,
    pub depth: u8,
    pub kind: Kind,
    /// A file's extension id; for `Rest`, how many items it gathers.
    pub ext: u32,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Folder,
    File,
    /// Items too small to draw, gathered.
    Rest,
}

/// A child as the map reads it: (node, value, is a folder, ext id).
pub type Kid = (u32, u64, bool, u32);

/// What the map reads of the tree (a trait, so the layout is tested on its own).
pub trait Source {
    /// Children with a value above zero, largest first: (node, value, is a folder, ext id).
    fn kids(&self, node: u32) -> Vec<Kid>;

    /// The folder's total, when known without its children (the tree keeps it).
    fn total(&self, _node: u32) -> Option<u64> {
        None
    }

    /// The children big enough to draw (`big(value)`), largest first, and the sum and count of
    /// the others. A source that can tell sizes cheaply sorts only the big ones.
    fn split(&self, node: u32, big: &dyn Fn(u64) -> bool) -> (Vec<Kid>, u64, usize) {
        let mut kids = self.kids(node);
        let keep = kids.iter().position(|k| !big(k.1)).unwrap_or(kids.len());
        let rest: Vec<_> = kids.split_off(keep);
        (kids, rest.iter().map(|k| k.1).sum(), rest.len())
    }
}

pub struct TreeSource<'a> {
    pub tree: &'a Tree,
    pub metric: Metric,
}

impl Source for TreeSource<'_> {
    fn kids(&self, node: u32) -> Vec<Kid> {
        let t = self.tree;
        t.sorted_children(node, self.metric)
            .into_iter()
            .map(|c| {
                let n = t.node(c);
                (c, n.value(self.metric), n.is_dir(), if n.is_dir() { 0 } else { n.aux })
            })
            .filter(|k| k.1 > 0)
            .collect()
    }

    fn total(&self, node: u32) -> Option<u64> {
        Some(self.tree.node(node).value(self.metric))
    }

    /// A folder of 100 000 tiny files is summed, not sorted.
    fn split(&self, node: u32, big: &dyn Fn(u64) -> bool) -> (Vec<Kid>, u64, usize) {
        let t = self.tree;
        let (mut keep, mut rest, mut n) = (Vec::new(), 0u64, 0usize);
        for c in t.children(node) {
            let x = t.node(c);
            let v = x.value(self.metric);
            if v == 0 {
                continue;
            }
            if big(v) {
                keep.push(c);
            } else {
                rest += v;
                n += 1;
            }
        }
        keep.sort_unstable_by(|&a, &b| t.compare(a, b, self.metric));
        let kids = keep.into_iter().map(|c| {
            let x = t.node(c);
            (c, x.value(self.metric), x.is_dir(), if x.is_dir() { 0 } else { x.aux })
        });
        (kids.collect(), rest, n)
    }
}

/// Tiles smaller than this (px²) are gathered into one.
pub const MIN_AREA: f64 = 6.0;
/// At most this many tiles per folder (the rest gathered).
pub const MAX_KIDS: usize = 4000;
/// Levels drawn under the map's folder at most. In practice the 8 px rule below ends the
/// nesting first: a folder is opened as long as its tile is legible, so a big file five levels
/// down still shows its colour (three levels left real trees as grey boxes).
pub const DEPTH: u8 = 32;

/// Lay out `root` in a `w` x `h` pixel map.
pub fn layout(src: &dyn Source, root: u32, w: i32, h: i32) -> Vec<Tile> {
    let mut out = Vec::new();
    if w > 0 && h > 0 {
        place(src, root, Rect { x0: 0.0, y0: 0.0, x1: w as f64, y1: h as f64 }, 0, &mut out);
    }
    out
}

fn place(src: &dyn Source, folder: u32, r: Rect, depth: u8, out: &mut Vec<Tile>) {
    let total = match src.total(folder) {
        Some(t) => t as f64,
        None => src.kids(folder).iter().map(|k| k.1 as f64).sum(),
    };
    if total <= 0.0 || r.area() <= 0.0 {
        return;
    }
    let px = r.area() / total;
    let (mut kids, mut rest, mut rest_n) = src.split(folder, &|v| v as f64 * px >= MIN_AREA);
    if kids.len() > MAX_KIDS {
        for k in kids.drain(MAX_KIDS..) {
            rest += k.1;
            rest_n += 1;
        }
    }
    let keep = kids.len();
    let mut values: Vec<f64> = kids.iter().map(|k| k.1 as f64).collect();
    if rest_n > 0 {
        values.push(rest as f64);
    }
    let rects = squarify(&values, r);
    for (i, rect) in rects.iter().enumerate() {
        let (x0, y0, x1, y1) = (rect.x0.round() as i32, rect.y0.round() as i32, rect.x1.round() as i32, rect.y1.round() as i32);
        // Room for at least one pixel besides the gap.
        if x1 - x0 < 2 || y1 - y0 < 2 {
            continue;
        }
        if i == keep {
            out.push(Tile { x0, y0, x1, y1, node: folder, depth, kind: Kind::Rest, ext: rest_n as u32, value: rest });
            continue;
        }
        let (node, value, is_dir, ext) = kids[i];
        out.push(Tile { x0, y0, x1, y1, node, depth, kind: if is_dir { Kind::Folder } else { Kind::File }, ext, value });
        // Inside its 1 px frame (and its own gap), a folder shows what it holds.
        if is_dir && depth + 1 < DEPTH && x1 - x0 >= 8 && y1 - y0 >= 8 {
            let inner = Rect { x0: (x0 + 1) as f64, y0: (y0 + 1) as f64, x1: (x1 - 1) as f64, y1: (y1 - 1) as f64 };
            place(src, node, inner, depth + 1, out);
        }
    }
}

/// The deepest tile under a pixel.
pub fn hit(tiles: &[Tile], x: i32, y: i32) -> Option<usize> {
    tiles.iter().rposition(|t| x >= t.x0 && x < t.x1 && y >= t.y0 && y < t.y1)
}

// ---------- colour ----------

pub type Rgb = [u8; 3];

/// The map's colours for one theme: graphite/paper tones by kind, a restrained set of muted
/// hues for the top extensions (data colours, desaturated: the accent is for selection only).
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub bg: Rgb,
    /// Folders by depth (frames and the folder's own fill).
    pub folder: [Rgb; 3],
    pub file: Rgb,
    pub rest: Rgb,
    pub hues: [Rgb; 8],
    /// Files outside a type filter.
    pub dim: Rgb,
    /// Files of a filtered type without a hue of their own.
    pub lit: Rgb,
}

pub const HUE_NAMES: [&str; 8] = ["ochre", "sage", "teal", "clay", "plum", "olive", "rose", "sand"];

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: [0x16, 0x19, 0x1d],
            folder: [[0x2b, 0x30, 0x37], [0x31, 0x36, 0x3e], [0x37, 0x3c, 0x45]],
            file: [0x4a, 0x50, 0x59],
            rest: [0x3c, 0x41, 0x49],
            hues: [
                [0x93, 0x80, 0x4f], // ochre
                [0x5f, 0x80, 0x62], // sage
                [0x4f, 0x7c, 0x7e], // teal
                [0x94, 0x64, 0x55], // clay
                [0x7a, 0x64, 0x86], // plum
                [0x76, 0x7a, 0x4b], // olive
                [0x8d, 0x5f, 0x6e], // rose
                [0x86, 0x78, 0x66], // sand
            ],
            dim: [0x26, 0x2a, 0x30],
            lit: [0xae, 0xb2, 0xb9],
        }
    } else {
        Palette {
            bg: [0xf6, 0xf4, 0xef],
            folder: [[0xbf, 0xb8, 0xaa], [0xc8, 0xc2, 0xb5], [0xd1, 0xcb, 0xc0]],
            file: [0xe0, 0xdb, 0xd0],
            rest: [0xd5, 0xd0, 0xc5],
            hues: [
                [0xd9, 0xc4, 0x8c], // ochre
                [0xa9, 0xc6, 0xa9], // sage
                [0x9c, 0xc4, 0xc4], // teal
                [0xde, 0xab, 0x99], // clay
                [0xc2, 0xad, 0xcf], // plum
                [0xc3, 0xc5, 0x92], // olive
                [0xd9, 0xa9, 0xb7], // rose
                [0xcf, 0xc0, 0xa8], // sand
            ],
            dim: [0xef, 0xec, 0xe5],
            lit: [0x48, 0x4d, 0x55],
        }
    }
}

/// Fill a `w` x `h` RGB buffer. `hue_of(ext)` gives an extension's hue slot (the top ones);
/// `only` keeps one extension lit and dims every other file.
pub fn paint(tiles: &[Tile], w: i32, h: i32, pal: &Palette, hue_of: &dyn Fn(u32) -> Option<usize>, only: Option<u32>) -> Vec<u8> {
    let (w, h) = (w.max(0) as usize, h.max(0) as usize);
    let mut px = vec![0u8; w * h * 3];
    for c in px.as_chunks_mut::<3>().0 {
        c.copy_from_slice(&pal.bg);
    }
    for t in tiles {
        let colour = match t.kind {
            Kind::Folder => pal.folder[(t.depth as usize).min(2)],
            Kind::Rest => {
                if only.is_some() {
                    pal.dim
                } else {
                    pal.rest
                }
            }
            Kind::File => {
                let hue = hue_of(t.ext).map(|i| pal.hues[i % pal.hues.len()]);
                match only {
                    Some(e) if e != t.ext => pal.dim,
                    Some(_) => hue.unwrap_or(pal.lit),
                    None => hue.unwrap_or(pal.file),
                }
            }
        };
        let (x0, x1) = (t.x0.max(0) as usize, ((t.x1 - 1).max(0) as usize).min(w));
        let (y0, y1) = (t.y0.max(0) as usize, ((t.y1 - 1).max(0) as usize).min(h));
        if x0 >= x1 {
            continue;
        }
        for y in y0..y1 {
            let row = &mut px[(y * w + x0) * 3..(y * w + x1) * 3];
            for c in row.as_chunks_mut::<3>().0 {
                c.copy_from_slice(&colour);
            }
        }
    }
    px
}
