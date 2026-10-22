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
pub fn show_toast(_: &str) {}
pub fn set_bar_icons_dark(_: bool) {}

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
