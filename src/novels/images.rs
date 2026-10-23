//! A book's images: each fetched once and packed as images/0001.jpg…, the
//! chapters' `src`s pointing at those. With "Small images", hosts are asked
//! for a smaller version (srcset, or the host's own size option) rather
//! than shrinking anything here. GIFs don't move in EPUB readers, so a
//! still of one is asked for instead when the host has one.

use std::collections::HashMap;

/// Wide enough for a phone screen; what "Small images" asks for.
const SMALL_WIDTH: u32 = 480;

pub struct Image {
    /// Inside the book: "images/0001.jpg".
    pub path: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

pub struct Collector {
    small: bool,
    /// Original URL → its path in the book (None: couldn't be fetched).
    seen: HashMap<String, Option<String>>,
    images: Vec<Image>,
}

impl Collector {
    pub fn new(small: bool) -> Self {
        Self { small, seen: HashMap::new(), images: Vec::new() }
    }

    /// The chapter with its images fetched and pointing inside the book;
    /// ones that can't be fetched are dropped (their alt text stays).
    pub async fn rewrite(&mut self, xhtml: &str) -> String {
        let img = regex::Regex::new(r#"<img src="([^"]*)" alt="([^"]*)"(?: data-eddy-srcset="([^"]*)")?/>"#).unwrap();
        let mut out = String::with_capacity(xhtml.len());
        let mut last = 0;
        let found: Vec<(std::ops::Range<usize>, String, String, Option<String>)> = img
            .captures_iter(xhtml)
            .map(|c| (c.get(0).unwrap().range(), unescape(&c[1]), c[2].to_string(), c.get(3).map(|m| unescape(m.as_str()))))
            .collect();
        for (range, src, alt, srcset) in found {
            out.push_str(&xhtml[last..range.start]);
            last = range.end;
            match self.add(&src, srcset.as_deref()).await {
                Some(path) => out.push_str(&format!("<img src=\"../{path}\" alt=\"{alt}\"/>")),
                None if !alt.is_empty() => out.push_str(&format!("<span class=\"image-alt\">[{alt}]</span>")),
                None => {}
            }
        }
        out.push_str(&xhtml[last..]);
        out
    }

    async fn add(&mut self, src: &str, srcset: Option<&str>) -> Option<String> {
        if let Some(known) = self.seen.get(src) {
            return known.clone();
        }
        let wanted = choose(src, srcset, self.small);
        let mut result = fetch(&wanted).await;
        if result.is_err() && wanted != src {
            // The still or small version isn't there: the image itself.
            result = fetch(src).await;
        }
        let path = match result {
            Ok((bytes, media_type)) => {
                let path = format!("images/{:04}.{}", self.images.len() + 1, extension(&media_type));
                self.images.push(Image { path: path.clone(), media_type, bytes });
                Some(path)
            }
            Err(e) => {
                log::warn!("couldn't fetch book image {wanted}: {e}");
                None
            }
        };
        self.seen.insert(src.to_string(), path.clone());
        path
    }

    pub fn into_images(self) -> Vec<Image> {
        self.images
    }
}

/// An image's bytes and type, if it's one EPUB readers show.
pub async fn fetch(url: &str) -> Result<(Vec<u8>, String), String> {
    let (bytes, content_type) = super::fetch::bytes(url, None).await?;
    let media_type = sniff(&bytes).map(str::to_string).unwrap_or(content_type);
    if !matches!(media_type.as_str(), "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/svg+xml") {
        return Err(format!("not an image ({media_type})"));
    }
    Ok((bytes, media_type))
}

fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [b'G', b'I', b'F', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "jpg",
    }
}

/// Which URL to fetch for an image: a still for a GIF where the host has
/// one, a smaller version with "Small images", else the image itself.
fn choose(src: &str, srcset: Option<&str>, small: bool) -> String {
    let Ok(url) = url::Url::parse(src) else {
        return src.to_string();
    };
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let path = url.path().to_ascii_lowercase();
    let is_gif = path.ends_with(".gif");

    // Imgur: "abc.gif" → "abc.jpg"; "l" (640 px) / "h" (1024 px) suffixes.
    if host == "i.imgur.com" {
        if let Some(stem) = url.path().rsplit_once('.').map(|(s, _)| s.trim_start_matches('/').to_string()) {
            // Ids are 5 or 7 characters; a 6th or 8th is a size suffix.
            let mut stem = stem;
            if matches!(stem.len(), 6 | 8) && stem.ends_with(['s', 'b', 't', 'm', 'l', 'h']) {
                stem.pop();
            }
            if is_gif || small {
                let suffix = if small { "l" } else { "" };
                return format!("https://i.imgur.com/{stem}{suffix}.jpg");
            }
        }
    }
    // Giphy: the still frame of the same GIF.
    if is_gif && host.ends_with("giphy.com") {
        if let Some(id) = url.path().split('/').skip_while(|p| *p != "media").nth(1) {
            return format!("https://media.giphy.com/media/{id}/giphy_s.gif");
        }
    }
    if !small {
        return src.to_string();
    }
    // The page's own smaller versions.
    if let Some(candidate) = srcset.and_then(smallest_wide_enough) {
        return candidate;
    }
    // Hosts that resize on request.
    let mut resized = url.clone();
    if host.ends_with(".wp.com") || path.contains("/wp-content/") {
        resized.query_pairs_mut().append_pair("w", &SMALL_WIDTH.to_string());
        return resized.to_string();
    }
    if host == "media.discordapp.net" || host == "cdn.discordapp.com" {
        resized.query_pairs_mut().append_pair("width", &SMALL_WIDTH.to_string());
        return resized.to_string();
    }
    src.to_string()
}

/// From "a.jpg 320w, b.jpg 640w, c.jpg 1280w": the smallest at least
/// SMALL_WIDTH wide (or the widest there is).
fn smallest_wide_enough(srcset: &str) -> Option<String> {
    let mut candidates: Vec<(u32, String)> = srcset
        .split(", ")
        .filter_map(|c| {
            let (url, size) = c.trim().rsplit_once(' ')?;
            let width = size.strip_suffix('w')?.parse().ok()?;
            Some((width, url.to_string()))
        })
        .collect();
    candidates.sort();
    candidates
        .iter()
        .find(|(w, _)| *w >= SMALL_WIDTH)
        .or(candidates.last())
        .map(|(_, u)| u.clone())
}

fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}
