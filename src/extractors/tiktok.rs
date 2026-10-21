//! TikTok videos and photo posts, adapted from urlsula's tiktok_dl: resolve
//! short links, read the page's rehydration JSON (solving TikTok's WAF
//! challenge when it shows one), and fetch the media with the page's cookies.

use super::http::{self, CookieJar};
use super::{Fetch, MediaInfo, MediaItem, MediaKind, MediaRequest, Quality, Service};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

static SHORT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://((?:vm|vt|www)\.)?(vx)?tiktok\.com/(?:t/)?([a-zA-Z0-9-]+)").unwrap());
static FULL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://((?:www|m)\.)?(vx)?tiktok\.com/(embed/|@[^/]+/)?(video|photo)/(\d+)").unwrap()
});
static UNIVERSAL_DATA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<script[^>]*id="__UNIVERSAL_DATA_FOR_REHYDRATION__"[^>]*>(.*?)</script>"#).unwrap()
});

const VIDEO_URL_BASE: &str = "https://www.tiktok.com/@_/video/";

/// Page data per video id for 5 minutes, so a download right after the
/// preview doesn't scrape the page again: (item JSON, page URL, cookies).
static CACHE: LazyLock<Mutex<HashMap<String, (Value, String, CookieJar, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn can_handle(url: &str) -> bool {
    SHORT_RE.is_match(url) || FULL_RE.is_match(url)
}

fn id_from(url: &str) -> Option<String> {
    FULL_RE.captures(url).and_then(|c| c.get(5)).map(|m| m.as_str().to_string())
}

/// The video id and its canonical page, following short links.
async fn resolve_id(url: &str) -> Result<(String, String), String> {
    let clean = url.split('?').next().unwrap_or(url);
    if let Some(id) = id_from(clean) {
        return Ok((id.clone(), format!("{VIDEO_URL_BASE}{id}")));
    }

    let mut current = clean.to_string();
    for _ in 0..5 {
        let response = http::no_redirect_client().get(&current).send().await.map_err(|e| e.to_string())?;
        if response.status().is_redirection() {
            let Some(next) = response.headers().get("location").and_then(|l| l.to_str().ok()) else {
                break;
            };
            // A login wall (geo-restricted) still carries the target.
            if next.contains("/login") && next.contains("redirect_url=") {
                let target = url::Url::parse(next)
                    .ok()
                    .and_then(|u| u.query_pairs().find(|(k, _)| k == "redirect_url").map(|(_, v)| v.into_owned()));
                if let Some(id) = target.as_deref().and_then(id_from) {
                    return Ok((id.clone(), format!("{VIDEO_URL_BASE}{id}")));
                }
                return Err("Geo-restricted: this TikTok needs cookies".to_string());
            }
            current = next.to_string();
            if let Some(id) = id_from(&current) {
                return Ok((id.clone(), format!("{VIDEO_URL_BASE}{id}")));
            }
        } else if response.status().as_u16() == 429 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else {
            break;
        }
    }
    id_from(&current)
        .map(|id| (id.clone(), format!("{VIDEO_URL_BASE}{id}")))
        .ok_or_else(|| "Couldn't find the TikTok video".to_string())
}

/// Finds `keys` as a path anywhere in the JSON, backtracking from dead ends.
fn traverse(value: &Value, keys: &[&str]) -> Option<Value> {
    let Some((key, rest)) = keys.split_first() else {
        return Some(value.clone());
    };
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(*key).and_then(|v| traverse(v, rest)) {
                return Some(found);
            }
            map.values().find_map(|v| traverse(v, keys))
        }
        Value::Array(items) => items.iter().find_map(|v| traverse(v, keys)),
        _ => None,
    }
}

/// The page's item JSON, its final URL and the session cookies.
async fn fetch_metadata(id: &str, page: &str) -> Result<(Value, String, CookieJar), String> {
    if let Some((item, url, jar, at)) = CACHE.lock().unwrap().get(id) {
        if at.elapsed() < Duration::from_secs(300) {
            return Ok((item.clone(), url.clone(), jar.clone()));
        }
    }

    let mut jar = CookieJar::from_netscape_file(&http::cookie_file(), "tiktok.com");
    let mut page_url = page.to_string();
    let mut json = None;
    for attempt in 1..=10u64 {
        let mut request = http::no_redirect_client()
            .get(&page_url)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.9");
        if !jar.is_empty() {
            request = request.header("Cookie", jar.header());
        }
        let response = match request.send().await {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        jar.absorb(&response);
        if response.status().is_redirection() {
            if let Some(next) = response.headers().get("location").and_then(|l| l.to_str().ok()) {
                page_url = next.to_string();
                continue;
            }
        }
        let html = response.text().await.unwrap_or_default();

        if html.contains("id=\"cs\"") || html.contains("id=\"wci\"") {
            if let Err(e) = solve_challenge(&html, &mut jar) {
                log::warn!("TikTok WAF challenge failed: {e}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        if let Some(found) = UNIVERSAL_DATA_RE.captures(&html).and_then(|c| c.get(1)) {
            json = Some(found.as_str().to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
    }

    let json = json.ok_or("TikTok's page had no video data (deleted, or a captcha)")?;
    let data: Value = serde_json::from_str(&json).map_err(|_| "Couldn't read TikTok's page data")?;
    let scope = traverse(&data, &["__DEFAULT_SCOPE__"]).ok_or("TikTok's page data has changed")?;
    let item = traverse(&scope, &["itemStruct"]).ok_or("TikTok's page data has no video")?;

    CACHE
        .lock()
        .unwrap()
        .insert(id.to_string(), (item.clone(), page_url.clone(), jar.clone(), Instant::now()));
    Ok((item, page_url, jar))
}

/// TikTok's WAF challenge: find the number whose SHA-256 (after the given
/// prefix) matches, and hand it back as a cookie.
fn solve_challenge(html: &str, jar: &mut CookieJar) -> Result<(), String> {
    let class_of = |id: &str| {
        Regex::new(&format!(r#"<p[^>]*id="{id}"[^>]*class="([^"]+)""#))
            .ok()?
            .captures(html)?
            .get(1)
            .map(|m| m.as_str().to_string())
    };
    let mut data = class_of("cs").ok_or("no challenge data")?;
    while data.len() % 4 != 0 {
        data.push('=');
    }
    let decoded = BASE64.decode(data.as_bytes()).map_err(|e| e.to_string())?;
    let mut challenge: Value = serde_json::from_slice(&decoded).map_err(|e| e.to_string())?;
    let v = challenge.get("v").ok_or("challenge without 'v'")?;
    let expected = BASE64.decode(v["c"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
    let prefix = BASE64.decode(v["a"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;

    let answer = (0..=1_000_000u32)
        .map(|i| i.to_string().into_bytes())
        .find(|n| {
            let mut hasher = Sha256::new();
            hasher.update(&prefix);
            hasher.update(n);
            hasher.finalize().as_slice() == expected.as_slice()
        })
        .ok_or("no answer found")?;
    challenge["d"] = Value::String(BASE64.encode(&answer));

    let solution = serde_json::to_string(&challenge).map_err(|e| e.to_string())?.replace(' ', "");
    jar.set(&class_of("wci").unwrap_or_else(|| "_wafchallengeid".to_string()), &BASE64.encode(solution));
    if let (Some(name), Some(value)) = (class_of("rci"), class_of("rs")) {
        jar.set(&name, &value);
    }
    Ok(())
}

/// The post's media: every image of a photo post, else the video.
fn media_urls(item: &Value) -> Vec<(String, MediaKind)> {
    if let Some(images) = item.pointer("/imagePost/images").and_then(|i| i.as_array()) {
        return images
            .iter()
            .filter_map(|img| img.pointer("/imageURL/urlList")?.as_array()?.last()?.as_str())
            .map(|u| (u.to_string(), MediaKind::Image))
            .collect();
    }
    item.pointer("/video/PlayAddrStruct/UrlList")
        .and_then(|l| l.as_array())
        .and_then(|l| l.last())
        .and_then(|u| u.as_str())
        .map(|u| vec![(u.to_string(), MediaKind::Video)])
        .unwrap_or_default()
}

/// Headers TikTok's CDN wants: the page as referer, and its cookies.
fn download_headers(page: &str, jar: &CookieJar) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Referer".to_string(), page.to_string()),
        ("Origin".to_string(), "https://www.tiktok.com".to_string()),
    ];
    if !jar.is_empty() {
        headers.push(("Cookie".to_string(), jar.header()));
    }
    headers
}

pub async fn get_info(url: &str) -> Result<MediaInfo, String> {
    let (id, page) = resolve_id(url).await?;
    let (item, page_url, jar) = fetch_metadata(&id, &page).await?;
    let urls = media_urls(&item);
    if urls.is_empty() {
        return Err("No playable media on this TikTok".to_string());
    }
    let caption = item.get("desc").and_then(|d| d.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let cover = item
        .pointer("/video/originCover")
        .or_else(|| item.pointer("/video/cover"))
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    let duration = item.pointer("/video/duration").and_then(|d| d.as_f64());
    let headers = download_headers(&page_url, &jar);

    let mut items = Vec::new();
    for (index, (media_url, kind)) in urls.into_iter().enumerate() {
        let size = http::content_length(&media_url, &headers).await.unwrap_or(0);
        items.push(MediaItem {
            index,
            kind,
            ext: if kind == MediaKind::Video { "mp4" } else { "jpg" }.to_string(),
            thumbnail: if kind == MediaKind::Video { cover.clone() } else { media_url.clone() },
            qualities: vec![Quality { key: "best".to_string(), size }],
            caption: caption.clone(),
            duration,
        });
    }
    Ok(MediaInfo { service: Service::TikTok, id, items, info_json: None })
}

pub async fn resolve(url: &str, request: &MediaRequest) -> Result<Vec<Fetch>, String> {
    let (id, page) = resolve_id(url).await?;
    let (item, page_url, jar) = fetch_metadata(&id, &page).await?;
    let caption = item.get("desc").and_then(|d| d.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let headers = download_headers(&page_url, &jar);
    Ok(media_urls(&item)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| super::wanted(request, *i))
        .map(|(index, (url, kind))| Fetch {
            index,
            url,
            headers: headers.clone(),
            kind,
            ext: if kind == MediaKind::Video { "mp4" } else { "jpg" }.to_string(),
            hls: false,
            caption: caption.clone(),
        })
        .collect())
}
