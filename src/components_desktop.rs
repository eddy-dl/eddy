//! yt-dlp, FFmpeg and Deno on desktop (Settings → Downloads), adapted from
//! urlsula's installdeps: when the user asks (and confirms), Eddy downloads
//! them from GitHub into its own bin folder (yt-dlp with the community
//! plugin, ffmpeg and ffprobe from BtbN's builds, Deno). Copies already on
//! PATH count as installed; Eddy only updates its own.

use crate::{AppSettings, DownloaderApp, HelperPackage};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const COMMUNITY_PLUGIN_URL: &str =
    "https://github.com/biggestsonicfan/yt-dlp-community-plugin/archive/refs/heads/master.zip";
const COMMUNITY_PLUGIN_NAME: &str = "biggestsonicfan-yt-dlp-community-plugin";

/// The tools, in the order Settings lists them.
const TOOLS: [&str; 3] = ["ytdlp", "ffmpeg", "deno"];

/// A tool's newest build for this computer.
#[derive(Clone, Debug)]
struct Release {
    version: String,
    url: String,
    size: u64,
}

/// A tool as it is now: its version, and whether it's Eddy's own copy.
#[derive(Clone, Debug)]
struct Installed {
    version: String,
    own: bool,
}

#[derive(Default)]
struct State {
    /// None until checked; Err when GitHub couldn't be reached.
    releases: Option<Result<HashMap<String, Release>, String>>,
    installed: HashMap<String, Installed>,
    /// Downloads in progress: key → fraction done (None while unpacking).
    working: HashMap<String, Option<f32>>,
    checked_at: Option<std::time::Instant>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(State::default))
}

thread_local! {
    static UI: RefCell<Option<slint::Weak<DownloaderApp>>> = const { RefCell::new(None) };
}

/// Hooks the Settings rows up. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    let settings = ui.global::<AppSettings>();
    settings.on_helpers_check(check);
    settings.on_helper_download(|key| start_install(key.to_string()));
    // Nothing waits to be installed on desktop.
    settings.on_helper_install(|_| {});
    refresh();
}

fn redraw() {
    let _ = slint::invoke_from_event_loop(refresh);
}

/// The Settings rows from what's known (UI thread).
fn refresh() {
    let rows: Vec<HelperPackage> = with_state(|state| {
        TOOLS
            .iter()
            .map(|key| {
                let installed = state.installed.get(*key);
                let release = match &state.releases {
                    Some(Ok(map)) => map.get(*key),
                    _ => None,
                };
                let (code, progress) = match state.working.get(*key) {
                    Some(Some(p)) => (4, *p),
                    Some(None) => (8, 0.0),
                    None => {
                        let code = match (installed, release, &state.releases) {
                            // Only Eddy's own copy gets updates.
                            (Some(i), Some(r), _) if i.own && r.version != i.version => 3,
                            (Some(_), _, _) => 2,
                            (None, Some(_), _) => 1,
                            (None, None, None) => 0,
                            (None, None, Some(Err(_))) => 7,
                            (None, None, Some(Ok(_))) => 6,
                        };
                        (code, 0.0)
                    }
                };
                HelperPackage {
                    key: (*key).into(),
                    installed: installed.map(|i| i.version.clone()).unwrap_or_default().into(),
                    latest: release.map(|r| r.version.clone()).unwrap_or_default().into(),
                    size: release.map(|r| crate::links::format_size(r.size)).unwrap_or_default().into(),
                    state: code,
                    progress,
                }
            })
            .collect()
    });
    UI.with(|u| {
        if let Some(ui) = u.borrow().as_ref().and_then(|w| w.upgrade()) {
            ui.global::<AppSettings>().set_helper_packages(ModelRc::new(VecModel::from(rows)));
        }
    });
}

/// Finds what's installed and asks GitHub what's newest (Settings opened);
/// GitHub at most every 10 minutes.
fn check() {
    let ask_github = with_state(|state| {
        let recent = state.checked_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(600));
        let ok = matches!(state.releases, Some(Ok(_)));
        if !(recent && ok) {
            state.checked_at = Some(std::time::Instant::now());
        }
        !(recent && ok)
    });
    crate::downloader::runtime().spawn(async move {
        let installed = tokio::task::spawn_blocking(installed_versions).await.unwrap_or_default();
        with_state(|s| s.installed = installed);
        redraw();
        if ask_github {
            let releases = fetch_releases().await;
            if let Err(e) = &releases {
                log::warn!("couldn't check yt-dlp, ffmpeg and Deno releases: {e}");
            }
            with_state(|s| s.releases = Some(releases));
            redraw();
        }
    });
}

/// Runs a program and returns the first line it prints.
fn first_line(program: &Path, arg: &str) -> Option<String> {
    let output = std::process::Command::new(program).arg(arg).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

/// Each tool's version, from Eddy's own copy or one on PATH.
fn installed_versions() -> HashMap<String, Installed> {
    let own_dir = crate::platform::tools_dir();
    let mut found = HashMap::new();
    for key in TOOLS {
        let name = if key == "ytdlp" { "yt-dlp" } else { key };
        let Some(path) = crate::platform::find_tool(name) else {
            continue;
        };
        let own = path.starts_with(&own_dir);
        let version = match key {
            // "2026.08.19"
            "ytdlp" => first_line(&path, "--version"),
            // "deno 2.9.1 (stable, release, …)"
            "deno" => first_line(&path, "--version").and_then(|l| l.split_whitespace().nth(1).map(String::from)),
            // Eddy's copy: the build date it was installed from. A system
            // copy: "ffmpeg version 6.1.1 …".
            _ if own => std::fs::read_to_string(own_dir.join(".ffmpeg-version")).ok().map(|v| v.trim().to_string()),
            _ => first_line(&path, "-version").and_then(|l| l.split_whitespace().nth(2).map(String::from)),
        };
        found.insert(key.to_string(), Installed { version: version.unwrap_or_default(), own });
    }
    found
}

/// The release file names for this computer: (yt-dlp, ffmpeg, Deno).
fn assets() -> (Option<&'static str>, Option<&'static str>, Option<String>) {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let ytdlp = match (os, arch) {
        ("linux", "x86_64") => Some("yt-dlp_linux"),
        ("linux", "aarch64") => Some("yt-dlp_linux_aarch64"),
        ("windows", "x86_64") => Some("yt-dlp.exe"),
        ("windows", "aarch64") => Some("yt-dlp_arm64.exe"),
        ("macos", _) => Some("yt-dlp_macos"),
        _ => None,
    };
    // BtbN builds Linux and Windows only.
    let ffmpeg = match (os, arch) {
        ("linux", "x86_64") => Some("ffmpeg-master-latest-linux64-gpl.tar.xz"),
        ("linux", "aarch64") => Some("ffmpeg-master-latest-linuxarm64-gpl.tar.xz"),
        ("windows", "x86_64") => Some("ffmpeg-master-latest-win64-gpl.zip"),
        ("windows", "aarch64") => Some("ffmpeg-master-latest-winarm64-gpl.zip"),
        _ => None,
    };
    let deno_target = match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    };
    (ytdlp, ffmpeg, deno_target.map(|t| format!("deno-{t}.zip")))
}

async fn github_release(api: &str) -> Result<serde_json::Value, String> {
    let text = crate::extractors::http::client()
        .get(api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// A release's asset by name: its download link and size.
fn asset(release: &serde_json::Value, name: &str) -> Option<(String, u64, String)> {
    let asset = release["assets"].as_array()?.iter().find(|a| a["name"] == name)?;
    Some((
        asset["browser_download_url"].as_str()?.to_string(),
        asset["size"].as_u64().unwrap_or(0),
        asset["updated_at"].as_str().unwrap_or_default().chars().take(10).collect(),
    ))
}

async fn fetch_releases() -> Result<HashMap<String, Release>, String> {
    let (ytdlp, ffmpeg, deno) = assets();
    let mut found = HashMap::new();
    if let Some(name) = ytdlp {
        let release = github_release("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest").await?;
        if let Some((url, size, _)) = asset(&release, name) {
            let version = release["tag_name"].as_str().unwrap_or_default().to_string();
            found.insert("ytdlp".to_string(), Release { version, url, size });
        }
    }
    if let Some(name) = ffmpeg {
        // "latest" is rebuilt daily; its build date stands in for a version.
        let release = github_release("https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/tags/latest").await?;
        if let Some((url, size, date)) = asset(&release, name) {
            found.insert("ffmpeg".to_string(), Release { version: date, url, size });
        }
    }
    if let Some(name) = deno {
        let release = github_release("https://api.github.com/repos/denoland/deno/releases/latest").await?;
        if let Some((url, size, _)) = asset(&release, &name) {
            let version = release["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string();
            found.insert("deno".to_string(), Release { version, url, size });
        }
    }
    Ok(found)
}

/// Downloads and installs a tool (the user confirmed).
fn start_install(key: String) {
    let release = with_state(|s| match &s.releases {
        Some(Ok(map)) => map.get(&key).cloned(),
        _ => None,
    });
    let Some(release) = release else {
        return;
    };
    with_state(|s| s.working.insert(key.clone(), Some(0.0)));
    refresh();

    crate::downloader::runtime().spawn(async move {
        let result = install_tool(&key, &release).await;
        if let Err(e) = &result {
            log::warn!("couldn't install {key}: {e}");
        }
        let installed = tokio::task::spawn_blocking(installed_versions).await.unwrap_or_default();
        with_state(|s| {
            s.working.remove(&key);
            s.installed = installed;
        });
        redraw();
    });
}

/// Streams `url` into a temporary file next to `dir`, reporting progress.
async fn download(key: &str, url: &str, size: u64, dir: &Path) -> Result<PathBuf, String> {
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(dir).await.map_err(|e| e.to_string())?;
    let path = dir.join(format!(".download-{key}"));
    let mut response = crate::extractors::http::client()
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let total = response.content_length().unwrap_or(size).max(1);
    let mut file = tokio::fs::File::create(&path).await.map_err(|e| e.to_string())?;
    let (mut done, mut shown) = (0u64, 0u64);
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        if done * 100 / total != shown {
            shown = done * 100 / total;
            with_state(|s| s.working.insert(key.to_string(), Some(done as f32 / total as f32)));
            redraw();
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(path)
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Puts `src` at `dest` (replacing it), executable.
fn place(src: &Path, dest: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(dest);
    std::fs::rename(src, dest)
        .or_else(|_| std::fs::copy(src, dest).map(|_| ()).and_then(|_| std::fs::remove_file(src)))
        .map_err(|e| e.to_string())?;
    make_executable(dest);
    Ok(())
}

async fn install_tool(key: &str, release: &Release) -> Result<(), String> {
    let dir = crate::platform::tools_dir();
    let exe = if cfg!(windows) { ".exe" } else { "" };
    let archive = download(key, &release.url, release.size, &dir).await?;
    // Unpacking.
    with_state(|s| s.working.insert(key.to_string(), None));
    redraw();

    let (key_owned, release_owned, dir_owned) = (key.to_string(), release.clone(), dir.clone());
    let unpacked = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let (key, release, dir) = (key_owned, release_owned, dir_owned);
        match key.as_str() {
            "ytdlp" => place(&archive, &dir.join(format!("yt-dlp{exe}"))),
            "ffmpeg" => {
                let wanted = [format!("ffmpeg{exe}"), format!("ffprobe{exe}")];
                if release.url.ends_with(".zip") {
                    extract_zip(&archive, &dir, &wanted, true)?;
                } else {
                    extract_tar_xz(&archive, &dir, &wanted)?;
                }
                let _ = std::fs::remove_file(&archive);
                std::fs::write(dir.join(".ffmpeg-version"), &release.version).map_err(|e| e.to_string())
            }
            _ => {
                extract_zip(&archive, &dir, &[format!("deno{exe}")], false)?;
                let _ = std::fs::remove_file(&archive);
                Ok(())
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    unpacked?;

    if key == "ytdlp" {
        // Nice to have; yt-dlp works without it.
        if let Err(e) = install_community_plugin(&dir).await {
            log::warn!("couldn't install the yt-dlp community plugin: {e}");
        }
    }
    Ok(())
}

/// Takes the `wanted` files (by name) out of a zip into `dir`; with
/// `in_bin`, only from the archive's bin/ folder.
fn extract_zip(archive: &Path, dir: &Path, wanted: &[String], in_bin: bool) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().replace('\\', "/");
        let base = name.rsplit('/').next().unwrap_or_default().to_string();
        if entry.is_dir() || !wanted.contains(&base) || (in_bin && !name.contains("/bin/")) {
            continue;
        }
        let temp = dir.join(format!(".unpack-{base}"));
        let mut out = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        drop(out);
        place(&temp, &dir.join(&base))?;
    }
    Ok(())
}

/// The same for BtbN's .tar.xz builds (xz unpacked to a temporary file
/// first: it's large).
fn extract_tar_xz(archive: &Path, dir: &Path, wanted: &[String]) -> Result<(), String> {
    let tar_path = archive.with_extension("tar");
    {
        let mut input = std::io::BufReader::new(std::fs::File::open(archive).map_err(|e| e.to_string())?);
        let mut output = std::io::BufWriter::new(std::fs::File::create(&tar_path).map_err(|e| e.to_string())?);
        lzma_rs::xz_decompress(&mut input, &mut output).map_err(|e| format!("{e:?}"))?;
        output.flush().map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::open(&tar_path).map_err(|e| e.to_string())?;
    let mut tar = tar::Archive::new(file);
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
        let base = path.rsplit('/').next().unwrap_or_default().to_string();
        if !path.contains("/bin/") || !wanted.contains(&base) {
            continue;
        }
        let temp = dir.join(format!(".unpack-{base}"));
        let mut out = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
        let mut buffer = Vec::new();
        entry.read_to_end(&mut buffer).map_err(|e| e.to_string())?;
        out.write_all(&buffer).map_err(|e| e.to_string())?;
        drop(out);
        place(&temp, &dir.join(&base))?;
    }
    let _ = std::fs::remove_file(&tar_path);
    Ok(())
}

/// yt-dlp looks for plugins in yt-dlp-plugins/<name>/yt_dlp_plugins next to
/// itself.
async fn install_community_plugin(dir: &Path) -> Result<(), String> {
    let plugin_dir = dir.join("yt-dlp-plugins").join(COMMUNITY_PLUGIN_NAME);
    if plugin_dir.join("yt_dlp_plugins").is_dir() {
        return Ok(());
    }
    let archive = download("plugin", COMMUNITY_PLUGIN_URL, 0, dir).await?;
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let file = std::fs::File::open(&archive).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            if entry.is_dir() {
                continue;
            }
            // Drop the archive's top folder.
            let Some(path) = entry.enclosed_name() else {
                continue;
            };
            let relative: PathBuf = path.components().skip(1).collect();
            if relative.as_os_str().is_empty() {
                continue;
            }
            let destination = plugin_dir.join(relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out = std::fs::File::create(&destination).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_file(&archive);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}
