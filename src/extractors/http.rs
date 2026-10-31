//! HTTP plumbing shared by the extractors: browser-like clients, a small
//! cookie jar (reqwest's cookie support would pull in crates eddy doesn't
//! otherwise need), Netscape cookie files, and size checks.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

pub const UA_DESKTOP: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";

fn build(redirects: bool) -> reqwest::Client {
    aria2_core::http::client_pool::ensure_rustls_provider();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(UA_DESKTOP)
        // As browsers write them on HTTP/1.1; Cloudflare (Royal Road)
        // turns lowercase header names away.
        .http1_title_case_headers()
        .redirect(if redirects { reqwest::redirect::Policy::limited(10) } else { reqwest::redirect::Policy::none() })
        .build()
        .expect("failed to build the HTTP client")
}

/// Follows redirects.
pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| build(true))
}

/// Doesn't follow redirects, for walking them by hand (short links, login
/// walls, cookies set along the way).
pub fn no_redirect_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| build(false))
}

/// Name → value cookies for one site, sent as a single Cookie header.
#[derive(Clone, Debug, Default)]
pub struct CookieJar(BTreeMap<String, String>);

impl CookieJar {
    pub fn set(&mut self, name: &str, value: &str) {
        self.0.insert(name.to_string(), value.to_string());
    }

    /// Takes every Set-Cookie of a response.
    pub fn absorb(&mut self, response: &reqwest::Response) {
        for header in response.headers().get_all("set-cookie") {
            if let Some((name, value)) = header
                .to_str()
                .ok()
                .and_then(|h| h.split(';').next())
                .and_then(|pair| pair.split_once('='))
            {
                self.set(name.trim(), value.trim());
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// The cookies' names (for logs: never their values).
    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    /// Adds `other`'s cookies, replacing ones of the same name.
    pub fn merge(&mut self, other: &CookieJar) {
        for (name, value) in &other.0 {
            self.set(name, value);
        }
    }

    /// The Cookie header value.
    pub fn header(&self) -> String {
        self.0.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    /// Cookies for `domain` from a Netscape cookies.txt (the format browser
    /// extensions export), if the user put one at `path`.
    pub fn from_netscape_file(path: &std::path::Path, domain: &str) -> Self {
        let mut jar = CookieJar::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return jar;
        };
        for line in text.lines() {
            let line = line.trim();
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 7 && parts[0].trim_start_matches('.').ends_with(domain) {
                jar.set(parts[5], parts[6]);
            }
        }
        jar
    }
}

/// Where a user's exported cookies.txt is looked for (in the app's data
/// folder), for sites that need a login.
pub fn cookie_file() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::platform::get_app_data_dir()).join("cookies.txt")
}

/// An application/x-www-form-urlencoded body.
pub fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

/// The site a host belongs to, roughly: its last two labels
/// ("v16-webapp-prime.tiktok.com" → "tiktok.com").
fn site_of(host: &str) -> String {
    let labels: Vec<&str> = host.rsplitn(3, '.').collect();
    labels.iter().take(2).rev().copied().collect::<Vec<_>>().join(".")
}

/// GETs `url` with `headers`, following redirects by hand so the headers
/// (cookies especially) go along while the redirect stays on the same site:
/// reqwest drops Cookie on any change of host, and TikTok's media links
/// redirect to CDN hosts that need them.
pub async fn get_following(url: &str, headers: &[(String, String)], range: Option<&str>) -> Result<reqwest::Response, String> {
    let original_site = url::Url::parse(url).ok().and_then(|u| u.host_str().map(site_of)).unwrap_or_default();
    let mut current = url.to_string();
    for _ in 0..10 {
        let same_site = url::Url::parse(&current)
            .ok()
            .and_then(|u| u.host_str().map(site_of))
            .is_some_and(|site| site == original_site);
        let mut request = no_redirect_client().get(&current);
        for (k, v) in headers {
            if same_site || !k.eq_ignore_ascii_case("cookie") {
                request = request.header(k.as_str(), v.as_str());
            }
        }
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let Some(location) = response.headers().get("location").and_then(|l| l.to_str().ok()) else {
            return Ok(response);
        };
        current = url::Url::parse(&current)
            .and_then(|base| base.join(location))
            .map(|u| u.to_string())
            .map_err(|e| e.to_string())?;
    }
    Err("Too many redirects".to_string())
}

/// A file's size from a one-byte range request ("bytes 0-0/1234"), else its
/// Content-Length; None when the server won't say.
pub async fn content_length(url: &str, headers: &[(String, String)]) -> Option<u64> {
    if url.contains(".m3u8") {
        return None; // a playlist, not the stream
    }
    let response = get_following(url, headers, Some("bytes=0-0")).await.ok()?;
    let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Some(total) = header("content-range").and_then(|r| r.rsplit('/').next()?.parse().ok()) {
        return Some(total);
    }
    if response.status().is_success() {
        return header("content-length").and_then(|v| v.parse().ok());
    }
    None
}
