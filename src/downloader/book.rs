//! Saving a web novel as an EPUB (novels::download), as a job like the media
//! downloads: progress counts chapters, and the notification names the
//! book's file.

use super::{Deliver, DownloadRequest, finished, media, runtime};
use crate::novels::NovelOptions;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

/// Starts saving `request.url`'s book into `dir` (`request.format` holds
/// the NovelOptions); returns its job.
pub fn start(request: &DownloadRequest, dir: &str, name: Option<String>, deliver: Option<Deliver>) -> Result<u64, String> {
    let options: NovelOptions = if request.format.is_empty() {
        NovelOptions::default()
    } else {
        serde_json::from_str(&request.format).map_err(|e| format!("Bad book request: {e}"))?
    };
    let label = super::label_for(request);
    let record = request.id;
    let (job, cancelled) = media::begin(&label);
    let (url, dir) = (request.url.clone(), PathBuf::from(dir));
    let task = runtime().spawn(async move {
        let result = crate::novels::download(&url, &options, &dir, name.as_deref(), |done, total| {
            media::progress(job, done as u64, total as u64);
        })
        .await;
        media::end(job);
        if cancelled.load(Ordering::SeqCst) {
            return;
        }
        match result {
            Ok(path) => {
                let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(label);
                finished(record, file, true, String::new(), deliver, Some(path));
            }
            Err(e) => {
                log::error!(target: "books", "{url}: {e}");
                finished(record, label, false, e, None, None)
            }
        }
    });
    media::set_task(job, task.abort_handle());
    Ok(job)
}
