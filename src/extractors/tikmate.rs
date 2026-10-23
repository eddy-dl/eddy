//! tikmate.io, a TikTok download helper (adapted from neon_bot's
//! tikmate_dl): its home page gives a token, posting a TikTok link with it
//! gives a packed script (see packed.rs) that writes the answer's HTML. A
//! photo post's answer has its images (`.card`: `img.card-img-top` the
//! preview, `a.btn-main` the image) and the slideshow as a video
//! (`a.download-btn`); a video's has just the video. Eddy uses it for that
//! slideshow video, and for everything when its own TikTok reader fails.

use super::http;
use scraper::{Html, Selector};
use std::sync::LazyLock;

const HOME: &str = "https://tikmate.io/";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/94.0.4606.81 Safari/537.36";

/// What tikmate found for a post.
#[derive(Clone, Debug, Default)]
pub struct Post {
    /// A photo post's images: (image, its preview).
    pub images: Vec<(String, String)>,
    /// The video: a video's own, or a photo post's slideshow.
    pub video: Option<String>,
    /// The video's preview picture, when the answer has one.
    pub thumbnail: Option<String>,
    pub caption: Option<String>,
}

/// The headers tikmate's links want.
pub fn headers() -> Vec<(String, String)> {
    vec![("Referer".to_string(), HOME.to_string()), ("User-Agent".to_string(), UA.to_string())]
}

pub async fn fetch(url: &str) -> Result<Post, String> {
    static TOKEN_RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"name="token" value="([^"]*)""#).unwrap());
    let home = http::client()
        .get(HOME)
        .header("Referer", HOME)
        .header("User-Agent", UA)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("tikmate: {e}"))?
        .text()
        .await
        .map_err(|e| format!("tikmate: {e}"))?;
    let token = TOKEN_RE.captures(&home).map(|c| c[1].to_string()).ok_or("tikmate: no token on its page")?;
    let raw = http::client()
        .post(format!("{HOME}abc.php"))
        .header("Referer", HOME)
        .header("Origin", "https://tikmate.io")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("User-Agent", UA)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(http::form(&[("url", url), ("token", &token)]))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("tikmate: {e}"))?
        .text()
        .await
        .map_err(|e| format!("tikmate: {e}"))?;
    let decoded = super::packed::decode(&raw).await;
    let post = parse(&decoded);
    if post.images.is_empty() && post.video.is_none() {
        return Err("tikmate: nothing to download in its answer".to_string());
    }
    Ok(post)
}

/// The answer's HTML (inside the script that writes it), read.
pub fn parse(decoded: &str) -> Post {
    static HTML_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"(?s)innerHTML\s*=\s*"(.*?)";\s*parent\.document"#).unwrap());
    let html = HTML_RE.captures(decoded).map(|c| c[1].replace("\\\"", "\"")).unwrap_or_else(|| decoded.to_string());
    let page = Html::parse_fragment(&html);
    let select = |css: &str| Selector::parse(css).unwrap();
    let text = |el: scraper::ElementRef| el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");

    let caption = page
        .select(&select("div.videotikmate-middle p, .video-info p, p[itemprop=\"description\"]"))
        .next()
        .map(text)
        .filter(|c| !c.is_empty())
        .or_else(|| page.select(&select("p")).map(text).find(|t| t.chars().count() > 3));
    let images = page
        .select(&select(".card"))
        .filter_map(|card| {
            let preview = card.select(&select("img.card-img-top")).next()?.attr("src")?.to_string();
            let image = card.select(&select("a.btn-main")).next()?.attr("href")?.to_string();
            Some((image, preview))
        })
        .collect();
    let video = page.select(&select("a.download-btn")).next().and_then(|a| a.attr("href")).map(str::to_string);
    let thumbnail = page
        .select(&select(".thumbnail img, .image-tikmate img, img[alt=\"avatar\"]"))
        .next()
        .and_then(|i| i.attr("src"))
        .map(str::to_string);
    Post { images, video, thumbnail, caption }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photo_post_answer() {
        let decoded = r#"document.getElementById("x").innerHTML = "<div class=\"video-info\"><p>Two pictures</p></div><div class=\"card\"><img class=\"card-img-top\" src=\"https://p/1s.jpg\"><a class=\"btn-main\" href=\"https://d/1.jpg\">Download</a></div><div class=\"card\"><img class=\"card-img-top\" src=\"https://p/2s.jpg\"><a class=\"btn-main\" href=\"https://d/2.jpg\">Download</a></div><a class=\"abutton download-btn\" href=\"https://d/slideshow.mp4\">Video</a>"; parent.document.x();"#;
        let post = parse(decoded);
        assert_eq!(post.images, vec![("https://d/1.jpg".into(), "https://p/1s.jpg".into()), ("https://d/2.jpg".into(), "https://p/2s.jpg".into())]);
        assert_eq!(post.video.as_deref(), Some("https://d/slideshow.mp4"));
        assert_eq!(post.caption.as_deref(), Some("Two pictures"));
    }
}
