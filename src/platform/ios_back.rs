//! iOS: the back swipe from the left edge, as iOS has no Back key. UIKit's
//! own edge-pan recognizer on Slint's view (so Slint's touches, lists'
//! flings included, are left alone until it recognises a swipe from the
//! edge), its distance followed by what Back would close (BackGesture),
//! and Back sent to Slint as Android's Back key is when it's done.

use crate::{BackGesture, DownloaderApp};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_ui_kit::{UIGestureRecognizerState, UIRectEdge, UIScreenEdgePanGestureRecognizer, UIView};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use std::cell::RefCell;

/// How far, of the view's width, a swipe goes for Back; and a flick's
/// speed (points a second) that's Back however short.
const DISTANCE: f64 = 0.35;
const FLICK: f64 = 800.0;

define_class!(
    // The recognizer's target: UIKit calls `swiped:` as the swipe goes.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EddyBackSwipe"]
    struct BackSwipe;

    impl BackSwipe {
        #[unsafe(method(swiped:))]
        fn swiped(&self, recognizer: &UIScreenEdgePanGestureRecognizer) {
            swiped(recognizer);
        }
    }
);

thread_local! {
    static UI: RefCell<Option<slint::Weak<DownloaderApp>>> = const { RefCell::new(None) };
    // The target (a recognizer doesn't keep its target) and the recognizer.
    static KEPT: RefCell<Option<(Retained<BackSwipe>, Retained<UIScreenEdgePanGestureRecognizer>)>> = const { RefCell::new(None) };
}

/// Puts the recognizer on the app's view once its window has one (when
/// the event loop runs). Call from ios_main.
pub fn install(ui: &DownloaderApp) {
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    let ui = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        if let (Some(ui), Some(mtm)) = (ui.upgrade(), MainThreadMarker::new()) {
            attach(&ui, mtm);
        }
    });
}

fn attach(ui: &DownloaderApp, mtm: MainThreadMarker) {
    let handle = ui.window().window_handle();
    let Ok(handle) = HasWindowHandle::window_handle(&handle) else {
        log::warn!("no view for the back swipe");
        return;
    };
    let RawWindowHandle::UiKit(kit) = handle.as_raw() else {
        return;
    };
    // SAFETY: the window's UIView, alive as long as the window (the app).
    let view: &UIView = unsafe { kit.ui_view.cast::<UIView>().as_ref() };
    // SAFETY: a plain NSObject subclass, initialised as one.
    let target: Retained<BackSwipe> = unsafe { msg_send![BackSwipe::alloc(mtm), init] };
    let object: &AnyObject = &target;
    // SAFETY: the target answers `swiped:`, taking the recognizer.
    let recognizer = unsafe {
        UIScreenEdgePanGestureRecognizer::initWithTarget_action(UIScreenEdgePanGestureRecognizer::alloc(mtm), Some(object), Some(sel!(swiped:)))
    };
    recognizer.setEdges(UIRectEdge::Left);
    view.addGestureRecognizer(&recognizer);
    KEPT.with(|k| *k.borrow_mut() = Some((target, recognizer)));
}

/// The swipe moved or ended: its distance and progress to the app, and Back
/// when it went far (or fast) enough.
fn swiped(recognizer: &UIScreenEdgePanGestureRecognizer) {
    let Some(view) = recognizer.view() else {
        return;
    };
    let width = view.bounds().size.width.max(1.0);
    let moved = recognizer.translationInView(Some(&view)).x.max(0.0);
    let progress = (moved / (width * DISTANCE)).min(1.0) as f32;
    let state = recognizer.state();
    let going = state == UIGestureRecognizerState::Began || state == UIGestureRecognizerState::Changed;
    let back = state == UIGestureRecognizerState::Ended && (progress >= 1.0 || recognizer.velocityInView(Some(&view)).x > FLICK);
    let ui = UI.with(|u| u.borrow().as_ref().and_then(|w| w.upgrade()));
    let Some(ui) = ui else {
        return;
    };
    let gesture = ui.global::<BackGesture>();
    gesture.set_dragging(going);
    gesture.set_progress(if going { progress } else { 0.0 });
    if going {
        gesture.set_offset(moved as f32);
        return;
    }
    if !back {
        // Let go short: what moved goes back (animated, no longer dragging).
        gesture.set_offset(0.0);
        return;
    }
    let text: slint::SharedString = slint::platform::Key::Back.into();
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
    // What Back closed keeps its place while it fades; then the offset goes.
    let weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<BackGesture>().set_offset(0.0);
        }
    });
}
