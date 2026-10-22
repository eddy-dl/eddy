//! The download lists' selection: which items are selected, as the UI
//! reports each change, and the action picked for them (done by
//! downloads::act).

use crate::DownloaderApp;
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
        crate::downloads::act(&ids, action, delete_files);
    });
}
