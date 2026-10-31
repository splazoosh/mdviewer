//! The eframe application: three fixed panes — repository tree or search
//! results, editor, prompt library — and the menu bar above them.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::time::SystemTime;

use eframe::CreationContext;

use crate::editor::{Document, SaveError};
use crate::library_pane::LibraryPane;
use crate::persist::DataPaths;
use crate::search::{self, Corpus, Hit, Results};
use crate::settings::Settings;
use crate::tree::{Node, SortDir, SortKey, SortOrder};

/// Hits shown for one query; the rest are counted.
const MAX_HITS: usize = 500;

/// The editor's widget id, fixed so the app can place its cursor.
const EDITOR_ID: &str = "editor";

/// An opened repository: its root, the scanned tree and the searchable
/// text.
pub(crate) struct Repository {
    root: PathBuf,
    tree: Node,
    corpus: Corpus,
}

impl Repository {
    /// Sorts the scanned `tree` of `root` by `order` and brings `corpus` —
    /// the previous scan's text, or empty — in line with it.
    fn new(root: PathBuf, mut tree: Node, order: SortOrder, mut corpus: Corpus) -> Self {
        tree.sort(order);
        let started = std::time::Instant::now();
        corpus.refresh(&tree);
        tracing::info!(
            files = corpus.len(),
            bytes = corpus.bytes(),
            ms = started.elapsed().as_millis(),
            "search corpus refreshed"
        );
        Self { root, tree, corpus }
    }

    /// `path` relative to the root, for display.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// The repository search box and its results.
#[derive(Default)]
struct RepoSearch {
    query: String,
    /// The query `results` answer; recomputed when it or the corpus changes.
    answered: Option<String>,
    results: Results,
    /// The row the user chose, for highlighting.
    chosen: Option<(PathBuf, usize, usize)>,
}

impl RepoSearch {
    fn invalidate(&mut self) {
        self.answered = None;
    }

    /// Runs the query when it has not been answered yet.
    fn refresh(&mut self, corpus: &Corpus) {
        if self.answered.as_deref() == Some(self.query.as_str()) {
            return;
        }
        let started = std::time::Instant::now();
        self.results = corpus.search(&self.query, MAX_HITS);
        if !self.query.trim().is_empty() {
            tracing::debug!(
                hits = self.results.total,
                files = self.results.files,
                us = started.elapsed().as_micros(),
                "repository search"
            );
        }
        self.answered = Some(self.query.clone());
    }
}

/// A place in a file the editor should show once the file is open: 1-based
/// line, character column, match length.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Jump {
    path: PathBuf,
    line: usize,
    column: usize,
    len: usize,
}

/// A question the user must answer before the app continues.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Prompt {
    /// The open document is dirty and the user asked to open another file.
    UnsavedThenOpen(PathBuf),
    /// The open document is dirty and the user asked to close the window.
    UnsavedThenQuit,
    /// A save was refused because the file changed on disk.
    ChangedOnDisk,
}

/// The whole app state.
pub(crate) struct ViewerApp {
    /// `None` when no data directory could be resolved; the session then runs
    /// on in-memory defaults.
    paths: Option<DataPaths>,
    library: LibraryPane,
    settings: Settings,
    repo: Option<Repository>,
    search: RepoSearch,
    document: Option<Document>,
    /// The editor's current selection as char indices, when non-empty.
    selection: Option<Range<usize>>,
    /// Applied by the editor pane once the file is the open document.
    jump: Option<Jump>,
    prompt: Option<Prompt>,
    /// The open folder dialog's answer, once it is given.
    folder_pick: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// Set once the user has confirmed a close so the next close request is
    /// not intercepted again.
    quit_confirmed: bool,
    /// The last failure worth showing in the status line.
    error: Option<String>,
}

impl ViewerApp {
    pub(crate) fn new(
        _cc: &CreationContext<'_>,
        paths: Option<DataPaths>,
        initial_repo: Option<PathBuf>,
    ) -> Self {
        let library = LibraryPane::new(
            paths.as_ref().map(DataPaths::library),
            paths.as_ref().map(DataPaths::legacy_library),
        );
        let settings = paths
            .as_ref()
            .map(|p| Settings::load(&p.settings()))
            .unwrap_or_default();
        let initial_repo = initial_repo.or_else(|| settings.last_repo.clone());
        let mut app = Self {
            paths,
            library,
            settings,
            repo: None,
            search: RepoSearch::default(),
            document: None,
            selection: None,
            jump: None,
            prompt: None,
            folder_pick: None,
            quit_confirmed: false,
            error: None,
        };
        if let Some(root) = initial_repo {
            app.open_repository(root);
        }
        app
    }

    fn report(&mut self, error: impl std::fmt::Display) {
        let message = error.to_string();
        tracing::warn!("{message}");
        self.error = Some(message);
    }

    /// Writes the settings; a failure is reported, not fatal.
    fn save_settings(&mut self) {
        let Some(paths) = &self.paths else {
            return;
        };
        if let Err(error) = self.settings.save(&paths.settings()) {
            self.report(format!("{error:#}"));
        }
    }

    /// Every way in — the dialog, the command line, the last repository
    /// remembered — comes through here, so the root is normalised once.
    fn open_repository(&mut self, root: PathBuf) {
        let root = normalise_root(root);
        let tree = match Node::scan(&root) {
            Ok(tree) => tree,
            Err(error) => {
                self.report(format!("{error:#}"));
                return;
            }
        };
        let corpus = match self.repo.take() {
            Some(repo) if repo.root == root => repo.corpus,
            _ => Corpus::default(),
        };
        let repo = Repository::new(root, tree, self.settings.sort, corpus);
        tracing::info!(root = %repo.root.display(), files = repo.tree.file_count(), "opened repository");
        if self.settings.last_repo.as_deref() != Some(repo.root.as_path()) {
            self.settings.last_repo = Some(repo.root.clone());
            self.save_settings();
        }
        self.repo = Some(repo);
        self.search.invalidate();
        self.error = None;
    }

    /// Shows the folder dialog on its own thread, so the window keeps
    /// drawing while it is open; [`Self::poll_folder_pick`] takes the answer.
    fn pick_repository(&mut self, ctx: &egui::Context) {
        if self.folder_pick.is_some() {
            return;
        }
        let mut dialog = rfd::FileDialog::new().set_title("Open repository");
        if let Some(repo) = &self.repo {
            dialog = dialog.set_directory(&repo.root);
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("folder-dialog".to_owned())
            .spawn(move || {
                let _ = tx.send(dialog.pick_folder());
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.folder_pick = Some(rx),
            Err(error) => self.report(format!("failed to open the folder dialog: {error}")),
        }
    }

    fn poll_folder_pick(&mut self) {
        let Some(rx) = &self.folder_pick else {
            return;
        };
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            Ok(picked) => {
                self.folder_pick = None;
                if let Some(root) = picked {
                    self.open_repository(root);
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.folder_pick = None;
                self.report("the folder dialog failed");
            }
        }
    }

    fn rescan(&mut self) {
        if let Some(root) = self.repo.as_ref().map(|r| r.root.clone()) {
            self.open_repository(root);
        }
    }

    fn set_order(&mut self, order: SortOrder) {
        if order == self.settings.sort {
            return;
        }
        self.settings.sort = order;
        if let Some(repo) = &mut self.repo {
            repo.tree.sort(order);
        }
        self.save_settings();
    }

    /// Opens `path` in the editor, asking first if the current buffer is
    /// dirty.
    fn request_open(&mut self, path: PathBuf) {
        if self.document.as_ref().is_some_and(|d| d.path() == path) {
            return;
        }
        self.jump = None;
        if self.document.as_ref().is_some_and(Document::is_dirty) {
            self.prompt = Some(Prompt::UnsavedThenOpen(path));
        } else {
            self.open_document(&path);
        }
    }

    fn open_document(&mut self, path: &Path) {
        match Document::open(path) {
            Ok(doc) => {
                self.document = Some(doc);
                self.selection = None;
                self.error = None;
            }
            Err(error) => self.report(format!("{error:#}")),
        }
    }

    /// Saves the open document; a conflict raises the prompt instead. Returns
    /// whether the buffer is now on disk.
    fn save(&mut self) -> bool {
        let Some(doc) = &mut self.document else {
            return false;
        };
        match doc.save() {
            Ok(()) => {
                self.error = None;
                self.rescan_sizes();
                true
            }
            Err(SaveError::ChangedOnDisk) => {
                self.prompt = Some(Prompt::ChangedOnDisk);
                false
            }
            Err(error) => {
                self.report(format!("{:#}", anyhow::Error::new(error)));
                false
            }
        }
    }

    /// Refreshes the tree's sizes and dates after a save without losing the
    /// selection.
    fn rescan_sizes(&mut self) {
        self.rescan();
    }

    /// Opens the hit's file and shows the match.
    fn open_hit(&mut self, hit: &Hit) {
        self.search.chosen = Some((hit.path.clone(), hit.line, hit.column));
        self.request_open(hit.path.clone());
        self.jump = Some(Jump {
            path: hit.path.clone(),
            line: hit.line,
            column: hit.column,
            len: hit.len,
        });
    }

    /// Stores the editor's selection in the library.
    fn clip(&mut self) {
        let Some(doc) = &self.document else {
            return;
        };
        let Some(range) = self.selection.clone() else {
            return;
        };
        let text = doc.slice_chars(range);
        if text.trim().is_empty() {
            return;
        }
        self.library.clip(&text, doc.path());
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                let open = egui::Button::new("Open repository…");
                if ui.add_enabled(self.folder_pick.is_none(), open).clicked() {
                    ui.close();
                    self.pick_repository(&ui.ctx().clone());
                }
                if ui
                    .add_enabled(self.repo.is_some(), egui::Button::new("Rescan"))
                    .clicked()
                {
                    ui.close();
                    self.rescan();
                }
                ui.separator();
                let save = egui::Button::new("Save").shortcut_text("Ctrl+S");
                if ui.add_enabled(self.document.is_some(), save).clicked() {
                    ui.close();
                    self.save();
                }
                let clip = egui::Button::new("Clip selection").shortcut_text("Ctrl+Shift+K");
                if ui.add_enabled(self.selection.is_some(), clip).clicked() {
                    ui.close();
                    self.clip();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Help", |ui| {
                match &self.paths {
                    Some(paths) => ui.label(format!("Data: {}", paths.dir().display())),
                    None => ui.label("Data: persistence disabled for this session"),
                };
            });
        });
    }

    fn tree_pane(&mut self, ui: &mut egui::Ui, focus_search: bool) {
        ui.horizontal(|ui| {
            let open = egui::Button::new("Open…");
            if ui.add_enabled(self.folder_pick.is_none(), open).clicked() {
                self.pick_repository(&ui.ctx().clone());
            }
            let mut order = self.settings.sort;
            egui::ComboBox::from_id_salt("sort_key")
                .selected_text(order.key.label())
                .show_ui(ui, |ui| {
                    for key in SortKey::ALL {
                        ui.selectable_value(&mut order.key, key, key.label());
                    }
                });
            let arrow = match order.dir {
                SortDir::Ascending => "▲",
                SortDir::Descending => "▼",
            };
            if ui.button(arrow).on_hover_text("Toggle direction").clicked() {
                order.dir = order.dir.toggled();
            }
            self.set_order(order);
        });
        let search_id = egui::Id::new("repo_search");
        ui.horizontal(|ui| {
            let response = ui.add_enabled(
                self.repo.is_some(),
                egui::TextEdit::singleline(&mut self.search.query)
                    .id(search_id)
                    .hint_text("search files (Ctrl+Shift+F)")
                    .desired_width(ui.available_width() - 28.0),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.search.query.clear();
            }
            if ui
                .add_enabled(!self.search.query.is_empty(), egui::Button::new("×"))
                .on_hover_text("Clear")
                .clicked()
            {
                self.search.query.clear();
            }
        });
        if focus_search {
            ui.memory_mut(|m| m.request_focus(search_id));
        }
        ui.separator();

        let Some(repo) = &self.repo else {
            ui.weak("No repository open.");
            return;
        };

        if !self.search.query.trim().is_empty() {
            self.search.refresh(&repo.corpus);
            self.results_list(ui);
            return;
        }

        ui.label(repo.root.display().to_string())
            .on_hover_text(format!("{} markdown files", repo.tree.file_count()));

        let selected = self.document.as_ref().map(|d| d.path().to_path_buf());
        let mut clicked = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(children) = &repo.tree.children {
                    for child in children {
                        tree_node(ui, child, selected.as_deref(), &mut clicked);
                    }
                }
            });
        if let Some(path) = clicked {
            self.request_open(path);
        }
    }

    /// The search results, grouped by file; a click opens the hit.
    fn results_list(&mut self, ui: &mut egui::Ui) {
        let Some(repo) = &self.repo else {
            return;
        };
        let results = &self.search.results;
        let summary = match (results.total, results.hits.len()) {
            (0, _) => "No matches.".to_owned(),
            (total, shown) if shown < total => {
                format!(
                    "{total} matches in {} files; first {shown} shown",
                    results.files
                )
            }
            (total, _) => format!("{total} matches in {} files", results.files),
        };
        ui.weak(summary);

        let mut clicked = None;
        egui::ScrollArea::both()
            .id_salt("results")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                let mut current: Option<&Path> = None;
                for hit in &results.hits {
                    if current != Some(hit.path.as_path()) {
                        current = Some(hit.path.as_path());
                        ui.add_space(4.0);
                        ui.strong(repo.relative(&hit.path))
                            .on_hover_text(hit.path.display().to_string());
                    }
                    let chosen = self.search.chosen.as_ref()
                        == Some(&(hit.path.clone(), hit.line, hit.column));
                    let label = hit_label(ui, hit);
                    if ui
                        .add(
                            egui::Button::selectable(chosen, label)
                                .wrap_mode(egui::TextWrapMode::Extend),
                        )
                        .clicked()
                    {
                        clicked = Some(hit.clone());
                    }
                }
            });
        if let Some(hit) = clicked {
            self.open_hit(&hit);
        }
    }

    /// Places the editor's cursor on a pending jump once its file is open,
    /// and scrolls it into view.
    fn apply_jump(
        &mut self,
        ui: &mut egui::Ui,
        doc: &Document,
        output: &egui::text_edit::TextEditOutput,
    ) {
        let Some(jump) = &self.jump else {
            return;
        };
        if jump.path != doc.path() {
            return;
        }
        let start = search::char_index(&doc.text, jump.line, jump.column);
        let end = (start + jump.len).min(doc.text.chars().count());
        let mut state = output.state.clone();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(start),
                egui::text::CCursor::new(end),
            )));
        let id = output.response.response.id;
        state.store(ui.ctx(), id);
        ui.memory_mut(|m| m.request_focus(id));
        let rect = output
            .galley
            .pos_from_cursor(egui::text::CCursor::new(start))
            .translate(output.galley_pos.to_vec2());
        ui.scroll_to_rect(rect, Some(egui::Align::Center));
        self.selection = (start < end).then_some(start..end);
        self.jump = None;
    }

    /// Returns which header buttons were clicked as `(save, clip)`; the
    /// caller acts on them, because the document is borrowed for the whole
    /// pane.
    fn editor_pane(&mut self, ui: &mut egui::Ui) -> (bool, bool) {
        let Some(mut doc) = self.document.take() else {
            ui.weak("Select a markdown file in the tree.");
            return (false, false);
        };
        let result = self.editor_pane_for(ui, &mut doc);
        self.document = Some(doc);
        result
    }

    fn editor_pane_for(&mut self, ui: &mut egui::Ui, doc: &mut Document) -> (bool, bool) {
        let mut save_clicked = false;
        let mut clip_clicked = false;
        let has_selection = self.selection.is_some();

        ui.horizontal(|ui| {
            let title = if doc.is_dirty() {
                format!("{} •", doc.name())
            } else {
                doc.name()
            };
            ui.strong(title)
                .on_hover_text(doc.path().display().to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(doc.is_dirty(), egui::Button::new("Save"))
                    .clicked()
                {
                    save_clicked = true;
                }
                if ui
                    .add_enabled(has_selection, egui::Button::new("Clip"))
                    .on_hover_text("Store the selection in the library (Ctrl+Shift+K)")
                    .clicked()
                {
                    clip_clicked = true;
                }
            });
        });
        ui.separator();

        let jumping = self.jump.as_ref().is_some_and(|j| j.path == doc.path());
        let output = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let output = egui::TextEdit::multiline(&mut doc.text)
                    .id(egui::Id::new(EDITOR_ID))
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(40)
                    .show(ui);
                self.apply_jump(ui, doc, &output);
                output
            })
            .inner;

        if !jumping && output.response.response.has_focus() {
            self.selection = output.cursor_range.filter(|r| !r.is_empty()).map(|r| {
                let r = r.as_sorted_char_range();
                r.start.0..r.end.0
            });
        }
        (save_clicked, clip_clicked)
    }

    fn prompt_window(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.prompt.clone() else {
            return;
        };
        let name = self
            .document
            .as_ref()
            .map(Document::name)
            .unwrap_or_default();
        let mut resolved = false;
        egui::Modal::new(egui::Id::new("prompt")).show(ctx, |ui| {
            ui.set_width(360.0);
            match &prompt {
                Prompt::UnsavedThenOpen(_) | Prompt::UnsavedThenQuit => {
                    ui.heading("Unsaved changes");
                    ui.label(format!("{name} has unsaved changes."));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() && self.save() {
                            resolved = true;
                            self.continue_after(&prompt, ctx);
                        }
                        if ui.button("Discard").clicked() {
                            resolved = true;
                            self.continue_after(&prompt, ctx);
                        }
                        if ui.button("Cancel").clicked() {
                            resolved = true;
                            self.cancel_prompt();
                        }
                    });
                }
                Prompt::ChangedOnDisk => {
                    ui.heading("File changed on disk");
                    ui.label(format!(
                        "{name} was modified outside the editor since it was opened."
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Overwrite").clicked() {
                            resolved = true;
                            if let Some(doc) = &mut self.document
                                && let Err(error) = doc.save_regardless()
                            {
                                self.report(format!("{error:#}"));
                            }
                        }
                        if ui.button("Reload").clicked() {
                            resolved = true;
                            if let Some(doc) = &mut self.document
                                && let Err(error) = doc.reload()
                            {
                                self.report(format!("{error:#}"));
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            resolved = true;
                            self.cancel_prompt();
                        }
                    });
                }
            }
        });
        if resolved && self.prompt.as_ref() == Some(&prompt) {
            self.prompt = None;
        }
    }

    /// Cancel on any prompt, also given by Escape: the prompt goes and
    /// nothing else happens.
    fn cancel_prompt(&mut self) {
        self.jump = None;
        self.prompt = None;
    }

    /// What the user was trying to do when the unsaved-changes prompt got in
    /// the way.
    fn continue_after(&mut self, prompt: &Prompt, ctx: &egui::Context) {
        match prompt {
            Prompt::UnsavedThenOpen(path) => self.open_document(&path.clone()),
            Prompt::UnsavedThenQuit => {
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Prompt::ChangedOnDisk => {}
        }
    }
}

/// `root` made absolute, with `.`, `..` and symbolic links resolved. On
/// Windows the `\\?\` prefix is left off wherever the plain form names the
/// same directory. A root that cannot be resolved is kept as given; the scan
/// then reports why.
fn normalise_root(root: PathBuf) -> PathBuf {
    let Ok(resolved) = dunce::canonicalize(&root) else {
        return root;
    };
    match plain_unc(&resolved) {
        Some(plain) if std::fs::canonicalize(&plain).is_ok_and(|p| p == resolved) => plain,
        _ => resolved,
    }
}

/// `\\?\UNC\server\share\dir` as `\\server\share\dir`, within the 259
/// UTF-16 units (260 with the terminating NUL) every Windows API accepts.
fn plain_unc(path: &Path) -> Option<PathBuf> {
    let rest = path.to_str()?.strip_prefix(r"\\?\UNC\")?;
    let plain = format!(r"\\{rest}");
    (plain.encode_utf16().count() < 260).then(|| PathBuf::from(plain))
}

/// `  12  …text before match text after…` with the match in strong colour.
fn hit_label(ui: &egui::Ui, hit: &Hit) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let (window, range) = search::excerpt(&hit.text, &hit.range, 30, 80);
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let plain = TextFormat {
        font_id: font.clone(),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.append(
        &format!("{:>5}  ", hit.line),
        0.0,
        TextFormat {
            color: ui.visuals().weak_text_color(),
            ..plain.clone()
        },
    );
    job.append(&window[..range.start], 0.0, plain.clone());
    job.append(
        &window[range.clone()],
        0.0,
        TextFormat {
            color: ui.visuals().strong_text_color(),
            background: ui.visuals().selection.bg_fill,
            ..plain.clone()
        },
    );
    job.append(&window[range.end..], 0.0, plain);
    job
}

/// One node, recursively: a collapsing header for a directory, a selectable
/// row for a file. A click on a file lands in `clicked`.
fn tree_node(
    ui: &mut egui::Ui,
    node: &Node,
    selected: Option<&Path>,
    clicked: &mut Option<PathBuf>,
) {
    match &node.children {
        Some(children) => {
            egui::CollapsingHeader::new(&node.name)
                .id_salt(&node.path)
                .default_open(false)
                .show(ui, |ui| {
                    for child in children {
                        tree_node(ui, child, selected, clicked);
                    }
                });
        }
        None => {
            let is_selected = selected == Some(node.path.as_path());
            let response = ui
                .selectable_label(is_selected, &node.name)
                .on_hover_text(format!(
                    "{}\n{} · {}",
                    node.path.display(),
                    human_size(node.size),
                    human_time(node.modified)
                ));
            if response.clicked() {
                *clicked = Some(node.path.clone());
            }
        }
    }
}

/// `1.2 KB`-style size.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Seconds since the epoch rendered as `YYYY-MM-DD HH:MM` (UTC).
fn human_time(time: SystemTime) -> String {
    let secs = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (date, hm) = crate::clock::split_utc(secs);
    format!("{date} {hm}")
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let started = std::time::Instant::now();
        let ctx = ui.ctx().clone();

        if ctx.input(|i| i.viewport().close_requested())
            && !self.quit_confirmed
            && self.document.as_ref().is_some_and(Document::is_dirty)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.prompt = Some(Prompt::UnsavedThenQuit);
        }

        let save_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::S);
        let clip_shortcut = egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::K,
        );
        let search_shortcut = egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::F,
        );
        if self.prompt.is_some()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.cancel_prompt();
        }
        self.poll_folder_pick();

        let (mut save_requested, mut clip_requested, focus_search) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&save_shortcut),
                i.consume_shortcut(&clip_shortcut),
                i.consume_shortcut(&search_shortcut),
            )
        });

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));

        let mixed = self
            .document
            .as_ref()
            .filter(|d| d.has_mixed_endings())
            .map(|d| {
                format!(
                    "{}: mixed line endings: a save will make them uniform",
                    d.path().display()
                )
            });
        if self.error.is_some() || mixed.is_some() {
            egui::Panel::bottom("status").show(ui, |ui| {
                if let Some(warning) = mixed {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                }
                if let Some(error) = self.error.clone() {
                    ui.horizontal(|ui| {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                        if ui.small_button("×").clicked() {
                            self.error = None;
                        }
                    });
                }
            });
        }

        egui::Panel::left("tree")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| self.tree_pane(ui, focus_search));

        egui::Panel::right("library")
            .resizable(true)
            .default_size(320.0)
            .show(ui, |ui| {
                if let Some(error) = self.library.ui(ui) {
                    self.report(error);
                }
            });

        let (save_clicked, clip_clicked) = egui::CentralPanel::default()
            .show(ui, |ui| self.editor_pane(ui))
            .inner;
        save_requested |= save_clicked;
        clip_requested |= clip_clicked;
        if self.prompt.is_none() {
            if clip_requested {
                self.clip();
            }
            if save_requested {
                self.save();
            }
        }

        if let Some(error) = self.library.flush(false) {
            self.report(error);
        }
        if self.library.has_pending_write() {
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        }

        self.prompt_window(&ctx);

        let elapsed = started.elapsed();
        if elapsed.as_millis() >= 8 {
            tracing::debug!(ms = elapsed.as_millis(), "slow frame");
        }
    }

    fn on_exit(&mut self) {
        if let Some(error) = self.library.flush(true) {
            tracing::error!("{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draws one frame of `app` with Escape pressed.
    fn press_escape(ctx: &egui::Context, app: &mut ViewerApp) {
        use eframe::App as _;
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut frame = eframe::Frame::_new_kittest();
        let _ = ctx.run_ui(input, |ui| app.ui(ui, &mut frame));
    }

    #[test]
    fn escape_cancels_every_prompt() {
        let ctx = egui::Context::default();
        let cc = CreationContext::_new_kittest(ctx.clone());
        let mut app = ViewerApp::new(&cc, None, None);
        for prompt in [
            Prompt::UnsavedThenOpen(PathBuf::from("other.md")),
            Prompt::UnsavedThenQuit,
            Prompt::ChangedOnDisk,
        ] {
            app.prompt = Some(prompt.clone());
            app.jump = Some(Jump {
                path: PathBuf::from("other.md"),
                line: 1,
                column: 0,
                len: 1,
            });
            press_escape(&ctx, &mut app);
            assert_eq!(app.prompt, None, "{prompt:?}");
            assert_eq!(app.jump, None, "{prompt:?}");
            assert!(app.document.is_none() && !app.quit_confirmed, "{prompt:?}");
        }
    }

    #[test]
    fn escape_on_a_prompt_leaves_the_search_box_alone() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let ctx = egui::Context::default();
        let cc = CreationContext::_new_kittest(ctx.clone());
        let mut app = ViewerApp::new(&cc, None, Some(dir.path().to_path_buf()));
        for prompt in [None, Some(Prompt::ChangedOnDisk)] {
            app.search.query = "query".to_owned();
            app.prompt = None;
            let mut frame = eframe::Frame::_new_kittest();
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                use eframe::App as _;
                app.ui(ui, &mut frame);
            });
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("repo_search")));
            app.prompt = prompt.clone();
            press_escape(&ctx, &mut app);
            let expected = if prompt.is_some() { "query" } else { "" };
            assert_eq!(app.search.query, expected, "{prompt:?}");
            assert_eq!(app.prompt, None, "{prompt:?}");
        }
        Ok(())
    }

    #[test]
    fn a_picked_folder_opens_when_the_dialog_answers() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("foo"))?;
        let ctx = egui::Context::default();
        let cc = CreationContext::_new_kittest(ctx);
        let mut app = ViewerApp::new(&cc, None, None);
        let (tx, rx) = mpsc::channel();
        app.folder_pick = Some(rx);
        app.poll_folder_pick();
        assert!(app.folder_pick.is_some() && app.repo.is_none());
        let _ = tx.send(Some(dir.path().join("foo").join("..")));
        app.poll_folder_pick();
        assert!(app.folder_pick.is_none());
        let root = app.repo.as_ref().map(|r| r.root.clone());
        assert_eq!(root, Some(normalise_root(dir.path().to_path_buf())));
        Ok(())
    }

    #[test]
    fn a_dialog_that_dies_is_reported() {
        let ctx = egui::Context::default();
        let cc = CreationContext::_new_kittest(ctx);
        let mut app = ViewerApp::new(&cc, None, None);
        let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
        app.folder_pick = Some(rx);
        drop(tx);
        app.poll_folder_pick();
        assert!(app.folder_pick.is_none());
        assert_eq!(app.error.as_deref(), Some("the folder dialog failed"));
    }

    #[test]
    fn root_is_normalised() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("foo"))?;
        std::fs::create_dir(dir.path().join("folder"))?;
        let clean = normalise_root(dir.path().join("folder"));
        assert_eq!(
            normalise_root(dir.path().join("foo").join("..").join("folder")),
            clean
        );
        assert!(clean.is_absolute());
        assert!(!clean.to_string_lossy().starts_with(r"\\?\"));
        assert!(clean.components().all(|c| matches!(
            c,
            std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::Normal(_)
        )));
        Ok(())
    }

    #[test]
    fn a_share_is_named_plainly() {
        assert_eq!(
            plain_unc(Path::new(r"\\?\UNC\server\share\notes")),
            Some(PathBuf::from(r"\\server\share\notes"))
        );
        assert_eq!(plain_unc(Path::new(r"\\server\share\notes")), None);
        let at = |tail: String| plain_unc(Path::new(&format!(r"\\?\UNC\server\share\{tail}")));
        assert!(at("a".repeat(244)).is_some());
        assert_eq!(at("a".repeat(245)), None);
        assert_eq!(at(format!("{}😀", "a".repeat(243))), None);
    }

    #[test]
    fn unresolvable_root_is_kept() {
        let missing = PathBuf::from("no-such-dir").join("..").join("x");
        assert_eq!(normalise_root(missing.clone()), missing);
    }

    #[test]
    fn sizes_are_humanised() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
