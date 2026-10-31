//! Files that only come from a page's download button (a countdown, a
//! captcha, a link made when it's clicked): the page opens in Eddy's
//! browser (Android's BrowserActivity), the user clicks through it, and the
//! download it starts is caught there and handed to Eddy's downloader, with
//! the browser's cookies, user agent and the page it was on.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long the browser may stay open (the user reads, waits, taps).
const TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A download caught in the browser.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct Caught {
    /// The file's link.
    pub url: String,
    /// The browser's, which the link (and cookies) may be tied to.
    pub user_agent: String,
    pub cookies: String,
    /// The page the download was clicked on.
    pub referer: String,
    /// Its name, from the site or guessed from the link.
    pub filename: String,
    pub mime: String,
    /// Bytes; 0 when the site didn't say.
    pub size: u64,
}

impl Caught {
    /// What a download of it goes with (`page`: where the browser started).
    pub fn pass(&self, page: &str) -> crate::downloader::BrowserPass {
        crate::downloader::BrowserPass {
            page: page.to_string(),
            cookies: self.cookies.clone(),
            referer: self.referer.clone(),
        }
    }
}

/// One browser at a time.
static OPEN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Opens `page` in Eddy's browser and waits for the download clicked there.
/// Err when it can't open here, or was closed without one.
pub async fn catch(page: &str) -> Result<Caught, String> {
    let _one = OPEN.lock().await;
    let file = PathBuf::from(crate::platform::get_app_data_dir()).join("browser").join("caught.json");
    let _ = std::fs::remove_file(&file);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if !crate::platform::open_browser(page, &file.to_string_lossy()) {
        return Err("Eddy's browser is only on Android for now".to_string());
    }
    let began = Instant::now();
    while began.elapsed() < TIMEOUT {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Written whole, then moved into place; empty when closed.
        if let Ok(text) = std::fs::read_to_string(&file) {
            let _ = std::fs::remove_file(&file);
            return match serde_json::from_str::<Caught>(&text) {
                Ok(caught) if !caught.url.is_empty() => Ok(caught),
                _ => Err("The browser was closed before a download started".to_string()),
            };
        }
    }
    Err("The browser was open too long".to_string())
}
