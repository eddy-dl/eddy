//! The download lists' selection: which items are selected, as the UI
//! reports each change, and the action picked for them (done by
//! downloads::act).

use crate::{DownloadState, DownloaderApp};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
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

    // Select all, or Invert: the tab's items become the selection (less the
    // ones selected, when inverting). Returns the counts the header shows.
    let ui_weak = ui.as_weak();
    ui.on_selection_all(move |tab, invert| {
        let Some(ui) = ui_weak.upgrade() else {
            return ModelRc::default();
        };
        let items = match tab {
            0 => ui.get_downloading_items(),
            1 => ui.get_stopped_items(),
            _ => ui.get_completed_items(),
        };
        let mut counts = [0; 4];
        SELECTED.with(|s| {
            let mut s = s.borrow_mut();
            for item in items.iter() {
                let key = (tab, item.id);
                let selected = if invert { !s.contains(&key) } else { true };
                if !selected {
                    s.remove(&key);
                    continue;
                }
                s.insert(key);
                counts[0] += 1;
                match item.state {
                    DownloadState::Active | DownloadState::Queued => counts[1] += 1,
                    DownloadState::Paused => counts[2] += 1,
                    DownloadState::Failed | DownloadState::Cancelled => counts[3] += 1,
                    _ => {}
                }
            }
        });
        ModelRc::new(VecModel::from(counts.to_vec()))
    });

    ui.on_selection_action(|tab, action, delete_files| {
        let ids: Vec<i32> = SELECTED.with(|s| {
            s.borrow().iter().filter(|(t, _)| *t == tab).map(|(_, id)| *id).collect()
        });
        crate::downloads::act(&ids, action, delete_files);
    });
}
