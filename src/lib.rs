pub mod cache;
pub mod components;
pub mod cookies;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod components_desktop;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod desktop_notify;
pub mod db;
pub mod downloader;
pub mod extractors;
pub mod links;
pub mod novels;
pub mod platform;
pub mod preferences;
pub mod selection;
pub mod storage;

slint::include_modules!();

#[cfg(target_os = "android")]
use android_activity::AndroidApp;

pub const APP_ID: &str = "dev.pah.eddy";
pub const SLINT_VERSION: &str = env!("DEPENDENCY_SLINT_VERSION");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    use android_logger::Config;
    use log::LevelFilter;

    android_logger::init_once(
        Config::default()
            .with_max_level(LevelFilter::Info)
            .with_tag("eddy"),
    );

    log::info!("Starting Slint App on Android");
    platform::init(app.clone());
    if let Err(e) = slint::android::init(app) {
        log::error!("Failed to initialize Slint Android backend: {:?}", e);
        return;
    }


    if platform::is_share_popup() {
        run_share_popup();
        platform::deinit();
        return;
    }

    let ui = match DownloaderApp::new() {
        Ok(ui) => ui,
        Err(e) => {
            log::error!("Failed to create Slint window: {:?}", e);
            return;
        }
    };
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

    preferences::load_preferences(ui_weak.clone());
    let update_system_bars = move || {
        if let Some(ui) = ui_weak.upgrade() {
            let dark_icons = !ui.global::<ThemeSettings>().get_is_dark();
            platform::set_bar_icons_dark(dark_icons);
        }
    };

    update_system_bars();
    let app_settings = ui.global::<AppSettings>();
    app_settings.on_theme_changed({
        move || {
            update_system_bars();
            update_system_accent();
        }
    });
    let ui_weak = ui.as_weak();
    let settings_weak = slint::Global::<'_, DownloaderApp>::as_weak(&app_settings);
    app_settings.on_language_changed(move || {
        if let Some(settings) = settings_weak.upgrade() {
            preferences::apply_language(&settings.get_language());
        }
    });
    app_settings.on_preferences_changed({
        move || {
            preferences::save_preferences(ui_weak.clone());
        }
    });

    let settings_weak = slint::Global::<'_, DownloaderApp>::as_weak(&app_settings);
    app_settings.on_select_download_folder({
        move || {
            let settings_weak_clone = settings_weak.clone();

            platform::pick_download_folder(move |raw_path, display_name| {
                if let Err(e) =
                    platform::save_pref(preferences::DOWNLOAD_DIR_DISPLAY, &display_name)
                {
                    eprintln!("Failed to save display path to pref: {}", e);
                }

                if let Err(e) = db::set_raw_download_path(&raw_path) {
                    eprintln!("Failed to save raw path to DB: {}", e);
                }

                let settings_weak_slint = settings_weak_clone.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(settings) = settings_weak_slint.upgrade() {
                        settings.set_current_download_path(display_name.into());
                    }
                });
            });
        }
    });

    app_settings.set_is_mobile(true);
    app_settings.set_default_download_path(storage::default_display().into());
    app_settings.set_all_files_access(platform::has_all_files_access());
    app_settings.on_request_all_files_access(platform::request_all_files_access);
    let settings_weak = slint::Global::<'_, DownloaderApp>::as_weak(&app_settings);
    app_settings.on_storage_access_check(move || {
        if let Some(settings) = settings_weak.upgrade() {
            settings.set_all_files_access(platform::has_all_files_access());
        }
    });
    links::install_main(&ui);
    selection::install(&ui);
    components::install(&ui);
    cookies::install(&ui);
    cache::install(&ui);
    ui.run().expect("Failed to run Slint app");
    platform::deinit();
}

/// The share popup ("Download with Eddy" / "Preview in Eddy"): a small Slint
/// window over the app the links came from, in its own process. Whatever
/// isn't started or skipped stays pending for the main app.
#[cfg(target_os = "android")]
fn run_share_popup() {
    let Some((mode, shared)) = platform::popup_share() else {
        platform::finish_activity();
        return;
    };
    let ui = match PreviewApp::new() {
        Ok(ui) => ui,
        Err(e) => {
            log::error!("Failed to create the share popup: {:?}", e);
            platform::finish_activity();
            return;
        }
    };
    preferences::load_theme(&ui);
    ui.set_mode(mode);
    links::install(&ui);

    let added = match shared {
        platform::Shared::Text(text) => links::add_links_from_text(&text),
        platform::Shared::File { path, name } => links::add_file(&path, &name),
    };
    if added.is_empty() {
        platform::show_toast("No links found");
        platform::finish_activity();
        return;
    }

    let close = || {
        platform::finish_activity();
        let _ = slint::quit_event_loop();
    };
    links::on_empty(close);
    ui.on_close_requested(close);

    if let Err(e) = ui.run() {
        log::error!("Share popup failed: {:?}", e);
    }
}
