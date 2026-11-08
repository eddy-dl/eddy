use super::PrefError;
use std::collections::HashMap;
use std::path::PathBuf;

/// The app's own container (iOS sets HOME to it).
fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

// Preferences: a JSON file, as on desktop.
fn prefs_path() -> PathBuf {
    PathBuf::from(get_app_data_dir()).join("prefs.json")
}

fn read_all_prefs() -> HashMap<String, String> {
    std::fs::read_to_string(prefs_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_pref(key: &str, value: &str) -> Result<(), PrefError> {
    let mut map = read_all_prefs();
    map.insert(key.to_string(), value.to_string());
    std::fs::write(prefs_path(), serde_json::to_string(&map)?)?;
    Ok(())
}

pub fn get_pref(key: &str, default: &str) -> Result<String, PrefError> {
    Ok(read_all_prefs().remove(key).unwrap_or_else(|| default.to_string()))
}

pub fn system_accent() -> Option<slint::Color> {
    None
}

pub fn system_second_accent() -> Option<slint::Color> {
    None
}

/// Documents, which the Files app shows under "On My iPhone › Eddy"
/// (UIFileSharingEnabled, LSSupportsOpeningDocumentsInPlace).
pub fn get_default_download_location() -> String {
    home().join("Documents").to_string_lossy().into_owned()
}

pub fn pick_download_folder<F>(on_picked: F)
where
    F: Fn(String, String) + Send + 'static,
{
    on_picked(String::new(), String::new());
}

pub fn pick_link_file<F>(_on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
}

pub fn pick_cookies_file<F>(_on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
}

/// In-process, as on desktop (iOS has no download service to hand it to).
pub fn start_download(request: &crate::downloader::DownloadRequest) -> Result<(), String> {
    crate::downloader::start(request)
}

pub fn open_file(_path: &str) {}

pub fn open_site_check(_url: &str, _result: &str) -> bool {
    false
}

pub fn open_browser(_page: &str, _result: &str) -> bool {
    false
}

pub fn fetch_page(_url: &str, _result: &str) -> bool {
    false
}

pub fn open_url(_url: &str) {}

pub fn delete_document(_uri: &str) {}

pub fn share(_files: &[String], _text: &str) {}

pub fn open_in_folder(_doc: &str, _rel: &str) {}

pub fn has_all_files_access() -> bool {
    false
}

pub fn request_all_files_access() {}

/// Library/Application Support/Eddy: kept and backed up, not shown in Files.
pub fn get_app_data_dir() -> String {
    let dir = home().join("Library").join("Application Support").join("Eddy");
    let _ = std::fs::create_dir_all(&dir);
    dir.to_string_lossy().into_owned()
}

pub fn open_native_url(_url: &str) {}
pub fn trigger_haptic(_: super::Haptic) {}
/// The app's snackbar, set up by ios_main (iOS has no toasts).
static TOAST: std::sync::OnceLock<Box<dyn Fn(String) + Send + Sync>> = std::sync::OnceLock::new();

pub fn set_toast_handler(show: impl Fn(String) + Send + Sync + 'static) {
    let _ = TOAST.set(Box::new(show));
}

pub fn show_toast(message: &str) {
    if let Some(show) = TOAST.get() {
        show(message.to_string());
    }
}
pub fn set_bar_icons_dark(_: bool) {}

/// The yt-dlp helper apps are Android's; elsewhere yt-dlp is a normal program.
pub fn helper_apps() -> Option<(std::collections::HashMap<String, String>, String)> {
    None
}

pub fn install_apk(_path: &str) {}

pub fn ffmpeg_command() -> Option<std::process::Command> {
    None
}

pub fn deno_command() -> Option<std::process::Command> {
    None
}

/// iOS can't run yt-dlp: TikTok, Instagram and Pinterest still work through
/// the native extractors, other pages download directly.
pub fn ytdlp_command() -> Option<std::process::Command> {
    None
}

pub fn ytdlp_available() -> bool {
    false
}

pub fn ytdlp_ready() -> bool {
    false
}

/// Where Eddy would keep its own yt-dlp (Android's; desktop installs its
/// tools through components_desktop instead).
pub fn own_ytdlp() -> std::path::PathBuf {
    std::path::PathBuf::from(get_app_data_dir()).join("yt-dlp")
}
