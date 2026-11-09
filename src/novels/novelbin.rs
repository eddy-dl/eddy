//! NovelPing (novelping.com) and NovelBin (novelbin.me): one site engine
//! (NovelFull's, as WebToEpub's NovelfullParser has it). A book's page
//! carries its details as meta tags (`og:novel:*`) and its description in
//! `div.desc-text`, but only its first chapters; the whole list is the page
//! its script loads, `/ajax/chapter-archive?novelId=<slug>`
//! (`li[data-chapter-item]` or `ul.list-chapter`, a link's title its
//! chapter's, as its text has the comment count too). A chapter's text is
//! `div#chr-content`, with ad slots, and on some sites a watermark sentence
//! the page's script takes out (`original11Content.replace("…"`).

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use scraper::{Html, Selector};
use std::sync::LazyLock;

/// A book's link (or a chapter's): its site and slug.
static BOOK_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^https?://(?:www\.)?(novelping\.com/(?:book|novel)|novelbin\.me/(?:novel-book|novel|b))/([A-Za-z0-9_-]+)").unwrap()
});

/// Elements that are ads or buttons, not the chapter.
const NOT_TEXT: [&str; 2] = ["js-ad-slot", "unlock-buttons"];

/// The site a book (or chapter) link is on, if it's one of these.
pub fn site_for(url: &str) -> Option<Site> {
    let captures = BOOK_RE.captures(url.trim())?;
    Some(if captures[1].starts_with("novelping") { Site::NovelPing } else { Site::NovelBin })
}

/// Where a site keeps its books, and its name in page titles.
fn origin(site: Site) -> (&'static str, &'static str) {
    match site {
        Site::NovelBin => ("https://novelbin.me/novel-book", "NovelBin"),
        _ => ("https://novelping.com/book", "NovelPing"),
    }
}

pub async fn book(url: &str) -> Result<Book, String> {
    let site = site_for(url).ok_or("Not a NovelPing or NovelBin book link")?;
    let slug = BOOK_RE.captures(url.trim()).map(|c| c[2].to_string()).ok_or("Not a book link")?;
    let (books, _) = origin(site);
    let page_url = format!("{books}/{slug}");
    let html = fetch::text(&page_url).await?;
    let host = url::Url::parse(books).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    // Without it, the page's own first chapters.
    let archive = fetch::text(&format!("https://{host}/ajax/chapter-archive?novelId={slug}"))
        .await
        .inspect_err(|e| log::warn!("{slug}'s chapter archive: {e}"))
        .unwrap_or_default();
    parse_book(site, &slug, &page_url, &html, &archive)
}

/// The book from its page and its chapter archive (for tests).
pub fn parse_book(site: Site, slug: &str, page_url: &str, html: &str, archive: &str) -> Result<Book, String> {
    let (_, name) = origin(site);
    let page = Html::parse_document(html);
    let meta = |property: &str| {
        let selector = Selector::parse(&format!("meta[property=\"{property}\"], meta[name=\"{property}\"]")).ok()?;
        page.select(&selector).next().and_then(|m| m.attr("content")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let first = |css: &str| {
        let selector = Selector::parse(css).ok()?;
        page.select(&selector).next()
    };
    let text_of = |css: &str| first(css).map(|e| e.text().collect::<String>().trim().to_string()).filter(|t| !t.is_empty());
    let title = meta("og:novel:novel_name")
        .or_else(|| text_of("h3.title"))
        .or_else(|| meta("og:title").map(|t| t.trim_end_matches(name).trim_end_matches(['|', '-', ' ']).to_string()))
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            log::warn!("{page_url}: no book on the page ({} characters, titled {:?}; saved as unread-page.html beside the logs)", html.len(), fetch::title_of(html));
            crate::logs::save("unread-page.html", html);
            "No book on this page".to_string()
        })?;
    let mut chapters = chapter_list(archive);
    if chapters.is_empty() {
        chapters = chapter_list(html);
    }
    if chapters.is_empty() {
        return Err("This book has no chapters (or the page changed)".to_string());
    }
    let description = first("#novel-description-content")
        .or_else(|| first("div.desc-text"))
        .map(|d| clean::clean(&d.inner_html(), &[], page_url, false))
        .filter(|d| !d.trim().is_empty());
    let author = meta("og:novel:author").or_else(|| {
        let item = Selector::parse("ul.info-meta li, ul.info li").unwrap();
        let link = Selector::parse("a").unwrap();
        page.select(&item)
            .find(|li| li.text().collect::<String>().trim_start().starts_with("Author"))
            .and_then(|li| li.select(&link).next())
            .map(|a| a.text().collect::<String>().trim().to_string())
    });
    let tags = meta("og:novel:genre")
        .map(|g| g.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default();
    let base = url::Url::parse(page_url).ok();
    let cover = meta("og:image").or_else(|| {
        let src = first("div.book img")?.attr("data-src").or_else(|| first("div.book img")?.attr("src"))?.to_string();
        base.as_ref()?.join(&src).ok().map(|u| u.to_string())
    });
    Ok(Book {
        site,
        id: slug.to_string(),
        url: meta("og:url").unwrap_or_else(|| page_url.to_string()),
        title,
        author,
        description,
        covers: cover.into_iter().collect(),
        language: Some("en".to_string()),
        tags,
        chapters,
    })
}

/// The chapters listed on a page, in order, each once.
fn chapter_list(html: &str) -> Vec<ChapterRef> {
    let page = Html::parse_document(html);
    let item = Selector::parse("li[data-chapter-item] a[href], ul.list-chapter li a[href]").unwrap();
    let mut seen = std::collections::HashSet::new();
    page.select(&item)
        .filter_map(|a| {
            let url = a.attr("href")?.trim().to_string();
            if !seen.insert(url.clone()) {
                return None;
            }
            // The link's text has the comment count too.
            let title = a.attr("title").map(str::to_string).unwrap_or_else(|| a.text().collect());
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
    let content = Selector::parse("div#chr-content, div#chapter-content").unwrap();
    let mut body = page.select(&content).next().map(|c| c.inner_html()).ok_or("No chapter text on the page")?;
    if let Some(mark) = watermark(html) {
        body = body.replace(&mark, "");
    }
    let mut hidden_classes = clean::hidden_classes(&page);
    hidden_classes.extend(NOT_TEXT.iter().map(|c| c.to_string()));
    Ok(RawChapter { body, notes: Vec::new(), hidden_classes })
}

/// The sentence the page's script takes out of the text, if any.
fn watermark(html: &str) -> Option<String> {
    const TOKEN: &str = "original11Content.replace(\"";
    let rest = &html[html.find(TOKEN)? + TOKEN.len()..];
    Some(rest[..rest.find('"')?].to_string()).filter(|m| !m.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEM: &str = r#"<li data-chapter-item data-chapter-id="chapter-{n}" class="chapter-list-item">
        <span class="glyphicon glyphicon-certificate chapter-list-bullet"></span>
        <a href="https://novelping.com/book/my-slain-dragon-bride/chapter-{n}" title="Chapter {n}">
            <span class="nchr-text chapter-title">Chapter {n}</span> <span class="chapter-comment-count">3</span>
        </a></li>"#;

    fn items(n: usize) -> String {
        (1..=n).map(|i| ITEM.replace("{n}", &i.to_string())).collect()
    }

    #[test]
    fn links() {
        assert_eq!(site_for("https://novelping.com/book/my-slain-dragon-bride/"), Some(Site::NovelPing));
        assert_eq!(site_for("https://novelping.com/book/my-slain-dragon-bride/chapter-2"), Some(Site::NovelPing));
        assert_eq!(site_for("https://novelbin.me/novel-book/shadow-slave"), Some(Site::NovelBin));
        assert_eq!(site_for("https://novelbin.me/novel-book/shadow-slave/chapter-1"), Some(Site::NovelBin));
        assert_eq!(site_for("https://novelping.com/author/Apexnovelhub"), None);
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
        let url = "https://novelping.com/book/my-slain-dragon-bride";
        let book = parse_book(Site::NovelPing, "my-slain-dragon-bride", url, &html, &archive).unwrap();
        assert_eq!(book.title, "My Slain Dragon Bride");
        assert_eq!(book.author.as_deref(), Some("Apexnovelhub"));
        assert_eq!(book.tags, ["ACTION", "ROMANCE"]);
        assert!(book.description.unwrap().contains("The pinnacle of magic."));
        assert_eq!(book.chapters.len(), 3);
        assert_eq!(book.chapters[2].url, "https://novelping.com/book/my-slain-dragon-bride/chapter-3");
        assert_eq!(book.chapters[2].title, "Chapter 3");
        // No archive: the page's own.
        assert_eq!(parse_book(Site::NovelPing, "x", url, &html, "").unwrap().chapters.len(), 2);
    }

    #[test]
    fn book_without_meta_tags() {
        let html = r#"<html><body><div class="book"><img src="/media/novel/x.jpg"></div>
            <h3 class="title">Shadow Slave</h3>
            <ul class="info info-meta"><li><h3>Author:</h3><a href="/a/guiltythree">Guiltythree</a></li></ul>
            <ul class="list-chapter"><li><a href="https://novelbin.me/novel-book/shadow-slave/chapter-1" title="Chapter 1: Nightmare Begins">Chapter 1</a></li></ul>
            </body></html>"#;
        let book = parse_book(Site::NovelBin, "shadow-slave", "https://novelbin.me/novel-book/shadow-slave", html, "").unwrap();
        assert_eq!(book.title, "Shadow Slave");
        assert_eq!(book.author.as_deref(), Some("Guiltythree"));
        assert_eq!(book.covers, ["https://novelbin.me/media/novel/x.jpg"]);
        assert_eq!(book.chapters[0].title, "Chapter 1: Nightmare Begins");
    }

    #[test]
    fn chapter_text_without_ads_or_watermark() {
        let html = r#"<div id="chr-content" class="chr-c" data-chapter-id="chapter-2"><p>Have I Returned Again?
</p><div class="js-ad-slot">Ad</div><p> "My younger brother?" Find it on novelbin.</p></div><hr class="chr-end">
<script>let a = original11Content.replace(" Find it on novelbin.", "");</script>"#;
        let raw = parse_chapter(html).unwrap();
        assert!(raw.body.contains("My younger brother?"));
        assert!(!raw.body.contains("Find it on novelbin"));
        assert!(!raw.body.contains("chr-end"));
        assert!(raw.hidden_classes.iter().any(|c| c == "js-ad-slot"));
    }
}
