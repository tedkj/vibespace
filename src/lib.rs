//! VibeSpace: what is eating your disk, in seconds. A VibeWare app: Rust and Slint on the software renderer,
//! one small exe, no runtime beside it.

pub mod check;
pub mod controller;
pub mod csvformat;
pub mod drives;
pub mod format;
pub mod host;
pub mod launch;
pub mod prefs;
pub mod relocate;
pub mod scan;
pub mod shell;
pub mod text;
pub mod tree;
pub mod treemap;
pub mod trust;
pub mod types;

use serde::{Deserialize, Serialize};
use slint::ComponentHandle;

slint::include_modules!();

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the window remembers between runs (prefs.json in %APPDATA%\com.tedkj.vibespace).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// "dark" or "light" once the user has picked one; until then the system's choice.
    pub theme: Option<String>,
    /// Folders and drives scanned lately, newest first (the first screen offers them again).
    pub recent: Vec<String>,
    /// What the window sorts and draws by: size on disk (what deleting frees) unless changed.
    pub metric: tree::Metric,
    /// The list shown: 0 folders, 1 types, 2 largest files.
    pub tab: u8,
}

impl Prefs {
    pub fn dark(&self) -> bool {
        match self.theme.as_deref() {
            Some("dark") => true,
            Some("light") => false,
            _ => host::system_dark(),
        }
    }
}

/// Bring the window's toast model to `want` in place, by id (X15RFTP): a toast whose id is gone
/// is removed, one that changed is set, a new one is added at its place. A toast that stays is
/// never removed and put back, so Slint keeps its element and a screen reader does not read a
/// standing error out again each time another toast comes or goes.
pub fn sync_toasts(model: &slint::VecModel<ToastItem>, want: Vec<ToastItem>) {
    use slint::Model;
    let mut i = 0;
    while i < model.row_count() {
        let id = model.row_data(i).map(|t| t.id);
        if want.iter().any(|w| Some(w.id) == id) {
            i += 1;
        } else {
            model.remove(i);
        }
    }
    for (at, w) in want.into_iter().enumerate() {
        match model.row_data(at) {
            Some(have) if have.id == w.id => {
                if have != w {
                    model.set_row_data(at, w);
                }
            }
            _ => model.insert(at.min(model.row_count()), w),
        }
    }
}

/// The window with its basics wired: theme, About, copying. The app's own controller adds the
/// rest (the theme switch too: it saves on the prefs thread).
pub fn window(prefs: prefs::Store<Prefs>) -> Result<AppWindow, slint::PlatformError> {
    let ui = AppWindow::new()?;
    let p = prefs.load();
    ui.global::<Theme>().set_dark(p.dark());
    ui.set_version(VERSION.into());
    ui.on_copy_text(|t| host::copy_text(&t).is_ok());
    Ok(ui)
}
