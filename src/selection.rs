//! Acting on the download lists: the selection (which items are selected,
//! as the UI reports each change, and the action picked for them) and the
//! options for a single download.

use crate::{DownloaderApp, SelectionAction};
use std::cell::RefCell;
use std::collections::BTreeSet;

thread_local! {
    // (tab, item id) of every selected item; lives on the UI thread.
    static SELECTED: RefCell<BTreeSet<(i32, i32)>> = const { RefCell::new(BTreeSet::new()) };
}

/// Hooks the selection callbacks up. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    ui.on_selection_changed(|tab, id, selected| {
        SELECTED.with(|s| {
            let mut s = s.borrow_mut();
            if selected {
                s.insert((tab, id));
            } else {
                s.remove(&(tab, id));
            }
        });
    });

    ui.on_selection_cleared(|| SELECTED.with(|s| s.borrow_mut().clear()));

    ui.on_selection_action(|tab, action, delete_files| {
        let ids: Vec<i32> = SELECTED.with(|s| {
            s.borrow().iter().filter(|(t, _)| *t == tab).map(|(_, id)| *id).collect()
        });
        // TODO: act on the real downloads once the lists come from the
        // download engine (they're placeholders in the UI for now):
        // pause / resume / cancel through aria2 and yt-dlp, and for delete,
        // drop them from the list and remove their files if asked.
        let what = match action {
            SelectionAction::Pause => "pause",
            SelectionAction::Resume => "resume",
            SelectionAction::Cancel => "cancel",
            SelectionAction::Delete if delete_files => "delete with files",
            SelectionAction::Delete => "delete",
        };
        log::info!("{what} {ids:?} in tab {tab}");
        eprintln!("{what} {ids:?} in tab {tab}");
    });

    ui.on_download_action(|id, action, value| {
        // TODO: look the download up once the lists come from the download
        // engine (they're placeholders in the UI for now), then:
        // - Open: platform::open_file on its saved path
        // - Pause / Resume / Cancel / Delete: through aria2 or yt-dlp
        // - Rename: rename the file (or aria2's `out` while it downloads)
        // - ChangeFolder: pick a folder, then move the file there
        // - RefreshLink: aria2 changeUri to `value`, keeping what's downloaded
        log::info!("{action:?} download {id} {value:?}");
        eprintln!("{action:?} download {id} {value:?}");
    });
}
