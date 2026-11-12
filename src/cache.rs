//! Settings → Advanced → Clear download cache: the files Eddy keeps only to
//! save work and can make again. Link preview thumbnails, yt-dlp's saved
//! link info, TikTok's page data, leftovers of size checks, and on Android
//! the app's cache folder (downloaded helper installers, picked files'
//! copies, Deno's cache, temporary files). Saved .torrent files (pending links use them), files
//! still being copied into a picked folder, and desktop's installed tools
//! stay. Next to it, Book images: the pictures kept for books
//! (novels::images), cleared on their own.

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
    // Books' kept pictures, apart: clearing them means fetching them again.
    let ui_weak = ui.as_weak();
    settings.on_book_images_clear(move || {
        // A book being made reads and writes them.
        if crate::downloader::active_count() > 0 {
            return;
        }
        if let Some(ui) = ui_weak.upgrade() {
            ui.global::<AppSettings>().set_book_images_busy(true);
        }
        let ui_weak = ui_weak.clone();
        std::thread::spawn(move || {
            for dir in book_image_dirs() {
                let _ = std::fs::remove_dir_all(&dir);
            }
            refresh(ui_weak);
        });
    });
}

/// Measures the cache (off the UI thread) and shows it.
fn refresh(ui_weak: slint::Weak<DownloaderApp>) {
    std::thread::spawn(move || {
        let size: u64 = paths().iter().map(|p| size_of(p)).sum();
        let images: u64 = book_image_dirs().iter().map(|p| size_of(p)).sum();
        let busy = crate::downloader::active_count() > 0;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let settings = ui.global::<AppSettings>();
                settings.set_cache_size(crate::links::format_size(size).into());
                settings.set_cache_empty(size == 0);
                settings.set_downloads_running(busy);
                settings.set_cache_busy(false);
                settings.set_book_images_size(crate::links::format_size(images).into());
                settings.set_book_images_empty(images == 0);
                settings.set_book_images_busy(false);
            }
        });
    });
}

/// Everything the cache is made of (whichever exist).
fn paths() -> Vec<PathBuf> {
    let data = PathBuf::from(crate::platform::get_app_data_dir());
    let mut paths = vec![data.join("thumbs"), data.join("ytdlp-info"), data.join("tiktok")];
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

/// Each book's kept pictures (novels/<site>/<book>/images, see
/// novels::images); the chapters beside them stay.
fn book_image_dirs() -> Vec<PathBuf> {
    let novels = PathBuf::from(crate::platform::get_app_data_dir()).join("novels");
    let folders = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir).map(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default()
    };
    folders(novels.as_path()).iter().flat_map(|site| folders(site.as_path())).map(|book| book.join("images")).filter(|images| images.is_dir()).collect()
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
