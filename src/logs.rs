//! Logs for users: warnings and errors written to `logs/` in the download
//! folder (or Eddy's own folder when that can't be written), one file per
//! part of the app, so a failed download or link check can be looked into
//! without logcat:
//!
//! - ytdlp.log: yt-dlp's errors, with the end of what it printed;
//! - aria2.log: aria2's (direct downloads, torrents, Eddy's own files);
//! - books.log: the novels fetcher and book downloads;
//! - extractors.log: the TikTok, Instagram and Pinterest extractors;
//! - app.log: the rest (link checks, connection errors, the app itself).
//!
//! Eddy logs with `log` (also to logcat / the terminal as before); aria2-core
//! with `tracing`, caught by a small layer. Each file stops at 1 MB, its
//! older half kept as `.log.1`; and as the app starts, a file over 512 KB
//! is cut to its newest 256 KB (`trim`).

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_SIZE: u64 = 1024 * 1024;

/// Sets logging up, once (any of Android's entry points may call it).
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        #[cfg(target_os = "android")]
        let inner: Box<dyn log::Log> = Box::new(android_logger::AndroidLogger::new(
            android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("eddy"),
        ));
        #[cfg(not(target_os = "android"))]
        let inner: Box<dyn log::Log> = Box::new(Stderr);
        if log::set_boxed_logger(Box::new(Tee { inner })).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
        use tracing_subscriber::layer::SubscriberExt;
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(TracingFiles));
    });
}

/// A log file over this when the app starts is cut down to its newest
/// TRIM_TO bytes.
const TRIM_OVER: u64 = 512 * 1024;
const TRIM_TO: u64 = 256 * 1024;

/// Cuts the logs that grew too big down to their newest lines, once per
/// run, in the background. Call once the app's folders are known.
pub fn trim() {
    static TRIM: Once = Once::new();
    TRIM.call_once(|| {
        std::thread::spawn(|| {
            let Some(dir) = dir() else {
                return;
            };
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.metadata().is_ok_and(|m| m.is_file() && m.len() > TRIM_OVER) {
                    if let Err(e) = trim_file(&path) {
                        log::warn!("couldn't trim {}: {e}", path.display());
                    }
                }
            }
        });
    });
}

/// Keeps the file's last TRIM_TO bytes, from the start of a line.
fn trim_file(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TRIM_TO)))?;
    let mut tail = Vec::with_capacity(TRIM_TO as usize);
    file.read_to_end(&mut tail)?;
    // The first line is likely cut off part way.
    let start = tail.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    let tmp = path.with_extension("trim");
    std::fs::write(&tmp, &tail[start..])?;
    std::fs::rename(&tmp, path)
}

/// Which file a message goes to, by where it came from.
fn file_for(target: &str) -> &'static str {
    match target {
        t if t == "ytdlp" || t.contains("extractors::ytdlp") => "ytdlp.log",
        t if t == "aria2" || t.starts_with("aria2") || t.contains("downloader::aria2") => "aria2.log",
        t if t == "books" || t.contains("novels") || t.contains("downloader::book") => "books.log",
        t if t == "extractors" || t.contains("extractors") => "extractors.log",
        _ => "app.log",
    }
}

/// Appends a line to `file`.
fn write(file: &str, level: &str, target: &str, message: &str) {
    let Some(dir) = dir() else {
        return;
    };
    let path = dir.join(file);
    // Too big: the newer half carries on, the older is kept once.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_SIZE) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {level:<5} [{target}] {}", timestamp(), message.trim_end());
    }
}

/// The logs folder, looked up again every half minute (the download folder
/// can change).
fn dir() -> Option<PathBuf> {
    static CACHE: Mutex<Option<(PathBuf, Instant)>> = Mutex::new(None);
    let mut cache = CACHE.try_lock().ok()?;
    if let Some((dir, at)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(30) {
            return Some(dir.clone());
        }
    }
    let base = match crate::storage::resolve("") {
        crate::storage::SaveTarget::Dir(dir) => dir,
        // A picked folder only copied into: Eddy's own instead.
        crate::storage::SaveTarget::Staged { .. } => PathBuf::from(crate::platform::get_app_data_dir()),
    };
    let mut dir = base.join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        dir = PathBuf::from(crate::platform::get_app_data_dir()).join("logs");
        std::fs::create_dir_all(&dir).ok()?;
    }
    *cache = Some((dir.clone(), Instant::now()));
    Some(dir)
}

/// "2026-10-22 10:15:03Z".
fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// Eddy's `log` messages: on to logcat / the terminal, and warnings and
/// errors into the files.
struct Tee {
    inner: Box<dyn log::Log>,
}

impl log::Log for Tee {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.inner.log(record);
        if record.level() <= log::Level::Warn {
            let target = record.target();
            write(file_for(target), &record.level().to_string(), target, &record.args().to_string());
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(not(target_os = "android"))]
struct Stderr;

#[cfg(not(target_os = "android"))]
impl log::Log for Stderr {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("{} [{}] {}", record.level(), record.target(), record.args());
    }
    fn flush(&self) {}
}

/// aria2-core's `tracing` warnings and errors.
struct TracingFiles;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TracingFiles {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let metadata = event.metadata();
        if *metadata.level() > tracing::Level::WARN {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        let target = metadata.target();
        write(file_for(target), &metadata.level().to_string(), target, &message.0);
    }
}

/// An event's message and fields as one line.
struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.insert_str(0, &format!("{value:?}"));
        } else {
            self.0 += &format!(" {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.insert_str(0, value);
        } else {
            self.0 += &format!(" {}={value}", field.name());
        }
    }
}
