//! Eddy's own memory and CPU use, for the overlay Settings → Advanced turns
//! on: read from /proc/self once a second while it's on, nothing otherwise.
//! Only this process: the share popup, the WebView's renderer and yt-dlp
//! run in processes of their own.

use crate::{AppSettings, DownloaderApp};
use slint::ComponentHandle;
use std::time::{Duration, Instant};

/// How often the thread names are read again.
const NAMES_EVERY: Duration = Duration::from_secs(5);

/// Starts the once-a-second sampling (it reads nothing while the overlay is
/// off).
pub fn start(ui: &DownloaderApp) {
    let ui_weak = ui.as_weak();
    // The CPU time at the last sample, and when.
    let mut last: Option<(Duration, Instant)> = None;
    // Thread names, read every few seconds (dozens of files each time),
    // and when.
    let mut names: Option<(String, Instant)> = None;
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        let settings = ui.global::<AppSettings>();
        if !settings.get_show_usage() {
            last = None;
            if !settings.get_usage_text().is_empty() {
                settings.set_usage_text("".into());
            }
            return;
        }
        let now = Instant::now();
        let cpu = cpu_time();
        let percent = match (cpu, last) {
            (Some(cpu), Some((before, at))) => {
                let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
                let wall = now.duration_since(at).as_secs_f64();
                (wall > 0.0).then(|| (cpu.saturating_sub(before).as_secs_f64() / wall / cores * 100.0).clamp(0.0, 100.0))
            }
            _ => None,
        };
        last = cpu.map(|cpu| (cpu, now));
        let mut text = text(percent);
        if settings.get_usage_threads() {
            if names.as_ref().is_none_or(|(_, at)| at.elapsed() >= NAMES_EVERY) {
                names = Some((thread_names(), now));
            }
            if let Some((list, _)) = &names {
                text.push_str(list);
            }
        } else {
            names = None;
        }
        settings.set_usage_text(text.into());
    });
    std::mem::forget(timer);
}

/// "RAM 182 MB · PSS 140 MB · swap 0 B" over "CPU 4% · 38 threads".
fn text(cpu: Option<f64>) -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |text: &str, name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
    };
    let kb = |n: Option<u64>| n.map_or_else(|| "?".to_string(), |n| crate::links::format_size(n * 1024));
    // Shared memory split fairly between the processes using it: what
    // Android counts.
    let pss = field(&std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default(), "Pss:");
    let threads = field(&status, "Threads:").map_or_else(|| "?".to_string(), |n| n.to_string());
    let cpu = cpu.map_or_else(|| "…".to_string(), |p| format!("{p:.0}%"));
    format!(
        "RAM {} · PSS {} · swap {}\nCPU {cpu} · {threads} threads",
        kb(field(&status, "VmRSS:")),
        kb(pss),
        kb(field(&status, "VmSwap:")),
    )
}

/// This process's threads counted by name, most first, four to a line:
/// "\nbinder 15 · eddy-worker 6 · Chrome_ChildIOT 1 · …". Numbered names
/// ("binder:4321_2", "pool-3-thread-1") count as one.
fn thread_names() -> String {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entry in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
        let Ok(name) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        let name = name.trim().trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, '-' | '_' | ':' | '#' | ' '));
        let name = if name.is_empty() { "?" } else { name };
        *counts.entry(name.to_string()).or_default() += 1;
    }
    let mut counts: Vec<(String, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
        .chunks(4)
        .map(|line| line.iter().map(|(name, n)| format!("{name} {n}")).collect::<Vec<_>>().join(" · "))
        .fold(String::new(), |text, line| text + "\n" + &line)
}

/// The CPU time this process has used, user and system.
fn cpu_time() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // After the name (in brackets, which may hold spaces): state is the
    // first field, utime the 12th and stime the 13th.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    #[cfg(unix)]
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    #[cfg(not(unix))]
    let per_second = 100;
    Some(Duration::from_secs_f64(ticks as f64 / per_second as f64))
}
