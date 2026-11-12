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
pub fn remember(site: Site, id: &str, url: &str, title: &str, chapters: usize) {
    remember_at(site, id, url, title, chapters, now());
}

fn remember_at(site: Site, id: &str, url: &str, title: &str, chapters: usize, added: u64) {
    with_books(|books| {
        let book = match books.iter_mut().find(|b| b.site == site && b.id == id) {
            Some(book) => {
                book.url = url.to_string();
                book.title = title.to_string();
                book.chapters = chapters;
                book.clone()
            }
            None => {
                let book = Book { site, id: id.to_string(), url: url.to_string(), title: title.to_string(), chapters, added };
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
            remember_at(d.site, &d.book_id, &d.url, title, d.chapters, d.ended);
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
