//! Archive of Our Own (archiveofourown.org). A work's page has its details
//! (`h2.title`, `a[rel=author]`, the summary, tags, `dd.language[lang]`);
//! its chapter list is the work's navigate page (`ol.chapter.index`), and
//! a one-chapter work is its own chapter. A chapter's text is the
//! `div.userstuff` in `div#chapters` (less its "Chapter Text" heading), its
//! author notes the `div.notes` / `div.end.notes` there. Works marked for
//! adults ask first; `view_adult=true` answers. Works for registered users
//! only need the user's AO3 cookies.

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use scraper::{Html, Selector};
use std::sync::LazyLock;

static WORK_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^https?://(?:www\.)?archiveofourown\.org/works/(\d+)").unwrap());

/// A work, or a chapter of one.
pub fn is_book_url(url: &str) -> bool {
    WORK_RE.is_match(url.trim())
}

fn work_url(id: &str) -> String {
    format!("https://archiveofourown.org/works/{id}?view_adult=true")
}

pub async fn book(url: &str) -> Result<Book, String> {
    let id = WORK_RE.captures(url.trim()).map(|c| c[1].to_string()).ok_or("Not an AO3 work link")?;
    let page_url = work_url(&id);
    let html = fetch::text(&page_url).await?;
    let navigate = fetch::text(&format!("https://archiveofourown.org/works/{id}/navigate?view_adult=true")).await.unwrap_or_default();
    parse_book(&id, &page_url, &html, &navigate)
}

/// Everything from the work's page and its navigate page (for tests).
pub fn parse_book(id: &str, page_url: &str, html: &str, navigate: &str) -> Result<Book, String> {
    if html.contains("only available to registered users") {
        return Err("This work is for AO3's registered users: add your AO3 cookies in Settings → Downloads → Cookies".to_string());
    }
    let page = Html::parse_document(html);
    let first = |css: &str| {
        let selector = Selector::parse(css).ok()?;
        page.select(&selector).next()
    };
    let text_of = |el: scraper::ElementRef| el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");

    let title = first("h2.title.heading").map(text_of).filter(|t| !t.is_empty()).ok_or("No work on this page")?;
    let author_selector = Selector::parse("h3.byline a[rel=\"author\"]").unwrap();
    let authors: Vec<String> = page.select(&author_selector).map(text_of).filter(|a| !a.is_empty()).collect();
    let description = first("div.preface div.summary blockquote.userstuff")
        .map(|d| clean::clean(&d.inner_html(), &[], page_url, false))
        .filter(|d| !d.trim().is_empty());
    let tag_selector = Selector::parse("dd.fandom.tags a.tag, dd.freeform.tags a.tag").unwrap();
    let tags = page.select(&tag_selector).map(text_of).filter(|t| !t.is_empty()).collect();
    let language = first("dd.language").and_then(|d| d.attr("lang")).map(str::to_string);

    let mut chapters = chapter_list(navigate);
    if chapters.is_empty() {
        // A one-chapter work: the work's page is its chapter.
        chapters.push(ChapterRef { url: page_url.to_string(), title: title.clone(), volume: None, locked: false });
    }

    Ok(Book {
        site: Site::Ao3,
        id: id.to_string(),
        url: format!("https://archiveofourown.org/works/{id}"),
        title,
        author: (!authors.is_empty()).then(|| authors.join(", ")),
        description,
        // AO3 works have no covers.
        covers: Vec::new(),
        language,
        tags,
        chapters,
    })
}

/// The navigate page's chapters ("1. Title" links).
fn chapter_list(navigate: &str) -> Vec<ChapterRef> {
    let page = Html::parse_document(navigate);
    let selector = Selector::parse("ol.chapter.index li a").unwrap();
    page.select(&selector)
        .filter_map(|a| {
            let href = a.attr("href")?;
            let url = format!("https://archiveofourown.org{href}?view_adult=true");
            let text = a.text().collect::<String>();
            // "1. Title": the number's the list's own.
            let title = text.trim().split_once(". ").map(|(_, t)| t.trim()).unwrap_or(text.trim()).to_string();
            Some(ChapterRef { url, title, volume: None, locked: false })
        })
        .collect()
}

pub async fn chapter(url: &str) -> Result<RawChapter, String> {
    let html = fetch::text(url).await?;
    parse_chapter(&html)
}

pub fn parse_chapter(html: &str) -> Result<RawChapter, String> {
    let page = Html::parse_document(html);
    let content = Selector::parse("div#chapters div.userstuff").unwrap();
    let body = page.select(&content).next().map(|c| c.inner_html()).ok_or("No chapter text on the page")?;
    // The chapter's own notes, before and after it (the work's notes sit
    // outside #chapters).
    let note = Selector::parse("div#chapters div.notes blockquote.userstuff, div#chapters div.end.notes blockquote.userstuff").unwrap();
    let notes = page.select(&note).map(|n| n.inner_html()).filter(|n| !n.trim().is_empty()).collect();
    // Its "Chapter Text" heading is for screen readers.
    Ok(RawChapter { body, notes, hidden_classes: vec!["landmark".to_string()] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_links() {
        assert!(is_book_url("https://archiveofourown.org/works/19002013"));
        assert!(is_book_url("https://archiveofourown.org/works/19002013/chapters/45123823"));
        assert!(!is_book_url("https://archiveofourown.org/tags/Sherlock"));
    }

    #[test]
    fn chapters_and_text() {
        let navigate = r#"<ol class="chapter index group"><li><a href="/works/1/chapters/10">1. Start</a></li><li><a href="/works/1/chapters/11">2. Middle. Part</a></li></ol>"#;
        let list = chapter_list(navigate);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].title, "Middle. Part");
        assert_eq!(list[0].url, "https://archiveofourown.org/works/1/chapters/10?view_adult=true");
        let chapter = r#"<div class="preface group"><div class="notes"><blockquote class="userstuff">work note</blockquote></div></div>
            <div id="chapters"><div class="chapter"><div class="notes module"><blockquote class="userstuff"><p>before</p></blockquote></div>
            <div class="userstuff module" role="article"><h3 class="landmark heading">Chapter Text</h3><p>Hello</p></div>
            <div class="end notes module"><blockquote class="userstuff"><p>after</p></blockquote></div></div></div>"#;
        let raw = parse_chapter(chapter).unwrap();
        assert!(raw.body.contains("Hello"));
        assert_eq!(raw.notes.len(), 2);
        assert!(!raw.notes.iter().any(|n| n.contains("work note")));
    }
}
