//! The downloads lists: every download Eddy started, kept in the database,
//! with its state (downloading, queued, paused, failed, cancelled, done)
//! and, while it runs, its live progress from aria2 or its job. The lists'
//! actions (pause, resume, cancel, delete, open, rename) act on these. Only
//! so many run at once (Settings); the others are Queued, oldest first.
//!
//! On Android this lives in the main process, where the download service
//! runs too; the share popup's downloads reach it through the service.

use crate::downloader::{self, Backend, DownloadRequest, Handle};
use crate::{
    DownloadItem, DownloadState, DownloaderApp, FileFilter, FileType, FolderFile, FolderFiles, ItemAction, LinkBackend,
    ListOptions, SelectionAction, SortKey, TabListOptions,
};
use slint::ComponentHandle;
use serde::{Deserialize, Serialize};
use slint::{Model, ModelRc, VecModel};
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Where a download is at, as saved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    #[default]
    Active,
    Queued,
    Paused,
    Failed,
    Cancelled,
    Completed,
}

/// What the running work says right now (downloader::progress).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Live {
    Active,
    Queued,
    Paused,
    Done,
    Stopped,
}

/// One download's record, as saved (JSON in the database).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub request: DownloadRequest,
    pub name: String,
    /// FileType's index (see file_type / file_type_index).
    pub file_type: i32,
    pub state: State,
    pub done: u64,
    pub total: u64,
    /// Why it failed.
    pub message: String,
    /// The saved file, once done (None when it went into a picked folder).
    pub path: Option<String>,
    /// Where it's saved, as shown.
    pub folder: String,
    /// Unix seconds.
    pub added: u64,
    pub ended: u64,
    /// A new name for an unfinished download, given to the file once it's
    /// done.
    pub rename_to: Option<String>,
    /// A web novel's book, once saved: the library (see `saved_book`).
    pub book: Option<crate::novels::SavedBook>,
    /// A folder picked for an unfinished download (as `request.folder`
    /// takes it): its files move there once it's done (`apply_move`).
    pub move_to: Option<String>,
    /// What an unfinished download has written (`note_written`), removed
    /// if it's cancelled or deleted with its files.
    pub partial: Vec<String>,
    /// Done, it saved a folder (a playlist, a post, a torrent's files):
    /// `path` is the folder.
    pub is_folder: bool,
    /// It's several files (so they go in a folder): known from the
    /// request, or seen while it ran.
    pub several: bool,
    /// A finished folder's files when it finished (paths inside it, sizes),
    /// so ones moved or deleted since can be shown as missing.
    pub files: Vec<(String, u64)>,
}

/// A record and what's running it.
struct Entry {
    id: i64,
    saved: Saved,
    handle: Option<Handle>,
    /// aria2 downloads it was restarted away from, whose "removed" isn't a
    /// failure.
    retired: Vec<Handle>,
    /// For a speed where the work doesn't report one: (bytes, when).
    last: Option<(u64, Instant)>,
    speed: f64,
    /// With several files: which one it's on, and the whole's progress.
    part: Option<(String, f32)>,
}

static ENTRIES: Mutex<Option<Vec<Entry>>> = Mutex::new(None);

/// How many downloads run at once (Settings → Downloads → Downloading); 0
/// for no limit. The rest are Queued until one ends.
static MAX_CONCURRENT: AtomicUsize = AtomicUsize::new(crate::preferences::DEFAULT_MAX_CONCURRENT as usize);

/// The entries, loaded from the database the first time. Downloads that
/// were running when the app last stopped are paused now.
fn with_entries<T>(f: impl FnOnce(&mut Vec<Entry>) -> T) -> T {
    let mut guard = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
    let entries = guard.get_or_insert_with(|| {
        crate::db::downloads()
            .into_iter()
            .filter_map(|(id, data)| {
                let mut saved: Saved = serde_json::from_str(&data).ok()?;
                saved.request.id = id;
                if matches!(saved.state, State::Active | State::Queued) {
                    saved.state = State::Paused;
                }
                // Saved when torrents had a type of their own (6): what's in
                // them instead.
                if saved.file_type == 6 {
                    saved.file_type = file_type_index(guess_type(&saved.request));
                    saved.several |= several_files(&saved.request);
                }
                Some(Entry { id, saved, handle: None, retired: Vec::new(), last: None, speed: 0.0, part: None })
            })
            .collect()
    });
    f(entries)
}

fn save(entry: &Entry) {
    if entry.id > 0 {
        crate::db::update_download(entry.id, &serde_json::to_string(&entry.saved).unwrap_or_default());
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A new download's record; returns its id (0 without a database).
pub fn register(request: &DownloadRequest) -> i64 {
    let saved = Saved {
        request: request.clone(),
        name: downloader::label_for(request),
        file_type: file_type_index(guess_type(request)),
        state: State::Active,
        folder: folder_display(request),
        added: now(),
        several: several_files(request),
        ..Default::default()
    };
    let id = crate::db::insert_download(&serde_json::to_string(&saved).unwrap_or_default());
    let mut saved = saved;
    saved.request.id = id;
    if id > 0 {
        with_entries(|entries| entries.push(Entry { id, saved, handle: None, retired: Vec::new(), last: None, speed: 0.0, part: None }));
    }
    id
}

// --- The queue ---

/// A new limit; more may start now.
pub fn set_max_concurrent(max: usize) {
    if MAX_CONCURRENT.swap(max, Ordering::SeqCst) != max {
        pump();
    }
}

/// Whether `id` may run alongside the others running.
fn has_slot(entries: &[Entry], id: i64) -> bool {
    let max = MAX_CONCURRENT.load(Ordering::SeqCst);
    max == 0 || entries.iter().filter(|e| e.id != id && e.saved.state == State::Active).count() < max
}

/// Whether `request`'s download may start now. If not, it's Queued until
/// `pump` starts it. Its record takes the request: a saved book being
/// updated comes back with the preview's options.
pub fn take_slot(request: &DownloadRequest) -> bool {
    with_entries(|entries| {
        let free = has_slot(entries, request.id);
        if let Some(entry) = entries.iter_mut().find(|e| e.id == request.id) {
            let state = if free { State::Active } else { State::Queued };
            let same_request = serde_json::to_string(&entry.saved.request).ok() == serde_json::to_string(request).ok();
            if entry.saved.state != state || !same_request {
                if entry.saved.state == State::Completed {
                    // Saved again (a book's update): back in Downloading.
                    entry.saved.message.clear();
                    entry.saved.ended = 0;
                }
                entry.saved.state = state;
                entry.saved.request = request.clone();
                // A pending move shows where it's going instead.
                if entry.saved.move_to.is_none() {
                    entry.saved.folder = folder_display(request);
                }
                save(entry);
            }
        }
        free
    })
}

// --- Books ---

/// A book download saved its book.
pub fn book_saved(id: i64, book: crate::novels::SavedBook) {
    with_entries(|entries| {
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.saved.book = Some(book);
            save(entry);
        }
    });
}

/// The download a book was saved by, if it was: (its id, how many chapters
/// the book had then, the file's name without .epub).
pub fn saved_book(site: crate::novels::Site, book_id: &str) -> Option<(i64, usize, String)> {
    with_entries(|entries| {
        entries
            .iter()
            .filter(|e| e.saved.state == State::Completed)
            .filter_map(|e| {
                let book = e.saved.book.as_ref().filter(|b| b.site == site && b.id == book_id)?;
                Some((e.id, book.chapters, e.saved.name.trim_end_matches(".epub").to_string(), e.saved.ended))
            })
            // The latest, if it was saved more than once.
            .max_by_key(|(.., ended)| *ended)
            .map(|(id, chapters, name, _)| (id, chapters, name))
    })
}

/// "Check for new chapters" on a saved book: fetches its chapter list and,
/// with new chapters, saves it again in its place (the chapters it had
/// come from the cache). Runs in the background; says how it went.
fn update_book(id: i64) {
    let found = with_entries(|entries| {
        let entry = entries.iter().find(|e| e.id == id && e.saved.state == State::Completed)?;
        // Saved before Eddy kept count: everything counts as new (the cache
        // still has the chapters).
        Some((entry.saved.request.clone(), entry.saved.book.as_ref().map(|b| b.chapters)))
    });
    let Some((request, chapters)) = found else {
        return;
    };
    crate::platform::show_toast("Checking for new chapters…");
    downloader::runtime().spawn(async move {
        let result = crate::novels::book(&request.url).await;
        let _ = slint::invoke_from_event_loop(move || {
            let book = match result {
                Ok(book) => book,
                Err(e) => return crate::platform::show_toast(&format!("Couldn't check: {e}")),
            };
            let new = book.chapters.len().saturating_sub(chapters.unwrap_or(0));
            if new == 0 {
                return crate::platform::show_toast("No new chapters");
            }
            let message = match (chapters, new) {
                (None, _) => "Updating".to_string(),
                (_, 1) => "1 new chapter: updating".to_string(),
                _ => format!("{new} new chapters: updating"),
            };
            crate::platform::show_toast(&message);
            let mut request = request;
            // Through to the last chapter, wherever it started.
            if let Ok(mut options) = serde_json::from_str::<crate::novels::NovelOptions>(&request.format) {
                options.to = 0;
                request.format = serde_json::to_string(&options).unwrap_or(request.format);
            }
            if let Err(e) = crate::platform::start_download(&request) {
                ended(id, None, &downloader::label_for(&request), false, &e, None);
            }
            refresh_now();
        });
    });
}

/// Downloads waiting their turn.
pub fn queued_count() -> usize {
    with_entries(|entries| entries.iter().filter(|e| e.saved.state == State::Queued).count())
}

/// Starts queued downloads, oldest first, while there's room. Call when one
/// stops running (or the limit goes up); never with ENTRIES or the
/// downloader's WATCHED locked.
pub fn pump() {
    let starts = with_entries(|entries| {
        let max = MAX_CONCURRENT.load(Ordering::SeqCst);
        let running = entries.iter().filter(|e| e.saved.state == State::Active).count();
        let room = if max == 0 { usize::MAX } else { max.saturating_sub(running) };
        let mut queued: Vec<&mut Entry> = entries.iter_mut().filter(|e| e.saved.state == State::Queued).collect();
        queued.sort_by_key(|e| (e.saved.added, e.id));
        queued
            .into_iter()
            .take(room)
            .map(|entry| {
                // Its place is kept while it starts.
                entry.saved.state = State::Active;
                save(entry);
                (entry.id, entry.handle.clone(), entry.saved.request.clone())
            })
            .collect::<Vec<_>>()
    });
    for (id, handle, request) in starts {
        // Paused in aria2: carry on with it.
        if let Some(handle) = handle {
            if downloader::unpause(&handle) {
                continue;
            }
            retire(id, &handle);
        }
        if let Err(e) = downloader::start(&request) {
            ended(id, None, &downloader::label_for(&request), false, &e, None);
        }
    }
}

/// Drops work a download is being started again without.
fn retire(id: i64, handle: &Handle) {
    downloader::cancel(handle);
    with_entries(|entries| {
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            if entry.handle.as_ref() == Some(handle) {
                entry.handle = None;
            }
            entry.retired.push(handle.clone());
        }
    });
}

/// The work running a download (again, or for a torrent's files).
pub fn running(id: i64, handle: Handle) {
    with_entries(|entries| {
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.handle = Some(handle);
            entry.last = None;
            if entry.saved.state != State::Active {
                entry.saved.state = State::Active;
                entry.saved.message.clear();
                save(entry);
            }
        }
    });
}

/// Auto picked how to download it: kept for resuming.
pub fn update_request(request: &DownloadRequest) {
    with_entries(|entries| {
        if let Some(entry) = entries.iter_mut().find(|e| e.id == request.id) {
            entry.saved.request = request.clone();
            entry.saved.file_type = file_type_index(guess_type(request));
            entry.saved.several |= several_files(request);
            save(entry);
        }
    });
}

/// A download ended (downloader::finished). Returns whether to tell the
/// user: not when they paused or cancelled it themselves.
pub fn ended(id: i64, handle: Option<&Handle>, name: &str, ok: bool, message: &str, path: Option<std::path::PathBuf>) -> bool {
    with_entries(|entries| {
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return true;
        };
        if matches!(entry.saved.state, State::Paused | State::Cancelled) {
            return false;
        }
        // An aria2 download it was restarted away from (new options).
        if let Some(ended) = handle {
            if let Some(i) = entry.retired.iter().position(|h| h == ended) {
                entry.retired.remove(i);
                return false;
            }
            if entry.handle.as_ref().is_some_and(|current| current != ended) {
                return false;
            }
        }
        entry.handle = None;
        entry.saved.state = if ok { State::Completed } else { State::Failed };
        entry.saved.message = message.to_string();
        if ok {
            entry.saved.name = name.to_string();
            entry.saved.partial.clear();
            // Several files: the folder they're in, not the first of them.
            let mut path = path.map(|p| top_item(&entry.saved, p));
            // Renamed while it downloaded.
            if let (Some(new_name), Some(old)) = (entry.saved.rename_to.take(), path.clone()) {
                // A free name if that one's taken (rename(2) would replace it).
                let new = downloader::free_path(&renamed(&old, &new_name));
                if std::fs::rename(&old, &new).is_ok() {
                    entry.saved.name = new.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(new_name);
                    path = Some(new);
                }
            }
            // What it came to on disk (books counted chapters until now).
            entry.saved.is_folder = path.as_ref().is_some_and(|p| p.is_dir());
            entry.saved.files = match &path {
                Some(dir) if entry.saved.is_folder => folder_listing(dir),
                _ => Vec::new(),
            };
            // A torrent's one file, now that it's known.
            if entry.saved.request.backend == Backend::Torrent && !entry.saved.is_folder {
                if let Some(name) = path.as_ref().and_then(|p| p.file_name()) {
                    entry.saved.file_type = file_type_index(crate::links::file_type_for(&name.to_string_lossy(), None));
                }
            }
            if let Some(size) = path.as_deref().map(disk_size).filter(|&n| n > 0) {
                entry.saved.total = size;
                entry.saved.done = size;
            }
            entry.saved.path = path.map(|p| p.to_string_lossy().into_owned());
            entry.saved.done = entry.saved.done.max(entry.saved.total);
        }
        entry.saved.ended = now();
        save(entry);
        true
    })
}

/// Whether a request is for several files: a playlist (not of one), a
/// post's or a torrent's picked files. A post with all of its items, or a
/// whole torrent, isn't known until it runs.
fn several_files(request: &DownloadRequest) -> bool {
    match request.backend {
        // Several picked, or a whole torrent whose name isn't a file's (a
        // single-file torrent is named after its file).
        Backend::Torrent => {
            request.select_files.contains([',', '-'])
                || (request.select_files.is_empty() && crate::links::file_type_for(&request.filename, None) == FileType::Other)
        }
        Backend::Media => serde_json::from_str::<crate::extractors::MediaRequest>(&request.format).is_ok_and(|media| {
            media.items.len() > 1 || (media.playlist && media.items.is_empty() && media.entries > 1)
        }),
        _ => false,
    }
}

/// Where a download's files go: its folder (and subfolder), as written to.
fn base_dir(saved: &Saved) -> std::path::PathBuf {
    let dir = crate::storage::resolve_for(&saved.request.folder, saved.request.id).write_dir().to_path_buf();
    let subfolder = &saved.request.settings.subfolder;
    if subfolder.trim().is_empty() { dir } else { dir.join(crate::novels::clean::safe_file_name(subfolder)) }
}

/// What a download saved at the top of its folder: the folder its files
/// went into (a playlist's, a post's, a torrent's), or the file itself.
fn top_item(saved: &Saved, path: std::path::PathBuf) -> std::path::PathBuf {
    let base = base_dir(saved);
    match path.strip_prefix(&base).ok().and_then(|rel| rel.components().next()) {
        Some(top) => base.join(top),
        None => path,
    }
}

/// A file's size, or everything in a folder.
fn disk_size(path: &std::path::Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path).map(|d| d.flatten().map(|e| disk_size(&e.path())).sum()).unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Adds what the running work has written to the record.
fn note_written(entry: &mut Entry) {
    let Some(handle) = &entry.handle else {
        return;
    };
    let mut changed = false;
    for path in downloader::written(handle) {
        let path = path.to_string_lossy().into_owned();
        if !entry.saved.partial.contains(&path) {
            entry.saved.partial.push(path);
            changed = true;
        }
    }
    if changed {
        save(entry);
    }
}

/// Removes what an unfinished download wrote: its files, the partial
/// files beside them (yt-dlp's .part, per-format and fragment files,
/// aria2's .aria2), and folders that leaves empty. A moment later, so the
/// work it stopped has let go of them.
fn discard_partial(saved: &mut Saved) {
    let written: Vec<std::path::PathBuf> = saved.partial.drain(..).map(Into::into).collect();
    if written.is_empty() {
        return;
    }
    let base = base_dir(saved);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        for path in &written {
            remove_with_partials(path);
        }
        for path in &written {
            let mut dir = path.parent();
            while let Some(d) = dir.filter(|d| d.starts_with(&base) && *d != base) {
                if std::fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
    });
}

fn remove_with_partials(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let Ok(siblings) = std::fs::read_dir(dir) else {
        return;
    };
    for sibling in siblings.flatten() {
        let other = sibling.file_name().to_string_lossy().into_owned();
        let partial = other == format!("{name}.aria2")
            || other.strip_prefix(stem).filter(|rest| rest.starts_with('.')).is_some_and(is_partial_suffix);
        if partial {
            let _ = std::fs::remove_file(sibling.path());
        }
    }
}

/// What yt-dlp adds to an item's name while it works: `.part`, `.ytdl`,
/// `.temp`, fragments, a format's own file (`.f137.mp4`), the thumbnail.
fn is_partial_suffix(rest: &str) -> bool {
    static FORMAT_RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^\.f[0-9A-Za-z_-]+\.").unwrap());
    [".part", ".ytdl", ".temp", "-Frag"].iter().any(|p| rest.contains(p))
        || FORMAT_RE.is_match(rest)
        || matches!(rest, ".webp" | ".jpg" | ".png")
}

/// A multi-file download picked a free folder name (one of that name was
/// there already): kept as its name, so resuming finds the same folder.
pub fn set_folder_name(id: i64, name: &str) {
    with_entries(|entries| {
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.saved.request.filename = name.to_string();
            entry.saved.name = name.to_string();
            save(entry);
        }
    });
}

/// A finished download's copy in its picked folder (Android: a document
/// URI), which opening it uses.
pub fn delivered(id: i64, uri: &str) {
    with_entries(|entries| {
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.saved.path = Some(uri.to_string());
            save(entry);
        }
    });
}

/// A copy in a picked folder: opened and deleted through Android, not
/// renamed.
fn is_document(path: &str) -> bool {
    path.starts_with("content://")
}

// --- Actions ---

/// Shares the selected downloads: finished ones' files (a folder's, all of
/// them that are still there), the rest's links.
fn share(ids: &[i32]) {
    let (files, links) = with_entries(|entries| {
        let (mut files, mut links) = (Vec::new(), Vec::new());
        for entry in entries.iter().filter(|e| ids.contains(&(e.id as i32))) {
            let saved = &entry.saved;
            match (&saved.state, &saved.path) {
                (State::Completed, Some(path)) if is_document(path) => files.push(path.clone()),
                (State::Completed, Some(path)) if saved.is_folder => {
                    let dir = std::path::Path::new(path);
                    files.extend(folder_listing(dir).into_iter().map(|(rel, _)| dir.join(rel).to_string_lossy().into_owned()));
                }
                (State::Completed, Some(path)) if std::path::Path::new(path).exists() => files.push(path.clone()),
                // A saved .torrent's path means nothing to anyone else.
                _ if !saved.request.url.starts_with('/') => links.push(saved.request.url.clone()),
                _ => {}
            }
        }
        (files, links)
    });
    if files.is_empty() && links.is_empty() {
        crate::platform::show_toast("Nothing to share: their files can't be found");
        return;
    }
    crate::platform::share(&files, &links.join("\n"));
}

fn pause(id: i64) {
    with_entries(|entries| {
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if !matches!(entry.saved.state, State::Active | State::Queued) {
            return;
        }
        note_written(entry);
        if let Some(handle) = &entry.handle {
            downloader::pause(handle);
            // A job is gone once stopped; aria2 keeps its download, paused.
            if matches!(handle, Handle::Job(_)) {
                entry.handle = None;
            }
        }
        entry.saved.state = State::Paused;
        save(entry);
    });
}

/// Resumes a paused download, or tries a failed or cancelled one again.
fn resume(id: i64) {
    let restart = with_entries(|entries| {
        let entry = entries.iter_mut().find(|e| e.id == id)?;
        if !matches!(entry.saved.state, State::Paused | State::Failed | State::Cancelled) {
            return None;
        }
        // No room: it waits its turn (keeping aria2's paused download).
        if !has_slot(entries, id) {
            let entry = entries.iter_mut().find(|e| e.id == id)?;
            entry.saved.state = State::Queued;
            entry.saved.message.clear();
            save(entry);
            return None;
        }
        let entry = entries.iter_mut().find(|e| e.id == id)?;
        // aria2 still has a paused download: carry on with it.
        if entry.saved.state == State::Paused {
            if let Some(handle) = &entry.handle {
                if downloader::unpause(handle) {
                    entry.saved.state = State::Active;
                    save(entry);
                    return None;
                }
            }
        }
        entry.handle = None;
        entry.saved.state = State::Active;
        entry.saved.message.clear();
        save(entry);
        Some(DownloadRequest { resume: true, ..entry.saved.request.clone() })
    });
    // Outside the lock: starting reports back through `running`.
    if let Some(request) = restart {
        if let Err(e) = crate::platform::start_download(&request) {
            ended(id, None, &downloader::label_for(&request), false, &e, None);
        }
    }
}

fn cancel(id: i64) {
    with_entries(|entries| {
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if !matches!(entry.saved.state, State::Active | State::Queued | State::Paused) {
            return;
        }
        note_written(entry);
        if let Some(handle) = entry.handle.take() {
            downloader::cancel(&handle);
        }
        // Trying it again starts over, so what it had is no use.
        discard_partial(&mut entry.saved);
        entry.saved.state = State::Cancelled;
        save(entry);
    });
}

/// Drops a download from the lists, and with `files`, what it saved.
fn delete(id: i64, files: bool) {
    let removed = with_entries(|entries| {
        let index = entries.iter().position(|e| e.id == id)?;
        let mut entry = entries.remove(index);
        note_written(&mut entry);
        if let Some(handle) = entry.handle.take() {
            downloader::cancel(&handle);
        }
        Some(entry)
    });
    let Some(mut entry) = removed else {
        return;
    };
    crate::db::delete_download(id);
    if files && entry.saved.state != State::Completed {
        discard_partial(&mut entry.saved);
    } else if files {
        if let Some(uri) = entry.saved.path.as_ref().filter(|p| is_document(p)) {
            crate::platform::delete_document(uri);
        } else if let Some(path) = &entry.saved.path {
            let path = std::path::Path::new(path);
            let _ = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
        }
    }
}

/// Opens a finished download: its file, or for a folder, the list of its
/// files (FolderFilesSheet). A file that's gone says so instead.
fn open(id: i64) {
    let found = with_entries(|entries| entries.iter().find(|e| e.id == id).map(|e| e.saved.clone()));
    let Some(saved) = found else {
        return;
    };
    let Some(path) = saved.path.clone() else {
        crate::platform::show_toast(&format!("Saved in {}", saved.folder));
        return;
    };
    if is_document(&path) {
        crate::platform::open_file(&path);
        return;
    }
    let dir = std::path::Path::new(&path);
    if saved.is_folder {
        // Recorded when it finished; downloads from before that, as it is now.
        let recorded = if saved.files.is_empty() { folder_listing(dir) } else { saved.files.clone() };
        if recorded.is_empty() {
            crate::platform::show_toast(if dir.exists() { "This folder is empty" } else { "This folder can't be found. It may have been moved or deleted" });
            return;
        }
        show_folder(id, &saved.name, dir, &recorded);
        return;
    }
    if dir.exists() {
        crate::platform::open_file(&path);
    } else {
        crate::platform::show_toast("This file can't be found. It may have been moved or deleted");
    }
}

/// The files in a folder (paths inside it, sizes), sorted. aria2's
/// .aria2 files are left out.
fn folder_listing(dir: &std::path::Path) -> Vec<(String, u64)> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if out.len() >= 5000 {
                return;
            }
            let path = entry.path();
            match entry.metadata() {
                Ok(m) if m.is_dir() => walk(base, &path, out),
                Ok(m) if !path.extension().is_some_and(|e| e == "aria2") => {
                    let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().into_owned();
                    out.push((rel, m.len()));
                }
                _ => {}
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    files
}

/// Fills in and opens the folder sheet.
fn show_folder(id: i64, name: &str, dir: &std::path::Path, files: &[(String, u64)]) {
    let rows: Vec<FolderFile> = files
        .iter()
        .map(|(rel, size)| {
            let on_disk = std::fs::metadata(dir.join(rel)).ok().filter(|m| m.is_file());
            FolderFile {
                name: rel.clone().into(),
                size: crate::links::format_size(on_disk.as_ref().map_or(*size, |m| m.len())).into(),
                file_type: crate::links::file_type_for(rel, None),
                missing: on_disk.is_none(),
            }
        })
        .collect();
    let missing = rows.iter().filter(|r| r.missing).count();
    APP.with(|app| {
        let Some(ui) = app.borrow().as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        let sheet = ui.global::<FolderFiles>();
        sheet.set_id(id as i32);
        sheet.set_title(name.into());
        sheet.set_files(ModelRc::new(VecModel::from(rows)));
        sheet.set_missing(missing as i32);
        sheet.set_open(true);
    });
}

/// A finished folder download's folder, and the path of its file at
/// `index` in the sheet's list.
fn folder_file(id: i64, index: Option<usize>) -> Option<(std::path::PathBuf, Option<std::path::PathBuf>)> {
    with_entries(|entries| {
        let saved = &entries.iter().find(|e| e.id == id)?.saved;
        let dir = std::path::PathBuf::from(saved.path.as_ref()?);
        let files = if saved.files.is_empty() { folder_listing(&dir) } else { saved.files.clone() };
        let file = index.and_then(|i| files.get(i)).map(|(rel, _)| dir.join(rel));
        Some((dir, file))
    })
}

/// Renames a finished download's file (unfinished ones keep their name).
fn rename(id: i64, name: &str) {
    let name = crate::novels::clean::safe_file_name(name.trim());
    with_entries(|entries| {
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if entry.saved.state != State::Completed {
            // Still downloading: the file gets the name once it's done.
            entry.saved.rename_to = Some(name.clone());
            entry.saved.name = name;
            save(entry);
            return;
        }
        let Some(path) = entry.saved.path.clone().filter(|p| !is_document(p)) else {
            crate::platform::show_toast("This one went into a picked folder; rename it there");
            return;
        };
        let old = std::path::PathBuf::from(&path);
        let new = renamed(&old, &name);
        // rename(2) would quietly replace it.
        if new != old && new.exists() {
            crate::platform::show_toast("There's already a file with that name there");
            return;
        }
        match std::fs::rename(&old, &new) {
            Ok(()) => {
                entry.saved.name = new.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
                entry.saved.path = Some(new.to_string_lossy().into_owned());
                save(entry);
            }
            Err(e) => crate::platform::show_toast(&format!("Couldn't rename it: {e}")),
        }
    });
}

/// `old` with the name `name`, keeping its extension unless `name` has one.
fn renamed(old: &std::path::Path, name: &str) -> std::path::PathBuf {
    if old.is_dir() {
        return old.with_file_name(name);
    }
    let new_name = match (old.extension(), std::path::Path::new(name).extension()) {
        (Some(ext), None) => format!("{name}.{}", ext.to_string_lossy()),
        _ => name.to_string(),
    };
    old.with_file_name(new_name)
}

/// New speed, connections, user agent or folder for one download. A
/// running one restarts with them (aria2 carries on from its partial file
/// in the same folder; yt-dlp from its .part; books from their chapters).
fn change_options(id: i64, options: &crate::LinkOptions) {
    let restart = with_entries(|entries| {
        let entry = entries.iter_mut().find(|e| e.id == id)?;
        if entry.saved.state == State::Completed {
            return None;
        }
        let book = entry.saved.request.backend == Backend::Book;
        entry.saved.request.settings = crate::downloader::DownloadSettings {
            max_speed: options.speed_kbps.max(0) as u64 * 1024,
            connections: options.connections.clamp(1, 16) as u16,
            user_agent: match options.ua_mode {
                1 => crate::extractors::http::UA_DESKTOP.to_string(),
                2 => options.ua_custom.trim().to_string(),
                _ => String::new(),
            },
            subfolder: match options.category {
                false => String::new(),
                true if book => "Books".to_string(),
                true => crate::links::category_folder(file_type(entry.saved.file_type)).to_string(),
            },
            replace_existing: entry.saved.request.settings.replace_existing,
        };
        if entry.saved.move_to.is_none() {
            entry.saved.folder = folder_display(&entry.saved.request);
        }
        let running = matches!(entry.saved.state, State::Active | State::Queued);
        // Whatever's running (or paused in aria2) is replaced.
        if let Some(handle) = entry.handle.take() {
            downloader::cancel(&handle);
            entry.retired.push(handle);
        }
        save(entry);
        running.then(|| entry.saved.request.clone())
    });
    if let Some(request) = restart {
        if let Err(e) = crate::platform::start_download(&request) {
            ended(id, None, &downloader::label_for(&request), false, &e, None);
        }
    }
}

/// "Change folder": picks a folder for an unfinished download. It isn't
/// moved now: once it's done, its files go there (`apply_move`).
fn change_folder(id: i64) {
    crate::platform::pick_download_folder(move |raw, display| {
        let _ = slint::invoke_from_event_loop(move || {
            with_entries(|entries| {
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id && e.saved.state != State::Completed) {
                    entry.saved.move_to = Some(raw.clone());
                    entry.saved.folder = display.clone();
                    save(entry);
                    crate::platform::show_toast("It moves there once it's done");
                }
            });
            refresh_now();
        });
    });
}

/// A finished download with a folder picked since it started: moves what it
/// saved (`path`, its first file; a torrent's whole folder) there. Returns
/// where it is now and what's still to be copied into a picked folder.
/// Plain renames happen straight away; a copy across drives runs on its own
/// thread, and the record follows when it's done.
pub fn apply_move(id: i64, path: Option<std::path::PathBuf>, deliver: Option<downloader::Deliver>)
    -> (Option<std::path::PathBuf>, Option<downloader::Deliver>) {
    let found = with_entries(|entries| {
        let entry = entries.iter_mut().find(|e| e.id == id)?;
        let to = entry.saved.move_to.take()?;
        let from_folder = entry.saved.request.folder.clone();
        entry.saved.request.folder = to.clone();
        save(entry);
        Some((to, from_folder, entry.saved.request.settings.subfolder.clone()))
    });
    let (Some((to, from_folder, subfolder)), Some(path)) = (found, path.clone()) else {
        return (path, deliver);
    };
    let in_sub = |dir: &std::path::Path| {
        if subfolder.trim().is_empty() { dir.to_path_buf() } else { dir.join(crate::novels::clean::safe_file_name(&subfolder)) }
    };
    // Where it was written, and the top item there to take along (a
    // torrent's folder, or the file itself).
    let old_dir = match &deliver {
        Some(d) => d.staging.clone(),
        None => in_sub(crate::storage::resolve(&from_folder).write_dir()),
    };
    let rel = path.strip_prefix(&old_dir).map(|r| r.to_path_buf()).unwrap_or_else(|_| path.file_name().map(Into::into).unwrap_or_default());
    let Some(top) = rel.components().next().map(|c| old_dir.join(c)) else {
        return (Some(path), deliver);
    };
    match crate::storage::resolve(&to) {
        // A picked folder only reached through Android: the usual copy, from
        // a staging folder of its own.
        crate::storage::SaveTarget::Staged { tree_uri, staging } => {
            let dir = in_sub(&staging);
            if std::fs::create_dir_all(&dir).is_err() || move_item(id, &top, &dir.join(top.file_name().unwrap_or_default()), false).is_err() {
                return (Some(path), deliver);
            }
            (None, Some(downloader::Deliver { staging, tree_uri }))
        }
        crate::storage::SaveTarget::Dir(dir) => {
            let dir = in_sub(&dir);
            if std::fs::create_dir_all(&dir).is_err() {
                crate::platform::show_toast("Couldn't make the new folder; it stayed where it was");
                return (Some(path), deliver);
            }
            let new_top = dir.join(top.file_name().unwrap_or_default());
            let staging = deliver.as_ref().map(|d| d.staging.clone());
            match move_item(id, &top, &new_top, true) {
                Ok(true) => {
                    // Its staging folder, emptied.
                    if let Some(staging) = staging {
                        let _ = std::fs::remove_dir(staging);
                    }
                    (Some(dir.join(&rel)), None)
                }
                // Copying across drives: the record follows once it's done.
                Ok(false) => (Some(path), None),
                Err(()) => (Some(path), deliver),
            }
        }
    }
}

/// A finished torrent with several files: its folder (the torrent's own
/// name) renamed to the folder name the preview gave it. Returns where its
/// first file is now.
pub fn name_torrent_folder(id: i64, path: Option<std::path::PathBuf>, deliver: Option<&downloader::Deliver>) -> Option<std::path::PathBuf> {
    let path = path?;
    let wanted = with_entries(|entries| {
        let request = &entries.iter().find(|e| e.id == id)?.saved.request;
        let name = crate::novels::clean::safe_file_name(request.filename.trim());
        (request.backend == Backend::Torrent && !name.is_empty()).then(|| (name, request.folder.clone(), request.settings.subfolder.clone()))
    });
    let Some((name, folder, subfolder)) = wanted else {
        return Some(path);
    };
    let mut dir = match deliver {
        Some(d) => d.staging.clone(),
        None => crate::storage::resolve(&folder).write_dir().to_path_buf(),
    };
    if !subfolder.trim().is_empty() {
        dir = dir.join(crate::novels::clean::safe_file_name(&subfolder));
    }
    let Ok(rel) = path.strip_prefix(&dir).map(|r| r.to_path_buf()) else {
        return Some(path);
    };
    let mut parts = rel.components();
    let Some(top) = parts.next().map(|c| dir.join(c)) else {
        return Some(path);
    };
    let inside: std::path::PathBuf = parts.collect();
    // Single-file torrents keep their file's name.
    if !top.is_dir() || inside.as_os_str().is_empty() || top.file_name().is_some_and(|n| n == name.as_str()) {
        return Some(path);
    }
    // A folder of that name there already: the first free "name (n)".
    let renamed = downloader::free_path(&dir.join(&name));
    if std::fs::rename(&top, &renamed).is_err() {
        log::warn!("couldn't rename {} to {name}", top.display());
        return Some(path);
    }
    Some(renamed.join(inside))
}

/// Moves `from` to `to`: Ok(true) when renamed, Ok(false) when it's being
/// copied across drives in the background (with `background`), Err when it
/// couldn't.
fn move_item(id: i64, from: &std::path::Path, to: &std::path::Path, background: bool) -> Result<bool, ()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(true);
    }
    let copy = {
        let (from, to) = (from.to_path_buf(), to.to_path_buf());
        move || -> std::io::Result<()> {
            copy_all(&from, &to)?;
            if from.is_dir() { std::fs::remove_dir_all(&from) } else { std::fs::remove_file(&from) }
        }
    };
    if !background {
        return copy().map(|_| true).map_err(|_| ());
    }
    let (from, to) = (from.to_path_buf(), to.to_path_buf());
    std::thread::spawn(move || match copy() {
        Ok(()) => with_entries(|entries| {
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                // What moved is what it saved (a folder's whole).
                entry.saved.path = Some(to.to_string_lossy().into_owned());
                entry.saved.is_folder = to.is_dir();
                save(entry);
            }
        }),
        Err(e) => {
            log::warn!("couldn't move {} to {}: {e}", from.display(), to.display());
            crate::platform::show_toast("Couldn't move it to the new folder; it stayed where it was");
        }
    });
    Ok(false)
}

fn copy_all(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Restarts an unfinished download after `change` (a new folder or link):
/// whatever's running (or paused in aria2) is dropped, and a download that
/// was running starts again; a paused one stays paused, with the change.
/// `change` gets the file aria2 was writing, if it was, before it stops.
fn restart_with(id: i64, change: impl FnOnce(&mut Saved, Option<std::path::PathBuf>) -> Result<(), String>) {
    let restart = with_entries(|entries| {
        let entry = entries.iter_mut().find(|e| e.id == id)?;
        if entry.saved.state == State::Completed {
            return None;
        }
        let file = entry.handle.as_ref().and_then(downloader::aria_file);
        let running = matches!(entry.saved.state, State::Active | State::Queued);
        if let Some(handle) = entry.handle.take() {
            downloader::cancel(&handle);
            entry.retired.push(handle);
        }
        if let Err(e) = change(&mut entry.saved, file) {
            crate::platform::show_toast(&e);
        }
        if entry.saved.move_to.is_none() {
            entry.saved.folder = folder_display(&entry.saved.request);
        }
        if !running && entry.saved.state == State::Failed {
            // Ready to try again with the change.
            entry.saved.message.clear();
        }
        save(entry);
        running.then(|| entry.saved.request.clone())
    });
    if let Some(request) = restart {
        if let Err(e) = crate::platform::start_download(&request) {
            ended(id, None, &downloader::label_for(&request), false, &e, None);
        }
    }
}

/// "Refresh link": a new link to the same file (one that expired, say).
/// The file keeps its name, so aria2 carries on with what it has.
fn refresh_link(id: i64, url: &str) {
    let url = url.trim().to_string();
    if !url.contains("://") {
        crate::platform::show_toast("That isn't a link");
        return;
    }
    restart_with(id, |saved, file| {
        saved.request.url = url;
        if saved.request.filename.is_empty() {
            saved.request.filename = file
                .and_then(|f| f.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| saved.name.clone());
        }
        Ok(())
    });
}

// --- The lists ---

/// Hooks the lists and their actions up. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    let downloading = Rc::new(VecModel::<DownloadItem>::default());
    let stopped = Rc::new(VecModel::<DownloadItem>::default());
    let completed = Rc::new(VecModel::<DownloadItem>::default());
    ui.set_downloading_items(ModelRc::from(downloading.clone()));
    ui.set_stopped_items(ModelRc::from(stopped.clone()));
    ui.set_completed_items(ModelRc::from(completed.clone()));

    APP.with(|app| *app.borrow_mut() = Some(ui.as_weak()));
    let folder_files = ui.global::<FolderFiles>();
    folder_files.on_open_file(|id, index| {
        match folder_file(id as i64, Some(index as usize)) {
            Some((_, Some(file))) if file.is_file() => crate::platform::open_file(&file.to_string_lossy()),
            _ => crate::platform::show_toast("This file can't be found. It may have been moved or deleted"),
        }
    });
    folder_files.on_open_folder(|id| match folder_file(id as i64, None) {
        Some((dir, _)) if dir.is_dir() => crate::platform::open_file(&dir.to_string_lossy()),
        _ => crate::platform::show_toast("This folder can't be found. It may have been moved or deleted"),
    });

    ui.on_download_options_changed(|id, options| {
        change_options(id as i64, &options);
        refresh_now();
    });

    ui.on_download_action(|id, action, value| {
        let id = id as i64;
        match action {
            ItemAction::Pause => pause(id),
            ItemAction::Resume => resume(id),
            ItemAction::Cancel => cancel(id),
            ItemAction::Delete => delete(id, value == "files"),
            ItemAction::Open => open(id),
            ItemAction::Rename => rename(id, &value),
            ItemAction::UpdateBook => update_book(id),
            ItemAction::ChangeFolder => change_folder(id),
            ItemAction::RefreshLink => refresh_link(id, &value),
        }
        // A paused, cancelled or deleted one makes room.
        pump();
        refresh_now();
    });

    // Each tab's sort and filter (the list options sheet).
    let ui_weak = ui.as_weak();
    let refresh = move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        let options = ui.global::<ListOptions>();
        let rows = items();
        // The rows in `states`, each state's own after the one before, sorted
        // and filtered by `options` within it.
        let pick = |states: &[DownloadState], options: &TabListOptions| -> Vec<DownloadItem> {
            let mut picked = Vec::new();
            for state in states {
                let mut group: Vec<&Row> =
                    rows.iter().filter(|r| r.item.state == *state && matches_filter(&r.item, options.filter)).collect();
                group.sort_by(|a, b| {
                    let order = match options.sort {
                        SortKey::Name => a.item.filename.to_lowercase().cmp(&b.item.filename.to_lowercase()),
                        SortKey::Size => a.size.cmp(&b.size),
                        SortKey::Progress => a.item.progress.total_cmp(&b.item.progress),
                        _ => a.added.cmp(&b.added),
                    };
                    if options.ascending { order } else { order.reverse() }
                });
                picked.extend(group.into_iter().map(|r| r.item.clone()));
            }
            picked
        };
        sync(&downloading, pick(&[DownloadState::Active, DownloadState::Queued], &options.get_downloading()));
        sync(&stopped, pick(&[DownloadState::Paused, DownloadState::Failed, DownloadState::Cancelled], &options.get_stopped()));
        sync(&completed, pick(&[DownloadState::Completed], &options.get_completed()));
    };
    // Each tab's choice is kept, so the lists come back sorted the same way.
    let list_options = ui.global::<ListOptions>();
    for tab in 0..3 {
        if let Some(saved) = load_list_options(tab) {
            match tab {
                0 => list_options.set_downloading(saved),
                1 => list_options.set_stopped(saved),
                _ => list_options.set_completed(saved),
            }
        }
    }
    let ui_weak = ui.as_weak();
    list_options.on_options_changed(move |tab| {
        if let Some(ui) = ui_weak.upgrade() {
            let options = ui.global::<ListOptions>();
            let current = match tab {
                0 => options.get_downloading(),
                1 => options.get_stopped(),
                _ => options.get_completed(),
            };
            save_list_options(tab, &current);
        }
        refresh_now();
    });
    refresh();
    REFRESH.with(|r| *r.borrow_mut() = Some(Box::new(refresh.clone())));
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), refresh);
    std::mem::forget(timer);
}

thread_local! {
    static REFRESH: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
    /// The app, for the folder sheet `open` fills in.
    static APP: std::cell::RefCell<Option<slint::Weak<DownloaderApp>>> = const { std::cell::RefCell::new(None) };
}

/// Shows an action's result straight away rather than at the next tick.
fn refresh_now() {
    REFRESH.with(|r| {
        if let Some(refresh) = r.borrow().as_ref() {
            refresh();
        }
    });
}

/// The notification's buttons: pauses (or with `cancel`, cancels) every
/// running or queued download. Any thread; the lists catch up on their own.
pub fn control_all(cancel_them: bool) {
    let ids: Vec<i64> =
        with_entries(|entries| entries.iter().filter(|e| matches!(e.saved.state, State::Active | State::Queued)).map(|e| e.id).collect());
    for id in ids {
        if cancel_them { cancel(id) } else { pause(id) }
    }
}

/// Acts on the selected downloads (the selection bar).
pub fn act(ids: &[i32], action: SelectionAction, delete_files: bool) {
    if action == SelectionAction::Share {
        share(ids);
        return;
    }
    for &id in ids {
        let id = id as i64;
        match action {
            SelectionAction::Pause => pause(id),
            SelectionAction::Resume => resume(id),
            SelectionAction::Cancel => cancel(id),
            SelectionAction::Delete => delete(id, delete_files),
            SelectionAction::Share => {}
        }
    }
    pump();
    refresh_now();
}

/// Makes `model` hold `items`, changing rows in place where it can (so
/// scrolling and animations aren't disturbed every second).
fn sync(model: &VecModel<DownloadItem>, items: Vec<DownloadItem>) {
    let same_rows = model.row_count() == items.len() && model.iter().zip(&items).all(|(a, b)| a.id == b.id);
    if !same_rows {
        model.set_vec(items);
        return;
    }
    for (i, item) in items.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&item) {
            model.set_row_data(i, item);
        }
    }
}

/// A list row, with what it's sorted by.
struct Row {
    item: DownloadItem,
    /// When it was added (unix seconds).
    added: u64,
    /// Its size in bytes, 0 when unknown.
    size: u64,
}

/// A tab's sort and filter, as kept: "<sort>,<ascending>,<filter>" (the
/// enums' indexes).
fn list_options_key(tab: i32) -> String {
    format!("list_options_{tab}")
}

fn save_list_options(tab: i32, options: &TabListOptions) {
    let sort = match options.sort {
        SortKey::Name => 1,
        SortKey::Size => 2,
        SortKey::Progress => 3,
        _ => 0,
    };
    let filter = match options.filter {
        FileFilter::Video => 1,
        FileFilter::Audio => 2,
        FileFilter::Image => 3,
        FileFilter::Document => 4,
        FileFilter::Archive => 5,
        FileFilter::Folder => 6,
        FileFilter::Other => 7,
        _ => 0,
    };
    let value = format!("{sort},{},{filter}", options.ascending as u8);
    if let Err(e) = crate::platform::save_pref(&list_options_key(tab), &value) {
        log::warn!("couldn't save the list options: {e}");
    }
}

fn load_list_options(tab: i32) -> Option<TabListOptions> {
    let value = crate::platform::get_pref(&list_options_key(tab), "").ok()?;
    let parts: Vec<u8> = value.split(',').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let [sort, ascending, filter] = parts.as_slice() else {
        return None;
    };
    Some(TabListOptions {
        sort: match sort {
            1 => SortKey::Name,
            2 => SortKey::Size,
            3 => SortKey::Progress,
            _ => SortKey::DateAdded,
        },
        ascending: *ascending == 1,
        filter: match filter {
            1 => FileFilter::Video,
            2 => FileFilter::Audio,
            3 => FileFilter::Image,
            4 => FileFilter::Document,
            5 => FileFilter::Archive,
            6 => FileFilter::Folder,
            7 => FileFilter::Other,
            _ => FileFilter::All,
        },
    })
}

/// Whether `item` is one the filter keeps.
fn matches_filter(item: &DownloadItem, filter: FileFilter) -> bool {
    match filter {
        FileFilter::Video => item.file_type == FileType::Video,
        FileFilter::Audio => item.file_type == FileType::Audio,
        FileFilter::Image => item.file_type == FileType::Image,
        FileFilter::Document => item.file_type == FileType::Document,
        FileFilter::Archive => matches!(item.file_type, FileType::Archive | FileType::DiskImage),
        // A folder also shows under what's in it (a playlist under Video).
        FileFilter::Folder => item.is_folder,
        // Not a folder, and none of the types above.
        FileFilter::Other => !item.is_folder && matches!(item.file_type, FileType::Other | FileType::Program),
        _ => true,
    }
}

/// Every download as the lists show it, with live progress.
fn items() -> Vec<Row> {
    with_entries(|entries| {
        entries
            .iter_mut()
            .map(|entry| {
                let mut state = entry.saved.state;
                entry.part = None;
                note_written(entry);
                if let Some(handle) = &entry.handle {
                    entry.part = downloader::part(handle);
                    if entry.part.is_some() && !entry.saved.several {
                        entry.saved.several = true;
                        save(entry);
                    }
                    if let Some((live, done, total, speed)) = downloader::progress(handle) {
                        // Queued in aria2's own queue (Eddy's queued ones are
                        // paused there, or not started).
                        if state == State::Active {
                            state = match live {
                                Live::Queued => State::Queued,
                                _ => State::Active,
                            };
                        }
                        entry.saved.done = done;
                        if total > 0 {
                            entry.saved.total = total;
                        }
                        entry.speed = if speed > 0.0 { speed } else { measured_speed(entry, done) };
                    }
                }
                Row { item: view(entry, state), added: entry.saved.added, size: entry.saved.total }
            })
            .collect()
    })
}

/// Bytes per second since the last look, for work that doesn't say.
fn measured_speed(entry: &mut Entry, done: u64) -> f64 {
    let now = Instant::now();
    let speed = match entry.last {
        Some((before, at)) if done >= before => {
            let secs = now.duration_since(at).as_secs_f64();
            if secs > 0.2 { (done - before) as f64 / secs } else { entry.speed }
        }
        _ => 0.0,
    };
    entry.last = Some((done, now));
    // Smoothed, so it doesn't jump about.
    if entry.speed > 0.0 { entry.speed * 0.6 + speed * 0.4 } else { speed }
}

fn view(entry: &Entry, state: State) -> DownloadItem {
    let saved = &entry.saved;
    let running = state == State::Active;
    // An unfinished book's progress is in chapters, not bytes.
    let counts_chapters = saved.request.backend == Backend::Book && state != State::Completed;
    let progress = match &entry.part {
        Some((_, fraction)) if running => *fraction,
        _ if saved.total > 0 => (saved.done as f64 / saved.total as f64).min(1.0) as f32,
        _ => 0.0,
    };
    let eta = if running && entry.speed > 1.0 && saved.total > saved.done {
        duration(((saved.total - saved.done) as f64 / entry.speed) as u64)
    } else {
        String::new()
    };
    DownloadItem {
        id: entry.id as i32,
        filename: saved.name.clone().into(),
        file_type: file_type(saved.file_type),
        state: match state {
            State::Active => DownloadState::Active,
            State::Queued => DownloadState::Queued,
            State::Paused => DownloadState::Paused,
            State::Failed => DownloadState::Failed,
            State::Cancelled => DownloadState::Cancelled,
            State::Completed => DownloadState::Completed,
        },
        progress: if state == State::Completed { 1.0 } else { progress },
        downloaded: if counts_chapters { saved.done.to_string().into() } else { crate::links::format_size(saved.done).into() },
        size: match saved.total {
            0 => "".into(),
            n if counts_chapters => format!("{n} chapters").into(),
            n => crate::links::format_size(n).into(),
        },
        speed: if running && entry.speed > 0.0 && !counts_chapters {
            format!("{}/s", crate::links::format_size(entry.speed as u64)).into()
        } else {
            "".into()
        },
        eta: eta.into(),
        date: short_date(if saved.ended > 0 { saved.ended } else { saved.added }).into(),
        url: saved.request.url.clone().into(),
        backend: match saved.request.backend {
            Backend::Torrent => LinkBackend::Torrent,
            Backend::Media => LinkBackend::Media,
            Backend::Book => LinkBackend::Book,
            Backend::Direct | Backend::Auto => LinkBackend::Direct,
        },
        folder: saved.folder.clone().into(),
        part: entry.part.as_ref().filter(|_| running).map(|(p, _)| p.clone()).unwrap_or_default().into(),
        is_folder: if state == State::Completed { saved.is_folder } else { saved.several },
        options: options_of(&saved.request.settings),
    }
}

/// A download's settings as the options editor shows them.
fn options_of(settings: &crate::downloader::DownloadSettings) -> crate::LinkOptions {
    let ua = settings.user_agent.trim();
    crate::LinkOptions {
        speed_kbps: (settings.max_speed / 1024) as i32,
        connections: settings.connections() as i32,
        ua_mode: match ua {
            "" => 0,
            ua if ua == crate::extractors::http::UA_DESKTOP => 1,
            _ => 2,
        },
        ua_custom: if ua == crate::extractors::http::UA_DESKTOP { "".into() } else { ua.into() },
        category: !settings.subfolder.is_empty(),
    }
}

/// "4m 48s", "1h 5m", "12s".
fn duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// "Oct 22" (the year too when it isn't this one).
fn short_date(secs: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d) = civil(secs as i64 / 86_400);
    let (this_year, _, _) = civil(now() as i64 / 86_400);
    if y == this_year { format!("{} {d}", MONTHS[m as usize - 1]) } else { format!("{} {d}, {y}", MONTHS[m as usize - 1]) }
}

/// Days since 1970-01-01 → (year, month, day).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// What a new download most likely is, for its icon.
fn guess_type(request: &DownloadRequest) -> FileType {
    match request.backend {
        // What's in it: a single file's type (its name is the file's);
        // several files show as a folder (`several_files`).
        Backend::Torrent => crate::links::file_type_for(&request.filename, None),
        Backend::Book => FileType::Document,
        Backend::Media => {
            let media: crate::extractors::MediaRequest = serde_json::from_str(&request.format).unwrap_or_default();
            if media.image {
                FileType::Image
            } else if !media.audio_codec.is_empty() {
                FileType::Audio
            } else {
                FileType::Video
            }
        }
        Backend::Direct | Backend::Auto => {
            let name = if request.filename.is_empty() { downloader::label_for(request) } else { request.filename.clone() };
            crate::links::file_type_for(&name, None)
        }
    }
}

fn folder_display(request: &DownloadRequest) -> String {
    let base = if request.folder.is_empty() {
        crate::links::usual_folder_display()
    } else {
        crate::storage::tree_uri_to_path(&request.folder).unwrap_or_else(|| crate::folders::display(std::path::Path::new(&request.folder)))
    };
    if request.settings.subfolder.is_empty() { base } else { format!("{base}/{}", request.settings.subfolder) }
}

fn file_type_index(file_type: FileType) -> i32 {
    match file_type {
        FileType::Video => 0,
        FileType::Audio => 1,
        FileType::Image => 2,
        FileType::Document => 3,
        FileType::Archive => 4,
        FileType::DiskImage => 5,
        // 6 was torrents' own type (see with_entries).
        FileType::Program => 7,
        _ => 8,
    }
}

fn file_type(index: i32) -> FileType {
    match index {
        0 => FileType::Video,
        1 => FileType::Audio,
        2 => FileType::Image,
        3 => FileType::Document,
        4 => FileType::Archive,
        5 => FileType::DiskImage,
        7 => FileType::Program,
        _ => FileType::Other,
    }
}
