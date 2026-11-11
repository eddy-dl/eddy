//! eddy-webview: the desktop app's browser for sites behind bot checks, as
//! Android's WebView is for the phone (PageFetcher, CloudflareActivity).
//! Eddy starts it the first time a site needs it (platform::desktop) and
//! hands it one job a line on stdin, "<kind>\t<url>\t<result file>": the
//! jobs (lib.rs) are done in one webview, with its cookies kept in the folder
//! given as the only argument; a `check` or `browse` shows in a window of its
//! own, which the user passes it in (or clicks the download in) or closes.
//! It says "ready" on
//! stdout once the webview is up, and quits when stdin closes (Eddy went)
//! or after a while with nothing to do.


#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eddy_webview::{Answered, Host, Job, Jobs, POLL};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use wry::{NewWindowResponse, WebContext, WebView, WebViewBuilder};

enum UserEvent {
    Job(Job),
    Script(Answered),
    /// A download the page started (stopped: Eddy downloads it), and the
    /// name the browser would have given it.
    Download(String, String),
    /// A page asking for a new window (not opened as one).
    NewWindow(String),
    Quit,
}

/// The helper's window, shown for checks, and its event loop.
struct Desktop {
    window: Window,
    proxy: EventLoopProxy<UserEvent>,
}

impl Host for Desktop {
    fn show(&mut self, _webview: &WebView, shown: bool) {
        self.window.set_visible(shown);
        if shown {
            self.window.set_focus();
        }
    }

    fn sender(&self) -> Box<dyn Fn(Answered) + Send> {
        let proxy = self.proxy.clone();
        Box::new(move |answered| {
            let _ = proxy.send_event(UserEvent::Script(answered));
        })
    }
}

fn read_jobs(proxy: EventLoopProxy<UserEvent>) {
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Some(job) = Job::parse(&line) else {
            continue;
        };
        if proxy.send_event(UserEvent::Job(job)).is_err() {
            return;
        }
    }
    let _ = proxy.send_event(UserEvent::Quit);
}

fn main() {
    let Some(data) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: eddy-webview <data folder>");
        std::process::exit(2);
    };
    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    // No Dock icon for reading pages unseen; the check window still shows.
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let window = match WindowBuilder::new()
        .with_title("Eddy")
        .with_visible(false)
        .with_inner_size(LogicalSize::new(480.0, 720.0))
        .build(&event_loop)
    {
        Ok(window) => window,
        Err(e) => {
            eprintln!("no window: {e}");
            std::process::exit(1);
        }
    };
    let proxy = event_loop.create_proxy();
    let mut context = WebContext::new(Some(data));
    let (downloads, windows) = (proxy.clone(), proxy.clone());
    let builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_download_started_handler(move |url, path| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let _ = downloads.send_event(UserEvent::Download(url, name));
            false
        })
        .with_new_window_req_handler(move |url, _features| {
            let _ = windows.send_event(UserEvent::NewWindow(url));
            NewWindowResponse::Deny
        });
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        match window.default_vbox() {
            Some(vbox) => builder.build_gtk(vbox),
            None => builder.build(&window),
        }
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window);
    let webview = match webview {
        Ok(webview) => webview,
        Err(e) => {
            eprintln!("no webview: {e}");
            std::process::exit(1);
        }
    };
    println!("ready");
    let _ = std::io::stdout().flush();

    let reader = proxy.clone();
    std::thread::spawn(move || read_jobs(reader));
    let mut jobs = Jobs::new(Desktop { window, proxy }, webview);
    event_loop.run(move |event, _, control_flow| {
        // Keeps the context alive as long as the webview.
        let _ = &context;
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                if jobs.busy() {
                    jobs.poll();
                } else if jobs.idle() {
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Job(job)) => jobs.add(job),
            Event::UserEvent(UserEvent::Script(answered)) => jobs.answered(answered),
            Event::UserEvent(UserEvent::Download(url, name)) => jobs.download_started(url, name),
            Event::UserEvent(UserEvent::NewWindow(url)) => jobs.new_window(url),
            Event::UserEvent(UserEvent::Quit) => *control_flow = ControlFlow::Exit,
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => jobs.closed(),
            _ => {}
        }
    });
}
