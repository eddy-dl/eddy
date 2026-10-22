pub mod aria2;
pub mod book;
pub mod media;

use aria2::AriaEngineClient;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group_man::RequestGroupMan;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone)]
pub struct DownloadClient {}

/// How a download is fetched (same order as the UI's LinkBackend).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
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
#[derive(Clone, Debug)]
pub struct DownloadRequest {
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

/// How a download goes about it. Each backend uses what it can: aria2 all
/// of it; yt-dlp the speed, fragments at once and user agent; the native
/// extractors the speed (their sites need their own user agents); books
/// none of it but the folder.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DownloadSettings {
    /// Bytes per second; 0 for no limit.
    pub max_speed: u64,
    /// Connections at once (1–16); 0 for the usual 16.
    pub connections: u16,
    /// "" for a browser's (Eddy's usual one).
    pub user_agent: String,
    /// A folder inside the save folder for the file's category ("Video"…);
    /// "" for none.
    pub subfolder: String,
}

impl DownloadSettings {
    pub fn connections(&self) -> u16 {
        if self.connections == 0 { 16 } else { self.connections.clamp(1, 16) }
    }

    pub fn user_agent(&self) -> &str {
        if self.user_agent.trim().is_empty() { crate::extractors::http::UA_DESKTOP } else { self.user_agent.trim() }
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

/// aria2 downloads started here, watched until they end.
struct Watched {
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
    pub name: String,
    pub ok: bool,
    /// Files to copy into a picked folder now that it's done (only when it
    /// succeeded and was staged).
    pub deliver: Option<Deliver>,
    /// Why it failed (empty when it succeeded).
    pub message: String,
}

fn finished(name: String, ok: bool, message: String, deliver: Option<Deliver>) {
    if ok {
        log::info!("download finished: {name}");
    } else {
        log::error!("download failed: {name}: {message}");
    }
    let deliver = deliver.filter(|_| ok);
    FINISHED.lock().unwrap_or_else(|e| e.into_inner()).push(Finished { name, ok, message, deliver });
}

/// A short name for notifications: the chosen name, else the URL's end.
fn label_for(request: &DownloadRequest) -> String {
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
                    finished(w.label.clone(), true, String::new(), w.deliver.clone());
                } else {
                    followers.extend(children.into_iter().map(|gid| Watched {
                        gid,
                        label: w.label.clone(),
                        deliver: w.deliver.clone(),
                    }));
                }
                false
            }
            Some(p) if p.status == "error" || p.status == "removed" => {
                let message = engine.error_message(&w.gid).unwrap_or_else(|| p.status.clone());
                finished(w.label.clone(), false, message, None);
                false
            }
            _ => true,
        });
        watched.extend(followers);
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

/// Starts a download in the background. On Android this runs inside the
/// download service, which keeps the app alive until downloads finish.
pub fn start(request: &DownloadRequest) -> Result<(), String> {
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
            gid,
            label: label.clone(),
            deliver: deliver.clone(),
        })
    };
    let name = (!request.filename.is_empty()).then(|| request.filename.replace(['/', '\\'], "_"));
    let select = (!request.select_files.is_empty()).then_some(request.select_files.as_str());
    match request.backend {
        Backend::Torrent if !request.url.contains(':') || request.url.starts_with('/') => {
            // A saved .torrent file.
            let torrent = std::fs::read(&request.url).map_err(|e| e.to_string())?;
            watch(engine().start_torrent(torrent, &dir, select, &request.settings)?);
        }
        // A magnet, or a .torrent link aria2 fetches and then follows: the
        // files take their names from the torrent.
        Backend::Torrent => {
            watch(engine().start_download(&request.url, &dir, None, select, &request.settings)?);
        }
        // Direct on a .torrent link saves the .torrent file itself.
        Backend::Direct => {
            watch(engine().start_download(&request.url, &dir, name.as_deref(), None, &request.settings)?);
        }
        Backend::Media => media::start(request, &dir, name, deliver.clone())?,
        Backend::Book => book::start(request, &dir, name, deliver.clone())?,
        Backend::Auto => {
            CHOOSING.fetch_add(1, Ordering::SeqCst);
            let mut request = request.clone();
            runtime().spawn(async move {
                (request.backend, request.format) = crate::links::pick_backend(&request.url).await;
                if let Err(e) = start(&request) {
                    finished(label_for(&request), false, e, None);
                }
                CHOOSING.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }
    Ok(())
}

/// Downloads still running or queued, across every backend. Doesn't start
/// the engine just to ask.
pub fn active_count() -> usize {
    let aria = ENGINE.get().map(|e| e.active_count()).unwrap_or(0);
    aria + media::jobs().len() + CHOOSING.load(Ordering::SeqCst)
}
