//! Spotify track links (open.spotify.com/track/…, spotify.link short
//! links), adapted from neon_bot's spotify_dl. Spotify's own audio is
//! protected, so the song is looked up instead: its title and artists come
//! from Spotify's embed page (no login or key), and the first song YouTube
//! Music finds for them (asked through yt-dlp) is what Eddy downloads. The
//! link is swapped for that one as it's added, so everything after sees a
//! YouTube Music link.

use super::http;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static TRACK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"open\.spotify\.com/(?:intl-[a-z]{2}/)?(?:embed/)?track/([A-Za-z0-9]{22})").unwrap());
static SHORT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://spotify(?:\.app)?\.link/[A-Za-z0-9]+").unwrap());
static NEXT_DATA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<script id="__NEXT_DATA__"[^>]*>(.*?)</script>"#).unwrap());

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// A song as Spotify names it.
#[derive(Debug, PartialEq)]
pub struct Track {
    pub title: String,
    pub artists: Vec<String>,
}

impl Track {
    /// What YouTube Music is asked for: "artists title".
    fn query(&self) -> String {
        format!("{} {}", self.artists.join(" "), self.title).trim().to_string()
    }
}

/// A Spotify track's link, or a Spotify short link (which may lead to one).
pub fn is_track_link(url: &str) -> bool {
    let url = url.trim();
    TRACK_RE.is_match(url) || SHORT_RE.is_match(url)
}

async fn page(url: &str) -> Result<reqwest::Response, String> {
    http::client()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Spotify: {e}"))
}

/// The track a link is for, read from Spotify's embed page.
pub async fn track(url: &str) -> Result<Track, String> {
    let id = match TRACK_RE.captures(url.trim()) {
        Some(found) => found[1].to_string(),
        // A short link: the track's is where its redirects end.
        None => {
            let landed = page(url.trim()).await?.url().to_string();
            TRACK_RE.captures(&landed).map(|found| found[1].to_string()).ok_or("Not a Spotify track link (only single tracks are looked up)")?
        }
    };
    let html = page(&format!("https://open.spotify.com/embed/track/{id}")).await?.text().await.map_err(|e| format!("Spotify: {e}"))?;
    parse_track(&html)
}

/// The track in an embed page's data (apart from the fetch, for tests).
fn parse_track(html: &str) -> Result<Track, String> {
    let data = NEXT_DATA_RE.captures(html).ok_or("Spotify's page has no track on it")?;
    let json: Value = serde_json::from_str(&data[1]).map_err(|_| "Spotify's page has no track on it")?;
    let entity = json.pointer("/props/pageProps/state/data/entity").ok_or("Spotify didn't find this track")?;
    let title = ["name", "title"]
        .iter()
        .find_map(|key| entity.get(key)?.as_str().map(str::trim).filter(|t| !t.is_empty()))
        .ok_or("Spotify's track has no title")?
        .to_string();
    let artists = entity["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|artist| artist["name"].as_str().map(str::trim).filter(|n| !n.is_empty()).map(str::to_string))
        .collect();
    Ok(Track { title, artists })
}

/// The first song YouTube Music finds for `query`, through yt-dlp (a flat
/// read of its search page's Songs: no video is looked into).
async fn search(query: &str) -> Result<Option<String>, String> {
    let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let mut args: Vec<String> =
        ["--dump-single-json", "--flat-playlist", "--playlist-end", "1", "--no-warnings", "--socket-timeout", "15"].map(String::from).into();
    args.extend(super::ytdlp::cookie_args());
    args.push(format!("https://music.youtube.com/search?q={encoded}#songs"));
    let (success, json, stderr) = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut child = crate::children::spawn_tool(crate::children::Tool::Ytdlp, &args)?;
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
        let mut json = Vec::new();
        if let Some(mut stdout) = child.stdout.take() {
            let _ = stdout.read_to_end(&mut json);
        }
        let success = child.wait();
        crate::children::checked(child.id());
        Ok::<_, std::io::Error>((success?, json, errors.join().unwrap_or_default()))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("Couldn't run yt-dlp: {e}"))?;
    if !success {
        crate::downloader::media::log_stderr(&format!("searching YouTube Music for {query}"), &stderr);
        let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed");
        return Err(last.trim_start_matches("ERROR: ").to_string());
    }
    // The JSON is the last line that starts one.
    let line = json.split(|b| *b == b'\n').rev().find(|l| l.first() == Some(&b'{')).unwrap_or_default();
    let found: Value = serde_json::from_slice(line).map_err(|e| format!("Unexpected yt-dlp output: {e}"))?;
    Ok(first_song(&found))
}

/// The first entry's link in yt-dlp's answer.
fn first_song(found: &Value) -> Option<String> {
    let id = found["entries"].as_array()?.iter().find_map(|entry| entry["id"].as_str().filter(|id| !id.is_empty()))?;
    Some(format!("https://music.youtube.com/watch?v={id}"))
}

/// The YouTube Music link of the song a Spotify link is for.
pub async fn youtube_music_link(url: &str) -> Result<String, String> {
    if !crate::platform::ytdlp_available() {
        return Err("Spotify links need yt-dlp (Settings → Downloads → External packages)".to_string());
    }
    let track = track(url).await?;
    let query = track.query();
    let link = search(&query).await?.ok_or_else(|| format!("“{query}” wasn't found on YouTube Music"))?;
    log::info!("Spotify: {url} -> {link} ({query})");
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert!(is_track_link("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=abc"));
        assert!(is_track_link("https://open.spotify.com/intl-fr/track/4cOdK2wGLETKBW3PvgPWqT"));
        assert!(is_track_link("https://open.spotify.com/embed/track/4cOdK2wGLETKBW3PvgPWqT"));
        assert!(is_track_link("https://spotify.link/AbCdEf123"));
        assert!(is_track_link("https://spotify.app.link/AbCdEf123"));
        assert!(!is_track_link("https://open.spotify.com/album/4cOdK2wGLETKBW3PvgPWqT"));
        assert!(!is_track_link("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"));
        assert!(!is_track_link("https://example.com/?u=https://spotify.link/AbCdEf123"));
    }

    #[test]
    fn track_from_the_embed_page() {
        let html = r#"<html><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"state":{"data":{"entity":
            {"type":"track","name":"Never Gonna Give You Up","artists":[{"name":"Rick Astley"},{"name":""},{"uri":"x"}]}}}}}}</script></html>"#;
        let track = parse_track(html).unwrap();
        assert_eq!(track, Track { title: "Never Gonna Give You Up".to_string(), artists: vec!["Rick Astley".to_string()] });
        assert_eq!(track.query(), "Rick Astley Never Gonna Give You Up");
        // "title" where there's no "name"; no artists.
        let html = r#"<script id="__NEXT_DATA__">{"props":{"pageProps":{"state":{"data":{"entity":{"title":"Intro"}}}}}}</script>"#;
        assert_eq!(parse_track(html).unwrap().query(), "Intro");
        assert!(parse_track("<html></html>").is_err());
        assert!(parse_track(r#"<script id="__NEXT_DATA__">{"props":{"pageProps":{}}}</script>"#).is_err());
    }

    #[test]
    fn first_search_result() {
        let found = serde_json::json!({ "entries": [null, { "id": "" }, { "id": "dQw4w9WgXcQ" }, { "id": "other" }] });
        assert_eq!(first_song(&found).as_deref(), Some("https://music.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert_eq!(first_song(&serde_json::json!({ "entries": [] })), None);
        assert_eq!(first_song(&serde_json::json!({})), None);
    }
}
