//! Wuxiaworld (wuxiaworld.com). A novel's page carries its details as the
//! JSON its app starts from (`window.__REACT_QUERY_STATE__`, the "novel"
//! query); the chapter list comes from its API (gRPC-web,
//! `Chapters/GetChapterList`, protobuf), in volumes. A chapter's page has
//! its text in the same JSON (the "chapter" query). Many chapters need
//! Wuxiaworld's karma or a subscription: those are listed as locked.
//!
//! The API's protobuf, as far as it's read here (field numbers):
//! answer { 1: volume { 2: title, 6: chapter { 2: name, 3: slug,
//! 20: pricing { 1: free } } } }.

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use serde_json::Value;
use std::sync::LazyLock;

static NOVEL_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^https?://(?:www\.)?wuxiaworld\.com/novel/([a-z0-9-]+)").unwrap());

/// A novel, or a chapter of one.
pub fn is_book_url(url: &str) -> bool {
    NOVEL_RE.is_match(url.trim())
}

pub async fn book(url: &str) -> Result<Book, String> {
    let slug = NOVEL_RE.captures(url.trim()).map(|c| c[1].to_string()).ok_or("Not a Wuxiaworld novel link")?;
    let page_url = format!("https://www.wuxiaworld.com/novel/{slug}");
    let html = fetch::text(&page_url).await?;
    let novel = query(&html, "novel").ok_or("No novel on this page (or the page changed)")?;
    let id = novel.get("id").and_then(Value::as_i64).ok_or("No novel on this page")?;
    let list = fetch::post(
        "https://api2.wuxiaworld.com/wuxiaworld.api.v2.Chapters/GetChapterList",
        "application/grpc-web+proto",
        &[("X-Grpc-Web", "1"), ("Origin", "https://www.wuxiaworld.com")],
        grpc_frame(&varint_field(1, id as u64)),
    )
    .await?;
    parse_book(&slug, &novel, &list)
}

/// The novel from its page's JSON and the API's chapter list (for tests).
pub fn parse_book(slug: &str, novel: &Value, list: &[u8]) -> Result<Book, String> {
    let value = |key: &str| novel.get(key).and_then(|v| v.get("value")).and_then(Value::as_str).map(str::to_string);
    let title = novel.get("name").and_then(Value::as_str).ok_or("No novel on this page")?.to_string();
    let chapters = chapter_list(slug, list)?;
    if chapters.is_empty() {
        return Err("This novel has no chapters (or the API changed)".to_string());
    }
    let tags = ["genres", "tags"]
        .iter()
        .flat_map(|k| novel.get(*k).and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect();
    Ok(Book {
        site: Site::Wuxiaworld,
        id: slug.to_string(),
        url: format!("https://www.wuxiaworld.com/novel/{slug}"),
        title,
        author: value("authorName"),
        description: value("synopsis").or_else(|| value("description")).map(|d| clean::clean(&d, &[], "https://www.wuxiaworld.com/", false)),
        // Its covers are WebP, which not every reader shows; the same image
        // comes as a PNG too.
        covers: value("coverUrl")
            .map(|c| {
                let png = c.replacen(".webp", ".png", 1);
                if png != c { vec![png, c] } else { vec![c] }
            })
            .unwrap_or_default(),
        // Translated into English (its "language" is the original's).
        language: Some("en".to_string()),
        tags,
        chapters,
    })
}

/// The data of one of the page's queries (`["novel", …]`, `["chapter", …]`).
fn query(html: &str, key: &str) -> Option<Value> {
    let at = html.find("window.__REACT_QUERY_STATE__")?;
    let rest = html[at..].split_once('=')?.1.trim_start();
    let state: Value = serde_json::Deserializer::from_str(rest).into_iter::<Value>().next()?.ok()?;
    state.get("queries")?.as_array()?.iter().find_map(|q| {
        let first = q.get("queryKey")?.as_array()?.first()?.as_str()?;
        (first == key).then(|| q.get("state")?.get("data")?.get("item").cloned()).flatten()
    })
}

/// The chapters from the API's answer, in volumes.
fn chapter_list(slug: &str, answer: &[u8]) -> Result<Vec<ChapterRef>, String> {
    let message = grpc_message(answer).ok_or("Wuxiaworld's chapter list wasn't readable")?;
    let mut chapters = Vec::new();
    for volume in fields(message).filter(|(f, _)| *f == 1).filter_map(|(_, v)| v.bytes()) {
        let volume_title = fields(volume).find(|(f, _)| *f == 2).and_then(|(_, v)| v.text()).filter(|t| !t.is_empty());
        for chapter in fields(volume).filter(|(f, _)| *f == 6).filter_map(|(_, v)| v.bytes()) {
            let field = |n: u64| fields(chapter).find(|(f, _)| *f == n).map(|(_, v)| v);
            let Some(chapter_slug) = field(3).and_then(|v| v.text()) else {
                continue;
            };
            let free = field(20)
                .and_then(|v| v.bytes())
                .and_then(|p| fields(p).find(|(f, _)| *f == 1).and_then(|(_, v)| v.number()))
                == Some(1);
            chapters.push(ChapterRef {
                url: format!("https://www.wuxiaworld.com/novel/{slug}/{chapter_slug}"),
                title: field(2).and_then(|v| v.text()).unwrap_or_else(|| chapter_slug.clone()),
                volume: volume_title.clone(),
                locked: !free,
            });
        }
    }
    Ok(chapters)
}

pub async fn chapter(url: &str) -> Result<RawChapter, String> {
    let html = fetch::text(url).await?;
    parse_chapter(&html)
}

pub fn parse_chapter(html: &str) -> Result<RawChapter, String> {
    let chapter = query(html, "chapter").ok_or("No chapter on the page")?;
    if chapter.get("isTeaser").and_then(Value::as_bool) == Some(true) {
        return Err("Only a preview of this chapter is free".to_string());
    }
    let content = chapter.get("content").and_then(|c| c.get("value")).and_then(Value::as_str).ok_or("No chapter text on the page")?;
    // It opens with its own title and a rule; the book has its heading.
    let name = chapter.get("name").and_then(Value::as_str).unwrap_or("");
    let body = TITLE_RE
        .captures(content)
        .filter(|c| !name.is_empty() && c[1].trim() == clean::escape(name).trim())
        .map(|c| content[c.get(0).unwrap().end()..].to_string())
        .unwrap_or_else(|| content.to_string());
    Ok(RawChapter { body, notes: Vec::new(), hidden_classes: Vec::new() })
}

static TITLE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^\s*<p>(.*?)</p>(?:\s*<p><br\s*/?></p>)?\s*(?:<hr\s*/?>)?(?:\s*<p><br\s*/?></p>)?").unwrap());

// --- A little protobuf, for the chapter list ---

/// A field's value: a number, or bytes (text or a message).
enum Field<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

impl<'a> Field<'a> {
    fn number(&self) -> Option<u64> {
        match self {
            Field::Number(n) => Some(*n),
            Field::Bytes(_) => None,
        }
    }
    fn bytes(&self) -> Option<&'a [u8]> {
        match self {
            Field::Bytes(b) => Some(b),
            Field::Number(_) => None,
        }
    }
    fn text(&self) -> Option<String> {
        self.bytes().and_then(|b| std::str::from_utf8(b).ok()).map(|s| s.trim().to_string())
    }
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// A message's fields in order (field number, value); stops at anything
/// it can't read.
fn fields(message: &[u8]) -> impl Iterator<Item = (u64, Field<'_>)> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let key = varint(message, &mut at)?;
        let value = match key & 7 {
            0 => Field::Number(varint(message, &mut at)?),
            1 => {
                let bytes = message.get(at..at + 8)?;
                at += 8;
                Field::Bytes(bytes)
            }
            2 => {
                let len = varint(message, &mut at)? as usize;
                let bytes = message.get(at..at.checked_add(len)?)?;
                at += len;
                Field::Bytes(bytes)
            }
            5 => {
                let bytes = message.get(at..at + 4)?;
                at += 4;
                Field::Bytes(bytes)
            }
            _ => return None,
        };
        Some((key >> 3, value))
    })
}

fn varint_field(number: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for mut n in [number << 3, value] {
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
    out
}

/// gRPC-web's framing: a data frame (flag 0, then its length).
fn grpc_frame(message: &[u8]) -> Vec<u8> {
    let mut out = vec![0];
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// The first data frame's message.
fn grpc_message(answer: &[u8]) -> Option<&[u8]> {
    let (&flag, rest) = answer.split_first()?;
    let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    (flag & 0x80 == 0).then(|| rest.get(4..4 + len)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert!(is_book_url("https://www.wuxiaworld.com/novel/martial-world"));
        assert!(is_book_url("https://www.wuxiaworld.com/novel/martial-world/mw-chapter-1"));
        assert!(!is_book_url("https://www.wuxiaworld.com/blog/news"));
    }

    #[test]
    fn protobuf() {
        assert_eq!(varint_field(1, 36), vec![0x08, 36]);
        assert_eq!(varint_field(1, 300), vec![0x08, 0xac, 0x02]);
        // A volume holding one free chapter and one locked.
        let chapter = |name: &str, slug: &str, free: bool| {
            let mut c = Vec::new();
            for (n, s) in [(2u8, name), (3u8, slug)] {
                c.push(n << 3 | 2);
                c.push(s.len() as u8);
                c.extend_from_slice(s.as_bytes());
            }
            // Field 20 (pricing), its key 20 << 3 | 2 as a varint; free: { 1: 1 }.
            let pricing: Vec<u8> = if free { vec![0x08, 1] } else { Vec::new() };
            c.extend_from_slice(&[0xa2, 0x01, pricing.len() as u8]);
            c.extend_from_slice(&pricing);
            c
        };
        let mut volume = vec![0x12, 3];
        volume.extend_from_slice(b"One");
        for c in [chapter("Chapter 1", "mw-chapter-1", true), chapter("Chapter 2", "mw-chapter-2", false)] {
            volume.push(0x32);
            volume.push(c.len() as u8);
            volume.extend_from_slice(&c);
        }
        let mut message = vec![0x0a, volume.len() as u8];
        message.extend_from_slice(&volume);
        let list = chapter_list("martial-world", &grpc_frame(&message)).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].url, "https://www.wuxiaworld.com/novel/martial-world/mw-chapter-1");
        assert_eq!(list[0].volume.as_deref(), Some("One"));
        assert!(!list[0].locked);
        assert!(list[1].locked);
    }

    #[test]
    fn chapter_text_without_its_title() {
        let html = r#"<script>window.__REACT_QUERY_STATE__ = {"queries":[{"queryKey":["chapter","x","c1",null],"state":{"data":{"item":{"name":"Chapter 1 - Lin Ming","isTeaser":false,"content":{"value":"<p>Chapter 1 - Lin Ming</p><p><br></p><hr><p><br></p><p>Within the city</p>"}}}}}]}</script>"#;
        let raw = parse_chapter(html).unwrap();
        assert_eq!(raw.body, "<p>Within the city</p>");
    }
}
