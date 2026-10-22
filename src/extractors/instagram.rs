//! Instagram posts, reels, carousels and stories, adapted from urlsula's
//! instagram_dl. Three ways in, tried in order: the web GraphQL post query
//! (with the page's tokens and an anonymous session), the embed page's JSON,
//! and SnapSave. Stories only work through SnapSave. Share links are
//! resolved first. With the user's cookies.txt (Settings → Downloads), the
//! GraphQL query goes as their logged-in session.

use super::http::{self, CookieJar};
use super::{Fetch, MediaInfo, MediaItem, MediaKind, MediaRequest, Quality, Service};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use rand::seq::SliceRandom;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GRAPHQL_ENDPOINT: &str = "https://www.instagram.com/graphql/query/";
const POLARIS_ACTION: &str = "PolarisPostRootQuery";
const DOC_ID: &str = "28077897148546091";
const APP_ID: &str = "936619743392459";
const BLOKS_VERSION_ID: &str = "6309c8d03d8a3f47a1658ba38b304a3f837142ef5f637ebf1f8f52d4b802951e";
const ASBD_ID: &str = "129477";
const HIDDEN_STATE: &str = "20126.HYP:instagram_web_pkg.2.1...0";
const SESSION_INTERNAL: &str = "7436540909012459023";
const ROLLOUT_HASH: &str = "1019933358";
const UA_BROWSER: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36";
const UA_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

static EMBED_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r#"(?s)"init",\[\],\[(.*?)\]\],"#).unwrap(),
        Regex::new(r"(?s)new ServerJS\(\)\);s\.handle\((\{.*?\})\);requireLazy").unwrap(),
    ]
});
static SHORTCODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"instagram\.com/(?:[^/]+/)?(?:p|reel|reels|tv)/([A-Za-z0-9_-]+)").unwrap());
static STORY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"instagram\.com/stories/([^/]+)/(\d+)").unwrap());
static SHARE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"instagram\.com/share(?:/(?:reels?|video|s|p))?/(?P<id>[^/?]+)").unwrap());

#[derive(Debug, Clone)]
struct RawMedia {
    caption: String,
    kind: MediaKind,
    source_url: String,
    thumbnail_url: String,
}

/// Media per resolved URL for 5 minutes (the download right after the
/// preview reuses it).
static CACHE: LazyLock<Mutex<HashMap<String, (Vec<RawMedia>, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn can_handle(url: &str) -> bool {
    SHORTCODE_RE.is_match(url) || STORY_RE.is_match(url) || SHARE_RE.is_match(url)
}

async fn raw_media(url: &str) -> Result<(String, Vec<RawMedia>), String> {
    let resolved = if SHARE_RE.is_match(url) {
        let response = http::client().get(url).headers(web_headers()).send().await.map_err(|e| e.to_string())?;
        response.url().to_string()
    } else {
        url.to_string()
    };
    if let Some((media, at)) = CACHE.lock().unwrap().get(&resolved) {
        if at.elapsed() < Duration::from_secs(300) {
            return Ok((resolved.clone(), media.clone()));
        }
    }

    let (id, media) = if let Some(story) = STORY_RE.captures(&resolved) {
        let media = try_snapsave(&resolved).await?;
        (story[2].to_string(), media.into_iter().take(1).collect())
    } else {
        let shortcode = SHORTCODE_RE
            .captures(&resolved)
            .map(|c| c[1].to_string())
            .ok_or_else(|| format!("Not a supported Instagram link: {url}"))?;
        (shortcode.clone(), fetch_raw_media(&resolved, &shortcode).await?)
    };
    if media.is_empty() {
        return Err("No media found on this post".to_string());
    }
    CACHE.lock().unwrap().insert(resolved.clone(), (media.clone(), Instant::now()));
    Ok((id, media))
}

pub async fn get_info(url: &str) -> Result<MediaInfo, String> {
    let (id, media) = raw_media(url).await?;
    let mut items = Vec::new();
    for (index, m) in media.into_iter().enumerate() {
        let size = fetch_size(&m.source_url).await;
        items.push(MediaItem {
            index,
            kind: m.kind,
            ext: if m.kind == MediaKind::Video { "mp4" } else { "jpg" }.to_string(),
            thumbnail: m.thumbnail_url,
            qualities: vec![Quality { key: "default".to_string(), size }],
            caption: (!m.caption.is_empty()).then_some(m.caption),
            duration: None,
        });
    }
    Ok(MediaInfo { service: Service::Instagram, id, items, info_json: None })
}

pub async fn resolve(url: &str, request: &MediaRequest) -> Result<Vec<Fetch>, String> {
    let (_, media) = raw_media(url).await?;
    Ok(media
        .into_iter()
        .enumerate()
        .filter(|(i, _)| super::wanted(request, *i))
        .map(|(index, m)| Fetch {
            index,
            url: m.source_url,
            headers: Vec::new(),
            ext: if m.kind == MediaKind::Video { "mp4" } else { "jpg" }.to_string(),
            kind: m.kind,
            hls: false,
            caption: (!m.caption.is_empty()).then_some(m.caption),
        })
        .collect())
}

async fn fetch_raw_media(url: &str, shortcode: &str) -> Result<Vec<RawMedia>, String> {
    match get_gql_media(shortcode).await {
        Ok(media) => {
            let items = parse_new_media(&media);
            if !items.is_empty() {
                return Ok(items);
            }
            log::warn!("[1/3] Instagram GraphQL gave no items for {shortcode}");
        }
        Err(e) => log::warn!("[1/3] Instagram GraphQL failed for {shortcode}: {e}"),
    }
    match get_embed_media(shortcode).await {
        Ok(media) => {
            let items = parse_gql_media(&media);
            if !items.is_empty() {
                return Ok(items);
            }
            log::warn!("[2/3] Instagram embed gave no items for {shortcode}");
        }
        Err(e) => log::warn!("[2/3] Instagram embed failed for {shortcode}: {e}"),
    }
    match try_snapsave(url).await {
        Ok(items) if !items.is_empty() => Ok(items),
        Ok(_) => Err("Every way of reading this Instagram post came back empty".to_string()),
        Err(e) => Err(format!("Every way of reading this Instagram post failed ({e})")),
    }
}

// --- SnapSave ---

/// The fallback when Deno isn't there (or fails): SnapSave hides its answer
/// in an eval() of a packed string: groups of digits in base `e`, written
/// with the alphabet `n` and split by `n[e]`, each a character code plus
/// `t`; the characters are UTF-8 bytes.
fn unpack_snapsave(raw_js: &str) -> Option<String> {
    static PACKED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\}\("([^"]*)",\s*\d+,\s*"([^"]*)",\s*(\d+),\s*(\d+),\s*\d+\)\)"#).unwrap()
    });
    let caps = PACKED.captures(raw_js)?;
    let (packed, alphabet) = (&caps[1], caps[2].chars().collect::<Vec<char>>());
    let offset: u32 = caps[3].parse().ok()?;
    let base: u32 = caps[4].parse().ok()?;
    let separator = *alphabet.get(base as usize)?;
    let mut bytes = Vec::new();
    for group in packed.split(separator).filter(|g| !g.is_empty()) {
        let mut value: u32 = 0;
        for c in group.chars() {
            let digit = alphabet.iter().position(|&a| a == c)? as u32;
            value = value.checked_mul(base)?.checked_add(digit)?;
        }
        bytes.push(u8::try_from(value.checked_sub(offset)?).ok()?);
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// SnapSave's real answer, by running its script through Deno (on Android,
/// YTDLnis's Deno helper app) with eval() caught, so whatever SnapSave
/// changes in its packing still works. None without Deno, or if it failed.
async fn deobfuscate_with_deno(raw_js: &str) -> Option<String> {
    let script = format!(
        r#"
    const raw = {};
    const fakeEvalHolder = {{ val: '' }};
    const patched = raw.replace(/\beval\s*\(/, 'fakeEvalHolder.val=(');
    try {{
        const fn = new Function('fakeEvalHolder', patched);
        fn(fakeEvalHolder);
    }} catch (e) {{}}
    const output = fakeEvalHolder.val;
    Deno.stdout.writeSync(new TextEncoder().encode(output));
    "#,
        serde_json::to_string(raw_js).unwrap_or_default()
    );
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut child = crate::platform::deno_command()?
            .args(["run", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(script.as_bytes());
        }
        let out = child.wait_with_output().ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        (out.status.success() && !text.is_empty()).then_some(text)
    })
    .await
    .ok()
    .flatten()
}

fn parse_download_html(html: &str) -> Vec<RawMedia> {
    let unescape = |u: &str| u.replace("&amp;", "&").replace(r"\u0026", "&");
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let video_re = Regex::new(r#"href=\\?["'](https?://[^"'\\]+/v2\?token=[^"'\\]+|[^"'\\]+\.mp4[^"'\\]*)"#).unwrap();
    let links: Vec<String> = video_re
        .captures_iter(html)
        .map(|c| unescape(&c[1]))
        .filter(|u| !u.is_empty() && seen.insert(u.clone()))
        .collect();
    let thumb_re = Regex::new(r#"src=\\?["'](https?://[^"'\\]+/thumb\?token=[^"'\\]+|[^"'\\]+\.jpg[^"'\\]*)"#).unwrap();
    let thumbs: Vec<String> = thumb_re.captures_iter(html).map(|c| unescape(&c[1])).filter(|u| !u.is_empty()).collect();

    if !links.is_empty() {
        for (i, url) in links.into_iter().enumerate() {
            results.push(RawMedia {
                caption: String::new(),
                kind: snapsave_kind(&url),
                thumbnail_url: thumbs.get(i).cloned().unwrap_or_else(|| url.clone()),
                source_url: url,
            });
        }
        return results;
    }
    for pattern in [
        Regex::new(r#"href=\\?["'](https?://[^"'\\]+\.jpg[^"'\\]*)"#).unwrap(),
        Regex::new(r#""url"\s*:\s*"(https?://[^"]+\.jpg[^"]*)""#).unwrap(),
    ] {
        for c in pattern.captures_iter(html) {
            let url = unescape(&c[1]);
            if !url.is_empty() && seen.insert(url.clone()) {
                results.push(RawMedia {
                    caption: String::new(),
                    kind: MediaKind::Image,
                    source_url: url.clone(),
                    thumbnail_url: url,
                });
            }
        }
    }
    results
}

/// Whether a SnapSave download link is a video or an image. Its /v2 links
/// carry a JWT whose payload names the file ("….jpg" for an image post).
fn snapsave_kind(url: &str) -> MediaKind {
    let filename = url
        .split_once("token=")
        .and_then(|(_, token)| token.split(['&', '#']).next())
        .and_then(|token| token.split('.').nth(1))
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok())
        .and_then(|json| serde_json::from_slice::<Value>(&json).ok())
        .and_then(|claims| claims.get("filename").and_then(|f| f.as_str()).map(str::to_lowercase));
    match filename {
        Some(name) if [".jpg", ".jpeg", ".png", ".webp", ".heic"].iter().any(|ext| name.ends_with(ext)) => MediaKind::Image,
        _ => MediaKind::Video,
    }
}

async fn try_snapsave(url: &str) -> Result<Vec<RawMedia>, String> {
    // The landing page hands out the session cookie the form needs.
    let mut jar = CookieJar::default();
    if let Ok(home) = http::client().get("https://snapsave.app/").header("User-Agent", UA_BROWSER).send().await {
        jar.absorb(&home);
    }
    let mut request = http::client()
        .post("https://snapsave.app/action.php")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Referer", "https://snapsave.app/")
        .header("Origin", "https://snapsave.app")
        .header("User-Agent", UA_BROWSER)
        .body(http::form(&[("url", url)]));
    if !jar.is_empty() {
        request = request.header("Cookie", jar.header());
    }
    let raw = request
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let decoded = if raw.contains("eval(") {
        match deobfuscate_with_deno(&raw).await {
            Some(html) => html,
            None => unpack_snapsave(&raw).unwrap_or(raw),
        }
    } else {
        raw
    };
    if decoded.contains("Unable to connect") || decoded.contains("error_api") {
        return Err("SnapSave: Instagram blocked the request".to_string());
    }
    let items = parse_download_html(&decoded);
    if items.is_empty() {
        return Err("SnapSave: no media in its answer".to_string());
    }
    Ok(items)
}

// --- Helpers ---

async fn fetch_size(url: &str) -> u64 {
    let Ok(response) = http::client().head(url).send().await else {
        return 0;
    };
    if !response.status().is_success() {
        return 0;
    }
    response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn random_base64(n_bytes: usize) -> String {
    let mut bytes = vec![0u8; n_bytes];
    rand::thread_rng().fill(&mut bytes[..]);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn random_alpha(n: usize) -> String {
    let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ".chars().collect();
    let mut rng = rand::thread_rng();
    (0..n).map(|_| *alphabet.choose(&mut rng).unwrap()).collect()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn web_headers() -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    let mut add = |k: &'static str, v: &'static str| {
        headers.insert(k, HeaderValue::from_static(v));
    };
    add("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7");
    add("Accept-Language", "en-GB,en;q=0.9");
    add("Cache-Control", "max-age=0");
    add("Dnt", "1");
    add("Priority", "u=0, i");
    add("Sec-Ch-Ua", r#"Chromium";v="124", "Google Chrome";v="124", "Not-A.Brand";v="99"#);
    add("Sec-Ch-Ua-Mobile", "?0");
    add("Sec-Ch-Ua-Platform", "macOS");
    add("Sec-Fetch-Dest", "document");
    add("Sec-Fetch-Mode", "navigate");
    add("Sec-Fetch-Site", "none");
    add("Sec-Fetch-User", "?1");
    add("Upgrade-Insecure-Requests", "1");
    add("User-Agent", UA_MAC);
    headers
}

fn number_from_query(name: &str, data: &str) -> Option<i64> {
    let re = Regex::new(&format!(r"{}=(\d+)", regex::escape(name))).ok()?;
    re.captures(data)?.get(1)?.as_str().parse().ok()
}

fn object_from_entries(name: &str, data: &str) -> Option<Value> {
    let re = Regex::new(&format!(r#"(?s)\["{}"\s*,\s*.*?\s*,\s*(\{{.*?\}}),\s*\d+\]"#, regex::escape(name))).ok()?;
    serde_json::from_str(re.captures(data)?.get(1)?.as_str()).ok()
}

/// The GraphQL request: tokens from the post page (or Instagram's usual
/// values when the page holds back), an anonymous session, and the query.
async fn gql_request(shortcode: &str) -> Result<(reqwest::header::HeaderMap, String), String> {
    use reqwest::header::HeaderValue;
    // The user's instagram.com cookies, if they added a cookies.txt: logged
    // in, the post isn't behind the login wall.
    let user_cookies = CookieJar::from_netscape_file(&http::cookie_file(), "instagram.com");
    let page = format!("https://www.instagram.com/p/{shortcode}/");
    let mut page_request = http::client().get(&page).headers(web_headers());
    if !user_cookies.is_empty() {
        page_request = page_request.header("Cookie", user_cookies.header());
    }
    let html = match page_request.send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        _ => String::new(),
    };

    let site_data = object_from_entries("SiteData", &html);
    let polaris = object_from_entries("PolarisSiteData", &html);
    let web_config = object_from_entries("DGWWebConfig", &html);
    let push_info = object_from_entries("InstagramWebPushInfo", &html);
    let security = object_from_entries("InstagramSecurityConfig", &html);
    let bloks = object_from_entries("WebBloksVersioningID", &html);
    let lsd_data = object_from_entries("LSD", &html);
    let field = |v: &Option<Value>, key: &str| v.as_ref().and_then(|v| v.get(key)).and_then(|v| v.as_str()).map(str::to_string);

    let lsd = field(&lsd_data, "token").unwrap_or_else(|| random_base64(8));
    // The CSRF header has to match the session's own csrftoken cookie.
    let csrf = user_cookies.get("csrftoken").map(str::to_string).or_else(|| field(&security, "csrf_token"));
    let app_id = field(&web_config, "appId").unwrap_or_else(|| APP_ID.to_string());
    let bloks_version = field(&bloks, "versioningID").unwrap_or_else(|| BLOKS_VERSION_ID.to_string());
    let rollout_hash = field(&push_info, "rollout_hash").unwrap_or_else(|| ROLLOUT_HASH.to_string());
    let haste_session = field(&site_data, "haste_session").unwrap_or_else(|| HIDDEN_STATE.to_string());
    let hsi = field(&site_data, "hsi").unwrap_or_else(|| SESSION_INTERNAL.to_string());
    let spin_r = field(&site_data, "__spin_r").unwrap_or_else(|| rollout_hash.clone());
    let spin_b = field(&site_data, "__spin_b").unwrap_or_else(|| "trunk".to_string());
    let spin_t = site_data
        .as_ref()
        .and_then(|v| v.get("__spin_t"))
        .and_then(|v| v.as_i64())
        .map(|v| v.to_string())
        .unwrap_or_else(|| now_secs().to_string());

    let mut cookie = CookieJar::default();
    if let Some(c) = &csrf {
        cookie.set("csrftoken", c);
    }
    if let Some(d) = field(&polaris, "device_id") {
        cookie.set("ig_did", &d);
    }
    cookie.set("wd", "1280x720");
    cookie.set("dpr", "2");
    if let Some(m) = field(&polaris, "machine_id") {
        cookie.set("mid", &m);
    }
    cookie.set("ig_nrcb", "1");
    cookie.merge(&user_cookies);

    let comet_req = number_from_query("__comet_req", &html).unwrap_or(7).to_string();
    let jazoest = number_from_query("jazoest", &html).unwrap_or_else(|| rand::thread_rng().gen_range(1..10001)).to_string();
    // The logged-in user's id when the cookies have one, else anonymous.
    let user_id = user_cookies.get("ds_user_id").unwrap_or("0").to_string();
    let session = format!("::{}", random_alpha(6));
    let dyn_value = random_base64(154);
    let csr_value = random_base64(154);

    let mut headers = web_headers();
    let value = |s: &str| HeaderValue::from_str(s).map_err(|e| e.to_string());
    headers.insert("x-ig-app-id", value(&app_id)?);
    headers.insert("X-FB-LSD", value(&lsd)?);
    headers.insert("X-CSRFToken", value(csrf.as_deref().unwrap_or(""))?);
    headers.insert("X-Bloks-Version-Id", value(&bloks_version)?);
    headers.insert("x-asbd-id", HeaderValue::from_static(ASBD_ID));
    headers.insert("cookie", value(&cookie.header())?);
    headers.insert("Content-Type", HeaderValue::from_static("application/x-www-form-urlencoded"));
    headers.insert("X-FB-Friendly-Name", HeaderValue::from_static(POLARIS_ACTION));

    let variables = json!({
        "shortcode": shortcode,
        "__relay_internal__pv__PolarisShortDramaEnabledrelayprovider": true,
        "__relay_internal__pv__PolarisMultiCaptionCarouselEnabledrelayprovider": true
    })
    .to_string();
    let body = http::form(&[
        ("__d", "www"),
        ("__a", "1"),
        ("__s", &session),
        ("__hs", &haste_session),
        ("__req", "b"),
        ("__ccg", "EXCELLENT"),
        ("__rev", &rollout_hash),
        ("__hsi", &hsi),
        ("__dyn", &dyn_value),
        ("__csr", &csr_value),
        ("__user", &user_id),
        ("__comet_req", &comet_req),
        ("av", &user_id),
        ("dpr", "2"),
        ("lsd", &lsd),
        ("jazoest", &jazoest),
        ("__spin_r", &spin_r),
        ("__spin_b", &spin_b),
        ("__spin_t", &spin_t),
        ("fb_api_caller_class", "RelayModern"),
        ("fb_api_req_friendly_name", POLARIS_ACTION),
        ("variables", &variables),
        ("server_timestamps", "true"),
        ("doc_id", DOC_ID),
    ]);
    Ok((headers, body))
}

async fn get_gql_media(shortcode: &str) -> Result<Value, String> {
    let (headers, body) = gql_request(shortcode).await?;
    let text = http::client()
        .post(GRAPHQL_ENDPOINT)
        .headers(headers)
        .body(body)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let data: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    data.pointer("/data/xdt_api__v1__media__shortcode__web_info/items")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .cloned()
        .ok_or_else(|| "no items in the GraphQL answer".to_string())
}

fn traverse<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => map.get(key).or_else(|| map.values().find_map(|v| traverse(v, key))),
        Value::Array(items) => items.iter().find_map(|v| traverse(v, key)),
        _ => None,
    }
}

async fn get_embed_media(shortcode: &str) -> Result<Value, String> {
    let url = format!("https://www.instagram.com/p/{shortcode}/embed/captioned");
    let html = http::client()
        .get(&url)
        .headers(web_headers())
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let raw = EMBED_PATTERNS
        .iter()
        .find_map(|p| p.captures(&html).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()))
        .ok_or("no embedded JSON on the embed page")?;
    let data: Value = serde_json::from_str(&raw)
        .or_else(|_| {
            // The blob sometimes has trailing commas.
            let cleaned = Regex::new(r",\s*([\}\]])").unwrap().replace_all(&raw, "$1").to_string();
            serde_json::from_str(&cleaned)
        })
        .map_err(|e| e.to_string())?;
    let context = traverse(&data, "contextJSON").ok_or("no contextJSON on the embed page")?;
    let context: Value = match context {
        Value::String(s) => serde_json::from_str(s).map_err(|e| e.to_string())?,
        other if other.is_object() => other.clone(),
        _ => return Err("unexpected contextJSON".to_string()),
    };
    let gql = context.get("gql_data").ok_or("no gql_data on the embed page")?;
    gql.get("shortcode_media")
        .or_else(|| gql.get("xdt_shortcode_media"))
        .cloned()
        .ok_or_else(|| "no media on the embed page".to_string())
}

/// The largest of a list of versions (by width × height).
fn largest<'a>(versions: &'a [Value]) -> Option<&'a Value> {
    versions.iter().max_by_key(|v| {
        v.get("width").and_then(|n| n.as_i64()).unwrap_or(0) * v.get("height").and_then(|n| n.as_i64()).unwrap_or(0)
    })
}

/// A GraphQL (API v1) item: a video or an image, at its largest.
fn parse_new_item(item: &Value, caption: &str) -> Option<RawMedia> {
    let thumb = item.pointer("/image_versions2/candidates/0/url").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if let Some(videos) = item.get("video_versions").and_then(|v| v.as_array()).filter(|v| !v.is_empty()) {
        let url = largest(videos)?.get("url")?.as_str()?;
        return Some(RawMedia { caption: caption.to_string(), kind: MediaKind::Video, source_url: url.to_string(), thumbnail_url: thumb });
    }
    let candidates = item.pointer("/image_versions2/candidates")?.as_array()?;
    let url = largest(candidates)?.get("url")?.as_str()?;
    Some(RawMedia { caption: caption.to_string(), kind: MediaKind::Image, source_url: url.to_string(), thumbnail_url: thumb })
}

fn parse_new_media(media: &Value) -> Vec<RawMedia> {
    let caption = media.pointer("/caption/text").and_then(|v| v.as_str()).unwrap_or("");
    match media.get("carousel_media").and_then(|v| v.as_array()) {
        Some(carousel) => carousel.iter().filter_map(|item| parse_new_item(item, caption)).collect(),
        None => parse_new_item(media, caption).into_iter().collect(),
    }
}

/// An embed-page (old GraphQL) node.
fn parse_gql_node(node: &Value, caption: &str) -> Option<RawMedia> {
    let display = node.get("display_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if let Some(video) = node.get("video_url").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        return Some(RawMedia { caption: caption.to_string(), kind: MediaKind::Video, source_url: video.to_string(), thumbnail_url: display });
    }
    (!display.is_empty()).then(|| RawMedia {
        caption: caption.to_string(),
        kind: MediaKind::Image,
        source_url: display.clone(),
        thumbnail_url: display,
    })
}

fn parse_gql_media(media: &Value) -> Vec<RawMedia> {
    let caption = media.pointer("/edge_media_to_caption/edges/0/node/text").and_then(|v| v.as_str()).unwrap_or("");
    match media.pointer("/edge_sidecar_to_children/edges").and_then(|e| e.as_array()) {
        Some(edges) => edges.iter().filter_map(|e| parse_gql_node(e.get("node")?, caption)).collect(),
        None => parse_gql_node(media, caption).into_iter().collect(),
    }
}
