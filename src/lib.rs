//! mdviewer: a repository-level markdown editor with a prompt library.
//!
//! `app` is the eframe application, `persist` the app's own files under the
//! data directory. Logic lives here rather than in `main.rs`.

mod app;
pub mod clock;
pub mod editor;
pub mod library;
mod library_pane;
pub mod persist;
pub mod search;
pub mod settings;
pub mod tree;

use anyhow::{Context as _, Result};
use eframe::NativeOptions;
use tracing_subscriber::EnvFilter;

use crate::app::ViewerApp;
use crate::persist::DataPaths;

/// Window title and the name eframe uses for its own storage.
const APP_NAME: &str = "mdviewer";

/// Runs the app: installs logging, opens the window, blocks until closed.
///
/// A single command-line argument is a repository to open on launch.
///
/// # Errors
///
/// Returns an error if eframe cannot create the window or its graphics
/// context.
pub fn run() -> Result<()> {
    init_tracing();

    let paths = DataPaths::from_project_dirs();
    let initial_repo = std::env::args_os().nth(1).map(std::path::PathBuf::from);

    let native_options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };

    tracing::info!("starting {APP_NAME}");

    eframe::run_native(
        APP_NAME,
        native_options,
        Box::new(move |cc| Ok(Box::new(ViewerApp::new(cc, paths, initial_repo)))),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))
    .context("failed to run mdviewer")
}

/// Levels come from `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
