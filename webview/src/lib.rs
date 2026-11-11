//! The browser's jobs for sites behind bot checks, shared by the desktop
//! helper (main.rs, a window of its own on tao's event loop) and Eddy's iOS
//! app (platform::ios_webview, a webview in Eddy's own window, on Slint's).
//! A job is one of:
//!
//! - `fetch`: the page, read unseen. The result file's first line says how
//!   it went: "page" (the HTML follows), "check" (a bot check that wants the
//!   user) or "error <why>".
//! - `check`: the site's check, shown for the user to pass. The result is
//!   the browser's user agent, its cookies for the site and "checked" (or
//!   "none" when it had a pass already), a line each; left empty when the
//!   browser is closed first.
//! - `browse`: a page whose file only comes from a click (a countdown, a
//!   captcha), shown for the user to click through. The download it starts
//!   isn't saved: it's written to the result as JSON (the file's link, the
//!   browser's user agent and cookies, the page it was on, its name), as
//!   Android's BrowserActivity does; left empty when the browser is closed
//!   first. A page it opens in a new window, on the same site, opens in its
//!   place (elsewhere, it's mostly ads, and isn't opened).
//!
//! One webview serves every job. Once it has a page of a site open, the next
//! pages of that site are asked for from inside it (fetch(), with the
//! browser's cookies and connection), so only their HTML comes.
//!
//! What differs is left to a [`Host`]: showing and hiding the browser, and
//! handing scripts' answers back on the browser's thread. The host's event
//! loop calls [`Jobs::poll`] every [`POLL`] while there's a job.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wry::WebView;

pub use wry;

/// How often the job in hand is looked at.
pub const POLL: Duration = Duration::from_millis(500);
/// A check that hasn't passed by itself in this long wants the user.
const CHECK_WAIT: Duration = Duration::from_secs(15);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// As long as Eddy waits for a check to be passed.
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
/// As long as Eddy waits for a download to be clicked (crate::browser's).
const BROWSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The desktop helper quits once nothing's been asked for this long.
pub const IDLE: Duration = Duration::from_secs(120);

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
pub enum Kind {
    Fetch,
    Check,
    Browse,
}

pub struct Job {
    pub kind: Kind,
    pub url: String,
    pub result: PathBuf,
}

impl Job {
    /// "fetch", "check" or "browse", as Eddy names them.
    pub fn new(kind: &str, url: &str, result: &str) -> Option<Job> {
        let kind = match kind {
            "fetch" => Kind::Fetch,
            "check" => Kind::Check,
            "browse" => Kind::Browse,
            _ => return None,
        };
        Some(Job { kind, url: url.to_string(), result: PathBuf::from(result) })
    }

    /// A job as the desktop helper gets it, "<kind>\t<url>\t<result file>".
    pub fn parse(line: &str) -> Option<Job> {
        let mut parts = line.splitn(3, '\t');
        Job::new(parts.next()?, parts.next()?, parts.next()?)
    }
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

/// A script's answer (JSON), for the job it was asked for: handed back to
/// [`Jobs::answered`] by the host.
pub struct Answered {
    serial: u64,
    answer: Answer,
    json: String,
}

/// What the browser's home does its own way.
pub trait Host {
    /// Shows the browser for the user to pass a check in, or hides it.
    fn show(&mut self, webview: &WebView, shown: bool);
    /// Something that hands a script's answer to [`Jobs::answered`], later
    /// and on the browser's thread (it's called from wherever the webview
    /// answers).
    fn sender(&self) -> Box<dyn Fn(Answered) + Send>;
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
    /// A browse job's download: its link, name and the page it was on.
    caught: Option<(String, String, String)>,
}

/// The jobs asked for, done one at a time in one webview.
pub struct Jobs<H: Host> {
    host: H,
    webview: WebView,
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

/// The site's name, without "www." (a site's pages and its files' host
/// often differ only by that).
fn site(url: &str) -> String {
    origin(url).split_once("://").map(|(_, host)| host.trim_start_matches("www.").to_string()).unwrap_or_default()
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

impl<H: Host> Jobs<H> {
    pub fn new(host: H, webview: WebView) -> Self {
        Jobs { host, webview, queue: VecDeque::new(), current: None, serial: 0, last_active: Instant::now() }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn webview(&self) -> &WebView {
        &self.webview
    }

    pub fn add(&mut self, job: Job) {
        self.queue.push_back(job);
        self.last_active = Instant::now();
        self.start_next();
    }

    /// A job's being done (the host polls meanwhile).
    pub fn busy(&self) -> bool {
        self.current.is_some()
    }

    /// Nothing's been asked for in [`IDLE`].
    pub fn idle(&self) -> bool {
        self.current.is_none() && self.last_active.elapsed() > IDLE
    }

    /// The browser closed by the user: a check in it isn't passed, nor a
    /// download clicked (its file is left empty). The browser itself stays
    /// for the next job.
    pub fn closed(&mut self) {
        if self.current.as_ref().is_some_and(|c| c.job.kind != Kind::Fetch) {
            self.finish("");
        } else {
            self.host.show(&self.webview, false);
        }
    }

    /// A download the page started (the host's webview handler, which
    /// stops it): a browse job's file, with the browser's cookies for it.
    /// `name`: what the browser would have called it, if it knows.
    pub fn download_started(&mut self, url: String, name: String) {
        let page = self.webview.url().unwrap_or_default();
        let Some(current) = self.current.as_mut().filter(|c| c.job.kind == Kind::Browse && c.caught.is_none()) else {
            eprintln!("a download nobody asked for: {url}");
            return;
        };
        let cookies = self.webview.cookies_for_url(&url).unwrap_or_default();
        current.cookies = cookies.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; ");
        current.caught = Some((url, name, page));
        self.eval("navigator.userAgent", Answer::UserAgent);
    }

    /// A page asked for a new window (the host's webview handler, which
    /// doesn't open it): in a browse job, on the page's site, it opens in
    /// the page's place, where its download can be caught.
    pub fn new_window(&mut self, url: String) {
        let browsing = self.current.as_ref().is_some_and(|c| c.job.kind == Kind::Browse);
        let here = self.webview.url().unwrap_or_default();
        if browsing && site(&url) == site(&here) {
            let _ = self.webview.load_url(&url);
        } else {
            eprintln!("a new window not opened: {url}");
        }
    }

    fn eval(&mut self, js: &str, answer: Answer) {
        let (send, serial) = (self.host.sender(), self.serial);
        if let Some(current) = self.current.as_mut() {
            current.waiting = true;
        }
        let sent = self.webview.evaluate_script_with_callback(js, move |json| send(Answered { serial, answer, json }));
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
            caught: None,
        });
        match kind {
            Kind::Check | Kind::Browse => {
                self.host.show(&self.webview, true);
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

    /// Looks at how the job in hand is going.
    pub fn poll(&mut self) {
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
            Kind::Browse if elapsed > BROWSE_TIMEOUT => self.finish(""),
            // The user clicks; the download comes to download_started.
            Kind::Browse => {}
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

    pub fn answered(&mut self, answered: Answered) {
        let Answered { serial, answer, json } = answered;
        if serial != self.serial {
            return;
        }
        let Some(current) = self.current.as_mut() else {
            return;
        };
        current.waiting = false;
        let value = serde_json::from_str::<Option<String>>(&json).ok().flatten();
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
            (Kind::Browse, Answer::UserAgent, Some(user_agent)) => {
                let Some((url, name, page)) = current.caught.clone() else {
                    return;
                };
                let filename = if name.is_empty() { name_from_url(&url) } else { name };
                let text = serde_json::json!({
                    "url": url,
                    "user_agent": user_agent,
                    "cookies": current.cookies,
                    "referer": page,
                    "filename": filename,
                })
                .to_string();
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
        write_result(&current.job.result, text);
        if current.job.kind != Kind::Fetch {
            self.host.show(&self.webview, false);
        }
        self.last_active = Instant::now();
        self.start_next();
    }
}

/// A file's name from its link's path ("" for none).
fn name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    if path.matches('/').count() < 3 || last.is_empty() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let mut rest = last.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        match (b, tail) {
            (b'%', [h, l, more @ ..]) if h.is_ascii_hexdigit() && l.is_ascii_hexdigit() => {
                let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
                bytes.push(hex(*h) << 4 | hex(*l));
                rest = more;
            }
            _ => {
                bytes.push(b);
                rest = tail;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Written whole, then moved into place: Eddy never reads half of it.
pub fn write_result(file: &std::path::Path, text: &str) {
    let part = PathBuf::from(format!("{}.part", file.display()));
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&part, text).and_then(|_| std::fs::rename(&part, file)) {
        eprintln!("couldn't write {}: {e}", file.display());
    }
}
