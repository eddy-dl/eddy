//! Web novels from public reading sites, saved as EPUB books (see
//! docs/novel-extractors.md). A site says which chapters a book has and
//! where a chapter's text is; fetching, cleaning, images and packing the
//! book are shared.

pub mod clean;
pub mod epub;
pub mod fetch;
pub mod ao3;
pub mod fanfiction;
pub mod images;
pub mod royalroad;
pub mod wattpad;
pub mod wuxiaworld;
pub mod xenforo;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The reading sites Eddy knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Site {
    RoyalRoad,
    Ao3,
    Wattpad,
    SufficientVelocity,
    Wuxiaworld,
    SpaceBattles,
    FanFiction,
    FictionPress,
}

impl Site {
    pub fn label(self) -> &'static str {
        match self {
            Site::RoyalRoad => "Royal Road",
            Site::Ao3 => "AO3",
            Site::Wattpad => "Wattpad",
            Site::SufficientVelocity => "Sufficient Velocity",
            Site::Wuxiaworld => "Wuxiaworld",
            Site::SpaceBattles => "SpaceBattles",
            Site::FanFiction => "FanFiction.net",
            Site::FictionPress => "FictionPress",
        }
    }

    /// Short and stable: the chapter cache's folder name.
    fn key(self) -> &'static str {
        match self {
            Site::RoyalRoad => "royalroad",
            Site::Ao3 => "ao3",
            Site::Wattpad => "wattpad",
            Site::SufficientVelocity => "sufficientvelocity",
            Site::Wuxiaworld => "wuxiaworld",
            Site::SpaceBattles => "spacebattles",
            Site::FanFiction => "fanfiction",
            Site::FictionPress => "fictionpress",
        }
    }
}

/// The site a book link is on, if Eddy can read it.
pub fn site_for(url: &str) -> Option<Site> {
    if royalroad::is_book_url(url) {
        Some(Site::RoyalRoad)
    } else if ao3::is_book_url(url) {
        Some(Site::Ao3)
    } else if wattpad::is_book_url(url) {
        Some(Site::Wattpad)
    } else if wuxiaworld::is_book_url(url) {
        Some(Site::Wuxiaworld)
    } else {
        xenforo::site_for(url).or_else(|| fanfiction::site_for(url))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Book {
    pub site: Site,
    /// The site's id for the book (stable across title changes).
    pub id: String,
    pub url: String,
    pub title: String,
    pub author: Option<String>,
    /// Cleaned XHTML.
    pub description: Option<String>,
    /// Cover image URLs, largest first: the book takes the first one that
    /// loads, the preview's banner the last (smallest).
    pub covers: Vec<String>,
    pub language: Option<String>,
    pub tags: Vec<String>,
    pub chapters: Vec<ChapterRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChapterRef {
    pub url: String,
    pub title: String,
    /// Starts a new section in the table of contents.
    pub volume: Option<String>,
    /// Paid / early access: listed, never fetched.
    pub locked: bool,
}

/// A chapter as the site has it, before cleaning.
pub struct RawChapter {
    /// The chapter's text.
    pub body: String,
    /// Author notes, apart from the text, so they can be dropped or kept.
    pub notes: Vec<String>,
    /// Class names the page's CSS hides (watermarks).
    pub hidden_classes: Vec<String>,
}

/// What a book download asks for (the preview's options; defaults from
/// Settings → Downloads → Books).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NovelOptions {
    /// Chapters to save, 1-based and inclusive; 0 for "to the last".
    pub from: usize,
    pub to: usize,
    pub author_notes: bool,
    pub images: bool,
    /// Images inside author notes (banners, "support me" buttons).
    pub author_note_images: bool,
    /// Ask image hosts for smaller versions.
    pub small_images: bool,
    /// Chapters picked one by one (1-based), in place of `from`..`to`;
    /// empty for the range.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chapters: Vec<usize>,
    /// The book's title in place of the site's (in the EPUB, and the file's
    /// name when it has none of its own); empty for the site's.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// New chapters (Check for new chapters) come as a part of their own,
    /// a file beside the book, rather than the book being made again: a
    /// reader that knows books by their contents keeps its place.
    pub parts: bool,
    /// Which part this is (2 on); 0 for the book itself.
    #[serde(skip_serializing_if = "is_zero")]
    pub part: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl Default for NovelOptions {
    fn default() -> Self {
        Self { from: 1, to: 0, author_notes: true, images: true, author_note_images: false, small_images: false, chapters: Vec::new(), title: String::new(), parts: true, part: 0 }
    }
}

/// Reads a book's details and chapter list.
pub async fn book(url: &str) -> Result<Book, String> {
    match site_for(url).ok_or("Not a book link Eddy can read")? {
        Site::RoyalRoad => royalroad::book(url).await,
        Site::Ao3 => ao3::book(url).await,
        Site::Wattpad => wattpad::book(url).await,
        Site::SufficientVelocity | Site::SpaceBattles => xenforo::book(url).await,
        Site::FanFiction | Site::FictionPress => fanfiction::book(url).await,
        Site::Wuxiaworld => wuxiaworld::book(url).await,
    }
}

/// A chapter, cleaned to XHTML with its images listed (cached, so an
/// interrupted download picks up where it was and updates only fetch new
/// chapters).
async fn chapter(book: &Book, chapter: &ChapterRef, options: &NovelOptions) -> Result<String, String> {
    let cache = cache_file(book, chapter);
    let raw = match std::fs::read_to_string(&cache).ok().and_then(|s| serde_json::from_str::<CachedChapter>(&s).ok()) {
        Some(cached) => cached,
        None => {
            let raw = match book.site {
                Site::RoyalRoad => royalroad::chapter(&chapter.url).await?,
                Site::Ao3 => ao3::chapter(&chapter.url).await?,
                Site::Wattpad => wattpad::chapter(&chapter.url).await?,
                Site::SufficientVelocity | Site::SpaceBattles => xenforo::chapter(&chapter.url).await?,
                Site::FanFiction | Site::FictionPress => fanfiction::chapter(&chapter.url).await?,
                Site::Wuxiaworld => wuxiaworld::chapter(&chapter.url).await?,
            };
            let cached = CachedChapter { body: raw.body, notes: raw.notes, hidden_classes: raw.hidden_classes };
            if let Some(dir) = cache.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&cache, serde_json::to_string(&cached).unwrap_or_default());
            cached
        }
    };
    let mut xhtml = clean::clean(&raw.body, &raw.hidden_classes, &chapter.url, options.images);
    if options.author_notes {
        for note in &raw.notes {
            let note = clean::clean(note, &raw.hidden_classes, &chapter.url, options.images && options.author_note_images);
            if !note.trim().is_empty() {
                xhtml += &format!("<aside class=\"author-note\">{note}</aside>");
            }
        }
    }
    Ok(xhtml)
}

/// The chapter cache keeps the site's HTML, not the cleaned result, so
/// changing the options doesn't mean fetching again.
#[derive(Serialize, Deserialize)]
struct CachedChapter {
    body: String,
    notes: Vec<String>,
    hidden_classes: Vec<String>,
}

fn cache_file(book: &Book, chapter: &ChapterRef) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    chapter.url.hash(&mut hasher);
    PathBuf::from(crate::platform::get_app_data_dir())
        .join("novels")
        .join(book.site.key())
        .join(clean::safe_component(&book.id))
        .join(format!("{:016x}.json", hasher.finish()))
}

/// The chapters `options` picks (one by one, else its range), locked ones
/// left out.
pub fn wanted<'a>(book: &'a Book, options: &NovelOptions) -> Vec<&'a ChapterRef> {
    let last = if options.to == 0 { book.chapters.len() } else { options.to.min(book.chapters.len()) };
    let first = options.from.max(1);
    let picked = |n: usize| if options.chapters.is_empty() { n >= first && n <= last } else { options.chapters.contains(&n) };
    book.chapters.iter().enumerate().filter(|(i, c)| picked(*i + 1) && !c.locked).map(|(_, c)| c).collect()
}

/// A saved book, for the library: which book it is, and how many chapters
/// it had then (new ones are counted from it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedBook {
    pub site: Site,
    pub id: String,
    pub chapters: usize,
    /// Its title on the site (the book may have been given another).
    #[serde(default)]
    pub title: String,
    /// The last chapter saved (1-based; 0 from before it was kept): new
    /// ones are those after it.
    #[serde(default)]
    pub last: usize,
}

/// Saves the book as `<dir>/<name>.epub`, reporting (chapters done, total).
/// Returns the file's path and the book.
pub async fn download(
    url: &str,
    options: &NovelOptions,
    dir: &Path,
    name: Option<&str>,
    progress: impl Fn(usize, usize),
) -> Result<(PathBuf, SavedBook), String> {
    let mut book = book(url).await?;
    let site_title = book.title.clone();
    if !options.title.trim().is_empty() {
        book.title = options.title.trim().to_string();
    }
    let chapters = wanted(&book, options);
    if chapters.is_empty() {
        return Err("No chapters to save".to_string());
    }
    let last = book.chapters.iter().rposition(|c| chapters.iter().any(|w| std::ptr::eq(*w, c))).map_or(0, |i| i + 1);
    // The cover first: a book that has one is never saved without it.
    let cover = cover(&book).await?;
    let total = chapters.len();
    // Chapters saved before (in the cache) count from the start, so a book
    // carried on with shows what it has straight away.
    let mut done = chapters.iter().filter(|c| cache_file(&book, c).exists()).count();
    progress(done, total);
    let mut texts = Vec::with_capacity(total);
    for c in &chapters {
        let cached = cache_file(&book, c).exists();
        texts.push((*c, chapter(&book, c, options).await?));
        if !cached {
            done += 1;
            progress(done, total);
        }
    }

    // Images: each once, smaller or as a still where asked.
    let mut collected = images::Collector::new(options.small_images);
    let mut packed = Vec::with_capacity(texts.len());
    for (c, xhtml) in texts {
        let xhtml = if options.images { collected.rewrite(&xhtml).await } else { xhtml };
        packed.push(epub::Chapter { title: c.title.clone(), volume: c.volume.clone(), xhtml });
    }

    let file_name = format!("{}.epub", clean::safe_file_name(name.unwrap_or(&default_name(&book))));
    let path = dir.join(file_name);
    let series = options.parts.then_some(options.part.max(1));
    let bytes = epub::pack(&book, &packed, cover, collected.into_images(), series)?;
    tokio::fs::write(&path, bytes).await.map_err(|e| e.to_string())?;
    Ok((path, SavedBook { site: book.site, id: book.id.clone(), chapters: book.chapters.len(), title: site_title, last }))
}

/// The book's cover (the first of its covers that comes, each tried a few
/// times); None only when it has none. Err when it has one that won't come.
async fn cover(book: &Book) -> Result<Option<(Vec<u8>, String)>, String> {
    if book.covers.is_empty() {
        return Ok(None);
    }
    let mut last = String::new();
    for url in &book.covers {
        for attempt in 0..3u64 {
            match images::fetch(url).await {
                Ok(image) => return Ok(Some(image)),
                Err(e) => {
                    log::warn!(target: "books", "cover {url}: {e}");
                    last = e;
                }
            }
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_secs(2 << attempt)).await;
            }
        }
    }
    Err(format!("Couldn't fetch the book's cover: {last}"))
}

/// "<title> - <author>".
pub fn default_name(book: &Book) -> String {
    match &book.author {
        Some(author) => format!("{} - {author}", book.title),
        None => book.title.clone(),
    }
}
