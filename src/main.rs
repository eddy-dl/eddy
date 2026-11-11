#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eddy::DownloaderApp;
use eddy::{AppSettings, SettingsLayout, ThemeSettings, links, platform, preferences};
use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    eddy::logs::init();
    // Started by a browser, for its extension: a download to hand over.
    if eddy::native_host::started_by_browser() {
        eddy::native_host::run();
        return Ok(());
    }
    // `eddy <link>… <file>…`: to the Eddy already open if there is one (it
    // comes forward), else to this one once it's up.
    let args = eddy::handoff::arguments();
    if eddy::handoff::hand_over(&args) {
        return Ok(());
    }
    if let Err(e) = eddy::db::init(platform::get_app_data_dir()) {
        eprintln!("Failed to initialize database: {e}");
    }
    let ui = DownloaderApp::new()?;
    // Short messages show in the app's snackbar.
    let ui_weak = ui.as_weak();
    platform::set_toast_handler(move |message| {
        let ui_weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let snackbar = ui.global::<eddy::Snackbar>();
                snackbar.set_message(message.into());
                snackbar.set_serial(snackbar.get_serial().wrapping_add(1));
            }
        });
    });
    // Logs that grew too big since last time (in the background).
    eddy::logs::trim();

    let ui_weak = ui.as_weak();
    let update_system_accent = move || {
        if let Some(ui) = ui_weak.upgrade() {
            if let Some(accent) = platform::system_accent() {
                ui.global::<ThemeSettings>().set_system_accent(accent);
            }
        }
    };
    update_system_accent();

    let ui_weak = ui.as_weak();

    // Settings without the Android-only rows (the system font).
    ui.global::<SettingsLayout>().set_desktop(true);

    // Desktop defaults to the top navigation bar; a saved preference overrides it.
    ui.global::<AppSettings>().set_top_navigation_bar(true);
    preferences::load_preferences(ui_weak.clone());
    let app_settings = ui.global::<AppSettings>();
    app_settings.set_app_version(eddy::VERSION.into());
    app_settings.set_slint_version(eddy::SLINT_VERSION.into());
    app_settings.on_open_url(|url| platform::open_url(&url));
    app_settings.set_can_register_handlers(eddy::handoff::can_register_handlers());
    app_settings.on_register_handlers(|| match eddy::handoff::register_handlers() {
        Ok(()) => platform::show_toast("Eddy now opens magnet links and .torrent files"),
        Err(e) => platform::show_toast(&format!("Couldn't set Eddy up for them: {e}")),
    });
    app_settings.set_can_register_browser(true);
    app_settings.on_register_browser(|| match eddy::native_host::register() {
        Ok(1) => platform::show_toast("The browser extension can now reach Eddy"),
        Ok(n) => platform::show_toast(&format!("The browser extension can now reach Eddy, in {n} browsers")),
        Err(e) => platform::show_toast(&format!("Couldn't set the browser extension up: {e}")),
    });
    app_settings.on_preferences_changed({
        move || {
            preferences::save_preferences(ui_weak.clone());
        }
    });
    app_settings.on_theme_changed(update_system_accent);
    let settings_weak = slint::Global::<'_, DownloaderApp>::as_weak(&app_settings);
    app_settings.on_language_changed(move || {
        if let Some(settings) = settings_weak.upgrade() {
            preferences::apply_language(&settings.get_language());
        }
    });
    ui.global::<AppSettings>().set_default_download_path(eddy::storage::default_display().into());
    links::install_main(&ui);
    eddy::selection::install(&ui);
    eddy::downloads::install(&ui);
    eddy::components_desktop::install(&ui);
    eddy::desktop_notify::install(&ui);
    eddy::cookies::install(&ui);
    eddy::cache::install(&ui);
    eddy::handoff::listen(&ui);
    eddy::handoff::open(&ui, &args);
    ui.window().set_size(slint::PhysicalSize::new(800, 600));
    ui.run()
}
