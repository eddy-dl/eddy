//! Eddy's own folder browser (ui/components/FolderBrowserSheet.slint). On
//! Android with "All files access" it replaces Android's folder picker,
//! which refuses the top of internal storage and of Download (Android
//! 11+), whatever the app may do. The chosen folder is a plain path, which
//! downloads write to directly (storage::resolve).

use crate::FolderBrowser;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// Internal storage, the top the browser goes up to.
pub const ROOT: &str = "/storage/emulated/0";

type Picked = Box<dyn Fn(String, String) + Send>;

struct Browsing {
    dir: PathBuf,
    on_picked: Picked,
}

thread_local! {
    static BROWSING: RefCell<Option<Browsing>> = const { RefCell::new(None) };
    /// Shows the browser's state in whichever window installed it.
    static SHOW: RefCell<Option<Box<dyn Fn(&View)>>> = const { RefCell::new(None) };
}

/// What the browser shows.
struct View {
    open: bool,
    path: String,
    folders: Vec<SharedString>,
    can_up: bool,
    error: String,
}

/// Hooks the browser up in `ui` (the app or the share popup). Call once.
pub fn install<T>(ui: &T)
where
    T: ComponentHandle + 'static,
    for<'a> FolderBrowser<'a>: slint::Global<'a, T>,
{
    let browser = ui.global::<FolderBrowser>();
    browser.on_enter(|name| change(|dir| Some(dir.join(name.as_str()))));
    browser.on_up(|| change(|dir| dir.parent().filter(|p| p.starts_with(ROOT)).map(Path::to_path_buf)));
    browser.on_cancel(close);
    browser.on_choose(|| {
        let chosen = BROWSING.with(|b| b.borrow_mut().take());
        if let Some(Browsing { dir, on_picked }) = chosen {
            let path = dir.to_string_lossy().into_owned();
            on_picked(path.clone(), display(&dir));
        }
        show_closed();
    });
    browser.on_create(|name| {
        let name = crate::novels::clean::safe_file_name(name.trim());
        let error = BROWSING.with(|b| {
            let b = b.borrow();
            let browsing = b.as_ref()?;
            let new = browsing.dir.join(&name);
            if new.exists() {
                return Some(format!("There's already a \"{name}\" here"));
            }
            std::fs::create_dir(&new).err().map(|e| format!("Couldn't make it: {e}"))
        });
        match error {
            Some(error) => render(&error),
            None => change(|dir| Some(dir.join(&name))),
        }
    });

    let ui = ui.as_weak();
    SHOW.with(|s| {
        *s.borrow_mut() = Some(Box::new(move |view: &View| {
            if let Some(ui) = ui.upgrade() {
                let browser = ui.global::<FolderBrowser>();
                browser.set_path(view.path.clone().into());
                browser.set_folders(ModelRc::new(VecModel::from(view.folders.clone())));
                browser.set_can_up(view.can_up);
                browser.set_error(view.error.clone().into());
                browser.set_open(view.open);
            }
        }))
    });
}

/// Opens the browser at `start` (or Download), calling `on_picked(path,
/// shown name)` once a folder is chosen. Must run on the UI thread.
pub fn browse(start: Option<&str>, on_picked: impl Fn(String, String) + Send + 'static) {
    let start = start
        .map(PathBuf::from)
        .filter(|p| p.starts_with(ROOT) && p.is_dir())
        .unwrap_or_else(|| Path::new(ROOT).join("Download"));
    BROWSING.with(|b| *b.borrow_mut() = Some(Browsing { dir: start, on_picked: Box::new(on_picked) }));
    render("");
}

/// Moves to the folder `to` picks (if it's one that can be read).
fn change(to: impl FnOnce(&Path) -> Option<PathBuf>) {
    let moved = BROWSING.with(|b| {
        let mut b = b.borrow_mut();
        let browsing = b.as_mut()?;
        let next = to(&browsing.dir).filter(|p| std::fs::read_dir(p).is_ok())?;
        browsing.dir = next;
        Some(())
    });
    render(if moved.is_some() { "" } else { "That folder can't be opened" });
}

fn close() {
    BROWSING.with(|b| b.borrow_mut().take());
    show_closed();
}

fn show_closed() {
    let view = View { open: false, path: String::new(), folders: Vec::new(), can_up: false, error: String::new() };
    SHOW.with(|s| {
        if let Some(show) = s.borrow().as_ref() {
            show(&view);
        }
    });
}

/// Shows the folder being browsed, with `error` under it.
fn render(error: &str) {
    let view = BROWSING.with(|b| {
        let b = b.borrow();
        let dir = &b.as_ref()?.dir;
        let mut folders: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        folders.sort_by_key(|n| n.to_lowercase());
        Some(View {
            open: true,
            path: display(dir),
            folders: folders.into_iter().map(SharedString::from).collect(),
            can_up: dir != Path::new(ROOT),
            error: error.to_string(),
        })
    });
    if let Some(view) = view {
        SHOW.with(|s| {
            if let Some(show) = s.borrow().as_ref() {
                show(&view);
            }
        });
    }
}

/// "Internal storage/Download" for /storage/emulated/0/Download.
pub fn display(path: &Path) -> String {
    match path.strip_prefix(ROOT) {
        Ok(rest) if rest.as_os_str().is_empty() => "Internal storage".to_string(),
        Ok(rest) => format!("Internal storage/{}", rest.to_string_lossy()),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}
