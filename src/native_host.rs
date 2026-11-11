//! The browser extension's way in (extension/): browsers start Eddy as their
//! native messaging host, "dev.pah.eddy", which Settings → General → Browser
//! extension sets up. Chrome (and the browsers built on it) passes the
//! extension's origin as an argument, Firefox the host's manifest and the
//! extension's id. Eddy then reads the one message the extension sends (a
//! download, as browser::Caught has it), hands it to the Eddy running
//! (handoff), starting one if there's none, answers and quits.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

pub const NAME: &str = "dev.pah.eddy";
/// The extension's id in Chrome, fixed by the key in its manifest.
const CHROME_ID: &str = "jedmmhndimmkhedpmggbhcllpccngilc";
/// Its id in Firefox (browser_specific_settings.gecko.id).
const FIREFOX_ID: &str = "eddy@pah.dev";
/// A message is a download's details, nowhere near this.
const LARGEST: usize = 1 << 20;

/// Whether a browser started this Eddy as its native messaging host.
pub fn started_by_browser() -> bool {
    std::env::args().skip(1).any(|arg| arg.starts_with("chrome-extension://") || arg == FIREFOX_ID)
}

/// Takes the extension's message, hands it over and answers it.
pub fn run() {
    let answer = match read_message().and_then(|message| deliver(&message)) {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => {
            log::warn!("browser extension: {e}");
            serde_json::json!({ "ok": false, "error": e })
        }
    };
    let data = answer.to_string().into_bytes();
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&(data.len() as u32).to_ne_bytes());
    let _ = stdout.write_all(&data);
    let _ = stdout.flush();
}

/// A message: its length (4 bytes, the machine's order), then its JSON.
fn read_message() -> Result<serde_json::Value, String> {
    let mut stdin = std::io::stdin().lock();
    let mut length = [0u8; 4];
    stdin.read_exact(&mut length).map_err(|e| format!("no message: {e}"))?;
    let length = u32::from_ne_bytes(length) as usize;
    if length > LARGEST {
        return Err("the message is too big".to_string());
    }
    let mut data = vec![0; length];
    stdin.read_exact(&mut data).map_err(|e| format!("the message was cut short: {e}"))?;
    serde_json::from_slice(&data).map_err(|e| format!("the message isn't JSON: {e}"))
}

fn deliver(message: &serde_json::Value) -> Result<(), String> {
    let caught: crate::browser::Caught = serde_json::from_value(message.clone()).map_err(|e| e.to_string())?;
    let scheme = caught.url.split_once("://").map(|(s, _)| s.to_ascii_lowercase()).unwrap_or_default();
    if !matches!(scheme.as_str(), "http" | "https" | "ftp") {
        return Err("Eddy can't fetch this kind of link".to_string());
    }
    let line = format!("{}{}", crate::handoff::BROWSER, serde_json::to_string(&message).map_err(|e| e.to_string())?);
    let args = [line];
    if crate::handoff::hand_over(&args) {
        return Ok(());
    }
    // None running: one started, which takes it once it's up.
    start_eddy()?;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(250));
        if crate::handoff::hand_over(&args) {
            return Ok(());
        }
    }
    Err("Eddy didn't start".to_string())
}

/// Starts Eddy on its own: it outlives this host, which the browser ends.
fn start_eddy() -> Result<(), String> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(&exe);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Out of the browser's job (Chrome allows it), so closing the host
        // doesn't close Eddy; without, if the browser doesn't allow it.
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0200;
        const BREAKAWAY: u32 = 0x0100_0000;
        if command.creation_flags(DETACHED | BREAKAWAY).spawn().is_ok() {
            return Ok(());
        }
        command.creation_flags(DETACHED);
    }
    command.spawn().map(|_| ()).map_err(|e| format!("couldn't start Eddy: {e}"))
}

/// Where each browser looks for a host's manifest, for those installed:
/// (folder, Firefox's kind of manifest).
#[cfg(not(target_os = "windows"))]
fn host_folders() -> Vec<(PathBuf, bool)> {
    let home = dirs::home_dir().unwrap_or_default();
    // (the browser's own folder, where in it hosts go, Firefox)
    #[cfg(target_os = "macos")]
    let browsers: [(PathBuf, &str, bool); 6] = {
        let support = home.join("Library/Application Support");
        [
            (support.join("Google/Chrome"), "NativeMessagingHosts", false),
            (support.join("Chromium"), "NativeMessagingHosts", false),
            (support.join("Microsoft Edge"), "NativeMessagingHosts", false),
            (support.join("BraveSoftware/Brave-Browser"), "NativeMessagingHosts", false),
            (support.join("Vivaldi"), "NativeMessagingHosts", false),
            (support.join("Mozilla"), "NativeMessagingHosts", true),
        ]
    };
    #[cfg(not(target_os = "macos"))]
    let browsers: [(PathBuf, &str, bool); 6] = {
        let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        [
            (config.join("google-chrome"), "NativeMessagingHosts", false),
            (config.join("chromium"), "NativeMessagingHosts", false),
            (config.join("microsoft-edge"), "NativeMessagingHosts", false),
            (config.join("BraveSoftware/Brave-Browser"), "NativeMessagingHosts", false),
            (config.join("vivaldi"), "NativeMessagingHosts", false),
            (home.join(".mozilla"), "native-messaging-hosts", true),
        ]
    };
    browsers.into_iter().filter(|(browser, _, _)| browser.is_dir()).map(|(browser, hosts, firefox)| (browser.join(hosts), firefox)).collect()
}

/// The host's manifest: Eddy itself, for the extension only.
fn manifest(exe: &std::path::Path, firefox: bool) -> String {
    let mut manifest = serde_json::json!({
        "name": NAME,
        "description": "Eddy, the download manager",
        "path": exe.to_string_lossy(),
        "type": "stdio",
    });
    if firefox {
        manifest["allowed_extensions"] = serde_json::json!([FIREFOX_ID]);
    } else {
        manifest["allowed_origins"] = serde_json::json!([format!("chrome-extension://{CHROME_ID}/")]);
    }
    serde_json::to_string_pretty(&manifest).unwrap_or_default()
}

/// Lets the browser extension reach Eddy: the host's manifest where each
/// browser looks (Windows: in Eddy's data folder, pointed to from the
/// user's registry). Returns how many browsers it was set up for.
pub fn register() -> Result<usize, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    #[cfg(not(target_os = "windows"))]
    {
        let folders = host_folders();
        if folders.is_empty() {
            return Err("no Chrome, Chromium, Edge, Brave, Vivaldi or Firefox found".to_string());
        }
        for (folder, firefox) in &folders {
            std::fs::create_dir_all(folder)
                .and_then(|_| std::fs::write(folder.join(format!("{NAME}.json")), manifest(&exe, *firefox)))
                .map_err(|e| format!("{}: {e}", folder.display()))?;
        }
        Ok(folders.len())
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let dir = PathBuf::from(crate::platform::get_app_data_dir()).join("browser-host");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let chrome = dir.join(format!("{NAME}.json"));
        let firefox = dir.join(format!("{NAME}.firefox.json"));
        std::fs::write(&chrome, manifest(&exe, false)).map_err(|e| e.to_string())?;
        std::fs::write(&firefox, manifest(&exe, true)).map_err(|e| e.to_string())?;
        let keys = [
            (r"HKCU\Software\Google\Chrome\NativeMessagingHosts", &chrome),
            (r"HKCU\Software\Chromium\NativeMessagingHosts", &chrome),
            (r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts", &chrome),
            (r"HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts", &chrome),
            (r"HKCU\Software\Vivaldi\NativeMessagingHosts", &chrome),
            (r"HKCU\Software\Mozilla\NativeMessagingHosts", &firefox),
        ];
        for (key, file) in keys {
            // No console window flashing up.
            let output = std::process::Command::new("reg")
                .args(["add", &format!(r"{key}\{NAME}"), "/ve", "/d", &file.to_string_lossy(), "/f"])
                .creation_flags(0x0800_0000)
                .output()
                .map_err(|e| format!("reg: {e}"))?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
        }
        Ok(keys.len())
    }
}
