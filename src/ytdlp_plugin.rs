//! yt-dlp's community plugin (biggestsonicfan/yt-dlp-community-plugin): more
//! sites, image and gallery ones especially. Installed next to the yt-dlp
//! Eddy runs, in yt-dlp-plugins/<name>/yt_dlp_plugins, where yt-dlp looks
//! for plugins (Android also passes --plugin-dirs).

use std::path::{Path, PathBuf};

const URL: &str = "https://github.com/biggestsonicfan/yt-dlp-community-plugin/archive/refs/heads/master.zip";
const NAME: &str = "biggestsonicfan-yt-dlp-community-plugin";

/// The plugins folder next to yt-dlp in `dir`.
pub fn plugins_dir(dir: &Path) -> PathBuf {
    dir.join("yt-dlp-plugins")
}

pub fn installed(dir: &Path) -> bool {
    plugins_dir(dir).join(NAME).join("yt_dlp_plugins").is_dir()
}

/// Installs the plugin next to yt-dlp in `dir`, unless it's there.
pub async fn install(dir: &Path) -> Result<(), String> {
    if installed(dir) {
        return Ok(());
    }
    let plugin_dir = plugins_dir(dir).join(NAME);
    let archive = crate::downloader::fetch_file(URL, dir, ".download-plugin", |_, _| {}).await?;
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let file = std::fs::File::open(&archive).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            if entry.is_dir() {
                continue;
            }
            // Drop the archive's top folder.
            let Some(path) = entry.enclosed_name() else {
                continue;
            };
            let relative: PathBuf = path.components().skip(1).collect();
            if relative.as_os_str().is_empty() {
                continue;
            }
            let destination = plugin_dir.join(relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out = std::fs::File::create(&destination).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_file(&archive);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}
