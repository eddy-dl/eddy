//! Polite fetching for reading sites: browser headers (Royal Road's
//! Cloudflare turns bare requests away), the user's cookies for the site,
//! one request at a time with a pause in between, retries when the site
//! asks to slow down, and bot checks (Cloudflare's, Anubis's) recognised for
//! what they are. On Android a check is passed in a WebView, as manga
//! readers do (see CloudflareActivity): the cookies it leaves and its user
//! agent then go with every request to that site, until the site asks again.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const USER_AGENT: &str =
    "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36";
/// Between two requests to reading sites.
const PAUSE: Duration = Duration::from_millis(1000);
/// FanFiction.net and FictionPress block readers that go faster.
const SLOW_PAUSE: Duration = Duration::from_millis(3000);

fn pause_for(host: &str) -> Duration {
    if host.ends_with("fanfiction.net") || host.ends_with("fictionpress.com") { SLOW_PAUSE } else { PAUSE }
}
const ATTEMPTS: u32 = 4;

/// When the last request went out (all reading sites share it: books are
/// saved one at a time anyway).
static LAST: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);
/// One browser check at a time (fetches waiting on the same site use the
/// one that just passed).
static CHECK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// How long a browser check may take (the user may be asked to tap).
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
const CHALLENGE: &str = "This site only opens in a browser right now (it checks for bots)";
const STILL_BLOCKED: &str = "The site still turns Eddy away after its browser check";
/// How long a site that turned Eddy away despite a pass is read through the
/// background browser before Eddy's own requests are tried again.
const BROWSER_ONLY_FOR: Duration = Duration::from_secs(30 * 60);
/// How long the background browser may take over a page.
const BROWSER_TIMEOUT: Duration = Duration::from_secs(90);

/// One background browser page at a time.
static BROWSER: tokio::sync::Mutex<u64> = tokio::sync::Mutex::const_new(0);

/// What a passed browser check left for a site: the browser's user agent and
/// its cookies (cf_clearance, Anubis's pass), which only work together.
struct Clearance {
    user_agent: String,
    cookies: crate::extractors::http::CookieJar,
    /// The WebView met a check before the page (else it had a pass already).
    checked: bool,
}

/// Where a site's clearance is kept (the WebView writes it): line one the
/// user agent, line two the cookies ("a=b; c=d"), line three "checked" when
/// the WebView met a check on the way; empty when the check wasn't passed.
fn clearance_file(host: &str) -> PathBuf {
    PathBuf::from(crate::platform::get_app_data_dir()).join("cloudflare").join(format!("{}.txt", super::clean::safe_component(host)))
}

fn clearance(host: &str) -> Option<Clearance> {
    let text = std::fs::read_to_string(clearance_file(host)).ok()?;
    let mut lines = text.lines();
    let (user_agent, cookie_line) = (lines.next()?, lines.next()?);
    let checked = lines.next().is_some_and(|l| l.trim() == "checked");
    let mut cookies = crate::extractors::http::CookieJar::default();
    for pair in cookie_line.split(';') {
        if let Some((name, value)) = pair.split_once('=') {
            cookies.set(name.trim(), value.trim());
        }
    }
    (!user_agent.trim().is_empty() && !cookies.is_empty()).then(|| Clearance { user_agent: user_agent.trim().to_string(), cookies, checked })
}

/// Has `url`'s Cloudflare check passed in a browser (Android's WebView), and
/// waits for it. `seen`: when this fetch met the challenge; a check passed
/// since (by another fetch) does.
async fn pass_check(url: &str, host: &str, seen: SystemTime) -> Result<(), String> {
    let _one = CHECK.lock().await;
    let file = clearance_file(host);
    // Asked since this fetch met it (another fetch was waiting too): passed,
    // or closed without, it isn't asked again.
    if std::fs::metadata(&file).and_then(|m| m.modified()).is_ok_and(|at| at > seen) {
        return match clearance(host) {
            Some(_) => Ok(()),
            None => Err("The site's browser check wasn't passed".to_string()),
        };
    }
    let _ = std::fs::remove_file(&file);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if !crate::platform::open_site_check(url, &file.to_string_lossy()) {
        return Err(CHALLENGE.to_string());
    }
    log::info!("waiting for {host}'s browser check");
    let began = Instant::now();
    while began.elapsed() < CHECK_TIMEOUT {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if file.exists() {
            return match clearance(host) {
                Some(_) => Ok(()),
                None => Err("The site's browser check wasn't passed".to_string()),
            };
        }
    }
    Err("The site's browser check took too long".to_string())
}

/// The sites read through the background browser, a line each: the host
/// and since when (Unix seconds). Kept in a file, so a restart doesn't send
/// Eddy's own requests (and the check) at them all over again.
fn browser_only_file() -> PathBuf {
    PathBuf::from(crate::platform::get_app_data_dir()).join("browser").join("sites.txt")
}

/// The sites still read through the background browser.
fn browser_only_sites() -> Vec<(String, u64)> {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    std::fs::read_to_string(browser_only_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (host, since) = line.split_once(' ')?;
            let since: u64 = since.trim().parse().ok()?;
            (now.saturating_sub(since) < BROWSER_ONLY_FOR.as_secs()).then(|| (host.to_string(), since))
        })
        .collect()
}

fn browser_only(host: &str) -> bool {
    browser_only_sites().iter().any(|(h, _)| h == host)
}

fn set_browser_only(host: &str) {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut sites = browser_only_sites();
    sites.retain(|(h, _)| h != host);
    sites.push((host.to_string(), now));
    let file = browser_only_file();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text: String = sites.iter().map(|(h, since)| format!("{h} {since}\n")).collect();
    if let Err(e) = std::fs::write(&file, text) {
        log::warn!("couldn't keep the sites read through the background browser: {e}");
    }
}

/// A page through the background browser (Android's WebView, unseen; see
/// PageFetcher), for a site that lets the browser in but not Eddy's own
/// requests: Cloudflare judges the connection itself, so its pass doesn't
/// carry over. A check that wants the user opens as usual, then the page is
/// asked for again.
async fn browser_text(url: &str, host: &str) -> Result<String, String> {
    let mut count = BROWSER.lock().await;
    for round in 0..2 {
        *count += 1;
        let file = PathBuf::from(crate::platform::get_app_data_dir()).join("browser").join(format!("page-{}.html", *count));
        let _ = std::fs::remove_file(&file);
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        {
            let mut last = LAST.lock().await;
            if let Some(at) = *last {
                let pause = pause_for(host);
                if at.elapsed() < pause {
                    tokio::time::sleep(pause - at.elapsed()).await;
                }
            }
            *last = Some(Instant::now());
        }
        if !crate::platform::fetch_page(url, &file.to_string_lossy()) {
            return Err(if round == 0 { CHALLENGE } else { STILL_BLOCKED }.to_string());
        }
        let began = Instant::now();
        let text = loop {
            if began.elapsed() > BROWSER_TIMEOUT {
                return Err("The site took too long in the browser".to_string());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Ok(text) = std::fs::read_to_string(&file) {
                let _ = std::fs::remove_file(&file);
                break text;
            }
        };
        let (state, html) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        // A check the WebView took for the page (a title in another
        // language, say) is still a check.
        let state = if state.trim() == "page" && looks_like_check(html) { "check" } else { state.trim() };
        match state {
            "page" => return Ok(html.to_string()),
            "check" if round == 0 => pass_check(url, host, SystemTime::now()).await?,
            "check" => return Err(CHALLENGE.to_string()),
            _ => {
                log::warn!("background browser: {url}: {}", state.trim().trim_start_matches("error").trim());
                return Err("The site's page didn't load in the browser".to_string());
            }
        }
    }
    Err(CHALLENGE.to_string())
}

/// A page's HTML.
pub async fn text(url: &str) -> Result<String, String> {
    let domain = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_default();
    let cookies = crate::extractors::http::CookieJar::from_netscape_file(&crate::extractors::http::cookie_file(), &domain);
    let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    if browser_only(&host) {
        return browser_text(url, &host).await;
    }

    let pause = pause_for(&host);
    let mut wait = Duration::from_secs(2);
    let mut checked = false;
    for attempt in 1..=ATTEMPTS {
        // A passed browser check's cookies, with its user agent.
        let pass = clearance(&host);
        let mut jar = cookies.clone();
        if let Some(pass) = &pass {
            jar.merge(&pass.cookies);
        }
        let user_agent = pass.as_ref().map_or(USER_AGENT, |p| p.user_agent.as_str());
        let response = {
            let mut last = LAST.lock().await;
            if let Some(at) = *last {
                let since = at.elapsed();
                if since < pause {
                    tokio::time::sleep(pause - since).await;
                }
            }
            let response = get(url, &jar, user_agent).await;
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
        if let Some(kind) = challenge(status.as_u16(), &body) {
            // Anubis's pass is a cookie, which Eddy's own requests can
            // carry: passed in a browser once, then tried again.
            if kind == ANUBIS && !checked {
                checked = true;
                pass_check(url, &host, SystemTime::now()).await?;
                continue;
            }
            // Cloudflare's pass doesn't carry over (it judges the
            // connection), nor did Anubis's this time: the site's pages come
            // through the background browser for a while, which meets the
            // check itself (and asks the user only when it has to).
            let pass = clearance(&host);
            log::warn!(
                "{host} answers {status} with {kind} (the browser {}; cookies sent: {}); reading it through the background browser",
                match &pass {
                    Some(p) if p.checked => "met a check",
                    Some(_) => "had a pass already",
                    None => "had no pass",
                },
                pass.as_ref().map(|p| p.cookies.names().join(", ")).unwrap_or_default(),
            );
            set_browser_only(&host);
            return browser_text(url, &host).await;
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
async fn get(url: &str, cookies: &crate::extractors::http::CookieJar, user_agent: &str) -> Result<reqwest::Response, reqwest::Error> {
    let mut url = url.to_string();
    for _ in 0..5 {
        let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        let mut request = crate::extractors::http::no_redirect_client()
            .get(&url)
            .header("Host", host)
            .header("User-Agent", user_agent)
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

/// A bot check instead of the page, and which: Cloudflare's "Just a
/// moment…" (always a 403 or 503; every page behind Cloudflare loads its
/// challenge-platform script, and a real page can carry a Turnstile box, so
/// its markers alone mean nothing), or Anubis's "Making sure you're not a
/// bot!" (Royal Road since October 2026). Both pass in a browser.
/// A bot check's page, whatever its status and language.
fn looks_like_check(html: &str) -> bool {
    html.contains("id=\"anubis_challenge\"") || html.contains("window._cf_chl_opt") || html.contains("id=\"challenge-error-text\"")
}

pub(super) fn title_of(html: &str) -> String {
    let start = html.find("<title").and_then(|i| html[i..].find('>').map(|j| i + j + 1));
    start.and_then(|s| html[s..].find("</title>").map(|e| html[s..s + e].trim().to_string())).unwrap_or_default()
}

const ANUBIS: &str = "Anubis's check";

fn challenge(status: u16, body: &str) -> Option<&'static str> {
    if body.contains("id=\"anubis_challenge\"") {
        Some(ANUBIS)
    } else if matches!(status, 403 | 429 | 503) && (body.contains("<title>Just a moment...</title>") || body.contains("cf-chl-") || body.contains("_cf_chl_opt")) {
        Some("Cloudflare's check")
    } else {
        None
    }
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
    // From a site behind a bot check (not its CDN): with the check's pass.
    let pass = clearance(&host);
    let user_agent = pass.as_ref().map_or(USER_AGENT, |p| p.user_agent.as_str());
    let mut request = crate::extractors::http::client().get(url).header("Host", &host).header("User-Agent", user_agent);
    if let Some(pass) = &pass {
        request = request.header("Cookie", pass.cookies.header());
    }
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
