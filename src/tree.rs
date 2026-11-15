//! The scan's result: one compact arena. Every file and folder is a 48-byte `Node`; names live in
//! one pool (WTF-8, so names Windows holds but Rust cannot spell survive exactly); a folder's
//! children are contiguous in the arena (a directory is listed in one go), so a folder needs
//! only its first child and a count. Folders carry live totals: what is under them, hard links
//! counted once, cloud placeholders at 0 on disk, things moved to the Recycle Bin subtracted.
//!
//! One writer at a time (the scanner's builder, a rescan, a recycle), readers between its
//! batches (the window): the controller keeps it in an `RwLock`.

use std::collections::HashSet;
use std::path::PathBuf;

pub const NONE: u32 = u32::MAX;

/// The deepest a folder is listed below the scanned one (IN-VibeSpace-6). A real tree is far
/// shallower: 256 names make a path of over 500 characters even at one letter each.
pub const MAX_DEPTH: usize = 256;

pub const DIR: u16 = 1;
/// A symbolic link, junction or mount point: listed with its own size, never followed.
pub const LINK: u16 = 2;
/// Another name of a file already counted: it adds nothing.
pub const HARDLINK: u16 = 4;
/// Cloud placeholder (OneDrive "online only"): logical size, nothing on disk. A folder with it
/// is not listed (listing it would fetch the listing from the cloud).
pub const ONLINE: u16 = 8;
/// A folder that could not be opened: access denied.
pub const NO_ACCESS: u16 = 16;
/// A folder whose listing failed (or broke off) for another reason.
pub const UNREADABLE: u16 = 32;
/// A folder not listed yet (or never: the scan was cancelled).
pub const PENDING: u16 = 64;
pub const COMPRESSED: u16 = 128;
pub const SPARSE: u16 = 256;
/// Moved to the Recycle Bin, or replaced by a rescan: counts nothing, never shown.
pub const GONE: u16 = 512;
/// Kept in step with the cloud (a OneDrive placeholder, on this disk or online only): deleting
/// it deletes it from the cloud on every device too (FS4).
pub const CLOUD: u16 = 1024;

/// Which size the window sorts and draws by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Metric {
    /// Logical size (what the file holds).
    Size,
    /// Allocation (what it takes on the disk; what deleting it frees).
    #[default]
    OnDisk,
}

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Node {
    /// Logical bytes; a folder's is the live total of what is under it.
    pub size: u64,
    /// Bytes on disk; a folder's likewise.
    pub alloc: u64,
    /// The file id (0: unknown), folders' too: what a recycle checks the path still names.
    /// Only files' ids take part in the hard-link bookkeeping.
    pub id: u64,
    pub parent: u32,
    pub name_off: u32,
    /// Last write, unix seconds (0: unknown).
    pub modified: u32,
    /// A folder: its index in `dirs`. A file: its extension's index in `exts`.
    pub aux: u32,
    pub name_len: u16,
    pub flags: u16,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.flags & DIR != 0
    }
    pub fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }
    pub fn value(&self, m: Metric) -> u64 {
        match m {
            Metric::Size => self.size,
            Metric::OnDisk => self.alloc,
        }
    }
}

/// A folder's own part: its children and what is under it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Dir {
    pub node: u32,
    pub first: u32,
    pub count: u32,
    /// Files (every name, hard links too) and folders under it, at any depth.
    pub files: u64,
    pub folders: u64,
}

/// File ids seen in this scan: a second name of the same file adds nothing. On NTFS the id's
/// low 48 bits are the MFT record number, dense from 0: a bitset over them costs an eighth of a
/// byte per record. Other ids go in a set.
#[derive(Default)]
pub struct Seen {
    pub on: bool,
    pub ntfs: bool,
    bits: Vec<u64>,
    set: HashSet<u64>,
}

const BITSET_RECORDS: u64 = 1 << 28;

impl Seen {
    pub fn new(on: bool, ntfs: bool) -> Seen {
        Seen { on, ntfs, ..Seen::default() }
    }
    /// True when `id` is new (it counts); false for a name already seen. Unknown ids (0, all
    /// ones) always count.
    pub fn insert(&mut self, id: u64) -> bool {
        if !self.on || id == 0 || id == u64::MAX {
            return true;
        }
        let rec = id & 0x0000_FFFF_FFFF_FFFF;
        if self.ntfs && rec < BITSET_RECORDS {
            let (w, b) = ((rec / 64) as usize, rec % 64);
            if self.bits.len() <= w {
                let want = (w + 1).max(self.bits.len() + self.bits.len() / 4);
                self.bits.resize(want, 0);
            }
            let had = self.bits[w] & (1 << b) != 0;
            self.bits[w] |= 1 << b;
            !had
        } else {
            self.set.insert(id)
        }
    }
    pub fn remove(&mut self, id: u64) {
        if !self.on || id == 0 || id == u64::MAX {
            return;
        }
        let rec = id & 0x0000_FFFF_FFFF_FFFF;
        if self.ntfs && rec < BITSET_RECORDS {
            let (w, b) = ((rec / 64) as usize, rec % 64);
            if let Some(x) = self.bits.get_mut(w) {
                *x &= !(1 << b);
            }
        } else {
            self.set.remove(&id);
        }
    }
    pub fn bytes(&self) -> usize {
        self.bits.capacity() * 8 + self.set.capacity() * 9
    }
    /// Whether a name of `id` still counts in the scan (`insert` would say false). Unknown ids,
    /// and every id where ids are not trusted, are never "still there".
    pub fn contains(&self, id: u64) -> bool {
        if !self.on || id == 0 || id == u64::MAX {
            return false;
        }
        let rec = id & 0x0000_FFFF_FFFF_FFFF;
        if self.ntfs && rec < BITSET_RECORDS {
            self.bits.get((rec / 64) as usize).is_some_and(|x| x & (1 << (rec % 64)) != 0)
        } else {
            self.set.contains(&id)
        }
    }
}

/// One entry of a directory listing, as the scanner hands it over.
#[derive(Clone, Copy, Debug, Default)]
pub struct Entry {
    pub attrs: u32,
    pub tag: u32,
    pub size: u64,
    pub alloc: u64,
    /// FILETIME ticks.
    pub modified: i64,
    pub id: u64,
    /// FILETIME ticks of the last change to the file or its metadata (ChangeTime: no copying
    /// tool sets it back); 0 where the listing has none.
    pub changed: i64,
}

pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_SPARSE: u32 = 0x200;
pub const ATTR_REPARSE: u32 = 0x400;
pub const ATTR_COMPRESSED: u32 = 0x800;
pub const ATTR_OFFLINE: u32 = 0x1000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x40000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x400000;

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attrs & ATTR_DIRECTORY != 0
    }
    /// A name surrogate (symbolic link, junction, mount point): an alias for something else.
    /// Other reparse points (OneDrive's cloud folders, dedup, WCI) are the thing itself.
    pub fn is_link(&self) -> bool {
        self.attrs & ATTR_REPARSE != 0 && self.tag & 0x2000_0000 != 0
    }
    /// A cloud placeholder whose data (or, for a folder, whose listing) is not on this disk.
    pub fn online_only(&self) -> bool {
        self.attrs & (ATTR_RECALL_ON_DATA_ACCESS | ATTR_OFFLINE) != 0 || (self.is_dir() && self.attrs & ATTR_RECALL_ON_OPEN != 0)
    }
    /// A cloud-files placeholder (`IO_REPARSE_TAG_CLOUD`, `_1` to `_F`: OneDrive and the other
    /// sync apps on the same Windows API), its data on this disk or not.
    pub fn in_cloud(&self) -> bool {
        self.online_only() || (self.attrs & ATTR_REPARSE != 0 && self.tag & 0xFFFF_0FFF == 0x9000_001A)
    }
}

/// FILETIME ticks to unix seconds, 0 when unknown or out of range.
pub fn unix_secs(ft: i64) -> u32 {
    if ft <= 0 {
        return 0;
    }
    let s = ft / 10_000_000 - 11_644_473_600;
    if s <= 0 { 0 } else { s.min(u32::MAX as i64) as u32 }
}

// ---------- names: WTF-8 ----------

/// UTF-16 (possibly ill-formed) to WTF-8: UTF-8, with a lone surrogate as its 3-byte form.
pub fn wtf8_push(out: &mut Vec<u8>, units: &[u16]) {
    let mut i = 0;
    while i < units.len() {
        let u = units[i] as u32;
        let c = if (0xD800..0xDC00).contains(&u) && i + 1 < units.len() && (0xDC00..0xE000).contains(&(units[i + 1] as u32)) {
            i += 1;
            0x10000 + ((u - 0xD800) << 10) + (units[i] as u32 - 0xDC00)
        } else {
            u
        };
        i += 1;
        match c {
            0..=0x7F => out.push(c as u8),
            0x80..=0x7FF => out.extend_from_slice(&[0xC0 | (c >> 6) as u8, 0x80 | (c & 0x3F) as u8]),
            0x800..=0xFFFF => out.extend_from_slice(&[0xE0 | (c >> 12) as u8, 0x80 | ((c >> 6) & 0x3F) as u8, 0x80 | (c & 0x3F) as u8]),
            _ => out.extend_from_slice(&[0xF0 | (c >> 18) as u8, 0x80 | ((c >> 12) & 0x3F) as u8, 0x80 | ((c >> 6) & 0x3F) as u8, 0x80 | (c & 0x3F) as u8]),
        }
    }
}

/// WTF-8 back to UTF-16, exactly as it was.
pub fn wtf8_units(b: &[u8], out: &mut Vec<u16>) {
    let mut i = 0;
    while i < b.len() {
        let x = b[i] as u32;
        let (c, n) = if x < 0x80 {
            (x, 1)
        } else if x < 0xE0 {
            (((x & 0x1F) << 6) | (b.get(i + 1).copied().unwrap_or(0) as u32 & 0x3F), 2)
        } else if x < 0xF0 {
            (((x & 0x0F) << 12) | ((b.get(i + 1).copied().unwrap_or(0) as u32 & 0x3F) << 6) | (b.get(i + 2).copied().unwrap_or(0) as u32 & 0x3F), 3)
        } else {
            let g = |k: usize| b.get(i + k).copied().unwrap_or(0) as u32 & 0x3F;
            (((x & 0x07) << 18) | (g(1) << 12) | (g(2) << 6) | g(3), 4)
        };
        i += n;
        if c >= 0x10000 {
            let c = c - 0x10000;
            out.push(0xD800 + (c >> 10) as u16);
            out.push(0xDC00 + (c & 0x3FF) as u16);
        } else {
            out.push(c as u16);
        }
    }
}

/// Text from outside (a file name) as the window, toasts, the status line and the reports show
/// it: valid Unicode (a lone surrogate becomes U+FFFD), without control characters, bidi
/// overrides and isolates (`invoice<U+202E>fdp.exe` reads "invoiceexe.pdf") or zero-width
/// spaces. The joiners U+200C (ZWNJ) and U+200D (ZWJ) stay: emoji sequences and Persian, Arabic
/// and Indic names need them (family rule, 2026-10-20). Display only: a path handed to Windows
/// or copied stays raw. At most a path's worth (2048 characters, as the family's `shown_path`).
pub fn clean(s: &str) -> String {
    clean_max(s, 2048)
}

/// `clean`, at most `max` characters (IN-VibeSpace-4): a longer text is cut, an ellipsis marks
/// the cut (characters only, never a byte). Not the template's `clean_max` (RIN3-12): runs of
/// whitespace are kept and nothing is trimmed, as a file name keeps its spaces.
pub fn clean_max(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max.saturating_mul(4)));
    for (n, c) in s.chars().filter(|&c| !invisible(c)).enumerate() {
        if n >= max {
            out.pop();
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// Characters that change how the text around them reads without showing themselves: controls,
/// bidirectional embeddings, overrides, isolates and marks, zero-width spaces and line
/// separators, the soft hyphen and the Unicode tag characters (U+E0000..U+E007F). The joiners
/// U+200C (ZWNJ) and U+200D (ZWJ) are not among them: emoji sequences and Persian, Arabic and
/// Indic names need them (family rule, 2026-10-20). The family's set
/// (VibeTemplate's text.rs), IN-VibeSpace-4.
pub fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
}

/// The path exactly as it is, for the clipboard (only a lone surrogate, which no text can
/// hold, becomes U+FFFD).
pub fn raw_path(t: &Tree, i: u32) -> String {
    String::from_utf16_lossy(&t.path_units(i))
}

/// A file name's extension as the Types table groups it: after the last dot, lower case, 1 to
/// 16 characters of letters, digits and `_-~$+`. Anything else ("Report v1.2 final") has none.
pub fn extension(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&c| c == b'.') {
        Some(p) => {
            let e = &name[p + 1..];
            if (1..=16).contains(&e.len()) && e.iter().all(|c| c.is_ascii_alphanumeric() || b"_-~$+".contains(c)) { e } else { b"" }
        }
        None => b"",
    }
}

// ---------- the arena ----------

pub struct Tree {
    pub nodes: Vec<Node>,
    pub dirs: Vec<Dir>,
    pub names: Vec<u8>,
    /// Extensions (lower case, "" for none), by `Node::aux` of a file.
    pub exts: Vec<Box<str>>,
    ext_ids: std::collections::HashMap<Box<[u8]>, u32>,
    /// The scanned folder, as given (normalized): `C:\`, `D:\Photos`, `\\server\share`.
    pub root_path: PathBuf,
    root_wide: Vec<u16>,
    pub seen: Seen,
    /// Live hard-link names (HARDLINK nodes) per file id: what a counted name that goes can
    /// hand its size to. Only hard-linked files are in it.
    links: std::collections::HashMap<u64, u32>,
    /// Names of hard-linked files this tree sent to the Recycle Bin (`recycle`), per file id:
    /// they still are names of the file, but not outside the scan (X15R2BC-1).
    binned: std::collections::HashMap<u64, u32>,
}

/// What a listing added: the totals to add to the folder and all its ancestors, and the
/// subfolders to list next.
#[derive(Debug, Default)]
pub struct Added {
    pub subdirs: Vec<u32>,
}

impl Tree {
    /// A tree with only its root folder, pending.
    pub fn new(root: PathBuf, seen: Seen) -> Tree {
        let root_wide: Vec<u16> = {
            use std::os::windows::ffi::OsStrExt;
            root.as_os_str().encode_wide().collect()
        };
        let mut t = Tree {
            nodes: Vec::new(),
            dirs: Vec::new(),
            names: Vec::new(),
            exts: vec!["".into()],
            ext_ids: Default::default(),
            root_path: root,
            root_wide,
            seen,
            links: Default::default(),
            binned: Default::default(),
        };
        t.ext_ids.insert(Box::from(&b""[..]), 0);
        let name: Vec<u16> = t.root_wide.clone();
        t.push_node(&name, Entry { attrs: ATTR_DIRECTORY, ..Entry::default() }, NONE);
        t.nodes[0].flags |= PENDING;
        t
    }

    pub fn root(&self) -> u32 {
        0
    }

    pub fn node(&self, i: u32) -> &Node {
        &self.nodes[i as usize]
    }

    pub fn dir(&self, i: u32) -> Option<&Dir> {
        let n = self.node(i);
        if n.is_dir() { self.dirs.get(n.aux as usize) } else { None }
    }

    pub fn name_bytes(&self, i: u32) -> &[u8] {
        let n = self.node(i);
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    /// The name as UTF-16, exactly as on disk.
    pub fn name_units(&self, i: u32) -> Vec<u16> {
        let mut v = Vec::new();
        wtf8_units(self.name_bytes(i), &mut v);
        v
    }

    /// The name as the window shows it (the root: its whole path).
    pub fn name(&self, i: u32) -> String {
        let s = String::from_utf16_lossy(&self.name_units(i));
        if i == 0 { clean(&s) } else { clean_max(&s, 255) }
    }

    pub fn ext(&self, i: u32) -> &str {
        let n = self.node(i);
        if n.is_dir() { "" } else { &self.exts[n.aux as usize] }
    }

    /// The full path, exactly (also for names that are not valid Unicode).
    pub fn path(&self, i: u32) -> PathBuf {
        use std::os::windows::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_wide(&self.path_units(i)))
    }

    pub fn path_units(&self, i: u32) -> Vec<u16> {
        let mut chain = Vec::new();
        let mut at = i;
        while at != 0 && at != NONE {
            chain.push(at);
            at = self.node(at).parent;
        }
        let mut out = self.root_wide.clone();
        for &c in chain.iter().rev() {
            if out.last() != Some(&(b'\\' as u16)) {
                out.push(b'\\' as u16);
            }
            wtf8_units(self.name_bytes(c), &mut out);
        }
        out
    }

    /// The path as the window shows it.
    pub fn display_path(&self, i: u32) -> String {
        clean(&String::from_utf16_lossy(&self.path_units(i)))
    }

    /// True when `i` is `a` or below it.
    pub fn is_within(&self, mut i: u32, a: u32) -> bool {
        while i != NONE {
            if i == a {
                return true;
            }
            i = self.node(i).parent;
        }
        false
    }

    pub fn depth(&self, mut i: u32) -> usize {
        let mut d = 0;
        while i != 0 && i != NONE {
            d += 1;
            i = self.node(i).parent;
        }
        d
    }

    fn ext_id(&mut self, name: &[u8]) -> u32 {
        let e = extension(name).to_ascii_lowercase();
        if let Some(&id) = self.ext_ids.get(e.as_slice()) {
            return id;
        }
        let id = self.exts.len() as u32;
        self.exts.push(String::from_utf8_lossy(&e).into());
        self.ext_ids.insert(e.into_boxed_slice(), id);
        id
    }

    fn push_node(&mut self, name: &[u16], e: Entry, parent: u32) -> u32 {
        if self.nodes.len() == self.nodes.capacity() {
            // A quarter more at a time, not double: the arena is most of the app's memory.
            self.nodes.reserve_exact((self.nodes.len() / 4).max(1024));
        }
        if self.names.capacity() - self.names.len() < name.len() * 3 {
            self.names.reserve_exact((self.names.len() / 4).max(64 * 1024));
        }
        let off = self.names.len();
        wtf8_push(&mut self.names, name);
        let len = (self.names.len() - off).min(u16::MAX as usize) as u16;
        let mut flags = 0u16;
        if e.is_dir() {
            flags |= DIR;
        }
        if e.is_link() {
            flags |= LINK;
        }
        if e.attrs & ATTR_COMPRESSED != 0 {
            flags |= COMPRESSED;
        }
        if e.attrs & ATTR_SPARSE != 0 {
            flags |= SPARSE;
        }
        if e.online_only() {
            flags |= ONLINE;
        }
        if e.in_cloud() {
            flags |= CLOUD;
        }
        let i = self.nodes.len() as u32;
        let aux = if e.is_dir() {
            if self.dirs.len() == self.dirs.capacity() {
                self.dirs.reserve_exact((self.dirs.len() / 4).max(256));
            }
            self.dirs.push(Dir { node: i, ..Dir::default() });
            (self.dirs.len() - 1) as u32
        } else {
            let b = self.names[off..off + len as usize].to_vec();
            self.ext_id(&b)
        };
        self.nodes.push(Node {
            size: 0,
            alloc: 0,
            id: e.id,
            parent,
            name_off: off as u32,
            modified: unix_secs(e.modified),
            aux,
            name_len: len,
            flags,
        });
        i
    }

    /// Set a folder's own last write (FILETIME ticks): the root's (it is in no listing it can
    /// see), and a rescanned folder's (its listing in its parent is older than the rescan).
    pub fn set_modified(&mut self, i: u32, ft: i64) {
        self.nodes[i as usize].modified = unix_secs(ft);
    }

    /// A folder's listing: its children, appended in one run. `error` marks the folder (the
    /// entries that came before a failure are kept). Returns the subfolders to list next.
    pub fn add_listing(&mut self, d: u32, entries: &[(Entry, &[u16])], error: Option<u32>) -> Added {
        let mut added = Added::default();
        if self.node(d).has(GONE) || !self.node(d).has(PENDING) {
            return added; // replaced by a rescan, or recycled, while it was being read
        }
        let first = self.nodes.len() as u32;
        // IN-VibeSpace-6: a folder that leads back up (a share's symlink loop, a damaged index)
        // is never listed again and again. None deeper than MAX_DEPTH is listed; where ids are
        // trusted (local NTFS and ReFS, as for hard links), none with the id of a folder it is
        // in. Either is kept as a folder that could not be read in full, never descended.
        let deep = self.depth(d) + 1 > MAX_DEPTH;
        let mut above = Vec::new();
        if self.seen.on && !deep {
            let mut at = d;
            while at != NONE {
                above.push(self.node(at).id);
                at = self.node(at).parent;
            }
        }
        let (mut size, mut alloc, mut files, mut folders) = (0u64, 0u64, 0u64, 0u64);
        for (e, name) in entries {
            let i = self.push_node(name, *e, d);
            if e.is_dir() {
                folders += 1;
                let n = &mut self.nodes[i as usize];
                if n.flags & (LINK | ONLINE) == 0 {
                    if deep || (e.id != 0 && e.id != u64::MAX && above.contains(&e.id)) {
                        n.flags |= UNREADABLE;
                    } else {
                        n.flags |= PENDING;
                        added.subdirs.push(i);
                    }
                }
            } else {
                files += 1;
                let counts = e.is_link() || self.seen.insert(e.id);
                let n = &mut self.nodes[i as usize];
                if !counts {
                    n.flags |= HARDLINK;
                    *self.links.entry(e.id).or_insert(0) += 1;
                } else {
                    n.size = e.size;
                    // A placeholder's data is in the cloud: nothing on this disk.
                    n.alloc = if n.flags & ONLINE != 0 { 0 } else { e.alloc };
                    size += n.size;
                    alloc += n.alloc;
                }
            }
        }
        let count = self.nodes.len() as u32 - first;
        let di = self.node(d).aux as usize;
        self.dirs[di].first = first;
        self.dirs[di].count = count;
        let n = &mut self.nodes[d as usize];
        n.flags &= !PENDING;
        match error {
            Some(5) => n.flags |= NO_ACCESS,
            Some(_) => n.flags |= UNREADABLE,
            None => {}
        }
        self.add_up(d, size as i128, alloc as i128, files as i128, folders as i128);
        added
    }

    /// Add to a folder's totals and every ancestor's.
    fn add_up(&mut self, mut d: u32, size: i128, alloc: i128, files: i128, folders: i128) {
        while d != NONE {
            let n = &mut self.nodes[d as usize];
            n.size = (n.size as i128 + size).max(0) as u64;
            n.alloc = (n.alloc as i128 + alloc).max(0) as u64;
            let di = n.aux as usize;
            let parent = n.parent;
            let dr = &mut self.dirs[di];
            dr.files = (dr.files as i128 + files).max(0) as u64;
            dr.folders = (dr.folders as i128 + folders).max(0) as u64;
            d = parent;
        }
    }

    /// Every live child of a folder (in listing order).
    pub fn children(&self, d: u32) -> impl Iterator<Item = u32> + '_ {
        let (first, count) = self.dir(d).map(|x| (x.first, x.count)).unwrap_or((0, 0));
        (first..first + count).filter(move |&c| !self.node(c).has(GONE))
    }

    pub fn has_children(&self, d: u32) -> bool {
        self.children(d).next().is_some()
    }

    /// A folder's children, largest first by `m`; ties by name (ASCII case-blind, then
    /// byte-exact, then arena order): a total order whatever the names hold.
    pub fn sorted_children(&self, d: u32, m: Metric) -> Vec<u32> {
        let mut v: Vec<u32> = self.children(d).collect();
        v.sort_unstable_by(|&a, &b| self.compare(a, b, m));
        v
    }

    pub fn compare(&self, a: u32, b: u32, m: Metric) -> std::cmp::Ordering {
        let (na, nb) = (self.node(a), self.node(b));
        nb.value(m)
            .cmp(&na.value(m))
            .then_with(|| {
                let (x, y) = (self.name_bytes(a), self.name_bytes(b));
                x.iter().map(|c| c.to_ascii_lowercase()).cmp(y.iter().map(|c| c.to_ascii_lowercase())).then_with(|| x.cmp(y))
            })
            .then(a.cmp(&b))
    }

    /// Files and folders under `i` (a file: itself).
    pub fn counts(&self, i: u32) -> (u64, u64) {
        match self.dir(i) {
            Some(d) => (d.files, d.folders),
            None => (1, 0),
        }
    }

    /// How many names of the file with this id the scan saw: the counted one and its hard-link
    /// names (X15WFS-4: a file with more names than that has one outside the scan).
    pub fn names_of(&self, id: u64) -> u32 {
        1 + self.links.get(&id).copied().unwrap_or(0)
    }

    /// The names of the file with this id that this tree knows of: those in the scan and those
    /// it sent to the Recycle Bin (X15R2BC-1: a name in the bin is not one outside the scan;
    /// emptying the bin frees it).
    pub fn known_names(&self, id: u64) -> u32 {
        self.names_of(id) + self.binned.get(&id).copied().unwrap_or(0)
    }

    /// Appends the name of `i` to `out` as UTF-16 (no allocation of its own).
    pub fn push_name(&self, i: u32, out: &mut Vec<u16>) {
        wtf8_units(self.name_bytes(i), out);
    }

    /// `i` and everything under it (not gone), depth first.
    pub fn subtree(&self, i: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![i];
        while let Some(x) = stack.pop() {
            out.push(x);
            if let Some(d) = self.dir(x) {
                for c in d.first..d.first + d.count {
                    if !self.node(c).has(GONE) {
                        stack.push(c);
                    }
                }
            }
        }
        out
    }

    /// Take `items` out (moved to the Recycle Bin): each and everything under it count nothing
    /// any more, their ancestors shrink by what they held. The root cannot be taken out.
    pub fn remove(&mut self, items: &[u32]) {
        self.take_out(items, false);
    }

    /// `remove` for items moved to the Recycle Bin: the names of hard-linked files among them
    /// are kept count of (`known_names`).
    pub fn recycle(&mut self, items: &[u32]) {
        self.take_out(items, true);
    }

    fn take_out(&mut self, items: &[u32], bin: bool) {
        let mut gone = Vec::new();
        for &i in items {
            if i == 0 || self.node(i).has(GONE) {
                continue;
            }
            let n = *self.node(i);
            let (files, folders) = self.counts(i);
            let (f, d) = if n.is_dir() { (files, folders + 1) } else { (1, 0) };
            self.mark_gone(i, &mut gone, bin);
            self.add_up(n.parent, -(n.size as i128), -(n.alloc as i128), -(f as i128), -(d as i128));
        }
        self.forget(gone);
    }

    /// Empty folders for a rescan: what was under them goes (file ids no other name holds are
    /// forgotten, so the rescan counts them again), their totals go to zero, and they are
    /// pending again.
    pub fn reset(&mut self, dirs: &[u32]) {
        let mut gone = Vec::new();
        for &d in dirs {
            let Some(dr) = self.dir(d).copied() else { continue };
            let n = *self.node(d);
            for c in dr.first..dr.first + dr.count {
                if !self.node(c).has(GONE) {
                    self.mark_gone(c, &mut gone, false);
                }
            }
            self.add_up(d, -(n.size as i128), -(n.alloc as i128), -(dr.files as i128), -(dr.folders as i128));
            let di = n.aux as usize;
            self.dirs[di].first = 0;
            self.dirs[di].count = 0;
            self.nodes[d as usize].flags = (self.nodes[d as usize].flags & !(NO_ACCESS | UNREADABLE)) | PENDING;
        }
        self.forget(gone);
    }

    /// `i` and everything under it marked gone. A counted file whose id no hard-link name
    /// holds is forgotten at once; one that has such a name goes into `gone` as
    /// (id, size, alloc), for `forget` (few: nothing is collected for a tree without links).
    /// `bin`: it went to the Recycle Bin, where a hard-linked file's name stays one of its names.
    fn mark_gone(&mut self, i: u32, gone: &mut Vec<(u64, u64, u64)>, bin: bool) {
        for x in self.subtree(i) {
            let n = self.nodes[x as usize];
            self.nodes[x as usize].flags |= GONE;
            if n.flags & DIR != 0 {
                continue;
            }
            if bin && (n.flags & HARDLINK != 0 || self.links.contains_key(&n.id)) {
                *self.binned.entry(n.id).or_insert(0) += 1;
            }
            if n.flags & HARDLINK != 0 {
                self.unlink(n.id);
            } else if self.links.contains_key(&n.id) {
                gone.push((n.id, n.size, n.alloc));
            } else {
                self.seen.remove(n.id);
            }
        }
    }

    /// One hard-link name of `id` fewer.
    fn unlink(&mut self, id: u64) {
        if let Some(c) = self.links.get_mut(&id) {
            *c -= 1;
            if *c == 0 {
                self.links.remove(&id);
            }
        }
    }

    /// Counted files that went while hard-link names of them were in the tree: one such name
    /// still there (it showed 0) counts the file now, with the sizes the counted name had. Ids
    /// with no name left are forgotten; one taken over stays seen, so a rescan that finds the
    /// name that went marks it a hard link again. One pass over the arena, only when needed.
    fn forget(&mut self, gone: Vec<(u64, u64, u64)>) {
        let mut left: std::collections::HashMap<u64, (u64, u64)> = std::collections::HashMap::new();
        for (id, s, a) in gone {
            // Its other names may have gone in the same call.
            if self.links.contains_key(&id) {
                left.insert(id, (s, a));
            } else {
                self.seen.remove(id);
            }
        }
        if left.is_empty() {
            return;
        }
        for i in 0..self.nodes.len() {
            let n = self.nodes[i];
            if n.flags & (HARDLINK | GONE) == HARDLINK
                && let Some((size, alloc)) = left.remove(&n.id)
            {
                let x = &mut self.nodes[i];
                x.flags &= !HARDLINK;
                x.size = size;
                x.alloc = alloc;
                self.unlink(n.id);
                self.add_up(n.parent, size as i128, alloc as i128, 0, 0);
            }
        }
        for id in left.into_keys() {
            self.seen.remove(id);
        }
    }

    /// Folders that could not be read (no access, other errors), and links not followed.
    pub fn skipped(&self) -> Skipped {
        let mut s = Skipped::default();
        for d in &self.dirs {
            let n = self.node(d.node);
            if n.has(GONE) {
                continue;
            }
            if n.has(NO_ACCESS) {
                s.no_access += 1;
            }
            if n.has(UNREADABLE) {
                s.unreadable += 1;
            }
            if n.has(PENDING) {
                s.pending += 1;
            }
            if n.has(ONLINE) {
                s.online += 1;
            }
        }
        s
    }

    /// The live nodes (not gone), for the stats.
    pub fn live(&self) -> impl Iterator<Item = (u32, &Node)> + '_ {
        self.nodes.iter().enumerate().filter(|(_, n)| !n.has(GONE)).map(|(i, n)| (i as u32, n))
    }

    /// Bytes the arena holds (capacity, the memory it costs).
    pub fn bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.dirs.capacity() * std::mem::size_of::<Dir>()
            + self.names.capacity()
            + self.seen.bytes()
            + self.links.capacity() * 13
            + self.binned.capacity() * 13
    }

    /// Give back what growing reserved (at the end of a scan).
    pub fn shrink(&mut self) {
        self.nodes.shrink_to_fit();
        self.dirs.shrink_to_fit();
        self.names.shrink_to_fit();
    }

    /// The node at `path` (exact, case-blind for ASCII as Windows is), if the scan has it.
    pub fn find(&self, path: &std::path::Path) -> Option<u32> {
        let root = self.root_path.to_string_lossy().to_lowercase();
        let p = path.to_string_lossy().to_lowercase();
        let rest = p.strip_prefix(root.trim_end_matches('\\'))?;
        // `D:\Photos2` is not under `D:\Photos`: the root must end at a separator.
        if !(rest.is_empty() || rest.starts_with('\\')) {
            return None;
        }
        let mut at = 0u32;
        for part in rest.split('\\').filter(|s| !s.is_empty()) {
            at = self.children(at).find(|&c| String::from_utf8_lossy(self.name_bytes(c)).to_lowercase() == part)?;
        }
        Some(at)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    pub no_access: u64,
    pub unreadable: u64,
    pub pending: u64,
    pub online: u64,
}
