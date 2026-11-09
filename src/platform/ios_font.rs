//! iOS: the font Eddy draws with. Settings → Appearance → System font is
//! iOS's own (San Francisco), as Android's is the phone's; Font picks any
//! other with Apple's font picker (fonts installed from other apps too).
//! Either way its file goes to Slint as SLINT_DEFAULT_FONT before the UI
//! starts. A font whose file Eddy can't read (another app's) is rebuilt
//! from the tables CoreText hands over, in Eddy's own folder.

use crate::{AppSettings, DownloaderApp};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_core_foundation::{CFString, CFURL, CFURLPathStyle};
use objc2_core_text::{CTFont, CTFontTableOptions, CTFontUIFontType, kCTFontURLAttribute};
use objc2_ui_kit::{UIFontPickerViewController, UIFontPickerViewControllerDelegate};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::path::PathBuf;

/// The picked font: its file (found, or rebuilt, as it was picked: another
/// app's font may not be Eddy's to open later) and its name to show.
const CHOSEN: &str = "ios_font";
const CHOSEN_NAME: &str = "ios_font_name";

/// Points Slint at the picked font, else (none, or its file is gone) iOS's
/// system font if Settings asks for it. Call before the UI is created
/// (Slint picks its font once). The outcome goes where Android's does (`system_font_status`), for
/// Settings' line under the switch when it couldn't be used.
pub fn apply(system_font: bool) {
    let chosen = super::get_pref(CHOSEN, "").unwrap_or_default();
    let picked = Some(chosen).filter(|path| !path.is_empty()).and_then(|path| match std::fs::File::open(&path) {
        Ok(_) => Some(Ok(path)),
        Err(e) => {
            log::warn!("the picked font, {path}: {e}");
            None
        }
    });
    let file = if picked.is_some() {
        picked
    } else if system_font {
        // SAFETY: CoreText's own font for the system's UI, at its default size.
        let font = unsafe { CTFont::new_ui_font_for_language(CTFontUIFontType::System, 0.0, None) };
        Some(font.ok_or_else(|| "iOS gave no system font".to_string()).and_then(|f| font_file(&f)))
    } else {
        None
    };
    let status = match file {
        None => "none: off".to_string(),
        Some(Ok(path)) => {
            // SAFETY: set before Slint (or anything else) reads it.
            unsafe { std::env::set_var("SLINT_DEFAULT_FONT", &path) };
            path
        }
        Some(Err(why)) => format!("none: {why}"),
    };
    if let Err(e) = super::save_pref("system_font_status", &status) {
        log::warn!("couldn't save the font's status: {e}");
    }
}

/// A file of `font` Slint can read: its own, else one rebuilt from its
/// tables (kept in Eddy's fonts folder, by its PostScript name).
fn font_file(font: &CTFont) -> Result<String, String> {
    // SAFETY: a key CoreText defines, asked of a font it made.
    let own = unsafe { font.attribute(kCTFontURLAttribute) }
        .and_then(|url| url.downcast_ref::<CFURL>().and_then(|url| url.file_system_path(CFURLPathStyle::CFURLPOSIXPathStyle)))
        .map(|path| path.to_string())
        .filter(|path| std::fs::File::open(path).is_ok());
    if let Some(path) = own {
        return Ok(path);
    }
    // SAFETY: a font CoreText made.
    let name = unsafe { font.post_script_name() }.to_string();
    let path = fonts_dir().join(format!("{}.otf", safe(&name)));
    if !path.is_file() {
        let data = rebuilt(font).ok_or_else(|| format!("{name} gave no tables to draw with"))?;
        std::fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(|e| e.to_string())?;
        std::fs::write(&path, data).map_err(|e| e.to_string())?;
    }
    Ok(path.to_string_lossy().into_owned())
}

fn safe(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// `font` as an OpenType file, from its tables: a table directory, then
/// each table (4-byte aligned), as the format has it.
fn rebuilt(font: &CTFont) -> Option<Vec<u8>> {
    // SAFETY: a font CoreText made; the array holds tags as its values.
    let tags = unsafe { font.available_tables(CTFontTableOptions::NoOptions) }?;
    let mut tables: Vec<(u32, Vec<u8>)> = (0..tags.count())
        // SAFETY: in range; each value is a four-byte tag, not an object.
        .map(|i| unsafe { tags.value_at_index(i) } as usize as u32)
        // SAFETY: a tag the font has.
        .filter_map(|tag| unsafe { font.table(tag, CTFontTableOptions::NoOptions) }.map(|data| (tag, data.to_vec())))
        .collect();
    if tables.is_empty() {
        return None;
    }
    tables.sort_by_key(|(tag, _)| *tag);
    let count = tables.len() as u16;
    let entry_selector = 15 - count.leading_zeros() as u16;
    let search_range = (1u16 << entry_selector) * 16;
    let cff = tables.iter().any(|(tag, _)| *tag == u32::from_be_bytes(*b"CFF "));
    let mut out = Vec::new();
    out.extend_from_slice(if cff { b"OTTO" } else { &[0, 1, 0, 0] });
    for value in [count, search_range, entry_selector, count * 16 - search_range] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    let mut offset = 12 + 16 * tables.len();
    let mut body = Vec::new();
    for (tag, data) in &tables {
        let checksum = data.chunks(4).fold(0u32, |sum, chunk| {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            sum.wrapping_add(u32::from_be_bytes(word))
        });
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&checksum.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        let padded = data.len().div_ceil(4) * 4;
        body.resize(body.len() + padded - data.len(), 0);
        offset += padded;
    }
    out.extend_from_slice(&body);
    Some(out)
}

define_class!(
    // Apple's font picker's delegate: a pick is kept for the next start.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EddyFontPicker"]
    struct FontPicker;

    unsafe impl NSObjectProtocol for FontPicker {}

    unsafe impl UIFontPickerViewControllerDelegate for FontPicker {
        #[unsafe(method(fontPickerViewControllerDidPickFont:))]
        fn picked(&self, picker: &UIFontPickerViewController) {
            if let Some(descriptor) = picker.selectedFontDescriptor() {
                // SAFETY: a descriptor the picker made.
                chosen(&unsafe { descriptor.postscriptName() }.to_string());
            }
            picker.dismissViewControllerAnimated_completion(true, None);
        }

        #[unsafe(method(fontPickerViewControllerDidCancel:))]
        fn cancelled(&self, picker: &UIFontPickerViewController) {
            picker.dismissViewControllerAnimated_completion(true, None);
        }
    }
);

thread_local! {
    static UI: RefCell<Option<slint::Weak<DownloaderApp>>> = const { RefCell::new(None) };
    // The picker's delegate (a picker doesn't keep its delegate).
    static PICKER: RefCell<Option<Retained<FontPicker>>> = const { RefCell::new(None) };
}

/// Settings' Font row: what's picked, picking another, going back to none.
pub fn install(ui: &DownloaderApp) {
    UI.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    let settings = ui.global::<AppSettings>();
    settings.set_can_choose_font(true);
    settings.set_font_choice(super::get_pref(CHOSEN_NAME, "").unwrap_or_default().into());
    settings.on_choose_font(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            show_picker(mtm);
        }
    });
    settings.on_reset_font(|| {
        let _ = std::fs::remove_dir_all(fonts_dir());
        let _ = super::save_pref(CHOSEN, "");
        let _ = super::save_pref(CHOSEN_NAME, "");
        shown("");
    });
}

fn show_picker(mtm: MainThreadMarker) {
    let Some(top) = super::ios::uikit::top_view_controller(mtm) else {
        return;
    };
    let delegate = PICKER.with(|p| {
        p.borrow_mut()
            // SAFETY: a plain NSObject subclass, initialised as one.
            .get_or_insert_with(|| unsafe { msg_send![FontPicker::alloc(mtm), init] })
            .clone()
    });
    let picker = UIFontPickerViewController::init(UIFontPickerViewController::alloc(mtm));
    picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    top.presentViewController_animated_completion(&picker, true, None);
}

/// A font was picked: kept for the next start, its name shown now.
fn chosen(postscript: &str) {
    // SAFETY: a name, at the default size, no matrix.
    let font = unsafe { CTFont::with_name(&CFString::from_str(postscript), 0.0, std::ptr::null()) };
    // SAFETY: a font CoreText made.
    let family = unsafe { font.family_name() }.to_string();
    // Only the newest pick's rebuilt file is kept.
    let _ = std::fs::remove_dir_all(fonts_dir());
    match font_file(&font) {
        Ok(path) => {
            let _ = super::save_pref(CHOSEN, &path);
            let _ = super::save_pref(CHOSEN_NAME, &family);
            shown(&family);
        }
        Err(why) => {
            log::warn!("{postscript}: {why}");
            super::show_toast(&format!("Eddy can't draw with {family}"));
        }
    }
}

fn fonts_dir() -> PathBuf {
    PathBuf::from(super::get_app_data_dir()).join("fonts")
}

fn shown(name: &str) {
    UI.with(|u| {
        if let Some(ui) = u.borrow().as_ref().and_then(|w| w.upgrade()) {
            ui.global::<AppSettings>().set_font_choice(name.into());
        }
    });
    // Slint picks its font once, as the app starts.
    super::show_toast("Takes effect the next time Eddy opens");
}
