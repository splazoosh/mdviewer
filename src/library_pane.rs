//! The library pane: search, list, and the selected entry's editor.
//!
//! Edits are written to the entry's file shortly after they stop, and at
//! exit, so a keystroke in a title never costs a synced file write of its
//! own. A delete removes the file at once.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::clock;
use crate::library::Library;

/// How long after the last edit the entry is written.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(800);

/// The library and its pane's UI state.
pub(crate) struct LibraryPane {
    store: Library,
    query: String,
    selected: Option<String>,
    dirty_since: Option<Instant>,
    clipboard: Option<arboard::Clipboard>,
    /// A transient confirmation ("copied") shown under the toolbar.
    notice: Option<(String, Instant)>,
}

impl LibraryPane {
    /// Opens the store at `dir`, carrying over `legacy` if it exists; `None`
    /// keeps the library in memory for the session.
    pub(crate) fn new(dir: Option<PathBuf>, legacy: Option<PathBuf>) -> Self {
        let store = match dir {
            Some(dir) => Library::open(&dir, legacy.as_deref()),
            None => Library::in_memory(),
        };
        Self {
            store,
            query: String::new(),
            selected: None,
            dirty_since: None,
            clipboard: None,
            notice: None,
        }
    }

    /// Adds a clip and selects it.
    pub(crate) fn clip(&mut self, text: &str, source: &Path) {
        let id = self.store.clip(text, source, &clock::today());
        self.selected = Some(id);
        self.query.clear();
        self.mark_dirty();
        self.notice = Some(("clipped".to_owned(), Instant::now()));
    }

    fn mark_dirty(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    /// Writes edited entries — after the debounce, or at once when `force`
    /// is set. A failed write is retried after another debounce.
    ///
    /// Returns an error message to show rather than failing: the app keeps
    /// running on the in-memory store.
    pub(crate) fn flush(&mut self, force: bool) -> Option<String> {
        if !self.store.has_unsaved() {
            self.dirty_since = None;
            return None;
        }
        let since = *self.dirty_since.get_or_insert_with(Instant::now);
        if !force && since.elapsed() < SAVE_DEBOUNCE {
            return None;
        }
        match self.store.flush() {
            Ok(()) => {
                self.dirty_since = None;
                None
            }
            Err(error) => {
                self.dirty_since = Some(Instant::now());
                Some(format!("{error:#}"))
            }
        }
    }

    /// Whether a write is pending; the app keeps repainting until it lands.
    pub(crate) fn has_pending_write(&self) -> bool {
        self.dirty_since.is_some()
    }

    fn copy(&mut self, text: &str) -> Option<String> {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(error) => return Some(format!("clipboard unavailable: {error}")),
            }
        }
        let result = self.clipboard.as_mut().map(|c| c.set_text(text.to_owned()));
        match result {
            Some(Ok(())) => {
                self.notice = Some(("copied".to_owned(), Instant::now()));
                None
            }
            Some(Err(error)) => Some(format!("copy failed: {error}")),
            None => None,
        }
    }

    /// Draws the pane. Returns an error message when something failed.
    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let mut error = None;

        ui.horizontal(|ui| {
            let heading = ui.strong("Library");
            if let Some(dir) = self.store.dir() {
                heading.on_hover_text(dir.display().to_string());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("{}", self.store.len()));
            });
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("search")
                    .desired_width(f32::INFINITY),
            );
        });
        if let Some((text, at)) = &self.notice {
            if at.elapsed() < Duration::from_secs(2) {
                ui.small(text);
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            } else {
                self.notice = None;
            }
        }
        ui.separator();

        let ids = self.store.search(&self.query);
        let list_height = ui.available_height() * 0.4;
        egui::ScrollArea::vertical()
            .id_salt("library_list")
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if ids.is_empty() {
                    ui.weak(if self.store.is_empty() {
                        "Select text in the editor and clip it."
                    } else {
                        "No matches."
                    });
                }
                for id in ids {
                    let Some(entry) = self.store.get(&id) else {
                        continue;
                    };
                    let selected = self.selected.as_deref() == Some(id.as_str());
                    let label = format!(
                        "{}\n{} · {}",
                        entry.title,
                        entry.clipped,
                        source_name(&entry.source)
                    );
                    let response = ui
                        .selectable_label(selected, label)
                        .on_hover_text(&entry.source);
                    if response.clicked() {
                        self.selected = Some(id);
                    }
                }
            });
        ui.separator();

        let Some(id) = self.selected.clone() else {
            return error;
        };
        let Some(entry) = self.store.get(&id).cloned() else {
            self.selected = None;
            return error;
        };

        let mut title = entry.title.clone();
        let mut text = entry.text.clone();
        let mut remove = false;
        let mut copy = false;

        ui.horizontal(|ui| {
            if ui
                .button("Copy")
                .on_hover_text("Copy the text to the clipboard")
                .clicked()
            {
                copy = true;
            }
            if ui.button("Delete").clicked() {
                remove = true;
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut title)
                .hint_text("title")
                .desired_width(f32::INFINITY),
        );
        let file = self
            .store
            .path_of(&id)
            .map_or_else(|| id.clone(), |p| p.display().to_string());
        ui.small(format!("{} · {}", entry.clipped, entry.source))
            .on_hover_text(&file);
        egui::ScrollArea::vertical()
            .id_salt("library_text")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(8),
                );
            });

        if copy {
            error = self.copy(&entry.text);
        }
        if remove {
            match self.store.remove(&id) {
                Ok(_) => self.selected = None,
                Err(err) => error = Some(format!("{err:#}")),
            }
        } else if title != entry.title || text != entry.text {
            if let Some(live) = self.store.edit(&id) {
                live.title = title;
                live.text = text;
            }
            self.mark_dirty();
        }
        error
    }
}

/// The file name part of a stored source path.
fn source_name(source: &str) -> &str {
    source
        .rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_name_takes_the_last_component() {
        assert_eq!(source_name("C:\\repo\\docs\\a.md"), "a.md");
        assert_eq!(source_name("/r/b.md"), "b.md");
        assert_eq!(source_name(""), "");
    }

    #[test]
    fn flush_is_debounced_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("library");
        let mut pane = LibraryPane::new(Some(lib.clone()), None);
        pane.clip("hello", Path::new("x.md"));
        assert!(pane.has_pending_write());
        assert_eq!(pane.flush(false), None);
        assert!(!lib.join("0001-hello.md").exists());
        assert_eq!(pane.flush(true), None);
        assert!(!pane.has_pending_write());
        assert!(lib.join("0001-hello.md").exists());
        assert_eq!(Library::open(&lib, None).len(), 1);
    }

    #[test]
    fn a_failed_write_stays_pending_and_lands_at_exit() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("library");
        let mut pane = LibraryPane::new(Some(lib.clone()), None);
        pane.clip("hello", Path::new("x.md"));
        std::fs::remove_dir(&lib).unwrap();
        std::fs::write(&lib, "a file where the directory was").unwrap();
        assert!(pane.flush(true).is_some());
        assert!(pane.has_pending_write());
        std::fs::remove_file(&lib).unwrap();
        std::fs::create_dir(&lib).unwrap();
        assert_eq!(pane.flush(true), None);
        assert!(lib.join("0001-hello.md").exists());
    }
}
