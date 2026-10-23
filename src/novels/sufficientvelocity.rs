//! Sufficient Velocity (forums.sufficientvelocity.com), a XenForo forum where
//! stories are threads and their chapters are threadmarked posts. The
//! threadmarks page lists them (`.structItem--threadmark`, 200 a page when
//! asked); the thread's reader mode shows them in order, ten a page, so a
//! chapter is its post (`article[data-content="post-<id>"]`'s
//! `.bbWrapper`) on its reader page, and each page is fetched once for its
//! ten. Only the main threadmarks (the story, not its side stories or
//! informational posts) are listed.

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use scraper::{Html, Selector};
use std::sync::{LazyLock, Mutex};

static THREAD_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^https?://forums\.sufficientvelocity\.com/threads/(?:[^/]*\.)?(\d+)").unwrap()
});

const PER_PAGE: usize = 200;
/// Reader mode's posts per page.
const READER_PAGE: usize = 10;

/// A story thread (or a page or post of one).
pub fn is_book_url(url: &str) -> bool {
    THREAD_RE.is_match(url.trim())
}

fn thread_url(id: &str) -> String {
    format!("https://forums.sufficientvelocity.com/threads/{id}")
}

pub async fn book(url: &str) -> Result<Book, String> {
    let id = THREAD_RE.captures(url.trim()).map(|c| c[1].to_string()).ok_or("Not a Sufficient Velocity thread link")?;
    let base = thread_url(&id);
    // Threadmarks, a page at a time until one isn't full.
    let mut pages = Vec::new();
    for page in 1.. {
        let html = fetch::text(&format!("{base}/threadmarks?per_page={PER_PAGE}&page={page}")).await?;
        let full = count_threadmarks(&html) >= PER_PAGE;
        pages.push(html);
        if !full || page >= 50 {
            break;
        }
    }
    parse_book(&id, &pages)
}

/// The story from its threadmarks pages (for tests).
pub fn parse_book(id: &str, pages: &[String]) -> Result<Book, String> {
    let first = Html::parse_document(pages.first().ok_or("No threadmarks")?);
    let select = |css: &str| Selector::parse(css).unwrap();
    let title = first
        .select(&select("h1.p-title-value"))
        .next()
        // Its own text, not the prefix labels in front of it.
        .map(|h| h.children().filter_map(|c| c.value().as_text().map(|t| t.to_string())).collect::<String>())
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty())
        .ok_or("No story on this page")?;
    let meta = |property: &str| {
        first
            .select(&select(&format!("meta[property=\"{property}\"]")))
            .next()
            .and_then(|m| m.attr("content"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let base = thread_url(id);
    let mut chapters = Vec::new();
    let mut author = None;
    for page in pages {
        let html = Html::parse_document(page);
        for row in html.select(&select(".structItem--threadmark")) {
            if author.is_none() {
                author = row.attr("data-content-author").map(str::to_string);
            }
            let Some(link) = row.select(&select(".structItem-title a")).next() else {
                continue;
            };
            let Some(post) = link.attr("href").and_then(|h| h.rsplit_once("post-")).map(|(_, p)| p.to_string()) else {
                continue;
            };
            let index = chapters.len();
            chapters.push(ChapterRef {
                // Its reader page, and the post on it.
                url: format!("{base}/reader/page-{}#post-{post}", index / READER_PAGE + 1),
                title: link.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "),
                volume: None,
                locked: false,
            });
        }
    }
    if chapters.is_empty() {
        return Err("This thread has no threadmarked chapters".to_string());
    }
    Ok(Book {
        site: Site::SufficientVelocity,
        id: id.to_string(),
        url: base,
        title,
        author,
        description: meta("og:description").map(|d| format!("<p>{}</p>", clean::escape(&d))),
        // Threads have no covers (og:image is the forum's logo).
        covers: Vec::new(),
        language: Some("en".to_string()),
        tags: Vec::new(),
        chapters,
    })
}

fn count_threadmarks(html: &str) -> usize {
    let page = Html::parse_document(html);
    page.select(&Selector::parse(".structItem--threadmark").unwrap()).count()
}

/// The reader page fetched last, so its ten chapters cost one request.
static LAST_PAGE: Mutex<Option<(String, String)>> = Mutex::new(None);

pub async fn chapter(url: &str) -> Result<RawChapter, String> {
    let (page_url, post) = url.split_once("#post-").ok_or("Not a chapter link")?;
    let cached = LAST_PAGE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|(u, _)| u == page_url).map(|(_, h)| h.clone());
    let html = match cached {
        Some(html) => html,
        None => {
            let html = fetch::text(page_url).await?;
            *LAST_PAGE.lock().unwrap_or_else(|e| e.into_inner()) = Some((page_url.to_string(), html.clone()));
            html
        }
    };
    parse_chapter(&html, post)
}

/// Post `post`'s text on a reader page (for tests).
pub fn parse_chapter(html: &str, post: &str) -> Result<RawChapter, String> {
    let page = Html::parse_document(html);
    let selector = Selector::parse(&format!("article[data-content=\"post-{post}\"] .message-body .bbWrapper"))
        .map_err(|_| "Not a chapter link")?;
    let body = page.select(&selector).next().map(|b| b.inner_html()).ok_or("The chapter's post isn't on its page (it may have moved)")?;
    Ok(RawChapter { body, notes: Vec::new(), hidden_classes: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert!(is_book_url("https://forums.sufficientvelocity.com/threads/brocktons-celestial-forge-worm-jumpchain.70036/"));
        assert!(is_book_url("https://forums.sufficientvelocity.com/threads/brocktons-celestial-forge-worm-jumpchain.70036/threadmarks"));
        assert!(!is_book_url("https://forums.sufficientvelocity.com/forums/user-fiction.2/"));
    }

    #[test]
    fn threadmarks_and_posts() {
        let row = |n: usize| {
            format!(r#"<div class="structItem structItem--threadmark" data-content-author="Writer"><div class="structItem-title"><ul><li><a href="/threads/story.70036/#post-{n}">{n} Chapter</a></li></ul></div></div>"#)
        };
        let page = format!(
            r#"<h1 class="p-title-value"><span class="label">Prefix</span> A Story</h1><meta property="og:description" content="About it">{}"#,
            (1..=12).map(row).collect::<String>()
        );
        let book = parse_book("70036", &[page]).unwrap();
        assert_eq!(book.title, "A Story");
        assert_eq!(book.author.as_deref(), Some("Writer"));
        assert_eq!(book.chapters.len(), 12);
        assert_eq!(book.chapters[0].url, "https://forums.sufficientvelocity.com/threads/70036/reader/page-1#post-1");
        assert_eq!(book.chapters[11].url, "https://forums.sufficientvelocity.com/threads/70036/reader/page-2#post-12");
        let reader = r#"<article data-content="post-7"><div class="message-body"><div class="bbWrapper">Seven</div></div></article>"#;
        assert_eq!(parse_chapter(reader, "7").unwrap().body, "Seven");
    }
}
