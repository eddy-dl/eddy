//! FanFiction.net (www.fanfiction.net) and FictionPress (www.fictionpress.com),
//! one engine. A story's chapter page has its details in `#profile_top`
//! (title, author, summary, cover, and a grey line of "Rated: … - English -
//! … - Chapters: N - …"), its chapters in the `#chap_select` dropdown (none
//! for a one-chapter story), and its text in `#storytext`. Chapter n is
//! `/s/<id>/<n>/`. Both sit behind Cloudflare's check, passed in a WebView
//! (see fetch).

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use scraper::{ElementRef, Html, Selector};
use std::sync::LazyLock;

static STORY_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^https?://(?:www\.|m\.)?(fanfiction\.net|fictionpress\.com)/s/(\d+)").unwrap()
});

/// The site a story link (or a chapter of one, on the full or the mobile
/// site) is on.
pub fn site_for(url: &str) -> Option<Site> {
    match &STORY_RE.captures(url.trim())?[1] {
        "fictionpress.com" => Some(Site::FictionPress),
        _ => Some(Site::FanFiction),
    }
}

fn story_url(site: Site, id: &str) -> String {
    let host = if site == Site::FictionPress { "www.fictionpress.com" } else { "www.fanfiction.net" };
    format!("https://{host}/s/{id}")
}

pub async fn book(url: &str) -> Result<Book, String> {
    let site = site_for(url).ok_or("Not a story link")?;
    let id = STORY_RE.captures(url.trim()).map(|c| c[2].to_string()).ok_or("Not a story link")?;
    // The first chapter's page: the story's details are on every chapter's.
    let html = fetch::text(&format!("{}/1/", story_url(site, &id))).await?;
    parse_book(site, &id, &html)
}

/// The story from a chapter's page (for tests).
pub fn parse_book(site: Site, id: &str, html: &str) -> Result<Book, String> {
    let page = Html::parse_document(html);
    let select = |css: &str| Selector::parse(css).unwrap();
    let first = |css: &str| page.select(&select(css)).next();
    let text = |e: ElementRef| e.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    let base = story_url(site, id);

    let title = first("#profile_top b.xcontrast_txt").map(text).filter(|t| !t.is_empty()).ok_or("No story on this page (or the page changed)")?;
    let author = first("#profile_top a[href^=\"/u/\"]").map(text).filter(|a| !a.is_empty());
    let description = first("#profile_top div.xcontrast_txt").map(text).filter(|d| !d.is_empty()).map(|d| format!("<p>{}</p>", clean::escape(&d)));
    // The grey line: "Rated: Fiction T - English - Romance - Chapters: 3 - …".
    let facts: Vec<String> = first("#profile_top span.xgray").map(text).map(|l| l.split(" - ").map(str::to_string).collect()).unwrap_or_default();
    let language = facts.get(1).and_then(|l| language_code(l)).map(str::to_string);
    // The fandom (FanFiction.net) or genre (FictionPress): the last link of
    // the breadcrumb above the story.
    let tags = page.select(&select("#pre_story_links a")).last().map(text).filter(|t| !t.is_empty()).into_iter().collect();
    // Covers are served at 75 px wide in the page, and at 180 as well.
    let page_url = url::Url::parse(&base).ok();
    let covers = first("#profile_top img.cimage")
        .and_then(|img| img.attr("data-original").or_else(|| img.attr("src")))
        .and_then(|src| page_url.as_ref()?.join(src).ok())
        .map(|small| {
            let small = small.to_string();
            let large = small.replace("/75/", "/180/");
            if large != small { vec![large, small] } else { vec![small] }
        })
        .unwrap_or_default();

    // The dropdown is on the page twice (above and below the text).
    let mut chapters: Vec<ChapterRef> = Vec::new();
    if let Some(dropdown) = first("select#chap_select") {
        for option in dropdown.select(&select("option")) {
            let Some(n) = option.attr("value").and_then(|v| v.trim().parse::<usize>().ok()) else {
                continue;
            };
            // "12. Its name": the number's already the chapter's place.
            let name = text(option);
            let name = name.split_once(". ").filter(|(number, _)| number.parse::<usize>().is_ok()).map_or(name.as_str(), |(_, rest)| rest).to_string();
            chapters.push(ChapterRef { url: format!("{base}/{n}/"), title: name, volume: None, locked: false });
        }
    }
    if chapters.is_empty() && first("#storytext").is_some() {
        chapters.push(ChapterRef { url: format!("{base}/1/"), title: title.clone(), volume: None, locked: false });
    }
    if chapters.is_empty() {
        return Err("This story has no chapters (or the page changed)".to_string());
    }
    Ok(Book { site, id: id.to_string(), url: base, title, author, description, covers, language, tags, chapters })
}

/// The language names the grey line uses, as codes for the book.
fn language_code(name: &str) -> Option<&'static str> {
    Some(match name.trim() {
        "English" => "en",
        "Spanish" => "es",
        "French" => "fr",
        "German" => "de",
        "Portuguese" => "pt",
        "Italian" => "it",
        "Dutch" => "nl",
        "Polish" => "pl",
        "Russian" => "ru",
        "Indonesian" => "id",
        "Tagalog" | "Filipino" => "tl",
        "Swedish" => "sv",
        "Norwegian" => "no",
        "Danish" => "da",
        "Finnish" => "fi",
        "Turkish" => "tr",
        "Czech" => "cs",
        "Hungarian" => "hu",
        "Romanian" => "ro",
        "Vietnamese" => "vi",
        "Chinese" => "zh",
        "Japanese" => "ja",
        "Korean" => "ko",
        _ => return None,
    })
}

pub async fn chapter(url: &str) -> Result<RawChapter, String> {
    let html = fetch::text(url).await?;
    parse_chapter(&html)
}

pub fn parse_chapter(html: &str) -> Result<RawChapter, String> {
    let page = Html::parse_document(html);
    let body = page.select(&Selector::parse("#storytext").unwrap()).next().map(|b| b.inner_html()).ok_or("No chapter text on the page")?;
    Ok(RawChapter { body, notes: Vec::new(), hidden_classes: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert_eq!(site_for("https://www.fanfiction.net/s/5782108/1/Harry-Potter-and-the-Methods-of-Rationality"), Some(Site::FanFiction));
        assert_eq!(site_for("https://m.fanfiction.net/s/5782108/12/"), Some(Site::FanFiction));
        assert_eq!(site_for("https://www.fictionpress.com/s/2961893/1/Mother-of-Learning"), Some(Site::FictionPress));
        assert_eq!(site_for("https://www.fanfiction.net/u/2269863/Less-Wrong"), None);
    }

    const PAGE: &str = r#"<div id="pre_story_links"><span class="lc-left"><a href="/book/">Books</a> » <a href="/book/Harry-Potter/">Harry Potter</a></span></div>
<div id="profile_top"><span><img class="cimage" src="/image/1234/75/" width="75" height="100"></span>
<b class="xcontrast_txt">A  Story</b> <span class="xcontrast_txt">By:</span> <a class="xcontrast_txt" href="/u/42/Writer">Writer</a>
<div class="xcontrast_txt" style="margin-top:2px">What it's <b>about</b>.</div>
<span class="xgray xcontrast_txt">Rated: <a>Fiction T</a> - English - Drama - Chapters: 3 - Words: 9,000</span></div>
<select id="chap_select"><option value="1" selected>1. Beginnings</option><option value="2">2. Middles</option><option value="3">3. 3. Ends</option></select>
<div class="storytext xcontrast_txt nocopy" id="storytext"><p>Once.</p></div>
<select id="chap_select"><option value="1" selected>1. Beginnings</option><option value="2">2. Middles</option><option value="3">3. 3. Ends</option></select>"#;

    #[test]
    fn story_and_chapters() {
        let book = parse_book(Site::FanFiction, "5782108", PAGE).unwrap();
        assert_eq!(book.title, "A Story");
        assert_eq!(book.author.as_deref(), Some("Writer"));
        assert_eq!(book.description.as_deref(), Some("<p>What it's about.</p>"));
        assert_eq!(book.language.as_deref(), Some("en"));
        assert_eq!(book.tags, vec!["Harry Potter".to_string()]);
        assert_eq!(book.covers, vec!["https://www.fanfiction.net/image/1234/180/".to_string(), "https://www.fanfiction.net/image/1234/75/".to_string()]);
        assert_eq!(book.chapters.len(), 3);
        assert_eq!(book.chapters[1].url, "https://www.fanfiction.net/s/5782108/2/");
        assert_eq!(book.chapters[1].title, "Middles");
        assert_eq!(book.chapters[2].title, "3. Ends");
        assert_eq!(parse_chapter(PAGE).unwrap().body, "<p>Once.</p>");
    }

    #[test]
    fn one_chapter() {
        let page = PAGE.replace("<select id=\"chap_select\">", "<div>").replace("</select>", "</div>");
        let book = parse_book(Site::FictionPress, "1", &page).unwrap();
        assert_eq!(book.chapters.len(), 1);
        assert_eq!(book.chapters[0].url, "https://www.fictionpress.com/s/1/1/");
        assert_eq!(book.chapters[0].title, "A Story");
    }
}
