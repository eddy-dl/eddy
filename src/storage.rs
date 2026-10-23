//! Where downloads are saved.
//!
//! aria2 and yt-dlp can only write to real file paths, but Android's folder
//! picker gives a content:// "tree" URI. So a folder is used in one of three
//! ways, best first:
//! 1. Its real path, when the app can write there: on Android 11+ any app may
//!    create files under Download/ and Documents/ by path, and with "All files
//!    access" (MANAGE_EXTERNAL_STORAGE) anywhere on shared storage.
//! 2. Otherwise, a staging folder in the app's own storage; the finished
//!    download is copied into the picked folder (the download service does
//!    this through Android's document APIs) and the staging copy removed.
//!
//! Whether a folder is writable is found out by trying, so this works out
//! the same whatever the Android version and permissions.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Where a download's files go.
#[derive(Clone, Debug)]
pub enum SaveTarget {
    /// Written straight into this folder.
    Dir(PathBuf),
    /// Written to `staging`, then copied into the picked folder (`tree_uri`)
    /// once finished.
    Staged { tree_uri: String, staging: PathBuf },
}

impl SaveTarget {
    /// The folder the download engine writes to.
    pub fn write_dir(&self) -> &Path {
        match self {
            SaveTarget::Dir(dir) => dir,
            SaveTarget::Staged { staging, .. } => staging,
        }
    }
}

/// Where a download goes. `folder` is a download's own choice (a tree URI on
/// Android, a path on desktop), or "" for the folder picked in settings,
/// or failing that the default (Download/Eddy).
pub fn resolve(folder: &str) -> SaveTarget {
    resolve_for(folder, 0)
}

/// `resolve`, for the download with record `id` (its staging folder, if
/// it needs one, is its own each time it starts).
pub fn resolve_for(folder: &str, id: i64) -> SaveTarget {
    let folder = if folder.is_empty() {
        crate::db::raw_download_path().unwrap_or_default()
    } else {
        folder.to_string()
    };

    if folder.is_empty() {
        let dir = default_dir();
        if writable(&dir) {
            return SaveTarget::Dir(dir);
        }
        return SaveTarget::Dir(private_dir());
    }

    if folder.starts_with("content://") {
        if let Some(path) = tree_uri_to_path(&folder) {
            let path = PathBuf::from(path);
            if writable(&path) {
                return SaveTarget::Dir(path);
            }
        }
        return SaveTarget::Staged { tree_uri: folder, staging: staging_dir(id) };
    }

    SaveTarget::Dir(PathBuf::from(folder))
}

/// The default download folder: Download/Eddy (desktop: the system
/// Downloads folder).
pub fn default_dir() -> PathBuf {
    let downloads = PathBuf::from(crate::platform::get_default_download_location());
    if cfg!(target_os = "android") { downloads.join("Eddy") } else { downloads }
}

/// How the default folder is shown in the UI.
pub fn default_display() -> String {
    if cfg!(target_os = "android") {
        "Download/Eddy".to_string()
    } else {
        default_dir().to_string_lossy().into_owned()
    }
}

/// The app's own downloads folder: always writable, but hidden from other
/// apps on Android. The last resort.
fn private_dir() -> PathBuf {
    crate::platform::get_internal_download_dir()
        .unwrap_or_else(|_| PathBuf::from(crate::platform::get_app_data_dir()))
}

/// A staged download's folder, in the app's own storage: its record's
/// (`id`), so starting it again carries on with what's there; else a
/// fresh one.
fn staging_dir(id: i64) -> PathBuf {
    if id > 0 {
        return PathBuf::from(crate::platform::get_app_data_dir()).join("staging").join(format!("download-{id}"));
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    PathBuf::from(crate::platform::get_app_data_dir())
        .join("staging")
        .join(format!("{stamp}-{}", NEXT.fetch_add(1, Ordering::SeqCst)))
}

/// Whether files can be created in `dir` (creating it if needed).
fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".eddy-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The real path behind an Android external-storage tree URI, e.g.
/// `content://com.android.externalstorage.documents/tree/primary%3AMovies%2FX`
/// is `/storage/emulated/0/Movies/X`, and an SD card's `1234-ABCD:Films` is
/// `/storage/1234-ABCD/Films`. Other providers (cloud drives…) have no path.
pub fn tree_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("content://com.android.externalstorage.documents/tree/")?;
    let id = percent_encoding::percent_decode_str(rest.split('/').next()?).decode_utf8().ok()?;
    let (volume, path) = id.split_once(':')?;
    let root = if volume == "primary" {
        // The primary volume's root, from the Downloads folder's path.
        let downloads = PathBuf::from(crate::platform::get_default_download_location());
        downloads.parent()?.to_string_lossy().into_owned()
    } else {
        format!("/storage/{volume}")
    };
    Some(if path.is_empty() { root } else { format!("{root}/{path}") })
}
