//! "Move to another drive and leave a link" (SPEC.md §6): a big folder is copied to another
//! drive, every file checked by hash, and a directory junction left at the old path, so every
//! program still finds it there. At every moment either the original is whole where it was, or
//! a complete, checked copy is, with a journal that says how to get back.
//!
//! `plan` and `plan_back` look (they change nothing) and say what would happen and what is in
//! the way; `run::move_folder`, `run::move_back` and `run::recover` act, each step only after
//! the one before succeeded, all through the `Fs` seam.

pub mod fs;
pub mod journal;
pub mod run;
pub mod win;

use fs::Fs;
use journal::{Journal, Record};
use std::path::{Path, PathBuf};
use win::Identity;

/// What the move refuses to touch, and where its journals live.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub journal: Option<Journal>,
    /// Refused with everything inside them, and any folder holding one (the Windows folder,
    /// Program Files, ProgramData, the Store apps' data).
    pub sealed: Vec<PathBuf>,
    /// Refused themselves, and any folder that holds one (the profile, AppData, ...).
    pub roots: Vec<PathBuf>,
    /// The folder of every account's profile, and this account's own: every other profile is
    /// refused with everything inside it.
    pub users: Option<PathBuf>,
    pub me: Option<PathBuf>,
    /// VibeSpace's own (its data, its exe's folder): refused, and any folder holding them.
    pub own: Vec<PathBuf>,
    /// OneDrive's folders (and the SharePoint libraries it syncs): refused inside, themselves,
    /// or held, as the folder moved and as the destination.
    pub onedrive: Vec<PathBuf>,
    /// Only for the one test on a real removable drive: its refusal is lifted.
    pub allow_removable: bool,
    /// The Windows drive (`C:`): the only one whose `WINDOWS_DRIVE_SYSTEM` folders are Windows'
    /// own (RIN3-10). None: not known, and no drive's are.
    pub windows_drive: Option<String>,
}

/// Windows' own folders at the root of every drive: refused with everything inside them.
pub const DRIVE_SYSTEM: [&str; 2] = ["$Recycle.Bin", "System Volume Information"];
/// Windows' own folders at the root of the Windows drive only (the recovery environment, Setup
/// and its updates, the installer's rollback): on a data drive `E:\Recovery` is the user's own
/// (RIN3-10).
pub const WINDOWS_DRIVE_SYSTEM: [&str; 7] = ["Recovery", "$WinREAgent", "$WINDOWS.~BT", "$Windows.~WS", "Windows.old", "$SysReset", "Config.Msi"];
/// Left free on the destination beyond what the copy takes.
pub const HEADROOM: u64 = 64 << 20;
/// What the proof reads back through the link at most.
pub const PROOF_BYTES: u64 = 256 << 20;

impl Env {
    /// The system's folders as Windows names them, and the app's own. No disk is asked here
    /// (a profile redirected to a share that does not answer would hold up the start): each
    /// is resolved by the plan, on its thread (`resolved`).
    pub fn system(local_data: &Path, config: &Path) -> Env {
        use windows_sys::Win32::UI::Shell::{
            FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow, FOLDERID_Profile, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86,
            FOLDERID_RoamingAppData, FOLDERID_UserProfiles,
        };
        let known = |id| crate::host::known_folder(id);
        let mut roots: Vec<PathBuf> = [&FOLDERID_UserProfiles, &FOLDERID_Profile, &FOLDERID_RoamingAppData, &FOLDERID_LocalAppData, &FOLDERID_LocalAppDataLow].into_iter().filter_map(known).collect();
        if let Some(local) = known(&FOLDERID_LocalAppData)
            && let Some(appdata) = local.parent()
        {
            roots.push(appdata.to_path_buf());
        }
        roots.push(std::env::temp_dir());
        let mut sealed: Vec<PathBuf> = crate::host::windows_dir(false).into_iter().collect();
        sealed.extend([&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86, &FOLDERID_ProgramFilesX64, &FOLDERID_ProgramData].into_iter().filter_map(known));
        if let Some(local) = known(&FOLDERID_LocalAppData) {
            sealed.push(local.join("Packages"));
        }
        let mut own = vec![local_data.to_path_buf(), config.to_path_buf()];
        if let Some(exe) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            own.push(exe);
        }
        let norm = |v: Vec<PathBuf>| -> Vec<PathBuf> { v.iter().map(|p| crate::scan::normalize(p)).collect() };
        Env {
            journal: Some(Journal::new(local_data.join("moves"))),
            sealed: norm(sealed),
            roots: norm(roots),
            users: known(&FOLDERID_UserProfiles).map(|p| crate::scan::normalize(&p)),
            me: known(&FOLDERID_Profile).map(|p| crate::scan::normalize(&p)),
            own: norm(own),
            onedrive: norm(onedrive_folders()),
            allow_removable: false,
            windows_drive: crate::host::windows_dir(false).and_then(|w| w.to_str().and_then(|s| s.get(..2)).filter(|d| d.ends_with(':')).map(str::to_string)),
        }
    }

    /// Each folder as given and as Windows ends up at it (through a junction, an 8.3 name, a
    /// mapped letter), so neither form slips past. Asks the disk: the plan's thread only.
    fn resolved(fs: &dyn Fs, v: &[PathBuf]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for p in v {
            if let Ok(f) = fs.final_path(p)
                && !same_path(&f, p)
            {
                out.push(f);
            }
            out.push(p.clone());
        }
        out
    }
}

/// OneDrive's folders: the `OneDrive*` environment variables and each account's `UserFolder`.
pub fn onedrive_folders() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"].iter().filter_map(std::env::var_os).filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    v.extend(registry_user_folders());
    v
}

fn registry_user_folders() -> Vec<PathBuf> {
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegGetValueW, RegOpenKeyExW};
    let mut out = Vec::new();
    let base = crate::host::wide("Software\\Microsoft\\OneDrive\\Accounts");
    let mut key: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, base.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return out;
    }
    let value = crate::host::wide("UserFolder");
    for i in 0..64u32 {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        if unsafe { RegEnumKeyExW(key, i, name.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) } != 0 {
            break;
        }
        let mut data = vec![0u16; 1024];
        let mut size = (data.len() * 2) as u32;
        let sub: Vec<u16> = name[..len as usize].iter().copied().chain([0]).collect();
        if unsafe { RegGetValueW(key, sub.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), data.as_mut_ptr().cast(), &mut size) } == 0 {
            let n = data.iter().position(|&c| c == 0).unwrap_or(0);
            if n > 0 {
                out.push(PathBuf::from(String::from_utf16_lossy(&data[..n])));
            }
        }
        // SharePoint and Teams libraries this account syncs: each tenant's values are named by
        // the folders.
        let tenants = crate::host::wide(&format!("Software\\Microsoft\\OneDrive\\Accounts\\{}\\Tenants", String::from_utf16_lossy(&name[..len as usize])));
        let mut tk: HKEY = std::ptr::null_mut();
        if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, tenants.as_ptr(), 0, KEY_READ, &mut tk) } == 0 {
            for t in 0..64u32 {
                let mut tname = [0u16; 256];
                let mut tlen = tname.len() as u32;
                if unsafe { RegEnumKeyExW(tk, t, tname.as_mut_ptr(), &mut tlen, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) } != 0 {
                    break;
                }
                let mut vk: HKEY = std::ptr::null_mut();
                let tsub: Vec<u16> = tname[..tlen as usize].iter().copied().chain([0]).collect();
                if unsafe { RegOpenKeyExW(tk, tsub.as_ptr(), 0, KEY_READ, &mut vk) } != 0 {
                    continue;
                }
                for v in 0..256u32 {
                    let mut vname = vec![0u16; 1024];
                    let mut vlen = vname.len() as u32;
                    if unsafe { RegEnumValueW(vk, v, vname.as_mut_ptr(), &mut vlen, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) } != 0 {
                        break;
                    }
                    let folder = String::from_utf16_lossy(&vname[..vlen as usize]);
                    if folder.len() > 3 && folder.as_bytes()[1] == b':' {
                        out.push(PathBuf::from(folder));
                    }
                }
                unsafe { RegCloseKey(vk) };
            }
            unsafe { RegCloseKey(tk) };
        }
    }
    unsafe { RegCloseKey(key) };
    out
}

/// Two plain paths name the same place (case-blind, a trailing separator aside).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let k = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    k(a) == k(b)
}

/// `p` is `root` or inside it (case-blind, on whole names).
pub fn within(p: &Path, root: &Path) -> bool {
    let (p, r) = (p.to_string_lossy().to_lowercase(), root.to_string_lossy().trim_end_matches('\\').to_lowercase());
    if r.is_empty() {
        return false;
    }
    p.trim_end_matches('\\') == r || p.strip_prefix(&r).is_some_and(|rest| rest.starts_with('\\'))
}

/// A path as the window shows it (no control or invisible characters).
pub fn show(p: &Path) -> String {
    crate::tree::clean(&p.display().to_string())
}

/// One file or folder under the folder moved, as the walk saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Relative to the folder, `\`-separated, the exact units.
    pub rel: Vec<u16>,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    /// ChangeTime (0: unknown, not compared).
    pub ctime: i64,
    /// The file id, folded (0: unknown, not compared).
    pub id: u64,
}

impl Item {
    pub fn path(&self, root: &Path) -> PathBuf {
        use std::os::windows::ffi::OsStringExt;
        root.join(std::ffi::OsString::from_wide(&self.rel))
    }
    pub fn name(&self) -> String {
        crate::tree::clean(&String::from_utf16_lossy(&self.rel))
    }
}

/// What a walk found.
#[derive(Clone, Debug, Default)]
pub struct Walk {
    /// Folders before what is in them.
    pub items: Vec<Item>,
    pub links: Vec<String>,
    pub special: Vec<String>,
    pub online: u64,
    pub alloc: u64,
    /// Sparse files (they take their whole size where they go) and EFS-encrypted ones.
    pub sparse: Vec<String>,
    pub encrypted: Vec<String>,
}

const ATTR_DIR: u32 = 0x10;
const ATTR_SPARSE: u32 = 0x200;
const ATTR_REPARSE: u32 = 0x400;
const ATTR_ENCRYPTED: u32 = 0x4000;
const TAG_WOF: u32 = 0x8000_0017;
const TAG_DEDUP: u32 = 0x8000_0013;

/// Everything under `root`, never through a link. Err: a folder could not be listed (a move
/// must read everything) or `stop`.
pub fn walk(fs: &dyn Fs, root: &Path, stop: &dyn Fn() -> bool) -> Result<Walk, String> {
    let mut w = Walk::default();
    let mut stack: Vec<Vec<u16>> = vec![Vec::new()];
    while let Some(rel) = stack.pop() {
        if stop() {
            return Err("Cancelled".into());
        }
        let dir = if rel.is_empty() { root.to_path_buf() } else { Item { rel: rel.clone(), dir: true, size: 0, mtime: 0, ctime: 0, id: 0 }.path(root) };
        let entries = fs.list(&dir).map_err(|e| format!("{} could not be read ({})", show(&dir), win::describe(&e)))?;
        let mut subs = Vec::new();
        for e in entries {
            let mut r = rel.clone();
            if !r.is_empty() {
                r.push(u16::from(b'\\'));
            }
            r.extend_from_slice(&e.name);
            let dir = e.attrs & ATTR_DIR != 0;
            let it = Item { rel: r, dir, size: if dir { 0 } else { e.size }, mtime: e.mtime, ctime: e.ctime, id: e.id };
            let entry = crate::tree::Entry { attrs: e.attrs, tag: e.tag, ..Default::default() };
            if entry.is_link() {
                w.links.push(it.name());
                continue;
            }
            if entry.online_only() {
                w.online += 1;
                continue;
            }
            if e.attrs & ATTR_REPARSE != 0 && e.tag != TAG_WOF && e.tag != TAG_DEDUP {
                w.special.push(it.name());
                continue;
            }
            w.alloc += e.alloc;
            if it.dir {
                subs.push(it.rel.clone());
            } else {
                if e.attrs & ATTR_SPARSE != 0 {
                    w.sparse.push(it.name());
                }
                if e.attrs & ATTR_ENCRYPTED != 0 {
                    w.encrypted.push(it.name());
                }
            }
            w.items.push(it);
        }
        // Depth first, in listing order: a folder's own entries come before its subfolders'.
        stack.extend(subs.into_iter().rev());
    }
    Ok(w)
}

/// The plan of a move or a move back: what goes where, what it takes, and what is in the way.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub back: bool,
    /// Where the data is now.
    pub from: PathBuf,
    /// Where it goes.
    pub to: PathBuf,
    /// The path that is a link (before a move: will be; before a move back: is).
    pub link: PathBuf,
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    /// On disk where it is now (what that drive gains).
    pub alloc: u64,
    /// What the copy takes where it goes (whole clusters).
    pub need: u64,
    /// Free on the drive it goes to, now.
    pub free: Option<u64>,
    pub from_root: String,
    pub to_root: String,
    /// The destination's file system: FAT and exFAT keep no permissions.
    pub to_fs: String,
    pub warnings: Vec<String>,
    pub refusals: Vec<String>,
    pub items: Vec<Item>,
    pub from_ident: Identity,
    pub sparse: Vec<String>,
    pub encrypted: Vec<String>,
    /// The unfinished move a refusal is about (`another_move`): the window leads back to it
    /// (R-SPACE-MV3-4).
    pub other_move: Option<Record>,
    /// Where the copy goes as Windows ends up at it (the destination resolved, the name added):
    /// kept in the journal, so Move back checks the place the plan checked (RIN3-9).
    pub to_real: PathBuf,
}

impl Plan {
    pub fn ok(&self) -> bool {
        self.refusals.is_empty()
    }
    /// Free on the destination once the copy is there (negative: it does not fit).
    pub fn free_after(&self) -> Option<i128> {
        self.free.map(|f| f as i128 - self.need as i128)
    }
}

/// The request: this folder into that one.
#[derive(Clone, Debug)]
pub struct Request {
    pub source: PathBuf,
    /// The folder picked; the folder goes into it under its own name.
    pub dest: PathBuf,
}

fn names(v: &[String]) -> String {
    let shown: Vec<&str> = v.iter().take(5).map(String::as_str).collect();
    let more = v.len().saturating_sub(5);
    if more > 0 { format!("{} and {} more", shown.join(", "), crate::format::count(more as u64)) } else { shown.join(", ") }
}

fn fat(fs: &str) -> bool {
    fs.eq_ignore_ascii_case("FAT") || fs.eq_ignore_ascii_case("FAT32") || fs.eq_ignore_ascii_case("exFAT")
}

fn round_up(n: u64, c: u64) -> u64 {
    if c == 0 { n } else { n.div_ceil(c).saturating_mul(c) }
}

/// What the files take on a drive with clusters of `cluster` bytes (each folder a cluster).
fn need(items: &[Item], cluster: u64) -> u64 {
    items.iter().map(|i| if i.dir { cluster } else { round_up(i.size, cluster) }).sum::<u64>() + cluster
}

/// Why a folder cannot be renamed, in words.
fn rename_refusal(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(5) => "VibeSpace may not rename this folder here (it needs administrator rights): nothing can be moved".into(),
        Some(32) | Some(33) => "a program is working inside this folder (a console or an app has it open): close it first".into(),
        _ => format!("this folder cannot be renamed now ({})", win::describe(e)),
    }
}

/// Which programs have any file under `root` open, a handle or a mapped view; every file is
/// asked. Ok(None): none. Ok(Some(names)): these. Err: the answer is not complete (never
/// taken as "none").
pub fn in_use(fs: &dyn Fs, root: &Path, items: &[Item], stop: &dyn Fn() -> bool) -> Result<Option<String>, String> {
    let files: Vec<PathBuf> = items.iter().filter(|i| !i.dir).map(|i| i.path(root)).collect();
    let h = fs.holders(&files, stop)?;
    if h.is_empty() {
        return Ok(None);
    }
    let mut n: Vec<String> = h.into_iter().map(|(pid, name)| if name.is_empty() { format!("process {pid}") } else { name }).collect();
    n.sort();
    n.dedup();
    Ok(Some(names(&n)))
}

fn holders(fs: &dyn Fs, root: &Path, items: &[Item], stop: &dyn Fn() -> bool, plan: &mut Plan) {
    match in_use(fs, root, items, stop) {
        Ok(None) => {}
        Ok(Some(n)) => plan.refusals.push(format!("Files in it are in use by {n}: close them first")),
        Err(e) => plan.refusals.push(format!("Could not ask which programs have files open: {e}")),
    }
}

/// Said in every plan that leaves a link: what the link names, and what a sparse or encrypted
/// file becomes where it goes.
fn advise(p: &mut Plan) {
    let to = p.to_root.trim_end_matches('\\').to_string();
    if !p.back && to.len() == 2 && to.ends_with(':') {
        p.warnings.push(format!("The link leads to {to} by its letter: if that drive ever gets another letter, programs find nothing at the old path until it is {to} again"));
    }
    if !p.sparse.is_empty() {
        p.warnings.push(format!(
            "{} sparse ({}): where they go they take their whole size; FREE AFTER counts that, the drive they leave gains only what they take there now",
            crate::format::plural(p.sparse.len() as u64, "file is", "files are"),
            names(&p.sparse)
        ));
    }
    if !p.encrypted.is_empty() {
        let n = crate::format::plural(p.encrypted.len() as u64, "file is", "files are");
        p.warnings.push(if fat(&p.to_fs) {
            format!("{n} encrypted ({}): {} cannot keep them encrypted, so the copy stops at the first one and nothing is moved", names(&p.encrypted), p.to_fs)
        } else {
            format!("{n} encrypted ({}): they stay encrypted where they go, readable by this account only", names(&p.encrypted))
        });
    }
}

/// Walk results into the plan (counts and refusals).
fn take_walk(plan: &mut Plan, w: Walk) {
    plan.sparse = w.sparse;
    plan.encrypted = w.encrypted;
    if !w.links.is_empty() {
        plan.refusals.push(format!("It holds links (junctions or symbolic links), which a copy cannot carry safely: {}", names(&w.links)));
    }
    if !w.special.is_empty() {
        plan.refusals.push(format!("It holds special files Windows treats on its own: {}", names(&w.special)));
    }
    if w.online > 0 {
        plan.refusals.push(format!("It holds {} online-only (cloud) files: reading them would download them", crate::format::count(w.online)));
    }
    plan.files = w.items.iter().filter(|i| !i.dir).count() as u64;
    plan.folders = w.items.iter().filter(|i| i.dir).count() as u64;
    plan.bytes = w.items.iter().map(|i| i.size).sum();
    plan.alloc = w.alloc;
    plan.items = w.items;
}

/// The plan of moving `req.source` into `req.dest`. Changes nothing.
pub fn plan(fs: &dyn Fs, env: &Env, req: &Request, stop: &dyn Fn() -> bool) -> Plan {
    let src = crate::scan::normalize(&req.source);
    let dest = crate::scan::normalize(&req.dest);
    let name = src.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    let mut p = Plan { from: src.clone(), to: dest.join(&name), link: src.clone(), ..Plan::default() };
    if src.to_str().is_none() || p.to.to_str().is_none() {
        p.refusals.push("Its path holds a character Windows allows but text cannot (rename it first)".into());
        return p;
    }
    // The folder itself.
    let it = match fs.item(&src, false) {
        Ok(i) => i,
        Err(e) => {
            p.refusals.push(format!("The folder could not be read ({})", win::describe(&e)));
            return p;
        }
    };
    if !it.dir {
        p.refusals.push("It is a file, not a folder".into());
        return p;
    }
    if it.tag.is_some() {
        let link = it.tag.is_some_and(|t| t & 0x2000_0000 != 0);
        p.refusals.push(if link { "It is already a link to somewhere else".into() } else { "It is a special folder Windows treats on its own (a cloud or system folder)".into() });
        return p;
    }
    if it.attrs & 0x40000 != 0 {
        p.refusals.push("It is an online-only (cloud) folder".into());
        return p;
    }
    p.from_ident = it.ident.clone();
    let real = fs.final_path(&src).unwrap_or_else(|_| src.clone());
    if real.parent().is_none() || src.parent().is_none() {
        p.refusals.push("It is a drive's root: a whole drive cannot be moved".into());
        return p;
    }
    let resolved = env.resolved_by(fs);
    p.refusals.extend(place_refusals(fs, &resolved, &src, false));
    // IN-VibeSpace-7, RIN3-9: the copy's place passes the same check a Move back and a recovery
    // make (as given and as Windows ends up at it), so no journal names a copy they would
    // refuse. OneDrive is said below, in the destination's own words.
    p.to_real = fs.final_path(&dest).map(|d| d.join(&name)).unwrap_or_else(|_| p.to.clone());
    if let Some(r) = place_refusals(fs, &Resolved { onedrive: Vec::new(), ..resolved.clone() }, &p.to, true).into_iter().next() {
        p.refusals.push(r);
    }
    p.refusals.dedup();
    if !p.refusals.is_empty() {
        return p;
    }
    // Its drive.
    match fs.volume(&src) {
        Ok(v) => {
            p.from_root = v.root.clone();
            if v.kind != 3 {
                p.refusals.push("It is not on a fixed local drive: a link cannot be left there".into());
            } else if v.flags & 0x80 == 0 {
                p.refusals.push(format!("Its drive ({}) cannot hold a link", if v.fs.is_empty() { "?" } else { &v.fs }));
            }
        }
        Err(e) => p.refusals.push(format!("Its drive could not be asked ({})", win::describe(&e))),
    }
    if let Err(e) = fs.may_rename(&src) {
        p.refusals.push(rename_refusal(&e));
    }
    if let Some(parent) = src.parent()
        && let Err(e) = fs.may_add(parent)
    {
        p.refusals.push(format!("VibeSpace may not leave a link in {} ({})", show(parent), win::describe(&e)));
    }
    // The destination.
    let to_vol = match fs.item(&dest, true) {
        Ok(d) if d.dir => {
            let dreal = fs.final_path(&dest).unwrap_or_else(|_| dest.clone());
            if within(&dreal, &real) {
                p.refusals.push("The destination is inside the folder itself".into());
            }
            if let Some(o) = resolved.onedrive.iter().find(|o| within(&dreal, o) || within(&dest, o)) {
                p.refusals.push(format!("The destination is in a OneDrive folder ({}): OneDrive would upload the copy and may leave only online-only files", show(o)));
            }
            let same_serial = d.ident.volume != 0 && d.ident.volume == it.ident.volume;
            let same_guid = matches!((&d.ident.guid, &it.ident.guid), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
            if same_serial || same_guid {
                p.refusals.push("The destination is on the same drive: a move there frees nothing".into());
            } else if d.ident.volume == 0 && d.ident.guid.is_none() {
                p.refusals.push("Cannot tell whether the destination is another drive".into());
            }
            match fs.volume(&dest) {
                Ok(v) => Some(v),
                Err(e) => {
                    p.refusals.push(format!("The destination's drive could not be asked ({})", win::describe(&e)));
                    None
                }
            }
        }
        Ok(_) => {
            p.refusals.push("The destination is not a folder".into());
            None
        }
        Err(e) => {
            p.refusals.push(format!("The destination could not be read ({})", win::describe(&e)));
            None
        }
    };
    if let Some(v) = &to_vol {
        p.to_root = v.root.clone();
        p.to_fs = v.fs.clone();
        p.free = Some(v.free);
        match v.kind {
            4 => p.refusals.push("The destination is a network drive: a link cannot lead there".into()),
            2 if !env.allow_removable => p.refusals.push("The destination is a removable drive: unplugged, it would leave programs with nothing at the old path".into()),
            2 | 3 => {}
            _ => p.refusals.push("The destination is not a drive a link should lead to (an optical or RAM drive)".into()),
        }
        if fat(&v.fs) {
            p.warnings.push(format!("{} is {}: the copy keeps no permissions and no alternate streams (such as Windows' 'downloaded from the internet' mark)", v.root.trim_end_matches('\\'), v.fs));
        }
        if v.kind == 3 && v.usb == Some(true) {
            p.warnings.push(format!("{} is a USB disk: while it is unplugged, programs find nothing at the old path", v.root.trim_end_matches('\\')));
        }
    }
    match fs.item(&p.to, false) {
        Ok(_) => p.refusals.push(format!("Something named {} is already in {}", show(Path::new(&name)), show(&dest))),
        Err(e) if win::is_gone(&e) => {}
        Err(e) => p.refusals.push(format!("Cannot tell whether {} is free ({})", show(&p.to), win::describe(&e))),
    }
    // What is in it.
    match walk(fs, &src, stop) {
        Ok(w) => take_walk(&mut p, w),
        Err(e) => {
            p.refusals.push(format!("Not everything in it could be read: {e}"));
            return p;
        }
    }
    if let Some(v) = &to_vol {
        p.need = need(&p.items, v.cluster);
        if v.fs.eq_ignore_ascii_case("FAT32") || v.fs.eq_ignore_ascii_case("FAT") {
            let big: Vec<String> = p.items.iter().filter(|i| !i.dir && i.size >= 4 << 30).map(Item::name).collect();
            if !big.is_empty() {
                p.refusals.push(format!("{} holds no file of 4 GB or more: {}", v.fs, names(&big)));
            }
        }
        if p.need.saturating_add(HEADROOM) > v.free {
            p.refusals.push(format!("Not enough room on {}: the copy takes {}, {} is free", v.root.trim_end_matches('\\'), crate::format::bytes(p.need), crate::format::bytes(v.free)));
        }
    }
    if let Some((r, o)) = env.journal.as_ref().and_then(|j| another_move(j, &src, Some(&p.from_ident), "")) {
        p.refusals.push(o);
        p.other_move = Some(r);
    }
    if p.refusals.is_empty() {
        let items = std::mem::take(&mut p.items);
        holders(fs, &src, &items, stop, &mut p);
        p.items = items;
    }
    advise(&mut p);
    fs.checkpoint("planned");
    p
}

/// A move under way or not finished (a journal other than `except`, not done) that names
/// `folder` as its folder or as its copy, and that move, in words: a second move of it would
/// take that move's folder or link from under it (R-SPACE-MV2-1). Known by what it is
/// (`ident`: the original and its aside are one folder, the copy and its aside another) as
/// well as by how it is spelled, so an 8.3 short name does not slip past (R-SPACE-MV4-5).
pub fn another_move(j: &Journal, folder: &Path, ident: Option<&Identity>, except: &str) -> Option<(Record, String)> {
    let (records, _) = j.read_all();
    let open = || records.iter().filter(|r| r.id != except && r.phase != journal::Phase::Done);
    let is = |i: Option<&Identity>| ident.is_some_and(|x| i.is_some_and(|i| i.same(x)));
    if let Some(r) = open().find(|r| same_path(&r.source(), folder) || same_path(&r.target(), folder) || is(Some(&r.source_ident)) || is(r.target_ident.as_ref())) {
        let why = format!(
            "Another move of this folder is under way or did not finish ({} to {}): that one is finished, put back or forgotten first",
            show(&r.source()),
            show(&r.target())
        );
        return Some((r.clone(), why));
    }
    // The names a move makes beside the folder and the copy (R-SPACE-MV3-6).
    open().find(|r| [r.aside(), r.staging(), r.target_aside()].iter().any(|p| same_path(p, folder)) || is(r.staging_ident.as_ref())).map(|r| {
        let why = format!(
            "It is a folder another move made and has not finished ({} to {}): that one is finished, put back or forgotten first",
            show(&r.source()),
            show(&r.target())
        );
        (r.clone(), why)
    })
}

/// The system-folder lists as given and as Windows ends up at them (asks the disk: the plan's
/// thread only).
#[derive(Clone, Debug, Default)]
pub struct Resolved {
    pub sealed: Vec<PathBuf>,
    pub roots: Vec<PathBuf>,
    pub users: Vec<PathBuf>,
    pub me: Vec<PathBuf>,
    pub own: Vec<PathBuf>,
    pub onedrive: Vec<PathBuf>,
    pub windows_drive: Option<String>,
}

impl Env {
    pub fn resolved_by(&self, fs: &dyn Fs) -> Resolved {
        let opt = |p: &Option<PathBuf>| p.iter().cloned().collect::<Vec<_>>();
        Resolved {
            sealed: Env::resolved(fs, &self.sealed),
            roots: Env::resolved(fs, &self.roots),
            users: Env::resolved(fs, &opt(&self.users)),
            me: Env::resolved(fs, &opt(&self.me)),
            own: Env::resolved(fs, &self.own),
            onedrive: Env::resolved(fs, &self.onedrive),
            windows_drive: self.windows_drive.clone(),
        }
    }
}

/// Why the folder at `real` is never moved, if it is not to be. `dest`: `real` is where a
/// move's copy would go, said as the destination (RIN3-11: a copy there could never be moved
/// back).
pub fn system_refusals(r: &Resolved, real: &Path, dest: bool) -> Vec<String> {
    let (it, is) = if dest { ("The destination", "The destination is") } else { ("It", "It is") };
    let never = if dest { "a copy there could never be moved back" } else { "never moved" };
    let mut out = Vec::new();
    // Windows' own folders at a drive's root: some on any drive, the rest on the Windows
    // drive only (RIN3-10).
    let s = real.to_string_lossy();
    if let Some(first) = s.get(3..).and_then(|rest| rest.split('\\').next())
        && s.as_bytes().get(1) == Some(&b':')
        && (DRIVE_SYSTEM.iter().any(|n| n.eq_ignore_ascii_case(first))
            || (r.windows_drive.as_deref().is_some_and(|d| s.get(..2).is_some_and(|x| x.eq_ignore_ascii_case(d))) && WINDOWS_DRIVE_SYSTEM.iter().any(|n| n.eq_ignore_ascii_case(first))))
    {
        out.push(format!("{is} in {}, a folder Windows keeps for itself: {never}", show(Path::new(&s[..3 + first.len()]))));
    }
    for x in &r.sealed {
        if within(real, x) {
            out.push(format!("{is} in {}, a system folder: {never}", show(x)));
        } else if within(x, real) {
            out.push(format!("{it} holds {}, a system folder", show(x)));
        }
    }
    for x in &r.roots {
        if same_path(real, x) {
            out.push(if dest { format!("The destination is {}, a system folder: {never}", show(x)) } else { format!("{} is a system folder: never moved (a folder inside it can be)", show(x)) });
        } else if within(x, real) {
            out.push(format!("{it} holds {}, a system folder", show(x)));
        }
    }
    // Another account's profile: the folder of profiles, minus this account's own. Directly in
    // the folder of profiles, it may be no account's (RIN3-11).
    if let Some(u) = r.users.iter().find(|u| within(real, u) && !same_path(real, u))
        && !r.me.iter().any(|m| within(real, m))
    {
        out.push(if real.parent().is_some_and(|p| same_path(p, u)) { format!("{is} in {}, the folder of every account's profile: {never}", show(u)) } else { format!("{is} in another account's profile: {never}") });
    }
    if r.own.iter().any(|o| within(o, real)) {
        out.push(format!("{it} holds VibeSpace itself or its own data"));
    }
    if let Some(o) = r.onedrive.iter().find(|o| within(real, o) || within(o, real)) {
        out.push(format!("{is} a OneDrive folder or holds one ({}): OneDrive keeps it in step itself", show(o)));
    }
    out
}

/// RIN3-9: a place as given and as Windows ends up at it: the path itself resolved (through a
/// junction, a mount point, an 8.3 name), and its folder resolved with its name added (a path
/// not made yet, or a link, which leads somewhere else than its own place).
fn forms(fs: &dyn Fs, p: &Path) -> Vec<PathBuf> {
    let mut v = vec![p.to_path_buf()];
    let resolved = [fs.final_path(p).ok(), p.parent().zip(p.file_name()).and_then(|(d, n)| fs.final_path(d).ok().map(|d| d.join(n)))];
    for f in resolved.into_iter().flatten() {
        if !v.iter().any(|x| same_path(x, &f)) {
            v.push(f);
        }
    }
    v
}

/// RIN3-9: the one check of a place the plan, Move back and recovery all make: refused when any
/// of its forms is (`forms`). Asks the disk.
pub fn place_refusals(fs: &dyn Fs, r: &Resolved, p: &Path, dest: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in forms(fs, p) {
        for why in system_refusals(r, &f, dest) {
            if !out.contains(&why) {
                out.push(why);
            }
        }
    }
    out
}

/// IN-VibeSpace-7, RIN3-9: what a journal names (the copy and the folder moved) passes the check
/// a move is planned by, in every form, before any step of a Move back or a recovery: a journal
/// another program wrote never leads VibeSpace into a system folder. And its copy is where the
/// plan checked it would be, as Windows ends up at it (a journal from before this check names
/// none). None: all pass. Asks the disk.
pub fn journal_refusal(fs: &dyn Fs, env: &Env, rec: &Record) -> Option<String> {
    let r = env.resolved_by(fs);
    for (what, p) in [("The copy", rec.target()), ("The folder", rec.source())] {
        if let Some(why) = place_refusals(fs, &r, &p, false).first() {
            return Some(format!("{what} its journal names ({}) is not one VibeSpace moves: {}", show(&p), run::lower(why)));
        }
    }
    let target = rec.target();
    if !rec.target_real.is_empty()
        && let Some(now) = target.parent().zip(target.file_name()).and_then(|(d, n)| fs.final_path(d).ok().map(|d| d.join(n)))
        && !same_path(&now, Path::new(&rec.target_real))
    {
        return Some(format!("The copy its journal names ({}) is not where it was moved ({}): Windows now ends up at {}", show(&target), show(Path::new(&rec.target_real)), show(&now)));
    }
    None
}

/// The plan of moving a moved folder back. Changes nothing.
pub fn plan_back(fs: &dyn Fs, env: &Env, rec: &Record, stop: &dyn Fn() -> bool) -> Plan {
    let (link, copy) = (rec.source(), rec.target());
    let mut p = Plan { back: true, from: copy.clone(), to: link.clone(), link: link.clone(), ..Plan::default() };
    if let Some(why) = journal_refusal(fs, env, rec) {
        p.refusals.push(why);
        return p;
    }
    match fs.junction_target(&link) {
        Ok(Some(t)) if same_path(&t, &copy) => {}
        Ok(_) => {
            p.refusals.push(format!("{} no longer leads to {}: VibeSpace leaves it as it is", show(&link), show(&copy)));
            return p;
        }
        Err(e) => {
            p.refusals.push(format!("{} could not be read ({})", show(&link), win::describe(&e)));
            return p;
        }
    }
    match fs.item(&copy, false) {
        Ok(i) if rec.target_ident.as_ref().is_some_and(|t| t.same(&i.ident)) => p.from_ident = i.ident,
        Ok(_) => {
            p.refusals.push(format!("{} is not the copy VibeSpace made", show(&copy)));
            return p;
        }
        Err(e) => {
            p.refusals.push(format!("{} could not be read ({})", show(&copy), win::describe(&e)));
            return p;
        }
    }
    for x in [rec.staging(), rec.target_aside()] {
        match fs.item(&x, false) {
            Err(e) if win::is_gone(&e) => {}
            _ => p.refusals.push(format!("{} is in the way", show(&x))),
        }
    }
    match fs.volume(&copy) {
        Ok(v) => p.from_root = v.root,
        Err(e) => p.refusals.push(format!("The drive of {} could not be asked ({})", show(&copy), win::describe(&e))),
    }
    if let Err(e) = fs.may_rename(&copy) {
        p.refusals.push(rename_refusal(&e));
    }
    let parent = link.parent().map(Path::to_path_buf).unwrap_or_default();
    if let Err(e) = fs.may_add(&parent) {
        p.refusals.push(format!("VibeSpace may not make a folder in {} ({})", show(&parent), win::describe(&e)));
    }
    let to_vol = match fs.volume(&parent) {
        Ok(v) => Some(v),
        Err(e) => {
            p.refusals.push(format!("The drive of {} could not be asked ({})", show(&parent), win::describe(&e)));
            None
        }
    };
    match walk(fs, &copy, stop) {
        Ok(w) => take_walk(&mut p, w),
        Err(e) => {
            p.refusals.push(format!("Not everything in it could be read: {e}"));
            return p;
        }
    }
    if let Some(v) = &to_vol {
        p.to_root = v.root.clone();
        p.to_fs = v.fs.clone();
        p.free = Some(v.free);
        p.need = need(&p.items, v.cluster);
        if p.need.saturating_add(HEADROOM) > v.free {
            p.refusals.push(format!("Not enough room on {}: the folder takes {}, {} is free", v.root.trim_end_matches('\\'), crate::format::bytes(p.need), crate::format::bytes(v.free)));
        }
    }
    if p.refusals.is_empty() {
        let items = std::mem::take(&mut p.items);
        holders(fs, &copy, &items, stop, &mut p);
        p.items = items;
    }
    advise(&mut p);
    p
}
