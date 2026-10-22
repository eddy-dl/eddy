//! The cookies.txt for sites that need a login (Settings → Downloads):
//! yt-dlp gets it with --cookies, and the TikTok and Instagram extractors
//! read their site's cookies from it. The user picks one exported from
//! their browser (Netscape format); Eddy keeps a copy in its data folder.

use crate::{AppSettings, DownloaderApp};
use slint::ComponentHandle;
use std::collections::BTreeSet;

/// Hooks up the Settings row. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    let settings = ui.global::<AppSettings>();
    let ui_weak = ui.as_weak();
    settings.on_cookies_add(move || {
        let ui_weak = ui_weak.clone();
        crate::platform::pick_cookies_file(move |path, _name| {
            let result = import(std::path::Path::new(&path));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let settings = ui.global::<AppSettings>();
                    settings.set_cookies_error(result.err().unwrap_or(0));
                    refresh(&settings);
                }
            });
        });
    });
    let ui_weak = ui.as_weak();
    settings.on_cookies_remove(move || {
        let _ = std::fs::remove_file(crate::extractors::http::cookie_file());
        if let Some(ui) = ui_weak.upgrade() {
            let settings = ui.global::<AppSettings>();
            settings.set_cookies_error(0);
            refresh(&settings);
        }
    });
    refresh(&settings);
}

fn refresh(settings: &AppSettings) {
    let (count, sites) = summary(&std::fs::read_to_string(crate::extractors::http::cookie_file()).unwrap_or_default());
    settings.set_cookies_count(count as i32);
    // A few, so the row stays one line.
    let shown: Vec<&str> = sites.iter().take(3).map(String::as_str).collect();
    let more = sites.len().saturating_sub(shown.len());
    let mut text = shown.join(", ");
    if more > 0 {
        text += &format!(" +{more}");
    }
    settings.set_cookies_sites(text.into());
}

/// How many cookies a Netscape cookies.txt holds, and for which sites.
fn summary(text: &str) -> (usize, BTreeSet<String>) {
    let mut count = 0;
    let mut sites = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 7 {
            count += 1;
            let host = parts[0].trim_start_matches('.');
            // The last two labels: "www.instagram.com" → "instagram.com".
            let labels: Vec<&str> = host.rsplitn(3, '.').collect();
            sites.insert(labels.iter().take(2).rev().copied().collect::<Vec<_>>().join("."));
        }
    }
    (count, sites)
}

/// Keeps a copy of the picked file, if it's a cookies.txt. The error is
/// AppSettings.cookies_error's code.
fn import(path: &std::path::Path) -> Result<(), i32> {
    let text = std::fs::read_to_string(path).map_err(|_| 1)?;
    if summary(&text).0 == 0 {
        return Err(2);
    }
    let target = crate::extractors::http::cookie_file();
    if let Some(dir) = target.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&target, text).map_err(|e| {
        log::warn!("couldn't save cookies.txt: {e}");
        3
    })
}
