//! yt-dlp, FFmpeg, Deno, gallery-dl and Eddy's browser helper on desktop
//! (Settings → Downloads), adapted from urlsula's installdeps: when the user
//! asks (and confirms), Eddy downloads them from GitHub into its own bin
//! folder (yt-dlp with the community plugin, ffmpeg and ffprobe from BtbN's
//! builds, Deno, gallery-dl's own builds, eddy-webview from Eddy's own
//! release for its version). Copies already on PATH (or, for the helper,
//! next to Eddy) count as installed; Eddy only updates its own. A file
//! whose release gives its SHA-256 is checked against it.

use crate::{AppSettings, DownloaderApp, HelperPackage};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;


/// The tools, in the order Settings lists them.
const TOOLS: [&str; 5] = ["ytdlp", "ffmpeg", "deno", "gallerydl", "webview"];

/// Where Eddy's own releases are.
const EDDY_RELEASES: &str = "https://api.github.com/repos/eddy-dl/eddy/releases";

/// A tool's newest build for this computer.
#[derive(Clone, Debug)]
struct Release {
    version: String,
    url: String,
    size: u64,
    /// The file's SHA-256 (hex), when GitHub gives it.
    sha256: Option<String>,
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
                log::warn!("couldn't check the tools' releases: {e}");
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
        let name = match key {
            "ytdlp" => "yt-dlp",
            "gallerydl" => "gallery-dl",
            "webview" => "eddy-webview",
            _ => key,
        };
        // The helper may be next to Eddy too (as released, or in Eddy.app).
        let path = if key == "webview" { crate::platform::webview_file() } else { crate::platform::find_tool(name) };
        let Some(path) = path else {
            continue;
        };
        let own = path.starts_with(&own_dir);
        let version = match key {
            // "2026.08.19"
            "ytdlp" => first_line(&path, "--version"),
            // "deno 2.9.1 (stable, release, …)"
            "deno" => first_line(&path, "--version").and_then(|l| l.split_whitespace().nth(1).map(String::from)),
            // Eddy's copy: the build date it was installed from (gallery-dl's
            // builds too). A system copy: "ffmpeg version 6.1.1 …",
            // gallery-dl's "1.32.19".
            _ if own => std::fs::read_to_string(own_dir.join(format!(".{key}-version"))).ok().map(|v| v.trim().to_string()),
            "gallerydl" => first_line(&path, "--version"),
            // Next to Eddy: it came with this Eddy.
            "webview" => Some(env!("CARGO_PKG_VERSION").to_string()),
            _ => first_line(&path, "-version").and_then(|l| l.split_whitespace().nth(2).map(String::from)),
        };
        found.insert(key.to_string(), Installed { version: version.unwrap_or_default(), own });
    }
    found
}

/// gallery-dl's build for this computer (gdl-org/builds: Linux x86_64,
/// macOS, Windows; Windows on ARM runs the x86_64 one).
fn gallerydl_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("gallery-dl_linux"),
        ("macos", _) => Some("gallery-dl_macos"),
        ("windows", "x86_64" | "aarch64") => Some("gallery-dl_windows.exe"),
        ("windows", "x86") => Some("gallery-dl_windows_x86.exe"),
        _ => None,
    }
}

/// Eddy's browser helper for this computer, as the release names it
/// (macOS has it inside Eddy.app).
fn webview_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("eddy-webview-linux-x86_64"),
        ("linux", "aarch64") => Some("eddy-webview-linux-arm64"),
        ("windows", "x86_64") => Some("eddy-webview-windows-x86_64.exe"),
        ("windows", "aarch64") => Some("eddy-webview-windows-arm64.exe"),
        _ => None,
    }
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

/// A release's asset's SHA-256, from GitHub's "digest" ("sha256:…").
fn digest(release: &serde_json::Value, name: &str) -> Option<String> {
    let asset = release["assets"].as_array()?.iter().find(|a| a["name"] == name)?;
    asset["digest"].as_str()?.strip_prefix("sha256:").map(str::to_ascii_lowercase)
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
            found.insert("ytdlp".to_string(), Release { version, url, size, sha256: digest(&release, name) });
        }
    }
    if let Some(name) = ffmpeg {
        // "latest" is rebuilt daily; its build date stands in for a version.
        let release = github_release("https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/tags/latest").await?;
        if let Some((url, size, date)) = asset(&release, name) {
            found.insert("ffmpeg".to_string(), Release { version: date, url, size, sha256: digest(&release, name) });
        }
    }
    if let Some(name) = deno {
        let release = github_release("https://api.github.com/repos/denoland/deno/releases/latest").await?;
        if let Some((url, size, _)) = asset(&release, &name) {
            let version = release["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string();
            found.insert("deno".to_string(), Release { version, url, size, sha256: digest(&release, &name) });
        }
    }
    if let Some(name) = gallerydl_asset() {
        // Built from gallery-dl daily, tagged with the date.
        let release = github_release("https://api.github.com/repos/gdl-org/builds/releases/latest").await?;
        if let Some((url, size, _)) = asset(&release, name) {
            let version = release["tag_name"].as_str().unwrap_or_default().to_string();
            found.insert("gallerydl".to_string(), Release { version, url, size, sha256: digest(&release, name) });
        }
    }
    if let Some(name) = webview_asset() {
        // This Eddy's own release, else the newest that has it (a build
        // between releases).
        let release = match github_release(&format!("{EDDY_RELEASES}/tags/v{}", env!("CARGO_PKG_VERSION"))).await {
            Ok(release) if asset(&release, name).is_some() => release,
            // Not found isn't the other tools' failure: the row says so.
            _ => match github_release(&format!("{EDDY_RELEASES}?per_page=10")).await {
                Ok(all) => all.as_array().into_iter().flatten().find(|r| asset(r, name).is_some()).cloned().unwrap_or_default(),
                Err(e) => {
                    log::warn!("couldn't look for eddy-webview's release: {e}");
                    serde_json::Value::Null
                }
            },
        };
        if let Some((url, size, _)) = asset(&release, name) {
            let version = release["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string();
            found.insert("webview".to_string(), Release { version, url, size, sha256: digest(&release, name) });
        }
    }
    Ok(found)
}

/// Whether `file` is what its release says (when it says).
fn verify(file: &Path, sha256: Option<&str>) -> Result<(), String> {
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
        Err(format!("the download doesn't match its release's SHA-256 ({got}, not {wanted})"))
    }
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
    // With aria2: several connections, and it retries.
    let shown = std::sync::atomic::AtomicU64::new(0);
    crate::downloader::fetch_file(url, dir, &format!(".download-{key}"), |done, total| {
        let total = if total > 0 { total } else { size.max(1) };
        let percent = done * 100 / total;
        if shown.swap(percent, std::sync::atomic::Ordering::Relaxed) != percent {
            with_state(|s| s.working.insert(key.to_string(), Some(done as f32 / total as f32)));
            redraw();
        }
    })
    .await
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
        verify(&archive, release.sha256.as_deref())?;
        match key.as_str() {
            "ytdlp" => place(&archive, &dir.join(format!("yt-dlp{exe}"))),
            "webview" => {
                place(&archive, &dir.join(format!("eddy-webview{exe}")))?;
                std::fs::write(dir.join(".webview-version"), &release.version).map_err(|e| e.to_string())
            }
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
            "gallerydl" => {
                place(&archive, &dir.join(format!("gallery-dl{exe}")))?;
                std::fs::write(dir.join(".gallerydl-version"), &release.version).map_err(|e| e.to_string())
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
        if let Err(e) = crate::ytdlp_plugin::install(&dir).await {
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

