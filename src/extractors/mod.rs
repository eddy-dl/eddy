//! Media extractors, adapted from urlsula: finding what a page holds (videos,
//! images, audio), in which qualities and sizes, with a thumbnail and
//! caption, and fetching it.
//!
//! The TikTok, Instagram and Pinterest extractors pick up where yt-dlp is
//! slow, blocked or missing (it can't run on iOS at all); yt-dlp handles every
//! other site, and steps in when one of them fails.

pub mod http;
pub mod instagram;
pub mod pinterest;
pub mod tiktok;
pub mod ytdlp;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which extractor found a link's media (and fetches it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Service {
    TikTok,
    Instagram,
    Pinterest,
    YtDlp,
}

impl Service {
    /// How the service is shown in the preview's "Download with" chips.
    pub fn label(self) -> &'static str {
        match self {
            Service::TikTok => "TikTok",
            Service::Instagram => "Instagram",
            Service::Pinterest => "Pinterest",
            Service::YtDlp => "yt-dlp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

/// One quality an item can be fetched in.
#[derive(Clone, Debug)]
pub struct Quality {
    /// What the extractor understands: "1920:1080" (width:height), "128k",
    /// "best", "original", "default", or a format note.
    pub key: String,
    /// Estimated size in bytes; 0 when unknown.
    pub size: u64,
}

/// One downloadable thing on a page: a video, an image of a carousel, a
/// playlist entry.
#[derive(Clone, Debug)]
pub struct MediaItem {
    pub index: usize,
    pub kind: MediaKind,
    /// Usual file extension ("mp4", "jpg", "m4a"…).
    pub ext: String,
    pub thumbnail: String,
    /// Best first.
    pub qualities: Vec<Quality>,
    pub caption: Option<String>,
    /// Seconds, when known (audio sizes are estimated from it).
    pub duration: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub service: Service,
    pub id: String,
    pub items: Vec<MediaItem>,
    /// yt-dlp's full info, saved so the download can reuse it
    /// (`--load-info-json`) instead of asking the site again.
    pub info_json: Option<PathBuf>,
}

/// What the user picked, handed from the preview to the downloader (as JSON,
/// since it crosses into the download service).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MediaRequest {
    pub service: Option<Service>,
    /// Items to fetch (MediaItem::index); empty for all.
    pub items: Vec<usize>,
    /// A Quality::key, or "best".
    pub quality: String,
    /// Convert to audio: "mp3" / "m4a" (empty to keep the media as it is),
    /// at `audio_kbps` (0 for the best the source has).
    pub audio_codec: String,
    pub audio_kbps: u32,
    /// Saved yt-dlp info, if the preview's check left one.
    pub info_json: Option<PathBuf>,
    /// The items are images (yt-dlp picks formats differently for them).
    pub image: bool,
}

/// Finds a link's media: the matching native extractor first, then yt-dlp
/// (also when the native one fails).
pub async fn probe(url: &str) -> Result<MediaInfo, String> {
    let native = match native_service(url) {
        Some(Service::TikTok) => Some(tiktok::get_info(url).await),
        Some(Service::Instagram) => Some(instagram::get_info(url).await),
        Some(Service::Pinterest) => Some(pinterest::get_info(url).await),
        _ => None,
    };
    let native_error = match native {
        Some(Ok(info)) if !info.items.is_empty() => return Ok(info),
        Some(Ok(_)) => Some("No media found".to_string()),
        Some(Err(e)) => {
            log::warn!("native extractor failed for {url}: {e}");
            Some(e)
        }
        None => None,
    };
    if crate::platform::ytdlp_available() {
        ytdlp::get_info(url).await
    } else {
        Err(native_error.unwrap_or_else(|| "yt-dlp isn't available".to_string()))
    }
}

/// The native extractor for a link, if one handles it.
pub fn native_service(url: &str) -> Option<Service> {
    if tiktok::can_handle(url) {
        Some(Service::TikTok)
    } else if instagram::can_handle(url) {
        Some(Service::Instagram)
    } else if pinterest::can_handle(url) {
        Some(Service::Pinterest)
    } else {
        None
    }
}

/// A file a native extractor resolved for downloading.
#[derive(Clone, Debug)]
pub struct Fetch {
    pub index: usize,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub kind: MediaKind,
    pub ext: String,
    /// An HLS playlist (needs ffmpeg to save).
    pub hls: bool,
    pub caption: Option<String>,
}

/// Fresh media URLs for the requested items (native services only; they
/// expire, so they're fetched again at download time).
pub async fn resolve(service: Service, url: &str, request: &MediaRequest) -> Result<Vec<Fetch>, String> {
    match service {
        Service::TikTok => tiktok::resolve(url, request).await,
        Service::Instagram => instagram::resolve(url, request).await,
        Service::Pinterest => pinterest::resolve(url, request).await,
        Service::YtDlp => Err("yt-dlp downloads itself".to_string()),
    }
}

/// Whether item `index` was asked for.
pub(crate) fn wanted(request: &MediaRequest, index: usize) -> bool {
    request.items.is_empty() || request.items.contains(&index)
}

/// Sorts qualities best first, as urlsula does: by pixels for "w:h", by
/// bitrate for "128k", with "best"/"original" on top.
pub fn sort_qualities(qualities: &mut [Quality]) {
    fn weight(q: &str) -> u64 {
        if let Some((w, h)) = q.split_once(':') {
            if let (Ok(w), Ok(h)) = (w.parse::<u64>(), h.parse::<u64>()) {
                return w * h;
            }
        }
        if let Some(abr) = q.strip_suffix('k') {
            if let Ok(n) = abr.parse::<u64>() {
                return n;
            }
        }
        match q {
            "best" | "original" => u64::MAX,
            "audio" | "default" => 1,
            _ => q.parse().unwrap_or(0),
        }
    }
    qualities.sort_by_key(|q| std::cmp::Reverse(weight(&q.key)));
}

/// Keeps the largest size for each quality key.
pub(crate) fn add_quality(qualities: &mut Vec<Quality>, key: String, size: u64) {
    match qualities.iter_mut().find(|q| q.key == key) {
        Some(existing) => existing.size = existing.size.max(size),
        None => qualities.push(Quality { key, size }),
    }
}

/// The audio bitrates offered when converting to audio, with sizes
/// estimated from the duration (bitrate × seconds / 8).
pub fn audio_presets(duration: Option<f64>) -> Vec<Quality> {
    [320u64, 256, 192, 128, 64]
        .iter()
        .map(|&kbps| Quality {
            key: format!("{kbps}k"),
            size: duration.map(|d| (kbps as f64 * 1000.0 * d / 8.0) as u64).unwrap_or(0),
        })
        .collect()
}
