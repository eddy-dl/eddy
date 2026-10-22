//! Settings → Advanced → Clear download cache: the files Eddy keeps only to
//! save work and can make again. Link preview thumbnails, yt-dlp's saved
//! link info, leftovers of size checks, and on Android the app's cache
//! folder (downloaded helper installers, picked files' copies, Deno's cache,
//! temporary files). Saved .torrent files (pending links use them), files
//! still being copied into a picked folder, and desktop's installed tools
//! stay.

use crate::{AppSettings, DownloaderApp};
use slint::ComponentHandle;
use std::path::{Path, PathBuf};

/// Hooks up the Settings row. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    let settings = ui.global::<AppSettings>();
    let ui_weak = ui.as_weak();
    settings.on_cache_refresh(move || refresh(ui_weak.clone()));
    let ui_weak = ui.as_weak();
    settings.on_cache_clear(move || {
        // Some of it may be in use while something downloads.
        if crate::downloader::active_count() > 0 {
            return;
        }
        if let Some(ui) = ui_weak.upgrade() {
            ui.global::<AppSettings>().set_cache_busy(true);
        }
        let ui_weak = ui_weak.clone();
        std::thread::spawn(move || {
            for path in paths() {
                let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
                let _ = if is_dir { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            }
            refresh(ui_weak);
        });
    });
}

/// Measures the cache (off the UI thread) and shows it.
fn refresh(ui_weak: slint::Weak<DownloaderApp>) {
    std::thread::spawn(move || {
        let size: u64 = paths().iter().map(|p| size_of(p)).sum();
        let busy = crate::downloader::active_count() > 0;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let settings = ui.global::<AppSettings>();
                settings.set_cache_size(crate::links::format_size(size).into());
                settings.set_cache_empty(size == 0);
                settings.set_downloads_running(busy);
                settings.set_cache_busy(false);
            }
        });
    });
}

/// Everything the cache is made of (whichever exist).
fn paths() -> Vec<PathBuf> {
    let data = PathBuf::from(crate::platform::get_app_data_dir());
    let mut paths = vec![data.join("thumbs"), data.join("ytdlp-info")];
    // Size checks' folders, left behind if the app stopped mid-check.
    if let Ok(entries) = std::fs::read_dir(&data) {
        paths.extend(
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("eddy-probe-"))
                .map(|e| e.path()),
        );
    }
    // Android's cache folder, next to the files folder: its contents, not
    // the folder itself.
    #[cfg(target_os = "android")]
    if let Some(Ok(entries)) = data.parent().map(|p| std::fs::read_dir(p.join("cache"))) {
        paths.extend(entries.flatten().map(|e| e.path()));
    }
    paths.into_iter().filter(|p| p.exists()).collect()
}

/// A file's size, or a folder's (everything in it); symlinks aren't followed.
fn size_of(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        std::fs::read_dir(path).map(|e| e.flatten().map(|e| size_of(&e.path())).sum()).unwrap_or(0)
    } else {
        meta.len()
    }
}
