//! What the app remembers between launches: the last repository and the
//! sort order. `settings.json` under the data directory
//! ([`crate::persist::DataPaths`]), schema 1.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::persist::{back_up, read_or_backup, write_atomic};
use crate::tree::SortOrder;

/// The settings format this build reads and writes.
pub const SCHEMA: u32 = 1;

/// The persisted settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// See [`SCHEMA`].
    pub schema: u32,
    /// Reopened on launch when no repository is given on the command line.
    #[serde(default)]
    pub last_repo: Option<PathBuf>,
    /// The tree's sort order.
    #[serde(default)]
    pub sort: SortOrder,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            last_repo: None,
            sort: SortOrder::default(),
        }
    }
}

impl Settings {
    /// Loads the settings at `path`, or the defaults; another schema is
    /// backed up and replaced.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let settings: Settings = read_or_backup(path);
        if settings.schema != SCHEMA {
            tracing::warn!(path = %path.display(), found = settings.schema, "settings schema mismatch");
            back_up(path);
            return Self::default();
        }
        settings
    }

    /// Writes the settings to `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{SortDir, SortKey};

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = Settings {
            schema: SCHEMA,
            last_repo: Some(PathBuf::from("C:\\r")),
            sort: SortOrder {
                key: SortKey::Modified,
                dir: SortDir::Descending,
            },
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        assert_eq!(
            Settings::load(&dir.path().join("none.json")),
            Settings::default()
        );
    }
}
