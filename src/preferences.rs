use crate::DownloaderApp;
use crate::platform;
use crate::{AccentColor, AppSettings, ThemeMode, ThemeSettings};
use slint::ComponentHandle;

const ACCENT_COLOR_KEY: &str = "accent_color";
const AMOLED_KEY: &str = "amoled";
const HAPTIC_FEEDBACK_KEY: &str = "enable_haptic_feedback";
const THEME_MODE_KEY: &str = "theme_mode";
const TOP_NAVIGATION_BAR_KEY: &str = "top_navigation_bar";
const LANGUAGE_KEY: &str = "language";
// Settings → Downloads → Books: the preview's starting options.
const NOVEL_NOTES_KEY: &str = "novel_author_notes";
const NOVEL_IMAGES_KEY: &str = "novel_images";
const NOVEL_NOTE_IMAGES_KEY: &str = "novel_author_note_images";
const NOVEL_SMALL_IMAGES_KEY: &str = "novel_small_images";

// Locales with a bundled translation (English is the source language).
const BUNDLED_LANGUAGES: &[&str] = &["en", "fr"];

/// Switches the UI to `language` ("" = follow the system locale). Languages
/// without a bundled translation fall back to English.
pub fn apply_language(language: &str) {
    let language = if language.is_empty() {
        sys_locale::get_locale().unwrap_or_default()
    } else {
        language.to_string()
    };
    // "fr-CA" / "fr_FR" -> "fr"
    let base = language.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
    let selected = if BUNDLED_LANGUAGES.contains(&base.as_str()) { base.as_str() } else { "en" };
    if let Err(e) = slint::select_bundled_translation(selected) {
        eprintln!("failed to select translation {selected}: {e}");
    }
}

// pub const DOWNLOAD_DIR_RAW: &str = "download_dir_raw";
pub const DOWNLOAD_DIR_DISPLAY: &str = "download_dir_display";

fn parse_theme_mode(s: &str) -> ThemeMode {
    match s.to_ascii_lowercase().as_str() {
        "light" => ThemeMode::Light,
        "dark" => ThemeMode::Dark,
        _ => ThemeMode::System,
    }
}
fn theme_mode_to_str(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::System => "system",
        ThemeMode::Light => "light",
        ThemeMode::Dark => "dark",
    }
}
fn parse_accent(s: &str) -> AccentColor {
    match s.to_ascii_lowercase().as_str() {
        "mutedred" => AccentColor::MutedRed,
        "green" => AccentColor::Green,
        "pink" => AccentColor::Pink,
        "purple" => AccentColor::Purple,
        "yellow" => AccentColor::Yellow,
        "orange" => AccentColor::Orange,
        "blue" => AccentColor::Blue,
        "greyscale" => AccentColor::Greyscale,
        "system" => AccentColor::System,
        _ => AccentColor::MutedRed,
    }
}

fn accent_to_str(accent: AccentColor) -> &'static str {
    match accent {
        AccentColor::MutedRed => "mutedred",
        AccentColor::Green => "green",
        AccentColor::Pink => "pink",
        AccentColor::Purple => "purple",
        AccentColor::Yellow => "yellow",
        AccentColor::Orange => "orange",
        AccentColor::Blue => "blue",
        AccentColor::Greyscale => "greyscale",
        AccentColor::System => "system",
    }
}

pub fn load_preferences(ui_weak: slint::Weak<DownloaderApp>) {
    let mode = parse_theme_mode(
        &platform::get_pref(THEME_MODE_KEY, "system").unwrap_or_else(|_| "sytem".into()),
    );
    let accent_color = parse_accent(
        &platform::get_pref(ACCENT_COLOR_KEY, "system").unwrap_or_else(|_| "system".into()),
    );
    let amoled: bool = platform::get_pref(AMOLED_KEY, "false")
        .unwrap_or_else(|_| "false".into())
        .parse()
        .unwrap_or(false);
    let enable_haptic_feedback: bool = platform::get_pref(HAPTIC_FEEDBACK_KEY, "false")
        .unwrap_or_else(|_| "false".into())
        .parse()
        .unwrap_or(false);
    // let download_dir_raw: String =
    //     platform::get_pref(DOWNLOAD_DIR_RAW, "").unwrap_or_else(|_| String::new());
    let download_dir_display: String =
        platform::get_pref(DOWNLOAD_DIR_DISPLAY, "").unwrap_or_default();
    if let Some(ui) = ui_weak.upgrade() {
        let app_settings = ui.global::<AppSettings>();
        let theme_settings = ui.global::<ThemeSettings>();
        // The platform default is whatever the caller set before loading.
        let top_nav_default = app_settings.get_top_navigation_bar();
        let top_navigation_bar: bool =
            platform::get_pref(TOP_NAVIGATION_BAR_KEY, &top_nav_default.to_string())
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(top_nav_default);
        app_settings.set_top_navigation_bar(top_navigation_bar);
        let language = platform::get_pref(LANGUAGE_KEY, "").unwrap_or_default();
        apply_language(&language);
        app_settings.set_language(language.into());
        theme_settings.set_mode(mode);
        theme_settings.set_accent(accent_color);
        theme_settings.set_amoled_dark(amoled);
        app_settings.set_haptic_feedback(enable_haptic_feedback);
        app_settings.set_current_download_path(download_dir_display.into());
        let novel = novel_defaults();
        app_settings.set_novel_author_notes(novel.author_notes);
        app_settings.set_novel_images(novel.images);
        app_settings.set_novel_author_note_images(novel.author_note_images);
        app_settings.set_novel_small_images(novel.small_images);
    }
}

/// The Books defaults (every chapter, with these options). Read from the
/// saved preferences, so the share popup's process gets them too.
pub fn novel_defaults() -> crate::novels::NovelOptions {
    let flag = |key: &str, default: bool| {
        platform::get_pref(key, &default.to_string()).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    };
    let base = crate::novels::NovelOptions::default();
    crate::novels::NovelOptions {
        author_notes: flag(NOVEL_NOTES_KEY, base.author_notes),
        images: flag(NOVEL_IMAGES_KEY, base.images),
        author_note_images: flag(NOVEL_NOTE_IMAGES_KEY, base.author_note_images),
        small_images: flag(NOVEL_SMALL_IMAGES_KEY, base.small_images),
        ..base
    }
}

pub fn save_preferences(ui_weak: slint::Weak<DownloaderApp>) {
    if let Some(ui) = ui_weak.upgrade() {
        let app_settings = ui.global::<AppSettings>();
        let theme_settings = ui.global::<ThemeSettings>();
        let mode = theme_settings.get_mode();
        if let Err(e) = platform::save_pref(THEME_MODE_KEY, theme_mode_to_str(mode)) {
            eprintln!("failed to save theme mode: {e}");
        }
        let accent = theme_settings.get_accent();
        if let Err(e) = platform::save_pref(ACCENT_COLOR_KEY, accent_to_str(accent)) {
            eprintln!("failed to save accent color: {e}");
        }
        let amoled: &str = if theme_settings.get_amoled_dark() {
            "true"
        } else {
            "false"
        };
        if let Err(e) = platform::save_pref(AMOLED_KEY, amoled) {
            eprintln!("failed to save amoled dark mode preference: {e}");
        }
        let enable_haptic_feedback: &str = if app_settings.get_haptic_feedback() {
            "true"
        } else {
            "false"
        };

        if let Err(e) = platform::save_pref(HAPTIC_FEEDBACK_KEY, enable_haptic_feedback) {
            eprintln!("failed to save haptic feedback preference: {e}")
        }

        let top_navigation_bar: &str = if app_settings.get_top_navigation_bar() {
            "true"
        } else {
            "false"
        };
        if let Err(e) = platform::save_pref(TOP_NAVIGATION_BAR_KEY, top_navigation_bar) {
            eprintln!("failed to save top navigation bar preference: {e}");
        }

        if let Err(e) = platform::save_pref(LANGUAGE_KEY, &app_settings.get_language()) {
            eprintln!("failed to save language preference: {e}");
        }

        for (key, value) in [
            (NOVEL_NOTES_KEY, app_settings.get_novel_author_notes()),
            (NOVEL_IMAGES_KEY, app_settings.get_novel_images()),
            (NOVEL_NOTE_IMAGES_KEY, app_settings.get_novel_author_note_images()),
            (NOVEL_SMALL_IMAGES_KEY, app_settings.get_novel_small_images()),
        ] {
            if let Err(e) = platform::save_pref(key, &value.to_string()) {
                eprintln!("failed to save {key}: {e}");
            }
        }
    }
}

/// Applies the saved theme, accent and language to another window (the share
/// popup), which only needs to look like the app.
pub fn load_theme<T>(ui: &T)
where
    T: slint::ComponentHandle,
    for<'a> ThemeSettings<'a>: slint::Global<'a, T>,
{
    let get = |key: &str, default: &str| platform::get_pref(key, default).unwrap_or_else(|_| default.into());
    let theme_settings = ui.global::<ThemeSettings>();
    theme_settings.set_mode(parse_theme_mode(&get(THEME_MODE_KEY, "system")));
    theme_settings.set_accent(parse_accent(&get(ACCENT_COLOR_KEY, "system")));
    theme_settings.set_amoled_dark(get(AMOLED_KEY, "false").parse().unwrap_or(false));
    if let Some(accent) = platform::system_accent() {
        theme_settings.set_system_accent(accent);
    }
    apply_language(&get(LANGUAGE_KEY, ""));
}
