//! What the scan holds by kind: per extension the total, the count, the share and the largest
//! file; and the largest files of the whole scan.

use crate::tree::{Metric, NONE, Tree};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeStat {
    /// The extension's id (`Tree::exts`); 0 is "no extension".
    pub ext: u32,
    pub size: u64,
    pub alloc: u64,
    /// Files (every name, hard links too).
    pub count: u64,
    /// Its largest file by the metric the stats were made with.
    pub largest: u32,
}

impl TypeStat {
    pub fn value(&self, m: Metric) -> u64 {
        match m {
            Metric::Size => self.size,
            Metric::OnDisk => self.alloc,
        }
    }
}

/// Every extension in the scan, largest total first (ties: by name, then id: a total order).
pub fn by_type(t: &Tree, m: Metric) -> Vec<TypeStat> {
    let mut map: HashMap<u32, TypeStat> = HashMap::new();
    for (i, n) in t.live() {
        if n.is_dir() {
            continue;
        }
        let s = map.entry(n.aux).or_insert(TypeStat { ext: n.aux, size: 0, alloc: 0, count: 0, largest: NONE });
        s.size += n.size;
        s.alloc += n.alloc;
        s.count += 1;
        if s.largest == NONE || t.compare(i, s.largest, m) == std::cmp::Ordering::Less {
            s.largest = i;
        }
    }
    let mut v: Vec<TypeStat> = map.into_values().collect();
    v.sort_unstable_by(|a, b| b.value(m).cmp(&a.value(m)).then_with(|| t.exts[a.ext as usize].cmp(&t.exts[b.ext as usize])).then(a.ext.cmp(&b.ext)));
    v
}

/// The extensions that get a hue of their own on the map: the largest `n`, "no extension" left
/// out (it is not one kind of file).
pub fn hue_slots(stats: &[TypeStat], m: Metric, n: usize) -> HashMap<u32, usize> {
    stats.iter().filter(|s| s.ext != 0 && s.value(m) > 0).take(n).enumerate().map(|(i, s)| (s.ext, i)).collect()
}

/// The `n` largest files of the scan, largest first (the tree's total order).
pub fn largest(t: &Tree, n: usize, m: Metric) -> Vec<u32> {
    let mut v: Vec<u32> = t.live().filter(|(_, x)| !x.is_dir()).map(|(i, _)| i).collect();
    let cmp = |a: &u32, b: &u32| t.compare(*a, *b, m);
    if v.len() > n && n > 0 {
        v.select_nth_unstable_by(n - 1, cmp);
        v.truncate(n);
    } else if n == 0 {
        v.clear();
    }
    v.sort_unstable_by(cmp);
    v
}
