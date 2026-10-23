//! Wattpad (wattpad.com), through the JSON its own site uses: a story's
//! details and parts from `api/v3/stories/<id>`, a part's text (plain
//! paragraphs) from `apiv2/storytext?id=<part>`. Links to a part
//! (`wattpad.com/<part id>-<slug>`) are looked up for their story first.
//! Drafts and deleted parts aren't listed. Paid parts (Wattpad's paywall)
//! only give their opening, so they're listed as locked and skipped.

use super::{Book, ChapterRef, RawChapter, Site, clean, fetch};
use serde_json::Value;
use std::sync::LazyLock;

static STORY_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^https?://(?:www\.|m\.)?wattpad\.com/story/(\d+)").unwrap());
static PART_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^https?://(?:www\.|m\.)?wattpad\.com/(\d+)(?:-[^/?#]*)?(?:[/?#]|$)").unwrap());

/// A story, or one of its parts.
pub fn is_book_url(url: &str) -> bool {
    let url = url.trim();
    STORY_RE.is_match(url) || PART_RE.is_match(url)
}

const STORY_FIELDS: &str =
    "id,title,url,description,cover,language(name),tags,isPaywalled,user(name),parts(id,title,url,draft,deleted,isBlocked)";

pub async fn book(url: &str) -> Result<Book, String> {
    let url = url.trim();
    let id = match STORY_RE.captures(url) {
        Some(c) => c[1].to_string(),
        None => {
            // A part: its story.
            let part = PART_RE.captures(url).map(|c| c[1].to_string()).ok_or("Not a Wattpad story link")?;
            let json = fetch::text(&format!("https://www.wattpad.com/api/v3/story_parts/{part}?fields=groupId")).await?;
            let value: Value = serde_json::from_str(&json).map_err(|_| "Wattpad didn't say which story this is")?;
            value.get("groupId").and_then(|g| g.as_str().map(str::to_string).or_else(|| g.as_i64().map(|n| n.to_string())))
                .ok_or("Wattpad didn't say which story this is")?
        }
    };
    let json = fetch::text(&format!("https://www.wattpad.com/api/v3/stories/{id}?fields={STORY_FIELDS}")).await?;
    parse_book(&id, &json)
}

/// The story from its API answer (for tests).
pub fn parse_book(id: &str, json: &str) -> Result<Book, String> {
    let story: Value = serde_json::from_str(json).map_err(|_| "Wattpad's answer wasn't readable")?;
    if let Some(message) = story.get("message").and_then(Value::as_str).filter(|_| story.get("error_code").is_some()) {
        return Err(format!("Wattpad: {message}"));
    }
    let text = |key: &str| story.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let title = text("title").ok_or("No story here")?;
    let paywalled = story.get("isPaywalled").and_then(Value::as_bool).unwrap_or(false);
    let chapters: Vec<ChapterRef> = story
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| !p.get("draft").and_then(Value::as_bool).unwrap_or(false) && !p.get("deleted").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|p| {
            let part_id = p.get("id").and_then(|i| i.as_i64().map(|n| n.to_string()).or_else(|| i.as_str().map(str::to_string)))?;
            Some(ChapterRef {
                // The text's own address; the cache knows chapters by it.
                url: format!("https://www.wattpad.com/apiv2/storytext?id={part_id}"),
                title: p.get("title").and_then(Value::as_str).unwrap_or("Part").trim().to_string(),
                volume: None,
                // On a paid story, blocked parts are the paid ones.
                locked: paywalled && p.get("isBlocked").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    if chapters.is_empty() {
        return Err("This story has no published parts".to_string());
    }
    let description = text("description").map(|d| {
        d.split("\n\n").filter(|p| !p.trim().is_empty()).map(|p| format!("<p>{}</p>", clean::escape(p.trim()).replace('\n', "<br/>"))).collect::<String>()
    });
    let tags = story.get("tags").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    Ok(Book {
        site: Site::Wattpad,
        id: id.to_string(),
        url: text("url").unwrap_or_else(|| format!("https://www.wattpad.com/story/{id}")),
        title,
        author: story.get("user").and_then(|u| u.get("name")).and_then(Value::as_str).map(str::to_string),
        description,
        // The API's cover is 256 wide; the same image comes at 512 too.
        covers: text("cover")
            .map(|c| {
                let large = c.replace("-256-", "-512-");
                if large != c { vec![large, c] } else { vec![c] }
            })
            .unwrap_or_default(),
        language: story.get("language").and_then(|l| l.get("name")).and_then(Value::as_str).map(language_code),
        tags,
        chapters,
    })
}

/// The language's code for the book, from Wattpad's English name for it.
fn language_code(name: &str) -> String {
    match name {
        "English" => "en",
        "Français" | "French" => "fr",
        "Español" | "Spanish" => "es",
        "Português" | "Portuguese" => "pt",
        "Deutsch" | "German" => "de",
        "Italiano" | "Italian" => "it",
        "Bahasa Indonesia" | "Indonesian" => "id",
        "Filipino" | "Tagalog" => "tl",
        "Türkçe" | "Turkish" => "tr",
        "Русский" | "Russian" => "ru",
        "Tiếng Việt" | "Vietnamese" => "vi",
        "العربية" | "Arabic" => "ar",
        _ => "und",
    }
    .to_string()
}

pub async fn chapter(url: &str) -> Result<RawChapter, String> {
    let body = fetch::text(url).await?;
    if body.trim().is_empty() {
        return Err("Wattpad gave no text for this part".to_string());
    }
    // Plain paragraphs, no author notes apart from the text.
    Ok(RawChapter { body, notes: Vec::new(), hidden_classes: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert!(is_book_url("https://www.wattpad.com/story/26327373-the-dragon-and-the-princess"));
        assert!(is_book_url("https://www.wattpad.com/80847228-the-dragon-and-the-princess-chapter-1"));
        assert!(!is_book_url("https://www.wattpad.com/user/someone"));
    }

    #[test]
    fn story() {
        let json = r#"{"id":"26327373","title":"The Dragon","language":{"name":"English"},"user":{"name":"Author"},
            "description":"One.\n\nTwo & three","cover":"https://img.wattpad.com/cover/26327373-256-k228682.jpg","tags":["dragon"],
            "parts":[{"id":1,"title":"Declaration","draft":false},{"id":2,"title":"Draft","draft":true},{"id":3,"title":"Chapter 1","deleted":false}]}"#;
        let book = parse_book("26327373", json).unwrap();
        assert_eq!(book.chapters.len(), 2);
        assert_eq!(book.chapters[1].url, "https://www.wattpad.com/apiv2/storytext?id=3");
        assert_eq!(book.covers[0], "https://img.wattpad.com/cover/26327373-512-k228682.jpg");
        assert_eq!(book.language.as_deref(), Some("en"));
        assert_eq!(book.description.as_deref(), Some("<p>One.</p><p>Two &amp; three</p>"));
    }
}
