//! The downloads lists: every download Eddy started, kept in the database,
//! with its state (downloading, queued, paused, failed, cancelled, done)
//! and, while it runs, its live progress from aria2 or its job. The lists'
//! actions (pause, resume, cancel, delete, open, rename) act on these.
//!
//! On Android this lives in the main process, where the download service
//! runs too; the share popup's downloads reach it through the service.

use crate::downloader::{self, Backend, DownloadRequest, Handle};
use crate::{DownloadItem, DownloadState, DownloaderApp, FileType, ItemAction, LinkBackend, SelectionAction};
use serde::{Deserialize, Serialize};
use slint::{Model, ModelRc, VecModel};
use std::rc::Rc;
use std::sync::Mutex;
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
}

static ENTRIES: Mutex<Option<Vec<Entry>>> = Mutex::new(None);

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
                Some(Entry { id, saved, handle: None, retired: Vec::new(), last: None, speed: 0.0 })
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
        ..Default::default()
    };
    let id = crate::db::insert_download(&serde_json::to_string(&saved).unwrap_or_default());
    let mut saved = saved;
    saved.request.id = id;
    if id > 0 {
        with_entries(|entries| entries.push(Entry { id, saved, handle: None, retired: Vec::new(), last: None, speed: 0.0 }));
    }
    id
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
            let mut path = path;
            // Renamed while it downloaded.
            if let (Some(new_name), Some(old)) = (entry.saved.rename_to.take(), path.clone()) {
                let new = renamed(&old, &new_name);
                if std::fs::rename(&old, &new).is_ok() {
                    entry.saved.name = new.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(new_name);
                    path = Some(new);
                }
            }
            // What it came to on disk (books counted chapters until now).
            if let Some(size) = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).filter(|m| m.is_file()).map(|m| m.len()) {
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

// --- Actions ---

fn pause(id: i64) {
    with_entries(|entries| {
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if !matches!(entry.saved.state, State::Active | State::Queued) {
            return;
        }
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
        Some(entry.saved.request.clone())
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
        if let Some(handle) = entry.handle.take() {
            downloader::cancel(&handle);
        }
        entry.saved.state = State::Cancelled;
        save(entry);
    });
}

/// Drops a download from the lists, and with `files`, what it saved.
fn delete(id: i64, files: bool) {
    let removed = with_entries(|entries| {
        let index = entries.iter().position(|e| e.id == id)?;
        let mut entry = entries.remove(index);
        if let Some(handle) = entry.handle.take() {
            downloader::cancel(&handle);
        }
        Some(entry)
    });
    let Some(entry) = removed else {
        return;
    };
    crate::db::delete_download(id);
    if files {
        if let Some(path) = &entry.saved.path {
            let path = std::path::Path::new(path);
            let _ = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
        }
    }
}

fn open(id: i64) {
    let found = with_entries(|entries| entries.iter().find(|e| e.id == id).map(|e| (e.saved.path.clone(), e.saved.folder.clone())));
    match found {
        Some((Some(path), _)) => crate::platform::open_file(&path),
        Some((None, folder)) => crate::platform::show_toast(&format!("Saved in {folder}")),
        None => {}
    }
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
        let Some(path) = entry.saved.path.clone() else {
            crate::platform::show_toast("This one went into a picked folder; rename it there");
            return;
        };
        let old = std::path::PathBuf::from(&path);
        let new = renamed(&old, &name);
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
                1 => "aria2/1.37.0".to_string(),
                2 => options.ua_custom.trim().to_string(),
                _ => String::new(),
            },
            subfolder: match options.category {
                false => String::new(),
                true if book => "Books".to_string(),
                true => crate::links::category_folder(file_type(entry.saved.file_type)).to_string(),
            },
        };
        entry.saved.folder = folder_display(&entry.saved.request);
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

// --- The lists ---

/// Hooks the lists and their actions up. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    let downloading = Rc::new(VecModel::<DownloadItem>::default());
    let stopped = Rc::new(VecModel::<DownloadItem>::default());
    let completed = Rc::new(VecModel::<DownloadItem>::default());
    ui.set_downloading_items(ModelRc::from(downloading.clone()));
    ui.set_stopped_items(ModelRc::from(stopped.clone()));
    ui.set_completed_items(ModelRc::from(completed.clone()));

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
            ItemAction::ChangeFolder | ItemAction::RefreshLink => {
                crate::platform::show_toast("Not available yet");
            }
        }
        refresh_now();
    });

    let refresh = move || {
        let items = items();
        let pick = |states: &[DownloadState]| -> Vec<DownloadItem> {
            items.iter().filter(|i| states.contains(&i.state)).cloned().collect()
        };
        let mut active = pick(&[DownloadState::Active]);
        active.extend(pick(&[DownloadState::Queued]));
        let mut done = pick(&[DownloadState::Completed]);
        done.reverse(); // newest first
        sync(&downloading, active);
        sync(&stopped, pick(&[DownloadState::Paused, DownloadState::Failed, DownloadState::Cancelled]));
        sync(&completed, done);
    };
    refresh();
    REFRESH.with(|r| *r.borrow_mut() = Some(Box::new(refresh.clone())));
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), refresh);
    std::mem::forget(timer);
}

thread_local! {
    static REFRESH: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

/// Shows an action's result straight away rather than at the next tick.
fn refresh_now() {
    REFRESH.with(|r| {
        if let Some(refresh) = r.borrow().as_ref() {
            refresh();
        }
    });
}

/// Acts on the selected downloads (the selection bar).
pub fn act(ids: &[i32], action: SelectionAction, delete_files: bool) {
    for &id in ids {
        let id = id as i64;
        match action {
            SelectionAction::Pause => pause(id),
            SelectionAction::Resume => resume(id),
            SelectionAction::Cancel => cancel(id),
            SelectionAction::Delete => delete(id, delete_files),
        }
    }
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

/// Every download as the lists show it, with live progress.
fn items() -> Vec<DownloadItem> {
    with_entries(|entries| {
        entries
            .iter_mut()
            .map(|entry| {
                let mut state = entry.saved.state;
                if let Some(handle) = &entry.handle {
                    if let Some((live, done, total, speed)) = downloader::progress(handle) {
                        if matches!(state, State::Active | State::Queued) {
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
                view(entry, state)
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
    let progress = if saved.total > 0 { (saved.done as f64 / saved.total as f64).min(1.0) as f32 } else { 0.0 };
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
            "aria2/1.37.0" => 1,
            _ => 2,
        },
        ua_custom: if ua == "aria2/1.37.0" { "".into() } else { ua.into() },
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
        Backend::Torrent => FileType::Torrent,
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
        FileType::Torrent => 6,
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
        6 => FileType::Torrent,
        7 => FileType::Program,
        _ => FileType::Other,
    }
}
