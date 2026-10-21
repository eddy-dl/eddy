//! The yt-dlp helper apps on Android (Settings → Downloads): YTDLnis's
//! Python & yt-dlp, FFmpeg and Deno packages. Eddy never installs them
//! itself: when the user asks (and confirms), it downloads a package's APK
//! from the ytdlnis-packages releases into its cache and opens Android's
//! installer, where the user decides. Once installed, the cached APK is
//! deleted.

use crate::{AppSettings, DownloaderApp, HelperPackage};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

const RELEASES_URL: &str = "https://api.github.com/repos/deniscerri/ytdlnis-packages/releases";

/// The helpers, in the order Settings lists them: (key, release tag prefix).
const PACKAGES: [(&str, &str); 3] = [("python", "python-"), ("ffmpeg", "ffmpeg-"), ("deno", "deno-")];

/// A package's newest release build for this device.
#[derive(Clone, Debug)]
struct Release {
    version: String,
    url: String,
    size: u64,
}

/// What GitHub said: releases per package key, or why it couldn't say.
static RELEASES: Mutex<Option<Result<HashMap<String, Release>, String>>> = Mutex::new(None);
/// Downloads in progress: key → fraction done.
static DOWNLOADING: Mutex<Option<HashMap<String, f32>>> = Mutex::new(None);

thread_local! {
    static UI: RefCell<Option<slint::Weak<DownloaderApp>>> = const { RefCell::new(None) };
}

fn downloading() -> HashMap<String, f32> {
    DOWNLOADING.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

fn set_downloading(key: &str, progress: Option<f32>) {
    let mut guard = DOWNLOADING.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    match progress {
        Some(p) => {
            map.insert(key.to_string(), p);
        }
        None => {
            map.remove(key);
        }
    }
}

/// Hooks the Settings rows up. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    let settings = ui.global::<AppSettings>();
    settings.on_helpers_check(check);
    settings.on_helper_download(|key| download(key.to_string()));
    settings.on_helper_install(|key| install_cached(&key));
    refresh();
}

/// Kotlin looked at the installed helpers again (e.g. back from the
/// installer). May come from any thread.
pub fn runtime_changed() {
    let _ = slint::invoke_from_event_loop(refresh);
}

/// Where downloaded APKs wait to be installed: the app's cache folder (next
/// to its files folder), which Android may also clear when space runs low.
fn apk_dir() -> PathBuf {
    let files = PathBuf::from(crate::platform::get_app_data_dir());
    files.parent().map(|p| p.join("cache")).unwrap_or_else(|| files.join("cache")).join("apks")
}

fn apk_path(key: &str, release: &Release) -> PathBuf {
    apk_dir().join(format!("{key}-{}.apk", release.version))
}

/// Downloaded completely (same size as the release says).
fn is_cached(key: &str, release: &Release) -> bool {
    std::fs::metadata(apk_path(key, release)).is_ok_and(|m| m.len() == release.size)
}

/// "3.14.6" < "3.14.10", ignoring a leading "v".
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v').split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect()
    };
    parts(a) > parts(b)
}

/// Rebuilds the Settings rows (UI thread). Also drops cached APKs whose
/// version is now installed.
fn refresh() {
    let Some((installed, _)) = crate::platform::helper_apps() else {
        return; // not Android, or Kotlin hasn't looked yet
    };
    let releases = RELEASES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let in_progress = downloading();

    let rows: Vec<HelperPackage> = PACKAGES
        .iter()
        .map(|(key, _)| {
            let version = installed.get(*key).cloned();
            let release = match &releases {
                Some(Ok(map)) => map.get(*key).cloned(),
                _ => None,
            };
            // Installed now: its downloaded APK isn't needed any more.
            if let (Some(v), Some(r)) = (&version, &release) {
                if !newer(&r.version, v) {
                    let _ = std::fs::remove_file(apk_path(key, r));
                }
            }
            let state = if let Some(p) = in_progress.get(*key) {
                (4, *p)
            } else {
                let code = match (&version, &release, &releases) {
                    (Some(v), Some(r), _) if newer(&r.version, v) => {
                        if is_cached(key, r) { 5 } else { 3 }
                    }
                    (Some(_), _, _) => 2,
                    (None, Some(r), _) => {
                        if is_cached(key, r) { 5 } else { 1 }
                    }
                    (None, None, None) => 0,
                    (None, None, Some(Err(_))) => 7,
                    // Checked, but no build for this device.
                    (None, None, Some(Ok(_))) => 6,
                };
                (code, 0.0)
            };
            HelperPackage {
                key: (*key).into(),
                installed: version.unwrap_or_default().into(),
                latest: release.as_ref().map(|r| r.version.clone()).unwrap_or_default().into(),
                size: release.as_ref().map(|r| crate::links::format_size(r.size)).unwrap_or_default().into(),
                state: state.0,
                progress: state.1,
            }
        })
        .collect();

    UI.with(|u| {
        if let Some(ui) = u.borrow().as_ref().and_then(|w| w.upgrade()) {
            ui.global::<AppSettings>().set_helper_packages(ModelRc::new(VecModel::from(rows)));
        }
    });
}

/// When GitHub was last asked, so reopening Settings doesn't ask again.
static CHECKED_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Asks GitHub for the newest release of each package (Settings opened),
/// at most every 10 minutes.
fn check() {
    {
        let mut at = CHECKED_AT.lock().unwrap_or_else(|e| e.into_inner());
        let ok = matches!(*RELEASES.lock().unwrap_or_else(|e| e.into_inner()), Some(Ok(_)));
        if ok && at.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(600)) {
            refresh();
            return;
        }
        *at = Some(std::time::Instant::now());
    }
    let abi = crate::platform::helper_apps().map(|(_, abi)| abi).unwrap_or_default();
    crate::downloader::runtime().spawn(async move {
        let result = fetch_releases(&abi).await;
        if let Err(e) = &result {
            log::warn!("couldn't check the yt-dlp packages: {e}");
        }
        *RELEASES.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        let _ = slint::invoke_from_event_loop(refresh);
    });
}

async fn fetch_releases(abi: &str) -> Result<HashMap<String, Release>, String> {
    let text = crate::extractors::http::client()
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let releases: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| e.to_string())?;

    let mut found: HashMap<String, Release> = HashMap::new();
    // Newest first; keep each package's first (newest) build for this ABI.
    for release in &releases {
        let tag = release["tag_name"].as_str().unwrap_or_default();
        let Some((key, prefix)) = PACKAGES.iter().find(|(_, prefix)| tag.starts_with(prefix)) else {
            continue;
        };
        if found.contains_key(*key) {
            continue;
        }
        let asset = release["assets"].as_array().into_iter().flatten().find(|a| {
            a["name"].as_str().is_some_and(|n| n.contains(abi) && n.ends_with(".apk"))
        });
        if let Some(asset) = asset {
            found.insert(
                key.to_string(),
                Release {
                    version: tag.trim_start_matches(prefix).to_string(),
                    url: asset["browser_download_url"].as_str().unwrap_or_default().to_string(),
                    size: asset["size"].as_u64().unwrap_or(0),
                },
            );
        }
    }
    Ok(found)
}

/// Downloads a package's APK (the user confirmed), then opens the installer.
fn download(key: String) {
    let release = match RELEASES.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(Ok(map)) => map.get(&key).cloned(),
        _ => None,
    };
    let Some(release) = release else {
        return;
    };
    if is_cached(&key, &release) {
        install_cached(&key);
        return;
    }
    set_downloading(&key, Some(0.0));
    refresh();

    crate::downloader::runtime().spawn(async move {
        let result = fetch_apk(&key, &release).await;
        set_downloading(&key, None);
        match result {
            Ok(path) => {
                let path = path.to_string_lossy().into_owned();
                let _ = slint::invoke_from_event_loop(move || {
                    refresh();
                    crate::platform::install_apk(&path);
                });
            }
            Err(e) => {
                log::warn!("couldn't download the {key} package: {e}");
                let _ = slint::invoke_from_event_loop(move || {
                    refresh();
                    crate::platform::show_toast(&format!("Couldn't download the package: {e}"));
                });
            }
        }
    });
}

async fn fetch_apk(key: &str, release: &Release) -> Result<PathBuf, String> {
    use tokio::io::AsyncWriteExt;
    let dir = apk_dir();
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    // Older downloads of this package go.
    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with(&format!("{key}-")) {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }

    let path = apk_path(key, release);
    let partial = path.with_extension("part");
    let mut response = crate::extractors::http::client()
        .get(&release.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let total = response.content_length().unwrap_or(release.size).max(1);
    let mut file = tokio::fs::File::create(&partial).await.map_err(|e| e.to_string())?;
    let (mut done, mut shown) = (0u64, 0u32);
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        // Redraw the row once per percent.
        let percent = (done * 100 / total) as u32;
        if percent != shown {
            shown = percent;
            set_downloading(key, Some(done as f32 / total as f32));
            let _ = slint::invoke_from_event_loop(refresh);
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    tokio::fs::rename(&partial, &path).await.map_err(|e| e.to_string())?;
    Ok(path)
}

/// Opens the installer for an already downloaded APK.
fn install_cached(key: &str) {
    let release = match RELEASES.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(Ok(map)) => map.get(key).cloned(),
        _ => None,
    };
    if let Some(release) = release.filter(|r| is_cached(key, r)) {
        crate::platform::install_apk(&apk_path(key, &release).to_string_lossy());
    }
}
