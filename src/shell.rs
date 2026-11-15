//! The shell: the Recycle Bin (IFileOperation), the folder and save pickers (IFileDialog), and
//! opening things. windows-sys has no COM interfaces, so the few methods used are called through
//! hand-written vtables (the layouts are the SDK's, in declaration order; only the slots called
//! are typed, the others are placeholders that keep the offsets).
//!
//! Never a permanent delete: only fixed drives are asked (removable and network drives have no
//! Recycle Bin, the shell would delete for good), and FOF_WANTNUKEWARNING makes the shell ask
//! the user first should it still find an item it cannot recycle (too big for the bin).

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use windows_sys::core::{GUID, HRESULT, PCWSTR, PWSTR};

type Fn0 = usize; // a slot not called from here

#[repr(C)]
struct UnknownVtbl {
    query_interface: Fn0,
    add_ref: Fn0,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct ShellItemVtbl {
    base: UnknownVtbl,
    bind_to_handler: Fn0,
    get_parent: Fn0,
    get_display_name: unsafe extern "system" fn(*mut c_void, u32, *mut PWSTR) -> HRESULT,
    get_attributes: Fn0,
    compare: Fn0,
}

#[repr(C)]
struct FileOperationVtbl {
    base: UnknownVtbl,
    advise: Fn0,
    unadvise: Fn0,
    set_operation_flags: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    set_progress_message: Fn0,
    set_progress_dialog: Fn0,
    set_properties: Fn0,
    set_owner_window: unsafe extern "system" fn(*mut c_void, isize) -> HRESULT,
    apply_properties_to_item: Fn0,
    apply_properties_to_items: Fn0,
    rename_item: Fn0,
    rename_items: Fn0,
    move_item: Fn0,
    move_items: Fn0,
    copy_item: Fn0,
    copy_items: Fn0,
    delete_item: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT,
    delete_items: Fn0,
    new_item: Fn0,
    perform_operations: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    get_any_operations_aborted: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
}

#[repr(C)]
struct FilterSpec {
    name: PCWSTR,
    spec: PCWSTR,
}

#[repr(C)]
struct FileDialogVtbl {
    base: UnknownVtbl,
    show: unsafe extern "system" fn(*mut c_void, isize) -> HRESULT,
    set_file_types: unsafe extern "system" fn(*mut c_void, u32, *const FilterSpec) -> HRESULT,
    set_file_type_index: Fn0,
    get_file_type_index: Fn0,
    advise: Fn0,
    unadvise: Fn0,
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    set_default_folder: Fn0,
    set_folder: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    get_folder: Fn0,
    get_current_selection: Fn0,
    set_file_name: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    get_file_name: Fn0,
    set_title: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    set_ok_button_label: Fn0,
    set_file_name_label: Fn0,
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    add_place: Fn0,
    set_default_extension: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
}

const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);
const IID_IFILEOPERATION: GUID = GUID::from_u128(0x947aab5f_0a5c_4c13_b4d6_4bf7836fc9f8);
const IID_IFILEDIALOG: GUID = GUID::from_u128(0x42f85136_db7e_439c_85f1_e4075d135fc8);
const SIGDN_FILESYSPATH: u32 = 0x8005_8000;

/// An owned COM pointer: released when dropped.
struct Com(*mut c_void);

impl Com {
    unsafe fn vtbl<T>(&self) -> &T {
        unsafe { &**(self.0 as *const *const T) }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.vtbl::<UnknownVtbl>().release)(self.0) };
        }
    }
}

/// An HRESULT in words: Windows' own for the Win32 ones, the number otherwise.
fn hr_text(h: HRESULT) -> String {
    let u = h as u32;
    if (u >> 16) & 0x1FFF == 7 { crate::scan::error_text(u & 0xFFFF).to_lowercase() } else { format!("0x{u:08X}") }
}

fn hr(h: HRESULT, what: &str) -> Result<(), String> {
    if h >= 0 { Ok(()) } else { Err(format!("{what} failed ({})", hr_text(h))) }
}

/// COM on this thread for as long as it lives (single-threaded apartment, as the shell wants).
struct Apartment(bool);

impl Apartment {
    fn enter() -> Apartment {
        use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
        let h = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
        Apartment(h >= 0)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows_sys::Win32::System::Com::CoUninitialize() };
        }
    }
}

fn shell_item(path: &Path) -> Result<Com, String> {
    use windows_sys::Win32::UI::Shell::SHCreateItemFromParsingName;
    let p = crate::host::wide_path(path);
    let mut out: *mut c_void = std::ptr::null_mut();
    hr(unsafe { SHCreateItemFromParsingName(p.as_ptr(), std::ptr::null_mut(), &IID_ISHELLITEM, &mut out) }, "Finding the item")?;
    Ok(Com(out))
}

fn create(clsid: &GUID, iid: &GUID) -> Result<Com, String> {
    use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    let mut out: *mut c_void = std::ptr::null_mut();
    hr(unsafe { CoCreateInstance(clsid, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, iid, &mut out) }, "Starting the shell")?;
    Ok(Com(out))
}

/// Whether the drive under `path` has a Recycle Bin the shell would use: fixed drives only.
pub fn can_recycle(path: &Path) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    let p = crate::host::wide_path(path);
    let mut root = vec![0u16; 1024];
    if unsafe { GetVolumePathNameW(p.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err("Cannot tell which drive this is on, so it is left where it is".into());
    }
    match unsafe { GetDriveTypeW(root.as_ptr()) } {
        3 => Ok(()), // DRIVE_FIXED
        2 => Err("This is a removable drive: it has no Recycle Bin, and VibeSpace never deletes for good".into()),
        4 => Err("This is a network drive: it has no Recycle Bin, and VibeSpace never deletes for good".into()),
        _ => Err("This drive has no Recycle Bin, and VibeSpace never deletes for good".into()),
    }
}

/// What `recycle` checks before it asks the shell anything (the tests' bin checks the same).
pub fn precheck(paths: &[PathBuf]) -> Result<(), String> {
    for p in paths {
        // The shell parses paths the Win32 way, which drops a trailing dot or space from every
        // name: such an item (made through `\\?\`) cannot be found, plain or `\\?\`, and one
        // under such a folder is another item to the shell than the one checked through its
        // exact path (IN-VibeSpace-1). Asked first: it needs no disk.
        crate::host::plain_ok(p)?;
        can_recycle(p)?;
    }
    Ok(())
}

/// Why the Recycle Bin did not finish.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub why: String,
    /// Refused before the shell was asked to move anything (a check, starting COM, queuing an
    /// item): nothing went, so nothing needs reading again (R2-SPACE-4).
    pub nothing_went: bool,
}

impl Refusal {
    /// Refused before anything was moved.
    pub fn before(why: String) -> Refusal {
        Refusal { why, nothing_went: true }
    }

    /// Stopped or failed while moving: some items may have gone.
    pub fn during(why: String) -> Refusal {
        Refusal { why, nothing_went: false }
    }
}

/// Move `paths` to the Recycle Bin in one operation. Ok means the shell finished; which items
/// actually went is for the caller to check (an item still there was not moved).
pub fn recycle(paths: &[PathBuf], owner: isize) -> Result<(), Refusal> {
    use windows_sys::Win32::UI::Shell::{FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FOFX_RECYCLEONDELETE, FileOperation};
    precheck(paths).map_err(Refusal::before)?;
    let _com = Apartment::enter();
    let op = create(&FileOperation, &IID_IFILEOPERATION).map_err(Refusal::before)?;
    unsafe {
        let v = op.vtbl::<FileOperationVtbl>();
        hr(
            (v.set_operation_flags)(op.0, FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOF_WANTNUKEWARNING | FOF_NOCONFIRMATION | FOF_NOCONFIRMMKDIR | FOF_NOERRORUI | FOF_SILENT),
            "Setting up the Recycle Bin",
        )
        .map_err(Refusal::before)?;
        if owner != 0 {
            (v.set_owner_window)(op.0, owner);
        }
        for p in paths {
            let item = shell_item(p).map_err(Refusal::before)?;
            hr((v.delete_item)(op.0, item.0, std::ptr::null_mut()), "Queuing the item").map_err(Refusal::before)?;
        }
        let done = (v.perform_operations)(op.0);
        let mut aborted = 0i32;
        (v.get_any_operations_aborted)(op.0, &mut aborted);
        // Per item the caller looks: a stop or an error can still mean some went.
        if aborted != 0 || done == 0x8027_0000u32 as i32 {
            return Err(Refusal::during("Stopped before everything was moved".into()));
        }
        if done < 0 {
            return Err(Refusal::during(format!("The Recycle Bin refused ({})", hr_text(done))));
        }
    }
    Ok(())
}

fn item_path(item: &Com) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    unsafe {
        let mut p: PWSTR = std::ptr::null_mut();
        if (item.vtbl::<ShellItemVtbl>().get_display_name)(item.0, SIGDN_FILESYSPATH, &mut p) < 0 || p.is_null() {
            return None;
        }
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        use std::os::windows::ffi::OsStringExt;
        let out = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(p, len)));
        CoTaskMemFree(p as *const _);
        Some(out)
    }
}

/// The folder picker (modal on `owner`), titled `title`. None: cancelled.
pub fn pick_folder(owner: isize, start: Option<&Path>, title: &str) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::FileOpenDialog;
    let _com = Apartment::enter();
    let d = create(&FileOpenDialog, &IID_IFILEDIALOG).ok()?;
    unsafe {
        let v = d.vtbl::<FileDialogVtbl>();
        let mut opts = 0u32;
        (v.get_options)(d.0, &mut opts);
        (v.set_options)(d.0, opts | 0x20 | 0x40 | 0x800 | 0x8); // PICKFOLDERS FORCEFILESYSTEM PATHMUSTEXIST NOCHANGEDIR
        let title = crate::host::wide(title);
        (v.set_title)(d.0, title.as_ptr());
        if let Some(s) = start
            && let Ok(f) = shell_item(s)
        {
            (v.set_folder)(d.0, f.0);
        }
        if (v.show)(d.0, owner) < 0 {
            return None;
        }
        let mut item: *mut c_void = std::ptr::null_mut();
        if (v.get_result)(d.0, &mut item) < 0 {
            return None;
        }
        item_path(&Com(item))
    }
}

/// The save picker for a CSV (modal on `owner`). None: cancelled.
pub fn save_csv(owner: isize, name: &str, start: Option<&Path>) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::FileSaveDialog;
    let _com = Apartment::enter();
    let d = create(&FileSaveDialog, &IID_IFILEDIALOG).ok()?;
    unsafe {
        let v = d.vtbl::<FileDialogVtbl>();
        let mut opts = 0u32;
        (v.get_options)(d.0, &mut opts);
        (v.set_options)(d.0, opts | 0x2 | 0x40 | 0x8); // OVERWRITEPROMPT FORCEFILESYSTEM NOCHANGEDIR
        let (fname, fspec) = (crate::host::wide("CSV (comma separated)"), crate::host::wide("*.csv"));
        let spec = [FilterSpec { name: fname.as_ptr(), spec: fspec.as_ptr() }];
        (v.set_file_types)(d.0, 1, spec.as_ptr());
        let ext = crate::host::wide("csv");
        (v.set_default_extension)(d.0, ext.as_ptr());
        let n = crate::host::wide(name);
        (v.set_file_name)(d.0, n.as_ptr());
        let title = crate::host::wide("Save the scan as CSV");
        (v.set_title)(d.0, title.as_ptr());
        if let Some(s) = start
            && let Ok(f) = shell_item(s)
        {
            (v.set_folder)(d.0, f.0);
        }
        if (v.show)(d.0, owner) < 0 {
            return None;
        }
        let mut item: *mut c_void = std::ptr::null_mut();
        if (v.get_result)(d.0, &mut item) < 0 {
            return None;
        }
        item_path(&Com(item))
    }
}

/// Open a file with its program, or a folder in Explorer.
pub fn open(path: &Path) -> Result<(), String> {
    crate::host::open_path(path)
}

/// Explorer with the item selected in its folder (a drive or the scan's root: opened).
pub fn reveal(path: &Path) -> Result<(), String> {
    let s = path.as_os_str().to_string_lossy();
    let root = s.len() <= 3 && s.ends_with('\\');
    crate::host::reveal(path, !root)
}
