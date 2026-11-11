use super::PrefError;
use std::collections::HashMap;
use std::path::PathBuf;

fn prefs_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(crate::APP_ID)
        .join("prefs.json")
}

fn read_all_prefs() -> HashMap<String, String> {
    std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_pref(key: &str, value: &str) -> Result<(), PrefError> {
    let mut map = read_all_prefs();
    map.insert(key.to_string(), value.to_string());
    let path = prefs_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string(&map)?)?;
    Ok(())
}

pub fn get_pref(key: &str, default: &str) -> Result<String, PrefError> {
    Ok(read_all_prefs()
        .remove(key)
        .unwrap_or_else(|| default.to_string()))
}

pub fn system_accent() -> Option<slint::Color> {
    use system_theme::SystemTheme;
    let theme = SystemTheme::new().ok()?;
    let color = theme.get_accent().ok()?;
    Some(slint::Color::from_rgb_f32(
        color.red,
        color.green,
        color.blue,
    ))
}

/// Desktops give one accent colour.
pub fn system_second_accent() -> Option<slint::Color> {
    None
}

pub fn open_native_url(url: &str) {
    if let Err(e) = webbrowser::open(url) {
        eprintln!("Failed to open URL on desktop: {e}");
    }
}

/// Helper to get the standard OS download directory
pub fn get_default_download_location() -> String {
    dirs::download_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("~/Downloads"))
}

/// Opens a folder picker.
pub fn pick_download_folder<F>(on_picked: F)
where
    F: Fn(String, String) + Send + 'static,
{
    if let Some(path) = rfd::FileDialog::new()
        .set_title("Select Download Folder")
        .pick_folder()
    {
        let path_str = path.to_string_lossy().into_owned();
        // For desktop, the raw path and display path are the same
        on_picked(path_str.clone(), path_str);
    }
}

/// Lets the user pick a .txt of links or a .torrent. `on_picked` gets the
/// file's path and its name.
pub fn pick_link_file<F>(on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
    if let Some(path) = rfd::FileDialog::new()
        .set_title("Add from File")
        .add_filter("Links or torrents", &["txt", "torrent"])
        .add_filter("All files", &["*"])
        .pick_file()
    {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        on_picked(path.to_string_lossy().into_owned(), name);
    }
}

/// Every setting (a backup's).
pub fn all_prefs() -> HashMap<String, String> {
    read_all_prefs()
}

/// Lets the user pick a backup to restore (Settings → General), starting
/// where Eddy puts them.
pub fn pick_backup_file<F>(on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
    if let Some(path) = rfd::FileDialog::new()
        .set_title("Restore a backup")
        .set_directory(crate::backup::backups_dir())
        .add_filter("Eddy backups", &["zip"])
        .add_filter("All files", &["*"])
        .pick_file()
    {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        on_picked(path.to_string_lossy().into_owned(), name);
    }
}

/// Lets the user pick a cookies.txt (Settings → Downloads → Cookies).
pub fn pick_cookies_file<F>(on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
    if let Some(path) = rfd::FileDialog::new()
        .set_title("Add cookies.txt")
        .add_filter("cookies.txt", &["txt"])
        .add_filter("All files", &["*"])
        .pick_file()
    {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        on_picked(path.to_string_lossy().into_owned(), name);
    }
}

/// Desktop apps stay running while they download, so downloads start
/// straight away.
pub fn start_download(request: &crate::downloader::DownloadRequest) -> Result<(), String> {
    crate::downloader::start(request)
}

/// Desktop apps can write wherever the user can.
pub fn has_all_files_access() -> bool {
    true
}

pub fn request_all_files_access() {}

/// Opens a downloaded file with the system's default app.
/// Desktop downloads are plain files (no documents).
pub fn delete_document(_uri: &str) {}

/// A site's bot check, passed by the user in the webview helper's window;
/// its user agent and cookies are written to `result` (see novels::fetch).
pub fn open_site_check(url: &str, result: &str) -> bool {
    send_to_webview("check", url, result)
}

/// Opens `page` in the webview helper's window, where the user clicks its
/// download: the file's link and what came with it are written to
/// `result`, as Android's BrowserActivity does (see crate::browser).
pub fn open_browser(page: &str, result: &str) -> bool {
    send_to_webview("browse", page, result)
}

/// A page read in the webview helper, unseen, for a site that turns Eddy's
/// own requests away; written to `result` as Android's PageFetcher does.
pub fn fetch_page(url: &str, result: &str) -> bool {
    send_to_webview("fetch", url, result)
}

/// The webview helper (eddy-webview, next to Eddy): a browser for sites
/// behind bot checks, as Android's WebView is. A program of its own, so
/// Eddy itself never needs WebKitGTK (Linux) or WebView2 (Windows): it's
/// started the first time a site needs it, takes one page a line on its
/// stdin, and quits once it's been idle a while or Eddy goes.
struct Webview {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
}

static WEBVIEW: std::sync::Mutex<Option<Webview>> = std::sync::Mutex::new(None);
/// Whether the user's been told the helper couldn't start (once a session).
static WEBVIEW_MISSING_SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The helper: EDDY_WEBVIEW, else next to Eddy (as built, or as released:
/// "eddy-webview-linux-x86_64"…), else Eddy's own copy (Settings →
/// Downloads → External packages), else on the PATH.
fn webview_path() -> PathBuf {
    webview_file().unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "eddy-webview.exe" } else { "eddy-webview" }))
}

/// The helper's file, if it's anywhere but (maybe) on the PATH.
pub fn webview_file() -> Option<PathBuf> {
    let name = if cfg!(windows) { "eddy-webview.exe" } else { "eddy-webview" };
    if let Some(path) = std::env::var_os("EDDY_WEBVIEW") {
        return Some(path.into());
    }
    let dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(PathBuf::from));
    let beside = dir.as_ref().map(|dir| dir.join(name)).filter(|path| path.exists()).or_else(|| {
        let mut released: Vec<PathBuf> = std::fs::read_dir(dir.as_ref()?)
            .ok()?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("eddy-webview-")))
            .collect();
        released.sort();
        released.pop()
    });
    beside.or_else(|| find_tool("eddy-webview"))
}

/// Starts the helper and waits for its "ready" (its browser is up); a
/// missing WebKitGTK or WebView2 ends it before that, saying why.
fn start_webview() -> Result<Webview, String> {
    use std::io::{BufRead, Read};
    use std::process::Stdio;
    let data = PathBuf::from(get_app_data_dir()).join("webview");
    let mut child = std::process::Command::new(webview_path())
        .arg(&data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "eddy-webview isn't installed next to Eddy".to_string(),
            _ => format!("eddy-webview didn't start: {e}"),
        })?;
    let (Some(stdin), Some(stdout), Some(mut stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        return Err("eddy-webview didn't start".to_string());
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // Graphics drivers may say things on the way (Mesa's EGL warnings).
    std::thread::spawn(move || {
        let ready = std::io::BufReader::new(stdout).lines().map_while(Result::ok).any(|line| line.trim() == "ready");
        let _ = tx.send(ready);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(true) => {}
        _ => {
            let _ = child.kill();
            let mut why = String::new();
            let _ = stderr.read_to_string(&mut why);
            let _ = child.wait();
            let why = why.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("it quit straight away").to_string();
            return Err(format!("eddy-webview didn't start: {why}"));
        }
    }
    // What it says on the way goes to the logs.
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            log::warn!(target: "browser", "{line}");
        }
    });
    Ok(Webview { child, stdin })
}

fn send_to_webview(kind: &str, url: &str, result: &str) -> bool {
    use std::io::Write;
    let mut webview = WEBVIEW.lock().unwrap_or_else(|e| e.into_inner());
    // Quit since (idle): started again.
    if webview.as_mut().is_some_and(|w| !matches!(w.child.try_wait(), Ok(None))) {
        *webview = None;
    }
    if webview.is_none() {
        match start_webview() {
            Ok(started) => *webview = Some(started),
            Err(e) => {
                log::warn!("{e}");
                if !WEBVIEW_MISSING_SAID.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    // macOS has its webview built in: only the helper can be missing.
                    show_toast(if webview_file().is_none() {
                        "Eddy's browser needs its helper: get it in Settings → Downloads → External packages"
                    } else if cfg!(windows) {
                        "Eddy's browser needs Microsoft Edge WebView2"
                    } else if cfg!(target_os = "macos") {
                        "Eddy's browser didn't start"
                    } else {
                        "Eddy's browser needs WebKitGTK (libwebkit2gtk-4.1)"
                    });
                }
                return false;
            }
        }
    }
    let Some(running) = webview.as_mut() else {
        return false;
    };
    let sent = writeln!(running.stdin, "{kind}\t{url}\t{result}").and_then(|_| running.stdin.flush());
    if let Err(e) = sent {
        log::warn!("eddy-webview went away: {e}");
        *webview = None;
        return false;
    }
    true
}

/// Opens a web page in the browser.
pub fn open_url(url: &str) {
    if let Err(e) = webbrowser::open(url) {
        log::error!("Couldn't open {url}: {e}");
    }
}

pub fn open_file(path: &str) {
    let program = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program).arg(path).spawn() {
        eprintln!("Couldn't open {path}: {e}");
    }
}

pub fn get_app_data_dir() -> String {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(crate::APP_ID)
        .to_string_lossy()
        .into_owned()
}

pub fn trigger_haptic(_: super::Haptic) {}
/// The app's snackbar, set up by main (desktop has no toasts of its own).
static TOAST: std::sync::OnceLock<Box<dyn Fn(String) + Send + Sync>> = std::sync::OnceLock::new();

pub fn set_toast_handler(show: impl Fn(String) + Send + Sync + 'static) {
    let _ = TOAST.set(Box::new(show));
}

/// A short message, in the app's snackbar. Any thread.
pub fn show_toast(message: &str) {
    if let Some(show) = TOAST.get() {
        show(message.to_string());
    }
}
pub fn set_bar_icons_dark(_: bool) {}

/// Picked-folder documents are Android's; desktop downloads are plain files.
pub fn open_in_folder(_doc: &str, _rel: &str) {}

/// Desktops have no share sheet to hand things to.
pub fn share(_files: &[String], _text: &str) {
    show_toast("Sharing isn't available here");
}

/// ffmpeg from PATH (merging, HLS, converting to audio).
/// The yt-dlp helper apps are Android's; elsewhere yt-dlp is a normal program.
pub fn helper_apps() -> Option<(std::collections::HashMap<String, String>, String)> {
    None
}

pub fn install_apk(_path: &str) {}

/// Where Eddy installs yt-dlp, ffmpeg and Deno itself (Settings →
/// Downloads), in its data folder.
pub fn tools_dir() -> PathBuf {
    PathBuf::from(get_app_data_dir()).join("bin")
}

/// A program: Eddy's own copy first, then PATH.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let own = tools_dir().join(&file);
    if own.is_file() {
        return Some(own);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&file))
        .find(|path| path.is_file())
}

/// ffmpeg (merging, HLS, converting to audio).
pub fn ffmpeg_command() -> Option<std::process::Command> {
    find_tool("ffmpeg").map(std::process::Command::new)
}

/// Deno (the Instagram extractor's SnapSave fallback; yt-dlp's JS runtime).
pub fn deno_command() -> Option<std::process::Command> {
    find_tool("deno").map(std::process::Command::new)
}

/// yt-dlp, ready for its arguments, told where Eddy's own ffmpeg and Deno
/// are (its folder isn't on PATH).
pub fn ytdlp_command() -> Option<std::process::Command> {
    let mut command = std::process::Command::new(find_tool("yt-dlp")?);
    if let Some(ffmpeg) = find_tool("ffmpeg") {
        command.arg("--ffmpeg-location").arg(ffmpeg);
    }
    if let Some(deno) = find_tool("deno") {
        command.arg("--js-runtimes").arg(format!("deno:{}", deno.display()));
    }
    Some(command)
}

pub fn ytdlp_available() -> bool {
    find_tool("yt-dlp").is_some()
}

pub fn ytdlp_ready() -> bool {
    ytdlp_available()
}

/// Where Eddy would keep its own yt-dlp (Android's; desktop installs its
/// tools through components_desktop instead).
pub fn own_ytdlp() -> std::path::PathBuf {
    tools_dir().join("yt-dlp-own")
}
