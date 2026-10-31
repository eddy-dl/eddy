//! Downloading a page's media: yt-dlp for its sites, and the native
//! extractors' files (fetched here with the headers the site wants, through
//! ffmpeg for HLS streams and for converting to audio). Each download is a
//! job with live progress for the notification (books use the same jobs).

use super::{Deliver, DownloadRequest, finished, runtime};
use crate::extractors::{self, Fetch, MediaRequest, Service};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A running media download: its name, and bytes done out of total (0 when
/// unknown).
#[derive(Clone, Debug)]
pub struct Job {
    pub label: String,
    /// The file being downloaded: bytes done and its size (0 unknown).
    pub done: u64,
    pub total: u64,
    /// With several files (a playlist, a post), which one this is.
    pub part: Option<Part>,
    /// What it has written so far (files, and yt-dlp's targets, whose
    /// partial files sit beside them), for cleaning up if it's cancelled.
    pub written: Vec<PathBuf>,
}

/// One of a job's several files: its place (from 1) and name, and the
/// bytes of the files before it.
#[derive(Clone, Debug, Default)]
pub struct Part {
    pub index: usize,
    pub count: usize,
    pub name: String,
    pub finished: u64,
}

impl Job {
    /// How far the whole job is (0..1), files and all.
    pub fn fraction(&self) -> Option<f32> {
        let part = self.part.as_ref()?;
        let file = if self.total > 0 { (self.done as f64 / self.total as f64).min(1.0) } else { 0.0 };
        Some((((part.index.saturating_sub(1)) as f64 + file) / part.count.max(1) as f64) as f32)
    }

    /// Bytes done across its files.
    pub fn bytes_done(&self) -> u64 {
        self.part.as_ref().map_or(0, |p| p.finished) + self.done
    }
}

/// How to stop a job: its flag (so a stopped job doesn't report a failure),
/// its task, and yt-dlp's process.
#[derive(Default)]
struct Control {
    cancelled: Arc<AtomicBool>,
    abort: Option<tokio::task::AbortHandle>,
    child: Option<Arc<Mutex<Option<std::process::Child>>>>,
    /// The aria2 download fetching a native extractor's file right now.
    aria: Option<String>,
}

static JOBS: Mutex<Option<HashMap<u64, (Job, Control)>>> = Mutex::new(None);
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

fn with_jobs<T>(f: impl FnOnce(&mut HashMap<u64, (Job, Control)>) -> T) -> T {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    f(jobs.get_or_insert_with(HashMap::new))
}

/// A new job; returns its id and its cancelled flag.
pub(super) fn begin(label: &str) -> (u64, Arc<AtomicBool>) {
    let id = NEXT_JOB.fetch_add(1, Ordering::SeqCst);
    let control = Control::default();
    let cancelled = Arc::clone(&control.cancelled);
    with_jobs(|jobs| jobs.insert(id, (Job { label: label.to_string(), done: 0, total: 0, part: None, written: Vec::new() }, control)));
    (id, cancelled)
}

/// The job's task, so cancelling can stop it.
pub(super) fn set_task(id: u64, abort: tokio::task::AbortHandle) {
    with_jobs(|jobs| {
        if let Some((_, control)) = jobs.get_mut(&id) {
            control.abort = Some(abort);
        }
    });
}

pub(super) fn progress(id: u64, done: u64, total: u64) {
    with_jobs(|jobs| {
        if let Some((job, _)) = jobs.get_mut(&id) {
            job.done = done;
            job.total = total;
        }
    });
}

/// The next of a job's `count` files starts (named `name`).
pub(super) fn next_part(id: u64, count: usize, name: &str) {
    with_jobs(|jobs| {
        if let Some((job, _)) = jobs.get_mut(&id) {
            let mut part = job.part.take().unwrap_or_default();
            if part.index > 0 {
                part.finished += job.done;
            }
            part.index += 1;
            part.count = count.max(part.index);
            part.name = name.to_string();
            job.part = Some(part);
            job.done = 0;
            job.total = 0;
        }
    });
}

/// The job is writing `path`.
pub(super) fn wrote(id: u64, path: &Path) {
    with_jobs(|jobs| {
        if let Some((job, _)) = jobs.get_mut(&id) {
            if !job.written.iter().any(|p| p == path) {
                job.written.push(path.to_path_buf());
            }
        }
    });
}

pub(super) fn end(id: u64) {
    with_jobs(|jobs| jobs.remove(&id));
}

/// Stops a job (pausing or cancelling): its task, or yt-dlp.
pub fn cancel(id: u64) {
    let control = with_jobs(|jobs| jobs.remove(&id)).map(|(_, c)| c);
    if let Some(control) = control {
        control.cancelled.store(true, Ordering::SeqCst);
        if let Some(abort) = control.abort {
            abort.abort();
        }
        if let Some(child) = control.child {
            if let Some(child) = child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                // With the ffmpeg it may be running.
                crate::children::kill_tree(child);
            }
        }
        // Its file and .aria2 stay, for carrying on.
        if let Some(gid) = control.aria {
            super::engine().remove_download(&gid);
        }
    }
}

/// How many jobs have an aria2 download of their own going (counted once,
/// as the job).
pub fn aria_downloads() -> usize {
    with_jobs(|jobs| jobs.values().filter(|(_, control)| control.aria.is_some()).count())
}

fn set_aria(id: u64, gid: Option<String>) {
    with_jobs(|jobs| {
        if let Some((_, control)) = jobs.get_mut(&id) {
            control.aria = gid;
        }
    });
}

/// Media and book downloads still going.
pub fn jobs() -> Vec<Job> {
    with_jobs(|jobs| jobs.values().map(|(j, _)| j.clone()).collect())
}

pub fn job(id: u64) -> Option<Job> {
    with_jobs(|jobs| jobs.get(&id).map(|(j, _)| j.clone()))
}

/// Starts a media download (`request.format` holds the MediaRequest);
/// returns its job.
pub fn start(request: &DownloadRequest, dir: &str, name: Option<String>, deliver: Option<Deliver>) -> Result<u64, String> {
    let media: MediaRequest = if request.format.is_empty() {
        MediaRequest::default()
    } else {
        serde_json::from_str(&request.format).map_err(|e| format!("Bad media request: {e}"))?
    };
    let service = media.service.or_else(|| extractors::native_service(&request.url)).unwrap_or(Service::YtDlp);
    let label = super::label_for(request);
    let record = request.id;
    match service {
        Service::YtDlp => start_ytdlp(record, &request.url, &media, dir, name, label, deliver, &request.settings),
        native => {
            let (job, cancelled) = begin(&label);
            let (url, dir) = (request.url.clone(), PathBuf::from(dir));
            let (settings, resume) = (request.settings.clone(), request.resume);
            let task = runtime().spawn(async move {
                let result = download_native(record, job, native, &url, &media, &dir, name.as_deref(), &settings, resume).await;
                end(job);
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                match result {
                    Ok(files) => finished(record, saved_name(&files, label), true, String::new(), deliver, files.first().cloned()),
                    Err(e) => finished(record, label, false, e, None, None),
                }
            });
            set_task(job, task.abort_handle());
            Ok(job)
        }
    }
}

/// Runs yt-dlp for `media`: once, or for a playlist partly saved as audio,
/// once for the video items and then once for the audio ones (see
/// ytdlp::download_args), as one job.
#[allow(clippy::too_many_arguments)]
fn start_ytdlp(
    record: i64,
    url: &str,
    media: &MediaRequest,
    dir: &str,
    name: Option<String>,
    label: String,
    deliver: Option<Deliver>,
    settings: &super::DownloadSettings,
) -> Result<u64, String> {
    use std::io::BufRead;
    let runs = extractors::ytdlp::download_args(url, media, dir, name.as_deref(), settings);
    // A playlist's files, for "3 of 12" (yt-dlp says when each starts).
    let count = if media.playlist { if media.items.is_empty() { media.entries } else { media.items.len() } } else { 0 };
    if crate::platform::ytdlp_command().is_none() {
        return Err("yt-dlp isn't available".to_string());
    }
    let (job, cancelled) = begin(&label);
    // The running yt-dlp, shared with cancel(), which kills it.
    let child: Arc<Mutex<Option<std::process::Child>>> = Arc::new(Mutex::new(None));
    with_jobs(|jobs| {
        if let Some((_, control)) = jobs.get_mut(&job) {
            control.child = Some(Arc::clone(&child));
        }
    });
    std::thread::spawn(move || {
        let mut files = Vec::new();
        let mut failure = None;
        for args in runs {
            let spawned = crate::platform::ytdlp_command().ok_or_else(|| std::io::Error::other("yt-dlp isn't available")).and_then(|mut c| {
                crate::children::own_group(&mut c)
                    .args(&args)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
            });
            let mut running = match spawned {
                Ok(running) => running,
                Err(e) => {
                    failure = Some(format!("Couldn't run yt-dlp: {e}"));
                    break;
                }
            };
            let stdout = running.stdout.take();
            *child.lock().unwrap_or_else(|e| e.into_inner()) = Some(running);
            if cancelled.load(Ordering::SeqCst) {
                // Cancelled between two runs: this one goes too.
                if let Some(mut c) = child.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    crate::children::kill_tree(&mut c);
                }
                break;
            }
            if let Some(stdout) = stdout {
                for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(path) = line.strip_prefix(extractors::ytdlp::SAVED_PREFIX) {
                        files.push(PathBuf::from(path.trim()));
                        wrote(job, Path::new(path.trim()));
                    } else if let Some(path) = line.strip_prefix(extractors::ytdlp::TARGET_PREFIX) {
                        wrote(job, Path::new(path.trim()));
                    } else if let Some(title) = line.strip_prefix(extractors::ytdlp::ITEM_PREFIX) {
                        if count > 1 {
                            next_part(job, count, title.trim());
                        }
                    } else if let Some((done, total)) = extractors::ytdlp::parse_progress(&line) {
                        progress(job, done, total);
                    }
                }
            }
            let taken = child.lock().unwrap_or_else(|e| e.into_inner()).take();
            let Some(taken) = taken else {
                break;
            };
            match taken.wait_with_output() {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    log_stderr(&label, &stderr);
                    let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed");
                    failure = Some(last.trim_start_matches("ERROR: ").to_string());
                    break;
                }
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }
        end(job);
        if cancelled.load(Ordering::SeqCst) {
            return; // paused or cancelled
        }
        match failure {
            None => finished(record, saved_name(&files, label), true, String::new(), deliver, files.first().cloned()),
            Some(message) => finished(record, label, false, message, None, None),
        }
    });
    Ok(job)
}

/// The end of what a failed yt-dlp run printed, into logs/ytdlp.log.
pub fn log_stderr(what: &str, stderr: &str) {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(30)..];
    log::error!(target: "ytdlp", "{what} failed:\n    {}", tail.join("\n    "));
}

/// What the notification calls a finished download: the file's name, with
/// its extension, when it made one file; otherwise the download's label.
fn saved_name(files: &[PathBuf], label: String) -> String {
    match files {
        [file] => file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(label),
        _ => label,
    }
}

/// A file name that's safe on every filesystem.
pub(super) fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect();
    // Captions can be essays; keep names reasonable.
    cleaned.trim().chars().take(120).collect::<String>().trim().to_string()
}

#[allow(clippy::too_many_arguments)]
async fn download_native(
    record: i64,
    job: u64,
    service: Service,
    url: &str,
    media: &MediaRequest,
    dir: &Path,
    name: Option<&str>,
    settings: &super::DownloadSettings,
    // Carrying on: files it finished are kept, partial ones continued.
    resume: bool,
) -> Result<Vec<PathBuf>, String> {
    let fetches = extractors::resolve(service, url, media).await?;
    if fetches.is_empty() {
        return Err("Nothing to download".to_string());
    }
    // Several files go in a folder of their own (`name`, else the post's
    // caption), each named by its place in the post.
    let several = fetches.len() > 1;
    let caption = fetches.first().and_then(|f| f.caption.as_deref()).map(safe_name).filter(|c| !c.is_empty());
    let dir = match (several, name) {
        (false, _) => dir.to_path_buf(),
        (true, Some(name)) => dir.join(safe_name(name)),
        (true, None) => dir.join(caption.clone().unwrap_or_else(|| format!("{} post", service.label()))),
    };
    // A folder of that name already there (and not this download's own,
    // being resumed): a free name for this one, unless replacing, and it's
    // remembered so resuming finds it.
    let in_use = |d: &Path| std::fs::read_dir(d).is_ok_and(|mut e| e.next().is_some());
    let dir = if several && !resume && !settings.replace_existing && in_use(&dir) {
        let free = super::free_path(&dir);
        if let Some(folder) = free.file_name() {
            crate::downloads::set_folder_name(record, &folder.to_string_lossy());
        }
        free
    } else {
        dir
    };
    if several {
        tokio::fs::create_dir_all(&dir).await.map_err(|e| format!("Couldn't make the folder: {e}"))?;
    }
    let mut files = Vec::new();
    for fetch in &fetches {
        let base = match (name, several) {
            (Some(name), false) => safe_name(name),
            (_, true) => format!("{:02} {}", fetch.index + 1, caption.as_deref().unwrap_or(service.label())),
            (None, false) => caption.clone().unwrap_or_else(|| format!("{} {}", service.label(), fetch.index + 1)),
        };
        let path = dir.join(format!("{base}.{}", fetch.ext));
        if several {
            next_part(job, fetches.len(), &format!("{base}.{}", fetch.ext));
        }
        wrote(job, &path);
        let audio = extractors::as_audio(media, fetch.index) && fetch.kind != extractors::MediaKind::Image;
        // Finished before it stopped (as audio, the converted file).
        let finished = if audio { audio_path(&path, &media.audio_codec) } else { path.clone() };
        if resume && !part_path(&path).exists() && std::fs::metadata(&finished).is_ok_and(|m| m.len() > 0) {
            let size = std::fs::metadata(&finished).map(|m| m.len()).unwrap_or(0);
            progress(job, size, size);
            wrote(job, &finished);
            files.push(finished);
            continue;
        }
        // Where it ends up: `path`, or with a clash a free name (or `path`
        // replaced).
        let saved = if fetch.hls {
            let out = if resume || settings.replace_existing { path.clone() } else { super::free_path(&path) };
            save_hls(job, fetch, &out).await?;
            out
        } else if let Some(audio) = &fetch.audio {
            let out = if resume || settings.replace_existing { path.clone() } else { super::free_path(&path) };
            save_joined(job, fetch, audio, &out, settings, resume).await?;
            out
        } else {
            let mut fetch = fetch.clone();
            let mut refreshed = 0;
            loop {
                match save(job, &fetch.url, &fetch.headers, &path, settings, resume || refreshed > 0).await {
                    Ok(saved) => break saved,
                    // Its link ran out (they're signed, and don't last):
                    // a fresh one, carrying on from what's saved.
                    Err(e) if refreshed < 2 && link_expired(&e) => {
                        refreshed += 1;
                        log::warn!("{}: link expired ({e}); getting a new one", path.display());
                        let fresh = extractors::resolve(service, url, media).await?;
                        fetch = fresh.into_iter().find(|f| f.index == fetch.index).ok_or(e)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        wrote(job, &saved);
        if audio {
            let audio = to_audio(job, &saved, &media.audio_codec, media.audio_kbps, settings.replace_existing).await?;
            wrote(job, &audio);
            files.push(audio);
        } else {
            files.push(saved);
        }
    }
    Ok(files)
}

/// Where a file is written until it's whole.
pub(super) fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// Downloads a file to `path` with aria2 (its connections, retries and
/// speed limit from `settings`), through its .part, reporting progress;
/// returns where it ended up (see `settle`).
/// A pause removes the aria2 download but keeps the .part and its .aria2,
/// which `resume` carries on from, even from a fresh link. Otherwise any
/// left from before are dropped first.
pub(super) async fn save(
    job: u64,
    url: &str,
    headers: &[(String, String)],
    path: &Path,
    settings: &super::DownloadSettings,
    resume: bool,
) -> Result<PathBuf, String> {
    let part = part_path(path);
    let control = part.with_file_name(format!("{}.aria2", part.file_name().unwrap_or_default().to_string_lossy()));
    if !resume {
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&control).await;
    }
    let dir = path.parent().unwrap_or(Path::new(".")).to_string_lossy().into_owned();
    let out = part.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let engine = super::engine();
    let gid = engine.start_download_with_headers(url, &dir, Some(&out), None, settings, headers, None)?;
    set_aria(job, Some(gid.clone()));
    let result = loop {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let Some(p) = engine.get_progress(&gid) else {
            continue; // not picked up yet
        };
        progress(job, p.completed_length, p.total_length);
        match p.status.as_str() {
            "complete" => break Ok(()),
            "error" | "removed" => break Err(engine.error_message(&gid).unwrap_or(p.status)),
            _ => {}
        }
    };
    set_aria(job, None);
    engine.drop_lock_file(&gid);
    result?;
    if tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = tokio::fs::remove_file(&part).await;
        return Err("The site sent an empty file".to_string());
    }
    settle(&part, path, settings.replace_existing).await
}

/// Moves a finished `part` to `path`. Something already there (another
/// download's file, an old copy) is replaced with `replace`; otherwise this
/// one takes the first free "name (n)" beside it. Returns where it went.
pub(super) async fn settle(part: &Path, path: &Path, replace: bool) -> Result<PathBuf, String> {
    let target = if replace {
        if tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file()) {
            let _ = tokio::fs::remove_file(path).await;
        }
        path.to_path_buf()
    } else {
        super::free_path(path)
    };
    tokio::fs::rename(part, &target).await.map_err(|e| format!("Couldn't save it as {}: {e}", target.display()))?;
    Ok(target)
}

/// An error that means the file's link has run out: the site refusing it
/// (403), or no longer knowing it (404, 410).
pub(super) fn link_expired(error: &str) -> bool {
    ["error: 403", "error: 404", "error: 410", "403 Forbidden", "404 Not Found", "410 Gone"].iter().any(|code| error.contains(code))
}

/// Runs ffmpeg for `job`, which can stop it: it's the job's child while it
/// runs (`cancel` kills it, like yt-dlp), so a pause doesn't leave it going.
async fn ffmpeg(job: u64, args: Vec<String>) -> Result<(), String> {
    let slot: Arc<Mutex<Option<std::process::Child>>> = Arc::new(Mutex::new(None));
    with_jobs(|jobs| {
        if let Some((_, control)) = jobs.get_mut(&job) {
            control.child = Some(Arc::clone(&slot));
        }
    });
    let (status, stderr) = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut command = crate::platform::ffmpeg_command().ok_or_else(|| std::io::Error::other("ffmpeg isn't available"))?;
        let mut child = crate::children::own_group(&mut command)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let mut err = child.stderr.take();
        let errors = std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(err) = err.as_mut() {
                let _ = err.read_to_string(&mut text);
            }
            text
        });
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        // Polled, not waited on under the lock: `cancel` takes it to kill.
        let status = loop {
            let done = slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(|c| c.try_wait());
            match done {
                Some(Ok(Some(status))) => break status,
                Some(Ok(None)) => std::thread::sleep(std::time::Duration::from_millis(100)),
                Some(Err(e)) => return Err(e),
                None => return Err(std::io::Error::other("stopped")),
            }
        };
        slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        Ok::<_, std::io::Error>((status, errors.join().unwrap_or_default()))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("Couldn't run ffmpeg: {e}"))?;
    with_jobs(|jobs| {
        if let Some((_, control)) = jobs.get_mut(&job) {
            control.child = None;
        }
    });
    if status.success() {
        Ok(())
    } else {
        Err(stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("ffmpeg failed").to_string())
    }
}

/// Saves an HLS stream as one mp4 (stream copy, no re-encoding).
async fn save_hls(job: u64, fetch: &Fetch, path: &Path) -> Result<(), String> {
    let headers: String = fetch.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let mut args = vec!["-y".to_string()];
    if !headers.is_empty() {
        args.extend(["-headers".to_string(), headers]);
    }
    args.extend(
        ["-i", &fetch.url, "-c", "copy", "-bsf:a", "aac_adtstoasc", "-movflags", "+faststart"]
            .iter()
            .map(|s| s.to_string()),
    );
    args.push(path.to_string_lossy().into_owned());
    ffmpeg(job, args).await
}

/// Saves a video-only stream and its sound, then joins them into `path`
/// (stream copy, no re-encoding). Each half resumes on its own.
async fn save_joined(job: u64, fetch: &Fetch, audio: &str, path: &Path, settings: &super::DownloadSettings, resume: bool) -> Result<(), String> {
    // A half finished before a pause isn't fetched again.
    let half = |url: String, file: PathBuf| async move {
        if resume && !part_path(&file).exists() && file.is_file() {
            Ok(file)
        } else {
            save(job, &url, &fetch.headers, &file, settings, resume).await
        }
    };
    let video = half(fetch.url.clone(), path.with_extension("video.mp4")).await?;
    let sound = half(audio.to_string(), path.with_extension("audio.m4a")).await?;
    let mut args = vec!["-y".to_string()];
    for input in [&video, &sound] {
        args.extend(["-i".to_string(), input.to_string_lossy().into_owned()]);
    }
    args.extend(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy", "-movflags", "+faststart"].iter().map(|s| s.to_string()));
    args.push(path.to_string_lossy().into_owned());
    let joined = ffmpeg(job, args).await;
    if joined.is_err() {
        let _ = tokio::fs::remove_file(path).await;
    }
    let _ = tokio::fs::remove_file(&video).await;
    let _ = tokio::fs::remove_file(&sound).await;
    joined
}

/// Replaces a downloaded video with its audio, as mp3 or m4a.
/// Returns the audio file's path.
/// The audio file `to_audio` makes of `path`.
fn audio_path(path: &Path, codec: &str) -> PathBuf {
    path.with_extension(if codec == "mp3" { "mp3" } else { "m4a" })
}

/// Converts `path` to audio and removes it. An audio file of that name
/// already there is replaced with `replace`, else this one gets a free name.
async fn to_audio(job: u64, path: &Path, codec: &str, kbps: u32, replace: bool) -> Result<PathBuf, String> {
    let encoder = if codec == "mp3" { "libmp3lame" } else { "aac" };
    let target = audio_path(path, codec);
    let target = if target != path && !replace { super::free_path(&target) } else { target };
    let ext = target.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    // ffmpeg can't write over its own input (an .m4a kept as m4a).
    let out = if target == path { path.with_extension(format!("audio.{ext}")) } else { target.clone() };
    let mut args = vec!["-y".to_string(), "-i".to_string(), path.to_string_lossy().into_owned()];
    args.extend(["-vn", "-c:a", encoder].iter().map(|s| s.to_string()));
    if kbps > 0 {
        args.extend(["-b:a".to_string(), format!("{kbps}k")]);
    }
    args.push(out.to_string_lossy().into_owned());
    ffmpeg(job, args).await?;
    let _ = tokio::fs::remove_file(path).await;
    if out != target {
        tokio::fs::rename(&out, &target).await.map_err(|e| e.to_string())?;
    }
    Ok(target)
}
