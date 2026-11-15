// VibeSpace: one process, no browser engine, the software renderer.
#![windows_subsystem = "windows"]

use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use slint::{ComponentHandle, Timer};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;
use vibespace::controller::{App, Deps, RealShell};
use vibespace::host;

thread_local! {
    /// The controller, for the wake-ups posted from other threads.
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// The window's handle, for the pickers and the shell's prompts to sit on.
fn hwnd(ui: &vibespace::AppWindow) -> isize {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    ui.window()
        .with_winit_window(|w| match w.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        })
        .unwrap_or(0)
}

/// The real release-list fetcher (WinHTTP), the exe's alone: it is in no test build, not the
/// lib's, not the integration tests', not this exe's own unit tests', so no test can go online
/// (review RAB-2; tests/pe.rs checks this line).
#[cfg(not(test))]
mod net;

/// The window's Win32 handle, for `host::fit_to_work_area`.
fn hwnd_of(w: &slint::winit_030::winit::window::Window) -> Option<isize> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match w.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // DLL hardening, first of all (VibeLink LINK-1): every DLL loaded from here on comes from
    // System32, never from the exe's own folder (the per-user install folder, which the user's
    // own programs can write). The static imports are covered by the link flag
    // /DEPENDENTLOADFLAG:0x800 (build.rs).
    // SAFETY: a process-wide setting with a valid flag; nothing has been loaded by name yet.
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
    // No Windows "No Disk" box: a file call on an empty card reader or drive gets its error back
    // instead of a modal box (review fix-popup, RX1). tests/pe.rs checks this block comes next.
    // SAFETY: a process-wide setting with a valid flag, the bits already set kept.
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{GetErrorMode, SEM_FAILCRITICALERRORS, SetErrorMode};
        SetErrorMode(GetErrorMode() | SEM_FAILCRITICALERRORS);
    }
    // The command line: `VibeSpace.exe [<path>]`, one folder (or file) to scan. Several items
    // dropped on the exe: the first is scanned, and a toast says the others were left out. An
    // option (VibeSpace takes none) or anything else ends the process before any window or file
    // (exit code 2, the family rule).
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let (arg, left_out) = match vibespace::launch::start(&raw) {
        vibespace::launch::Start::Window => (None, None),
        vibespace::launch::Start::Scan(path, note) => (Some(path), note),
        vibespace::launch::Start::Refused => std::process::exit(2),
    };
    #[allow(unused_mut)]
    let mut paths = vibespace::host::paths();
    // Live checks and measurements only: the app's folders in a place of the developer's choosing,
    // so nothing touches the real profile. Never in a shipped build.
    #[cfg(feature = "dev-hooks")]
    if let Some(home) = std::env::var_os("VIBESPACE_DEV_HOME").map(std::path::PathBuf::from) {
        paths = vibespace::host::Paths { config: home.join("config"), local_data: home.join("local") };
    }
    // The probe hands back the value it was asked about, and `main` goes on with that one only: the
    // old binding is gone (`dev_probe`, review RRF-2).
    #[cfg(feature = "dev-hooks")]
    let paths = dev_probe(paths);
    let prefs = vibespace::prefs::Store::new(paths.config.join("prefs.json"));
    let ui = vibespace::window(prefs.clone())?;

    // The one place that names the real release-list fetcher (tests/pe.rs checks it): every test
    // hands the controller a fake, so no test goes online. This exe's unit-test build has no
    // `net` and never runs `main`: it gets a stand-in that fetches nothing.
    #[cfg(not(test))]
    let fetch = std::sync::Arc::new(net::fetch_release_list);
    #[cfg(test)]
    let fetch = std::sync::Arc::new(|_: &str| vibespace::check::Fetched::Failed("a test build has no fetcher".into()));
    let owner = Arc::new(AtomicIsize::new(0));
    let (pick_ui, save_ui, dest_ui) = (ui.as_weak(), ui.as_weak(), ui.as_weak());
    // The exe's identity as this process started from it, taken now, before an install could
    // replace it, and handed to every check (FED3, RVD7).
    let started = vibespace::trust::exe_identity();
    let deps = Deps {
        prefs: prefs.clone(),
        wake: Arc::new(|| {
            let _ = slint::invoke_from_event_loop(|| {
                APP.with(|a| {
                    let app = a.borrow().clone();
                    if let Some(app) = app {
                        app.pump();
                    }
                })
            });
        }),
        pick_folder: Box::new(move |start| {
            let ui = pick_ui.upgrade()?;
            vibespace::shell::pick_folder(hwnd(&ui), start.as_deref(), "Pick a folder to scan")
        }),
        // Where a folder moves to: opens where the picker was last (never on the scan's folder,
        // which may be a share that does not answer).
        pick_dest: Box::new(move || {
            let ui = dest_ui.upgrade()?;
            vibespace::shell::pick_folder(hwnd(&ui), None, "Pick where it goes")
        }),
        relocate_fs: Arc::new(vibespace::relocate::fs::Real),
        relocate_env: Arc::new(vibespace::relocate::Env::system(&paths.local_data, &paths.config)),
        save_csv: Box::new(move |name, start| {
            let ui = save_ui.upgrade()?;
            vibespace::shell::save_csv(hwnd(&ui), name, start.as_deref())
        }),
        csv_format: Box::new(vibespace::host::csv_format),
        copy: Box::new(|t| vibespace::host::copy_text(t).is_ok()),
        shell: Arc::new(RealShell { owner: owner.clone() }),
        drives: Arc::new(vibespace::drives::list),
        drive_info: Arc::new(vibespace::drives::info),
        // The shell can take a moment: never on the window's thread.
        open_url: Box::new(|u| {
            let u = u.to_string();
            std::thread::spawn(move || {
                let _ = vibespace::host::open_url(&u);
            });
        }),
        inspect: Arc::new(move |installed: &bool| vibespace::trust::inspect_from(started, &vibespace::trust::own_exe(), *installed)),
        fetch,
        release: vibespace::check::Release::this(),
    };
    let app = App::new(&ui, deps);
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    app.boot();

    // `VibeSpace.exe <path>` (read at the top): scanned at once.
    if let Some(p) = arg {
        app.open_path(PathBuf::from(p));
    }
    if let Some(note) = left_out {
        app.toast(note, vibespace::ToastKind::Info);
    }

    // A folder dragged in from Explorer: highlighted while it hovers, scanned on drop (the
    // first one, when several come, and a toast says the others were left out).
    let dropped: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    // The window opens at its preferred size; on a small or highly scaled screen that runs past
    // the taskbar, so the first window event once it is on screen fits it into the monitor's work
    // area (X15A11Y-3), once.
    let fitted = std::cell::Cell::new(false);
    {
        let (app, weak, dropped) = (app.clone(), ui.as_weak(), dropped.clone());
        ui.window().on_winit_window_event(move |window, event| {
            if !fitted.get() && window.with_winit_window(hwnd_of).flatten().and_then(host::fit_to_work_area).is_some() {
                fitted.set(true);
            }
            match event {
                WindowEvent::HoveredFile(_) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_drop_hover(true);
                    }
                }
                WindowEvent::HoveredFileCancelled => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_drop_hover(false);
                    }
                }
                WindowEvent::DroppedFile(path) => {
                    let first = dropped.borrow().is_empty();
                    dropped.borrow_mut().push(path.clone());
                    if first {
                        let (app, weak, dropped) = (app.clone(), weak.clone(), dropped.clone());
                        Timer::single_shot(Duration::from_millis(0), move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_drop_hover(false);
                            }
                            let files = std::mem::take(&mut *dropped.borrow_mut());
                            app.open_dropped(files);
                        });
                    }
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }

    // The window exists once the loop runs: its handle then, for the shell's prompts.
    {
        let (weak, owner) = (ui.as_weak(), owner.clone());
        Timer::single_shot(Duration::from_millis(0), move || {
            if let Some(ui) = weak.upgrade() {
                owner.store(hwnd(&ui), Ordering::Relaxed);
            }
        });
    }

    let ran = ui.run();
    app.close_prefs(); // the changes of the last moments, still on their way to disk
    APP.with(|a| *a.borrow_mut() = None);
    prefs.flush();
    ran
}

/// Live checks only (review RRE-5, RRF-2): with VIBESPACE_DEV_PROBE set, `main` writes the
/// app's data folders, one a line on stdout, and ends (exit 0) right after the dev home, before
/// any of them is used and before any window. Else it hands back the value it was given,
/// unchanged, and `main` goes on with it (`let paths = dev_probe(paths);`), so the folders the
/// probe names are the ones `main` uses. tests/pe.rs asks this before it starts the exe for real
/// (`run_exe`): every folder must be inside the scratch dev home it gave, so an override that is
/// gone, compiled out or too late fails there, by what the exe does.
#[cfg(feature = "dev-hooks")]
fn dev_probe(paths: vibespace::host::Paths) -> vibespace::host::Paths {
    use std::io::Write;
    if std::env::var_os("VIBESPACE_DEV_PROBE").is_some() {
        let mut out = std::io::stdout().lock();
        for f in [&paths.config, &paths.local_data] {
            let _ = writeln!(out, "{}", f.display());
        }
        let _ = out.flush();
        std::process::exit(0);
    }
    paths
}
