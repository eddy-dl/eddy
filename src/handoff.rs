//! The desktop app's command line, and one Eddy at a time: `eddy <link>…
//! <file>…` opens Eddy with those in the preview (a .torrent, a text file of
//! links), as sharing does on Android. A running Eddy listens on a local port
//! (written with a random token to its data folder), so a second `eddy …`
//! hands its arguments over and quits rather than opening a second window on
//! the same database; with none, it brings the open window forward. Also makes
//! Eddy what the system opens magnet links and .torrent files with
//! (Settings → General). The browser extension's downloads come the same
//! way (see native_host), as a `browser` line.

use crate::DownloaderApp;
use slint::ComponentHandle;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Starts a line that's a download from the browser extension, its details
/// (browser::Caught) as JSON after it.
pub const BROWSER: &str = "browser\t";

/// "<port> <token>" of the Eddy running now.
fn instance_file() -> PathBuf {
    PathBuf::from(crate::platform::get_app_data_dir()).join("instance")
}

fn is_link(arg: &str) -> bool {
    arg.contains("://") || arg.to_ascii_lowercase().starts_with("magnet:")
}

/// The command line's links and files, files made absolute (they may go to
/// an Eddy started elsewhere).
pub fn arguments() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|arg| !arg.trim().is_empty())
        .map(|arg| match is_link(&arg) {
            false if Path::new(&arg).exists() => {
                std::fs::canonicalize(&arg).map(|p| p.to_string_lossy().into_owned()).unwrap_or(arg)
            }
            _ => arg,
        })
        .collect()
}

/// Hands `args` to the Eddy already running; false if there's none (or it
/// didn't answer: a stale file from one that stopped).
pub fn hand_over(args: &[String]) -> bool {
    let Ok(text) = std::fs::read_to_string(instance_file()) else {
        return false;
    };
    let Some((port, token)) = text.trim().split_once(' ') else {
        return false;
    };
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    // The token, an argument a line, and a blank line to end.
    let mut message = format!("{token}\n");
    for arg in args {
        message.push_str(&arg.replace(['\r', '\n'], " "));
        message.push('\n');
    }
    message.push('\n');
    if stream.write_all(message.as_bytes()).is_err() {
        return false;
    }
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).is_ok() && answer.trim() == "ok"
}

/// Takes the arguments of later `eddy …` runs. Call once the window is set
/// up (UI thread).
pub fn listen(ui: &DownloaderApp) {
    use rand::Rng;
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(e) => {
            log::warn!("no port for later eddy commands: {e}");
            return;
        }
    };
    let Ok(port) = listener.local_addr().map(|a| a.port()) else {
        return;
    };
    let token: String = rand::thread_rng().sample_iter(&rand::distributions::Alphanumeric).take(32).map(char::from).collect();
    if let Err(e) = std::fs::write(instance_file(), format!("{port} {token}")) {
        log::warn!("couldn't note the port for later eddy commands: {e}");
        return;
    }
    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
            let Ok(mut answer) = stream.try_clone() else {
                continue;
            };
            let mut lines = BufReader::new(stream).lines().map_while(Result::ok);
            if lines.next().as_deref() != Some(token.as_str()) {
                continue;
            }
            let args: Vec<String> = lines.take_while(|line| !line.is_empty()).collect();
            let _ = answer.write_all(b"ok\n");
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.window().set_minimized(false);
                    let _ = ui.show();
                    open(&ui, &args);
                }
            });
        }
    });
}

/// Puts the links and files of `args` in the pending list, and opens the
/// preview if anything came of them.
pub fn open(ui: &DownloaderApp, args: &[String]) {
    let mut links = String::new();
    let mut added = false;
    for arg in args {
        if let Some(json) = arg.strip_prefix(BROWSER) {
            match serde_json::from_str::<crate::browser::Caught>(json) {
                Ok(caught) => added |= crate::links::add_caught(&caught).is_some(),
                Err(e) => log::warn!("a download from the browser extension that isn't one: {e}"),
            }
            continue;
        }
        let path = Path::new(arg);
        if !is_link(arg) && path.is_file() {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            added |= !crate::links::add_file(arg, &name).is_empty();
        } else {
            links.push_str(arg);
            links.push('\n');
        }
    }
    if !links.is_empty() {
        added |= !crate::links::add_links_from_text(&links).is_empty();
    }
    if added {
        ui.set_preview_open(true);
    }
}

/// Whether Eddy can make itself the handler here (macOS sends an app its
/// links through Apple events, which Slint doesn't pass on).
pub fn can_register_handlers() -> bool {
    cfg!(any(target_os = "linux", target_os = "windows"))
}

/// Makes Eddy what the system opens magnet links and .torrent files with:
/// on Linux a .desktop file and xdg-mime, on Windows the user's registry.
pub fn register_handlers() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    #[cfg(target_os = "linux")]
    {
        let dir = dirs::data_dir().ok_or("no data folder")?.join("applications");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // Quoted as the Desktop Entry spec wants (and %, the field codes').
        let quoted: String = exe
            .to_string_lossy()
            .chars()
            .flat_map(|c| match c {
                '"' | '`' | '$' | '\\' => vec!['\\', c],
                '%' => vec!['%', '%'],
                _ => vec![c],
            })
            .collect();
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Eddy\nComment=Download manager\nExec=\"{quoted}\" %U\n\
             Terminal=false\nNoDisplay=true\nMimeType=x-scheme-handler/magnet;application/x-bittorrent;\n"
        );
        std::fs::write(dir.join("dev.pah.eddy.desktop"), entry).map_err(|e| e.to_string())?;
        let status = std::process::Command::new("xdg-mime")
            .args(["default", "dev.pah.eddy.desktop", "x-scheme-handler/magnet", "application/x-bittorrent"])
            .status()
            .map_err(|e| format!("xdg-mime: {e}"))?;
        if !status.success() {
            return Err(format!("xdg-mime failed ({status})"));
        }
        // Not everywhere; menus and file managers read it.
        let _ = std::process::Command::new("update-desktop-database").arg(&dir).status();
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let command = format!("\"{}\" \"%1\"", exe.to_string_lossy());
        let values: [(&str, Option<&str>, &str); 6] = [
            (r"HKCU\Software\Classes\magnet", None, "URL:Magnet link"),
            (r"HKCU\Software\Classes\magnet", Some("URL Protocol"), ""),
            (r"HKCU\Software\Classes\magnet\shell\open\command", None, &command),
            (r"HKCU\Software\Classes\Eddy.Torrent", None, "Torrent"),
            (r"HKCU\Software\Classes\Eddy.Torrent\shell\open\command", None, &command),
            (r"HKCU\Software\Classes\.torrent", None, "Eddy.Torrent"),
        ];
        for (key, value, data) in values {
            let mut reg = std::process::Command::new("reg");
            reg.args(["add", key]);
            match value {
                Some(name) => reg.args(["/v", name]),
                None => reg.arg("/ve"),
            };
            // No console window flashing up.
            let output = reg.args(["/d", data, "/f"]).creation_flags(0x0800_0000).output().map_err(|e| format!("reg: {e}"))?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = exe;
        Err("not on this system yet".to_string())
    }
}
