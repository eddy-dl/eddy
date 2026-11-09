//! iOS: yt-dlp and gallery-dl kept up to date (Settings → Downloads →
//! External packages). The app bundles them (app_packages, read-only);
//! newer ones from PyPI go in the app's data folder, ahead of those on
//! Python's path (platform::python). Both are pure Python, so a newer
//! version is just its wheels. Sideloaded builds only: the App Store doesn't
//! allow an app to download code.

use crate::{AppSettings, DownloaderApp, HelperPackage};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Each updatable package: its Settings key, Python module, and the wheels
/// an update downloads (yt-dlp-ejs goes with yt-dlp: its solver scripts
/// must be the version yt-dlp expects).
const PACKAGES: [(&str, &str, &[&str]); 2] =
    [("ytdlp", "yt_dlp", &["yt-dlp", "yt-dlp-ejs"]), ("gallerydl", "gallery_dl", &["gallery-dl"])];

/// Modules that go with a package's, removed with it.
fn modules(module: &str) -> &'static [&'static str] {
    if module == "yt_dlp" { &["yt_dlp", "yt_dlp_ejs"] } else { &["gallery_dl"] }
}

thread_local! {
    static UI: RefCell<Option<slint::Weak<DownloaderApp>>> = const { RefCell::new(None) };
}

/// What PyPI says is newest, per key (None: not asked yet).
static LATEST: Mutex<Option<Result<HashMap<String, (String, u64)>, String>>> = Mutex::new(None);
/// Updates in progress: key → fraction done.
static UPDATING: Mutex<Option<HashMap<String, f32>>> = Mutex::new(None);

/// Where updates go (on Python's path before the bundled packages).
pub fn updates_dir() -> PathBuf {
    PathBuf::from(super::get_app_data_dir()).join("python-packages")
}

fn bundled_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.join("app_packages"))).unwrap_or_default()
}

/// A package's version, from its version.py (`__version__ = '…'`).
fn version_in(dir: &Path, module: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(module).join("version.py")).ok()?;
    let line = text.lines().find(|l| l.trim_start().starts_with("__version__"))?;
    let value = line.split('=').nth(1)?.trim().trim_matches(|c| c == '\'' || c == '"');
    (!value.is_empty()).then(|| value.to_string())
}

/// "2026.10.26" < "2026.10.26.1" < "2026.11.2".
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect() };
    parts(a) > parts(b)
}

/// The version that runs: a downloaded one if it's newer than the app's.
fn installed(module: &str) -> Option<String> {
    let bundled = version_in(&bundled_dir(), module);
    match (version_in(&updates_dir(), module), bundled) {
        (Some(update), Some(bundled)) if newer(&update, &bundled) => Some(update),
        (_, bundled) => bundled,
    }
}

/// Drops downloaded packages no newer than the app's own (the app was
/// updated since), before Python starts. The folder is made if missing, so
/// Python's path has it from the start.
pub fn prune() {
    let updates = updates_dir();
    let _ = std::fs::create_dir_all(&updates);
    for (_, module, _) in PACKAGES {
        let Some(update) = version_in(&updates, module) else {
            continue;
        };
        if version_in(&bundled_dir(), module).is_some_and(|bundled| !newer(&update, &bundled)) {
            remove(&updates, module);
        }
    }
}

/// A package's modules and their dist-info folders, from `dir`.
fn remove(dir: &Path, module: &str) {
    for name in modules(module) {
        let _ = std::fs::remove_dir_all(dir.join(name));
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(Result::ok) {
                let file = entry.file_name().to_string_lossy().to_lowercase();
                if file.starts_with(&format!("{name}-")) && file.ends_with(".dist-info") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }
}

/// Hooks the Settings rows up. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    let settings = ui.global::<AppSettings>();
    settings.on_helpers_check(check);
    settings.on_helper_download(|key| update(key.to_string()));
    refresh();
}

fn updating() -> HashMap<String, f32> {
    UPDATING.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

fn set_updating(key: &str, progress: Option<f32>) {
    let mut guard = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    match progress {
        Some(p) => map.insert(key.to_string(), p),
        None => map.remove(key),
    };
}

/// The rows (UI thread): what's installed, and an update when there's one.
fn refresh() {
    let latest = LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let in_progress = updating();
    let rows: Vec<HelperPackage> = PACKAGES
        .iter()
        .map(|(key, module, _)| {
            let version = installed(module).unwrap_or_default();
            let release = latest.as_ref().and_then(|l| l.as_ref().ok()).and_then(|l| l.get(*key).cloned());
            let state = match (in_progress.get(*key), &release, &latest) {
                (Some(p), ..) => (4, *p),
                (None, Some((newest, _)), _) if newer(newest, &version) => (3, 0.0),
                (None, _, Some(Err(_))) => (7, 0.0),
                (None, None, None) => (0, 0.0),
                _ => (2, 0.0),
            };
            HelperPackage {
                key: (*key).into(),
                installed: version.into(),
                latest: release.as_ref().map(|r| r.0.clone()).unwrap_or_default().into(),
                size: release.as_ref().map(|r| crate::links::format_size(r.1)).unwrap_or_default().into(),
                state: state.0,
                progress: state.1,
            }
        })
        .collect();
    UI.with(|u| {
        if let Some(ui) = u.borrow().as_ref().and_then(|w| w.upgrade()) {
            ui.global::<AppSettings>().set_python_tools(ModelRc::new(VecModel::from(rows)));
        }
    });
}

/// Asks PyPI for the newest versions (the packages page opened).
fn check() {
    crate::downloader::runtime().spawn(async {
        let mut found = HashMap::new();
        let mut failed = None;
        for (key, _, wheels) in PACKAGES {
            match crate::gallerydl::wheel(wheels[0]).await {
                Ok((version, _, size)) => {
                    found.insert(key.to_string(), (version, size));
                }
                Err(e) => failed = Some(e),
            }
        }
        let result = match failed {
            Some(e) if found.is_empty() => {
                log::warn!(target: "ytdlp", "couldn't check for package updates: {e}");
                Err(e)
            }
            _ => Ok(found),
        };
        *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        let _ = slint::invoke_from_event_loop(refresh);
    });
}

/// Downloads `key`'s newest wheels into the updates folder (the user
/// confirmed), then tells Python they changed.
fn update(key: String) {
    let Some((_, module, wheels)) = PACKAGES.iter().find(|(k, ..)| *k == key) else {
        return;
    };
    let (module, wheels) = (*module, *wheels);
    set_updating(&key, Some(0.0));
    refresh();
    crate::downloader::runtime().spawn(async move {
        let result = fetch(&key, module, wheels).await;
        set_updating(&key, None);
        match result {
            Ok(version) => {
                super::python::packages_changed();
                let message = format!("{} {version} installed", if key == "ytdlp" { "yt-dlp" } else { "gallery-dl" });
                let _ = slint::invoke_from_event_loop(move || super::show_toast(&message));
            }
            Err(e) => {
                log::error!(target: "ytdlp", "couldn't update {key}: {e}");
                let message = format!("Couldn't update: {e}");
                let _ = slint::invoke_from_event_loop(move || super::show_toast(&message));
            }
        }
        let _ = slint::invoke_from_event_loop(refresh);
    });
}

async fn fetch(key: &str, module: &str, names: &[&str]) -> Result<String, String> {
    let updates = updates_dir();
    let downloads = updates.join(".download");
    let mut wheels = Vec::new();
    for name in names {
        let (version, url, size) = crate::gallerydl::wheel(name).await?;
        let file = url.rsplit('/').next().unwrap_or(name).to_string();
        wheels.push((version, url, size, file));
    }
    let version = wheels[0].0.clone();
    let total = wheels.iter().map(|w| w.2).sum::<u64>().max(1) as f32;
    let mut done = 0;
    let mut files = Vec::new();
    for (_, url, size, file) in &wheels {
        crate::downloader::fetch_file(url, &downloads, file, |completed, _| {
            set_updating(key, Some((done + completed.min(*size)) as f32 / total));
            let _ = slint::invoke_from_event_loop(refresh);
        })
        .await?;
        done += size;
        files.push(downloads.join(file));
    }
    let module = module.to_string();
    let unpacked = tokio::task::spawn_blocking(move || -> Result<(), String> {
        // In place of the version before (only this package's own files).
        remove(&updates, &module);
        for file in &files {
            let opened = std::fs::File::open(file).map_err(|e| e.to_string())?;
            zip::ZipArchive::new(opened).and_then(|mut zip| zip.extract(&updates)).map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?;
    let _ = tokio::fs::remove_dir_all(&downloads).await;
    unpacked?;
    Ok(version)
}
