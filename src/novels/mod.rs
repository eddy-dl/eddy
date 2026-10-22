//! Web novels from public reading sites, saved as EPUB books (see
//! docs/novel-extractors.md). A site says which chapters a book has and
//! where a chapter's text is; fetching, cleaning, images and packing the
//! book are shared.

pub mod clean;
pub mod epub;
pub mod fetch;
pub mod images;
pub mod royalroad;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The reading sites Eddy knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Site {
    RoyalRoad,
}

impl Site {
    pub fn label(self) -> &'static str {
        match self {
            Site::RoyalRoad => "Royal Road",
        }
    }

    /// Short and stable: the chapter cache's folder name.
    fn key(self) -> &'static str {
        match self {
            Site::RoyalRoad => "royalroad",
        }
    }
}

/// The site a book link is on, if Eddy can read it.
pub fn site_for(url: &str) -> Option<Site> {
    royalroad::is_book_url(url).then_some(Site::RoyalRoad)
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
}

impl Default for NovelOptions {
    fn default() -> Self {
        Self { from: 1, to: 0, author_notes: true, images: true, author_note_images: false, small_images: false }
    }
}

/// Reads a book's details and chapter list.
pub async fn book(url: &str) -> Result<Book, String> {
    match site_for(url).ok_or("Not a book link Eddy can read")? {
        Site::RoyalRoad => royalroad::book(url).await,
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

/// The chapters `options` picks, locked ones left out.
pub fn wanted<'a>(book: &'a Book, options: &NovelOptions) -> Vec<&'a ChapterRef> {
    let last = if options.to == 0 { book.chapters.len() } else { options.to.min(book.chapters.len()) };
    let first = options.from.max(1);
    book.chapters
        .iter()
        .enumerate()
        .filter(|(i, c)| *i + 1 >= first && *i + 1 <= last && !c.locked)
        .map(|(_, c)| c)
        .collect()
}

/// Saves the book as `<dir>/<name>.epub`, reporting (chapters done, total).
/// Returns the file's path.
pub async fn download(
    url: &str,
    options: &NovelOptions,
    dir: &Path,
    name: Option<&str>,
    progress: impl Fn(usize, usize),
) -> Result<PathBuf, String> {
    let book = book(url).await?;
    let chapters = wanted(&book, options);
    if chapters.is_empty() {
        return Err("No chapters to save".to_string());
    }
    let total = chapters.len();
    progress(0, total);
    let mut texts = Vec::with_capacity(total);
    for (i, c) in chapters.iter().enumerate() {
        texts.push((*c, chapter(&book, c, options).await?));
        progress(i + 1, total);
    }

    // Images: each once, smaller or as a still where asked.
    let mut collected = images::Collector::new(options.small_images);
    let mut packed = Vec::with_capacity(texts.len());
    for (c, xhtml) in texts {
        let xhtml = if options.images { collected.rewrite(&xhtml).await } else { xhtml };
        packed.push(epub::Chapter { title: c.title.clone(), volume: c.volume.clone(), xhtml });
    }
    let mut cover = None;
    for url in &book.covers {
        if let Ok(image) = images::fetch(url).await {
            cover = Some(image);
            break;
        }
    }

    let file_name = format!("{}.epub", clean::safe_file_name(name.unwrap_or(&default_name(&book))));
    let path = dir.join(file_name);
    let bytes = epub::pack(&book, &packed, cover, collected.into_images())?;
    tokio::fs::write(&path, bytes).await.map_err(|e| e.to_string())?;
    Ok(path)
}

/// "<title> - <author>".
pub fn default_name(book: &Book) -> String {
    match &book.author {
        Some(author) => format!("{} - {author}", book.title),
        None => book.title.clone(),
    }
}
