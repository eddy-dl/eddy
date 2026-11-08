//! Eddy's own memory and CPU use, for the overlay Settings → Advanced turns
//! on: read from /proc/self once a second while it's on, nothing otherwise.
//! Only this process: the share popup, the WebView's renderer and yt-dlp
//! run in processes of their own.

use crate::{AppSettings, DownloaderApp};
use slint::ComponentHandle;
use std::time::{Duration, Instant};

/// How often the threads are read again.
const NAMES_EVERY: Duration = Duration::from_secs(5);

/// Each thread's name and CPU time (clock ticks), by thread id.
type Threads = std::collections::HashMap<u32, (String, u64)>;

/// Starts the once-a-second sampling (it reads nothing while the overlay is
/// off).
pub fn start(ui: &DownloaderApp) {
    let ui_weak = ui.as_weak();
    // The CPU time at the last sample, and when.
    let mut last: Option<(Duration, Instant)> = None;
    // The threads' lines, read every few seconds (dozens of files each
    // time), their CPU times then (for the next sample), and when.
    let mut names: Option<(String, Threads, Instant)> = None;
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
            if names.as_ref().is_none_or(|(.., at)| at.elapsed() >= NAMES_EVERY) {
                let threads = read_threads();
                let mut list = thread_names(&threads);
                if let Some((_, before, at)) = &names {
                    list.push_str(&busiest(before, &threads, now.duration_since(*at)));
                }
                names = Some((list, threads, now));
            }
            if let Some((list, ..)) = &names {
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

/// This process's threads: each one's name (numbered ones, "binder:4321_2"
/// or "pool-3-thread-1", as one) and CPU time, from /proc/self/task.
fn read_threads() -> Threads {
    let mut threads = Threads::new();
    for entry in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
        let Some(id) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "<id> (<name>) <state> …": utime and stime are the 12th and 13th
        // fields after the name (which may hold spaces and brackets).
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let name = stat[open + 1..close].trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, '-' | '_' | ':' | '#' | ' '));
        let name = if name.is_empty() { "?" } else { name };
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let ticks = fields.get(11).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0)
            + fields.get(12).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
        threads.insert(id, (name.to_string(), ticks));
    }
    threads
}

/// The threads counted by name, most first, four to a line:
/// "\nbinder 15 · eddy-worker 6 · Chrome_ChildIOT 1 · …".
fn thread_names(threads: &Threads) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (name, _) in threads.values() {
        *counts.entry(name.as_str()).or_default() += 1;
    }
    let mut counts: Vec<(&str, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
        .chunks(4)
        .map(|line| line.iter().map(|(name, n)| format!("{name} {n}")).collect::<Vec<_>>().join(" · "))
        .fold(String::new(), |text, line| text + "\n" + &line)
}

/// The threads (by name) that used the most CPU since `before`, `wall` ago,
/// as shares of the whole phone like the CPU line: "\nbusiest: main 6% ·
/// eddy-worker 2% · …". Threads that ended since aren't counted.
fn busiest(before: &Threads, now: &Threads, wall: Duration) -> String {
    #[cfg(unix)]
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    #[cfg(not(unix))]
    let per_second = 100.0;
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let mut used: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for (id, (name, ticks)) in now {
        // A thread new since: all its time is since.
        let earlier = before.get(id).map_or(0, |(_, t)| *t);
        *used.entry(name.as_str()).or_default() += ticks.saturating_sub(earlier);
    }
    let mut used: Vec<(&str, f64)> = used
        .into_iter()
        .map(|(name, ticks)| (name, ticks as f64 / per_second / wall.as_secs_f64().max(0.001) / cores * 100.0))
        .filter(|(_, percent)| *percent >= 0.5)
        .collect();
    used.sort_by(|a, b| b.1.total_cmp(&a.1));
    if used.is_empty() {
        return "\nbusiest: none".to_string();
    }
    let top: Vec<String> = used.iter().take(4).map(|(name, percent)| format!("{name} {percent:.0}%")).collect();
    format!("\nbusiest: {}", top.join(" · "))
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
