//! Polite fetching for reading sites: browser headers (Royal Road's
//! Cloudflare turns bare requests away), the user's cookies for the site,
//! one request at a time with a pause in between, retries when the site
//! asks to slow down, and Cloudflare's challenge page recognised for what
//! it is.

use std::time::{Duration, Instant};

const USER_AGENT: &str =
    "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36";
/// Between two requests to reading sites.
const PAUSE: Duration = Duration::from_millis(1000);
const ATTEMPTS: u32 = 4;

/// When the last request went out (all reading sites share it: books are
/// saved one at a time anyway).
static LAST: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);

/// A page's HTML.
pub async fn text(url: &str) -> Result<String, String> {
    let domain = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_default();
    let cookies = crate::extractors::http::CookieJar::from_netscape_file(&crate::extractors::http::cookie_file(), &domain);

    let mut wait = Duration::from_secs(2);
    for attempt in 1..=ATTEMPTS {
        let response = {
            let mut last = LAST.lock().await;
            if let Some(at) = *last {
                let since = at.elapsed();
                if since < PAUSE {
                    tokio::time::sleep(PAUSE - since).await;
                }
            }
            let response = get(url, &cookies).await;
            *last = Some(Instant::now());
            response
        };
        let response = match response {
            Ok(r) => r,
            Err(e) if attempt < ATTEMPTS => {
                log::warn!("novel fetch {url} failed ({e}), retrying");
                tokio::time::sleep(wait).await;
                wait *= 2;
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let status = response.status();
        let body = response.text().await.map_err(|e| e.to_string())?;
        if is_challenge(&body) {
            return Err("This site only opens in a browser right now (Cloudflare's check)".to_string());
        }
        if (status.as_u16() == 429 || status.is_server_error()) && attempt < ATTEMPTS {
            // Slowing down, as asked.
            tokio::time::sleep(wait).await;
            wait *= 2;
            continue;
        }
        if !status.is_success() {
            return Err(format!("The site answered {status}"));
        }
        return Ok(body);
    }
    Err("The site kept refusing".to_string())
}

/// GETs `url` as a browser would, following redirects by hand so each hop
/// gets its own Host header. Cloudflare turns away requests whose Host
/// isn't the first header, and reqwest otherwise adds it last.
async fn get(url: &str, cookies: &crate::extractors::http::CookieJar) -> Result<reqwest::Response, reqwest::Error> {
    let mut url = url.to_string();
    for _ in 0..5 {
        let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        let mut request = crate::extractors::http::no_redirect_client()
            .get(&url)
            .header("Host", host)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.9");
        if !cookies.is_empty() {
            request = request.header("Cookie", cookies.header());
        }
        let response = request.send().await?;
        let next = response
            .status()
            .is_redirection()
            .then(|| response.headers().get("location").and_then(|l| l.to_str().ok()))
            .flatten()
            .and_then(|l| url::Url::parse(&url).ok()?.join(l).ok());
        match next {
            Some(next) => url = next.to_string(),
            None => return Ok(response),
        }
    }
    crate::extractors::http::no_redirect_client().get(&url).send().await
}

/// Cloudflare's "Just a moment…" page. (Every page behind Cloudflare loads
/// its challenge-platform script, so that alone means nothing.)
fn is_challenge(body: &str) -> bool {
    body.contains("<title>Just a moment...</title>") || body.contains("cf-chl-")
}

/// POSTs `body` to a site's API (same pause, retries and headers), for
/// sites whose chapter lists come from one (Wuxiaworld's gRPC-web). Returns
/// the answer's bytes.
pub async fn post(url: &str, content_type: &str, extra: &[(&str, &str)], body: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut wait = Duration::from_secs(2);
    for attempt in 1..=ATTEMPTS {
        let response = {
            let mut last = LAST.lock().await;
            if let Some(at) = *last {
                let since = at.elapsed();
                if since < PAUSE {
                    tokio::time::sleep(PAUSE - since).await;
                }
            }
            let mut request = crate::extractors::http::client()
                .post(url)
                .header("User-Agent", USER_AGENT)
                .header("Content-Type", content_type)
                .body(body.clone());
            for (name, value) in extra {
                request = request.header(*name, *value);
            }
            let response = request.send().await;
            *last = Some(Instant::now());
            response
        };
        match response {
            Ok(r) if (r.status().as_u16() == 429 || r.status().is_server_error()) && attempt < ATTEMPTS => {}
            Ok(r) if r.status().is_success() => return r.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string()),
            Ok(r) => return Err(format!("The site answered {}", r.status())),
            Err(e) if attempt < ATTEMPTS => log::warn!("novel API {url} failed ({e}), retrying"),
            Err(e) => return Err(e.to_string()),
        }
        tokio::time::sleep(wait).await;
        wait *= 2;
    }
    Err("The site kept refusing".to_string())
}

/// An image's bytes (same pause and headers), with its content type.
pub async fn bytes(url: &str, referer: Option<&str>) -> Result<(Vec<u8>, String), String> {
    {
        let mut last = LAST.lock().await;
        if let Some(at) = *last {
            // Images come from CDNs, a quarter of the pause is plenty.
            let since = at.elapsed();
            if since < PAUSE / 4 {
                tokio::time::sleep(PAUSE / 4 - since).await;
            }
        }
        *last = Some(Instant::now());
    }
    let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let mut request = crate::extractors::http::client().get(url).header("Host", host).header("User-Agent", USER_AGENT);
    if let Some(referer) = referer {
        request = request.header("Referer", referer);
    }
    let response = request.send().await.and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    Ok((bytes.to_vec(), content_type))
}
