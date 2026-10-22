//! Add links: picks the links out of pasted text (or a .txt file), checks each
//! one in the background (name, size, type, and whether yt-dlp can extract
//! it) and keeps the pending list the UI previews them from, one at a time.
//! A picked .torrent file goes straight onto the list.
//!
//! Pending links are also kept in the database, which the main app and the
//! share popup (a separate process on Android) both read and write.

use crate::downloader::{Backend, DownloadRequest};
use crate::extractors::{self, MediaInfo, MediaKind, MediaRequest, Service};
use crate::{
    DownloaderApp, FileType, LinkBackend, LinkFormat, Links, PendingLink, ProbeState, QuickBackend,
    TorrentFile,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

thread_local! {
    // Lives on the UI thread; probes report back through invoke_from_event_loop.
    static PENDING: Rc<VecModel<PendingLink>> = Rc::new(VecModel::default());
    static NEXT_ID: Cell<i32> = const { Cell::new(1) };
    // Media links' extractor details, by link id (needed again to download).
    static MEDIA: RefCell<HashMap<i32, MediaMeta>> = RefCell::new(HashMap::new());
    // Run when the last pending link is dealt with (the share popup closes).
    static ON_EMPTY: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Hooks a UI's Links global up to this module: the main app's, or the share
/// popup's. Call once per UI, after creating it.
pub fn install<T>(ui: &T)
where
    T: ComponentHandle,
    for<'a> Links<'a>: slint::Global<'a, T>,
{
    let links = ui.global::<Links>();
    links.set_pending(PENDING.with(|m| ModelRc::from(m.clone())));
    links.set_ytdlp_available(crate::platform::ytdlp_ready());

    links.on_process_text(|text| {
        add_links_from_text(&text);
    });

    links.on_skip(|id| {
        if let Some(link) = take_pending(id) {
            crate::db::remove_pending(&link.url);
        }
    });

    links.on_start(|id, backend, format, filename, options| {
        let backend = match backend {
            LinkBackend::Torrent => Backend::Torrent,
            LinkBackend::Media => Backend::Media,
            LinkBackend::Book => Backend::Book,
            _ => Backend::Direct,
        };
        start_link(id, backend, format.to_string(), filename.to_string(), &options);
    });

    links.on_pick_folder(|id| {
        crate::platform::pick_download_folder(move |raw, display| {
            let (raw, display) = (raw.clone(), display.clone());
            // Android reports the pick on its own UI thread.
            let _ = slint::invoke_from_event_loop(move || {
                PENDING.with(|m| {
                    let Some(index) = m.iter().position(|l| l.id == id) else {
                        return;
                    };
                    let mut link = m.row_data(index).unwrap();
                    link.folder = display.into();
                    link.folder_uri = raw.into();
                    m.set_row_data(index, link);
                });
            });
        });
    });

    links.on_toggle_file(|id, index| {
        update_files(id, |file| {
            if file.index == index {
                file.selected = !file.selected;
            }
        })
    });
    links.on_select_all_files(|id, selected| update_files(id, |file| file.selected = selected));

    // "Download now": everything is handed over at once, without waiting for
    // the checks. Torrents stay torrents; for the rest, Auto lets the
    // downloader check the link itself and pick (see downloader::start).
    links.on_quick_download(|choice| {
        let links: Vec<PendingLink> = PENDING.with(|m| m.iter().collect());
        for link in links {
            let torrent = link.backends.row_data(0) == Some(LinkBackend::Torrent);
            let book = link.backends.row_data(0) == Some(LinkBackend::Book);
            let backend = match choice {
                _ if torrent => Backend::Torrent,
                // Books stay books, with the Books defaults.
                _ if book => Backend::Book,
                QuickBackend::Direct => Backend::Direct,
                QuickBackend::Media => Backend::Media,
                _ => Backend::Auto,
            };
            // Names are only known once the check is done; otherwise the
            // backend names the file.
            let filename = if link.state == ProbeState::Ready { link.filename.to_string() } else { String::new() };
            start_link(link.id, backend, String::new(), filename, &crate::preferences::link_option_defaults());
        }
    });
}

/// The main app's extras: "Add from file", and the database sync that picks
/// up links the share popup added.
pub fn install_main(ui: &DownloaderApp) {
    install(ui);

    ui.on_add_from_txt_clicked(|| {
        crate::platform::pick_link_file(|path, name| {
            // Android reports the pick on its own UI thread.
            let _ = slint::invoke_from_event_loop(move || {
                add_file(&path, &name);
            });
        });
    });

    ui.global::<Links>().on_sync(sync_from_db);
    sync_from_db();
}

/// Calls `f` once nothing is pending any more (the share popup uses this to
/// close itself).
pub fn on_empty(f: impl Fn() + 'static) {
    ON_EMPTY.with(|e| *e.borrow_mut() = Some(Box::new(f)));
}

fn next_id() -> i32 {
    NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

fn is_pending(url: &str) -> bool {
    PENDING.with(|m| m.iter().any(|l| l.url.as_str() == url))
}

/// Adds a link to the pending list and starts checking it. `remember` also
/// saves it in the database (false when it came from there).
fn add_url(url: &str, remember: bool) -> Option<i32> {
    if is_pending(url) {
        return None;
    }
    if remember {
        crate::db::add_pending(url);
    }
    let id = next_id();
    PENDING.with(|m| m.push(initial_link(id, url)));

    let url = url.to_string();
    crate::downloader::runtime().spawn(async move {
        let result = probe(&url).await;
        let _ = slint::invoke_from_event_loop(move || apply_probe(id, result));
    });
    Some(id)
}

/// Adds every link in `text` to the pending list; returns the new links' ids.
pub fn add_links_from_text(text: &str) -> Vec<i32> {
    extract_links(text).iter().filter_map(|url| add_url(url, true)).collect()
}

/// A file from "Add from file" or a share: a .torrent is added as a pending
/// torrent; anything else is read as text and its links are added.
pub fn add_file(path: &str, name: &str) -> Vec<i32> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) => {
            crate::platform::show_toast(&format!("Couldn't read {name}: {e}"));
            return Vec::new();
        }
    };
    // On Android every path we get is our own copy in the cache.
    #[cfg(target_os = "android")]
    let _ = std::fs::remove_file(path);

    // Checked by content too: Android pickers don't always keep the extension.
    let torrent = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&data);
    match torrent {
        Ok(meta) => add_torrent_file(&meta, &data, name).into_iter().collect(),
        Err(_) if name.to_ascii_lowercase().ends_with(".torrent") => {
            crate::platform::show_toast(&format!("{name} isn't a valid torrent"));
            Vec::new()
        }
        Err(_) => add_links_from_text(&String::from_utf8_lossy(&data)),
    }
}

type TorrentMeta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta;

fn add_torrent_file(meta: &TorrentMeta, data: &[u8], name: &str) -> Option<i32> {
    let url = save_torrent(meta, data, name)?;
    if is_pending(&url) {
        return None;
    }
    crate::db::add_pending(&url);
    Some(push_torrent(&url, meta, name))
}

/// Keeps a .torrent in the app's data dir and returns its path: the picked
/// or shared copy (Android's cache) doesn't last, but the download needs the
/// file later.
fn save_torrent(meta: &TorrentMeta, data: &[u8], name: &str) -> Option<String> {
    let dir = PathBuf::from(crate::platform::get_app_data_dir()).join("torrents");
    let path = dir.join(format!("{}.torrent", meta.info_hash.as_hex()));
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, data)) {
        crate::platform::show_toast(&format!("Couldn't save {name}: {e}"));
        return None;
    }
    Some(path.to_string_lossy().into_owned())
}

/// Saves the links in `text` as pending without showing them ("Add to
/// Eddy" from the share sheet); the app picks them up from the database.
/// Returns how many there were.
pub fn remember_text(text: &str) -> usize {
    let links = extract_links(text);
    for url in &links {
        crate::db::add_pending(url);
    }
    links.len()
}

/// Like `remember_text`, for a shared file: a .torrent or a text file.
pub fn remember_file(path: &str, name: &str) -> usize {
    let Ok(data) = std::fs::read(path) else {
        return 0;
    };
    #[cfg(target_os = "android")]
    let _ = std::fs::remove_file(path);
    match TorrentMeta::parse(&data) {
        Ok(meta) => match save_torrent(&meta, &data, name) {
            Some(url) => {
                crate::db::add_pending(&url);
                1
            }
            None => 0,
        },
        Err(_) if name.to_ascii_lowercase().ends_with(".torrent") => 0,
        Err(_) => remember_text(&String::from_utf8_lossy(&data)),
    }
}

/// A saved .torrent file (its path is the pending link's url).
fn is_saved_torrent(url: &str) -> bool {
    url.starts_with('/') && url.ends_with(".torrent")
}

fn push_torrent(url: &str, meta: &TorrentMeta, name: &str) -> i32 {
    let id = next_id();
    let files = torrent_files(meta);
    let filename = clean_name(&meta.info.name);
    PENDING.with(|m| {
        m.push(PendingLink {
            id,
            url: url.into(),
            state: ProbeState::Ready,
            error: SharedString::new(),
            filename: if filename.is_empty() { name.into() } else { filename.into() },
            file_type: FileType::Torrent,
            size: match meta.total_size() {
                0 => SharedString::new(),
                size => format_size(size).into(),
            },
            source: "Torrent file".into(),
            backends: backends_model(&[LinkBackend::Torrent]),
            formats: ModelRc::new(VecModel::default()),
            files_selected: files.len() as i32,
            files: files_model(&files),
            folder: usual_folder_display().into(),
            folder_uri: SharedString::new(),
            service: SharedString::new(),
            thumbnail: slint::Image::default(),
            media_kind: 0,
            audio_formats: ModelRc::new(VecModel::default()),
            ..Default::default()
        })
    });
    id
}

/// Makes the pending list match the database: links the share popup added
/// appear, and ones it started or skipped go away.
fn sync_from_db() {
    let saved = crate::db::pending_links();
    let gone: Vec<i32> = PENDING.with(|m| {
        m.iter().filter(|l| !saved.iter().any(|u| u == l.url.as_str())).map(|l| l.id).collect()
    });
    for id in gone {
        take_pending(id);
    }
    for url in saved {
        if is_pending(&url) {
            continue;
        }
        if is_saved_torrent(&url) {
            let meta = std::fs::read(&url).ok().and_then(|data| TorrentMeta::parse(&data).ok());
            match meta {
                Some(meta) => {
                    push_torrent(&url, &meta, "");
                }
                // The saved file went missing; nothing left to download.
                None => crate::db::remove_pending(&url),
            }
        } else {
            add_url(&url, false);
        }
    }
}

/// Moves the given links to the front of the pending list, so the preview
/// shows them first.
pub fn bring_to_front(ids: &[i32]) {
    PENDING.with(|m| {
        for (target, &id) in ids.iter().enumerate() {
            if let Some(index) = m.iter().position(|l| l.id == id) {
                let link = m.remove(index);
                m.insert(target.min(m.row_count()), link);
            }
        }
    });
}

/// Hands a pending link to the downloader and drops it from the list.
fn start_link(id: i32, backend: Backend, format: String, filename: String, options: &crate::LinkOptions) {
    let Some(link) = PENDING.with(|m| m.iter().find(|l| l.id == id)) else {
        return;
    };
    // Only the torrent's chosen files, unless that's all of them.
    let select_files = if backend == Backend::Torrent && link.files_selected < link.files.row_count() as i32 {
        let chosen: Vec<String> = link.files.iter().filter(|f| f.selected).map(|f| f.index.to_string()).collect();
        if chosen.is_empty() {
            crate::platform::show_toast("Pick at least one file to download");
            return;
        }
        chosen.join(",")
    } else {
        String::new()
    };
    // Media downloads hand the extractor's details over as JSON; books their
    // options.
    let format = match backend {
        Backend::Media => serde_json::to_string(&media_request(id, &link, &format)).unwrap_or_default(),
        Backend::Book => serde_json::to_string(&book_options(&format)).unwrap_or_default(),
        _ => format,
    };
    let request = DownloadRequest {
        url: link.url.to_string(),
        backend,
        format,
        filename,
        select_files,
        folder: link.folder_uri.to_string(),
        settings: download_settings(options, &link),
        id: 0,
    };
    match crate::platform::start_download(&request) {
        Ok(()) => {
            take_pending(id);
            crate::db::remove_pending(&request.url);
        }
        Err(e) => crate::platform::show_toast(&format!("Couldn't start the download: {e}")),
    }
}

/// The preview's options as the downloader's settings.
fn download_settings(options: &crate::LinkOptions, link: &PendingLink) -> crate::downloader::DownloadSettings {
    crate::downloader::DownloadSettings {
        max_speed: options.speed_kbps.max(0) as u64 * 1024,
        connections: options.connections.clamp(1, 16) as u16,
        user_agent: match options.ua_mode {
            1 => "aria2/1.37.0".to_string(),
            2 => options.ua_custom.trim().to_string(),
            _ => String::new(),
        },
        subfolder: match options.category {
            false => String::new(),
            true if link.backends.row_data(0) == Some(LinkBackend::Book) => "Books".to_string(),
            true => category_folder(link.file_type).to_string(),
        },
    }
}

/// The folder a category's downloads go in, with "Category folders" on.
pub fn category_folder(file_type: FileType) -> &'static str {
    match file_type {
        FileType::Video => "Video",
        FileType::Audio => "Music",
        FileType::Image => "Pictures",
        FileType::Document => "Documents",
        FileType::Archive => "Compressed",
        FileType::DiskImage | FileType::Program => "Programs",
        FileType::Torrent => "Torrents",
        _ => "Other",
    }
}

/// Applies `change` to each of a pending torrent's files, then recounts the
/// selected ones.
fn update_files(id: i32, change: impl Fn(&mut TorrentFile)) {
    PENDING.with(|m| {
        let Some(index) = m.iter().position(|l| l.id == id) else {
            return;
        };
        let mut link = m.row_data(index).unwrap();
        for row in 0..link.files.row_count() {
            let mut file = link.files.row_data(row).unwrap();
            change(&mut file);
            link.files.set_row_data(row, file);
        }
        link.files_selected = link.files.iter().filter(|f| f.selected).count() as i32;
        m.set_row_data(index, link);
    });
}

fn take_pending(id: i32) -> Option<PendingLink> {
    let link = PENDING.with(|m| {
        let index = m.iter().position(|l| l.id == id)?;
        Some(m.remove(index))
    })?;
    MEDIA.with(|m| m.borrow_mut().remove(&id));
    if PENDING.with(|m| m.row_count() == 0) {
        ON_EMPTY.with(|e| {
            if let Some(f) = e.borrow().as_ref() {
                f();
            }
        });
    }
    Some(link)
}

// --- Finding links ---

/// Every link in `text`, in order, without duplicates. Magnets are found as
/// whole words; anything else that looks like a web address counts too, with
/// https:// assumed when the scheme is missing.
pub fn extract_links(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut rest = String::with_capacity(text.len());

    for word in text.split_whitespace() {
        let word = word.trim_matches(|c| matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | ','));
        if word.len() > 8 && word[..7].eq_ignore_ascii_case("magnet:") {
            found.push(word.to_string());
        } else {
            // Kept apart from magnets so their tracker URLs aren't picked up.
            rest.push_str(word);
            rest.push(' ');
        }
    }

    let mut finder = linkify::LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);
    finder.url_must_have_scheme(false);
    for link in finder.links(&rest) {
        let link = link.as_str();
        if link.contains("://") {
            found.push(link.to_string());
        } else {
            found.push(format!("https://{link}"));
        }
    }

    let mut unique = Vec::with_capacity(found.len());
    for link in found {
        if !unique.contains(&link) {
            unique.push(link);
        }
    }
    unique
}

// --- What the UI shows before and after the check ---

fn initial_link(id: i32, url: &str) -> PendingLink {
    let magnet = is_magnet(url);
    let filename = if magnet { magnet_name(url) } else { name_from_url(url) }.unwrap_or_default();
    PendingLink {
        id,
        url: url.into(),
        state: ProbeState::Probing,
        error: SharedString::new(),
        file_type: if magnet { FileType::Torrent } else { file_type_for(&filename, None) },
        filename: filename.into(),
        size: SharedString::new(),
        source: if magnet { "BitTorrent".into() } else { host_of(url).into() },
        backends: backends_model(&[if magnet { LinkBackend::Torrent } else { LinkBackend::Direct }]),
        formats: ModelRc::new(VecModel::default()),
        files: ModelRc::new(VecModel::default()),
        files_selected: 0,
        folder: usual_folder_display().into(),
        folder_uri: SharedString::new(),
        service: SharedString::new(),
        thumbnail: slint::Image::default(),
        media_kind: 0,
        audio_formats: ModelRc::new(VecModel::default()),
        ..Default::default()
    }
}

/// How the folder downloads usually go to is shown: the one picked in
/// settings, or the default.
pub fn usual_folder_display() -> String {
    crate::platform::get_pref(crate::preferences::DOWNLOAD_DIR_DISPLAY, "")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(crate::storage::default_display)
}

fn backends_model(backends: &[LinkBackend]) -> ModelRc<LinkBackend> {
    ModelRc::new(VecModel::from(backends.to_vec()))
}

/// What a check found out. Plain data so it can cross from the runtime threads.
struct Probe {
    filename: String,
    file_type: FileType,
    size: String,
    backends: Vec<LinkBackend>,
    formats: Vec<Format>,
    /// A torrent's files, or a post's / playlist's items (1-based index,
    /// name, length); empty otherwise.
    files: Vec<(usize, String, u64)>,
    /// What an extractor found, for media links.
    media: Option<MediaMeta>,
    /// A web novel's chapter counts, for book links.
    book: Option<BookMeta>,
}

#[derive(Clone, Debug)]
struct BookMeta {
    site: crate::novels::Site,
    chapters: usize,
    locked: usize,
    cover: Option<PathBuf>,
}

/// A media link's extras: who found it, what it is, and what's needed to
/// download it later.
#[derive(Clone, Debug)]
struct MediaMeta {
    service: Service,
    kind: MediaKind,
    /// Bitrates for converting to audio.
    audio_formats: Vec<(String, String, String)>,
    thumbnail: Option<PathBuf>,
    info_json: Option<PathBuf>,
}

struct Format {
    selector: String,
    label: String,
    detail: String,
}

fn apply_probe(id: i32, result: Result<Probe, String>) {
    PENDING.with(|m| {
        let Some(index) = m.iter().position(|l| l.id == id) else {
            return; // skipped or started while it was being checked
        };
        let mut link = m.row_data(index).unwrap();
        match result {
            Ok(probe) => {
                crate::platform::trigger_haptic(crate::platform::Haptic::LinkChecked);
                link.state = ProbeState::Ready;
                if !probe.filename.is_empty() {
                    link.filename = probe.filename.into();
                }
                link.file_type = probe.file_type;
                link.size = probe.size.into();
                link.backends = backends_model(&probe.backends);
                link.formats = ModelRc::new(VecModel::from(
                    probe
                        .formats
                        .into_iter()
                        .map(|f| LinkFormat {
                            selector: f.selector.into(),
                            label: f.label.into(),
                            detail: f.detail.into(),
                        })
                        .collect::<Vec<_>>(),
                ));
                link.files_selected = probe.files.len() as i32;
                link.files = files_model(&probe.files);
                if let Some(media) = probe.media {
                    link.service = media.service.label().into();
                    link.media_kind = match media.kind {
                        MediaKind::Video => 1,
                        MediaKind::Audio => 2,
                        MediaKind::Image => 3,
                    };
                    link.audio_formats = ModelRc::new(VecModel::from(
                        media
                            .audio_formats
                            .iter()
                            .map(|(selector, label, detail)| LinkFormat {
                                selector: selector.into(),
                                label: label.into(),
                                detail: detail.into(),
                            })
                            .collect::<Vec<_>>(),
                    ));
                    if let Some(thumb) = media.thumbnail.as_deref().and_then(|p| slint::Image::load_from_path(p).ok()) {
                        link.thumbnail = thumb;
                    }
                    MEDIA.with(|m| m.borrow_mut().insert(id, media));
                }
                if let Some(book) = probe.book {
                    let defaults = crate::preferences::novel_defaults();
                    link.service = book.site.label().into();
                    link.book_chapters = book.chapters as i32;
                    link.book_locked = book.locked as i32;
                    link.book_notes = defaults.author_notes;
                    link.book_images = defaults.images;
                    link.book_note_images = defaults.author_note_images;
                    link.book_small_images = defaults.small_images;
                    if let Some(thumb) = book.cover.as_deref().and_then(|p| slint::Image::load_from_path(p).ok()) {
                        link.thumbnail = thumb;
                    }
                }
            }
            Err(error) => {
                link.state = ProbeState::Failed;
                link.error = error.into();
            }
        }
        m.set_row_data(index, link);
    });
}

// --- Checking a link ---

/// The best way to download `url`, for "Download now" with Auto: the same
/// check the preview runs, then its first backend and best quality.
pub async fn pick_backend(url: &str) -> (Backend, String) {
    match probe(url).await {
        Ok(probe) => {
            let backend = match probe.backends.first() {
                Some(LinkBackend::Torrent) => Backend::Torrent,
                Some(LinkBackend::Media) => Backend::Media,
                Some(LinkBackend::Book) => Backend::Book,
                _ => Backend::Direct,
            };
            // Best quality; music stays audio.
            let format = match (backend, &probe.media) {
                // Every chapter, with the Books defaults.
                (Backend::Book, _) => serde_json::to_string(&crate::preferences::novel_defaults()).unwrap_or_default(),
                (Backend::Media, Some(media)) => serde_json::to_string(&MediaRequest {
                    service: Some(media.service),
                    info_json: media.info_json.clone(),
                    image: media.kind == MediaKind::Image,
                    quality: "best".to_string(),
                    audio_codec: if media.kind == MediaKind::Audio { "m4a".to_string() } else { String::new() },
                    ..Default::default()
                })
                .unwrap_or_default(),
                _ => String::new(),
            };
            (backend, format)
        }
        // Let the download itself report what's wrong.
        Err(_) => (Backend::Direct, String::new()),
    }
}

/// One link is checked at a time: sites (and yt-dlp) turn away a burst of
/// requests, which made most of a batch of shared links fail. The others
/// wait their turn, in the order they were added.
static PROBING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn probe(url: &str) -> Result<Probe, String> {
    let _turn = PROBING.lock().await;
    probe_now(url).await
}

async fn probe_now(url: &str) -> Result<Probe, String> {
    if is_magnet(url) {
        // aria2 fetches the torrent's metadata from peers, which can take up
        // to a minute.
        let (size, files) = crate::downloader::engine().check_files(url, crate::downloader::aria2::PROBE_TIMEOUT).await?;
        return Ok(Probe {
            filename: magnet_name(url).unwrap_or_default(),
            file_type: FileType::Torrent,
            size: format_size(size),
            backends: vec![LinkBackend::Torrent],
            formats: Vec::new(),
            files: files.into_iter().map(|f| (f.index, f.path, f.length)).collect(),
            media: None,
            book: None,
        });
    }

    // A web novel on a reading site Eddy knows: saved as a book.
    if crate::novels::site_for(url).is_some() {
        let book = crate::novels::book(url).await?;
        let locked = book.chapters.iter().filter(|c| c.locked).count();
        // The largest cover: tapping the banner shows all of it.
        let cover = match book.covers.first() {
            Some(cover) => fetch_thumbnail(cover).await,
            None => None,
        };
        return Ok(Probe {
            filename: crate::novels::default_name(&book),
            file_type: FileType::Document,
            size: String::new(),
            backends: vec![LinkBackend::Book],
            formats: Vec::new(),
            files: Vec::new(),
            media: None,
            book: Some(BookMeta { site: book.site, chapters: book.chapters.len(), locked, cover }),
        });
    }

    // TikTok, Instagram, Pinterest: their own extractors (yt-dlp if those fail).
    if extractors::native_service(url).is_some() {
        let info = extractors::probe(url).await?;
        return Ok(media_probe(info).await);
    }

    let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        if scheme == "ftp" || scheme == "sftp" {
            let filename = name_from_url(url).unwrap_or_default();
            return Ok(Probe {
                file_type: file_type_for(&filename, None),
                filename,
                size: String::new(),
                backends: vec![LinkBackend::Direct],
                formats: Vec::new(),
                files: Vec::new(),
                media: None,
                book: None,
            });
        }
        return Err(format!("Unsupported link type ({scheme})"));
    }

    let head = http_head(url).await;
    let page_or_error = match &head {
        Ok(info) => info.status >= 400 || info.is_page(),
        Err(_) => true,
    };

    // A web page (or a server that turns us away, as some media sites do with
    // plain requests): see whether yt-dlp can get something out of it.
    if page_or_error && crate::platform::ytdlp_available() {
        if let Ok(info) = extractors::ytdlp::get_info(url).await {
            if !info.items.is_empty() {
                return Ok(media_probe(info).await);
            }
        }
    }

    // The quick check couldn't tell (refused, error status, or no size):
    // ask aria2, which makes the real request with its own redirect and
    // retry handling.
    let quick = head.as_ref().ok().filter(|info| info.status < 400 && info.length.is_some());
    if quick.is_none() {
        let checked = crate::downloader::engine().check_files(url, DIRECT_ARIA2_TIMEOUT).await;
        if let Ok((size, files)) = checked {
            let filename = head
                .as_ref()
                .ok()
                .and_then(|info| info.disposition_name.clone())
                .or_else(|| files.first().and_then(|f| f.path.rsplit('/').next().map(str::to_string)))
                .filter(|n| !n.is_empty())
                .or_else(|| name_from_url(url))
                .map(|n| clean_name(&n))
                .unwrap_or_default();
            let is_torrent = filename.to_ascii_lowercase().ends_with(".torrent");
            if is_torrent {
                return probe_torrent_link(url, &filename).await;
            }
            return Ok(Probe {
                file_type: file_type_for(&filename, None),
                filename,
                size: format_size(size),
                backends: vec![LinkBackend::Direct],
                formats: Vec::new(),
                files: Vec::new(),
                media: None,
                book: None,
            });
        }
    }

    let info = head?;
    if info.status >= 400 {
        return Err(info.status_text());
    }
    let filename = info
        .disposition_name
        .clone()
        .or_else(|| name_from_url(&info.final_url))
        .unwrap_or_default();
    let is_torrent = info.content_type.as_deref() == Some("application/x-bittorrent")
        || filename.to_ascii_lowercase().ends_with(".torrent");
    if is_torrent {
        return probe_torrent_link(url, &filename).await;
    }
    let size = info.length;
    Ok(Probe {
        file_type: if is_torrent { FileType::Torrent } else { file_type_for(&filename, info.content_type.as_deref()) },
        filename,
        size: size.map(format_size).unwrap_or_default(),
        backends: if is_torrent {
            vec![LinkBackend::Torrent, LinkBackend::Direct]
        } else {
            vec![LinkBackend::Direct]
        },
        formats: Vec::new(),
        files: Vec::new(),
        media: None,
        book: None,
    })
}

/// A link to a .torrent: fetched and read here (it's small), so the torrent's
/// name, size and files are known straight away. Falls back to aria2 if the
/// file can't be read.
async fn probe_torrent_link(url: &str, filename: &str) -> Result<Probe, String> {
    const MAX_TORRENT_BYTES: usize = 16 * 1024 * 1024;
    let fetched = async {
        let response = http_client().get(url).send().await.ok()?.error_for_status().ok()?;
        let data = response.bytes().await.ok()?;
        (data.len() <= MAX_TORRENT_BYTES).then_some(data)
    }
    .await;
    let parsed = fetched.as_deref().and_then(|data| TorrentMeta::parse(data).ok());
    let (name, size, files) = match parsed {
        Some(meta) => (clean_name(&meta.info.name), meta.total_size(), torrent_files(&meta)),
        None => {
            let (size, files) = crate::downloader::engine().check_files(url, crate::downloader::aria2::PROBE_TIMEOUT).await?;
            let name = filename.strip_suffix(".torrent").unwrap_or(filename).to_string();
            (name, size, files.into_iter().map(|f| (f.index, f.path, f.length)).collect())
        }
    };
    Ok(Probe {
        // The torrent's own name: ".torrent" only goes back on when it's
        // downloaded Direct (the preview adds it).
        filename: if name.is_empty() { filename.strip_suffix(".torrent").unwrap_or(filename).to_string() } else { name },
        file_type: FileType::Torrent,
        size: format_size(size),
        backends: vec![LinkBackend::Torrent, LinkBackend::Direct],
        formats: Vec::new(),
        files,
        media: None,
        book: None,
    })
}

/// A torrent's files in its own order, numbered from 1 like aria2's
/// select-file.
fn torrent_files(meta: &TorrentMeta) -> Vec<(usize, String, u64)> {
    match &meta.info.files {
        Some(files) => files
            .iter()
            .enumerate()
            .map(|(i, f)| (i + 1, f.path.join("/"), f.length))
            .collect(),
        None => vec![(1, meta.info.name.clone(), meta.info.length.unwrap_or(0))],
    }
}

fn files_model(files: &[(usize, String, u64)]) -> ModelRc<TorrentFile> {
    ModelRc::new(VecModel::from(
        files
            .iter()
            .map(|(index, path, length)| TorrentFile {
                index: *index as i32,
                path: path.into(),
                size: format_size(*length).into(),
                selected: true,
            })
            .collect::<Vec<_>>(),
    ))
}

/// How long a direct link's aria2 check may take before giving up (magnets
/// get longer: they wait for peers).
const DIRECT_ARIA2_TIMEOUT: Duration = Duration::from_secs(20);

const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Mobile Safari/537.36";

struct HeadInfo {
    status: u16,
    reason: Option<&'static str>,
    final_url: String,
    content_type: Option<String>,
    length: Option<u64>,
    disposition_name: Option<String>,
}

impl HeadInfo {
    fn is_page(&self) -> bool {
        matches!(self.content_type.as_deref(), Some("text/html") | Some("application/xhtml+xml"))
    }
    fn status_text(&self) -> String {
        match self.reason {
            Some(reason) => format!("{} {}", self.status, reason),
            None => format!("Server replied {}", self.status),
        }
    }
}

/// Certificates are checked by the platform: Android's own verifier
/// (set up in platform::init), the OS store elsewhere, so certificates the
/// user installed are trusted too.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // reqwest is built without a crypto provider; share aria2's (ring).
        aria2_core::http::client_pool::ensure_rustls_provider();
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            // Some CDNs turn away unknown clients; look like a browser.
            .user_agent(BROWSER_USER_AGENT)
            .build()
            .expect("failed to build the HTTP client")
    })
}

/// Response headers for `url`, following redirects. Servers that refuse HEAD
/// get a GET for the first byte instead.
async fn http_head(url: &str) -> Result<HeadInfo, String> {
    let client = http_client();
    let mut response = client.head(url).send().await.map_err(describe_error)?;
    let mut ranged = false;
    if response.status().as_u16() >= 400 {
        if let Ok(retry) = client.get(url).header("Range", "bytes=0-0").send().await {
            response = retry;
            ranged = true;
        }
    }

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let content_type = header("content-type")
        .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase());
    let length = if ranged && response.status().as_u16() == 206 {
        // "bytes 0-0/12345"
        header("content-range").and_then(|v| v.rsplit('/').next()?.parse().ok())
    } else {
        header("content-length").and_then(|v| v.parse().ok())
    };

    Ok(HeadInfo {
        status: response.status().as_u16(),
        reason: response.status().canonical_reason(),
        final_url: response.url().to_string(),
        content_type,
        length: length.filter(|&l| l > 1 || !ranged),
        disposition_name: header("content-disposition").and_then(|v| disposition_name(&v)),
    })
}

fn describe_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "The server took too long to answer".to_string()
    } else if error.is_connect() {
        "Couldn't connect to the server".to_string()
    } else {
        error.to_string()
    }
}

/// The file name from a Content-Disposition header, preferring the
/// `filename*=UTF-8''…` form.
fn disposition_name(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';').map(str::trim) {
        if let Some(encoded) = part.strip_prefix("filename*=") {
            let encoded = encoded.splitn(3, '\'').nth(2).unwrap_or(encoded);
            let name = percent_encoding::percent_decode_str(encoded).decode_utf8_lossy();
            if !name.is_empty() {
                return Some(clean_name(&name));
            }
        } else if let Some(name) = part.strip_prefix("filename=") {
            plain = Some(clean_name(name.trim_matches('"')));
        }
    }
    plain.filter(|n| !n.is_empty())
}

// --- Media (extractors) ---

/// "1080p" for a "1920:1080" quality (the shorter side, so portrait videos
/// read the same), otherwise the key as it is.
fn quality_label(key: &str, kind: MediaKind) -> String {
    match key.split_once(':').and_then(|(w, h)| Some((w.parse::<u64>().ok()?, h.parse::<u64>().ok()?))) {
        Some((w, h)) if kind == MediaKind::Image => format!("{w}×{h}"),
        Some((w, h)) => format!("{}p", w.min(h)),
        None => match key {
            "best" | "default" => "Best".to_string(),
            "original" => "Original".to_string(),
            other => other.to_string(),
        },
    }
}

fn approx(bytes: u64) -> String {
    if bytes > 0 { format!("~{}", format_size(bytes)) } else { String::new() }
}

/// Turns what an extractor found into the preview's data, fetching the
/// thumbnail on the way.
async fn media_probe(info: MediaInfo) -> Probe {
    let first = info.items[0].clone();
    let several = info.items.len() > 1;
    let join = |parts: &[&str]| parts.iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(" · ");

    let formats = first
        .qualities
        .iter()
        .map(|q| {
            let dims = q.key.split_once(':').map(|(w, h)| format!("{w}×{h}")).unwrap_or_default();
            let dims = if first.kind == MediaKind::Image { String::new() } else { dims };
            Format {
                selector: q.key.clone(),
                label: quality_label(&q.key, first.kind),
                detail: join(&[&dims, &approx(q.size)]),
            }
        })
        .collect();
    let audio_formats = if first.kind == MediaKind::Image {
        Vec::new()
    } else {
        extractors::audio_presets(first.duration)
            .into_iter()
            .map(|q| {
                let kbps = q.key.trim_end_matches('k').to_string();
                (kbps.clone(), format!("{kbps} kbps"), approx(q.size))
            })
            .collect()
    };
    let total: u64 = info.items.iter().map(|i| i.qualities.first().map(|q| q.size).unwrap_or(0)).sum();
    let files = if several {
        info.items
            .iter()
            .map(|item| {
                let kind = match item.kind {
                    MediaKind::Video => "Video",
                    MediaKind::Audio => "Audio",
                    MediaKind::Image => "Image",
                };
                let name = item.caption.as_deref().map(clean_name).filter(|c| !c.is_empty());
                let name = match name {
                    Some(c) if info.service == Service::YtDlp => c,
                    _ => format!("{} {} · {kind}", info.service.label(), item.index + 1),
                };
                (item.index + 1, name, item.qualities.first().map(|q| q.size).unwrap_or(0))
            })
            .collect()
    } else {
        Vec::new()
    };

    let thumbnail = fetch_thumbnail(&first.thumbnail).await;
    let filename = first.caption.as_deref().map(clean_name).filter(|c| !c.is_empty()).unwrap_or(info.id.clone());
    Probe {
        filename: filename.chars().take(150).collect(),
        file_type: match first.kind {
            MediaKind::Video => FileType::Video,
            MediaKind::Audio => FileType::Audio,
            MediaKind::Image => FileType::Image,
        },
        size: approx(total),
        backends: if info.service == Service::YtDlp {
            vec![LinkBackend::Media, LinkBackend::Direct]
        } else {
            vec![LinkBackend::Media]
        },
        formats,
        files,
        media: Some(MediaMeta {
            service: info.service,
            kind: first.kind,
            audio_formats,
            thumbnail,
            info_json: info.info_json,
        }),
        book: None,
    }
}

/// Saves a thumbnail in the app's cache folder for the preview; None if it
/// couldn't be fetched (or isn't a JPEG / PNG, which is what the UI shows).
async fn fetch_thumbnail(url: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    if url.is_empty() {
        return None;
    }
    let response = extractors::http::client().get(url).send().await.ok()?.error_for_status().ok()?;
    let data = response.bytes().await.ok()?;
    let ext = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else {
        return None;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    let dir = PathBuf::from(crate::platform::get_app_data_dir()).join("thumbs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{:016x}.{ext}", hasher.finish()));
    std::fs::write(&path, &data).ok()?;
    Some(path)
}

/// The preview's book options, "book|<from>|<to>|<notes>|<images>|<note
/// images>|<small images>" (numbers, 1/0 switches), as NovelOptions.
fn book_options(format: &str) -> crate::novels::NovelOptions {
    let mut options = crate::preferences::novel_defaults();
    let parts: Vec<&str> = format.split('|').collect();
    if let ["book", from, to, notes, images, note_images, small] = parts.as_slice() {
        options.from = from.parse().unwrap_or(1);
        options.to = to.parse().unwrap_or(0);
        options.author_notes = *notes == "1";
        options.images = *images == "1";
        options.author_note_images = *note_images == "1";
        options.small_images = *small == "1";
    }
    options
}

/// What a media download asks the downloader for: the extractor, the
/// items, and the quality the preview passed (`format`: a quality key, or
/// "audio|<kbps>|<mp3/m4a>" for converting to audio).
fn media_request(id: i32, link: &PendingLink, format: &str) -> MediaRequest {
    let meta = MEDIA.with(|m| m.borrow().get(&id).cloned());
    let mut request = MediaRequest {
        service: meta.as_ref().map(|m| m.service),
        info_json: meta.as_ref().and_then(|m| m.info_json.clone()),
        image: meta.as_ref().is_some_and(|m| m.kind == MediaKind::Image),
        quality: "best".to_string(),
        ..Default::default()
    };
    match format.split('|').collect::<Vec<_>>().as_slice() {
        ["audio", kbps, codec] => {
            request.audio_codec = codec.to_string();
            request.audio_kbps = kbps.parse().unwrap_or(0);
        }
        [quality] if !quality.is_empty() => request.quality = quality.to_string(),
        _ => {}
    }
    // Music sites: audio is all there is.
    if request.audio_codec.is_empty() && meta.as_ref().is_some_and(|m| m.kind == MediaKind::Audio) {
        request.audio_codec = "m4a".to_string();
    }
    if link.files_selected < link.files.row_count() as i32 {
        request.items = link.files.iter().filter(|f| f.selected).map(|f| (f.index - 1) as usize).collect();
    }
    request
}

// --- Small helpers ---

fn is_magnet(url: &str) -> bool {
    url.len() > 7 && url[..7].eq_ignore_ascii_case("magnet:")
}

/// The display name (`dn=`) of a magnet link.
fn magnet_name(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("dn="))
        .map(|dn| clean_name(&percent_encoding::percent_decode_str(&dn.replace('+', " ")).decode_utf8_lossy()))
        .filter(|n| !n.is_empty())
}

/// The last path segment of a URL, decoded.
fn name_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.rev().find(|s| !s.is_empty())?;
    let name = clean_name(&percent_encoding::percent_decode_str(last).decode_utf8_lossy());
    (!name.is_empty()).then_some(name)
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_default()
}

/// Makes a name safe to save under: no path separators or control characters.
fn clean_name(name: &str) -> String {
    name.chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The list icon for a file, from its extension or else its content type.
pub fn file_type_for(name: &str, content_type: Option<&str>) -> FileType {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "m4v" | "flv" | "wmv" | "ts" | "3gp" => FileType::Video,
        "mp3" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wav" | "wma" | "alac" => FileType::Audio,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "heic" | "avif" | "tiff" => FileType::Image,
        "pdf" | "doc" | "docx" | "odt" | "txt" | "rtf" | "epub" | "xls" | "xlsx" | "ppt" | "pptx" | "md" => FileType::Document,
        "zip" | "rar" | "7z" | "tar" | "gz" | "xz" | "bz2" | "zst" | "tgz" => FileType::Archive,
        "iso" | "img" | "dmg" => FileType::DiskImage,
        "torrent" => FileType::Torrent,
        "exe" | "msi" | "apk" | "deb" | "rpm" | "appimage" | "pkg" | "sh" => FileType::Program,
        _ => match content_type.and_then(|t| t.split('/').next()) {
            Some("video") => FileType::Video,
            Some("audio") => FileType::Audio,
            Some("image") => FileType::Image,
            Some("text") => FileType::Document,
            _ => FileType::Other,
        },
    }
}

/// "1.2 GB", "52 MB", "4.8 MB": one decimal below 100, none from there.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        let text = format!("{:.1}", value);
        format!("{} {}", text.trim_end_matches(".0"), UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links_in_text() {
        let text = "grab https://example.com/a.zip and example.org/b.iso, also\n\
                    <magnet:?xt=urn:btih:abc&dn=Some+Thing&tr=http://tracker.example/announce> \
                    and https://example.com/a.zip again";
        assert_eq!(
            extract_links(text),
            vec![
                "magnet:?xt=urn:btih:abc&dn=Some+Thing&tr=http://tracker.example/announce",
                "https://example.com/a.zip",
                "https://example.org/b.iso",
            ]
        );
    }

    #[test]
    fn names() {
        assert_eq!(name_from_url("https://x.org/dir/My%20File.pdf?x=1").as_deref(), Some("My File.pdf"));
        assert_eq!(magnet_name("magnet:?xt=urn:btih:abc&dn=Some+Thing").as_deref(), Some("Some Thing"));
        assert_eq!(disposition_name("attachment; filename=\"a b.zip\"").as_deref(), Some("a b.zip"));
        assert_eq!(disposition_name("attachment; filename=x; filename*=UTF-8''%C3%A9t%C3%A9.mp3").as_deref(), Some("été.mp3"));
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(52 * 1024 * 1024), "52 MB");
        assert_eq!(format_size(4_800_000), "4.6 MB");
        assert_eq!(format_size(1_288_490_189), "1.2 GB");
        assert_eq!(format_size(730_000_000), "696 MB");
    }
}
