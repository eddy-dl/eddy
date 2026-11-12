//! Eddy's own updates (Settings → About). Its releases on GitHub are asked
//! for the newest version past this one: when the app opens (once a day at
//! most, unless that's turned off) and when asked. Pre-releases count only
//! if Settings says so.
//!
//! An update is downloaded when the user asks (this system's file, checked
//! against the SHA-256 GitHub gives for it), then:
//! - Android: its APK is handed to Android's installer.
//! - Windows and Linux: the running program's file is swapped for it (the
//!   old one kept as ".old" until the next start) and Eddy starts again.
//! - macOS and iOS (an app bundle, a sideloaded app): Eddy can't put it in
//!   place itself, so it opens the release's page.

use crate::{AppSettings, DownloaderApp};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Mutex;

const RELEASES: &str = "https://api.github.com/repos/eddy-dl/eddy/releases?per_page=20";
const PAGE: &str = "https://github.com/eddy-dl/eddy/releases";
/// Whether Eddy looks as it opens ("false" for not), and takes pre-releases.
const CHECK_KEY: &str = "update_check";
const PRERELEASES_KEY: &str = "update_prereleases";
/// When it last looked (Unix seconds).
const CHECKED_AT_KEY: &str = "update_checked_at";
const CHECK_EVERY: u64 = 24 * 3600;

/// A release newer than this Eddy.
#[derive(Clone, Debug, PartialEq)]
struct Update {
    version: String,
    /// Its page on GitHub.
    page: String,
    /// This system's file in it, if it has one.
    file: Option<File>,
}

#[derive(Clone, Debug, PartialEq)]
struct File {
    name: String,
    url: String,
    size: u64,
    /// Hex, when GitHub gives it.
    sha256: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Idle,
    Checking,
    UpToDate,
    Found,
    Downloading(f32),
    /// Downloaded: to install, or (desktop) to start again into.
    Ready,
    Failed,
}

struct State {
    step: Step,
    update: Option<Update>,
    downloaded: Option<PathBuf>,
}

static STATE: Mutex<State> = Mutex::new(State { step: Step::Idle, update: None, downloaded: None });

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    f(&mut STATE.lock().unwrap_or_else(|e| e.into_inner()))
}

thread_local! {
    static UI: RefCell<Option<slint::Weak<DownloaderApp>>> = const { RefCell::new(None) };
}

fn pref_on(key: &str, default: bool) -> bool {
    crate::platform::get_pref(key, if default { "true" } else { "false" }).map_or(default, |v| v == "true")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Where an update is downloaded to.
fn updates_dir() -> PathBuf {
    PathBuf::from(crate::platform::get_app_data_dir()).join("updates")
}

/// Hooks Settings' rows up, and looks for an update if it's time. Call
/// once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    // What the last update left: its download, and (desktop) the program
    // it replaced.
    let _ = std::fs::remove_dir_all(updates_dir());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
    }

    let settings = ui.global::<AppSettings>();
    settings.set_update_check_on_start(pref_on(CHECK_KEY, true));
    settings.set_update_prereleases(pref_on(PRERELEASES_KEY, false));
    let ui_weak = ui.as_weak();
    settings.on_update_settings_changed(move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        let settings = ui.global::<AppSettings>();
        let _ = crate::platform::save_pref(CHECK_KEY, if settings.get_update_check_on_start() { "true" } else { "false" });
        let _ = crate::platform::save_pref(PRERELEASES_KEY, if settings.get_update_prereleases() { "true" } else { "false" });
    });
    settings.on_update_check(|| check(true));
    settings.on_update_download(download);
    settings.on_update_install(install_downloaded);
    settings.on_update_open_page(|| {
        let page = with_state(|s| s.update.as_ref().map(|u| u.page.clone())).unwrap_or_else(|| PAGE.to_string());
        crate::platform::open_url(&page);
    });

    let last = crate::platform::get_pref(CHECKED_AT_KEY, "0").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    if pref_on(CHECK_KEY, true) && now().saturating_sub(last) >= CHECK_EVERY {
        check(false);
    }
}

fn redraw() {
    let _ = slint::invoke_from_event_loop(refresh);
}

/// Settings' row from what's known (UI thread). The row's states: 0 not
/// looked, 1 looking, 2 up to date, 3 an update to download, 4
/// downloading, 5 downloaded, 6 an update to get from its page, 7 couldn't
/// look.
fn refresh() {
    let (code, progress, version, size) = with_state(|state| {
        let file = state.update.as_ref().and_then(|u| u.file.as_ref());
        let (code, progress) = match state.step {
            Step::Idle => (0, 0.0),
            Step::Checking => (1, 0.0),
            Step::UpToDate => (2, 0.0),
            Step::Found if file.is_some() && can_install() => (3, 0.0),
            Step::Found => (6, 0.0),
            Step::Downloading(done) => (4, done),
            Step::Ready => (5, 0.0),
            Step::Failed => (7, 0.0),
        };
        let version = state.update.as_ref().map(|u| u.version.clone()).unwrap_or_default();
        let size = file.filter(|f| f.size > 0).map(|f| crate::links::format_size(f.size)).unwrap_or_default();
        (code, progress, version, size)
    });
    UI.with(|u| {
        if let Some(ui) = u.borrow().as_ref().and_then(|w| w.upgrade()) {
            let settings = ui.global::<AppSettings>();
            settings.set_update_state(code);
            settings.set_update_progress(progress);
            settings.set_update_version(version.into());
            settings.set_update_size(size.into());
        }
    });
}

/// Whether Eddy can put an update in place itself here.
fn can_install() -> bool {
    cfg!(any(target_os = "android", target_os = "windows", target_os = "linux"))
}

/// This system's file in a release, as the releases name them.
fn file_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("android", "aarch64") => Some("eddy-android-arm64.apk"),
        ("android", "arm") => Some("eddy-android-armv7.apk"),
        ("android", _) => Some("eddy-android-universal.apk"),
        ("windows", "x86_64") => Some("eddy-windows-x86_64.exe"),
        ("windows", "aarch64") => Some("eddy-windows-arm64.exe"),
        ("linux", "x86_64") => Some("eddy-linux-x86_64"),
        ("linux", "aarch64") => Some("eddy-linux-arm64"),
        _ => None,
    }
}

/// "v0.2.1" or "0.2.1-beta" as numbers (what follows a '-' or '+' left out).
fn version_of(tag: &str) -> Option<Vec<u64>> {
    let core = tag.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let parts: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

/// The newest release past `current` in GitHub's list (drafts never,
/// pre-releases only if `prereleases`), with `wanted`'s file if it's in it.
fn newest(releases: &serde_json::Value, current: &str, prereleases: bool, wanted: Option<&str>) -> Option<Update> {
    let current = version_of(current)?;
    releases
        .as_array()?
        .iter()
        .filter(|r| r["draft"].as_bool() != Some(true) && (prereleases || r["prerelease"].as_bool() != Some(true)))
        .filter_map(|r| Some((version_of(r["tag_name"].as_str()?)?, r)))
        .filter(|(version, _)| *version > current)
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(version, release)| {
            let file = wanted.and_then(|name| {
                let asset = release["assets"].as_array()?.iter().find(|a| a["name"] == name)?;
                Some(File {
                    name: name.to_string(),
                    url: asset["browser_download_url"].as_str()?.to_string(),
                    size: asset["size"].as_u64().unwrap_or(0),
                    sha256: asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase),
                })
            });
            Update {
                version: version.iter().map(u64::to_string).collect::<Vec<_>>().join("."),
                page: release["html_url"].as_str().unwrap_or(PAGE).to_string(),
                file,
            }
        })
}

/// Asks GitHub. `asked`: by the user (Settings' button), who's told how it
/// went; as the app opens, only an update is said.
fn check(asked: bool) {
    let busy = with_state(|s| {
        let busy = matches!(s.step, Step::Checking | Step::Downloading(_));
        if !busy {
            s.step = Step::Checking;
        }
        busy
    });
    if busy {
        return;
    }
    redraw();
    let prereleases = pref_on(PRERELEASES_KEY, false);
    crate::downloader::runtime().spawn(async move {
        let found = async {
            let text = crate::extractors::http::client()
                .get(RELEASES)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| e.to_string())?
                .text()
                .await
                .map_err(|e| e.to_string())?;
            let releases: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            Ok::<_, String>(newest(&releases, crate::VERSION, prereleases, file_name()))
        }
        .await;
        match found {
            Ok(update) => {
                let _ = crate::platform::save_pref(CHECKED_AT_KEY, &now().to_string());
                if let (Some(update), false) = (&update, asked) {
                    crate::platform::show_toast(&format!("Eddy {} is out: see Settings → About", update.version));
                }
                with_state(|s| {
                    s.step = if update.is_some() { Step::Found } else { Step::UpToDate };
                    s.update = update;
                    s.downloaded = None;
                });
            }
            Err(e) => {
                log::warn!("couldn't look for an update: {e}");
                with_state(|s| s.step = if asked { Step::Failed } else { Step::Idle });
            }
        }
        redraw();
    });
}

/// Downloads the update found (the user asked).
fn download() {
    let file = with_state(|s| {
        let file = s.update.as_ref().and_then(|u| u.file.clone()).filter(|_| s.step == Step::Found)?;
        s.step = Step::Downloading(0.0);
        Some(file)
    });
    let Some(file) = file else {
        return;
    };
    redraw();
    crate::downloader::runtime().spawn(async move {
        let dir = updates_dir();
        let shown = std::sync::atomic::AtomicU64::new(0);
        let fetched = crate::downloader::fetch_file(&file.url, &dir, &file.name, |done, total| {
            let total = if total > 0 { total } else { file.size.max(1) };
            let percent = done * 100 / total;
            if shown.swap(percent, std::sync::atomic::Ordering::Relaxed) != percent {
                with_state(|s| s.step = Step::Downloading(done as f32 / total as f32));
                redraw();
            }
        })
        .await;
        let checked = match fetched {
            Ok(path) => {
                let sha256 = file.sha256.clone();
                tokio::task::spawn_blocking(move || verify(&path, sha256.as_deref()).map(|()| path))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            }
            Err(e) => Err(e),
        };
        match checked {
            Ok(path) => {
                with_state(|s| {
                    s.downloaded = Some(path);
                    s.step = Step::Ready;
                });
                // Android's installer straight away; desktop waits for
                // "Restart" (downloads would stop).
                if cfg!(target_os = "android") {
                    let _ = slint::invoke_from_event_loop(install_downloaded);
                }
            }
            Err(e) => {
                log::warn!("couldn't download the update: {e}");
                crate::platform::show_toast(&format!("Couldn't download the update: {e}"));
                with_state(|s| s.step = Step::Found);
            }
        }
        redraw();
    });
}

/// Whether `file` is what its release says (when it says).
fn verify(file: &std::path::Path, sha256: Option<&str>) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let Some(wanted) = sha256 else {
        return Ok(());
    };
    let mut hasher = Sha256::new();
    std::io::copy(&mut std::fs::File::open(file).map_err(|e| e.to_string())?, &mut hasher).map_err(|e| e.to_string())?;
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got == wanted {
        Ok(())
    } else {
        let _ = std::fs::remove_file(file);
        Err("it doesn't match its release's SHA-256".to_string())
    }
}

/// Puts the downloaded update in place (UI thread).
fn install_downloaded() {
    let Some(path) = with_state(|s| s.downloaded.clone().filter(|_| s.step == Step::Ready)) else {
        return;
    };
    #[cfg(target_os = "android")]
    crate::platform::install_apk(&path.to_string_lossy());
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    if let Err(e) = restart_into(&path) {
        log::warn!("couldn't put the update in place: {e}");
        crate::platform::show_toast(&format!("{e}: get it from its page"));
        // Its page, from here on.
        with_state(|s| {
            s.step = Step::Found;
            if let Some(update) = s.update.as_mut() {
                update.file = None;
            }
        });
        refresh();
    }
    #[cfg(not(any(target_os = "android", target_os = "windows", target_os = "linux")))]
    let _ = path;
}

/// Swaps the running program's file for `new` and starts it, this Eddy
/// quitting. The old file stays as ".old" (Windows won't delete a running
/// program's) until the next start.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn restart_into(new: &std::path::Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("Eddy can't replace its own file ({e})"))?;
    let placed = std::fs::copy(new, &exe).map(|_| ()).and_then(|()| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    });
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("Eddy can't replace its own file ({e})"));
    }
    // The new Eddy mustn't hand itself over to this one, on its way out.
    crate::handoff::forget_instance();
    if let Err(e) = std::process::Command::new(&exe).spawn() {
        return Err(format!("The new Eddy is in place but didn't start ({e}): open it again"));
    }
    let _ = slint::quit_event_loop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn releases() -> serde_json::Value {
        serde_json::json!([
            { "tag_name": "v0.3.0", "draft": true, "prerelease": false, "html_url": "https://x/0.3.0", "assets": [] },
            { "tag_name": "v0.2.2", "draft": false, "prerelease": true, "html_url": "https://x/0.2.2", "assets": [
                { "name": "eddy-linux-x86_64", "browser_download_url": "https://x/eddy-linux-x86_64", "size": 42, "digest": "sha256:ABCD" }
            ] },
            { "tag_name": "v0.2.1", "draft": false, "prerelease": true, "html_url": "https://x/0.2.1", "assets": [] },
            { "tag_name": "v0.1.10", "draft": false, "prerelease": false, "html_url": "https://x/0.1.10", "assets": [] },
            { "tag_name": "v0.1.9", "draft": false, "prerelease": false, "html_url": "https://x/0.1.9", "assets": [] }
        ])
    }

    #[test]
    fn versions() {
        assert_eq!(version_of("v0.2.1"), Some(vec![0, 2, 1]));
        assert_eq!(version_of("1.0.0-beta.2"), Some(vec![1, 0, 0]));
        assert_eq!(version_of("nightly"), None);
        assert!(version_of("0.1.10") > version_of("0.1.9"));
    }

    #[test]
    fn newest_release() {
        // Stable only: 0.1.10 is past 0.1.0, by numbers (not "0.1.9" > "0.1.10").
        let update = newest(&releases(), "0.1.0", false, Some("eddy-linux-x86_64")).unwrap();
        assert_eq!((update.version.as_str(), update.page.as_str()), ("0.1.10", "https://x/0.1.10"));
        assert_eq!(update.file, None);
        // With pre-releases: the newest, never the draft.
        let update = newest(&releases(), "0.2.1", true, Some("eddy-linux-x86_64")).unwrap();
        assert_eq!(update.version, "0.2.2");
        let file = update.file.unwrap();
        assert_eq!((file.url.as_str(), file.size, file.sha256.as_deref()), ("https://x/eddy-linux-x86_64", 42, Some("abcd")));
        // Nothing newer.
        assert_eq!(newest(&releases(), "0.2.1", false, None), None);
        assert_eq!(newest(&releases(), "0.2.2", true, None), None);
    }
}
