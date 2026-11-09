//! gallery-dl (github.com/mikf/gallery-dl): a gallery's files from the many
//! sites it knows (image boards, art and photo sites…), offered in the
//! preview next to yt-dlp. On desktop it's gallery-dl's own build
//! (gdl-org/builds, installed by components_desktop) or one on PATH. On
//! Android it runs on the yt-dlp Python package: gallery-dl and requests
//! are pure Python, so their wheels from PyPI are unpacked into Eddy's
//! storage (components installs them).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

/// What the preview picked: files by their place (from 1, as gallery-dl
/// counts them; empty for all), and how many the gallery had then.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GalleryRequest {
    pub items: Vec<usize>,
    pub count: usize,
}

/// What a check found.
#[derive(Clone, Debug, Default)]
pub struct Gallery {
    pub title: String,
    /// Each file's name and address.
    pub files: Vec<(String, String)>,
    /// Galleries it leads to rather than files (a user's page): gallery-dl
    /// goes through them when it downloads.
    pub queued: usize,
}

/// A check reads at most this many files (a huge gallery lists slowly).
const CHECK_LIMIT: usize = 1000;

/// gallery-dl, ready for its arguments.
pub fn command() -> Option<Command> {
    #[cfg(target_os = "android")]
    {
        let lib = lib_dir();
        if !lib.join("gallery_dl").is_dir() {
            return None;
        }
        let mut command = crate::platform::python_command()?;
        command.env("PYTHONPATH", &lib).args(["-m", "gallery_dl"]);
        Some(command)
    }
    #[cfg(target_os = "ios")]
    {
        None
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        crate::platform::find_tool("gallery-dl").map(Command::new)
    }
}

/// Whether gallery-dl can run (on Android: installed, and Python with it).
pub fn available() -> bool {
    #[cfg(target_os = "android")]
    {
        lib_dir().join("gallery_dl").is_dir() && crate::platform::python_ready()
    }
    // In the app's own Python.
    #[cfg(target_os = "ios")]
    {
        crate::children::available(crate::children::Tool::GalleryDl)
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        command().is_some()
    }
}

fn cookie_args() -> Vec<String> {
    let file = crate::extractors::http::cookie_file();
    if file.is_file() { vec!["--cookies".to_string(), file.to_string_lossy().into_owned()] } else { Vec::new() }
}

/// The gallery at `url`, without downloading it (`gallery-dl -j`). Err
/// says "Unsupported URL" for a site gallery-dl doesn't know.
pub async fn probe(url: &str) -> Result<Gallery, String> {
    let link = url.to_string();
    let (success, json, stderr) = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut args: Vec<String> = ["-j", "--range", &format!("1-{CHECK_LIMIT}"), "-o", "extractor.timeout=15"].map(String::from).into();
        args.extend(cookie_args());
        args.push(link.clone());
        let mut child = crate::children::spawn_tool(crate::children::Tool::GalleryDl, &args)?;
        // Stopped if nobody's waiting for it any more (children::stop_checks).
        crate::children::checking(child.id());
        let mut stderr = child.stderr.take();
        let errors = std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(stderr) = stderr.as_mut() {
                let _ = stderr.read_to_string(&mut text);
            }
            text
        });
        let mut json = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            let _ = stdout.read_to_string(&mut json);
        }
        let success = child.wait();
        crate::children::checked(child.id());
        let success = success?;
        Ok::<_, std::io::Error>((success, json, errors.join().unwrap_or_default()))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("Couldn't run gallery-dl: {e}"))?;

    if stderr.contains("No suitable extractor found") || stderr.contains("Unsupported URL") {
        return Err("Unsupported URL".to_string());
    }
    let gallery = parse(&json);
    if gallery.files.is_empty() && gallery.queued == 0 {
        let why = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Nothing found");
        if !success {
            log::warn!(target: "ytdlp", "gallery-dl checking {url}: {why}");
        }
        return Err(why.split("] ").last().unwrap_or(why).to_string());
    }
    Ok(gallery)
}

/// gallery-dl's -j output: a list of messages, [2, metadata] for a
/// directory, [3, url, metadata] for a file, [6, url, metadata] for a
/// gallery it leads to.
fn parse(json: &str) -> Gallery {
    let Ok(serde_json::Value::Array(messages)) = serde_json::from_str::<serde_json::Value>(json) else {
        return Gallery::default();
    };
    let mut gallery = Gallery::default();
    let mut meta: Option<serde_json::Value> = None;
    for message in &messages {
        match message[0].as_i64() {
            Some(2) if meta.is_none() => meta = Some(message[1].clone()),
            Some(3) => {
                let (url, data) = (message[1].as_str().unwrap_or_default(), &message[2]);
                let stem = data["filename"].as_str().filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| {
                    url.split(['?', '#']).next().unwrap_or(url).rsplit('/').next().unwrap_or("file").to_string()
                });
                let name = match data["extension"].as_str().filter(|e| !e.is_empty()) {
                    Some(ext) if !stem.ends_with(&format!(".{ext}")) => format!("{stem}.{ext}"),
                    _ => stem,
                };
                gallery.files.push((name, url.to_string()));
                if meta.is_none() {
                    meta = Some(data.clone());
                }
            }
            Some(6) => {
                gallery.queued += 1;
                if meta.is_none() {
                    meta = Some(message[2].clone());
                }
            }
            _ => {}
        }
    }
    gallery.title = meta.as_ref().map(title).unwrap_or_default();
    gallery
}

/// A gallery's name from its metadata: its title if it has one, else the
/// site and whose it is.
fn title(meta: &serde_json::Value) -> String {
    let text = |key: &str| match &meta[key] {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        // imgur's album, a user: their own title or name.
        serde_json::Value::Object(o) => {
            ["title", "name"].iter().find_map(|k| o.get(*k).and_then(|n| n.as_str()).map(str::trim).filter(|n| !n.is_empty())).map(str::to_string)
        }
        _ => None,
    };
    // The gallery's own (an album's) before a file's.
    if let Some(title) = ["album", "gallery", "gallery_title", "title", "name"].iter().find_map(|k| text(k)) {
        return title;
    }
    let site = text("category").unwrap_or_default();
    let whose = ["user", "author", "username", "artist", "uploader", "id"].iter().find_map(|k| text(k)).unwrap_or_default();
    [site, whose].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

/// The arguments for downloading `request` from `url` into `dir`: each file
/// saved straight in it, its path printed as it's saved.
pub fn download_args(url: &str, request: &GalleryRequest, dir: &str) -> Vec<String> {
    let mut args = vec!["-D".to_string(), dir.to_string(), "-o".to_string(), "extractor.timeout=30".to_string()];
    if !request.items.is_empty() {
        let picked: Vec<String> = request.items.iter().map(|n| n.to_string()).collect();
        args.extend(["--range".to_string(), picked.join(",")]);
    }
    args.extend(cookie_args());
    args.push(url.to_string());
    args
}

/// What a download printed for a file: its path ("# path" when it was
/// there already).
pub fn saved_path(line: &str) -> Option<PathBuf> {
    let path = line.trim();
    let path = path.strip_prefix("# ").unwrap_or(path);
    (!path.is_empty() && (path.starts_with('/') || path.chars().nth(1) == Some(':'))).then(|| PathBuf::from(path))
}

// --- gallery-dl's wheels, run by a Python (Android's yt-dlp package) ---

/// gallery-dl and what it needs, all pure Python.
const WHEELS: [&str; 6] = ["gallery-dl", "requests", "urllib3", "idna", "charset-normalizer", "certifi"];

fn base_dir() -> PathBuf {
    PathBuf::from(crate::platform::get_app_data_dir()).join("gallery-dl")
}

/// Where the wheels are unpacked (on PYTHONPATH).
fn lib_dir() -> PathBuf {
    base_dir().join("lib")
}

/// The version installed, if it is.
pub fn installed_version() -> Option<String> {
    lib_dir().join("gallery_dl").is_dir().then(|| std::fs::read_to_string(base_dir().join("version")).unwrap_or_default().trim().to_string())
}

/// A small PyPI page as text, tried a few times (on a poor connection a
/// response is often cut off).
async fn pypi_text(url: &str) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2 * attempt)).await;
        }
        let response = crate::extractors::http::client().get(url).send().await.and_then(|r| r.error_for_status());
        match response {
            Ok(r) => match r.text().await {
                Ok(text) => return Ok(text),
                Err(e) => last = e.to_string(),
            },
            // Not there: asking again won't help.
            Err(e) if e.status().is_some_and(|s| s.is_client_error()) => return Err(e.to_string()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// A version's parts, for a final release only ("1.32.19", not "2.0.0rc1").
fn release_parts(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|p| p.parse().ok()).collect()
}

/// A package's newest release, from its feed of recent ones (a few KB;
/// its full JSON lists every release's files, megabytes for some). The
/// highest, not the latest posted: a fix to an older line comes later.
async fn newest_version(name: &str) -> Result<String, String> {
    let feed = pypi_text(&format!("https://pypi.org/rss/project/{name}/releases.xml")).await?;
    feed.split("<title>")
        .skip(1)
        .filter_map(|t| t.split("</title>").next())
        .map(str::trim)
        .filter_map(|v| release_parts(v).map(|parts| (parts, v)))
        .max()
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| format!("{name} has no release"))
}

/// A package's pure-Python wheel on PyPI: its version, address and size.
async fn wheel(name: &str) -> Result<(String, String, u64), String> {
    let version = newest_version(name).await?;
    let text = pypi_text(&format!("https://pypi.org/pypi/{name}/{version}/json")).await?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let file = json["urls"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|u| u["filename"].as_str().is_some_and(|f| f.ends_with("-py3-none-any.whl") || f.ends_with("-py2.py3-none-any.whl")))
        .ok_or_else(|| format!("{name} has no pure-Python wheel"))?;
    Ok((version, file["url"].as_str().unwrap_or_default().to_string(), file["size"].as_u64().unwrap_or(0)))
}

/// gallery-dl's newest version and its wheel's size.
pub async fn latest() -> Result<(String, u64), String> {
    wheel("gallery-dl").await.map(|(version, _, size)| (version, size))
}

/// Downloads gallery-dl's wheels and those it needs, and unpacks them in
/// place of the ones before. Returns the version installed. Wheels already
/// downloaded by a try that failed are kept, and not downloaded again.
pub async fn install(progress: impl Fn(f32)) -> Result<String, String> {
    let base = base_dir();
    let downloads = base.join("download");
    let mut wheels = Vec::new();
    for name in WHEELS {
        let (version, url, size) = wheel(name).await?;
        let file = url.rsplit('/').next().unwrap_or(name).to_string();
        wheels.push((version, url, size, file));
    }
    let version = wheels[0].0.clone();
    // The progress bar moves with the bytes (gallery-dl's is most of them).
    let total = wheels.iter().map(|w| w.2).sum::<u64>().max(1) as f32;
    let mut done = 0;
    let mut files = Vec::new();
    for (_, url, size, file) in &wheels {
        let path = downloads.join(file);
        let whole = tokio::fs::metadata(&path).await.is_ok_and(|m| *size > 0 && m.len() == *size);
        if !whole {
            crate::downloader::fetch_file(url, &downloads, file, |completed, _| {
                progress((done + completed.min(*size)) as f32 / total);
            })
            .await?;
        }
        done += size;
        progress(done as f32 / total);
        files.push(path);
    }
    let (fresh, lib) = (base.join("lib.new"), lib_dir());
    let unpacked = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let _ = std::fs::remove_dir_all(&fresh);
        for file in &files {
            let opened = std::fs::File::open(file).map_err(|e| e.to_string())?;
            zip::ZipArchive::new(opened).and_then(|mut zip| zip.extract(&fresh)).map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_dir_all(&lib);
        std::fs::rename(&fresh, &lib).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?;
    // Gone even when unpacking failed: a wheel that's whole but broken
    // would fail every try.
    let _ = tokio::fs::remove_dir_all(&downloads).await;
    unpacked?;
    tokio::fs::write(base.join("version"), &version).await.map_err(|e| e.to_string())?;
    Ok(version)
}
