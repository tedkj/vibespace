//! The Windows calls behind "Move to another drive": copied from the family, not shared (the
//! copy and the SHA-256 from VibeCopy's `copy.rs`/`verify.rs`, the identity from VibeSync's
//! `copy.rs`, the Restart Manager from VibePort's `locks.rs`), plus junctions
//! (`FSCTL_SET_REPARSE_POINT`, no admin) and the permissions of a folder.
//!
//! Every path handed in is plain (`C:\x`); each call reads it through its verbatim form
//! (`\\?\C:\x`): no MAX_PATH, and a name ending in a dot or a space is itself.

use std::io;
use std::path::{Path, PathBuf};

/// A path's verbatim form, NUL-terminated (`\\?\C:\x`, `\\?\UNC\srv\share\x`).
pub fn verbatim(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<u16> = p.as_os_str().encode_wide().collect();
    let mut v = crate::scan::long(&units);
    v.push(0);
    v
}

/// NUL-terminated, exactly as given.
fn plain(p: &Path) -> Vec<u16> {
    crate::host::wide_path(p)
}

pub const ERROR_FILE_NOT_FOUND: i32 = 2;
pub const ERROR_PATH_NOT_FOUND: i32 = 3;
pub const ERROR_ACCESS_DENIED: i32 = 5;
pub const ERROR_SHARING_VIOLATION: i32 = 32;
pub const ERROR_ALREADY_EXISTS: i32 = 183;
pub const ERROR_REQUEST_ABORTED: i32 = 1235;

/// Not there (and nothing else wrong with asking).
pub fn is_gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND))
}

/// A reason people can read, without the "(os error 32)".
pub fn describe(e: &io::Error) -> String {
    let s = match e.raw_os_error().unwrap_or(-1) {
        ERROR_FILE_NOT_FOUND => "not found",
        ERROR_PATH_NOT_FOUND => "the folder is not there",
        ERROR_ACCESS_DENIED => "access denied",
        ERROR_SHARING_VIOLATION | 33 => "in use by another program",
        39 | 112 => "the disk is full",
        1295 => "over the disk quota",
        15 | 21 | 53 | 55 | 64 | 67 | 1167 => "the drive went away",
        80 | ERROR_ALREADY_EXISTS => "something of that name is already there",
        206 => "the name is too long for the destination",
        123 => "the destination does not accept this name",
        23 => "the disk could not read it (a bad sector?)",
        145 => "the folder is not empty",
        6000 => "the destination cannot hold an encrypted (EFS) file",
        ERROR_REQUEST_ABORTED => "cancelled",
        _ => "",
    };
    if !s.is_empty() {
        return s.to_string();
    }
    let t = e.to_string();
    let t = match t.find(" (os error") {
        Some(i) => t[..i].trim_end_matches('.').to_string(),
        None => t,
    };
    let mut c = t.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => t,
    }
}

// ---------- identity ----------

/// Which file or folder a path is: the volume's full serial and the 128-bit file id
/// (`FileIdInfo`; else the 32-bit serial and 64-bit index), its creation time, and the volume's
/// GUID name where Windows has one. VibeSync's rules: a zero serial or id is unknown, never
/// "the same"; a clone keeps the serial, not the GUID name. Where only the short index is known
/// (FAT, exFAT) the creation time stands in for it (see `same`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub volume: u64,
    pub file: [u8; 16],
    pub full: bool,
    pub created: i64,
    pub guid: Option<String>,
}

impl Identity {
    pub fn same(&self, o: &Identity) -> bool {
        if self.volume == 0 || o.volume == 0 {
            return false;
        }
        let ids = if self.full && o.full {
            self.volume == o.volume && self.file == o.file && self.file != [0; 16]
        } else {
            // FAT and exFAT: the index is where the entry sits, and a rename to a longer name
            // moves it (measured on exFAT): the creation time, which a rename keeps, stands in.
            self.volume as u32 == o.volume as u32 && self.created != 0 && self.created == o.created
        };
        let created = self.created == 0 || o.created == 0 || self.created == o.created;
        let guid = match (&self.guid, &o.guid) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => true,
        };
        ids && created && guid
    }

    /// On the volume `o` is on (RS2-1; moved here to be tested on its own, RS3-2): the serial
    /// (full, or the low 32 bits where either is short, as `same`) and the GUID names where
    /// both are known (a clone keeps the serial, not the GUID name); a zero serial is unknown.
    pub fn same_volume(&self, o: &Identity) -> bool {
        let serial = self.volume != 0 && o.volume != 0 && if self.full && o.full { self.volume == o.volume } else { self.volume as u32 == o.volume as u32 };
        let guid = match (&self.guid, &o.guid) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            _ => true,
        };
        serial && guid
    }
}

/// What a path names right now. `follow`: through a link at the path (else the link itself).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    pub ident: Identity,
    pub dir: bool,
    /// The reparse tag when it is a reparse point (0x A000 0003: a junction or mount point).
    pub tag: Option<u32>,
    pub attrs: u32,
    pub size: u64,
    pub mtime: i64,
    /// ChangeTime (FILETIME ticks): moves with every write, rename or metadata change.
    pub ctime: i64,
}

impl Item {
    /// The file id as a listing gives it (folded to 64 bits as the scan folds it; 0: unknown).
    pub fn id(&self) -> u64 {
        crate::scan::fold_id(&self.ident.file)
    }
}

pub const TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const TAG_SYMLINK: u32 = 0xA000_000C;

struct Handle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

fn open(path: &Path, access: u32, share: u32, flags: u32) -> io::Result<Handle> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
    let w = verbatim(path);
    let h = unsafe { CreateFileW(w.as_ptr(), access, share, std::ptr::null(), OPEN_EXISTING, flags, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(Handle(h)) }
}

const SHARE_ALL: u32 = 7;
const BACKUP: u32 = 0x0200_0000; // FILE_FLAG_BACKUP_SEMANTICS
const OPEN_REPARSE: u32 = 0x0020_0000; // FILE_FLAG_OPEN_REPARSE_POINT

pub fn item(path: &Path, follow: bool) -> io::Result<Item> {
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    item_of(&open(path, FILE_READ_ATTRIBUTES, SHARE_ALL, BACKUP | if follow { 0 } else { OPEN_REPARSE })?)
}

/// What an open handle names (it needs FILE_READ_ATTRIBUTES).
fn item_of(h: &Handle) -> io::Result<Item> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_ID_INFO, FILE_STANDARD_INFO, FileAttributeTagInfo, FileBasicInfo, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW, VOLUME_NAME_GUID,
    };
    unsafe fn info<T>(h: &Handle, class: i32) -> Option<T> {
        let mut v: T = unsafe { std::mem::zeroed() };
        (unsafe { GetFileInformationByHandleEx(h.0, class, (&mut v as *mut T).cast(), std::mem::size_of::<T>() as u32) } != 0).then_some(v)
    }
    unsafe {
        let basic: FILE_BASIC_INFO = info(h, FileBasicInfo).ok_or_else(io::Error::last_os_error)?;
        let std: Option<FILE_STANDARD_INFO> = info(h, FileStandardInfo);
        let tag: Option<FILE_ATTRIBUTE_TAG_INFO> = info(h, FileAttributeTagInfo);
        let (volume, file, full) = match info::<FILE_ID_INFO>(h, FileIdInfo) {
            Some(i) if i.FileId.Identifier != [0; 16] => (i.VolumeSerialNumber, i.FileId.Identifier, true),
            _ => {
                let mut b: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
                if GetFileInformationByHandle(h.0, &mut b) != 0 {
                    let mut f = [0u8; 16];
                    f[..8].copy_from_slice(&(((b.nFileIndexHigh as u64) << 32) | b.nFileIndexLow as u64).to_le_bytes());
                    (b.dwVolumeSerialNumber as u64, f, false)
                } else {
                    (0, [0; 16], false)
                }
            }
        };
        let mut buf = vec![0u16; 1024];
        let n = GetFinalPathNameByHandleW(h.0, buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_GUID) as usize;
        let guid = (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n])).and_then(|s| s.find('}').map(|k| s[..=k].to_string()));
        let reparse = basic.FileAttributes & 0x400 != 0;
        Ok(Item {
            ident: Identity { volume, file, full, created: basic.CreationTime, guid },
            dir: basic.FileAttributes & 0x10 != 0,
            tag: if reparse { Some(tag.map_or(0, |t| t.ReparseTag)) } else { None },
            attrs: basic.FileAttributes,
            size: std.map_or(0, |s| s.EndOfFile.max(0) as u64),
            mtime: basic.LastWriteTime,
            ctime: basic.ChangeTime,
        })
    }
}

/// The path Windows ends up at (junctions, mapped and 8.3 names resolved), as a plain
/// `C:\...` path: what the system-folder checks compare.
pub fn final_path(path: &Path) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, GetFinalPathNameByHandleW};
    let h = open(path, FILE_READ_ATTRIBUTES, SHARE_ALL, BACKUP)?;
    let mut buf = vec![0u16; 1024];
    let mut n = unsafe { GetFinalPathNameByHandleW(h.0, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
    if n >= buf.len() {
        buf = vec![0u16; n + 1];
        n = unsafe { GetFinalPathNameByHandleW(h.0, buf.as_mut_ptr(), buf.len() as u32, 0) } as usize;
    }
    if n == 0 || n >= buf.len() {
        return Err(io::Error::last_os_error());
    }
    let p = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
    Ok(crate::scan::normalize(&p))
}

// ---------- the volume ----------

/// The drive under a folder, as a move needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Volume {
    /// `C:\` (or `\\server\share\`).
    pub root: String,
    /// `GetDriveTypeW`: 2 removable, 3 fixed, 4 network, 5 optical, 6 RAM disk.
    pub kind: u32,
    pub fs: String,
    /// `FILE_SUPPORTS_REPARSE_POINTS` (and the other flags).
    pub flags: u32,
    pub free: u64,
    pub cluster: u64,
    /// On a USB bus (a fixed drive can be); None: could not tell.
    pub usb: Option<bool>,
}

pub fn volume(path: &Path) -> io::Result<Volume> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW};
    let p = plain(path);
    let mut root = vec![0u16; 1024];
    if unsafe { GetVolumePathNameW(p.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = root.iter().position(|&c| c == 0).unwrap_or(0);
    root.truncate(len + 1);
    let mut v = Volume { root: String::from_utf16_lossy(&root[..len]), ..Volume::default() };
    unsafe {
        v.kind = GetDriveTypeW(root.as_ptr());
        let mut fs = [0u16; 64];
        let mut flags = 0u32;
        if GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut flags, fs.as_mut_ptr(), fs.len() as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
        let n = fs.iter().position(|&c| c == 0).unwrap_or(0);
        v.fs = String::from_utf16_lossy(&fs[..n]);
        v.flags = flags;
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        if GetDiskFreeSpaceExW(p.as_ptr(), &mut avail, &mut total, &mut free) == 0 {
            return Err(io::Error::last_os_error());
        }
        v.free = avail;
        let (mut spc, mut bps, mut fc, mut tc) = (0u32, 0u32, 0u32, 0u32);
        v.cluster = if GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut fc, &mut tc) != 0 { spc as u64 * bps as u64 } else { 4096 };
    }
    if v.root.starts_with("\\\\") {
        v.kind = 4;
    }
    v.usb = usb(&v.root);
    Ok(v)
}

/// Whether the disk under `C:\` sits on a USB bus (IOCTL_STORAGE_QUERY_PROPERTY on `\\.\C:`,
/// which needs no access rights). None: no letter, or it would not say.
fn usb(root: &str) -> Option<bool> {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY, StorageDeviceProperty};
    let b = root.as_bytes();
    if b.len() < 2 || b[1] != b':' {
        return None;
    }
    let dev = PathBuf::from(format!("\\\\.\\{}:", b[0] as char));
    let w = plain(&dev);
    let h = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(w.as_ptr(), 0, SHARE_ALL, std::ptr::null(), 3, 0, std::ptr::null_mut())
    };
    if h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return None;
    }
    let h = Handle(h);
    let q = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceProperty, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
    let mut out = vec![0u64; 128];
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(h.0, IOCTL_STORAGE_QUERY_PROPERTY, (&q as *const STORAGE_PROPERTY_QUERY).cast(), std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32, out.as_mut_ptr().cast(), 1024, &mut got, std::ptr::null_mut())
    } != 0;
    if !ok || (got as usize) < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return None;
    }
    let d = unsafe { &*(out.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    Some(d.BusType == 7) // BusTypeUsb
}

// ---------- listing ----------

/// One entry of a folder (VibeSpace's own bulk listing: attributes, reparse tag, sizes).
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: Vec<u16>,
    pub attrs: u32,
    pub tag: u32,
    pub size: u64,
    pub alloc: u64,
    pub mtime: i64,
    /// ChangeTime (0: the listing has none).
    pub ctime: i64,
    /// The file id, folded (0: unknown).
    pub id: u64,
}

pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut v = verbatim(dir);
    v.pop();
    let mode = crate::scan::Mode::new(0);
    let l = crate::scan::list(&v, &std::sync::atomic::AtomicBool::new(false), &mode);
    if let Some(e) = l.error {
        return Err(io::Error::from_raw_os_error(e as i32));
    }
    Ok(l.entries
        .iter()
        .map(|(e, s, n)| Entry { name: l.names[*s as usize..*s as usize + *n as usize].to_vec(), attrs: e.attrs, tag: e.tag, size: e.size, alloc: e.alloc, mtime: e.modified, ctime: e.changed, id: e.id })
        .collect())
}

// ---------- changing things ----------

/// A new folder (never one that is there).
pub fn create_dir(path: &Path) -> io::Result<()> {
    let w = verbatim(path);
    if unsafe { windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(w.as_ptr(), std::ptr::null()) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Rename or move within one volume, never replacing what is at `to`.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (verbatim(from), verbatim(to));
    if unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Rename `from` to `to` (never replacing what is there), but only while what `from` names is
/// what `is` accepts: it is judged on the handle that renames it (opened for DELETE, never
/// through a link), so what is renamed is what was checked (R-SPACE-MV2-1). Ok(false): it is
/// not, and nothing was renamed.
pub fn rename_if(from: &Path, to: &Path, is: &dyn Fn(&Item) -> bool) -> io::Result<bool> {
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FileRenameInfo, SYNCHRONIZE, SetFileInformationByHandle};
    let h = open(from, DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, SHARE_ALL, BACKUP | OPEN_REPARSE)?;
    if !is(&item_of(&h)?) {
        return Ok(false);
    }
    let mut name = verbatim(to);
    name.pop();
    let head = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = head + (name.len() + 1) * 2;
    let mut buf = vec![0u64; size.div_ceil(8)];
    unsafe {
        let info = buf.as_mut_ptr() as *mut FILE_RENAME_INFO;
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (info as *mut u8).add(head) as *mut u16, name.len());
        if SetFileInformationByHandle(h.0, FileRenameInfo, info.cast(), size as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(true)
}

/// Give a new folder its source folder's read-only (the shell's "customized folder" mark),
/// hidden, system and not-indexed attributes. A read-only folder still takes new files; only a
/// removal clears the mark first (`remove_dir`).
pub fn copy_dir_attributes(src: &Path, dst: &Path) {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW};
    let keep = 0x1 | 0x2 | 0x4 | 0x2000; // read-only, hidden, system, not content indexed
    let (s, d) = (verbatim(src), verbatim(dst));
    unsafe {
        let a = GetFileAttributesW(s.as_ptr());
        if a != INVALID_FILE_ATTRIBUTES && a & keep != 0 {
            let cur = GetFileAttributesW(d.as_ptr());
            if cur != INVALID_FILE_ATTRIBUTES {
                SetFileAttributesW(d.as_ptr(), cur | (a & keep));
            }
        }
    }
}

/// Give a folder its source folder's creation and modified times (after everything is in it:
/// each new entry moves a folder's modified time).
pub fn copy_dir_times(src: &Path, dst: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx, SetFileInformationByHandle};
    let s = open(src, 0x80 /* FILE_READ_ATTRIBUTES */, SHARE_ALL, BACKUP | OPEN_REPARSE)?;
    let d = open(dst, 0x100 /* FILE_WRITE_ATTRIBUTES */, SHARE_ALL, BACKUP | OPEN_REPARSE)?;
    unsafe {
        let mut b: FILE_BASIC_INFO = std::mem::zeroed();
        if GetFileInformationByHandleEx(s.0, FileBasicInfo, (&mut b as *mut FILE_BASIC_INFO).cast(), std::mem::size_of::<FILE_BASIC_INFO>() as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
        // Zero leaves a field as it is: only the creation and modified times are set.
        let set = FILE_BASIC_INFO { CreationTime: b.CreationTime, LastWriteTime: b.LastWriteTime, ..std::mem::zeroed() };
        if SetFileInformationByHandle(d.0, FileBasicInfo, (&set as *const FILE_BASIC_INFO).cast(), std::mem::size_of::<FILE_BASIC_INFO>() as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Write everything the drive under `p` holds in its cache to the disk itself: a flush of a
/// folder opened for writing, which asks the device to flush its own cache too (as a flush of a
/// file does). Before the originals go, the copy is on the disk, not in a cache a power cut
/// takes with it.
pub fn flush(p: &Path) -> io::Result<()> {
    let h = open(p, 0x0002 /* FILE_ADD_FILE (write data) */ | 0x0004, SHARE_ALL, BACKUP)?;
    if unsafe { windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(h.0) } == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// A folder's own permissions as SDDL (the DACL and whether it is protected); None: it has
/// no DACL (everyone may).
pub fn dacl(p: &Path) -> io::Result<Option<String>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION};
    let w = verbatim(p);
    unsafe {
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        let r = GetNamedSecurityInfoW(w.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut acl, std::ptr::null_mut(), &mut sd);
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        if acl.is_null() {
            LocalFree(sd);
            return Ok(None);
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, DACL_SECURITY_INFORMATION, &mut s, std::ptr::null_mut()) != 0;
        let e = io::Error::last_os_error();
        LocalFree(sd);
        if !ok {
            return Err(e);
        }
        let n = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s.cast());
        Ok(Some(out))
    }
}

/// Put permissions read by `dacl` back on a folder: protected as they were, or (unprotected)
/// their own entries with the parent's inherited ones recomputed by Windows.
pub fn set_dacl(p: &Path, sddl: &str) -> io::Result<()> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    let w = verbatim(p);
    let s = crate::host::wide(sddl);
    unsafe {
        let mut sd = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(s.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let (mut present, mut defaulted, mut acl): (i32, i32, *mut ACL) = (0, 0, std::ptr::null_mut());
        let (mut control, mut rev) = (0u16, 0u32);
        let ok = GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) != 0 && GetSecurityDescriptorControl(sd, &mut control, &mut rev) != 0;
        if !ok || present == 0 || acl.is_null() {
            let e = io::Error::last_os_error();
            LocalFree(sd);
            return Err(if ok { io::Error::other("no permissions in the record") } else { e });
        }
        let protect = if control & 0x1000 /* SE_DACL_PROTECTED */ != 0 { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
        let r = SetNamedSecurityInfoW(w.as_ptr() as *mut u16, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | protect, std::ptr::null_mut(), std::ptr::null_mut(), acl, std::ptr::null());
        LocalFree(sd);
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
    }
    Ok(())
}

/// Put the source folder's permissions on the new folder, protected (they no longer come from
/// the destination's parent): what is created in it afterwards inherits them.
pub fn copy_dacl(src: &Path, dst: &Path) -> io::Result<()> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION};
    let (s, d) = (verbatim(src), verbatim(dst));
    unsafe {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        let r = GetNamedSecurityInfoW(s.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut dacl, std::ptr::null_mut(), &mut sd);
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        if dacl.is_null() {
            LocalFree(sd);
            return Ok(()); // no DACL: everyone may; the destination's own is kept
        }
        // A copy with every ACE's "inherited" mark cleared: they are this folder's own now.
        let size = (*dacl).AclSize as usize;
        let mut buf = std::slice::from_raw_parts(dacl as *const u8, size).to_vec();
        LocalFree(sd);
        clear_inherited(&mut buf);
        let r = SetNamedSecurityInfoW(d.as_ptr() as *mut u16, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), buf.as_ptr() as *const ACL, std::ptr::null());
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
    }
    Ok(())
}

/// An ACL's ACEs lose INHERITED_ACE (0x10). Walks the buffer checked: an ACE that does not fit
/// stops it (the rest stay as they are).
pub fn clear_inherited(acl: &mut [u8]) {
    if acl.len() < 8 {
        return;
    }
    let count = u16::from_le_bytes([acl[4], acl[5]]) as usize;
    let mut off = 8;
    for _ in 0..count {
        if off + 4 > acl.len() {
            return;
        }
        let size = u16::from_le_bytes([acl[off + 2], acl[off + 3]]) as usize;
        if size < 4 || off + size > acl.len() {
            return;
        }
        acl[off + 1] &= !0x10;
        off += size;
    }
}

/// Delete a folder tree, never following a link inside (a link is removed itself). Read-only
/// files go too.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    let it = item(path, false)?;
    if it.tag.is_some() || !it.dir {
        return if it.dir { remove_dir(path) } else { remove_file(path) };
    }
    for e in list(path)? {
        use std::os::windows::ffi::OsStringExt;
        let p = path.join(std::ffi::OsString::from_wide(&e.name));
        let link = e.attrs & 0x400 != 0 && e.tag & 0x2000_0000 != 0;
        if e.attrs & 0x10 != 0 && !link {
            remove_tree(&p)?;
        } else if e.attrs & 0x10 != 0 {
            remove_dir(&p)?;
        } else {
            remove_file(&p)?;
        }
    }
    remove_dir(path)
}

/// Remove an empty folder (or a junction itself). A read-only one (a customized folder) has
/// the mark cleared for the removal, and put back when the removal still fails.
pub fn remove_dir(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES, RemoveDirectoryW, SetFileAttributesW};
    let w = verbatim(path);
    unsafe {
        if RemoveDirectoryW(w.as_ptr()) != 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        let a = GetFileAttributesW(w.as_ptr());
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) && a != INVALID_FILE_ATTRIBUTES && a & 1 != 0 {
            SetFileAttributesW(w.as_ptr(), a & !1);
            if RemoveDirectoryW(w.as_ptr()) != 0 {
                return Ok(());
            }
            let e2 = io::Error::last_os_error();
            SetFileAttributesW(w.as_ptr(), a);
            return Err(e2);
        }
        Err(e)
    }
}

pub fn remove_file(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{DeleteFileW, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW};
    let w = verbatim(path);
    unsafe {
        if DeleteFileW(w.as_ptr()) != 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        let a = GetFileAttributesW(w.as_ptr());
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) && a != INVALID_FILE_ATTRIBUTES && a & 1 != 0 {
            SetFileAttributesW(w.as_ptr(), a & !1);
            if DeleteFileW(w.as_ptr()) != 0 {
                return Ok(());
            }
            let e2 = io::Error::last_os_error();
            SetFileAttributesW(w.as_ptr(), a);
            return Err(e2);
        }
        Err(e)
    }
}

/// May this account rename (or remove) the folder now? Opened for DELETE: refused without the
/// right, and refused while a program has the folder itself open without delete sharing (a
/// console sitting in it). Files open inside are found by the rename itself.
pub fn may_rename(dir: &Path) -> io::Result<()> {
    open(dir, 0x0001_0000, SHARE_ALL, BACKUP | OPEN_REPARSE).map(|_| ())
}

/// May this account make a new entry (the link) in this folder?
pub fn may_add(dir: &Path) -> io::Result<()> {
    open(dir, 0x0004 /* FILE_ADD_SUBDIRECTORY */, SHARE_ALL, BACKUP).map(|_| ())
}

// ---------- junctions ----------

fn units(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().collect()
}

/// A junction at `at` (a new folder) leading to `target` (an absolute local folder path).
pub fn create_junction(at: &Path, target: &Path) -> io::Result<()> {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    let buf = junction_buffer(target)?;
    create_dir(at)?;
    let set = || -> io::Result<()> {
        let h = open(at, 0x4000_0000 /* GENERIC_WRITE */, SHARE_ALL, BACKUP | OPEN_REPARSE)?;
        let mut got = 0u32;
        if unsafe { DeviceIoControl(h.0, FSCTL_SET_REPARSE_POINT, buf.as_ptr().cast(), buf.len() as u32, std::ptr::null_mut(), 0, &mut got, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    set().inspect_err(|_| {
        let _ = remove_dir(at);
    })
}

/// The REPARSE_DATA_BUFFER of a junction to `target`: substitute name `\??\D:\x`, print name
/// `D:\x`.
pub fn junction_buffer(target: &Path) -> io::Result<Vec<u8>> {
    let mut t = units(target);
    // `\\?\D:\x` → `D:\x`; a share cannot be a junction's target.
    let pre: Vec<u16> = "\\\\?\\".encode_utf16().collect();
    if t.starts_with(&pre) {
        t.drain(..4);
    }
    let ok = t.len() >= 3 && t[1] == u16::from(b':') && t[2] == u16::from(b'\\') && (t[0] as u8).is_ascii_alphabetic() && t[0] < 128;
    if !ok {
        return Err(io::Error::other("a link can only lead to a folder on a local drive"));
    }
    while t.len() > 3 && t.last() == Some(&u16::from(b'\\')) {
        t.pop();
    }
    let sub: Vec<u16> = "\\??\\".encode_utf16().chain(t.iter().copied()).collect();
    let (sub_b, print_b) = (sub.len() * 2, t.len() * 2);
    let data_len = 8 + sub_b + 2 + print_b + 2;
    if data_len > 16 * 1024 - 8 {
        return Err(io::Error::from_raw_os_error(206));
    }
    let mut b = Vec::with_capacity(8 + data_len);
    b.extend_from_slice(&TAG_MOUNT_POINT.to_le_bytes());
    b.extend_from_slice(&(data_len as u16).to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    for v in [0u16, sub_b as u16, (sub_b + 2) as u16, print_b as u16] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for u in sub.iter().chain([0u16].iter()).chain(t.iter()).chain([0u16].iter()) {
        b.extend_from_slice(&u.to_le_bytes());
    }
    Ok(b)
}

/// Where the junction at `at` leads (`D:\x`), or None when `at` is no junction.
pub fn junction_target(at: &Path) -> io::Result<Option<PathBuf>> {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
    let it = item(at, false)?;
    if it.tag != Some(TAG_MOUNT_POINT) {
        return Ok(None);
    }
    let h = open(at, 0x80 /* FILE_READ_ATTRIBUTES */, SHARE_ALL, BACKUP | OPEN_REPARSE)?;
    let mut buf = vec![0u8; 16 * 1024];
    let mut got = 0u32;
    if unsafe { DeviceIoControl(h.0, FSCTL_GET_REPARSE_POINT, std::ptr::null(), 0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut got, std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate((got as usize).min(buf.len()));
    Ok(parse_junction(&buf))
}

/// A junction's substitute name as a plain path (`\??\D:\x` → `D:\x`); None for anything that
/// is not a junction buffer or does not fit.
pub fn parse_junction(buf: &[u8]) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let tag = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?);
    if tag != TAG_MOUNT_POINT {
        return None;
    }
    let u16_at = |o: usize| buf.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let (off, len) = (u16_at(8)?, u16_at(10)?);
    if len % 2 != 0 {
        return None;
    }
    let b = buf.get(16 + off..16 + off + len)?;
    let mut w: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    let pre: Vec<u16> = "\\??\\".encode_utf16().collect();
    if !w.starts_with(&pre) {
        return None;
    }
    w.drain(..4);
    if w.len() > 3 && w.last() == Some(&u16::from(b'\\')) {
        w.pop();
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(&w)))
}

// ---------- copying and hashing (VibeCopy's engine) ----------

/// Files from this size on are copied unbuffered.
pub const BIG: u64 = 256 << 20;

struct Ctx<'a> {
    step: &'a dyn Fn(u64) -> bool,
}

unsafe extern "system" fn on_progress(msg: *const windows_sys::Win32::Storage::FileSystem::COPYFILE2_MESSAGE, data: *const core::ffi::c_void) -> i32 {
    use windows_sys::Win32::Storage::FileSystem::*;
    let ctx = unsafe { &*(data as *const Ctx) };
    let m = unsafe { &*msg };
    let done = match m.Type {
        COPYFILE2_CALLBACK_CHUNK_FINISHED => unsafe { m.Info.ChunkFinished.uliTotalBytesTransferred },
        COPYFILE2_CALLBACK_STREAM_STARTED | COPYFILE2_CALLBACK_POLL_CONTINUE => u64::MAX,
        _ => return COPYFILE2_PROGRESS_CONTINUE,
    };
    if (ctx.step)(done) { COPYFILE2_PROGRESS_CONTINUE } else { COPYFILE2_PROGRESS_CANCEL }
}

/// Copy one file with `CopyFile2` (attributes, the modified time and alternate streams carried),
/// never over something that is there. `step` hears the bytes copied so far (`u64::MAX`: no new
/// count) and says whether to go on; Ok(false): stopped by it (the part is deleted).
pub fn copy_file(src: &Path, dst: &Path, step: &dyn Fn(u64) -> bool) -> io::Result<bool> {
    use windows_sys::Win32::Storage::FileSystem::*;
    let (s, d) = (verbatim(src), verbatim(dst));
    let ctx = Ctx { step };
    let size = item(src, false).map(|i| i.size).unwrap_or(0);
    let mut flags = COPY_FILE_FAIL_IF_EXISTS;
    if size >= BIG {
        flags |= COPY_FILE_NO_BUFFERING;
    }
    let params = COPYFILE2_EXTENDED_PARAMETERS {
        dwSize: std::mem::size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
        dwCopyFlags: flags,
        pfCancel: std::ptr::null_mut(),
        pProgressRoutine: Some(on_progress),
        pvCallbackContext: &ctx as *const Ctx as *mut core::ffi::c_void,
    };
    let hr = unsafe { CopyFile2(s.as_ptr(), d.as_ptr(), &params) };
    if hr >= 0 {
        return Ok(true);
    }
    let code = if (hr as u32 & 0xFFFF_0000) == 0x8007_0000 { (hr as u32 & 0xFFFF) as i32 } else { hr };
    if code == ERROR_REQUEST_ABORTED { Ok(false) } else { Err(io::Error::from_raw_os_error(code)) }
}

pub type Digest = [u8; 32];

struct Hasher(windows_sys::Win32::Security::Cryptography::BCRYPT_HASH_HANDLE);

impl Hasher {
    fn new() -> io::Result<Hasher> {
        use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash};
        let mut h = std::ptr::null_mut();
        let st = unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut h, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0) };
        if st < 0 {
            return Err(io::Error::other(format!("SHA-256 is not available (NTSTATUS {st:#x})")));
        }
        Ok(Hasher(h))
    }
    fn update(&mut self, data: &[u8]) -> io::Result<()> {
        use windows_sys::Win32::Security::Cryptography::BCryptHashData;
        for chunk in data.chunks(u32::MAX as usize) {
            let st = unsafe { BCryptHashData(self.0, chunk.as_ptr(), chunk.len() as u32, 0) };
            if st < 0 {
                return Err(io::Error::other(format!("hashing failed (NTSTATUS {st:#x})")));
            }
        }
        Ok(())
    }
    fn finish(self) -> io::Result<Digest> {
        use windows_sys::Win32::Security::Cryptography::BCryptFinishHash;
        let mut out = [0u8; 32];
        let st = unsafe { BCryptFinishHash(self.0, out.as_mut_ptr(), 32, 0) };
        if st < 0 {
            return Err(io::Error::other(format!("hashing failed (NTSTATUS {st:#x})")));
        }
        Ok(out)
    }
}

impl Drop for Hasher {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Security::Cryptography::BCryptDestroyHash(self.0) };
    }
}

pub fn sha256(data: &[u8]) -> io::Result<Digest> {
    let mut h = Hasher::new()?;
    h.update(data)?;
    h.finish()
}

/// A buffer aligned for unbuffered reads (sector and page multiples).
struct Aligned {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl Aligned {
    fn new(len: usize) -> Aligned {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Aligned { ptr, layout }
    }
    fn buf(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

const CHUNK: usize = 4 << 20;

/// Stream a file through SHA-256. `unbuffered`: read from the disk, past the cache (a copy just
/// written must be read from where it landed, not from memory). Ok(None): `stop` said stop.
pub fn hash_file(path: &Path, unbuffered: bool, stop: &dyn Fn() -> bool, read: &dyn Fn(u64)) -> io::Result<Option<Digest>> {
    use std::io::Read;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    const NO_BUFFERING: u32 = 0x2000_0000;
    const SEQUENTIAL: u32 = 0x0800_0000;
    let mut v = verbatim(path);
    v.pop();
    let p = PathBuf::from(std::ffi::OsString::from_wide(&v));
    let flags = if unbuffered { NO_BUFFERING | SEQUENTIAL } else { SEQUENTIAL };
    let mut f = std::fs::OpenOptions::new().read(true).share_mode(SHARE_ALL).custom_flags(flags).open(&p)?;
    let mut h = Hasher::new()?;
    let mut buf = Aligned::new(CHUNK);
    let mut total = 0u64;
    loop {
        if stop() {
            return Ok(None);
        }
        let n = loop {
            match f.read(buf.buf()) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => break r?,
            }
        };
        if n == 0 {
            break;
        }
        h.update(&buf.buf()[..n])?;
        total += n as u64;
        read(total);
    }
    h.finish().map(Some)
}

// ---------- who has files open ----------

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationFile(h: windows_sys::Win32::Foundation::HANDLE, iosb: *mut [usize; 2], info: *mut core::ffi::c_void, len: u32, class: i32) -> i32;
}

const FILE_PROCESS_IDS_USING_FILE: i32 = 47; // FileProcessIdsUsingFileInformation
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

/// The processes that have this file open: a handle, or only a mapped view of it (a program
/// that maps a file and closes its handle keeps writing through the view: measured, the
/// Restart Manager and this both name it; a rename of the folder does not see it). Each file is
/// asked on its own handle (opened for its attributes only, which is not counted), so no path
/// is too long to ask about.
fn pids_using(path: &Path) -> io::Result<Vec<u32>> {
    let h = open(path, 0x80 /* FILE_READ_ATTRIBUTES */, SHARE_ALL, BACKUP | OPEN_REPARSE)?;
    let mut buf = vec![0usize; 64];
    loop {
        let mut iosb = [0usize; 2];
        let st = unsafe { NtQueryInformationFile(h.0, &mut iosb, buf.as_mut_ptr().cast(), (buf.len() * size_of::<usize>()) as u32, FILE_PROCESS_IDS_USING_FILE) };
        if st == STATUS_INFO_LENGTH_MISMATCH && buf.len() < 1 << 16 {
            buf = vec![0usize; buf.len() * 4];
            continue;
        }
        if st < 0 {
            let e = unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(st) };
            return Err(io::Error::from_raw_os_error(e as i32));
        }
        // FILE_PROCESS_IDS_USING_FILE_INFORMATION: a count, then that many ids (pointer-sized).
        let n = (buf[0] & 0xFFFF_FFFF).min(buf.len() - 1);
        return Ok(buf[1..=n].iter().map(|&p| p as u32).collect());
    }
}

/// A process's name for people: its exe's name without `.exe`; empty when it cannot be asked.
fn process_name(pid: u32) -> String {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return String::new();
    }
    let h = Handle(h);
    let mut buf = vec![0u16; 1024];
    let mut n = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(h.0, 0, buf.as_mut_ptr(), &mut n) } == 0 {
        return String::new();
    }
    let full = String::from_utf16_lossy(&buf[..n as usize]);
    let file = full.rsplit('\\').next().unwrap_or(&full);
    let stem = if file.len() > 4 && file[file.len() - 4..].eq_ignore_ascii_case(".exe") { &file[..file.len() - 4] } else { file };
    crate::tree::clean(stem)
}

struct Session(u32);

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::RestartManager::RmEndSession(self.0) };
    }
}

/// The longest path the Restart Manager takes is one less than this; longer ones are asked
/// one by one (`pids_using`).
pub const RM_PATH_LIMIT: usize = 260;
/// Files per Restart Manager session (about a quarter of a second each, measured: 60 µs a
/// file, linear): the brakes are looked at between sessions.
pub const RM_BATCH: usize = 5000;

/// The programs that have any of `files` open, asked of the Restart Manager in one session (it
/// names a program holding a file only through a mapped view too: measured).
fn rm_holders(files: &[PathBuf]) -> Result<Vec<(u32, String)>, String> {
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::RestartManager::{CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmGetList, RmRegisterResources, RmStartSession};
    let words = |e: u32| describe(&io::Error::from_raw_os_error(e as i32));
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mut handle = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    let r = unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) };
    if r != ERROR_SUCCESS {
        return Err(format!("the Restart Manager would not start: {}", words(r)));
    }
    let session = Session(handle);
    let wide: Vec<Vec<u16>> = files.iter().map(|f| plain(f)).collect();
    let ptrs: Vec<*const u16> = wide.iter().map(|w| w.as_ptr()).collect();
    let r = unsafe { RmRegisterResources(session.0, ptrs.len() as u32, ptrs.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null()) };
    if r != ERROR_SUCCESS {
        return Err(format!("the Restart Manager would not take the files: {}", words(r)));
    }
    let mut room = 16u32;
    for _ in 0..6 {
        let mut list: Vec<RM_PROCESS_INFO> = vec![unsafe { std::mem::zeroed() }; room as usize];
        let (mut needed, mut count, mut reasons) = (0u32, room, 0u32);
        let r = unsafe { RmGetList(session.0, &mut needed, &mut count, list.as_mut_ptr(), &mut reasons) };
        if r == ERROR_SUCCESS {
            list.truncate(count as usize);
            return Ok(list
                .iter()
                .map(|p| {
                    let w = &p.strAppName;
                    let name = String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())]);
                    (p.Process.dwProcessId, crate::tree::clean(&name))
                })
                .collect());
        }
        if r != ERROR_MORE_DATA {
            return Err(format!("the Restart Manager could not list the programs: {}", words(r)));
        }
        room = needed + 8;
    }
    Err("the list of programs kept growing while it was read".into())
}

/// The programs that have any of `files` open (a handle or a mapped view), by pid and name.
/// Every file is asked: the Restart Manager in sessions of `RM_BATCH`, a path too long for it
/// on its own (slow: tens of ms a file, measured; such paths are few). Err when any answer
/// could not be had (it would be incomplete, never "none"). A file gone since the walk is no
/// holder (the compare after the rename finds it gone). `stop`: Err("Cancelled").
pub fn holders(files: &[PathBuf], stop: &dyn Fn() -> bool) -> Result<Vec<(u32, String)>, String> {
    let (short, long): (Vec<PathBuf>, Vec<PathBuf>) = files.iter().cloned().partition(|p| p.as_os_str().len() < RM_PATH_LIMIT);
    let mut found: Vec<(u32, String)> = Vec::new();
    for batch in short.chunks(RM_BATCH) {
        if stop() {
            return Err("Cancelled".into());
        }
        found.extend(rm_holders(batch)?);
    }
    let mut failed: Vec<String> = Vec::new();
    for (i, f) in long.iter().enumerate() {
        if i % 16 == 0 && stop() {
            return Err("Cancelled".into());
        }
        match pids_using(f) {
            Ok(p) => found.extend(p.into_iter().map(|p| (p, String::new()))),
            Err(e) if is_gone(&e) => {}
            Err(e) => failed.push(format!("{} ({})", crate::tree::clean(&f.display().to_string()), describe(&e))),
        }
    }
    if !failed.is_empty() {
        let more = if failed.len() > 1 { format!(" and {} more", failed.len() - 1) } else { String::new() };
        return Err(format!("{} could not be asked{more}", failed[0]));
    }
    // One entry a program, by the Restart Manager's name where it gave one, else its exe's.
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.len().cmp(&a.1.len())));
    found.dedup_by_key(|h| h.0);
    Ok(found.into_iter().map(|(p, n)| if n.is_empty() { (p, process_name(p)) } else { (p, n) }).collect())
}
