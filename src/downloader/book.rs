//! Saving a web novel as an EPUB (novels::download), as a job like the media
//! downloads: progress counts chapters, and the notification names the
//! book's file.

use super::{Deliver, DownloadRequest, finished, media, runtime};
use crate::novels::NovelOptions;
use std::path::PathBuf;

/// Starts saving `request.url`'s book into `dir` (`request.format` holds
/// the NovelOptions).
pub fn start(request: &DownloadRequest, dir: &str, name: Option<String>, deliver: Option<Deliver>) -> Result<(), String> {
    let options: NovelOptions = if request.format.is_empty() {
        NovelOptions::default()
    } else {
        serde_json::from_str(&request.format).map_err(|e| format!("Bad book request: {e}"))?
    };
    let label = super::label_for(request);
    let job = media::begin(&label);
    let (url, dir) = (request.url.clone(), PathBuf::from(dir));
    runtime().spawn(async move {
        let result = crate::novels::download(&url, &options, &dir, name.as_deref(), |done, total| {
            media::progress(job, done as u64, total as u64);
        })
        .await;
        media::end(job);
        match result {
            Ok(path) => {
                let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(label);
                finished(file, true, String::new(), deliver);
            }
            Err(e) => finished(label, false, e, None),
        }
    });
    Ok(())
}
