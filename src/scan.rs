//! Scanning without admin rights. WizTree reads the NTFS master file table (admin only);
//! VibeSpace lists directories in bulk instead: `GetFileInformationByHandleEx` with
//! `FileIdExtdDirectoryInfo` (or `FileIdBothDirectoryInfo` where a file system lacks it) hands
//! back a buffer of entries per call, each with its logical size, allocation, attributes, times,
//! reparse tag and file id. No file is ever opened, only folders; a file whose listing hides its
//! room (WOF compression, `hides_its_size`) gets one metadata query that reads no data.
//!
//! Eight workers list folders from one stack; one builder owns the tree's writes: it takes the
//! listings in batches, adds them under one write lock, and hands the subfolders back to the
//! workers. Name surrogates (links, junctions, mount points) are never followed. Cancel stops at
//! once: the builder lets go, the workers drop what they hold.

use crate::tree::{Entry, Seen, Tree};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const THREADS: usize = 8;
const BUF: usize = 64 * 1024;

/// What the scan tells the window.
#[derive(Clone, Debug)]
pub enum Event {
    /// The tree grew (at most every 100 ms).
    Progress,
    Done(Done),
}

#[derive(Clone, Debug, Default)]
pub struct Done {
    pub elapsed: Duration,
    pub cancelled: bool,
    /// The scanned folder itself could not be read (nothing under it is known).
    pub root_error: Option<String>,
    /// Folders it was asked to read that are gone: not found, their own folder still
    /// answering. Any other failure is "could not be read", never gone.
    pub gone: Vec<u32>,
    pub volume: Volume,
}

/// The drive a scan is on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Volume {
    /// `NTFS`, `ReFS`, `FAT32`, `exFAT`, ... (empty: unknown).
    pub fs: String,
    pub remote: bool,
    /// A fixed drive: the only kind with a Recycle Bin the shell uses.
    pub fixed: bool,
    pub free: Option<u64>,
    pub total: Option<u64>,
    /// The root of the drive (`C:\`): a scan of exactly it can show progress against the
    /// drive's used space.
    pub root: String,
    /// The scanned folder's volume serial (FILE_ID_INFO; 0: unknown): a recycle checks its
    /// items are still on this volume (a disk swapped under the same letter is not). Read as
    /// the walk reads the folder: through a link at the root (R2-SPACE-1).
    pub serial: u64,
    /// The scanned folder's own file id, read the same way (0: unknown).
    pub id: u64,
}

/// A scan under way.
pub struct Scan {
    pub cancel: Arc<AtomicBool>,
    /// The folder a worker started on last (`\\?\` stripped).
    pub current: Arc<Mutex<String>>,
    pub started: Instant,
    pub volume: Arc<Mutex<Volume>>,
}

impl Scan {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn current(&self) -> String {
        self.current.lock().unwrap().clone()
    }
}

/// A path as the app keeps it: absolute, `..` resolved, no trailing separator (a drive root
/// keeps its own: `C:\`).
pub fn normalize(path: &Path) -> PathBuf {
    // `C:` alone is "the current folder on C:" to Windows; typed, it means the drive.
    let raw = path.as_os_str().to_string_lossy();
    let b = raw.as_bytes();
    if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return PathBuf::from(format!("{}\\", raw.to_ascii_uppercase()));
    }
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let s = abs.as_os_str().to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix("\\\\?\\UNC\\").map(|r| format!("\\\\{r}")).unwrap_or(s);
    let s = s.strip_prefix("\\\\?\\").map(str::to_string).unwrap_or(s);
    let t = s.trim_end_matches('\\');
    if t.len() == 2 && t.ends_with(':') {
        return PathBuf::from(format!("{}\\", t.to_ascii_uppercase()));
    }
    // Keep the exact units when nothing had to change (a name Rust cannot spell).
    if t == abs.as_os_str().to_string_lossy() { abs } else { PathBuf::from(t) }
}

/// UTF-16 path to its `\\?\` form (long paths, nothing reinterpreted on the way).
pub fn long(units: &[u16]) -> Vec<u16> {
    let bs = b'\\' as u16;
    let s: Vec<u16> = units.iter().map(|&c| if c == b'/' as u16 { bs } else { c }).collect();
    let pre = |p: &str| p.encode_utf16().collect::<Vec<u16>>();
    if s.starts_with(&pre("\\\\?\\")) {
        s
    } else if s.starts_with(&pre("\\\\")) {
        let mut v = pre("\\\\?\\UNC\\");
        v.extend_from_slice(&s[2..]);
        v
    } else {
        let mut v = pre("\\\\?\\");
        v.extend_from_slice(&s);
        v
    }
}

fn with_nul(v: &[u16]) -> Vec<u16> {
    let mut w = v.to_vec();
    w.push(0);
    w
}

fn shown(long: &[u16]) -> String {
    let s = String::from_utf16_lossy(long);
    let s = s.strip_prefix("\\\\?\\UNC\\").map(|r| format!("\\\\{r}")).unwrap_or(s);
    crate::tree::clean(s.strip_prefix("\\\\?\\").unwrap_or(&s))
}

/// Windows' words for an error code, without the "(os error 5)".
pub fn error_text(code: u32) -> String {
    let s = std::io::Error::from_raw_os_error(code as i32).to_string();
    let s = match s.find(" (os error") {
        Some(p) => s[..p].to_string(),
        None => s,
    };
    s.trim_end_matches('.').to_string()
}

// ---------- the listing ----------

/// One folder's entries: (entry, start, len) into `names` (UTF-16).
#[derive(Default)]
pub struct Listing {
    pub entries: Vec<(Entry, u32, u16)>,
    pub names: Vec<u16>,
    pub error: Option<u32>,
}

const CLASS_EXTD: i32 = 19; // FileIdExtdDirectoryInfo
const CLASS_BOTH: i32 = 10; // FileIdBothDirectoryInfo

/// Which class a scan lists with: 0 not decided, 1 Extd works, 2 fall back to Both. One per
/// scan (its volume), never for the process: a FAT stick's fallback must not give a later
/// ReFS scan 64-bit ids.
pub type Mode = AtomicU32;

pub fn fold_id(b: &[u8; 16]) -> u64 {
    let lo = u64::from_le_bytes(b[..8].try_into().unwrap());
    let hi = u64::from_le_bytes(b[8..].try_into().unwrap());
    if hi == 0 {
        lo
    } else {
        let h = lo ^ hi.rotate_left(31).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if h == 0 || h == u64::MAX { 1 } else { h }
    }
}

/// List a folder (its `\\?\` path, no NUL) in bulk. Stops early on `cancel`. `mode` is the
/// scan's own (see `Mode`).
pub fn list(path: &[u16], cancel: &AtomicBool, mode: &Mode) -> Listing {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    let mut out = Listing::default();
    let p = with_nul(path);
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        out.error = Some(unsafe { GetLastError() });
        return out;
    }
    let mut buf = vec![0u64; BUF / 8];
    let mut class = if mode.load(Ordering::Relaxed) == 2 { CLASS_BOTH } else { CLASS_EXTD };
    let mut first = true;
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let ok = unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr().cast(), BUF as u32) } != 0;
        if !ok {
            let e = unsafe { GetLastError() };
            if e == 18 {
                break; // ERROR_NO_MORE_FILES
            }
            // A file system without the extended class (FAT, some shares): the older one,
            // remembered once it works (a handle that is no folder at all fails both).
            if first && class == CLASS_EXTD && matches!(e, 87 | 50 | 124 | 1) {
                class = CLASS_BOTH;
                continue;
            }
            out.error = Some(e);
            break;
        }
        if first {
            mode.store(if class == CLASS_EXTD { 1 } else { 2 }, Ordering::Relaxed);
        }
        first = false;
        if !parse(&buf, class, &mut out) {
            break;
        }
    }
    unsafe { CloseHandle(h) };
    true_sizes(path, &mut out);
    out
}

/// A file the listing says takes no room although it is bigger than any file NTFS keeps inside
/// its own record (~700 bytes): WOF-compressed (CompactOS, `compact /exe`, CompactGUI). wof.sys
/// hides the reparse point and reports 0 allocated in the listing (X15WFS-2, proved). Never a
/// folder, a link, a sparse file or a cloud placeholder (those are 0 on purpose).
pub fn hides_its_size(e: &Entry) -> bool {
    use crate::tree::{ATTR_DIRECTORY, ATTR_OFFLINE, ATTR_RECALL_ON_DATA_ACCESS, ATTR_RECALL_ON_OPEN, ATTR_SPARSE};
    e.alloc == 0 && e.size > 4096 && e.attrs & (ATTR_DIRECTORY | ATTR_SPARSE | ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) == 0 && !e.is_link()
}

/// The real room of every file that hides it (`hides_its_size`): `GetCompressedFileSizeW`, a
/// metadata query that reads no data (so it never downloads anything). Such a file is shown
/// "compressed", as it is. A file whose size cannot be asked keeps the listing's 0.
fn true_sizes(dir: &[u16], out: &mut Listing) {
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    for (e, start, len) in out.entries.iter_mut() {
        if !hides_its_size(e) {
            continue;
        }
        let mut p = dir.to_vec();
        if p.last() != Some(&(b'\\' as u16)) {
            p.push(b'\\' as u16);
        }
        p.extend_from_slice(&out.names[*start as usize..*start as usize + *len as usize]);
        p.push(0);
        let mut high = 0u32;
        let low = unsafe { GetCompressedFileSizeW(p.as_ptr(), &mut high) };
        if low == u32::MAX && unsafe { windows_sys::Win32::Foundation::GetLastError() } != 0 {
            continue;
        }
        e.alloc = (u64::from(high) << 32) | u64::from(low);
        e.attrs |= crate::tree::ATTR_COMPRESSED;
    }
}

/// One buffer of a listing into `out`. Every entry is checked against the buffer before it
/// is read (a buggy file system driver must not make the app read past it or crash): one that
/// does not fit sets ERROR_INVALID_DATA (the folder "could not be read in full") and stops.
fn parse(buf: &[u64], class: i32, out: &mut Listing) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO};
    let size = buf.len() * 8;
    let (whole, header) = if class == CLASS_EXTD {
        (std::mem::size_of::<FILE_ID_EXTD_DIR_INFO>(), std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName))
    } else {
        (std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>(), std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName))
    };
    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    loop {
        // Entries are 8-byte aligned and the struct must fit whole before it is referenced.
        if !off.is_multiple_of(8) || off + whole > size {
            out.error = Some(13); // ERROR_INVALID_DATA
            return false;
        }
        let (next, entry, name_bytes) = unsafe {
            if class == CLASS_EXTD {
                let d = &*(base.add(off) as *const FILE_ID_EXTD_DIR_INFO);
                let e = Entry {
                    attrs: d.FileAttributes,
                    tag: d.ReparsePointTag,
                    size: d.EndOfFile.max(0) as u64,
                    alloc: d.AllocationSize.max(0) as u64,
                    modified: d.LastWriteTime,
                    id: fold_id(&d.FileId.Identifier),
                    changed: d.ChangeTime,
                };
                (d.NextEntryOffset, e, d.FileNameLength as usize)
            } else {
                let d = &*(base.add(off) as *const FILE_ID_BOTH_DIR_INFO);
                let e = Entry {
                    attrs: d.FileAttributes,
                    // For a reparse point this field holds its tag (as FindFirstFile reads it).
                    tag: if d.FileAttributes & crate::tree::ATTR_REPARSE != 0 { d.EaSize } else { 0 },
                    size: d.EndOfFile.max(0) as u64,
                    alloc: d.AllocationSize.max(0) as u64,
                    modified: d.LastWriteTime,
                    id: d.FileId as u64,
                    changed: d.ChangeTime,
                };
                (d.NextEntryOffset, e, d.FileNameLength as usize)
            }
        };
        if off + header + name_bytes > size || (next != 0 && (next as usize) < header) {
            out.error = Some(13);
            return false;
        }
        let name = unsafe { std::slice::from_raw_parts(base.add(off + header) as *const u16, name_bytes / 2) };
        let dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
        if !dot && !name.is_empty() {
            let start = out.names.len() as u32;
            let len = name.len().min(u16::MAX as usize);
            out.names.extend_from_slice(&name[..len]);
            out.entries.push((entry, start, len as u16));
        }
        if next == 0 {
            return true;
        }
        off += next as usize;
    }
}

// ---------- the volume ----------

/// The drive under `path`: its file system, whether it is a network drive, its free space.
pub fn volume(path: &Path) -> Volume {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW};
    let p = crate::host::wide_path(path);
    let mut root = vec![0u16; 1024];
    let mut v = Volume::default();
    unsafe {
        if GetVolumePathNameW(p.as_ptr(), root.as_mut_ptr(), root.len() as u32) != 0 {
            let len = root.iter().position(|&c| c == 0).unwrap_or(0);
            root.truncate(len + 1);
            v.root = String::from_utf16_lossy(&root[..len]);
            let kind = GetDriveTypeW(root.as_ptr());
            v.remote = kind == 4; // DRIVE_REMOTE
            v.fixed = kind == 3; // DRIVE_FIXED
            let mut fs = [0u16; 64];
            if GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), fs.as_mut_ptr(), fs.len() as u32) != 0 {
                let n = fs.iter().position(|&c| c == 0).unwrap_or(0);
                v.fs = String::from_utf16_lossy(&fs[..n]);
            }
        }
        if path.as_os_str().to_string_lossy().starts_with("\\\\") {
            v.remote = true;
            v.fixed = false;
        }
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        if GetDiskFreeSpaceExW(p.as_ptr(), &mut avail, &mut total, &mut free) != 0 {
            v.free = Some(avail);
            v.total = Some(total);
        }
    }
    v
}

/// Hard links are counted once on local NTFS and ReFS. Elsewhere ids are not trusted: a
/// network share may join file systems whose ids collide, and counting a file twice is the
/// smaller lie than hiding one.
pub fn dedupe_for(v: &Volume) -> Seen {
    let local = !v.remote;
    Seen::new(local && (v.fs.eq_ignore_ascii_case("NTFS") || v.fs.eq_ignore_ascii_case("ReFS")), v.fs.eq_ignore_ascii_case("NTFS"))
}

fn modified_of(path: &[u16]) -> Option<i64> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA};
    let p = with_nul(path);
    let mut d: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    if unsafe { GetFileAttributesExW(p.as_ptr(), GetFileExInfoStandard, (&mut d as *mut WIN32_FILE_ATTRIBUTE_DATA).cast()) } == 0 {
        return None;
    }
    if d.dwFileAttributes & crate::tree::ATTR_DIRECTORY == 0 {
        return None;
    }
    Some(((d.ftLastWriteTime.dwHighDateTime as i64) << 32) | d.ftLastWriteTime.dwLowDateTime as i64)
}

/// What a path names right now, read through a handle to the item itself (a link is not
/// followed): what the Recycle Bin checks right before it acts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ident {
    /// FILE_ID_INFO's volume serial and the file id folded as the scan folds it (0: unknown).
    pub serial: u64,
    pub id: u64,
    /// Last write, unix seconds.
    pub modified: u32,
    pub size: u64,
    pub dir: bool,
    /// A folder that leads to a drive (a volume mounted there, or a link to a drive's root),
    /// or a folder link whose target could not be read: never a folder to remove.
    pub drive: bool,
    /// Online only now (a cloud placeholder whose data is not on this disk): never sent, as
    /// the scan never offers one (FS4).
    pub online: bool,
    /// How many names the file has (hard links; 1 for most, 0: unknown).
    pub links: u32,
}

/// Read `path` (`\\?\`, no NUL) as it is now. Err: the Windows error opening it.
pub fn ident(path: &[u16]) -> Result<Ident, u32> {
    ident_as(path, false)
}

/// How many names (hard links) the file at `path` (`\\?\`, no NUL) has: one handle for its
/// attributes and one query, the least that tells (X15R2BC-2). 0: unknown.
pub fn links(path: &[u16]) -> u32 {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, GetFileInformationByHandleEx,
        OPEN_EXISTING,
    };
    let p = with_nul(path);
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return 0;
    }
    let mut s: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandleEx(h, 1, (&mut s as *mut FILE_STANDARD_INFO).cast(), std::mem::size_of::<FILE_STANDARD_INFO>() as u32) } != 0; // FileStandardInfo
    unsafe { CloseHandle(h) };
    if ok { s.NumberOfLinks } else { 0 }
}

/// Whether `path` is the root of its volume (a drive, or a volume mounted in a folder): a scan of
/// it holds every name a file on that volume can have, but for folders it could not read.
pub fn volume_root(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let p: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut out = vec![0u16; p.len() + 64];
    if unsafe { GetVolumePathNameW(p.as_ptr(), out.as_mut_ptr(), out.len() as u32) } == 0 {
        return false;
    }
    let end = out.iter().position(|&c| c == 0).unwrap_or(out.len());
    let trim = |s: &str| s.trim_end_matches('\\').to_lowercase();
    trim(&String::from_utf16_lossy(&out[..end])) == trim(&path.to_string_lossy())
}

/// The scanned folder as the walk reads it: a junction, symbolic link or mount point at the
/// root is followed (the walk lists its target), so its serial is the target's volume, the one
/// every item under it is on (R2-SPACE-1: the link's own serial is its parent volume's).
pub fn root_ident(path: &[u16]) -> Result<Ident, u32> {
    ident_as(path, true)
}

fn ident_as(path: &[u16], follow: bool) -> Result<Ident, u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_STANDARD_INFO, GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    let p = with_nul(path);
    let h = unsafe {
        CreateFileW(
            p.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT },
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(unsafe { GetLastError() });
    }
    unsafe fn info<T>(h: windows_sys::Win32::Foundation::HANDLE, class: i32) -> Option<T> {
        let mut v: T = unsafe { std::mem::zeroed() };
        (unsafe { GetFileInformationByHandleEx(h, class, (&mut v as *mut T).cast(), std::mem::size_of::<T>() as u32) } != 0).then_some(v)
    }
    let out = unsafe {
        let basic: Option<FILE_BASIC_INFO> = info(h, 0); // FileBasicInfo
        match basic {
            None => Err(GetLastError()),
            Some(b) => {
                let std: Option<FILE_STANDARD_INFO> = info(h, 1); // FileStandardInfo
                let id: Option<FILE_ID_INFO> = info(h, 18); // FileIdInfo (not on every file system)
                let tag: Option<FILE_ATTRIBUTE_TAG_INFO> = info(h, 9); // FileAttributeTagInfo
                let dir = b.FileAttributes & crate::tree::ATTR_DIRECTORY != 0;
                let link = b.FileAttributes & crate::tree::ATTR_REPARSE != 0 && tag.is_some_and(|t| t.ReparseTag == 0xA000_0003 || t.ReparseTag == 0xA000_000C);
                Ok(Ident {
                    serial: id.map_or(0, |i| i.VolumeSerialNumber),
                    id: id.map_or(0, |i| fold_id(&i.FileId.Identifier)),
                    modified: crate::tree::unix_secs(b.LastWriteTime),
                    size: std.map_or(0, |s| s.EndOfFile.max(0) as u64),
                    dir,
                    drive: dir && link && link_target(h).is_none_or(|t| leads_to_drive(&t)),
                    online: crate::tree::Entry { attrs: b.FileAttributes, ..Default::default() }.online_only(),
                    links: std.map_or(0, |s| s.NumberOfLinks),
                })
            }
        }
    };
    unsafe { CloseHandle(h) };
    out
}

/// Where a junction, mount point or symbolic link (its handle) leads: the substitute name.
fn link_target(h: windows_sys::Win32::Foundation::HANDLE) -> Option<Vec<u16>> {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
    let mut buf = vec![0u8; 16 * 1024];
    let mut got = 0u32;
    let ok = unsafe { DeviceIoControl(h, FSCTL_GET_REPARSE_POINT, std::ptr::null(), 0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut got, std::ptr::null_mut()) } != 0;
    if !ok {
        return None;
    }
    let buf = &buf[..(got as usize).min(buf.len())];
    let u16_at = |o: usize| buf.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let tag = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?);
    // Mount points and junctions: the names follow the four offsets; symbolic links: a flags
    // field first.
    let names = if tag == 0xA000_0003 { 16 } else { 20 };
    let (off, len) = (u16_at(8)?, u16_at(10)?);
    let b = buf.get(names + off..names + off + len)?;
    Some(b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect())
}

/// Whether a link's substitute name is a whole drive: a volume (`\??\Volume{...}\`, a mount
/// point) or a drive's root (`\??\D:\`).
pub fn leads_to_drive(target: &[u16]) -> bool {
    let s = String::from_utf16_lossy(target);
    let rest = s.strip_prefix(r"\??\").or_else(|| s.strip_prefix(r"\\?\")).unwrap_or(&s);
    if rest.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("Volume{")) {
        return true;
    }
    let b = rest.trim_end_matches('\\').as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

// ---------- the walk ----------

struct Queue {
    jobs: Vec<(u32, Vec<u16>)>,
    closed: bool,
}

struct Work {
    q: Mutex<Queue>,
    cv: Condvar,
}

/// Scan a new folder: a tree with its root pending, filled in on other threads.
pub fn scan(root: &Path, notify: Arc<dyn Fn(Event) + Send + Sync>) -> (Arc<RwLock<Tree>>, Scan) {
    let root = normalize(root);
    let tree = Arc::new(RwLock::new(Tree::new(root.clone(), Seen::new(false, false))));
    let s = run(tree.clone(), vec![0], true, notify);
    (tree, s)
}

/// Scan again under `dirs` of an existing tree (each emptied first, then listed afresh).
pub fn rescan(tree: Arc<RwLock<Tree>>, dirs: Vec<u32>, notify: Arc<dyn Fn(Event) + Send + Sync>) -> Scan {
    tree.write().unwrap().reset(&dirs);
    run(tree, dirs, false, notify)
}

fn run(tree: Arc<RwLock<Tree>>, starts: Vec<u32>, fresh: bool, notify: Arc<dyn Fn(Event) + Send + Sync>) -> Scan {
    let cancel = Arc::new(AtomicBool::new(false));
    let current = Arc::new(Mutex::new(String::new()));
    let volume = Arc::new(Mutex::new(Volume::default()));
    let scan = Scan { cancel: cancel.clone(), current: current.clone(), started: Instant::now(), volume: volume.clone() };
    let started = scan.started;
    std::thread::spawn(move || {
        let mut root_path = tree.read().unwrap().root_path.clone();
        // A file given as what to scan (typed, dropped, on the command line) stands for its
        // folder. Asked here, off the UI thread: on an unreachable share this waits ~21 s.
        if fresh
            && std::fs::metadata(&root_path).is_ok_and(|m| m.is_file())
            && let Some(parent) = root_path.parent()
        {
            root_path = normalize(parent);
            *tree.write().unwrap() = Tree::new(root_path.clone(), Seen::new(false, false));
        }
        let mut vol = self::volume(&root_path);
        // The path copied out first: no lock is held while a share that went away keeps
        // CreateFileW waiting (~21 s), so a rescan's `tree.write()` on the UI thread never
        // waits for it (X15CON-5).
        let root_units = long(&tree.read().unwrap().path_units(0));
        if let Ok(i) = root_ident(&root_units) {
            (vol.serial, vol.id) = (i.serial, i.id);
        }
        *volume.lock().unwrap() = vol.clone();
        let start_paths: Vec<(u32, Vec<u16>)> = {
            let mut t = tree.write().unwrap();
            if fresh {
                t.seen = dedupe_for(&vol);
            }
            starts.iter().map(|&d| (d, long(&t.path_units(d)))).collect()
        };
        for (d, p) in &start_paths {
            if let Some(ft) = modified_of(p) {
                tree.write().unwrap().set_modified(*d, ft);
            }
        }
        let work = Arc::new(Work { q: Mutex::new(Queue { jobs: Vec::new(), closed: false }), cv: Condvar::new() });
        let (tx, rx) = channel::<(u32, Vec<u16>, Listing)>();
        let mode = Arc::new(Mode::new(0));
        for _ in 0..THREADS {
            let (work, tx, cancel, current, mode) = (work.clone(), tx.clone(), cancel.clone(), current.clone(), mode.clone());
            std::thread::spawn(move || worker(&work, &tx, &cancel, &current, &mode));
        }
        drop(tx);
        let mut outstanding = start_paths.len();
        {
            let mut q = work.q.lock().unwrap();
            q.jobs.extend(start_paths.iter().cloned());
        }
        work.cv.notify_all();
        let (root_error, gone) = build(&tree, &work, &rx, &cancel, &mut outstanding, &start_paths, &notify);
        {
            let mut q = work.q.lock().unwrap();
            q.closed = true;
            q.jobs.clear();
        }
        work.cv.notify_all();
        let cancelled = cancel.load(Ordering::Relaxed) && outstanding > 0;
        if fresh && !cancelled {
            tree.write().unwrap().shrink();
        }
        // Free space as it is now (the scan may have taken a while).
        let after = self::volume(&root_path);
        vol.free = after.free.or(vol.free);
        vol.total = after.total.or(vol.total);
        *volume.lock().unwrap() = vol.clone();
        notify(Event::Done(Done { elapsed: started.elapsed(), cancelled, root_error, gone, volume: vol }));
    });
    scan
}

fn worker(work: &Work, tx: &Sender<(u32, Vec<u16>, Listing)>, cancel: &AtomicBool, current: &Mutex<String>, mode: &Mode) {
    loop {
        let job = {
            let mut q = work.q.lock().unwrap();
            loop {
                if q.closed {
                    return;
                }
                if let Some(j) = q.jobs.pop() {
                    break j;
                }
                q = work.cv.wait(q).unwrap();
            }
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        *current.lock().unwrap() = shown(&job.1);
        let l = list(&job.1, cancel, mode);
        if tx.send((job.0, job.1, l)).is_err() {
            return;
        }
    }
}

/// The builder: listings in, the tree written in batches, subfolders out. Returns the root's
/// own error when the scanned folder could not be read, and the folders asked for that are
/// gone (see `Done::gone`).
fn build(
    tree: &RwLock<Tree>,
    work: &Work,
    rx: &Receiver<(u32, Vec<u16>, Listing)>,
    cancel: &AtomicBool,
    outstanding: &mut usize,
    starts: &[(u32, Vec<u16>)],
    notify: &Arc<dyn Fn(Event) + Send + Sync>,
) -> (Option<String>, Vec<u32>) {
    let mut last = Instant::now();
    let mut root_error = None;
    let mut gone = Vec::new();
    let mut batch = Vec::new();
    while *outstanding > 0 {
        if cancel.load(Ordering::Relaxed) {
            return (root_error, gone);
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(l) => batch.push(l),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return (root_error, gone),
        }
        while batch.len() < 512 {
            match rx.try_recv() {
                Ok(l) => batch.push(l),
                Err(_) => break,
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return (root_error, gone);
        }
        let mut jobs = Vec::new();
        {
            let mut t = tree.write().unwrap();
            for (d, path, l) in batch.drain(..) {
                *outstanding -= 1;
                if let Some(e) = l.error
                    && l.entries.is_empty()
                    && starts.iter().any(|s| s.0 == d)
                {
                    root_error = Some(format!("Could not read {}: {}", shown(&path), error_text(e)));
                    if matches!(e, 2 | 3) && folder_answers(parent_of(&path)) {
                        gone.push(d);
                    }
                }
                let items: Vec<(Entry, &[u16])> = l.entries.iter().map(|(e, s, n)| (*e, &l.names[*s as usize..*s as usize + *n as usize])).collect();
                let added = t.add_listing(d, &items, l.error);
                for sub in added.subdirs {
                    let mut p = path.clone();
                    if p.last() != Some(&(b'\\' as u16)) {
                        p.push(b'\\' as u16);
                    }
                    p.extend(t.name_units(sub));
                    jobs.push((sub, p));
                }
            }
        }
        if !jobs.is_empty() {
            *outstanding += jobs.len();
            work.q.lock().unwrap().jobs.extend(jobs);
            work.cv.notify_all();
        }
        if last.elapsed() >= Duration::from_millis(100) {
            last = Instant::now();
            notify(Event::Progress);
        }
    }
    (root_error, gone)
}

/// The folder a `\\?\` path is in (`\\?\C:\` for a drive's top level).
pub fn parent_of(path: &[u16]) -> &[u16] {
    let end = path.iter().rposition(|&c| c == u16::from(b'\\')).unwrap_or(0);
    // `\\?\C:\x`: the drive keeps its own separator.
    if end > 0 && path[end - 1] == u16::from(b':') { &path[..=end] } else { &path[..end] }
}

/// Whether a folder (`\\?\` path, no NUL) answers right now. One on a share that dropped, or
/// on a drive that went, does not: what was in it is unknown, not gone.
pub fn folder_answers(path: &[u16]) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    let a = unsafe { GetFileAttributesW(with_nul(path).as_ptr()) };
    a != INVALID_FILE_ATTRIBUTES && a & crate::tree::ATTR_DIRECTORY != 0
}

/// Scan to the end on this thread's behalf (tests and measurements): the tree and how it ended.
pub fn scan_blocking(root: &Path) -> (Arc<RwLock<Tree>>, Done) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let notify: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |e| {
        if let Event::Done(d) = e {
            let _ = tx.lock().unwrap().send(d);
        }
    });
    let (tree, _scan) = scan(root, notify);
    let done = rx.recv().unwrap();
    (tree, done)
}

pub fn rescan_blocking(tree: Arc<RwLock<Tree>>, dirs: Vec<u32>) -> Done {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let notify: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |e| {
        if let Event::Done(d) = e {
            let _ = tx.lock().unwrap().send(d);
        }
    });
    let _s = rescan(tree, dirs, notify);
    rx.recv().unwrap()
}
