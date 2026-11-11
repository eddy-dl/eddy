//! The browser for sites behind bot checks (iOS), as the desktop helper is
//! (eddy-webview, whose jobs these are: webview/src/lib.rs) and Android's
//! WebView (PageFetcher, CloudflareActivity, BrowserActivity). A WKWebView
//! in Eddy's own window: hidden while it reads pages, over the whole app
//! with a bar to close it while the user passes a check or clicks a page's
//! download. Built the first time a site
//! needs it, and polled by a timer while it has a job.
//!
//! All of it on the main thread (UIKit's and WebKit's), which is Slint's
//! event loop's.

use crate::DownloaderApp;
use block2::RcBlock;
use eddy_webview::wry::{NewWindowResponse, WebView, WebViewBuilder, WebViewExtIOS};
use eddy_webview::{Answered, Host, Job, Jobs, POLL};
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSString;
use objc2_ui_kit::{UIAction, UIButton, UIColor, UIView, UIViewAutoresizing};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::ptr::NonNull;

/// The bar over the page, with its Close button, in points.
const BAR: f64 = 44.0;

thread_local! {
    /// The app's window, for the browser to go in.
    static APP: RefCell<Option<slint::Weak<DownloaderApp>>> = const { RefCell::new(None) };
    static JOBS: RefCell<Option<Jobs<Ios>>> = const { RefCell::new(None) };
    /// Polls the job in hand; stopped while there's none.
    static TIMER: slint::Timer = slint::Timer::default();
}

pub fn install(ui: &DownloaderApp) {
    APP.with(|app| *app.borrow_mut() = Some(ui.as_weak()));
}

/// Hands `job` to the browser (from any thread), building it first if need be.
pub fn send(kind: &str, url: &str, result: &str) -> bool {
    let Some(job) = Job::new(kind, url, result) else {
        return false;
    };
    slint::invoke_from_event_loop(move || add(job)).is_ok()
}

fn add(job: Job) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let built = JOBS.with(|jobs| jobs.try_borrow().map(|jobs| jobs.is_some()));
    match built {
        // Busy (a script's answer being waited for): a moment later.
        Err(_) => {
            let _ = slint::invoke_from_event_loop(move || add(job));
            return;
        }
        Ok(false) => match build(mtm) {
            Ok(built) => JOBS.with(|jobs| *jobs.borrow_mut() = Some(built)),
            Err(e) => {
                log::warn!("no browser for {}: {e}", job.url);
                let text = if job.kind == eddy_webview::Kind::Fetch { format!("error no browser: {e}") } else { String::new() };
                eddy_webview::write_result(&job.result, &text);
                return;
            }
        },
        Ok(true) => {}
    }
    with_jobs(move |jobs| jobs.add(job));
    if !TIMER.with(|timer| timer.running()) {
        TIMER.with(|timer| timer.start(slint::TimerMode::Repeated, POLL, poll));
    }
}

fn poll() {
    let busy = JOBS.with(|jobs| match jobs.try_borrow_mut() {
        Ok(mut jobs) => jobs.as_mut().is_some_and(|jobs| {
            jobs.poll();
            jobs.busy()
        }),
        Err(_) => true,
    });
    if !busy {
        TIMER.with(|timer| timer.stop());
    }
}

/// Runs `f` on the jobs, or a moment later when they're in use (a blocking
/// WebKit call runs the main loop meanwhile).
fn with_jobs(f: impl FnOnce(&mut Jobs<Ios>) + 'static) {
    JOBS.with(|jobs| match jobs.try_borrow_mut() {
        Ok(mut jobs) => {
            if let Some(jobs) = jobs.as_mut() {
                f(jobs);
            }
        }
        Err(_) => {
            // Not Send: kept on this thread, run from the timer's next turn.
            LATER.with(|later| later.borrow_mut().push(Box::new(f)));
            let _ = slint::invoke_from_event_loop(run_later);
        }
    });
}

thread_local! {
    static LATER: RefCell<Vec<Box<dyn FnOnce(&mut Jobs<Ios>)>>> = const { RefCell::new(Vec::new()) };
}

fn run_later() {
    let later = LATER.with(|later| std::mem::take(&mut *later.borrow_mut()));
    for f in later {
        with_jobs(f);
    }
}

/// The webview, a child of the app's view, hidden.
fn build(mtm: MainThreadMarker) -> Result<Jobs<Ios>, String> {
    let ui = APP.with(|app| app.borrow().as_ref().and_then(|app| app.upgrade())).ok_or("the app's window has gone")?;
    let handle = ui.window().window_handle();
    let webview = WebViewBuilder::new()
        .with_visible(false)
        // Not saved here: Eddy downloads it (a browse job's file).
        .with_download_started_handler(|url, path| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || with_jobs(move |jobs| jobs.download_started(url, name)));
            false
        })
        .with_new_window_req_handler(|url, _features| {
            let _ = slint::invoke_from_event_loop(move || with_jobs(move |jobs| jobs.new_window(url)));
            NewWindowResponse::Deny
        })
        .build_as_child(&handle)
        .map_err(|e| e.to_string())?;
    let view = webview.webview();
    let parent = view.superview().ok_or("the webview isn't in the app's window")?;
    let (bar, close) = close_bar(mtm);
    bar.setHidden(true);
    parent.addSubview(&bar);
    Ok(Jobs::new(Ios { bar, close }, webview))
}

/// A bar across the top with a Close button, for a check the user doesn't
/// want to pass (as the desktop helper's window is closed).
fn close_bar(mtm: MainThreadMarker) -> (Retained<UIView>, Retained<UIButton>) {
    let bar = UIView::new(mtm);
    bar.setBackgroundColor(Some(&UIColor::systemBackgroundColor()));
    bar.setAutoresizingMask(UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleBottomMargin);
    let closed: RcBlock<dyn Fn(NonNull<UIAction>)> = RcBlock::new(|_action: NonNull<UIAction>| {
        let _ = slint::invoke_from_event_loop(|| with_jobs(|jobs| jobs.closed()));
    });
    // SAFETY: the block is valid, and UIKit copies it.
    let action = unsafe {
        UIAction::actionWithTitle_image_identifier_handler(&NSString::from_str("Close"), None, None, RcBlock::as_ptr(&closed), mtm)
    };
    let close = UIButton::systemButtonWithPrimaryAction(Some(&action), mtm);
    bar.addSubview(&close);
    (bar, close)
}

/// The webview in Eddy's window, and the bar over it.
struct Ios {
    bar: Retained<UIView>,
    close: Retained<UIButton>,
}

impl Host for Ios {
    fn show(&mut self, webview: &WebView, shown: bool) {
        let view = webview.webview();
        if let Some(parent) = view.superview().filter(|_| shown) {
            // The bar below the status bar (or the notch), the page below it.
            let bounds = parent.bounds();
            let safe = parent.safeAreaInsets().top;
            let top = safe + BAR;
            self.bar.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(bounds.size.width, top)));
            self.close.setFrame(CGRect::new(CGPoint::new(8.0, safe), CGSize::new(88.0, BAR)));
            view.setFrame(CGRect::new(CGPoint::new(0.0, top), CGSize::new(bounds.size.width, (bounds.size.height - top).max(0.0))));
            parent.bringSubviewToFront(&view);
            parent.bringSubviewToFront(&self.bar);
        }
        self.bar.setHidden(!shown);
        let _ = webview.set_visible(shown);
    }

    fn sender(&self) -> Box<dyn Fn(Answered) + Send> {
        Box::new(|answered| {
            let _ = slint::invoke_from_event_loop(move || with_jobs(move |jobs| jobs.answered(answered)));
        })
    }
}
