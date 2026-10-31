//! TikTok videos and photo posts, adapted from urlsula's tiktok_dl: resolve
//! short links, read the page's rehydration JSON (solving TikTok's WAF
//! challenge when it shows one), and fetch the media with the page's cookies.
//! A video's "best" is what tiktok_dl picks (an "original" variant if there
//! is one, else the highest bitrate); its other sizes are offered as other
//! qualities. Video-only (DASH) variants have their audio track joined in
//! with ffmpeg, and are left out without it.
//! A photo post's slideshow comes as a video from tikmate.io, which also
//! stands in when TikTok's page can't be read (see tikmate.rs).

use super::http::{self, CookieJar};
use super::{Fetch, MediaInfo, MediaItem, MediaKind, MediaRequest, Quality, Service, tikmate};
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

/// One way a video can be fetched: a quality key ("best", or "w:h"), its
/// link, the audio track to join in (a video-only variant's), and its size
/// when the page says.
#[derive(Clone, Debug)]
struct Choice {
    key: String,
    url: String,
    audio: Option<String>,
    size: u64,
}

/// One of the sizes and codecs TikTok serves a video in (`bitrateInfo`).
struct Variant {
    width: u64,
    height: u64,
    bitrate: u64,
    original: bool,
    /// Video only: its sound is a track apart.
    dash: bool,
    url: String,
    size: u64,
}

/// A number the page gives as a number or as a string.
fn number(value: Option<&Value>) -> u64 {
    value.and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())).unwrap_or(0)
}

/// A PlayAddr's first link straight to the CDN (the /aweme/v1/play/ ones
/// redirect), else its first.
fn direct_url(play: &Value) -> Option<String> {
    let urls: Vec<&str> = play.get("UrlList")?.as_array()?.iter().filter_map(Value::as_str).collect();
    urls.iter().find(|u| !u.contains("/aweme/v1/play/")).or(urls.first()).map(|u| u.to_string())
}

fn variants(video: &Value) -> Vec<Variant> {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_lowercase();
    video
        .get("bitrateInfo")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let play = v.get("PlayAddr")?;
            Some(Variant {
                width: number(play.get("Width")),
                height: number(play.get("Height")),
                bitrate: number(v.get("Bitrate")),
                original: text(v, "GearName").contains("original"),
                dash: text(v, "Format") == "dash",
                url: direct_url(play)?,
                size: number(play.get("DataSize")),
            })
        })
        .collect()
}

/// The highest-bitrate audio-only track, for video-only variants.
fn best_audio(video: &Value) -> Option<String> {
    let best = video
        .get("bitrateAudioInfo")?
        .as_array()?
        .iter()
        .filter(|a| a.get("UrlList").is_some())
        .max_by_key(|a| number(a.get("Bitrate")))?;
    let urls = best.get("UrlList")?;
    [urls.get("MainUrl"), urls.get("BackupUrl")].into_iter().flatten().find_map(|u| {
        u.as_str().map(str::to_string).or_else(|| u.as_array()?.first()?.as_str().map(str::to_string))
    })
}

/// A video's qualities, best first: tiktok_dl's pick as "best", then each
/// other size at its highest bitrate. Video-only variants need the audio
/// track and ffmpeg (`can_join`) to have sound, or are left out.
fn video_choices(video: &Value, can_join: bool) -> Vec<Choice> {
    let audio = if can_join { best_audio(video) } else { None };
    let pool: Vec<Variant> = variants(video).into_iter().filter(|v| !v.dash || audio.is_some()).collect();
    let with_audio = |v: &Variant| if v.dash { audio.clone() } else { None };
    let originals: Vec<&Variant> = pool.iter().filter(|v| v.original).collect();
    let best = if originals.is_empty() { pool.iter().max_by_key(|v| v.bitrate) } else { originals.into_iter().max_by_key(|v| v.bitrate) };
    let Some(best) = best else {
        // No variant to pick from: the page's one playable link.
        let play = video.get("PlayAddrStruct").and_then(direct_url).or_else(|| video.get("playAddr")?.as_str().map(str::to_string));
        return play.map(|url| vec![Choice { key: "best".to_string(), url, audio: None, size: 0 }]).unwrap_or_default();
    };
    let mut others: Vec<&Variant> = Vec::new();
    for v in &pool {
        if v.width == 0 || v.height == 0 || (v.width, v.height) == (best.width, best.height) {
            continue;
        }
        match others.iter_mut().find(|o| (o.width, o.height) == (v.width, v.height)) {
            Some(o) if o.bitrate < v.bitrate => *o = v,
            Some(_) => {}
            None => others.push(v),
        }
    }
    others.sort_by_key(|v| std::cmp::Reverse(v.width * v.height));
    std::iter::once(Choice { key: "best".to_string(), url: best.url.clone(), audio: with_audio(best), size: best.size })
        .chain(others.into_iter().map(|v| Choice { key: format!("{}:{}", v.width, v.height), url: v.url.clone(), audio: with_audio(v), size: v.size }))
        .collect()
}

/// Whether ffmpeg is there to join a video-only variant with its sound.
fn can_join() -> bool {
    crate::platform::ffmpeg_command().is_some()
}

/// The post's media: every image of a photo post, else the video in each
/// of its qualities.
fn media(item: &Value, can_join: bool) -> Vec<(Vec<Choice>, MediaKind)> {
    if let Some(images) = item.pointer("/imagePost/images").and_then(|i| i.as_array()) {
        return images
            .iter()
            .filter_map(|img| img.pointer("/imageURL/urlList")?.as_array()?.last()?.as_str())
            .map(|u| (vec![Choice { key: "best".to_string(), url: u.to_string(), audio: None, size: 0 }], MediaKind::Image))
            .collect();
    }
    item.get("video")
        .map(|video| video_choices(video, can_join))
        .filter(|choices| !choices.is_empty())
        .map(|choices| vec![(choices, MediaKind::Video)])
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

/// A post's media. A photo post also offers its slideshow as a video
/// (from tikmate.io); and when TikTok's page can't be read, tikmate's answer
/// is the post.
pub async fn get_info(url: &str) -> Result<MediaInfo, String> {
    match native_info(url).await {
        Ok(mut info) => {
            if is_photo_post(&info.items) {
                match tikmate::fetch(url).await {
                    Ok(post) => {
                        if let Some(video) = post.video {
                            let size = http::content_length(&video, &tikmate::headers()).await.unwrap_or(0);
                            let caption = info.items.first().and_then(|i| i.caption.clone());
                            let thumbnail = info.items.first().map(|i| i.thumbnail.clone()).unwrap_or_default();
                            info.items.push(slideshow_item(info.items.len(), size, thumbnail, caption));
                        }
                    }
                    Err(e) => log::warn!("no slideshow video for {url}: {e}"),
                }
            }
            Ok(info)
        }
        Err(e) => {
            log::warn!("TikTok's page failed for {url} ({e}); trying tikmate");
            tikmate_info(url).await.map_err(|t| format!("{e} (and {t})"))
        }
    }
}

fn is_photo_post(items: &[MediaItem]) -> bool {
    !items.is_empty() && items.iter().all(|i| i.kind == MediaKind::Image)
}

/// A photo post's slideshow, as its last item.
fn slideshow_item(index: usize, size: u64, thumbnail: String, caption: Option<String>) -> MediaItem {
    MediaItem {
        square_cover: false,
        index,
        kind: MediaKind::Video,
        ext: "mp4".to_string(),
        thumbnail,
        qualities: vec![Quality { key: "best".to_string(), size }],
        caption,
        duration: None,
    }
}

/// The post from tikmate alone: its images, then its video (a photo post's
/// slideshow, or the video itself).
async fn tikmate_info(url: &str) -> Result<MediaInfo, String> {
    let post = tikmate::fetch(url).await?;
    let headers = tikmate::headers();
    let mut items = Vec::new();
    for (image, preview) in &post.images {
        let size = http::content_length(image, &headers).await.unwrap_or(0);
        items.push(MediaItem {
            square_cover: false,
            index: items.len(),
            kind: MediaKind::Image,
            ext: "jpg".to_string(),
            thumbnail: preview.clone(),
            qualities: vec![Quality { key: "best".to_string(), size }],
            caption: post.caption.clone(),
            duration: None,
        });
    }
    if let Some(video) = &post.video {
        let size = http::content_length(video, &headers).await.unwrap_or(0);
        let thumbnail = post.thumbnail.clone().or_else(|| post.images.first().map(|(_, p)| p.clone())).unwrap_or_default();
        items.push(slideshow_item(items.len(), size, thumbnail, post.caption.clone()));
    }
    let id = FULL_RE.captures(url).map(|c| c[5].to_string()).unwrap_or_else(|| "tiktok".to_string());
    Ok(MediaInfo { service: Service::TikTok, id, title: None, items, info_json: None })
}

/// The post from TikTok's own page.
async fn native_info(url: &str) -> Result<MediaInfo, String> {
    let (id, page) = resolve_id(url).await?;
    let (item, page_url, jar) = fetch_metadata(&id, &page).await?;
    let media = media(&item, can_join());
    if media.is_empty() {
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
    for (index, (choices, kind)) in media.into_iter().enumerate() {
        let mut qualities = Vec::new();
        for choice in &choices {
            let size = if choice.size > 0 { choice.size } else { http::content_length(&choice.url, &headers).await.unwrap_or(0) };
            qualities.push(Quality { key: choice.key.clone(), size });
        }
        items.push(MediaItem {
            square_cover: false,
            index,
            kind,
            ext: if kind == MediaKind::Video { "mp4" } else { "jpg" }.to_string(),
            thumbnail: if kind == MediaKind::Video { cover.clone() } else { choices[0].url.clone() },
            qualities,
            caption: caption.clone(),
            duration,
        });
    }
    Ok(MediaInfo { service: Service::TikTok, id, title: None, items, info_json: None })
}

/// Fresh links for the items asked for, the same way get_info found them:
/// TikTok's page, with tikmate for a photo post's slideshow (the item after
/// its images), or tikmate for all of it when the page fails.
pub async fn resolve(url: &str, request: &MediaRequest) -> Result<Vec<Fetch>, String> {
    let native = async {
        let (id, page) = resolve_id(url).await?;
        let (item, page_url, jar) = fetch_metadata(&id, &page).await?;
        Ok::<_, String>((item, page_url, jar))
    }
    .await;
    let (item, page_url, jar) = match native {
        Ok(found) => found,
        Err(e) => {
            log::warn!("TikTok's page failed for {url} ({e}); trying tikmate");
            return tikmate_fetches(url, request).await;
        }
    };
    let caption = item.get("desc").and_then(|d| d.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let headers = download_headers(&page_url, &jar);
    let media = media(&item, can_join());
    let photo_post = !media.is_empty() && media.iter().all(|(_, k)| *k == MediaKind::Image);
    let count = media.len();
    let mut fetches: Vec<Fetch> = media
        .into_iter()
        .enumerate()
        .filter(|(i, _)| super::wanted(request, *i))
        .filter_map(|(index, (choices, kind))| {
            // The item's own quality, else the one asked for all; the best
            // when this video doesn't come in it.
            let key = request.item_qualities.iter().find(|(i, _)| *i == index).map_or(request.quality.as_str(), |(_, q)| q.as_str());
            let choice = choices.iter().find(|c| c.key == key).or(choices.first())?.clone();
            Some(Fetch {
                index,
                url: choice.url,
                audio: choice.audio,
                headers: headers.clone(),
                kind,
                ext: if kind == MediaKind::Video { "mp4" } else { "jpg" }.to_string(),
                hls: false,
                caption: caption.clone(),
            })
        })
        .collect();
    if photo_post && super::wanted(request, count) {
        match tikmate::fetch(url).await {
            Ok(tikmate::Post { video: Some(video), .. }) => fetches.push(Fetch {
                index: count,
                url: video,
                audio: None,
                headers: tikmate::headers(),
                kind: MediaKind::Video,
                ext: "mp4".to_string(),
                hls: false,
                caption: caption.clone(),
            }),
            Ok(_) => log::warn!("tikmate has no slideshow video for {url}"),
            Err(e) => log::warn!("no slideshow video for {url}: {e}"),
        }
    }
    Ok(fetches)
}

/// The items asked for, from tikmate alone (images, then the video).
async fn tikmate_fetches(url: &str, request: &MediaRequest) -> Result<Vec<Fetch>, String> {
    let post = tikmate::fetch(url).await?;
    let media = post
        .images
        .iter()
        .map(|(image, _)| (image.clone(), MediaKind::Image))
        .chain(post.video.clone().map(|v| (v, MediaKind::Video)));
    Ok(media
        .enumerate()
        .filter(|(i, _)| super::wanted(request, *i))
        .map(|(index, (url, kind))| Fetch {
            index,
            url,
            audio: None,
            headers: tikmate::headers(),
            kind,
            ext: if kind == MediaKind::Video { "mp4" } else { "jpg" }.to_string(),
            hls: false,
            caption: post.caption.clone(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn variant(gear: &str, format: &str, bitrate: u64, w: u64, h: u64) -> Value {
        json!({
            "GearName": gear, "Format": format, "Bitrate": bitrate,
            "PlayAddr": { "Width": w, "Height": h, "DataSize": format!("{bitrate}"),
                "UrlList": [format!("https://www.tiktok.com/aweme/v1/play/?{gear}"), format!("https://cdn/{gear}")] }
        })
    }

    #[test]
    fn best_is_tiktok_dls_pick() {
        let video = json!({
            "bitrateInfo": [
                variant("normal_1080_0", "mp4", 3000, 1080, 1920),
                variant("adapt_lowest_720_1", "mp4", 900, 720, 1280),
                variant("normal_720_0", "mp4", 1500, 720, 1280),
                variant("original_dash", "dash", 2000, 1080, 1920),
                variant("lower_540_0", "mp4", 600, 540, 960),
            ],
            "bitrateAudioInfo": [
                { "Bitrate": 64, "UrlList": { "MainUrl": "https://cdn/audio64" } },
                { "Bitrate": 128, "UrlList": { "MainUrl": "https://cdn/audio128" } },
            ],
        });
        // An "original" wins over a higher bitrate; it's video only, so its
        // sound comes with it.
        let choices = video_choices(&video, true);
        assert_eq!(choices[0].key, "best");
        assert_eq!(choices[0].url, "https://cdn/original_dash");
        assert_eq!(choices[0].audio.as_deref(), Some("https://cdn/audio128"));
        let keys: Vec<&str> = choices.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["best", "720:1280", "540:960"]);
        assert_eq!(choices[1].url, "https://cdn/normal_720_0");
        assert_eq!(choices[1].size, 1500);

        // Without ffmpeg the video-only one is left out.
        let choices = video_choices(&video, false);
        assert_eq!(choices[0].url, "https://cdn/normal_1080_0");
        assert_eq!(choices[0].audio, None);
    }

    #[test]
    fn no_variants() {
        let video = json!({ "PlayAddrStruct": { "UrlList": ["https://cdn/only"] } });
        let choices = video_choices(&video, true);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].url, "https://cdn/only");
    }
}
