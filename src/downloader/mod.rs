pub mod aria2;
pub mod book;
pub mod media;

use aria2::AriaEngineClient;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group_man::RequestGroupMan;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone)]
pub struct DownloadClient {}

/// How a download is fetched (same order as the UI's LinkBackend).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Backend {
    #[default]
    Direct,
    Torrent,
    /// A page's media, through an extractor; `format` is a MediaRequest.
    Media,
    /// Check the link first and use whatever suits it best.
    Auto,
    /// A web novel saved as an EPUB; `format` is a novels::NovelOptions.
    Book,
}

impl Backend {
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Backend::Torrent,
            2 => Backend::Media,
            3 => Backend::Auto,
            4 => Backend::Book,
            _ => Backend::Direct,
        }
    }
    pub fn index(self) -> i32 {
        match self {
            Backend::Direct => 0,
            Backend::Torrent => 1,
            Backend::Media => 2,
            Backend::Auto => 3,
            Backend::Book => 4,
        }
    }
}

/// Everything needed to start one download.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DownloadRequest {
    /// Its record in the downloads lists (downloads.rs); 0 for a new one,
    /// which gets a record when it starts.
    pub id: i64,
    /// A URL or magnet, or the path of a saved .torrent file.
    pub url: String,
    pub backend: Backend,
    /// yt-dlp format selector; empty for the other backends.
    pub format: String,
    /// File name to save as (yt-dlp: without extension); empty to let the
    /// backend choose. Ignored for torrents, whose files keep their names.
    pub filename: String,
    /// Torrent files to download, as aria2's 1-based list ("1,3"); empty for
    /// all of them.
    pub select_files: String,
    /// Where to save: a folder picked for this download (Android tree URI or
    /// desktop path), or "" for the usual one. See storage::resolve.
    pub folder: String,
    /// Speed, connections, user agent and category folder (Settings →
    /// Downloads, or the preview's options for this one).
    pub settings: DownloadSettings,
}

/// How a download goes about it. Each backend uses what it can: aria2
/// (direct downloads, torrents) all of it; yt-dlp the speed and fragments at
/// once; the native extractors the speed; books none of it but the folder.
/// The user agent is only aria2's: the extractors' sites need their own.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DownloadSettings {
    /// Bytes per second; 0 for no limit.
    pub max_speed: u64,
    /// Connections at once (1–16); 0 for the usual 16.
    pub connections: u16,
    /// "" for aria2's own; otherwise what direct downloads say they are.
    pub user_agent: String,
    /// A folder inside the save folder for the file's category ("Video"…);
    /// "" for none.
    pub subfolder: String,
}

impl DownloadSettings {
    pub fn connections(&self) -> u16 {
        if self.connections == 0 { 16 } else { self.connections.clamp(1, 16) }
    }

    /// The user agent direct downloads send, if not aria2's own.
    pub fn user_agent(&self) -> Option<&str> {
        let ua = self.user_agent.trim();
        (!ua.is_empty()).then_some(ua)
    }
}

/// The runtime background work runs on: the aria2 engine and link checks.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("eddy-worker")
            .enable_all()
            .build()
            .expect("failed to start the background runtime")
    })
}

static ENGINE: OnceLock<AriaEngineClient> = OnceLock::new();

/// The app's aria2 engine, started on first use and kept running.
pub fn engine() -> &'static AriaEngineClient {
    ENGINE.get_or_init(|| {
        let _context = runtime().enter();
        let group_man = Arc::new(RequestGroupMan::new());
        let mut engine = DownloadEngine::new();
        engine.set_request_group_man(Arc::clone(&group_man));
        // Keep the loop going when nothing is downloading.
        engine.set_keep_alive(true);
        let cmd_tx = engine.engine_command_sender();
        // The engine keeps its own shutdown sender, so it runs until the app exits.
        runtime().spawn(async move {
            if let Err(e) = engine.run().await {
                eprintln!("aria2 engine stopped: {e}");
            }
        });
        AriaEngineClient::new(group_man, cmd_tx)
    })
}

/// Auto downloads still being checked before they start.
static CHOOSING: AtomicUsize = AtomicUsize::new(0);

/// What's doing a download's work, for its progress and to pause or stop
/// it: an aria2 download (its gid), or a job (media, books).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handle {
    Aria(String),
    Job(u64),
}

/// aria2 downloads started here, watched until they end.
struct Watched {
    /// The download's record.
    id: i64,
    gid: String,
    label: String,
    /// Where the finished files still have to be copied, if anywhere.
    deliver: Option<Deliver>,
}
static WATCHED: Mutex<Vec<Watched>> = Mutex::new(Vec::new());

/// A staged download's files, and the picked folder they belong in.
#[derive(Clone, Debug)]
pub struct Deliver {
    pub staging: std::path::PathBuf,
    pub tree_uri: String,
}
/// Downloads that ended since the last `take_finished`.
static FINISHED: Mutex<Vec<Finished>> = Mutex::new(Vec::new());

/// How a download ended, for the notification.
#[derive(Clone, Debug)]
pub struct Finished {
    /// The download's record.
    pub id: i64,
    pub name: String,
    pub ok: bool,
    /// Files to copy into a picked folder now that it's done (only when it
    /// succeeded and was staged).
    pub deliver: Option<Deliver>,
    /// Why it failed (empty when it succeeded).
    pub message: String,
}

/// A download ended. `path` is what it saved (its first file), when it's
/// staying where it was saved. Downloads the user stopped don't get a
/// notification.
fn finished(id: i64, name: String, ok: bool, message: String, deliver: Option<Deliver>, path: Option<PathBuf>) {
    finished_by(id, None, name, ok, message, deliver, path);
    // Its turn's over: the next queued one starts.
    crate::downloads::pump();
}

/// `finished`, for the aria2 download `handle` (ignored when the download
/// has been restarted onto another one since).
fn finished_by(id: i64, handle: Option<&Handle>, name: String, ok: bool, message: String, deliver: Option<Deliver>, path: Option<PathBuf>) {
    if ok {
        log::info!("download finished: {name}");
    } else {
        log::error!("download failed: {name}: {message}");
    }
    let deliver = deliver.filter(|_| ok);
    // A folder picked while it downloaded: it moves there now.
    let (path, deliver) = if ok { crate::downloads::apply_move(id, path, deliver) } else { (path, deliver) };
    // Staged files move into the picked folder, so the path won't hold.
    let path = if deliver.is_some() { None } else { path };
    if !crate::downloads::ended(id, handle, &name, ok, &message, path) {
        return;
    }
    FINISHED.lock().unwrap_or_else(|e| e.into_inner()).push(Finished { id, name, ok, message, deliver });
}

/// A short name for notifications: the chosen name, else the URL's end.
pub fn label_for(request: &DownloadRequest) -> String {
    if !request.filename.is_empty() {
        return request.filename.clone();
    }
    let url = request.url.split(['?', '#']).next().unwrap_or(&request.url);
    url.trim_end_matches('/').rsplit('/').next().unwrap_or(url).to_string()
}

/// Downloads that ended since the last call (checking aria2's for news).
pub fn take_finished() -> Vec<Finished> {
    if let Some(engine) = ENGINE.get() {
        let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
        let mut followers = Vec::new();
        watched.retain(|w| match engine.get_progress(&w.gid) {
            Some(p) if p.status == "complete" => {
                // A torrent's metadata is done; its files are still to come.
                let children = engine.followed_by(&w.gid);
                if children.is_empty() {
                    let path = (!p.filename.is_empty()).then(|| PathBuf::from(&p.filename));
                    finished_by(w.id, Some(&Handle::Aria(w.gid.clone())), w.label.clone(), true, String::new(), w.deliver.clone(), path);
                } else {
                    // Its progress is the files' now.
                    crate::downloads::running(w.id, Handle::Aria(children[0].clone()));
                    followers.extend(children.into_iter().map(|gid| Watched {
                        id: w.id,
                        gid,
                        label: w.label.clone(),
                        deliver: w.deliver.clone(),
                    }));
                }
                false
            }
            Some(p) if p.status == "error" || p.status == "removed" => {
                let message = engine.error_message(&w.gid).unwrap_or_else(|| p.status.clone());
                if p.status == "error" {
                    log::error!(target: "aria2", "{}: {message}", w.label);
                }
                finished_by(w.id, Some(&Handle::Aria(w.gid.clone())), w.label.clone(), false, message, None, None);
                false
            }
            _ => true,
        });
        watched.extend(followers);
        drop(watched);
        // Out of WATCHED's lock: starting the next ones takes it.
        crate::downloads::pump();
    }
    std::mem::take(&mut *FINISHED.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Puts results back for the next take_finished (they couldn't be handed
/// over this time).
pub fn return_finished(mut results: Vec<Finished>) {
    let mut finished = FINISHED.lock().unwrap_or_else(|e| e.into_inner());
    results.append(&mut finished);
    *finished = results;
}

/// What's downloading right now, for the notification: how many, the first
/// one's name, and overall progress (0..1, or None when sizes are unknown).
pub fn summary() -> (usize, String, Option<f64>) {
    let count = active_count();
    let (mut done, mut total) = (0u64, 0u64);
    let mut first = String::new();
    if let Some(engine) = ENGINE.get() {
        let watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
        for w in watched.iter() {
            if let Some(p) = engine.get_progress(&w.gid) {
                if first.is_empty() {
                    first = w.label.clone();
                }
                done += p.completed_length;
                total += p.total_length;
            }
        }
    }
    for job in media::jobs() {
        if first.is_empty() {
            first = job.label.clone();
        }
        done += job.done;
        total += job.total;
    }
    (count, first, (total > 0).then(|| done as f64 / total as f64))
}

/// Starts a download in the background, giving it a record in the
/// downloads lists if it's new (`request.id` 0), or queues it when as many
/// as Settings allows are already running. On Android this runs inside
/// the download service, which keeps the app alive until downloads finish.
pub fn start(request: &DownloadRequest) -> Result<(), String> {
    let mut request = request.clone();
    if request.id == 0 {
        request.id = crate::downloads::register(&request);
    }
    // Enough are running: it waits its turn (downloads::pump starts it).
    if !crate::downloads::take_slot(&request) {
        log::info!("queued {}", request.url);
        return Ok(());
    }
    match launch(&request) {
        Ok(Some(handle)) => {
            crate::downloads::running(request.id, handle);
            Ok(())
        }
        // Auto: the handle comes once it's picked how.
        Ok(None) => Ok(()),
        Err(e) => {
            crate::downloads::ended(request.id, None, &label_for(&request), false, &e, None);
            Err(e)
        }
    }
}

/// Starts `request`'s work; its handle, or None while Auto decides.
fn launch(request: &DownloadRequest) -> Result<Option<Handle>, String> {
    let target = crate::storage::resolve(&request.folder);
    let deliver = match &target {
        crate::storage::SaveTarget::Staged { tree_uri, staging } => {
            std::fs::create_dir_all(staging).map_err(|e| e.to_string())?;
            Some(Deliver { staging: staging.clone(), tree_uri: tree_uri.clone() })
        }
        crate::storage::SaveTarget::Dir(_) => None,
    };
    // The category's folder, inside whichever folder it's saved to (a
    // picked folder gets the staging copy's folders too).
    let subfolder = crate::novels::clean::safe_file_name(&request.settings.subfolder);
    let dir = if request.settings.subfolder.trim().is_empty() {
        target.write_dir().to_path_buf()
    } else {
        let dir = target.write_dir().join(subfolder);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        dir
    };
    let dir = dir.to_string_lossy().into_owned();
    log::info!("starting {:?} download of {} into {dir} ({target:?})", request.backend, request.url);
    let label = label_for(request);
    let watch = |gid: String| {
        WATCHED.lock().unwrap_or_else(|e| e.into_inner()).push(Watched {
            id: request.id,
            gid: gid.clone(),
            label: label.clone(),
            deliver: deliver.clone(),
        });
        Some(Handle::Aria(gid))
    };
    let name = (!request.filename.is_empty()).then(|| request.filename.replace(['/', '\\'], "_"));
    let select = (!request.select_files.is_empty()).then_some(request.select_files.as_str());
    Ok(match request.backend {
        Backend::Torrent if !request.url.contains(':') || request.url.starts_with('/') => {
            // A saved .torrent file.
            let torrent = std::fs::read(&request.url).map_err(|e| e.to_string())?;
            watch(engine().start_torrent(torrent, &dir, select, &request.settings)?)
        }
        // A magnet, or a .torrent link aria2 fetches and then follows: the
        // files take their names from the torrent.
        Backend::Torrent => watch(engine().start_download(&request.url, &dir, None, select, &request.settings)?),
        // Direct on a .torrent link saves the .torrent file itself.
        Backend::Direct => watch(engine().start_download(&request.url, &dir, name.as_deref(), None, &request.settings)?),
        Backend::Media => Some(Handle::Job(media::start(request, &dir, name, deliver.clone())?)),
        Backend::Book => Some(Handle::Job(book::start(request, &dir, name, deliver.clone())?)),
        Backend::Auto => {
            CHOOSING.fetch_add(1, Ordering::SeqCst);
            let mut request = request.clone();
            runtime().spawn(async move {
                (request.backend, request.format) = crate::links::pick_backend(&request.url).await;
                // The record keeps what was picked, for resuming.
                crate::downloads::update_request(&request);
                match launch(&request) {
                    Ok(Some(handle)) => crate::downloads::running(request.id, handle),
                    Ok(None) => {}
                    Err(e) => finished(request.id, label_for(&request), false, e, None, None),
                }
                CHOOSING.fetch_sub(1, Ordering::SeqCst);
            });
            None
        }
    })
}

/// Pauses a download's work. aria2 keeps its place; a job stops (resuming
/// starts it again: yt-dlp carries on with its .part file, books with the
/// chapters they have).
pub fn pause(handle: &Handle) {
    match handle {
        Handle::Aria(gid) => {
            let _ = engine().pause_download(gid);
        }
        Handle::Job(job) => media::cancel(*job),
    }
}

/// Resumes a paused aria2 download (jobs are started again instead).
pub fn unpause(handle: &Handle) -> bool {
    match handle {
        Handle::Aria(gid) => engine().resume_download(gid).is_ok(),
        Handle::Job(_) => false,
    }
}

/// Stops a download for good (its partial files stay until it's deleted).
pub fn cancel(handle: &Handle) {
    match handle {
        Handle::Aria(gid) => engine().remove_download(gid),
        Handle::Job(job) => media::cancel(*job),
    }
}

/// A download's progress right now: (state, done, total, bytes per second).
pub fn progress(handle: &Handle) -> Option<(crate::downloads::Live, u64, u64, f64)> {
    use crate::downloads::Live;
    match handle {
        Handle::Aria(gid) => {
            let p = ENGINE.get()?.get_progress(gid)?;
            let live = match p.status.as_str() {
                "waiting" => Live::Queued,
                "paused" => Live::Paused,
                "complete" => Live::Done,
                "error" | "removed" => Live::Stopped,
                _ => Live::Active,
            };
            Some((live, p.completed_length, p.total_length, p.download_speed))
        }
        Handle::Job(job) => media::job(*job).map(|j| (Live::Active, j.done, j.total, 0.0)),
    }
}

/// The file an aria2 download is writing (its first), once aria2 knows it.
pub fn aria_file(handle: &Handle) -> Option<PathBuf> {
    match handle {
        Handle::Aria(gid) => {
            let p = ENGINE.get()?.get_progress(gid)?;
            (!p.filename.is_empty()).then(|| PathBuf::from(p.filename))
        }
        Handle::Job(_) => None,
    }
}

/// Downloads still running or queued, across every backend and Eddy's own
/// queue. Doesn't start the engine just to ask.
pub fn active_count() -> usize {
    let aria = ENGINE.get().map(|e| e.active_count()).unwrap_or(0);
    // Queued ones too: the download service keeps going for them.
    aria + media::jobs().len() + CHOOSING.load(Ordering::SeqCst) + crate::downloads::queued_count()
}

/// Downloads `url` to `dir/out` with aria2 (several connections, retries),
/// for Eddy's own files: the yt-dlp packages, desktop's tools. Not a user
/// download, so it's not in the lists. `on_progress(done, total)` gets
/// called as it goes.
pub async fn fetch_file(url: &str, dir: &std::path::Path, out: &str, on_progress: impl Fn(u64, u64)) -> Result<PathBuf, String> {
    tokio::fs::create_dir_all(dir).await.map_err(|e| e.to_string())?;
    // Written under another name until it's whole (aria2 may allocate the
    // full size up front).
    let partial = format!("{out}.part");
    let _ = tokio::fs::remove_file(dir.join(&partial)).await;
    let _ = tokio::fs::remove_file(dir.join(format!("{partial}.aria2"))).await;
    let engine = engine();
    let gid = engine.start_download(url, &dir.to_string_lossy(), Some(&partial), None, &DownloadSettings::default())?;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let Some(p) = engine.get_progress(&gid) else {
            continue; // not picked up yet
        };
        on_progress(p.completed_length, p.total_length);
        match p.status.as_str() {
            "complete" => break,
            "error" | "removed" => {
                let message = engine.error_message(&gid).unwrap_or(p.status);
                log::error!(target: "aria2", "couldn't download {url}: {message}");
                return Err(message);
            }
            _ => {}
        }
    }
    let path = dir.join(out);
    tokio::fs::rename(dir.join(&partial), &path).await.map_err(|e| e.to_string())?;
    Ok(path)
}