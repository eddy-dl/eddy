//! Ranobes (ranobes.top, ranobes.net: one site; ranobes.com is a Russian
//! one of its own). A book's page is `/novels/<id>-<slug>.html`: title
//! `h1.title` (with a "by author" subtitle), author a `/tags/authors/`
//! link, description `div.moreless__full`. Its table of contents,
//! `/chapters/<id>/`, is paged (`page/<n>/`), each page's chapters (newest
//! first) and the page count in `window.__DATA__`. A chapter
//! (`/<slug>-<id>/<chapter>.html`) has its text in `div#arrticle`. It's
//! behind Cloudflare, so its pages mostly come through the browser (see
//! fetch), and it's read slowly, as it turns fast readers away.

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use scraper::{Html, Selector};
use serde_json::Value;
use std::sync::LazyLock;

/// A book's page, its table of contents, or a chapter: the site and the
/// book's id.
static BOOK_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^https?://(?:www\.)?(ranobes\.(?:top|net))/(?:novels/(\d+)-[^/]*\.html|chapters/(\d+)(?:/|$)|[A-Za-z0-9_-]+-(\d+)/\d+\.html)").unwrap()
});

/// No more table of contents pages than this (25 chapters a page).
const MAX_PAGES: u64 = 400;

pub fn is_book_url(url: &str) -> bool {
    BOOK_RE.is_match(url.trim())
}

/// The site's address and the book's id.
fn parts(url: &str) -> Option<(String, String)> {
    let c = BOOK_RE.captures(url.trim())?;
    let id = c.get(2).or(c.get(3)).or(c.get(4))?.as_str().to_string();
    Some((format!("https://{}", &c[1]), id))
}

pub async fn book(url: &str) -> Result<Book, String> {
    let (site, id) = parts(url).ok_or("Not a Ranobes book link")?;
    // A chapter's or the contents' link: the book's page is linked from it.
    let page_url = if url.contains("/novels/") {
        url.trim().to_string()
    } else {
        let html = fetch::text(url.trim()).await?;
        book_link(&html, &site, &id).ok_or("No link to the book on this page")?
    };
    let html = fetch::text(&page_url).await?;
    let toc = format!("{site}/chapters/{id}/");
    let first = fetch::text(&toc).await?;
    let (mut chapters, pages) = toc_page(&first, &site);
    for n in 2..=pages.min(MAX_PAGES) {
        let page = fetch::text(&format!("{toc}page/{n}/")).await?;
        chapters.extend(toc_page(&page, &site).0);
    }
    parse_book(&id, &page_url, &html, chapters)
}

/// The book's page, as a page of it links it.
fn book_link(html: &str, site: &str, id: &str) -> Option<String> {
    let page = Html::parse_document(html);
    let link = Selector::parse("a[href]").unwrap();
    let prefix = format!("/novels/{id}-");
    page.select(&link).filter_map(|a| a.attr("href")).find(|h| h.contains(&prefix)).map(|h| {
        if h.starts_with("http") { h.to_string() } else { format!("{site}{h}") }
    })
}

/// A contents page's chapters (newest first, as listed) and the page count.
fn toc_page(html: &str, site: &str) -> (Vec<ChapterRef>, u64) {
    let Some(data) = json_after(html, "window.__DATA__") else {
        return (Vec::new(), 0);
    };
    let pages = data.get("pages_count").and_then(|p| p.as_u64().or_else(|| p.as_str()?.parse().ok())).unwrap_or(1);
    let chapters = data
        .get("chapters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let link = c.get("link").and_then(Value::as_str)?;
            let url = if link.starts_with('/') { format!("{site}{link}") } else { link.to_string() };
            let title = c.get("title").and_then(Value::as_str).unwrap_or("Chapter").trim().to_string();
            Some(ChapterRef { url, title, volume: None, locked: false })
        })
        .collect();
    (chapters, pages)
}

/// The JSON value assigned to `name` in a script (`name = {...};`).
fn json_after(html: &str, name: &str) -> Option<Value> {
    let at = html.find(name)? + name.len();
    let rest = html[at..].trim_start().strip_prefix('=')?.trim_start();
    serde_json::Deserializer::from_str(rest).into_iter::<Value>().next()?.ok()
}

/// The book from its page and its chapters, newest first (for tests).
pub fn parse_book(id: &str, page_url: &str, html: &str, mut chapters: Vec<ChapterRef>) -> Result<Book, String> {
    let page = Html::parse_document(html);
    let meta = |property: &str| {
        let selector = Selector::parse(&format!("meta[property=\"{property}\"]")).ok()?;
        page.select(&selector).next().and_then(|m| m.attr("content")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let first = |css: &str| {
        let selector = Selector::parse(css).ok()?;
        page.select(&selector).next()
    };
    // The title without its "by author" subtitle.
    let title = first("h1.title")
        .map(|h| {
            h.children()
                .filter_map(|n| n.value().as_text().map(|t| String::from(&**t)))
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.is_empty())
        .or_else(|| meta("og:title"))
        .ok_or_else(|| {
            log::warn!("{page_url}: no book on the page ({} characters, titled {:?}; saved as unread-page.html beside the logs)", html.len(), fetch::title_of(html));
            crate::logs::save("unread-page.html", html);
            "No book on this page".to_string()
        })?;
    // Oldest first, each once.
    chapters.reverse();
    let mut seen = std::collections::HashSet::new();
    chapters.retain(|c| seen.insert(c.url.clone()));
    if chapters.is_empty() {
        return Err("This book has no chapters (or the page changed)".to_string());
    }
    let author = first("a[href*=\"/tags/authors/\"]").map(|a| a.text().collect::<String>().trim().to_string()).filter(|a| !a.is_empty());
    let description = first("div.moreless__full")
        .map(|d| clean::clean(&d.inner_html(), &[], page_url, false))
        .filter(|d| !d.trim().is_empty());
    let genre = Selector::parse("#mc-fs-genre a").unwrap();
    let tags = page.select(&genre).map(|a| a.text().collect::<String>().trim().to_string()).filter(|t| !t.is_empty()).collect();
    let language = first("html").and_then(|h| h.attr("lang")).map(str::to_string);
    Ok(Book {
        site: Site::Ranobes,
        id: id.to_string(),
        url: page_url.to_string(),
        title,
        author,
        description,
        covers: meta("og:image").into_iter().collect(),
        language,
        tags,
        chapters,
    })
}

pub async fn chapter(url: &str) -> Result<RawChapter, String> {
    let html = fetch::text(url).await?;
    parse_chapter(&html)
}

pub fn parse_chapter(html: &str) -> Result<RawChapter, String> {
    let page = Html::parse_document(html);
    let content = Selector::parse("div#arrticle").unwrap();
    let body = page.select(&content).next().map(|c| c.inner_html()).ok_or("No chapter text on the page")?;
    Ok(RawChapter { body, notes: Vec::new(), hidden_classes: clean::hidden_classes(&page) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        let book = "https://ranobes.top/novels/976776-mother-of-learning-v812312.html";
        assert_eq!(parts(book), Some(("https://ranobes.top".to_string(), "976776".to_string())));
        assert_eq!(parts("https://ranobes.net/chapters/976776/").map(|p| p.1).as_deref(), Some("976776"));
        assert_eq!(parts("https://ranobes.top/mother-of-learning-v812312-976776/1461971.html").map(|p| p.1).as_deref(), Some("976776"));
        assert!(!is_book_url("https://ranobes.com/ranobe/123-x.html"));
        assert!(!is_book_url("https://ranobes.top/tags/genre/Action/"));
    }

    #[test]
    fn book_and_contents() {
        let html = r#"<html lang="en"><head><meta property="og:title" content="Mother of Learning">
            <meta property="og:image" content="https://ranobes.top/uploads/posts/2021-06/1624640386_mol.jpg"></head><body>
            <h1 class="title">
                Mother of Learning <span class="subtitle">by nobody103</span>
            </h1>
            <div class="moreless__full">Zorian is a teenage mage.</div>
            <li>Authors: <span class="tag_list"><a href="https://ranobes.top/tags/authors/nobody103/">nobody103</a></span></li>
            <div id="mc-fs-genre"><a href="/tags/genre/Action/">Action</a>, <a href="/tags/genre/Fantasy/">Fantasy</a></div>
            </body></html>"#;
        let toc = r#"<script>window.__DATA__ = {"chapters":[{"id":2,"title":"Chapter 2","link":"https://ranobes.top/mother-of-learning-v812312-976776/2.html"},{"id":1,"title":"Chapter 1","link":"https://ranobes.top/mother-of-learning-v812312-976776/1.html"}],"pages_count":3};</script>"#;
        let (chapters, pages) = toc_page(toc, "https://ranobes.top");
        assert_eq!(pages, 3);
        let book = parse_book("976776", "https://ranobes.top/novels/976776-mother-of-learning-v812312.html", html, chapters).unwrap();
        assert_eq!(book.title, "Mother of Learning");
        assert_eq!(book.author.as_deref(), Some("nobody103"));
        assert_eq!(book.tags, ["Action", "Fantasy"]);
        assert_eq!(book.language.as_deref(), Some("en"));
        assert_eq!(book.chapters[0].title, "Chapter 1");
        assert_eq!(book.chapters[1].url, "https://ranobes.top/mother-of-learning-v812312-976776/2.html");
    }

    #[test]
    fn book_link_from_a_chapter() {
        let html = r#"<a href="/novels/976776-mother-of-learning-v812312.html">Mother of Learning</a>"#;
        assert_eq!(book_link(html, "https://ranobes.top", "976776").as_deref(), Some("https://ranobes.top/novels/976776-mother-of-learning-v812312.html"));
    }

    #[test]
    fn chapter_text() {
        let raw = parse_chapter(r#"<div id="arrticle"><p>Zorian's eyes abruptly shot open.</p></div>"#).unwrap();
        assert!(raw.body.contains("Zorian"));
    }
}
