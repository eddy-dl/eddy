//! Google Drive downloads: a shared file, or a shared folder's picked files
//! one after another, as a job (like the native extractors' media). Each
//! file's link is worked out afresh as it starts (see extractors::gdrive)
//! and fetched through aria2 into its .part, which a resume carries on.

use super::media::{self, begin, end, next_part, progress, set_task, wrote};
use super::{Deliver, DownloadRequest, finished, runtime};
use crate::extractors::gdrive::{self, DriveRequest, Link};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Starts a Drive download; returns its job.
pub fn start(request: &DownloadRequest, dir: &str, name: Option<String>, deliver: Option<Deliver>) -> Result<u64, String> {
    let picked: DriveRequest = if request.format.is_empty() {
        DriveRequest::default()
    } else {
        serde_json::from_str(&request.format).map_err(|e| format!("Bad Drive request: {e}"))?
    };
    let label = super::label_for(request);
    let record = request.id;
    let (job, cancelled) = begin(&label);
    let (url, dir) = (request.url.clone(), PathBuf::from(dir));
    let (settings, resume) = (request.settings.clone(), request.resume);
    let task = runtime().spawn(async move {
        let result = download(record, job, &url, picked, &dir, name.as_deref(), &settings, resume).await;
        end(job);
        if cancelled.load(Ordering::SeqCst) {
            return;
        }
        match result {
            Ok((files, folder)) => {
                let name = match (&folder, files.as_slice()) {
                    (None, [file]) => file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(label),
                    (Some(folder), _) => folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(label),
                    _ => label,
                };
                finished(record, name, true, String::new(), deliver, files.first().cloned());
            }
            Err(e) => finished(record, label, false, e, None, None),
        }
    });
    set_task(job, task.abort_handle());
    Ok(job)
}

/// Saves the file, or the folder's files into a folder of its own (`name`,
/// else the Drive folder's). Returns the files, and the folder if it made one.
#[allow(clippy::too_many_arguments)]
async fn download(
    record: i64,
    job: u64,
    url: &str,
    picked: DriveRequest,
    dir: &Path,
    name: Option<&str>,
    settings: &super::DownloadSettings,
    resume: bool,
) -> Result<(Vec<PathBuf>, Option<PathBuf>), String> {
    let link = gdrive::identify(url).await.ok_or("Not a Google Drive link")?;
    let id = match link {
        Link::File(id) => {
            let resolved = gdrive::resolve(&id, "").await?;
            let file_name = media::safe_name(name.unwrap_or(&resolved.name));
            let file_name = if file_name.is_empty() { id.clone() } else { file_name };
            // Started unchecked ("Download now"): the record shows the
            // file's name, not the link's "view".
            if name.is_none() {
                crate::downloads::set_folder_name(record, &file_name);
            }
            let saved = save_file(job, &id, "", resolved, &dir.join(file_name), settings, resume).await?;
            return Ok((vec![saved], None));
        }
        Link::Folder(id) => id,
    };
    // Every file when none were picked (a quick download): listed now.
    let (entries, title) = if picked.files.is_empty() {
        let folder = gdrive::list_folder(&id).await?;
        (folder.entries, Some(folder.name))
    } else {
        (picked.files, None)
    };
    if entries.is_empty() {
        return Err("This Drive folder has no files to download".to_string());
    }
    let folder_name = name.map(media::safe_name).filter(|n| !n.is_empty()).or_else(|| title.as_deref().map(media::safe_name)).unwrap_or_else(|| "Google Drive".to_string());
    let folder = dir.join(folder_name);
    // A folder of that name already there (and not this download's own,
    // being resumed): a free name, unless replacing, kept so resuming finds it.
    let in_use = |d: &Path| std::fs::read_dir(d).is_ok_and(|mut e| e.next().is_some());
    let folder = if !resume && !settings.replace_existing && in_use(&folder) { super::free_path(&folder) } else { folder };
    // Unchecked ("Download now"), or a free name: the record has it now.
    if name.is_none() || folder.file_name().and_then(|n| n.to_str()) != name {
        if let Some(folder_name) = folder.file_name() {
            crate::downloads::set_folder_name(record, &folder_name.to_string_lossy());
        }
    }
    let mut files = Vec::new();
    for entry in &entries {
        let path = entry.path.split('/').map(media::safe_name).filter(|p| !p.is_empty() && p != "..").fold(folder.clone(), |p, part| p.join(part));
        next_part(job, entries.len(), &entry.path);
        wrote(job, &path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| format!("Couldn't make the folder: {e}"))?;
        }
        // Finished before it stopped.
        if resume && !media::part_path(&path).exists() && path.exists() {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            progress(job, size, size);
            files.push(path);
            continue;
        }
        let resolved = gdrive::resolve(&entry.id, &entry.native).await.map_err(|e| format!("{}: {e}", entry.path))?;
        let saved = save_file(job, &entry.id, &entry.native, resolved, &path, settings, resume).await.map_err(|e| format!("{}: {e}", entry.path))?;
        wrote(job, &saved);
        files.push(saved);
    }
    Ok((files, Some(folder)))
}

/// Downloads one resolved file to `path` (see media::save), with a fresh
/// link when Drive stops honouring the one it gave.
async fn save_file(
    job: u64,
    id: &str,
    native: &str,
    mut resolved: gdrive::Resolved,
    path: &Path,
    settings: &super::DownloadSettings,
    resume: bool,
) -> Result<PathBuf, String> {
    wrote(job, path);
    // aria2 has nothing to fetch for an empty file.
    if resolved.size == Some(0) {
        let target = if settings.replace_existing { path.to_path_buf() } else { super::free_path(path) };
        tokio::fs::write(&target, b"").await.map_err(|e| format!("Couldn't save it as {}: {e}", target.display()))?;
        return Ok(target);
    }
    if resolved.export {
        return save_export(job, &resolved.url, path, settings.replace_existing).await;
    }
    let mut refreshed = 0;
    loop {
        match media::save(job, &resolved.url, &[], path, settings, resume || refreshed > 0).await {
            Ok(saved) => return Ok(saved),
            Err(e) if refreshed < 2 && media::link_expired(&e) => {
                refreshed += 1;
                log::warn!("{}: Drive link expired ({e}); getting a new one", path.display());
                resolved = gdrive::resolve(id, native).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// A Docs export, fetched whole into its .part (made on request, it has no
/// size to split by, nor a place to carry on from).
async fn save_export(job: u64, url: &str, path: &Path, replace: bool) -> Result<PathBuf, String> {
    use tokio::io::AsyncWriteExt;
    let part = media::part_path(path);
    let mut response = crate::extractors::http::client()
        .get(url)
        // A big sheet takes a while to make and send.
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Google Drive: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Google Drive couldn't export it (HTTP {})", response.status().as_u16()));
    }
    let total = response.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(&part).await.map_err(|e| format!("Couldn't write {}: {e}", part.display()))?;
    let mut done = 0;
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("Google Drive: {e}"))? {
        file.write_all(&chunk).await.map_err(|e| format!("Couldn't write {}: {e}", part.display()))?;
        done += chunk.len() as u64;
        progress(job, done, total);
    }
    file.flush().await.map_err(|e| format!("Couldn't write {}: {e}", part.display()))?;
    drop(file);
    media::settle(&part, path, replace).await
}
