//! The Books page: every book saved, one row each however many files it
//! came as (its first file and the parts added since). A book stays after
//! its downloads are deleted, struck out, to be saved again or removed, so
//! the page keeps a list of its own (the database's library_books) with
//! what it needs to show the book and save it again.

use crate::novels::Site;
use crate::{BookRow, DownloaderApp, Library};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Book {
    site: Site,
    id: String,
    url: String,
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    chapters: usize,
    /// When it was first saved (Unix seconds): the newest first on the page.
    #[serde(default)]
    added: u64,
}

fn key(site: Site, id: &str) -> String {
    format!("{}:{id}", site.key())
}

/// The books, loaded from the database the first time.
static BOOKS: Mutex<Option<Vec<Book>>> = Mutex::new(None);

fn with_books<T>(f: impl FnOnce(&mut Vec<Book>) -> T) -> T {
    let mut guard = BOOKS.lock().unwrap_or_else(|e| e.into_inner());
    let books = guard.get_or_insert_with(|| {
        crate::db::library_books().into_iter().filter_map(|(_, data)| serde_json::from_str(&data).ok()).collect()
    });
    f(books)
}

fn store(book: &Book) {
    if let Ok(data) = serde_json::to_string(book) {
        crate::db::save_library_book(&key(book.site, &book.id), &data);
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A book was saved (any thread): onto the page, or brought up to date.
pub fn remember(site: Site, id: &str, url: &str, title: &str, author: &str, chapters: usize) {
    remember_at(site, id, url, title, author, chapters, now());
}

fn remember_at(site: Site, id: &str, url: &str, title: &str, author: &str, chapters: usize, added: u64) {
    with_books(|books| {
        let book = match books.iter_mut().find(|b| b.site == site && b.id == id) {
            Some(book) => {
                book.url = url.to_string();
                book.title = title.to_string();
                if !author.is_empty() {
                    book.author = author.to_string();
                }
                book.chapters = chapters;
                book.clone()
            }
            None => {
                let book = Book {
                    site,
                    id: id.to_string(),
                    url: url.to_string(),
                    title: title.to_string(),
                    author: author.to_string(),
                    chapters,
                    added,
                };
                books.push(book.clone());
                book
            }
        };
        store(&book);
    });
}

thread_local! {
    static MODEL: Rc<VecModel<BookRow>> = Rc::new(VecModel::default());
    /// Covers loaded, by file and when it was written (a book saved again
    /// may bring a new one).
    static COVERS: std::cell::RefCell<HashMap<(PathBuf, SystemTime), slint::Image>> = std::cell::RefCell::new(HashMap::new());
    /// Books whose file was looked in for a cover and author (once a run).
    static TRIED: std::cell::RefCell<std::collections::HashSet<String>> = std::cell::RefCell::new(std::collections::HashSet::new());
}

/// A book saved before its cover and author were kept: both from its EPUB
/// (Eddy's own: OEBPS/images/cover.<ext>, dc:creator in content.opf). The
/// page shows them at its next refresh.
fn from_epub(site: Site, id: &str, path: &std::path::Path) {
    let Ok(file) = std::fs::File::open(path) else { return };
    let Ok(mut zip) = zip::ZipArchive::new(file) else { return };
    let read = |zip: &mut zip::ZipArchive<std::fs::File>, name: &str| -> Option<Vec<u8>> {
        let mut entry = zip.by_name(name).ok()?;
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut data).ok()?;
        Some(data)
    };
    if crate::novels::cover_file(site, id).is_none() {
        for ext in ["jpg", "png", "webp", "gif", "svg"] {
            if let Some(data) = read(&mut zip, &format!("OEBPS/images/cover.{ext}")) {
                crate::novels::keep_cover_as(site, id, &data, ext);
                break;
            }
        }
    }
    let author = read(&mut zip, "OEBPS/content.opf").and_then(|opf| {
        let opf = String::from_utf8_lossy(&opf);
        let start = opf.find("<dc:creator")?;
        let start = start + opf[start..].find('>')? + 1;
        let end = start + opf[start..].find("</dc:creator>")?;
        Some(unescape(opf[start..end].trim()))
    });
    if let Some(author) = author.filter(|a| !a.is_empty()) {
        with_books(|books| {
            if let Some(book) = books.iter_mut().find(|b| b.site == site && b.id == id && b.author.is_empty()) {
                book.author = author;
                store(book);
            }
        });
    }
}

/// XML's escapes, as clean::escape writes them.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn cover(site: Site, id: &str) -> slint::Image {
    let Some(path) = crate::novels::cover_file(site, id) else {
        return slint::Image::default();
    };
    let written = std::fs::metadata(&path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    COVERS.with(|covers| {
        covers
            .borrow_mut()
            .entry((path.clone(), written))
            .or_insert_with(|| slint::Image::load_from_path(&path).unwrap_or_default())
            .clone()
    })
}

/// Hooks the page up. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    let library = ui.global::<Library>();
    MODEL.with(|model| library.set_books(ModelRc::from(model.clone())));
    library.on_save_again(|wanted| {
        let url = with_books(|books| books.iter().find(|b| key(b.site, &b.id) == wanted.as_str()).map(|b| b.url.clone()));
        if let Some(url) = url {
            crate::links::add_links_from_text(&url);
        }
    });
    let ui_weak = ui.as_weak();
    library.on_remove(move |wanted| {
        let wanted = wanted.to_string();
        with_books(|books| books.retain(|b| key(b.site, &b.id) != wanted));
        crate::db::delete_library_book(&wanted);
        if let Some(ui) = ui_weak.upgrade() {
            refresh(&ui);
        }
    });
    refresh(ui);
}

/// The page's rows, from the books and their downloads as they are now
/// (with the lists, every second).
pub fn refresh(_ui: &DownloaderApp) {
    let downloads = crate::downloads::book_downloads();
    // Saved before the page kept its own list.
    for d in &downloads {
        if !with_books(|books| books.iter().any(|b| b.site == d.site && b.id == d.book_id)) {
            let title = if d.named.is_empty() { &d.title } else { &d.named };
            remember_at(d.site, &d.book_id, &d.url, title, &d.author, d.chapters, d.ended);
        }
    }
    // Saved before covers and authors were kept: both from the book's file.
    for d in &downloads {
        let Some(path) = &d.path else { continue };
        let missing = crate::novels::cover_file(d.site, &d.book_id).is_none()
            || with_books(|books| books.iter().any(|b| b.site == d.site && b.id == d.book_id && b.author.is_empty()));
        if missing && !path.starts_with("content://") && TRIED.with(|t| t.borrow_mut().insert(key(d.site, &d.book_id))) {
            let (site, id, path) = (d.site, d.book_id.clone(), PathBuf::from(path));
            std::thread::spawn(move || from_epub(site, &id, &path));
        }
    }
    let mut books = with_books(|books| books.clone());
    books.sort_by(|a, b| b.added.cmp(&a.added));
    let rows: Vec<BookRow> = books
        .iter()
        .map(|book| {
            let mine: Vec<_> = downloads.iter().filter(|d| d.site == book.site && d.book_id == book.id).collect();
            // The buttons act on its first file (the latest, if saved twice).
            let target = mine.iter().filter(|d| d.first).max_by_key(|d| d.ended).or_else(|| mine.iter().max_by_key(|d| d.ended));
            BookRow {
                key: key(book.site, &book.id).into(),
                title: book.title.as_str().into(),
                author: book.author.as_str().into(),
                size: match mine.iter().map(|d| d.size).sum::<u64>() {
                    0 => "".into(),
                    n => crate::links::format_size(n).into(),
                },
                site: book.site.label().into(),
                chapters: mine.iter().map(|d| d.chapters).max().unwrap_or(book.chapters) as i32,
                cover: cover(book.site, &book.id),
                deleted: mine.is_empty(),
                download_id: target.map_or(-1, |d| d.id as i32),
                checking: mine.iter().any(|d| d.checking),
            }
        })
        .collect();
    MODEL.with(|model| {
        if model.row_count() != rows.len() || model.iter().zip(&rows).any(|(a, b)| a != *b) {
            model.set_vec(rows);
        }
    });
}
