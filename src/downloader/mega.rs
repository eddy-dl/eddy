//! MEGA downloads: a public file, or a public folder's picked files one
//! after another, as a job (like Google Drive's). Each file's link is asked
//! for afresh as it starts (see extractors::mega) and fetched, still
//! encrypted, through aria2 into `<name>.mega` (its .part while it comes in,
//! which a resume carries on); then it's decrypted where it is, checked
//! against its MAC, and given its name.

use super::media::{self, begin, end, next_part, progress, set_task, wrote};
use super::{Deliver, DownloadRequest, finished, runtime};
use crate::extractors::mega::{self, FileKey, Link, MegaRequest};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Starts a MEGA download; returns its job.
pub fn start(request: &DownloadRequest, dir: &str, name: Option<String>, deliver: Option<Deliver>) -> Result<u64, String> {
    let picked: MegaRequest = if request.format.is_empty() {
        MegaRequest::default()
    } else {
        serde_json::from_str(&request.format).map_err(|e| format!("Bad MEGA request: {e}"))?
    };
    let label = super::label_for(request);
    let record = request.id;
    let (job, cancelled) = begin(&label);
    let (url, dir) = (request.url.clone(), PathBuf::from(dir));
    let (settings, resume) = (request.settings.clone(), request.resume);
    let stop = Arc::clone(&cancelled);
    let task = runtime().spawn(async move {
        let result = download(record, job, &url, picked, &dir, name.as_deref(), &settings, resume, stop).await;
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

/// Where a file's link comes from: its own public link, or its folder's.
enum Source<'a> {
    File { handle: &'a str, key: &'a [u8; 32] },
    Node { folder: &'a str, node: &'a str },
}

impl Source<'_> {
    async fn link(&self) -> Result<(String, u64), String> {
        match self {
            Source::File { handle, key } => mega::resolve_file(handle, key).await.map(|r| (r.url, r.size)),
            Source::Node { folder, node } => mega::resolve_node(folder, node).await,
        }
    }
}

/// Saves the file, or the folder's files into a folder of its own (`name`,
/// else the MEGA folder's). Returns the files, and the folder if it made one.
#[allow(clippy::too_many_arguments)]
async fn download(
    record: i64,
    job: u64,
    url: &str,
    picked: MegaRequest,
    dir: &Path,
    name: Option<&str>,
    settings: &super::DownloadSettings,
    resume: bool,
    stop: Arc<AtomicBool>,
) -> Result<(Vec<PathBuf>, Option<PathBuf>), String> {
    let (folder_handle, folder_key, node) = match mega::parse_link(url).ok_or("Not a MEGA link")? {
        Link::File { handle, key } => {
            let resolved = mega::resolve_file(&handle, &key).await?;
            let file_name = media::safe_name(name.unwrap_or(&resolved.name));
            let file_name = if file_name.is_empty() { handle.clone() } else { file_name };
            // Started unchecked ("Download now"): the record shows the
            // file's name, not the link's.
            if name.is_none() {
                crate::downloads::set_folder_name(record, &file_name);
            }
            let source = Source::File { handle: &handle, key: &key };
            let saved = save_file(job, &source, Some(resolved.url), resolved.size, &FileKey::new(&key), &dir.join(file_name), settings, resume, &stop).await?;
            return Ok((vec![saved], None));
        }
        Link::Folder { handle, key, node } => (handle, key, node),
    };
    // Every file when none were picked (a quick download): listed now.
    let (entries, title, single) = if picked.files.is_empty() {
        let folder = mega::list_folder(&folder_handle, &folder_key, node.as_deref()).await?;
        (folder.entries, Some(folder.name), folder.single)
    } else {
        // A folder link to one file inside: that file alone, as it was listed.
        let single = node.is_some() && picked.files.len() == 1 && picked.files[0].handle == node.clone().unwrap_or_default();
        (picked.files, None, single)
    };
    if entries.is_empty() {
        return Err("This MEGA folder has no files to download".to_string());
    }
    if single {
        let entry = &entries[0];
        let key = FileKey::from_b64(&entry.key).ok_or(mega::BAD_KEY)?;
        let file_name = media::safe_name(name.unwrap_or(&entry.path));
        if name.is_none() {
            crate::downloads::set_folder_name(record, &file_name);
        }
        let source = Source::Node { folder: &folder_handle, node: &entry.handle };
        let saved = save_file(job, &source, None, entry.size, &key, &dir.join(file_name), settings, resume, &stop).await?;
        return Ok((vec![saved], None));
    }
    let folder_name = name.map(media::safe_name).filter(|n| !n.is_empty()).or_else(|| title.as_deref().map(media::safe_name)).unwrap_or_else(|| "MEGA".to_string());
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
        if resume && path.exists() && !encrypted_path(&path).exists() && !media::part_path(&encrypted_path(&path)).exists() {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            progress(job, size, size);
            files.push(path);
            continue;
        }
        let key = FileKey::from_b64(&entry.key).ok_or_else(|| format!("{}: {}", entry.path, mega::BAD_KEY))?;
        let source = Source::Node { folder: &folder_handle, node: &entry.handle };
        let saved = save_file(job, &source, None, entry.size, &key, &path, settings, resume, &stop).await.map_err(|e| format!("{}: {e}", entry.path))?;
        wrote(job, &saved);
        files.push(saved);
    }
    Ok((files, Some(folder)))
}

/// Where a file waits, encrypted, while it downloads and is decrypted.
fn encrypted_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".mega");
    path.with_file_name(name)
}

/// Downloads one file (`url`: its link, if it's been asked for already) to
/// `path`, decrypting it once it's in.
#[allow(clippy::too_many_arguments)]
async fn save_file(
    job: u64,
    source: &Source<'_>,
    url: Option<String>,
    size: u64,
    key: &FileKey,
    path: &Path,
    settings: &super::DownloadSettings,
    resume: bool,
    stop: &Arc<AtomicBool>,
) -> Result<PathBuf, String> {
    wrote(job, path);
    // aria2 has nothing to fetch for an empty file (nor is there anything
    // to decrypt).
    if size == 0 {
        let target = if settings.replace_existing { path.to_path_buf() } else { super::free_path(path) };
        tokio::fs::write(&target, b"").await.map_err(|e| format!("Couldn't save it as {}: {e}", target.display()))?;
        return Ok(target);
    }
    let encrypted = encrypted_path(path);
    let journal = mega::journal_path(&encrypted);
    wrote(job, &encrypted);
    wrote(job, &journal);
    // Fetched in full before it stopped (it was being decrypted): on to that.
    let fetched = resume && encrypted.exists() && !media::part_path(&encrypted).exists();
    if !fetched {
        // Starting over: what an earlier try left goes.
        if !resume {
            let _ = tokio::fs::remove_file(&encrypted).await;
            let _ = tokio::fs::remove_file(&journal).await;
        }
        // The encrypted copy takes its place, whatever was there.
        let into = super::DownloadSettings { replace_existing: true, ..settings.clone() };
        let mut url = match url {
            Some(url) => url,
            None => source.link().await?.0,
        };
        let mut refreshed = 0;
        loop {
            match media::save(job, &url, &[], &encrypted, &into, resume || refreshed > 0).await {
                Ok(_) => break,
                // Over its transfer quota, MEGA answers 509.
                Err(e) if e.contains("509") => return Err(mega::QUOTA.to_string()),
                Err(e) if refreshed < 2 && media::link_expired(&e) => {
                    refreshed += 1;
                    log::warn!("{}: MEGA link expired ({e}); getting a new one", path.display());
                    url = source.link().await?.0;
                }
                Err(e) => return Err(e),
            }
        }
    }
    let (file, key, stop) = (encrypted.clone(), *key, Arc::clone(stop));
    let done = tokio::task::spawn_blocking(move || mega::decrypt_file(&file, &key, &stop, |done, total| progress(job, done, total)))
        .await
        .map_err(|e| format!("Couldn't decrypt it: {e}"))?;
    match done {
        Ok(true) => {}
        // Paused or cancelled while it decrypted: the job's over anyway.
        Ok(false) => return Err("Stopped".to_string()),
        Err(e) => {
            // Damaged: a new try fetches it again.
            if e == mega::DAMAGED {
                let _ = tokio::fs::remove_file(&encrypted).await;
            }
            return Err(e);
        }
    }
    let saved = media::settle(&encrypted, path, settings.replace_existing).await?;
    let _ = tokio::fs::remove_file(&journal).await;
    Ok(saved)
}
