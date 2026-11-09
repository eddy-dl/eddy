//! NovelPing (novelping.com). A book's page carries its details as meta
//! tags (`og:novel:*`) and its description in `#novel-description-content`,
//! but only the first 30 chapters; the whole list is the page its script
//! loads, `/ajax/chapter-archive?novelId=<slug>` (`li[data-chapter-item]`).
//! A chapter's text is `div#chr-content`.

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use scraper::{Html, Selector};
use std::sync::LazyLock;

static BOOK_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^https?://(?:www\.)?novelping\.com/book/([A-Za-z0-9_-]+)").unwrap());

/// A book, or a chapter of one.
pub fn is_book_url(url: &str) -> bool {
    BOOK_RE.is_match(url.trim())
}

pub async fn book(url: &str) -> Result<Book, String> {
    let slug = BOOK_RE.captures(url.trim()).map(|c| c[1].to_string()).ok_or("Not a NovelPing book link")?;
    let page_url = format!("https://novelping.com/book/{slug}/");
    let html = fetch::text(&page_url).await?;
    let archive = fetch::text(&format!("https://novelping.com/ajax/chapter-archive?novelId={slug}")).await?;
    parse_book(&slug, &page_url, &html, &archive)
}

/// The book from its page and its chapter archive (for tests).
pub fn parse_book(slug: &str, page_url: &str, html: &str, archive: &str) -> Result<Book, String> {
    let page = Html::parse_document(html);
    let meta = |property: &str| {
        let selector = Selector::parse(&format!("meta[property=\"{property}\"], meta[name=\"{property}\"]")).ok()?;
        page.select(&selector).next().and_then(|m| m.attr("content")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let title = meta("og:novel:novel_name")
        .or_else(|| meta("og:title").map(|t| t.trim_end_matches("| NovelPing").trim().to_string()))
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            log::warn!("{page_url}: no book on the page ({} characters, titled {:?}; saved as unread-page.html beside the logs)", html.len(), fetch::title_of(html));
            crate::logs::save("unread-page.html", html);
            "No book on this page".to_string()
        })?;
    // The archive, else the page's own first chapters.
    let mut chapters = chapter_list(archive);
    if chapters.is_empty() {
        chapters = chapter_list(html);
    }
    if chapters.is_empty() {
        return Err("This book has no chapters (or the page changed)".to_string());
    }
    let description_selector = Selector::parse("#novel-description-content, div.desc-text").unwrap();
    let description = page
        .select(&description_selector)
        .next()
        .map(|d| clean::clean(&d.inner_html(), &[], page_url, false))
        .filter(|d| !d.trim().is_empty());
    let tags = meta("og:novel:genre")
        .map(|g| g.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default();
    Ok(Book {
        site: Site::NovelPing,
        id: slug.to_string(),
        url: meta("og:url").unwrap_or_else(|| page_url.to_string()),
        title,
        author: meta("og:novel:author"),
        description,
        covers: meta("og:image").into_iter().collect(),
        language: Some("en".to_string()),
        tags,
        chapters,
    })
}

/// The chapters listed on a page, in order, each once.
fn chapter_list(html: &str) -> Vec<ChapterRef> {
    let page = Html::parse_document(html);
    let item = Selector::parse("li[data-chapter-item] a[href]").unwrap();
    let mut seen = std::collections::HashSet::new();
    page.select(&item)
        .filter_map(|a| {
            let url = a.attr("href")?.trim().to_string();
            if !seen.insert(url.clone()) {
                return None;
            }
            let text = a.text().collect::<String>();
            let title = a.attr("title").map(str::to_string).unwrap_or(text);
            let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
            Some(ChapterRef { url, title: if title.is_empty() { "Chapter".to_string() } else { title }, volume: None, locked: false })
        })
        .collect()
}

pub async fn chapter(url: &str) -> Result<RawChapter, String> {
    let html = fetch::text(url).await?;
    parse_chapter(&html)
}

pub fn parse_chapter(html: &str) -> Result<RawChapter, String> {
    let page = Html::parse_document(html);
    let content = Selector::parse("div#chr-content, div.chr-c").unwrap();
    let body = page.select(&content).next().map(|c| c.inner_html()).ok_or("No chapter text on the page")?;
    Ok(RawChapter { body, notes: Vec::new(), hidden_classes: clean::hidden_classes(&page) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEM: &str = r#"<li data-chapter-item data-chapter-id="chapter-{n}" class="chapter-list-item">
        <span class="glyphicon glyphicon-certificate chapter-list-bullet"></span>
        <a href="https://novelping.com/book/my-slain-dragon-bride/chapter-{n}" title="Chapter {n}">
            <span class="nchr-text chapter-title">Chapter {n}</span>
        </a></li>"#;

    fn items(n: usize) -> String {
        (1..=n).map(|i| ITEM.replace("{n}", &i.to_string())).collect()
    }

    #[test]
    fn links() {
        assert!(is_book_url("https://novelping.com/book/my-slain-dragon-bride/"));
        assert!(is_book_url("https://novelping.com/book/my-slain-dragon-bride/chapter-2"));
        assert!(!is_book_url("https://novelping.com/author/Apexnovelhub"));
    }

    #[test]
    fn book_from_its_page_and_archive() {
        let html = format!(
            r#"<html><head>
            <meta property="og:title" content="My Slain Dragon Bride | NovelPing">
            <meta property="og:url" content="https://novelping.com/book/my-slain-dragon-bride/">
            <meta property="og:image" content="https://images.novelping.com/novel/my-slain-dragon-bride.jpg">
            <meta property="og:novel:genre" content="ACTION,ROMANCE">
            <meta property="og:novel:author" content="Apexnovelhub">
            <meta property="og:novel:novel_name" content="My Slain Dragon Bride">
            </head><body><div class="desc-text" id="novel-description-content"><p>The pinnacle of magic.</p></div>
            <template data-first-chapter-template>{}</template></body></html>"#,
            items(2)
        );
        let archive = format!("<html><body><template data-chapter-item-template>{}{}</template></body></html>", items(3), items(1));
        let book = parse_book("my-slain-dragon-bride", "https://novelping.com/book/my-slain-dragon-bride/", &html, &archive).unwrap();
        assert_eq!(book.title, "My Slain Dragon Bride");
        assert_eq!(book.author.as_deref(), Some("Apexnovelhub"));
        assert_eq!(book.tags, ["ACTION", "ROMANCE"]);
        assert!(book.description.unwrap().contains("The pinnacle of magic."));
        assert_eq!(book.chapters.len(), 3);
        assert_eq!(book.chapters[2].url, "https://novelping.com/book/my-slain-dragon-bride/chapter-3");
        assert_eq!(book.chapters[2].title, "Chapter 3");
        // No archive: the page's own.
        assert_eq!(parse_book("x", "https://novelping.com/book/x/", &html, "").unwrap().chapters.len(), 2);
    }

    #[test]
    fn chapter_text() {
        let html = r#"<div id="chr-content" class="chr-c" data-chapter-id="chapter-2"><p>Have I Returned Again?
</p><p> "My younger brother?"</p></div><hr class="chr-end">"#;
        let raw = parse_chapter(html).unwrap();
        assert!(raw.body.contains("My younger brother?"));
        assert!(!raw.body.contains("chr-end"));
    }
}
