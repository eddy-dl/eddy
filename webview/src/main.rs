//! eddy-webview: the desktop app's browser for sites behind bot checks, as
//! Android's WebView is for the phone (PageFetcher, CloudflareActivity).
//! Eddy starts it the first time a site needs it (platform::desktop) and
//! hands it one job a line on stdin, "<kind>\t<url>\t<result file>":
//!
//! - `fetch`: the page, read unseen. The result file's first line says how
//!   it went: "page" (the HTML follows), "check" (a bot check that wants the
//!   user) or "error <why>".
//! - `check`: the site's check, in a window the user passes it in. The
//!   result is the browser's user agent, its cookies for the site and
//!   "checked" (or "none" when it had a pass already), a line each; left
//!   empty when the window is closed first.
//!
//! One webview serves every job, with its cookies kept in the folder given
//! as the only argument. Once it has a page of a site open, the next pages
//! of that site are asked for from inside it (fetch(), with the browser's
//! cookies and connection), so only their HTML comes. It says "ready" on
//! stdout once the webview is up, and quits when stdin closes (Eddy went)
//! or after a while with nothing to do.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use wry::{WebContext, WebView, WebViewBuilder};

const POLL: Duration = Duration::from_millis(500);
/// A check that hasn't passed by itself in this long wants the user.
const CHECK_WAIT: Duration = Duration::from_secs(15);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// As long as Eddy waits for a check to be passed.
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
/// Quits once nothing's been asked for this long.
const IDLE: Duration = Duration::from_secs(120);

/// "loading" until the page is in, then "check" for a bot check
/// (Cloudflare's "Just a moment…", Anubis's) or "page". As Android's
/// CloudflareActivity.PAGE_STATE.
const PAGE_STATE: &str = "(function() { if (document.readyState == 'loading') return 'loading';\
    return typeof window._cf_chl_opt != 'undefined' || document.title.indexOf('Just a moment') >= 0\
    || !!document.getElementById('challenge-form') || !!document.getElementById('challenge-error-text')\
    || !!document.getElementById('anubis_challenge') ? 'check' : 'page'; })()";
/// Set on the page before a load: the new page doesn't have it.
const MARK_OLD: &str = "window.eddyOldPage = true";
/// Asks for a page from inside the one open (only on the same site), and
/// keeps the answer, or false for none or a bot check, in eddyFetch.
const FETCH: &str = "(function(u, o, n) { window.eddyFetch = {n: n};\
    var no = function() { window.eddyFetch = {n: n, ok: false}; };\
    if (location.origin != o) { no(); return; }\
    fetch(u, {credentials: 'include'}).then(function(r) { return r.text().then(function(t) {\
    var check = /id=\"anubis_challenge\"|_cf_chl_opt|id=\"challenge-form\"|<title>Just a moment/.test(t);\
    window.eddyFetch = {n: n, ok: r.ok && !check, text: t}; }); }).catch(no); })";
/// null: the page that asked has gone.
const FETCHED: &str = "(function(n) { var f = window.eddyFetch; if (!f || f.n !== n) return null;\
    if (!('ok' in f)) return 'wait'; return f.ok ? 'page\\n' + f.text : 'failed'; })";

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Fetch,
    Check,
}

struct Job {
    kind: Kind,
    url: String,
    result: PathBuf,
}

/// What a script's answer is for.
#[derive(Clone, Copy)]
enum Answer {
    Marked,
    State,
    Fetched,
    Html,
    UserAgent,
}

enum UserEvent {
    Job(Job),
    /// A script's answer (JSON), for the job `serial`.
    Script(u64, Answer, String),
    Quit,
}

/// The job being done.
struct Current {
    job: Job,
    began: Instant,
    /// When the check page was first seen.
    check_since: Option<Instant>,
    /// Asking from inside the page open (FETCH), and whether that failed.
    asking: bool,
    ask_failed: bool,
    loading: bool,
    /// A script's answer is awaited: no new poll till it comes.
    waiting: bool,
    saw_check: bool,
    cookies: String,
}

struct App {
    window: Window,
    webview: WebView,
    proxy: EventLoopProxy<UserEvent>,
    queue: VecDeque<Job>,
    current: Option<Current>,
    serial: u64,
    last_active: Instant,
}

fn origin(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}", rest.split(['/', '?', '#']).next().unwrap_or("")),
        None => String::new(),
    }
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

impl App {
    fn eval(&mut self, js: &str, answer: Answer) {
        let (proxy, serial) = (self.proxy.clone(), self.serial);
        if let Some(current) = self.current.as_mut() {
            current.waiting = true;
        }
        let sent = self.webview.evaluate_script_with_callback(js, move |json| {
            let _ = proxy.send_event(UserEvent::Script(serial, answer, json));
        });
        if let Err(e) = sent {
            eprintln!("script didn't run: {e}");
            if let Some(current) = self.current.as_mut() {
                current.waiting = false;
            }
        }
    }

    fn start_next(&mut self) {
        if self.current.is_some() {
            return;
        }
        let Some(job) = self.queue.pop_front() else {
            return;
        };
        self.serial += 1;
        let (kind, same_site) = (job.kind, self.webview.url().is_ok_and(|u| origin(&u) == origin(&job.url)));
        self.current = Some(Current {
            job,
            began: Instant::now(),
            check_since: None,
            asking: false,
            ask_failed: false,
            loading: false,
            waiting: false,
            saw_check: false,
            cookies: String::new(),
        });
        match kind {
            Kind::Check => {
                self.window.set_visible(true);
                self.window.set_focus();
                self.load();
            }
            // The page open may be on the same site: asked from it first.
            Kind::Fetch if same_site => self.ask(),
            Kind::Fetch => self.load(),
        }
    }

    fn load(&mut self) {
        let Some(current) = self.current.as_mut() else {
            return;
        };
        current.loading = true;
        let blank = self.webview.url().map_or(true, |u| u.is_empty() || u == "about:blank");
        if blank {
            let url = current.job.url.clone();
            let _ = self.webview.load_url(&url);
        } else {
            self.eval(MARK_OLD, Answer::Marked);
        }
    }

    fn ask(&mut self) {
        let Some(current) = self.current.as_mut() else {
            return;
        };
        current.asking = true;
        let js = format!("{FETCH}({}, {}, {})", quoted(&current.job.url), quoted(&origin(&current.job.url)), self.serial);
        let _ = self.webview.evaluate_script(&js);
    }

    fn poll(&mut self) {
        let Some(current) = self.current.as_ref() else {
            return;
        };
        if current.waiting {
            return;
        }
        let elapsed = current.began.elapsed();
        match current.job.kind {
            Kind::Fetch if elapsed > FETCH_TIMEOUT => {
                self.finish(&format!("error it didn't load in {} s", FETCH_TIMEOUT.as_secs()));
            }
            Kind::Check if elapsed > CHECK_TIMEOUT => self.finish(""),
            Kind::Fetch if current.asking => self.eval(&format!("{FETCHED}({})", self.serial), Answer::Fetched),
            Kind::Fetch => {
                // 'here': the new page is on the site, still loading.
                let js = format!(
                    "(function(o) {{ if (window.eddyOldPage || location.href == 'about:blank') return 'old';\
                     var s = {PAGE_STATE}; return s == 'loading' && location.origin == o ? 'here' : s; }})({})",
                    quoted(&origin(&current.job.url))
                );
                self.eval(&js, Answer::State);
            }
            Kind::Check => {
                let js = format!("(window.eddyOldPage ? 'old' : {PAGE_STATE})");
                self.eval(&js, Answer::State);
            }
        }
    }

    fn answered(&mut self, serial: u64, answer: Answer, json: &str) {
        if serial != self.serial {
            return;
        }
        let Some(current) = self.current.as_mut() else {
            return;
        };
        current.waiting = false;
        let value = serde_json::from_str::<Option<String>>(json).ok().flatten();
        match (current.job.kind, answer, value.as_deref()) {
            (_, Answer::Marked, _) => {
                let url = current.job.url.clone();
                let _ = self.webview.load_url(&url);
            }
            (Kind::Fetch, Answer::Fetched, None) => current.asking = false,
            (Kind::Fetch, Answer::Fetched, Some("wait")) => {}
            (Kind::Fetch, Answer::Fetched, Some(text)) if text.starts_with("page\n") => {
                let text = text.to_string();
                self.finish(&text);
            }
            (Kind::Fetch, Answer::Fetched, Some(_)) => {
                current.asking = false;
                current.ask_failed = true;
                if !current.loading {
                    self.load();
                }
            }
            (Kind::Fetch, Answer::State, Some("page")) => self.eval("document.documentElement.outerHTML", Answer::Html),
            // On the site but still loading (ads, slow scripts): its HTML is
            // asked for now rather than after the rest.
            (Kind::Fetch, Answer::State, Some("here")) => {
                if !current.asking && !current.ask_failed {
                    self.ask();
                }
            }
            (Kind::Fetch, Answer::State, Some("check")) => {
                current.asking = false;
                if current.check_since.get_or_insert_with(Instant::now).elapsed() > CHECK_WAIT {
                    self.finish("check");
                }
            }
            (Kind::Fetch, Answer::Html, Some(html)) if !html.is_empty() => {
                let text = format!("page\n{html}");
                self.finish(&text);
            }
            (Kind::Fetch, Answer::Html, _) => self.finish("error its HTML didn't come out"),
            (Kind::Check, Answer::State, Some("check")) => current.saw_check = true,
            // Passed: the site's own page is in, with the cookies the check
            // left (cf_clearance, Anubis's pass).
            (Kind::Check, Answer::State, Some("page")) => {
                let cookies = self.webview.cookies_for_url(&current.job.url).unwrap_or_default();
                let cookies = cookies.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; ");
                if !cookies.is_empty() {
                    current.cookies = cookies;
                    self.eval("navigator.userAgent", Answer::UserAgent);
                }
            }
            (Kind::Check, Answer::UserAgent, Some(user_agent)) => {
                let text = format!("{user_agent}\n{}\n{}", current.cookies, if current.saw_check { "checked" } else { "none" });
                self.finish(&text);
            }
            _ => {}
        }
    }

    /// Hands the job's result over and goes on with the next.
    fn finish(&mut self, text: &str) {
        let Some(current) = self.current.take() else {
            return;
        };
        // Written whole, then moved into place: Eddy never reads half of it.
        let file = &current.job.result;
        let part = PathBuf::from(format!("{}.part", file.display()));
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&part, text).and_then(|_| std::fs::rename(&part, file)) {
            eprintln!("couldn't hand {} over: {e}", current.job.url);
        }
        if current.job.kind == Kind::Check {
            self.window.set_visible(false);
        }
        self.last_active = Instant::now();
        self.start_next();
    }
}

fn read_jobs(proxy: EventLoopProxy<UserEvent>) {
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let mut parts = line.splitn(3, '\t');
        let (Some(kind), Some(url), Some(result)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let kind = match kind {
            "fetch" => Kind::Fetch,
            "check" => Kind::Check,
            _ => continue,
        };
        let job = Job { kind, url: url.to_string(), result: PathBuf::from(result) };
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
    let mut context = WebContext::new(Some(data));
    let builder = WebViewBuilder::new_with_web_context(&mut context);
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

    let proxy = event_loop.create_proxy();
    let reader = proxy.clone();
    std::thread::spawn(move || read_jobs(reader));
    let mut app = App {
        window,
        webview,
        proxy,
        queue: VecDeque::new(),
        current: None,
        serial: 0,
        last_active: Instant::now(),
    };
    event_loop.run(move |event, _, control_flow| {
        // Keeps the context alive as long as the webview.
        let _ = &context;
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                if app.current.is_some() {
                    app.poll();
                } else if app.last_active.elapsed() > IDLE {
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Job(job)) => {
                app.queue.push_back(job);
                app.last_active = Instant::now();
                app.start_next();
            }
            Event::UserEvent(UserEvent::Script(serial, answer, json)) => app.answered(serial, answer, &json),
            Event::UserEvent(UserEvent::Quit) => *control_flow = ControlFlow::Exit,
            // Closed before the check passed: not passed (the file's left
            // empty). The helper itself stays for the next job.
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                if app.current.as_ref().is_some_and(|c| c.job.kind == Kind::Check) {
                    app.finish("");
                } else {
                    app.window.set_visible(false);
                }
            }
            _ => {}
        }
    });
}
