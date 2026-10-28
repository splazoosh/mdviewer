//! Thin entry point; the logic lives in the library.
//!
//! `windows_subsystem` is a crate-level attribute on the binary, so it sits
//! here. Gated to non-debug builds so a release build raises no console window
//! while debug builds keep the console for the `tracing` stream.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;

fn main() -> Result<()> {
    mdviewer::run()
}
