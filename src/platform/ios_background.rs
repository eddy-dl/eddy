//! Downloads that keep going in the background (iOS), as Android's download
//! service keeps them going there. The first download started (from the
//! user, in the foreground) asks for time in the background, and one grant
//! covers the whole queue, as a download that's queued can't ask for its own
//! once the app is in the background. It's given back once nothing's left.
//!
//! iOS 26 and later: a BGContinuedProcessingTask, which the system shows
//! with its progress (a task that reports none is ended as stalled). Before
//! that, or when the system won't start one: UIApplication's background
//! task, about 30 seconds after the app leaves the screen.
//!
//! All of it on the main thread (UIKit's and BackgroundTasks' calls), which
//! is Slint's event loop's.

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2_background_tasks::{
    BGContinuedProcessingTask, BGContinuedProcessingTaskRequest, BGContinuedProcessingTaskRequestSubmissionStrategy, BGTask,
    BGTaskScheduler,
};
use objc2_foundation::{NSProgressReporting, NSString};
use objc2_ui_kit::{UIApplication, UIBackgroundTaskIdentifier, UIBackgroundTaskInvalid};
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::time::Duration;

/// The task identifiers' prefix (BGTaskSchedulerPermittedIdentifiers has it
/// as `dev.pah.eddy.downloads.*`, in platforms/ios/project.yml).
const TASK_PREFIX: &str = "dev.pah.eddy.downloads.";

thread_local! {
    /// The running continued-processing task.
    static TASK: RefCell<Option<Retained<BGContinuedProcessingTask>>> = const { RefCell::new(None) };
    /// A task asked for whose launch handler hasn't run yet.
    static SUBMITTED: Cell<bool> = const { Cell::new(false) };
    /// The tasks asked for so far: each gets an identifier of its own (one
    /// can only be registered once).
    static SERIAL: Cell<u32> = const { Cell::new(0) };
    /// UIApplication's background task, when that's what's keeping the
    /// downloads going.
    static FALLBACK: Cell<Option<UIBackgroundTaskIdentifier>> = const { Cell::new(None) };
    /// Downloads ended since the task started, for its progress.
    static ENDED: Cell<i64> = const { Cell::new(0) };
    /// What the task shows (title, subtitle), so it's only told of changes.
    static SHOWN: RefCell<(String, String)> = const { RefCell::new((String::new(), String::new())) };
}

/// Starts checking on the downloads once a second (their ends, as Android's
/// service and desktop's notifications do, and the background task's
/// progress). Call once, from the main thread.
pub fn install() {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), tick);
    // Runs for as long as the app does.
    std::mem::forget(timer);
}

/// A download is starting: the downloads get time in the background, if
/// they haven't already. Any thread.
pub fn download_started() {
    let _ = slint::invoke_from_event_loop(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            begin(mtm);
        }
    });
}

fn begin(mtm: MainThreadMarker) {
    let busy = TASK.with_borrow(|t| t.is_some()) || SUBMITTED.get() || FALLBACK.get().is_some();
    if busy {
        return;
    }
    ENDED.set(0);
    // BGContinuedProcessingTask is iOS 26's.
    if AnyClass::get(c"BGContinuedProcessingTaskRequest").is_some() {
        match submit() {
            Ok(()) => {
                SUBMITTED.set(true);
                return;
            }
            Err(e) => log::warn!("no continued processing task for the downloads: {e}"),
        }
    }
    begin_fallback(mtm);
}

/// Asks for a continued-processing task, started now or not at all (it's
/// asked for in the foreground; queued, it might start once the app has
/// been suspended).
fn submit() -> Result<(), String> {
    let serial = SERIAL.get() + 1;
    SERIAL.set(serial);
    let identifier = NSString::from_str(&format!("{TASK_PREFIX}{serial}"));
    // SAFETY: on the main thread, with an identifier registered once; the
    // handler runs on the main queue, which it expects.
    unsafe {
        let scheduler = BGTaskScheduler::sharedScheduler();
        let handler = RcBlock::new(|task: NonNull<BGTask>| launched(task));
        // Continued-processing tasks may be registered after launch, as
        // they're asked for.
        if !scheduler.registerForTaskWithIdentifier_usingQueue_launchHandler(&identifier, Some(DispatchQueue::main()), &handler) {
            return Err("its identifier isn't permitted".to_string());
        }
        let request = BGContinuedProcessingTaskRequest::initWithIdentifier_title_subtitle(
            BGContinuedProcessingTaskRequest::alloc(),
            &identifier,
            &NSString::from_str("Downloading"),
            &NSString::new(),
        );
        request.setStrategy(BGContinuedProcessingTaskRequestSubmissionStrategy::Fail);
        scheduler.submitTaskRequest_error(&request).map_err(|e| e.localizedDescription().to_string())
    }
}

/// The task started (on the main queue).
fn launched(task: NonNull<BGTask>) {
    SUBMITTED.set(false);
    // SAFETY: the system's task, retained to be kept; it's the
    // continued-processing kind, the only kind asked for.
    let Some(task) = (unsafe { Retained::retain(task.as_ptr()) }) else {
        return;
    };
    let task: Retained<BGContinuedProcessingTask> = unsafe { Retained::cast_unchecked(task) };
    let expired = RcBlock::new(|| match MainThreadMarker::new() {
        Some(_) => expire(),
        None => {
            let _ = slint::invoke_from_event_loop(expire);
        }
    });
    // SAFETY: on the main thread; the handler only reaches the task through
    // TASK (no cycle).
    unsafe { task.setExpirationHandler(Some(&expired)) };
    SHOWN.with_borrow_mut(|shown| *shown = (String::new(), String::new()));
    TASK.with_borrow_mut(|t| *t = Some(task));
    // The downloads may have ended already.
    tick();
}

/// The system took the time back (or the user stopped the task): the
/// downloads pause rather than break off when the app is suspended, and
/// carry on when they're resumed.
fn expire() {
    log::info!("the downloads' background task expired: pausing them");
    crate::downloads::control_all(false);
    complete(false);
}

fn complete(success: bool) {
    if let Some(task) = TASK.with_borrow_mut(|t| t.take()) {
        // SAFETY: on the main thread (TASK is its own).
        unsafe { task.setTaskCompletedWithSuccess(success) };
    }
}

fn begin_fallback(mtm: MainThreadMarker) {
    let app = UIApplication::sharedApplication(mtm);
    // Called on the main thread when the time's up: the task has to end.
    let expired = RcBlock::new(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            end_fallback(mtm);
        }
    });
    let id = app.beginBackgroundTaskWithExpirationHandler(Some(&expired));
    // SAFETY: a constant UIKit defines.
    if id != unsafe { UIBackgroundTaskInvalid } {
        FALLBACK.set(Some(id));
    }
}

fn end_fallback(mtm: MainThreadMarker) {
    if let Some(id) = FALLBACK.take() {
        UIApplication::sharedApplication(mtm).endBackgroundTask(id);
    }
}

fn tick() {
    let ended = crate::downloader::take_finished().len() as i64;
    ENDED.set(ENDED.get() + ended);
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let (count, name, progress) = crate::downloader::summary();
    if count == 0 {
        complete(true);
        end_fallback(mtm);
        return;
    }
    TASK.with_borrow(|task| {
        let Some(task) = task else {
            return;
        };
        // In thousandths: the downloads that ended, and the running ones as
        // one more (their sizes together, when they're known).
        let ended = ENDED.get();
        let running = progress.map(|p| (p.clamp(0.0, 1.0) * 1000.0) as i64).unwrap_or(0);
        let shown = task.progress();
        shown.setTotalUnitCount((ended + 1) * 1000);
        shown.setCompletedUnitCount(ended * 1000 + running);
        let title = match count {
            1 => "Downloading".to_string(),
            n => format!("Downloading {n} files"),
        };
        let changed = SHOWN.with_borrow_mut(|shown| {
            let new = (title, name);
            let changed = *shown != new;
            *shown = new;
            changed
        });
        if changed {
            SHOWN.with_borrow(|(title, subtitle)| {
                        // SAFETY: on the main thread (TASK is its own).
                unsafe { task.updateTitle_subtitle(&NSString::from_str(title), &NSString::from_str(subtitle)) };
            });
        }
    });
}
