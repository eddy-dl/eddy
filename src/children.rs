//! Programs Eddy starts: yt-dlp, and the ffmpeg it starts in turn. Each runs
//! in a process group of its own, so stopping one stops what it started too
//! (killing yt-dlp alone left its ffmpeg merging or converting). Link checks
//! are listed here so they can be stopped once nobody's waiting for them: the
//! preview dismissed, the app or the share popup closed. iOS starts no
//! processes: there yt-dlp and gallery-dl run in the app's own Python
//! (platform::python), behind the same `Running`.

use std::io::Read;
use std::process::{Child, Command};
use std::sync::Mutex;

/// The programs Eddy reads output from: the Python ones, and ffmpeg.
#[derive(Clone, Copy, Debug)]
pub enum Tool {
    Ytdlp,
    GalleryDl,
    Ffmpeg,
}

impl Tool {
    #[cfg(not(target_os = "ios"))]
    fn name(self) -> &'static str {
        match self {
            Tool::Ytdlp => "yt-dlp",
            Tool::GalleryDl => "gallery-dl",
            Tool::Ffmpeg => "ffmpeg",
        }
    }
}

/// Whether `tool` can run here.
pub fn available(tool: Tool) -> bool {
    #[cfg(target_os = "ios")]
    {
        match tool {
            Tool::Ffmpeg => crate::platform::python::has_ffmpeg(),
            _ => crate::platform::python::has(module(tool)),
        }
    }
    #[cfg(not(target_os = "ios"))]
    {
        match tool {
            Tool::Ytdlp => crate::platform::ytdlp_command().is_some(),
            Tool::GalleryDl => crate::gallerydl::command().is_some(),
            Tool::Ffmpeg => crate::platform::ffmpeg_command().is_some(),
        }
    }
}

#[cfg(target_os = "ios")]
fn module(tool: Tool) -> &'static str {
    match tool {
        Tool::Ytdlp => "yt_dlp",
        Tool::GalleryDl => "gallery_dl",
        // The FFmpeg linked into the app, run by the same Python.
        Tool::Ffmpeg => "ffmpeg",
    }
}

/// Starts `tool` with `args`, its stdout and stderr to read: a process in a
/// group of its own, or on iOS a run in the app's Python.
pub fn spawn_tool(tool: Tool, args: &[String]) -> std::io::Result<Running> {
    #[cfg(target_os = "ios")]
    {
        crate::platform::python::run(module(tool), args)
    }
    #[cfg(not(target_os = "ios"))]
    {
        let command = match tool {
            Tool::Ytdlp => crate::platform::ytdlp_command(),
            Tool::GalleryDl => crate::gallerydl::command(),
            Tool::Ffmpeg => crate::platform::ffmpeg_command(),
        };
        let mut command = command.ok_or_else(|| std::io::Error::other(format!("{} isn't available", tool.name())))?;
        use std::process::Stdio;
        own_group(&mut command).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map(Running::process)
    }
}

/// A program running for Eddy, as `Child` has it: its output to read (taken
/// while it runs), waited on or stopped with whatever it started.
pub struct Running {
    pub stdout: Option<Box<dyn Read + Send>>,
    pub stderr: Option<Box<dyn Read + Send>>,
    inner: Inner,
}

enum Inner {
    Process(Child),
    #[cfg(target_os = "ios")]
    Python(crate::platform::python::Run),
}

/// How a program ended, and what it printed on stderr (unless that was
/// taken to read as it came).
pub struct Ended {
    pub success: bool,
    pub stderr: Vec<u8>,
}

impl Running {
    /// A child started with `own_group` (its piped output taken over).
    pub fn process(mut child: Child) -> Self {
        let stdout = child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>);
        let stderr = child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>);
        Running { stdout, stderr, inner: Inner::Process(child) }
    }

    #[cfg(target_os = "ios")]
    pub(crate) fn python(run: crate::platform::python::Run, stdout: std::fs::File, stderr: std::fs::File) -> Self {
        Running { stdout: Some(Box::new(stdout)), stderr: Some(Box::new(stderr)), inner: Inner::Python(run) }
    }

    /// What `checking` and `stop_checks` know it by.
    pub fn id(&self) -> u32 {
        match &self.inner {
            Inner::Process(child) => child.id(),
            #[cfg(target_os = "ios")]
            Inner::Python(run) => run.id(),
        }
    }

    /// Whether it succeeded, if it has ended.
    pub fn try_wait(&mut self) -> std::io::Result<Option<bool>> {
        match &mut self.inner {
            Inner::Process(child) => child.try_wait().map(|s| s.map(|s| s.success())),
            #[cfg(target_os = "ios")]
            Inner::Python(run) => Ok(run.try_wait()),
        }
    }

    /// Waits for it to end: whether it succeeded.
    pub fn wait(&mut self) -> std::io::Result<bool> {
        match &mut self.inner {
            Inner::Process(child) => child.wait().map(|s| s.success()),
            #[cfg(target_os = "ios")]
            Inner::Python(run) => Ok(run.wait()),
        }
    }

    /// Reads the rest of its stderr (if not taken) and waits for it.
    pub fn wait_with_output(mut self) -> std::io::Result<Ended> {
        let mut stderr = Vec::new();
        if let Some(mut err) = self.stderr.take() {
            let _ = err.read_to_end(&mut stderr);
        }
        let success = self.wait()?;
        Ok(Ended { success, stderr })
    }

    /// Stops it, and whatever it started.
    pub fn kill_tree(&mut self) {
        match &mut self.inner {
            Inner::Process(child) => kill_tree(child),
            #[cfg(target_os = "ios")]
            Inner::Python(run) => run.stop(),
        }
    }
}

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
    // A run in the app's Python (iOS has no processes).
    #[cfg(target_os = "ios")]
    crate::platform::python::stop(pid);
    #[cfg(all(unix, not(target_os = "ios")))]
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
