//! Everything else, through yt-dlp, adapted from urlsula's ytdlp extractor:
//! one `--dump-single-json` per link (playlist entries become items), the
//! media type, qualities by resolution with their sizes, a thumbnail, and
//! the download arguments (exact resolution, audio extraction with an
//! embedded square cover, images, chosen playlist items).
//!
//! The info JSON is saved in the app's data folder so the download can
//! reuse it (`--load-info-json`) instead of asking the site again; the
//! popup, the app and the download service all read the same folder.

use super::{MediaInfo, MediaItem, MediaKind, MediaRequest, Quality, Service};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a saved info file is reused (sites' media links expire).
const INFO_MAX_AGE: Duration = Duration::from_secs(30 * 60);

fn is_audio_site(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("music.youtube.com") || lower.contains("soundcloud.com") || lower.contains("sndcdn.com")
}

/// The link without tracking parameters, so the same video shared twice
/// shares its saved info (keeps v, list, p and id).
fn clean_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(mut parsed) = url::Url::parse(trimmed) else {
        return trimmed.split('#').next().unwrap_or(trimmed).trim_end_matches('/').to_string();
    };
    let keep: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| matches!(k.as_ref(), "v" | "list" | "p" | "id"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    parsed.set_query(None);
    parsed.set_fragment(None);
    if !keep.is_empty() {
        parsed.query_pairs_mut().extend_pairs(keep);
    }
    parsed.to_string().trim_end_matches('/').to_string()
}

/// Where a link's info JSON is saved.
fn info_path(url: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    clean_url(url).hash(&mut hasher);
    PathBuf::from(crate::platform::get_app_data_dir())
        .join("ytdlp-info")
        .join(format!("{:016x}.json", hasher.finish()))
}

/// A saved info file, if it's recent enough to reuse.
pub fn fresh_info(path: &Path) -> Option<PathBuf> {
    let age = std::fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()?;
    (age < INFO_MAX_AGE).then(|| path.to_path_buf())
}

/// The cookies.txt the user may have put in the app's folder.
fn cookie_args() -> Vec<String> {
    let file = super::http::cookie_file();
    if file.is_file() {
        vec!["--cookies".to_string(), file.to_string_lossy().into_owned()]
    } else {
        Vec::new()
    }
}

pub async fn get_info(url: &str) -> Result<MediaInfo, String> {
    let link = url.to_string();
    let output = tokio::task::spawn_blocking(move || {
        crate::platform::ytdlp_command()
            .ok_or_else(|| std::io::Error::other("yt-dlp isn't available"))?
            .args(["--dump-single-json", "--skip-download", "--no-warnings", "--socket-timeout", "15"])
            .args(cookie_args())
            .arg(&link)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("Couldn't run yt-dlp: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed");
        return Err(last.trim_start_matches("ERROR: ").to_string());
    }

    let json: Value = serde_json::from_slice(&output.stdout).map_err(|e| format!("Unexpected yt-dlp output: {e}"))?;
    let path = info_path(url);
    let saved = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| std::fs::write(&path, &output.stdout));
    if let Err(e) = &saved {
        log::warn!("couldn't save yt-dlp info: {e}");
    }

    let id = json.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string();
    let items = match json.get("entries").and_then(|e| e.as_array()) {
        Some(entries) => entries
            .iter()
            .filter(|e| e.is_object())
            .enumerate()
            .map(|(index, entry)| parse_entry(index, entry, url))
            .collect(),
        None => vec![parse_entry(0, &json, url)],
    };
    Ok(MediaInfo { service: Service::YtDlp, id, items, info_json: saved.ok().map(|_| path) })
}

/// The title (with the artist for music), else the description.
fn caption(entry: &Value, audio: bool) -> Option<String> {
    let text = |key: &str| entry.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let mut caption = text("title").or_else(|| text("description")).map(String::from);
    if audio {
        if let Some(artist) = text("artist") {
            match &mut caption {
                Some(c) if !c.contains(artist) => *c = format!("{c} - {artist}"),
                Some(_) => {}
                None => caption = Some(artist.to_string()),
            }
        }
    }
    caption
}

/// A thumbnail Slint can show (JPEG or PNG): for audio the largest square
/// one (cover art), otherwise the largest.
fn thumbnail(entry: &Value, audio: bool) -> String {
    let decodable = |u: &str| {
        let path = u.split(['?', '#']).next().unwrap_or(u).to_lowercase();
        path.ends_with(".jpg") || path.ends_with(".jpeg") || path.ends_with(".png")
    };
    let thumbs = entry.get("thumbnails").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let size = |t: &Value| (t.get("width").and_then(|v| v.as_u64()), t.get("height").and_then(|v| v.as_u64()));
    let best = thumbs
        .iter()
        .filter(|t| t.get("url").and_then(|u| u.as_str()).is_some_and(decodable))
        .filter(|t| !audio || matches!(size(t), (Some(w), Some(h)) if w == h))
        .max_by_key(|t| match size(t) {
            (Some(w), Some(h)) => w * h,
            // yt-dlp lists them worst to best when it has no sizes.
            _ => 0,
        })
        .or_else(|| thumbs.iter().rev().find(|t| t.get("url").and_then(|u| u.as_str()).is_some_and(decodable)));
    best.and_then(|t| t.get("url"))
        .and_then(|u| u.as_str())
        .or_else(|| entry.get("thumbnail").and_then(|t| t.as_str()).filter(|u| decodable(u)))
        .unwrap_or_default()
        .to_string()
}

fn parse_entry(index: usize, entry: &Value, url: &str) -> MediaItem {
    let formats = entry.get("formats").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    let image_exts = ["png", "jpg", "jpeg", "webp"];
    let vcodec = |f: &Value| f.get("vcodec").and_then(|v| v.as_str()).unwrap_or("none").to_string();
    let has_video = formats.iter().any(|f| !matches!(vcodec(f).as_str(), "none" | "mjpeg" | "png" | "webp"));
    let is_image = formats.iter().any(|f| {
        let ext = |key: &str| f.get(key).and_then(|e| e.as_str()).unwrap_or("none").to_string();
        image_exts.contains(&ext("ext").as_str()) || image_exts.contains(&ext("video_ext").as_str())
    });
    let kind = if is_image && !has_video {
        MediaKind::Image
    } else if is_audio_site(url) || !has_video {
        MediaKind::Audio
    } else {
        MediaKind::Video
    };
    let duration = entry.get("duration").and_then(|d| d.as_f64());
    let size_of = |f: &Value| f.get("filesize").or_else(|| f.get("filesize_approx")).and_then(|s| s.as_u64()).unwrap_or(0);
    let dims = |f: &Value| match (f.get("width").and_then(|w| w.as_u64()), f.get("height").and_then(|h| h.as_u64())) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some(format!("{w}:{h}")),
        _ => None,
    };

    let mut qualities = Vec::new();
    match kind {
        MediaKind::Audio => qualities = super::audio_presets(duration),
        MediaKind::Image => {
            for f in &formats {
                let key = dims(f)
                    .or_else(|| f.get("format_id").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()).map(String::from))
                    .unwrap_or_else(|| "original".to_string());
                super::add_quality(&mut qualities, key, size_of(f));
            }
        }
        MediaKind::Video => {
            // Each resolution with its video size plus the best audio
            // stream's, since that's what gets merged in.
            let audio_size = formats
                .iter()
                .filter(|f| vcodec(f) == "none" && f.get("acodec").and_then(|a| a.as_str()).is_some_and(|a| a != "none"))
                .map(size_of)
                .max()
                .unwrap_or(0);
            for f in formats.iter().filter(|f| vcodec(f) != "none") {
                let key = dims(f)
                    .or_else(|| f.get("format_note").and_then(|n| n.as_str()).filter(|s| !s.trim().is_empty()).map(String::from))
                    .unwrap_or_else(|| "default".to_string());
                let video_only = f.get("acodec").and_then(|a| a.as_str()) == Some("none");
                let size = size_of(f) + if video_only && size_of(f) > 0 { audio_size } else { 0 };
                super::add_quality(&mut qualities, key, size);
            }
        }
    }
    if qualities.is_empty() {
        let key = match kind {
            MediaKind::Audio => "audio",
            MediaKind::Image => "original",
            MediaKind::Video => "best",
        };
        qualities.push(Quality { key: key.to_string(), size: size_of(entry) });
    }
    super::sort_qualities(&mut qualities);

    let audio = kind == MediaKind::Audio;
    MediaItem {
        index,
        kind,
        ext: match kind {
            MediaKind::Audio => "m4a".to_string(),
            MediaKind::Video => "mp4".to_string(),
            MediaKind::Image => entry.get("ext").and_then(|e| e.as_str()).filter(|e| *e != "none").unwrap_or("png").to_string(),
        },
        thumbnail: thumbnail(entry, audio),
        qualities,
        caption: caption(entry, audio),
        duration,
    }
}

/// yt-dlp's progress lines, made easy to read back: "eddy <done> <total>".
pub const PROGRESS_TEMPLATE: &str =
    "download:eddy %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s";

/// Printed once per file yt-dlp saved, with its final path (after merging
/// or converting), so the notification can name the file it made.
pub const SAVED_PREFIX: &str = "eddyfile:";

/// The arguments for downloading `request` into `dir` (the URL or saved
/// info last). `name` names a single item's file (no extension).
pub fn download_args(
    url: &str,
    request: &MediaRequest,
    dir: &str,
    name: Option<&str>,
    settings: &crate::downloader::DownloadSettings,
) -> Vec<String> {
    // --print makes yt-dlp quiet; --progress keeps the progress lines.
    let saved = format!("after_move:{SAVED_PREFIX}%(filepath)s");
    let mut args: Vec<String> =
        ["--newline", "--no-warnings", "--progress", "--progress-template", PROGRESS_TEMPLATE, "--print", &saved]
            .iter()
            .map(|s| s.to_string())
            .collect();
    // Speed limit, fragments at once (HLS/DASH). The user agent is left to
    // yt-dlp, which knows what each site wants.
    if settings.max_speed > 0 {
        args.extend(["--limit-rate".to_string(), settings.max_speed.to_string()]);
    }
    args.extend(["--concurrent-fragments".to_string(), settings.connections().min(8).to_string()]);
    let several = request.items.len() > 1;
    let template = match name {
        Some(name) if !several => format!("{dir}/{name}.%(ext)s"),
        _ => format!("{dir}/%(title)s.%(ext)s"),
    };
    let quality = if request.quality.is_empty() { "best" } else { request.quality.as_str() };

    if !request.audio_codec.is_empty() {
        // Audio: extract (or convert) it, tag it, and give it a square cover.
        args.extend(["-f", "bestaudio/ba/b", "-x", "--audio-format"].map(String::from));
        args.push(request.audio_codec.clone());
        if request.audio_kbps > 0 {
            args.extend(["--audio-quality".to_string(), format!("{}K", request.audio_kbps)]);
        }
        args.extend(
            [
                "--embed-metadata",
                "--embed-thumbnail",
                "--convert-thumbnails",
                "png",
                "--ppa",
                r#"ThumbnailsConvertor+FFmpeg_o:-c:v png -vf crop="'if(gt(ih,iw),iw,ih)':'if(gt(iw,ih),ih,iw)'""#,
            ]
            .map(String::from),
        );
    } else if request.image {
        let spec = match quality.split_once(':') {
            Some((w, h)) => format!("*[width={w}][height={h}]/best"),
            None if quality == "best" || quality == "original" => "best".to_string(),
            None => format!("{quality}/best"),
        };
        args.extend(["-f".to_string(), spec]);
    } else {
        let spec = match quality.split_once(':') {
            Some((w, h)) => format!("bv*[width={w}][height={h}]+ba/bv*[height={h}]+ba/b"),
            None if quality == "best" => "bv*+ba/b".to_string(),
            None => format!("bv*[format_note*={quality}]+ba/b[format_note*={quality}]/b"),
        };
        args.extend(["-f".to_string(), spec, "--merge-output-format".to_string(), "mp4".to_string()]);
    }

    args.extend(["-o".to_string(), template]);
    if !request.items.is_empty() {
        let items: Vec<String> = request.items.iter().map(|i| (i + 1).to_string()).collect();
        args.extend(["--playlist-items".to_string(), items.join(",")]);
    } else {
        args.push("--no-playlist".to_string());
    }
    args.extend(cookie_args());
    match request.info_json.as_deref().and_then(fresh_info) {
        Some(info) => args.extend(["--load-info-json".to_string(), info.to_string_lossy().into_owned()]),
        None => args.push(url.to_string()),
    }
    args
}

/// Bytes done and total from one of yt-dlp's progress lines.
pub fn parse_progress(line: &str) -> Option<(u64, u64)> {
    let mut parts = line.strip_prefix("eddy ")?.split_whitespace();
    let number = |s: Option<&str>| s.and_then(|s| s.parse::<f64>().ok()).map(|n| n as u64);
    let done = number(parts.next())?;
    let total = number(parts.next()).or_else(|| number(parts.next())).unwrap_or(0);
    Some((done, total))
}
