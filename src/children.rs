//! Programs Eddy starts: yt-dlp, and the ffmpeg it starts in turn. Each runs
//! in a process group of its own, so stopping one stops what it started too
//! (killing yt-dlp alone left its ffmpeg merging or converting). Link checks
//! are listed here so they can be stopped once nobody's waiting for them: the
//! preview dismissed, the app or the share popup closed.

use std::process::{Child, Command};
use std::sync::Mutex;

/// Has `command` start in a new process group (Unix).
pub fn own_group(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

/// Kills a child started with `own_group`, and whatever it started.
pub fn kill_tree(child: &mut Child) {
    kill_group(child.id());
    let _ = child.kill();
}

fn kill_group(pid: u32) {
    #[cfg(unix)]
    // SAFETY: kill(2) on the group whose leader is our own child; a negative
    // pid names the group.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// The link checks (yt-dlp reading a link's media) running now.
static CHECKS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// A link check started (its process, from `own_group`).
pub fn checking(pid: u32) {
    CHECKS.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
}

/// A link check ended.
pub fn checked(pid: u32) {
    CHECKS.lock().unwrap_or_else(|e| e.into_inner()).retain(|&p| p != pid);
}

/// Stops every link check still running.
pub fn stop_checks() {
    let pids: Vec<u32> = std::mem::take(&mut *CHECKS.lock().unwrap_or_else(|e| e.into_inner()));
    if !pids.is_empty() {
        log::info!("stopping {} link check(s)", pids.len());
    }
    for pid in pids {
        kill_group(pid);
    }
}
