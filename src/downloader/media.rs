//! Downloading a page's media: yt-dlp for its sites, and the native
//! extractors' files (fetched here with the headers the site wants, through
//! ffmpeg for HLS streams and for converting to audio). Each download is a
//! job with live progress for the notification.

use super::{Deliver, DownloadRequest, finished, runtime};
use crate::extractors::{self, Fetch, MediaRequest, Service};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A running media download: its name, and bytes done out of total (0 when
/// unknown).
#[derive(Clone, Debug)]
pub struct Job {
    pub label: String,
    pub done: u64,
    pub total: u64,
}

static JOBS: Mutex<Option<HashMap<u64, Job>>> = Mutex::new(None);
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

fn with_jobs<T>(f: impl FnOnce(&mut HashMap<u64, Job>) -> T) -> T {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    f(jobs.get_or_insert_with(HashMap::new))
}

fn begin(label: &str) -> u64 {
    let id = NEXT_JOB.fetch_add(1, Ordering::SeqCst);
    with_jobs(|jobs| jobs.insert(id, Job { label: label.to_string(), done: 0, total: 0 }));
    id
}

fn progress(id: u64, done: u64, total: u64) {
    with_jobs(|jobs| {
        if let Some(job) = jobs.get_mut(&id) {
            job.done = done;
            job.total = total;
        }
    });
}

fn end(id: u64) {
    with_jobs(|jobs| jobs.remove(&id));
}

/// Media downloads still going.
pub fn jobs() -> Vec<Job> {
    with_jobs(|jobs| jobs.values().cloned().collect())
}

/// Starts a media download (`request.format` holds the MediaRequest).
pub fn start(request: &DownloadRequest, dir: &str, name: Option<String>, deliver: Option<Deliver>) -> Result<(), String> {
    let media: MediaRequest = if request.format.is_empty() {
        MediaRequest::default()
    } else {
        serde_json::from_str(&request.format).map_err(|e| format!("Bad media request: {e}"))?
    };
    let service = media.service.or_else(|| extractors::native_service(&request.url)).unwrap_or(Service::YtDlp);
    let label = super::label_for(request);
    match service {
        Service::YtDlp => start_ytdlp(&request.url, &media, dir, name, label, deliver),
        native => {
            let job = begin(&label);
            let (url, dir) = (request.url.clone(), PathBuf::from(dir));
            runtime().spawn(async move {
                let result = download_native(job, native, &url, &media, &dir, name.as_deref()).await;
                end(job);
                match result {
                    Ok(()) => finished(label, true, String::new(), deliver),
                    Err(e) => finished(label, false, e, None),
                }
            });
            Ok(())
        }
    }
}

fn start_ytdlp(
    url: &str,
    media: &MediaRequest,
    dir: &str,
    name: Option<String>,
    label: String,
    deliver: Option<Deliver>,
) -> Result<(), String> {
    use std::io::BufRead;
    let args = extractors::ytdlp::download_args(url, media, dir, name.as_deref());
    let mut child = crate::platform::ytdlp_command()
        .ok_or("yt-dlp isn't available")?
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't run yt-dlp: {e}"))?;

    let job = begin(&label);
    let stdout = child.stdout.take();
    std::thread::spawn(move || {
        if let Some(stdout) = stdout {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some((done, total)) = extractors::ytdlp::parse_progress(&line) {
                    progress(job, done, total);
                }
            }
        }
        let result = child.wait_with_output();
        end(job);
        match result {
            Ok(output) if output.status.success() => finished(label, true, String::new(), deliver),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed");
                finished(label, false, last.trim_start_matches("ERROR: ").to_string(), None);
            }
            Err(e) => finished(label, false, e.to_string(), None),
        }
    });
    Ok(())
}

/// A file name that's safe on every filesystem.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect();
    // Captions can be essays; keep names reasonable.
    cleaned.trim().chars().take(120).collect::<String>().trim().to_string()
}

async fn download_native(
    job: u64,
    service: Service,
    url: &str,
    media: &MediaRequest,
    dir: &Path,
    name: Option<&str>,
) -> Result<(), String> {
    let fetches = extractors::resolve(service, url, media).await?;
    if fetches.is_empty() {
        return Err("Nothing to download".to_string());
    }
    let several = fetches.len() > 1;
    for fetch in &fetches {
        let base = match (name, several) {
            (Some(name), false) => safe_name(name),
            (Some(name), true) => format!("{} {}", safe_name(name), fetch.index + 1),
            (None, _) => {
                let caption = fetch.caption.as_deref().map(safe_name).filter(|c| !c.is_empty());
                match caption {
                    Some(c) if several => format!("{c} {}", fetch.index + 1),
                    Some(c) => c,
                    None => format!("{} {}", service.label(), fetch.index + 1),
                }
            }
        };
        let path = dir.join(format!("{base}.{}", fetch.ext));
        if fetch.hls {
            save_hls(fetch, &path).await?;
        } else {
            save(job, fetch, &path).await?;
        }
        if !media.audio_codec.is_empty() && fetch.kind != extractors::MediaKind::Image {
            to_audio(&path, &media.audio_codec, media.audio_kbps).await?;
        }
    }
    Ok(())
}

/// Streams a file to `path`, reporting progress.
async fn save(job: u64, fetch: &Fetch, path: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut response = extractors::http::get_following(&fetch.url, &fetch.headers, None)
        .await?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let total = response.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(path).await.map_err(|e| e.to_string())?;
    let mut done = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        progress(job, done, total);
    }
    file.flush().await.map_err(|e| e.to_string())?;
    if done == 0 {
        let _ = tokio::fs::remove_file(path).await;
        return Err("The site sent an empty file".to_string());
    }
    Ok(())
}

async fn ffmpeg(args: Vec<String>) -> Result<(), String> {
    let output = tokio::task::spawn_blocking(move || match crate::platform::ffmpeg_command() {
        Some(mut command) => command.args(&args).output(),
        None => Err(std::io::Error::other("ffmpeg isn't available")),
    })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Couldn't run ffmpeg: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("ffmpeg failed").to_string())
    }
}

/// Saves an HLS stream as one mp4 (stream copy, no re-encoding).
async fn save_hls(fetch: &Fetch, path: &Path) -> Result<(), String> {
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
    ffmpeg(args).await
}

/// Replaces a downloaded video with its audio, as mp3 or m4a.
async fn to_audio(path: &Path, codec: &str, kbps: u32) -> Result<(), String> {
    let (encoder, ext) = match codec {
        "mp3" => ("libmp3lame", "mp3"),
        _ => ("aac", "m4a"),
    };
    let target = path.with_extension(ext);
    // ffmpeg can't write over its own input (an .m4a kept as m4a).
    let out = if target == path { path.with_extension(format!("audio.{ext}")) } else { target.clone() };
    let mut args = vec!["-y".to_string(), "-i".to_string(), path.to_string_lossy().into_owned()];
    args.extend(["-vn", "-c:a", encoder].iter().map(|s| s.to_string()));
    if kbps > 0 {
        args.extend(["-b:a".to_string(), format!("{kbps}k")]);
    }
    args.push(out.to_string_lossy().into_owned());
    ffmpeg(args).await?;
    let _ = tokio::fs::remove_file(path).await;
    if out != target {
        tokio::fs::rename(&out, &target).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}
