//! Pinterest pins (videos in several resolutions, story pins, images, GIFs),
//! adapted from urlsula's pinterest_dl: short links go through Pinterest's
//! shortener, pin data comes from its Resource API, sizes from range
//! requests. Some videos are HLS streams, which ffmpeg saves.

use super::http;
use super::{Fetch, MediaInfo, MediaItem, MediaKind, MediaRequest, Quality, Service};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

static SHORT_PIN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:www\.)?pin\.(?:it|co|com|de|es|fr|uk|ca|mx|br|jp|in|nz|au|se|no|dk|fi|nl|be|at|ch)/([a-zA-Z0-9_-]+)").unwrap()
});
static FULL_PIN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://(?:[^/]+\.)?pinterest\.[^/]+/pin/(?:[\w-]+--)?(\d+)").unwrap());

const REFERER: &str = "https://www.pinterest.com/";

#[derive(Debug, Clone)]
struct Variant {
    url: String,
    width: u64,
    height: u64,
    quality: String,
    thumbnail: String,
    size: u64,
}

#[derive(Debug, Clone)]
enum PinMedia {
    /// Best resolution first.
    Video { variants: Vec<Variant>, caption: Option<String> },
    Image { url: String, width: u64, height: u64, size: u64, caption: Option<String> },
    Gif { url: String, size: u64, caption: Option<String> },
}

/// Pin data per pin id for 5 minutes.
static CACHE: LazyLock<Mutex<HashMap<String, (Value, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn can_handle(url: &str) -> bool {
    SHORT_PIN_RE.is_match(url) || FULL_PIN_RE.is_match(url)
}

async fn pin_id(url: &str) -> Result<String, String> {
    let clean = url.split('?').next().unwrap_or(url);
    if let Some(id) = FULL_PIN_RE.captures(clean).and_then(|c| c.get(1)) {
        return Ok(id.as_str().to_string());
    }
    if let Some(short) = SHORT_PIN_RE.captures(clean).and_then(|c| c.get(1)) {
        let api = format!("https://api.pinterest.com/url_shortener/{}/redirect/", short.as_str());
        let response = http::no_redirect_client().get(&api).send().await.map_err(|e| e.to_string())?;
        if let Some(id) = response
            .headers()
            .get("location")
            .and_then(|l| l.to_str().ok())
            .and_then(|l| FULL_PIN_RE.captures(l))
            .and_then(|c| c.get(1))
        {
            return Ok(id.as_str().to_string());
        }
    }
    Err(format!("Not a supported Pinterest pin: {url}"))
}

async fn pin_data(id: &str) -> Result<Value, String> {
    if let Some((data, at)) = CACHE.lock().unwrap().get(id) {
        if at.elapsed() < Duration::from_secs(300) {
            return Ok(data.clone());
        }
    }
    let payload = serde_json::json!({ "options": { "field_set_key": "unauth_react_main_pin", "id": id } });
    let mut url = url::Url::parse("https://www.pinterest.com/resource/PinResource/get/").unwrap();
    url.query_pairs_mut().append_pair("data", &payload.to_string());
    let text = http::client()
        .get(url)
        .header("Accept", "application/json, text/plain, */*")
        .header("X-Pinterest-Pws-Handler", "www/[username].js")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let data = json
        .pointer("/resource_response/data")
        .cloned()
        .ok_or_else(|| "Pinterest returned no pin data".to_string())?;
    CACHE.lock().unwrap().insert(id.to_string(), (data.clone(), Instant::now()));
    Ok(data)
}

fn parse_video_list(list: &Value) -> Vec<Variant> {
    let mut variants: Vec<Variant> = list
        .as_object()
        .into_iter()
        .flat_map(|map| map.values())
        .filter_map(|v| {
            let url = v.get("url")?.as_str()?.to_string();
            let width = v.get("width").and_then(|w| w.as_u64()).unwrap_or(0);
            let height = v.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
            Some(Variant {
                thumbnail: v
                    .get("thumbnail")
                    .or_else(|| v.get("cover_image_url"))
                    .and_then(|t| t.as_str())
                    .unwrap_or(&url)
                    .to_string(),
                size: v.get("file_size").or_else(|| v.get("filesize")).and_then(|s| s.as_u64()).unwrap_or(0),
                quality: if width > 0 && height > 0 { format!("{width}:{height}") } else { "default".to_string() },
                url,
                width,
                height,
            })
        })
        .collect();
    variants.sort_by_key(|v| std::cmp::Reverse(v.width * v.height));
    variants
}

fn parse_pin(pin: &Value) -> Vec<PinMedia> {
    let caption = pin
        .get("title")
        .or_else(|| pin.get("grid_title"))
        .or_else(|| pin.get("description"))
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // 1. A video pin.
    if let Some(list) = pin.pointer("/videos/video_list") {
        let variants = parse_video_list(list);
        if !variants.is_empty() {
            return vec![PinMedia::Video { variants, caption }];
        }
    }

    // 2. A story pin: a video or image per page.
    let mut results = Vec::new();
    for page in pin.pointer("/story_pin_data/pages").and_then(|p| p.as_array()).into_iter().flatten() {
        let video = page
            .pointer("/video/video_list")
            .map(parse_video_list)
            .filter(|v| !v.is_empty())
            .or_else(|| {
                page.get("blocks")?.as_array()?.iter().find_map(|block| {
                    let list = block.pointer("/video/video_list")?;
                    Some(parse_video_list(list)).filter(|v| !v.is_empty())
                })
            });
        if let Some(variants) = video {
            results.push(PinMedia::Video { variants, caption: caption.clone() });
        } else if let Some(original) = page.pointer("/image/images/originals") {
            if let Some(url) = original.get("url").and_then(|u| u.as_str()) {
                results.push(PinMedia::Image {
                    url: url.to_string(),
                    width: original.get("width").and_then(|w| w.as_u64()).unwrap_or(0),
                    height: original.get("height").and_then(|h| h.as_u64()).unwrap_or(0),
                    size: 0,
                    caption: caption.clone(),
                });
            }
        }
    }
    if !results.is_empty() {
        return results;
    }

    // 3. An image pin.
    if let Some(original) = pin.pointer("/images/orig") {
        if let Some(url) = original.get("url").and_then(|u| u.as_str()) {
            return vec![PinMedia::Image {
                url: url.to_string(),
                width: original.get("width").and_then(|w| w.as_u64()).unwrap_or(0),
                height: original.get("height").and_then(|h| h.as_u64()).unwrap_or(0),
                size: 0,
                caption,
            }];
        }
    }

    // 4. An embedded GIF.
    if pin.pointer("/embed/type").and_then(|t| t.as_str()) == Some("gif") {
        if let Some(src) = pin.pointer("/embed/src").and_then(|s| s.as_str()) {
            return vec![PinMedia::Gif { url: src.to_string(), size: 0, caption }];
        }
    }
    Vec::new()
}

fn headers() -> Vec<(String, String)> {
    vec![("Referer".to_string(), REFERER.to_string())]
}

pub async fn get_info(url: &str) -> Result<MediaInfo, String> {
    let id = pin_id(url).await?;
    let mut media = parse_pin(&pin_data(&id).await?);
    if media.is_empty() {
        return Err("Nothing downloadable on this pin".to_string());
    }
    // Fill in the sizes the API left out.
    for item in &mut media {
        match item {
            PinMedia::Video { variants, .. } => {
                for v in variants.iter_mut().filter(|v| v.size == 0) {
                    v.size = http::content_length(&v.url, &headers()).await.unwrap_or(0);
                }
            }
            PinMedia::Image { url, size, .. } | PinMedia::Gif { url, size, .. } => {
                if *size == 0 {
                    *size = http::content_length(url, &headers()).await.unwrap_or(0);
                }
            }
        }
    }

    let items = media
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            PinMedia::Video { variants, caption } => {
                let mut qualities = Vec::new();
                for v in variants {
                    super::add_quality(&mut qualities, v.quality.clone(), v.size);
                }
                MediaItem {
                    index,
                    kind: MediaKind::Video,
                    ext: "mp4".to_string(),
                    thumbnail: variants.first().map(|v| v.thumbnail.clone()).unwrap_or_default(),
                    qualities,
                    caption: caption.clone(),
                    duration: None,
                }
            }
            PinMedia::Image { url, width, height, size, caption } => MediaItem {
                index,
                kind: MediaKind::Image,
                ext: "jpg".to_string(),
                thumbnail: url.clone(),
                qualities: vec![Quality {
                    key: if *width > 0 && *height > 0 { format!("{width}:{height}") } else { "default".to_string() },
                    size: *size,
                }],
                caption: caption.clone(),
                duration: None,
            },
            PinMedia::Gif { url, size, caption } => MediaItem {
                index,
                kind: MediaKind::Image,
                ext: "gif".to_string(),
                thumbnail: url.clone(),
                qualities: vec![Quality { key: "default".to_string(), size: *size }],
                caption: caption.clone(),
                duration: None,
            },
        })
        .collect();
    Ok(MediaInfo { service: Service::Pinterest, id, items, info_json: None })
}

pub async fn resolve(url: &str, request: &MediaRequest) -> Result<Vec<Fetch>, String> {
    let id = pin_id(url).await?;
    let media = parse_pin(&pin_data(&id).await?);
    Ok(media
        .into_iter()
        .enumerate()
        .filter(|(i, _)| super::wanted(request, *i))
        .filter_map(|(index, item)| {
            let (url, kind, ext, caption) = match item {
                // The asked-for resolution, else the best.
                PinMedia::Video { variants, caption } => {
                    let v = variants.iter().find(|v| v.quality == request.quality).or(variants.first())?;
                    (v.url.clone(), MediaKind::Video, "mp4", caption)
                }
                PinMedia::Image { url, caption, .. } => (url, MediaKind::Image, "jpg", caption),
                PinMedia::Gif { url, caption, .. } => (url, MediaKind::Image, "gif", caption),
            };
            Some(Fetch { index, hls: url.contains(".m3u8"), url, headers: headers(), kind, ext: ext.to_string(), caption })
        })
        .collect())
}
