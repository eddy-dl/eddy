use super::PrefError;
use std::collections::HashMap;
use std::path::PathBuf;

/// The app's own container (iOS sets HOME to it).
fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

// Preferences: a JSON file, as on desktop.
fn prefs_path() -> PathBuf {
    PathBuf::from(get_app_data_dir()).join("prefs.json")
}

fn read_all_prefs() -> HashMap<String, String> {
    std::fs::read_to_string(prefs_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_pref(key: &str, value: &str) -> Result<(), PrefError> {
    let mut map = read_all_prefs();
    map.insert(key.to_string(), value.to_string());
    std::fs::write(prefs_path(), serde_json::to_string(&map)?)?;
    Ok(())
}

pub fn get_pref(key: &str, default: &str) -> Result<String, PrefError> {
    Ok(read_all_prefs().remove(key).unwrap_or_else(|| default.to_string()))
}

pub fn system_accent() -> Option<slint::Color> {
    None
}

pub fn system_second_accent() -> Option<slint::Color> {
    None
}

/// Documents, which the Files app shows under "On My iPhone › Eddy"
/// (UIFileSharingEnabled, LSSupportsOpeningDocumentsInPlace).
pub fn get_default_download_location() -> String {
    home().join("Documents").to_string_lossy().into_owned()
}

pub fn pick_download_folder<F>(on_picked: F)
where
    F: Fn(String, String) + Send + 'static,
{
    on_picked(String::new(), String::new());
}

pub fn pick_link_file<F>(_on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
}

pub fn pick_cookies_file<F>(_on_picked: F)
where
    F: FnOnce(String, String) + Send + 'static,
{
}

/// In-process, as on desktop (iOS has no download service to hand it to),
/// with time asked for in the background for the queue (see background).
pub fn start_download(request: &crate::downloader::DownloadRequest) -> Result<(), String> {
    crate::downloader::start(request)?;
    super::background::download_started();
    Ok(())
}

/// Opens a download (a file or a folder) in the Files app, which shows
/// Eddy's Documents: a file's preview from there, a folder's contents.
pub fn open_file(path: &str) {
    let path = path.to_string();
    uikit::on_main(move |mtm| {
        if let Some(url) = uikit::files_url(&path) {
            uikit::open(&url, mtm);
        }
    });
}

pub fn open_site_check(_url: &str, _result: &str) -> bool {
    false
}

pub fn open_browser(_page: &str, _result: &str) -> bool {
    false
}

pub fn fetch_page(_url: &str, _result: &str) -> bool {
    false
}

pub fn open_url(url: &str) {
    let url = url.to_string();
    uikit::on_main(move |mtm| {
        if let Some(url) = objc2_foundation::NSURL::URLWithString(&objc2_foundation::NSString::from_str(&url)) {
            uikit::open(&url, mtm);
        }
    });
}

pub fn delete_document(_uri: &str) {}

/// The share sheet, with the files and the text (links).
pub fn share(files: &[String], text: &str) {
    let (files, text) = (files.to_vec(), text.to_string());
    uikit::on_main(move |mtm| uikit::share(&files, &text, mtm));
}

pub fn open_in_folder(_doc: &str, _rel: &str) {}

pub fn has_all_files_access() -> bool {
    false
}

pub fn request_all_files_access() {}

/// Library/Application Support/Eddy: kept and backed up, not shown in Files.
pub fn get_app_data_dir() -> String {
    let dir = home().join("Library").join("Application Support").join("Eddy");
    let _ = std::fs::create_dir_all(&dir);
    dir.to_string_lossy().into_owned()
}

pub fn open_native_url(url: &str) {
    open_url(url);
}
pub fn trigger_haptic(_: super::Haptic) {}
/// The app's snackbar, set up by ios_main (iOS has no toasts).
static TOAST: std::sync::OnceLock<Box<dyn Fn(String) + Send + Sync>> = std::sync::OnceLock::new();

pub fn set_toast_handler(show: impl Fn(String) + Send + Sync + 'static) {
    let _ = TOAST.set(Box::new(show));
}

pub fn show_toast(message: &str) {
    if let Some(show) = TOAST.get() {
        show(message.to_string());
    }
}
pub fn set_bar_icons_dark(_: bool) {}

/// The yt-dlp helper apps are Android's; elsewhere yt-dlp is a normal program.
pub fn helper_apps() -> Option<(std::collections::HashMap<String, String>, String)> {
    None
}

pub fn install_apk(_path: &str) {}

pub fn ffmpeg_command() -> Option<std::process::Command> {
    None
}

pub fn deno_command() -> Option<std::process::Command> {
    None
}

/// No yt-dlp program: it runs in the app's own Python (children::spawn_tool).
pub fn ytdlp_command() -> Option<std::process::Command> {
    None
}

/// yt-dlp is in this build (platforms/ios puts it in app_packages).
pub fn ytdlp_available() -> bool {
    crate::children::available(crate::children::Tool::Ytdlp)
}

pub fn ytdlp_ready() -> bool {
    ytdlp_available()
}

/// Where Eddy would keep its own yt-dlp (Android's; desktop installs its
/// tools through components_desktop instead).
pub fn own_ytdlp() -> std::path::PathBuf {
    std::path::PathBuf::from(get_app_data_dir()).join("yt-dlp")
}

/// UIKit, which wants the main thread (where Slint's event loop runs).
mod uikit {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_ui_kit::{UIActivityViewController, UIApplication, UIViewController, UIWindowScene};

    pub fn on_main(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                f(mtm);
            }
        });
    }

    pub fn open(url: &NSURL, mtm: MainThreadMarker) {
        let options = NSDictionary::<NSString, AnyObject>::new();
        // SAFETY: the options dictionary is empty (its keys' type is NSString).
        unsafe { UIApplication::sharedApplication(mtm).openURL_options_completionHandler(url, &options, None) };
    }

    /// `path` as the Files app's own link to it (shareddocuments://).
    pub fn files_url(path: &str) -> Option<Retained<NSURL>> {
        let file = NSURL::fileURLWithPath(&NSString::from_str(path)).absoluteString()?.to_string();
        let rest = file.strip_prefix("file://")?;
        NSURL::URLWithString(&NSString::from_str(&format!("shareddocuments://{rest}")))
    }

    /// The view controller showing now, to present over.
    fn top_view_controller(mtm: MainThreadMarker) -> Option<Retained<UIViewController>> {
        let scenes = UIApplication::sharedApplication(mtm).connectedScenes().allObjects();
        let window = scenes
            .iter()
            .filter_map(|scene| scene.downcast::<UIWindowScene>().ok())
            .flat_map(|scene| scene.windows().to_vec())
            .find(|window| window.isKeyWindow())?;
        let mut top = window.rootViewController()?;
        while let Some(presented) = top.presentedViewController() {
            top = presented;
        }
        Some(top)
    }

    pub fn share(files: &[String], text: &str, mtm: MainThreadMarker) {
        let mut items: Vec<Retained<AnyObject>> =
            files.iter().map(|f| Retained::into_super(Retained::into_super(NSURL::fileURLWithPath(&NSString::from_str(f))))).collect();
        if !text.is_empty() {
            items.push(Retained::into_super(Retained::into_super(NSString::from_str(text))));
        }
        let items = NSArray::from_retained_slice(&items);
        let Some(top) = top_view_controller(mtm) else {
            return;
        };
        // SAFETY: the items are NSURLs and an NSString, as the share sheet takes.
        let sheet = unsafe { UIActivityViewController::initWithActivityItems_applicationActivities(UIActivityViewController::alloc(mtm), &items, None) };
        // iPad shows it as a popover, which needs somewhere to point from.
        if let (Some(popover), Some(view)) = (sheet.popoverPresentationController(), top.view()) {
            let bounds = view.bounds();
            popover.setSourceView(Some(&view));
            popover.setSourceRect(CGRect::new(CGPoint::new(bounds.size.width / 2.0, bounds.size.height), CGSize::new(0.0, 0.0)));
        }
        top.presentViewController_animated_completion(&sheet, true, None);
    }
}
