#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eddy::DownloaderApp;
use eddy::{AppSettings, SettingsLayout, ThemeSettings, links, platform, preferences};
use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    eddy::logs::init();
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

    // Flat settings rows, not the phone's cards.
    ui.global::<SettingsLayout>().set_desktop(true);

    // Desktop defaults to the top navigation bar; a saved preference overrides it.
    ui.global::<AppSettings>().set_top_navigation_bar(true);
    preferences::load_preferences(ui_weak.clone());
    let app_settings = ui.global::<AppSettings>();
    app_settings.set_app_version(eddy::VERSION.into());
    app_settings.set_slint_version(eddy::SLINT_VERSION.into());
    app_settings.on_open_url(|url| platform::open_url(&url));
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
    ui.window().set_size(slint::PhysicalSize::new(800, 600));
    ui.run()
}
