//! A book's images: each fetched once and packed as images/0001.jpg…, the
//! chapters' `src`s pointing at those. With "Small images", hosts are asked
//! for a smaller version (srcset, or the host's own size option) rather
//! than shrinking anything here. GIFs don't move in EPUB readers, so a
//! still of one is asked for instead when the host has one.
//!
//! An image that came once is kept beside the book's chapters (its cache):
//! the book is made again from those, so an image host having a bad moment
//! (or gone since) doesn't take pictures out of a book that had them. One
//! that doesn't come is tried a few times before it's left out.

use std::collections::HashMap;
use std::path::PathBuf;

/// The types EPUB readers show, and the kept files' extensions for them.
const TYPES: [(&str, &str); 5] =
    [("image/jpeg", "jpg"), ("image/png", "png"), ("image/gif", "gif"), ("image/webp", "webp"), ("image/svg+xml", "svg")];

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
    /// Where the book's fetched images are kept.
    kept: PathBuf,
    /// Original URL → its path in the book (None: couldn't be fetched).
    seen: HashMap<String, Option<String>>,
    images: Vec<Image>,
}

impl Collector {
    /// `kept`: the folder the book's fetched images are kept in.
    pub fn new(small: bool, kept: PathBuf) -> Self {
        Self { small, kept, seen: HashMap::new(), images: Vec::new() }
    }

    /// A kept image's file, without its extension (the type's).
    fn kept_stem(&self, url: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let name: String = Sha256::digest(url.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect();
        self.kept.join(name)
    }

    fn read_kept(&self, url: &str) -> Option<(Vec<u8>, String)> {
        let stem = self.kept_stem(url);
        TYPES.iter().find_map(|(media_type, kept_as)| {
            let bytes = std::fs::read(stem.with_extension(kept_as)).ok()?;
            (!bytes.is_empty()).then(|| (bytes, media_type.to_string()))
        })
    }

    fn keep(&self, url: &str, bytes: &[u8], media_type: &str) {
        let file = self.kept_stem(url).with_extension(extension(media_type));
        let part = file.with_extension("part");
        let written = std::fs::create_dir_all(&self.kept).and_then(|_| std::fs::write(&part, bytes)).and_then(|_| std::fs::rename(&part, &file));
        if let Err(e) = written {
            log::warn!(target: "books", "couldn't keep a book image ({}): {e}", file.display());
        }
    }

    /// An image: the kept one, else fetched (a few tries, unless the host
    /// says it isn't there or isn't one) and kept.
    async fn get(&self, url: &str) -> Result<(Vec<u8>, String), String> {
        if let Some(kept) = self.read_kept(url) {
            return Ok(kept);
        }
        let mut last = String::new();
        for attempt in 0..3u64 {
            match fetch(url).await {
                Ok((bytes, media_type)) => {
                    self.keep(url, &bytes, &media_type);
                    return Ok((bytes, media_type));
                }
                // The host's answer, which asking again won't change.
                Err(e) if e.starts_with("not an image") || e.contains("status client error") => return Err(e),
                Err(e) => last = e,
            }
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
            }
        }
        Err(last)
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
        let mut result = self.get(&wanted).await;
        if result.is_err() && wanted != src {
            // The still or small version isn't there: the image itself.
            result = self.get(src).await;
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
    TYPES.iter().find(|(known, _)| *known == media_type).map_or("jpg", |(_, kept_as)| *kept_as)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_images_come_back() {
        let dir = std::env::temp_dir().join(format!("eddy-images-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let collector = Collector::new(false, dir.clone());
        let url = "https://example.com/a.png";
        assert!(collector.read_kept(url).is_none());
        collector.keep(url, &[0x89, b'P', b'N', b'G'], "image/png");
        assert_eq!(collector.read_kept(url), Some((vec![0x89, b'P', b'N', b'G'], "image/png".to_string())));
        // Another address is another image.
        assert!(collector.read_kept("https://example.com/b.png").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
