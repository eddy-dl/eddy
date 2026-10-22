use super::PrefError;

pub fn save_pref(_key: &str, _value: &str) -> Result<(), PrefError> {
    Ok(())
}

pub fn get_pref(_key: &str, default: &str) -> Result<String, PrefError> {
    Ok(default.to_string())
}

pub fn system_accent() -> Option<slint::Color> {
    None
}

pub fn get_default_download_location() -> String {
    String::from("Documents")
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

pub fn start_download(_request: &crate::downloader::DownloadRequest) -> Result<(), String> {
    Err("Downloads aren't supported on iOS yet".to_string())
}

pub fn open_file(_path: &str) {}

pub fn has_all_files_access() -> bool {
    false
}

pub fn request_all_files_access() {}

pub fn get_app_data_dir() -> String {
    std::env::var("HOME").into_owned()
}

pub fn open_native_url(_url: &str) {}
pub fn trigger_haptic(_: super::Haptic) {}
pub fn show_toast(_: &str) {}
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
