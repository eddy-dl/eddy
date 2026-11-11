//! Backup and restore (Settings → General): one .zip, "Eddy backup <date>
//! <time>.zip" in the download folder's Backups, holding Eddy's settings and
//! its download list (finished ones too, with each book's chapter list),
//! and, when asked for, the chapters fetched for books (they can be large).
//! Not cookies (the ones added in Settings, or a browser download's):
//! they're logins. Nor what's only true of one device (the download
//! folder, font files).
//!
//! Restoring merges: the backup's settings are taken, and its downloads not
//! in the lists already are added (paused, if they were running; in the
//! usual folder, if theirs isn't on this device). Fetched chapters are
//! added to the cache, never over what's there.
//!
//! The zip: `eddy-backup.json` ({"format", "version", "made", "settings":
//! {key: value}, "downloads": [record…]}), then `novels/…` as in Eddy's
//! data folder.

use crate::{AppSettings, DownloaderApp};
use slint::ComponentHandle;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MANIFEST: &str = "eddy-backup.json";
const FORMAT: u64 = 1;
/// Whether backups take the fetched chapters (Settings, off by default).
pub const CHAPTERS_KEY: &str = "backup_chapters";
/// Settings that only hold on the device they were made on.
const DEVICE_ONLY: [&str; 6] = ["download_dir_raw", "download_dir_display", "system_font_status", "ios_font", "ios_font_name", "logs_in_downloads"];

/// Hooks Settings' rows up. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    let settings = ui.global::<AppSettings>();
    settings.set_backup_chapters(crate::platform::get_pref(CHAPTERS_KEY, "false").is_ok_and(|v| v == "true"));
    settings.on_backup_chapters_changed(|on| {
        let _ = crate::platform::save_pref(CHAPTERS_KEY, if on { "true" } else { "false" });
    });
    settings.on_back_up(|| {
        let chapters = crate::platform::get_pref(CHAPTERS_KEY, "false").is_ok_and(|v| v == "true");
        crate::platform::show_toast("Backing up…");
        std::thread::spawn(move || match back_up(chapters) {
            Ok(path) => crate::platform::show_toast(&format!("Backed up to {}", shown(&path))),
            Err(e) => {
                log::warn!("backup: {e}");
                crate::platform::show_toast(&format!("Couldn't back up: {e}"));
            }
        });
    });
    let ui_weak = ui.as_weak();
    settings.on_restore(move || {
        let ui_weak = ui_weak.clone();
        crate::platform::pick_backup_file(move |path, _name| {
            if path.is_empty() {
                return;
            }
            std::thread::spawn(move || {
                let read = read_backup(Path::new(&path));
                // The picker's copy (Android's, in the cache) isn't needed now.
                #[cfg(target_os = "android")]
                let _ = std::fs::remove_file(&path);
                let ui_weak = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || match read {
                    Ok(backup) => apply(&backup, ui_weak),
                    Err(e) => {
                        log::warn!("restore: {e}");
                        crate::platform::show_toast(&format!("Couldn't restore: {e}"));
                    }
                });
            });
        });
    });
}

/// Where backups go: Backups in the download folder (a picked folder Eddy
/// can only copy into isn't used: the usual one is).
pub fn backups_dir() -> PathBuf {
    match crate::storage::resolve("") {
        crate::storage::SaveTarget::Dir(dir) => dir.join("Backups"),
        crate::storage::SaveTarget::Staged { .. } => crate::storage::default_dir().join("Backups"),
    }
}

/// `path` as the user knows it (Android: from Download).
fn shown(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    match text.find("/Download/") {
        Some(at) if cfg!(target_os = "android") => text[at + 1..].to_string(),
        _ => text,
    }
}

/// Writes a backup; returns its path.
fn back_up(chapters: bool) -> Result<PathBuf, String> {
    let settings: HashMap<String, String> =
        crate::platform::all_prefs().into_iter().filter(|(key, _)| !DEVICE_ONLY.contains(&key.as_str())).collect();
    let mut downloads: Vec<serde_json::Value> =
        crate::downloads::records().iter().filter_map(|record| serde_json::from_str(record).ok()).collect();
    // A download caught in the browser keeps that visit's cookies: logins,
    // like the rest, stay out.
    for download in &mut downloads {
        if let Some(cookies) = download.pointer_mut("/request/browser/cookies") {
            *cookies = serde_json::Value::String(String::new());
        }
    }
    let made = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let manifest = serde_json::json!({
        "format": FORMAT,
        "version": env!("CARGO_PKG_VERSION"),
        "made": made,
        "settings": settings,
        "downloads": downloads,
    });

    let dir = backups_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("Eddy backup {}.zip", stamp(made)));
    let part = dir.join(".backup.part");
    let file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(MANIFEST, options).map_err(|e| e.to_string())?;
    zip.write_all(manifest.to_string().as_bytes()).map_err(|e| e.to_string())?;
    if chapters {
        let novels = PathBuf::from(crate::platform::get_app_data_dir()).join("novels");
        for file in files_under(&novels) {
            let Ok(relative) = file.strip_prefix(&novels) else {
                continue;
            };
            let name = format!("novels/{}", relative.to_string_lossy().replace('\\', "/"));
            let data = std::fs::read(&file).map_err(|e| e.to_string())?;
            zip.start_file(name, options).map_err(|e| e.to_string())?;
            zip.write_all(&data).map_err(|e| e.to_string())?;
        }
    }
    zip.finish().map_err(|e| e.to_string())?;
    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// "2026-11-11 1530", in UTC (no time zone library here).
fn stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (hour, minute) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}{minute:02}")
}

/// Every file under `dir`, however deep.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// A backup as read (its chapters already added to the cache).
struct Backup {
    settings: HashMap<String, String>,
    downloads: Vec<serde_json::Value>,
    chapters: usize,
}

/// Reads a backup, adding its chapters to the cache on the way (off the UI
/// thread: there may be many).
fn read_backup(path: &Path) -> Result<Backup, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| "it isn't an Eddy backup".to_string())?;
    let manifest: serde_json::Value = {
        let mut entry = zip.by_name(MANIFEST).map_err(|_| "it isn't an Eddy backup".to_string())?;
        let mut text = String::new();
        entry.read_to_string(&mut text).map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())?
    };
    if manifest["format"].as_u64().is_none_or(|f| f > FORMAT) {
        return Err("it's from a newer Eddy: update Eddy first".to_string());
    }
    let settings = manifest["settings"]
        .as_object()
        .map(|map| map.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    let downloads = manifest["downloads"].as_array().cloned().unwrap_or_default();

    let novels = PathBuf::from(crate::platform::get_app_data_dir()).join("novels");
    let mut chapters = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        // Only inside novels/, never out of it.
        let Some(relative) = entry.enclosed_name().and_then(|p| p.strip_prefix("novels").ok().map(Path::to_path_buf)) else {
            continue;
        };
        let to = novels.join(relative);
        if entry.is_dir() || to.exists() {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&to).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        chapters += 1;
    }
    Ok(Backup { settings, downloads, chapters })
}

/// Takes a read backup's settings and downloads (UI thread).
fn apply(backup: &Backup, ui_weak: slint::Weak<DownloaderApp>) {
    for (key, value) in backup.settings.iter().filter(|(key, _)| !DEVICE_ONLY.contains(&key.as_str())) {
        if let Err(e) = crate::platform::save_pref(key, value) {
            log::warn!("restore: setting {key}: {e}");
        }
    }
    crate::preferences::load_preferences(ui_weak.clone());
    if let Some(ui) = ui_weak.upgrade() {
        ui.global::<AppSettings>().set_backup_chapters(crate::platform::get_pref(CHAPTERS_KEY, "false").is_ok_and(|v| v == "true"));
    }
    let added = crate::downloads::restore(&backup.downloads);
    let mut said = format!("Restored: settings, {added} downloads");
    if backup.chapters > 0 {
        said += &format!(", {} saved chapters", backup.chapters);
    }
    // Language and fonts are picked as Eddy starts.
    said += ". Some settings apply the next time Eddy opens";
    crate::platform::show_toast(&said);
}

#[cfg(test)]
mod tests {
    #[test]
    fn stamps() {
        assert_eq!(super::stamp(0), "1970-01-01 0000");
        // 2026-11-11 15:30 UTC.
        assert_eq!(super::stamp(1_794_411_000), "2026-11-11 1530");
        // A leap day.
        assert_eq!(super::stamp(951_782_400), "2000-02-29 0000");
    }
}
