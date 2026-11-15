//! The drives on the first screen. Listing the letters costs nothing; asking a drive for its
//! label and free space can take ~20 s when it is a network drive whose server is gone, so
//! each drive is asked on a thread of its own and answers when it can.

/// A drive letter and what kind of drive it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// `C:\`
    pub root: String,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fixed,
    Removable,
    Network,
    Optical,
    Ram,
    Unknown,
}

/// What a drive says about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub label: String,
    pub fs: String,
    pub total: u64,
    pub free: u64,
    /// Why it could not say (no disc, server gone): empty when it answered.
    pub error: String,
}

/// Every drive letter in use (no disk access).
pub fn list() -> Vec<Drive> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDriveStringsW};
    let mut buf = vec![0u16; 512];
    let n = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
    if n == 0 || n > buf.len() {
        return Vec::new();
    }
    buf[..n]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut w = s.to_vec();
            w.push(0);
            let kind = match unsafe { GetDriveTypeW(w.as_ptr()) } {
                2 => Kind::Removable,
                3 => Kind::Fixed,
                4 => Kind::Network,
                5 => Kind::Optical,
                6 => Kind::Ram,
                _ => Kind::Unknown,
            };
            Drive { root: String::from_utf16_lossy(s), kind }
        })
        .collect()
}

/// Ask one drive (slow for a network drive that does not answer: call it off the UI thread).
pub fn info(root: &str) -> Info {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumeInformationW};
    let w = crate::host::wide(root);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 64];
    let mut out = Info::default();
    unsafe {
        if GetVolumeInformationW(w.as_ptr(), label.as_mut_ptr(), label.len() as u32, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), fs.as_mut_ptr(), fs.len() as u32) == 0 {
            let e = windows_sys::Win32::Foundation::GetLastError();
            out.error = if e == 21 { "No disc or not ready".into() } else { crate::scan::error_text(e) };
            return out;
        }
        names(&mut out, &label, &fs);
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        if GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) != 0 {
            out.total = total;
            out.free = avail;
        }
    }
    out
}

/// The volume's label and file-system name as shown, from the buffers Windows filled (up to
/// their NUL). Both are cleaned: a network drive's server names its own file system
/// (IN-VibeSpace-5).
fn names(out: &mut Info, label: &[u16], fs: &[u16]) {
    let z = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
    out.label = crate::tree::clean_max(&z(label), 255);
    out.fs = crate::tree::clean_max(&z(fs), 64);
}
