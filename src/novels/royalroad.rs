//! Royal Road (royalroad.com). The fiction page carries every chapter as
//! JSON (`window.chapters`, with which ones are paid early access), the
//! book's details as meta tags; a chapter's text is `div.chapter-inner`,
//! its author notes `.author-note-portlet`. Each chapter hides a watermark
//! paragraph with a page style; see clean::hidden_classes.

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use scraper::{Html, Selector};
use serde_json::Value;
use std::sync::LazyLock;

static FICTION_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^https?://(?:www\.)?royalroadl?\.com/fiction/(\d+)").unwrap());

/// A fiction page, or a chapter of one (the book is read from its fiction
/// page either way).
pub fn is_book_url(url: &str) -> bool {
    FICTION_RE.is_match(url.trim())
}

pub async fn book(url: &str) -> Result<Book, String> {
    let id = FICTION_RE.captures(url.trim()).map(|c| c[1].to_string()).ok_or("Not a Royal Road fiction link")?;
    let page_url = format!("https://www.royalroad.com/fiction/{id}");
    let html = fetch::text(&page_url).await?;
    parse_book(&id, &page_url, &html)
}

/// Everything from the fiction page (separate from the fetch, for tests).
pub fn parse_book(id: &str, page_url: &str, html: &str) -> Result<Book, String> {
    let chapters = chapters(html, page_url);
    let page = Html::parse_document(html);
    let meta = |property: &str| {
        let selector = Selector::parse(&format!("meta[property=\"{property}\"], meta[name=\"{property}\"]")).ok()?;
        page.select(&selector).next().and_then(|m| m.attr("content")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let first = |css: &str| {
        let selector = Selector::parse(css).ok()?;
        page.select(&selector).next()
    };

    let title = first("h1")
        .map(|h| h.text().collect::<String>().trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| meta("og:title"))
        .ok_or_else(|| {
            // What came instead, for the logs.
            log::warn!("{page_url}: no book on the page ({} characters, titled {:?}; saved as unread-page.html beside the logs)", html.len(), fetch::title_of(html));
            crate::logs::save("unread-page.html", html);
            "No book on this page".to_string()
        })?;
    if chapters.is_empty() {
        return Err("This book has no chapters (or the page changed)".to_string());
    }
    let description = first("div.description .hidden-content")
        .or_else(|| first("div.description"))
        .map(|d| clean::clean(&d.inner_html(), &[], page_url, false))
        .filter(|d| !d.trim().is_empty());
    let tags_selector = Selector::parse("a.fiction-tag, span.fiction-tag").unwrap();
    let tags = page.select(&tags_selector).map(|t| t.text().collect::<String>().trim().to_string()).filter(|t| !t.is_empty()).collect();
    let language = first("html").and_then(|h| h.attr("lang")).map(str::to_string);

    Ok(Book {
        site: Site::RoyalRoad,
        id: id.to_string(),
        url: meta("og:url").unwrap_or_else(|| page_url.to_string()),
        title,
        author: meta("books:author").or_else(|| meta("twitter:creator")),
        description,
        // The page links the 200×300 cover; the CDN has a 400×600 one too
        // (and nothing larger).
        covers: meta("og:image")
            .filter(|c| !c.contains("nocover"))
            .map(|c| {
                let large = c.replace("/covers-full/", "/covers-large/");
                if large != c { vec![large, c] } else { vec![c] }
            })
            .unwrap_or_default(),
        language,
        tags,
        chapters,
    })
}

/// The chapter list from `window.chapters` (and volume names from
/// `window.volumes`, when the author uses them).
fn chapters(html: &str, page_url: &str) -> Vec<ChapterRef> {
    let Some(list) = json_after(html, "window.chapters") else {
        return Vec::new();
    };
    let volumes: Vec<(i64, String)> = json_after(html, "window.volumes")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| Some((v.get("id")?.as_i64()?, v.get("title")?.as_str()?.to_string())))
        .collect();
    let base = url::Url::parse(page_url).ok();
    list.as_array()
        .into_iter()
        .flatten()
        .filter(|c| c.get("visible").and_then(Value::as_i64).unwrap_or(1) != 0)
        .filter_map(|c| {
            let link = c.get("url").and_then(Value::as_str)?;
            let url = base.as_ref().and_then(|b| b.join(link).ok()).map(|u| u.to_string()).unwrap_or_else(|| link.to_string());
            let title = c.get("title").and_then(Value::as_str).unwrap_or("Chapter").trim().to_string();
            // Paid early access until it's released to everyone (free
            // chapters say true; a tier list alone doesn't mean locked).
            let locked = c.get("isUnlocked").and_then(Value::as_bool) == Some(false);
            let volume = c
                .get("volumeId")
                .and_then(Value::as_i64)
                .and_then(|id| volumes.iter().find(|(v, _)| *v == id))
                .map(|(_, name)| name.clone());
            Some(ChapterRef { url, title, volume, locked })
        })
        .collect()
}

/// The JSON value assigned to `name` in a script (`name = [...];`).
fn json_after(html: &str, name: &str) -> Option<Value> {
    let at = html.find(name)? + name.len();
    let rest = html[at..].trim_start().strip_prefix('=')?.trim_start();
    let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
    stream.next()?.ok()
}

pub async fn chapter(url: &str) -> Result<RawChapter, String> {
    let html = fetch::text(url).await?;
    parse_chapter(&html)
}

pub fn parse_chapter(html: &str) -> Result<RawChapter, String> {
    let page = Html::parse_document(html);
    let content = Selector::parse("div.chapter-inner.chapter-content, div.chapter-inner").unwrap();
    let body = page.select(&content).next().map(|c| c.inner_html()).ok_or("No chapter text on the page")?;
    let note = Selector::parse(".author-note-portlet .author-note, div.author-note").unwrap();
    let notes = page.select(&note).map(|n| n.inner_html()).filter(|n| !n.trim().is_empty()).collect();
    Ok(RawChapter { body, notes, hidden_classes: clean::hidden_classes(&page) })
}
