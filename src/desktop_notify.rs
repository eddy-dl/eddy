//! Desktop counterparts of Android's download service: a notification for
//! each download that finished or failed, and the downloads' progress on
//! the taskbar button (Windows: ITaskbarList3; Linux: the Unity launcher
//! entry signal, which KDE's task manager and Dash to Dock show). macOS
//! gets the notifications only.

use crate::DownloaderApp;
use slint::ComponentHandle;
use std::time::Duration;

/// Starts checking once a second. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    let ui = ui.as_weak();
    let timer = slint::Timer::default();
    let mut taskbar = taskbar::Taskbar::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        for result in crate::downloader::take_finished() {
            notify(result.ok, &result.name, &result.message);
        }
        if let Some(ui) = ui.upgrade() {
            let (count, _, progress) = crate::downloader::summary();
            taskbar.show(ui.window(), count, progress);
        }
    });
    // Runs for as long as the app does.
    std::mem::forget(timer);
}

/// Off the UI thread: showing a notification waits on the system.
fn notify(ok: bool, name: &str, message: &str) {
    let summary = if ok { "Downloaded".to_string() } else { "Download failed".to_string() };
    let body = if ok || message.is_empty() { name.to_string() } else { format!("{name}\n{message}") };
    std::thread::spawn(move || {
        let result = notify_rust::Notification::new().appname("Eddy").summary(&summary).body(&body).show();
        if let Err(e) = result {
            log::warn!("couldn't show a notification: {e}");
        }
    });
}

#[cfg(target_os = "windows")]
mod taskbar {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::UI::Shell::{ITaskbarList3, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TaskbarList};

    /// The progress bar on the window's taskbar button.
    #[derive(Default)]
    pub struct Taskbar {
        list: Option<ITaskbarList3>,
        // What's showing, so the taskbar is only told about changes.
        shown: Option<(bool, u64)>,
    }

    impl Taskbar {
        pub fn show(&mut self, window: &slint::Window, count: usize, progress: Option<f64>) {
            let Some(hwnd) = hwnd(window) else {
                return; // not on screen yet
            };
            if self.list.is_none() {
                self.list = unsafe {
                    // The UI thread's COM apartment; already set up by the
                    // windowing library, in which case this is a no-op.
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                    CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
                        .and_then(|list| list.HrInit().map(|_| list))
                        .ok()
                };
            }
            let Some(list) = &self.list else {
                return;
            };
            // Thousandths; u64::MAX for "busy, size unknown".
            let state = (count > 0, progress.map(|p| (p.clamp(0.0, 1.0) * 1000.0) as u64).unwrap_or(u64::MAX));
            if self.shown == Some(state) {
                return;
            }
            self.shown = Some(state);
            unsafe {
                let _ = match state {
                    (false, _) => list.SetProgressState(hwnd, TBPF_NOPROGRESS),
                    (true, u64::MAX) => list.SetProgressState(hwnd, TBPF_INDETERMINATE),
                    (true, done) => list
                        .SetProgressState(hwnd, TBPF_NORMAL)
                        .and_then(|_| list.SetProgressValue(hwnd, done, 1000)),
                };
            }
        }
    }

    fn hwnd(window: &slint::Window) -> Option<HWND> {
        let handle = window.window_handle();
        match handle.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut core::ffi::c_void)),
            _ => None,
        }
    }
}

#[cfg(target_os = "linux")]
mod taskbar {
    use std::collections::HashMap;
    use std::sync::mpsc::{Sender, channel};
    use zbus::zvariant::Value;

    /// The progress bar docks and task managers draw on the app's icon,
    /// through the Unity launcher entry D-Bus signal. They match it to the
    /// app by its .desktop file, eddy.desktop.
    pub struct Taskbar {
        updates: Option<Sender<Option<f64>>>,
        shown: Option<Option<u64>>,
    }

    impl Default for Taskbar {
        fn default() -> Self {
            // D-Bus calls wait on the bus, so they get a thread of their own.
            let (sender, receiver) = channel::<Option<f64>>();
            let spawned = std::thread::Builder::new().name("eddy-launcher".into()).spawn(move || {
                let Ok(connection) = zbus::blocking::Connection::session() else {
                    return; // no session bus: nothing to show it on
                };
                for progress in receiver {
                    let mut properties: HashMap<&str, Value> = HashMap::new();
                    properties.insert("progress-visible", Value::from(progress.is_some()));
                    properties.insert("progress", Value::from(progress.unwrap_or(0.0)));
                    let _ = connection.emit_signal(
                        None::<&str>,
                        "/dev/pah/eddy/launcher",
                        "com.canonical.Unity.LauncherEntry",
                        "Update",
                        &("application://eddy.desktop", properties),
                    );
                }
            });
            Self { updates: spawned.ok().map(|_| sender), shown: None }
        }
    }

    impl Taskbar {
        pub fn show(&mut self, _window: &slint::Window, count: usize, progress: Option<f64>) {
            // Hidden when nothing's downloading; empty while the size is
            // unknown (there's no "busy" look).
            let progress = (count > 0).then(|| progress.unwrap_or(0.0).clamp(0.0, 1.0));
            let key = progress.map(|p| (p * 1000.0) as u64);
            if self.shown == Some(key) {
                return;
            }
            self.shown = Some(key);
            if let Some(updates) = &self.updates {
                let _ = updates.send(progress);
            }
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod taskbar {
    #[derive(Default)]
    pub struct Taskbar;

    impl Taskbar {
        pub fn show(&mut self, _window: &slint::Window, _count: usize, _progress: Option<f64>) {}
    }
}
