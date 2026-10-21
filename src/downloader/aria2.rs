use aria2_core::engine::engine_command::{EngineCommand, EngineCommandSender};
use aria2_core::request::request_group::{
    DownloadOptions, DownloadStatus, FollowMode, GroupId, RequestGroup,
};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;
use std::sync::Arc;
use std::time::Duration;

/// Magnets can take a while to find peers for metadata
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Same rule aria2-core uses to decide a finished download spawns follow-up
/// downloads (torrent/metalink handlers): content type or path extension.
fn may_spawn_followers(content_type: Option<&str>, paths: &[String]) -> bool {
    const TYPES: &[&str] = &[
        "application/x-bittorrent",
        "application/metalink4+xml",
        "application/metalink+xml",
        "application/x-metalink",
    ];
    const EXTS: &[&str] = &[".torrent", ".meta4", ".metalink", ".metalink3"];
    if let Some(ct) = content_type {
        let ct = ct.to_ascii_lowercase();
        if TYPES.contains(&ct.split(';').next().unwrap_or("").trim()) {
            return true;
        }
    }
    paths.iter().any(|p| {
        let p = p.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
        EXTS.iter().any(|e| p.ends_with(e))
    })
}

/// A file a probe found, e.g. one inside a torrent.
#[derive(Debug, Clone)]
pub struct ProbeFile {
    /// 1-based, as aria2's `select-file` counts.
    pub index: usize,
    pub path: String,
    pub length: u64,
}

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub gid: String,
    pub filename: String,
    pub completed_length: u64,
    pub total_length: u64,
    pub download_speed: f64,
    pub status: String,
}

#[derive(Clone)]
pub struct AriaEngineClient {
    // For reading state and event listener subscriptions
    group_man: Arc<RequestGroupMan>,
    // For dispatching mutations to the running engine loop
    cmd_tx: EngineCommandSender,
}

impl AriaEngineClient {
    pub fn new(group_man: Arc<RequestGroupMan>, cmd_tx: EngineCommandSender) -> Self {
        Self { group_man, cmd_tx }
    }

    /// Fetches live progress by directly querying the engine's memory
    pub fn get_progress(&self, target_gid: &str) -> Option<DownloadProgress> {
        // 1. Check active/waiting groups
        let all_groups = self.group_man.all_groups();
        for (gid, group_lock) in &all_groups {
            if gid.to_hex_string() == target_gid {
                let group = group_lock.recover();
                let snapshot = group.status_snapshot();

                let filename = group
                    .uris()
                    .first()
                    .and_then(|u| u.rsplit('/').next())
                    .unwrap_or("unknown")
                    .to_string();

                let status_str = match group.status() {
                    DownloadStatus::Active => "active",
                    DownloadStatus::Waiting => "waiting",
                    DownloadStatus::Paused => "paused",
                    DownloadStatus::Complete => "complete",
                    DownloadStatus::Error(_) => "error",
                    DownloadStatus::Removed => "removed",
                };

                return Some(DownloadProgress {
                    gid: target_gid.to_string(),
                    filename,
                    completed_length: snapshot.completed_length,
                    total_length: snapshot.total_length,
                    download_speed: snapshot.download_speed as f64,
                    status: status_str.to_string(),
                });
            }
        }

        // 2. Check terminal results
        for result in self.group_man.get_stopped_results(0, usize::MAX) {
            if result.gid_hex() == target_gid {
                let status_str = if result.code.is_success() {
                    "complete"
                } else if result.code.is_user_stopped() {
                    "removed"
                } else {
                    "error"
                };

                return Some(DownloadProgress {
                    gid: target_gid.to_string(),
                    filename: result
                        .files
                        .first()
                        .map(|f| f.path.clone())
                        .unwrap_or_default(),
                    completed_length: result.completed_length,
                    total_length: result.total_length,
                    download_speed: result.download_speed as f64,
                    status: status_str.to_string(),
                });
            }
        }
        None
    }

    /// Submits a task to the engine via the command channel. `out` names
    /// the saved file; None lets aria2 pick it from the URL or server.
    /// `select_files` limits a torrent to some of its files ("1,3", 1-based);
    /// None downloads everything.
    pub fn start_download(
        &self,
        uri: &str,
        save_dir: &str,
        out: Option<&str>,
        select_files: Option<&str>,
    ) -> Result<String, String> {
        let gid = self.group_man.next_available_gid();

        let mut options = DownloadOptions::default();
        options.dir = Some(save_dir.to_string());
        options.out = out.map(str::to_string);
        options.select_file = select_files.map(str::to_string);
        options.continue_download = true;
        options.max_connection_per_server = Some(16);
        options.split = Some(16);

        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options,
        )));

        self.cmd_tx
            .send(EngineCommand::AddDownload { group })
            .map_err(|e| format!("Failed to dispatch AddDownload: {}", e))?;

        Ok(gid.to_hex_string())
    }

    /// Determines the total payload size by starting a throwaway download.
    /// Waits out magnet metadata and follows torrent/metalink child downloads.
    pub async fn check_file_size(&self, uri: &str) -> Result<u64, String> {
        self.check_files(uri, PROBE_TIMEOUT).await.map(|(total, _)| total)
    }

    /// Like `check_file_size`, also listing the files (for a torrent, the
    /// files inside it, with aria2's 1-based `select-file` indexes), giving
    /// up after `timeout`.
    pub async fn check_files(&self, uri: &str, timeout: Duration) -> Result<(u64, Vec<ProbeFile>), String> {
        // Probes start a real download, so give each one a dir we can delete
        // afterwards. Inside the app's own data dir: Android apps can't write
        // to the system temp dir.
        let probe_dir = std::path::PathBuf::from(crate::platform::get_app_data_dir()).join(format!(
            "eddy-probe-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));

        let gid = self.group_man.next_available_gid();

        let mut options = DownloadOptions::default();
        options.dir = Some(probe_dir.to_string_lossy().into_owned());
        options.follow_torrent = Some(FollowMode::Memory);

        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options,
        )));

        // Register before dispatch so the probe can see the group straight away
        self.group_man.add_group_arc(Arc::clone(&group));
        if let Err(e) = self.cmd_tx.send(EngineCommand::AddDownload { group }) {
            self.group_man.remove_group_by_id(gid);
            return Err(format!("Failed to add probing task: {}", e));
        }

        // Every GID the probe touched, kept outside the timeout so cleanup still runs
        let mut targets = vec![gid];
        let result = tokio::time::timeout(timeout, self.probe_total_length(&mut targets))
            .await
            .unwrap_or_else(|_| Err("Timed out waiting for file size".to_string()));

        for gid in &targets {
            let _ = self.cmd_tx.send(EngineCommand::RemoveDownload { gid: *gid });
        }
        // Removal is async and doesn't delete files; give the engine a moment to let go
        tokio::time::sleep(Duration::from_millis(500)).await;
        for gid in &targets {
            self.group_man.remove_stopped_result(&gid.to_hex_string());
        }
        let _ = std::fs::remove_dir_all(&probe_dir);

        // Paths come back under the probe dir; keep them relative.
        let prefix = format!("{}/", probe_dir.to_string_lossy());
        result.map(|(total, files)| {
            let files = files
                .into_iter()
                .map(|f| ProbeFile {
                    path: f.path.strip_prefix(&prefix).map(str::to_string).unwrap_or(f.path),
                    ..f
                })
                .collect();
            (total, files)
        })
    }

    /// Polls the probe's groups until every payload download reports a size.
    /// `targets` grows as torrent/metalink parents gain followed_by children.
    async fn probe_total_length(&self, targets: &mut Vec<GroupId>) -> Result<(u64, Vec<ProbeFile>), String> {
        let to_probe_files = |files: Vec<aria2_core::request::request_group::FileEntry>| {
            files
                .into_iter()
                .map(|f| ProbeFile { index: f.index, path: f.path, length: f.length })
                .collect::<Vec<_>>()
        };
        loop {
            let mut total = 0;
            let mut found_files = Vec::new();
            let mut pending = false;
            let mut i = 0;
            while i < targets.len() {
                let gid = targets[i];
                i += 1;

                let (followed_by, size, finished, may_follow, group_files) =
                    if let Some(group_lock) = self.group_man.find_group(gid) {
                        let group = group_lock.recover();
                        let files = group.create_download_result().files;
                        let group_files = to_probe_files(files.clone());
                        let paths: Vec<String> = files
                            .into_iter()
                            .flat_map(|f| {
                                std::iter::once(f.path).chain(f.uris.into_iter().map(|u| u.uri))
                            })
                            .collect();
                        let may_follow =
                            may_spawn_followers(group.content_type().as_deref(), &paths);
                        (group.followed_by_gids(), group.total_length(), false, may_follow, group_files)
                    } else if let Some(result) =
                        self.group_man.find_stopped_result(&gid.to_hex_string())
                    {
                        match result.status {
                            DownloadStatus::Error(_) => return Err(result.message),
                            DownloadStatus::Removed => {
                                return Err("Probe download was removed".to_string());
                            }
                            // Stopped results get followed_by filled in before demotion
                            _ => (
                                result.followed_by,
                                result.total_length,
                                true,
                                false,
                                to_probe_files(result.files),
                            ),
                        }
                    } else {
                        // Not picked up by the engine yet
                        pending = true;
                        continue;
                    };

                if !followed_by.is_empty() {
                    // A torrent/metalink file: its children carry the real payload
                    for child in followed_by {
                        if !targets.contains(&child) {
                            targets.push(child);
                        }
                    }
                } else if may_follow && !finished {
                    // Looks like a torrent/metalink still downloading; wait for its children
                    pending = true;
                } else if size > 0 {
                    total += size;
                    found_files.extend(group_files);
                } else if !finished {
                    // No Content-Length / magnet metadata / BT setup yet
                    pending = true;
                }
            }

            if total > 0 && !pending {
                return Ok((total, found_files));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Starts a BitTorrent download from the contents of a .torrent file.
    pub fn start_torrent(
        &self,
        torrent: Vec<u8>,
        save_dir: &str,
        select_files: Option<&str>,
    ) -> Result<String, String> {
        let gid = self.group_man.next_available_gid();

        let mut options = DownloadOptions::default();
        options.dir = Some(save_dir.to_string());
        options.select_file = select_files.map(str::to_string);
        options.continue_download = true;

        // Same steps aria2-core's own add_torrent takes: a bt:// placeholder
        // URI, with the file list and pieces read from the torrent.
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![format!("bt://{}", gid.to_hex_string())],
            options.clone(),
        )));
        aria2_core::engine::bittorrent::download::command::prepare_group_metadata(
            Arc::clone(&group),
            &torrent,
            &options,
            options.dir.as_deref(),
            &[],
        )
        .map_err(|e| e.to_string())?;
        group.recover().set_bt_metadata_data(torrent);

        self.group_man.add_group_arc(Arc::clone(&group));
        if let Err(e) = self.cmd_tx.send(EngineCommand::AddDownload { group }) {
            self.group_man.remove_group_by_id(gid);
            return Err(format!("Failed to dispatch AddDownload: {e}"));
        }
        Ok(gid.to_hex_string())
    }

    /// Downloads that are running or queued (paused ones don't count).
    pub fn active_count(&self) -> usize {
        self.group_man
            .all_groups()
            .iter()
            .filter(|(_, group)| {
                matches!(group.recover().status(), DownloadStatus::Active | DownloadStatus::Waiting)
            })
            .count()
    }

    /// Why a download stopped, if aria2 recorded a reason.
    pub fn error_message(&self, gid_hex: &str) -> Option<String> {
        let gid = GroupId::from_hex_string(gid_hex)?;
        if let Some(group) = self.group_man.find_group(gid) {
            if let DownloadStatus::Error(message) = group.recover().status() {
                return Some(message);
            }
        }
        self.group_man
            .find_stopped_result(gid_hex)
            .map(|r| r.message)
            .filter(|m| !m.is_empty())
    }

    /// Downloads this one handed its payload to (a .torrent or magnet's
    /// metadata download is followed by the torrent's own).
    pub fn followed_by(&self, gid_hex: &str) -> Vec<String> {
        let Some(gid) = GroupId::from_hex_string(gid_hex) else {
            return Vec::new();
        };
        let children = match self.group_man.find_group(gid) {
            Some(group) => group.recover().followed_by_gids(),
            None => self
                .group_man
                .find_stopped_result(gid_hex)
                .map(|r| r.followed_by)
                .unwrap_or_default(),
        };
        children.iter().map(|g| g.to_hex_string()).collect()
    }

    /// Safely commands the engine to unpause a specific GID
    pub fn resume_download(&self, gid_hex: &str) -> Result<(), String> {
        let gid = GroupId::from_hex_string(gid_hex).ok_or("Invalid GID")?;
        self.cmd_tx
            .send(EngineCommand::Unpause { gid })
            .map_err(|e| e.to_string())
    }

    /// Safely commands the engine to pause
    pub fn pause_download(&self, gid_hex: &str) -> Result<(), String> {
        let gid = GroupId::from_hex_string(gid_hex).ok_or("Invalid GID")?;
        self.cmd_tx
            .send(EngineCommand::Pause { gid })
            .map_err(|e| e.to_string())
    }
}
