//! The app's own Python (iOS): yt-dlp and gallery-dl run in it, as iOS
//! starts no processes. Python.framework (BeeWare's Python-Apple-support)
//! is embedded in the app with its standard library in `python/` and the
//! packages in `app_packages/` (platforms/ios); it's loaded when first
//! needed, so cargo doesn't need it to build.
//!
//! Each run is a thread in the one interpreter, its stdout and stderr going
//! to pipes read as a process's would be (children::Running). sys.stdout is
//! shared, so the bootstrap below sends each thread's writes to its own run,
//! and threads a run starts (yt-dlp's fragment downloads) inherit that.
//! Stopping a run raises KeyboardInterrupt in its thread, which yt-dlp takes
//! as "Interrupted by user".
//!
//! FFmpeg (tfourj/SwiftFFmpeg-iOS, linked in when the build has it:
//! `eddy_ffmpeg`, see build.rs) is ffmpeg and ffprobe as functions: the
//! bootstrap gives yt-dlp a Popen that calls them for those two programs.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int, c_ulong, c_void};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// The C API Eddy uses, from the framework.
// SwiftFFmpeg-iOS's wrapper: a run with its stdout and stderr captured, and
// cancelling the one running.
#[cfg(eddy_ffmpeg)]
unsafe extern "C" {
    fn ffmpeg_execute_with_output(argc: c_int, argv: *mut *mut c_char, out: *mut c_char, out_size: usize, err: *mut c_char, err_size: usize) -> c_int;
    fn ffprobe_execute_with_output(argc: c_int, argv: *mut *mut c_char, out: *mut c_char, out_size: usize, err: *mut c_char, err_size: usize) -> c_int;
    fn ffmpeg_request_cancel();
    fn ffmpeg_clear_cancel();
}

struct Api {
    gil_ensure: unsafe extern "C" fn() -> c_int,
    gil_release: unsafe extern "C" fn(c_int),
    run_string: unsafe extern "C" fn(*const c_char) -> c_int,
    set_async_exc: unsafe extern "C" fn(c_ulong, *mut c_void) -> c_int,
    keyboard_interrupt: *mut c_void,
}

// SAFETY: the pointers are the framework's functions and a type object that
// lives as long as the interpreter (the whole app); calls take the GIL.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// The app bundle (Eddy.app), where the executable is.
fn bundle() -> PathBuf {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(PathBuf::from)).unwrap_or_default()
}

/// Whether `module` (yt_dlp, gallery_dl) is in this build.
pub fn has(module: &str) -> bool {
    let bundle = bundle();
    bundle.join("Frameworks/Python.framework/Python").is_file() && bundle.join("app_packages").join(module).is_dir()
}

/// Sets up Python once: the bundle's home and search path, as Python's iOS
/// guide has them, then the bootstrap.
fn init() -> Result<Api, String> {
    let bundle = bundle();
    let library = bundle.join("Frameworks/Python.framework/Python");
    let home = bundle.join("python");
    // python/lib/python3.X
    let stdlib = std::fs::read_dir(home.join("lib"))
        .map_err(|e| format!("Python's library isn't in this build: {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("python3")))
        .ok_or("Python's library isn't in this build")?;
    let path = [stdlib.clone(), stdlib.join("lib-dynload"), bundle.join("app_packages")]
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    // SAFETY: set before Python starts, which reads them; nothing else in
    // Eddy reads these.
    unsafe {
        std::env::set_var("PYTHONHOME", &home);
        std::env::set_var("PYTHONPATH", path);
        std::env::set_var("PYTHONUTF8", "1");
        std::env::set_var("PYTHONUNBUFFERED", "1");
        std::env::set_var("PYTHONDONTWRITEBYTECODE", "1");
    }
    let name = CString::new(library.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: dlopen/dlsym on the embedded framework; each symbol is cast to
    // its C API signature.
    unsafe {
        let handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        if handle.is_null() {
            return Err(format!("couldn't load Python: {}", std::ffi::CStr::from_ptr(libc::dlerror()).to_string_lossy()));
        }
        let symbol = |name: &str| -> Result<*mut c_void, String> {
            let c = CString::new(name).unwrap();
            let found = libc::dlsym(handle, c.as_ptr());
            if found.is_null() { Err(format!("Python has no {name}")) } else { Ok(found) }
        };
        let initialize: unsafe extern "C" fn(c_int) = std::mem::transmute(symbol("Py_InitializeEx")?);
        let save_thread: unsafe extern "C" fn() -> *mut c_void = std::mem::transmute(symbol("PyEval_SaveThread")?);
        let api = Api {
            gil_ensure: std::mem::transmute(symbol("PyGILState_Ensure")?),
            gil_release: std::mem::transmute(symbol("PyGILState_Release")?),
            run_string: std::mem::transmute(symbol("PyRun_SimpleString")?),
            set_async_exc: std::mem::transmute(symbol("PyThreadState_SetAsyncExc")?),
            keyboard_interrupt: *(symbol("PyExc_KeyboardInterrupt")? as *const *mut c_void),
        };
        // No signal handlers: they're the app's.
        initialize(0);
        let bootstrap = CString::new(BOOTSTRAP).unwrap();
        if (api.run_string)(bootstrap.as_ptr()) != 0 {
            return Err("Python's bootstrap failed".to_string());
        }
        #[cfg(eddy_ffmpeg)]
        {
            // yt-dlp looks for the programs before running them: empty
            // stand-ins, in a folder it's told about (--ffmpeg-location).
            let stand_ins = PathBuf::from(super::get_app_data_dir()).join("ffmpeg");
            let _ = std::fs::create_dir_all(&stand_ins);
            for program in ["ffmpeg", "ffprobe"] {
                let _ = std::fs::write(stand_ins.join(program), "");
            }
            let setup = format!(
                "__eddy_ffmpeg_setup({}, {}, {}, {}, {:?})",
                ffmpeg_execute_with_output as usize,
                ffprobe_execute_with_output as usize,
                ffmpeg_request_cancel as usize,
                ffmpeg_clear_cancel as usize,
                stand_ins.to_string_lossy()
            );
            let setup = CString::new(setup).unwrap();
            if (api.run_string)(setup.as_ptr()) != 0 {
                log::warn!(target: "ytdlp", "FFmpeg didn't set up; yt-dlp runs without it");
            }
        }
        // The GIL goes to whichever run needs it.
        save_thread();
        log::info!(target: "ytdlp", "Python started from {}", stdlib.display());
        Ok(api)
    }
}

fn api() -> std::io::Result<&'static Api> {
    API.get_or_init(init).as_ref().map_err(|e| std::io::Error::other(e.clone()))
}

/// What a run's status pipe has said: its thread, and how it ended.
#[derive(Default)]
struct State {
    thread: Option<c_ulong>,
    exit: Option<i32>,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// A run in progress (children::Running's iOS half).
pub struct Run {
    id: u32,
    shared: Arc<Shared>,
}

static NEXT: AtomicU32 = AtomicU32::new(1);
/// Runs by id, for `stop` (children::stop_checks knows them by id).
static RUNS: Mutex<Option<HashMap<u32, Arc<Shared>>>> = Mutex::new(None);

fn pipe() -> std::io::Result<(c_int, c_int)> {
    let mut fds = [0 as c_int; 2];
    // SAFETY: pipe(2) with room for its two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

/// Starts `module` (yt_dlp, gallery_dl) with `args`, as its command line
/// would.
pub fn run(module: &str, args: &[String]) -> std::io::Result<crate::children::Running> {
    let api = api()?;
    let (out_r, out_w) = pipe()?;
    let (err_r, err_w) = pipe()?;
    let (status_r, status_w) = pipe()?;
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    let shared = Arc::new(Shared { state: Mutex::new(State::default()), changed: Condvar::new() });
    RUNS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(id, Arc::clone(&shared));
    // A JSON list of strings is a Python list literal too.
    let call = format!(
        "__eddy_run({out_w}, {err_w}, {status_w}, {module:?}, {})",
        serde_json::to_string(args).map_err(std::io::Error::other)?
    );
    let call = CString::new(call).map_err(std::io::Error::other)?;
    // The write ends are Python's from here (it closes them).
    std::thread::spawn(move || {
        // SAFETY: the API from init; the GIL is held for the call.
        unsafe {
            let gil = (api.gil_ensure)();
            (api.run_string)(call.as_ptr());
            (api.gil_release)(gil);
        }
    });
    // The status pipe: the run's thread, then how it ended.
    // SAFETY: our own read end, owned by this File from here.
    let status = unsafe { File::from_raw_fd(status_r) };
    let watched = Arc::clone(&shared);
    std::thread::spawn(move || {
        for line in BufReader::new(status).lines().map_while(Result::ok) {
            let mut state = watched.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(code) = line.strip_prefix("exit ") {
                state.exit = Some(code.trim().parse().unwrap_or(1));
            } else if let Ok(thread) = line.trim().parse() {
                state.thread = Some(thread);
                // Stopped before it had started.
                if state.stop {
                    interrupt(thread);
                }
            }
            watched.changed.notify_all();
        }
        let mut state = watched.state.lock().unwrap_or_else(|e| e.into_inner());
        state.exit.get_or_insert(1);
        watched.changed.notify_all();
    });
    // SAFETY: our own read ends, owned by these Files from here.
    let (stdout, stderr) = unsafe { (File::from_raw_fd(out_r), File::from_raw_fd(err_r)) };
    Ok(crate::children::Running::python(Run { id, shared }, stdout, stderr))
}

fn interrupt(thread: c_ulong) {
    if let Ok(api) = api() {
        // SAFETY: the API from init; the GIL is held for the call.
        unsafe {
            let gil = (api.gil_ensure)();
            (api.set_async_exc)(thread, api.keyboard_interrupt);
            (api.gil_release)(gil);
        }
    }
}

fn stop_shared(shared: &Shared) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.exit.is_some() {
        return;
    }
    state.stop = true;
    if let Some(thread) = state.thread {
        drop(state);
        interrupt(thread);
    }
}

/// Stops the run with this id (a link check nobody waits for any more).
pub fn stop(id: u32) {
    let shared = RUNS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|runs| runs.get(&id).cloned());
    if let Some(shared) = shared {
        stop_shared(&shared);
    }
}

impl Run {
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Whether it succeeded, if it has ended.
    pub fn try_wait(&mut self) -> Option<bool> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).exit.map(|code| code == 0)
    }

    /// Waits for it to end: whether it succeeded.
    pub fn wait(&mut self) -> bool {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.exit.is_none() {
            state = self.shared.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.exit == Some(0)
    }

    pub fn stop(&mut self) {
        stop_shared(&self.shared);
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(runs) = RUNS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            runs.remove(&self.id);
        }
    }
}

/// Run once, in __main__: per-thread stdout/stderr, inherited by the threads
/// a run starts, and `__eddy_run`, which runs a module's command line with
/// its output to the run's pipes and its thread and exit status to the
/// status pipe.
const BOOTSTRAP: &str = r#"
import io, os, sys, threading

_eddy = threading.local()

class _EddyStream(io.TextIOBase):
    """Writes to the stream of the run this thread belongs to."""
    def __init__(self, which, fallback):
        self._which, self._fallback = which, fallback
    def _target(self):
        return getattr(_eddy, self._which, None) or self._fallback
    def write(self, text):
        return self._target().write(text)
    def flush(self):
        self._target().flush()
    def isatty(self):
        return False
    def fileno(self):
        return self._target().fileno()
    @property
    def encoding(self):
        return "utf-8"
    @property
    def errors(self):
        return "replace"
    @property
    def buffer(self):
        return self._target().buffer

sys.stdout = _EddyStream("out", sys.stdout)
sys.stderr = _EddyStream("err", sys.stderr)

_eddy_start = threading.Thread.start
def _eddy_inherit(self, *args, **kwargs):
    out, err = getattr(_eddy, "out", None), getattr(_eddy, "err", None)
    run = self.run
    def run_in_its_run():
        _eddy.out, _eddy.err = out, err
        run()
    self.run = run_in_its_run
    return _eddy_start(self, *args, **kwargs)
threading.Thread.start = _eddy_inherit

# FFmpeg, as functions (set up when the app has it): one run at a time, as
# its command-line code keeps global state.
_eddy_ff = None
_eddy_ff_lock = threading.Lock()

def __eddy_ffmpeg_setup(run_ffmpeg, run_ffprobe, cancel, clear, stand_ins):
    global _eddy_ff
    import ctypes
    run = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_int, ctypes.POINTER(ctypes.c_char_p),
                           ctypes.c_char_p, ctypes.c_size_t, ctypes.c_char_p, ctypes.c_size_t)
    void = ctypes.CFUNCTYPE(None)
    _eddy_ff = {"ffmpeg": run(run_ffmpeg), "ffprobe": run(run_ffprobe),
                "cancel": void(cancel), "clear": void(clear), "dir": stand_ins}

def _eddy_ff_run(tool, args):
    """Runs ffmpeg or ffprobe: (exit code, stdout, stderr) as bytes."""
    import ctypes
    argv = [tool.encode()] + [os.fsencode(a) if isinstance(a, str) else bytes(a) for a in args]
    array = (ctypes.c_char_p * len(argv))(*argv)
    out = ctypes.create_string_buffer(16 << 20)
    err = ctypes.create_string_buffer(1 << 20)
    with _eddy_ff_lock:
        _eddy_ff["clear"]()
        code = _eddy_ff[tool](len(argv), array, out, len(out), err, len(err))
    return code, out.value, err.value

class _EddyPopen:
    """yt-dlp's Popen for ffmpeg and ffprobe, run in-process (iOS starts no
    programs: anything else isn't found)."""
    def __init__(self, args, *_, stdout=None, stderr=None, text=False,
                 universal_newlines=False, **__):
        import shlex, subprocess
        argv = shlex.split(args) if isinstance(args, str) else [os.fsdecode(a) for a in args]
        tool = os.path.basename(argv[0]) if argv else ""
        if _eddy_ff is None or tool not in ("ffmpeg", "ffprobe"):
            raise FileNotFoundError(2, "iOS runs no programs", argv[0] if argv else "")
        self.args, self.pid, self.returncode = args, 0, None
        self.stdin = self.stdout = self.stderr = None
        self._pipe_out = stdout == subprocess.PIPE
        self._pipe_err = stderr == subprocess.PIPE
        self._merge = stderr == subprocess.STDOUT
        self._text = text or universal_newlines
        self._result = None
        self._tool, self._argv = tool, argv[1:]
    def _finish(self):
        if self._result is None:
            code, out, err = _eddy_ff_run(self._tool, self._argv)
            self.returncode = code
            if self._merge:
                out, err = out + err, b""
            if self._text:
                out, err = out.decode("utf-8", "replace"), err.decode("utf-8", "replace")
            # Not piped: the run's own output, as a program's would be.
            if not self._pipe_out and not self._merge and out:
                sys.stdout.write(out if self._text else out.decode("utf-8", "replace"))
                out = None
            if not self._pipe_err and not self._merge and err:
                sys.stderr.write(err if self._text else err.decode("utf-8", "replace"))
                err = None
            self._result = (out if (self._pipe_out or self._merge) else None,
                            err if self._pipe_err else None)
        return self._result
    def communicate(self, input=None, timeout=None):
        return self._finish()
    def communicate_or_kill(self, *args, **kwargs):
        return self._finish()
    def wait(self, timeout=None):
        self._finish()
        return self.returncode
    def poll(self):
        return self.returncode
    def kill(self):
        _eddy_ff["cancel"]()
    terminate = kill
    def __enter__(self):
        return self
    def __exit__(self, *_):
        self.wait()
    @classmethod
    def run(cls, *args, timeout=None, **kwargs):
        with cls(*args, **kwargs) as proc:
            stdout, stderr = proc.communicate_or_kill(timeout=timeout)
            return stdout or "", stderr or "", proc.returncode

_eddy_patched = False

def _eddy_patch_ytdlp():
    """yt-dlp's modules use their own name for Popen: each gets ours."""
    global _eddy_patched
    if _eddy_patched or _eddy_ff is None:
        return
    import yt_dlp.utils
    original = yt_dlp.utils.Popen
    for name, module in list(sys.modules.items()):
        if (name == "yt_dlp" or name.startswith("yt_dlp.")) and getattr(module, "Popen", None) is original:
            module.Popen = _EddyPopen
    _eddy_patched = True

# gallery-dl reads its command line from sys.argv: one at a time for now.
_eddy_argv = threading.Lock()

def __eddy_run(out_fd, err_fd, status_fd, module, args):
    out = open(out_fd, "w", encoding="utf-8", errors="replace", buffering=1)
    err = open(err_fd, "w", encoding="utf-8", errors="replace", buffering=1)
    status = open(status_fd, "w", buffering=1)
    status.write(f"{threading.get_ident()}\n")
    _eddy.out, _eddy.err = out, err
    code = 0
    try:
        if module == "yt_dlp":
            import yt_dlp
            if _eddy_ff is not None:
                _eddy_patch_ytdlp()
                if "--ffmpeg-location" not in args:
                    args = ["--ffmpeg-location", _eddy_ff["dir"], *args]
            yt_dlp.main(args)
        elif module == "gallery_dl":
            import gallery_dl
            with _eddy_argv:
                sys.argv = ["gallery-dl", *args]
                code = gallery_dl.main() or 0
        else:
            raise ValueError(f"no such tool: {module}")
    except SystemExit as e:
        if e.code is None:
            code = 0
        elif isinstance(e.code, int):
            code = e.code
        else:
            err.write(f"{e.code}\n")
            code = 1
    except KeyboardInterrupt:
        code = 130
    except BaseException:
        import traceback
        traceback.print_exc(file=err)
        code = 1
    finally:
        _eddy.out = _eddy.err = None
        for stream in (out, err):
            try:
                stream.close()
            except Exception:
                pass
        status.write(f"exit {code}\n")
        status.close()
"#;
