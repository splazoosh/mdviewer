//! The one module that touches the app's own files on disk.
//!
//! Everything under the app's data directory — the library's files and the
//! settings — is written through here, and the JSON files are read through
//! here. The repository's markdown files are not: they belong to the user
//! and are handled by the editor.
//!
//! A missing, unreadable or malformed JSON file never keeps the app from
//! starting: [`read_or_backup`] yields the default and preserves a bad file
//! as a single rolling `.bak`. Only the writers are fallible, because a
//! failed write is worth surfacing.

use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use directories::BaseDirs;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The data directory's name under the platform's user data directory.
const DATA_DIR: &str = "mdviewer";
/// The data directory name of earlier Windows builds, if this build adopts
/// one.
const LEGACY_DATA_DIR: Option<&str> = if cfg!(windows) {
    option_env!("MDVIEWER_LEGACY_DATA_DIR")
} else {
    None
};
const LIBRARY_DIR: &str = "library";
const LEGACY_LIBRARY_FILE: &str = "library.json";
const SETTINGS_FILE: &str = "settings.json";

/// A resolved, existing data directory and the file paths inside it.
///
/// Only ever constructed for a directory that exists. When none can be
/// resolved the app holds `None` and runs on in-memory defaults for the
/// session.
#[derive(Debug, Clone)]
pub struct DataPaths {
    dir: PathBuf,
}

impl DataPaths {
    /// Resolves the data directory and ensures it exists: `%APPDATA%\mdviewer`
    /// on Windows, `$XDG_DATA_HOME/mdviewer` (by default
    /// `~/.local/share/mdviewer`) on Linux.
    ///
    /// On Windows the roaming profile, so the library follows the user's
    /// profile; the path carries only the application name. A Windows build
    /// configured with `MDVIEWER_LEGACY_DATA_DIR` first adopts that sibling
    /// directory ([`adopt_legacy`]). Returns `None` when there is no home
    /// directory, the adoption fails or the directory cannot be created; all
    /// are logged.
    pub fn from_project_dirs() -> Option<Self> {
        let base = BaseDirs::new().or_else(|| {
            tracing::warn!("no home directory found; persistence is disabled for this session");
            None
        })?;
        let dir = base.data_dir().join(DATA_DIR);
        if let Some(legacy) = LEGACY_DATA_DIR {
            let old = base.data_dir().join(legacy);
            match adopt_legacy(&old, &dir) {
                Ok(true) => {
                    tracing::info!(from = %old.display(), to = %dir.display(), "data directory copied from an earlier build")
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(from = %old.display(), %error, "could not copy the earlier data directory; persistence is disabled for this session");
                    return None;
                }
            }
        }
        match Self::at(&dir) {
            Ok(paths) => Some(paths),
            Err(error) => {
                tracing::warn!(
                    dir = %dir.display(),
                    %error,
                    "could not create the data directory; persistence is disabled for this session"
                );
                None
            }
        }
    }

    /// Roots the data at `dir`, creating it if needed. The testable seam.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created.
    pub fn at(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create data directory {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// The directory itself.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The prompt library directory.
    #[must_use]
    pub fn library(&self) -> PathBuf {
        self.dir.join(LIBRARY_DIR)
    }

    /// The JSON store of earlier builds, read once and carried over.
    #[must_use]
    pub fn legacy_library(&self) -> PathBuf {
        self.dir.join(LEGACY_LIBRARY_FILE)
    }

    /// The settings file.
    #[must_use]
    pub fn settings(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }
}

/// Copies the data directory `old` to `new` when `new` does not exist yet.
///
/// `old` is never modified. The copy is made in a sibling directory of this
/// process's own (`.partial-<pid>`) and renamed into place, so an interrupted
/// copy is redone on the next launch rather than taken for a finished one,
/// and two instances starting at once never fill or rename each other's.
/// Returns whether this call made the copy: `false` when `new` exists, `old`
/// is not a directory, or another instance's copy landed first.
///
/// # Errors
///
/// Returns an error if a file cannot be copied or the copy renamed.
pub fn adopt_legacy(old: &Path, new: &Path) -> Result<bool> {
    if fs::symlink_metadata(new).is_ok() || !old.is_dir() {
        return Ok(false);
    }
    let partial = sibling_with_suffix(new, &format!(".partial-{}", std::process::id()));
    if fs::symlink_metadata(&partial).is_ok() {
        fs::remove_dir_all(&partial)
            .with_context(|| format!("failed to clear {}", partial.display()))?;
    }
    let copied = copy_dir(old, &partial).and_then(|()| {
        fs::rename(&partial, new).with_context(|| {
            format!(
                "failed to rename {} to {}",
                partial.display(),
                new.display()
            )
        })
    });
    match copied {
        Ok(()) => Ok(true),
        Err(error) => {
            if let Err(cleanup) = fs::remove_dir_all(&partial) {
                tracing::warn!(path = %partial.display(), %cleanup, "could not remove an unfinished copy");
            }
            if fs::symlink_metadata(new).is_ok() {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

/// Copies the files and directories under `from` into a new `to`.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    let entries =
        fs::read_dir(from).with_context(|| format!("failed to read {}", from.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", from.display()))?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let kind = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", src.display()))?;
        if kind.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).with_context(|| {
                format!("failed to copy {} to {}", src.display(), dst.display())
            })?;
        }
    }
    Ok(())
}

/// Writes `value` to `path` as pretty JSON, atomically.
///
/// # Errors
///
/// Returns an error if `value` cannot be serialized or the write fails.
pub fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value).context("failed to serialize to JSON")?;
    text.push('\n');
    write_text_atomic(path, &text)
}

/// Writes `text` to `path`, atomically.
///
/// The write goes to a sibling `.tmp`, synced, then renamed over `path`, so a
/// crash mid-write leaves at worst a stale `.tmp` and never a half-written
/// file.
///
/// # Errors
///
/// Returns an error if the temp file cannot be written or renamed into place.
pub fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = sibling_with_suffix(path, ".tmp");
    let mut file =
        fs::File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Reads `path` as JSON, or returns the default and preserves a bad file.
///
/// Absent: the default, no `.bak`. Valid: the value. Unreadable or invalid:
/// renamed to a single rolling `.bak`, warned, and the default.
pub fn read_or_backup<T: DeserializeOwned + Default>(path: &Path) -> T {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return T::default(),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not read file; backing it up and starting from defaults"
            );
            back_up(path);
            return T::default();
        }
    };

    match serde_json::from_str::<T>(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "file is not valid JSON for its schema; backing it up and starting from defaults"
            );
            back_up(path);
            T::default()
        }
    }
}

/// Renames `path` aside to a single rolling `<name>.bak`, best effort.
pub fn back_up(path: &Path) {
    let bak = sibling_with_suffix(path, ".bak");
    match fs::rename(path, &bak) {
        Ok(()) => {
            tracing::warn!(from = %path.display(), to = %bak.display(), "preserved bad file")
        }
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "could not preserve the bad file"
        ),
    }
}

/// `library.json` + `.bak` → `library.json.bak` (an append, not
/// `with_extension`, which would drop `.json`).
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.into();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
    struct Sample {
        n: u32,
        label: String,
    }

    fn sample() -> Sample {
        Sample {
            n: 7,
            label: "written".to_owned(),
        }
    }

    fn temp_paths() -> (tempfile::TempDir, DataPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = DataPaths::at(dir.path()).unwrap();
        (dir, paths)
    }

    #[test]
    fn at_creates_a_missing_directory() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("does").join("not").join("exist");
        let paths = DataPaths::at(&nested).unwrap();
        assert!(nested.is_dir());
        assert_eq!(paths.library(), nested.join("library"));
        assert_eq!(paths.legacy_library(), nested.join("library.json"));
        assert_eq!(paths.settings(), nested.join("settings.json"));
    }

    #[test]
    fn a_legacy_directory_is_copied_and_left_in_place() {
        let root = tempfile::tempdir().unwrap();
        let (old, new) = (root.path().join("old"), root.path().join("new"));
        fs::create_dir_all(old.join("library")).unwrap();
        fs::write(old.join("settings.json"), "{\"schema\":1}").unwrap();
        fs::write(old.join("library").join("0001-a.md"), "a\r\n").unwrap();
        assert!(adopt_legacy(&old, &new).unwrap());
        for rel in ["settings.json", "library/0001-a.md"] {
            assert_eq!(
                fs::read(new.join(rel)).unwrap(),
                fs::read(old.join(rel)).unwrap()
            );
        }
        assert!(old.join("library").join("0001-a.md").exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn an_existing_directory_is_never_touched_by_adoption() {
        let root = tempfile::tempdir().unwrap();
        let (old, new) = (root.path().join("old"), root.path().join("new"));
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("settings.json"), "old").unwrap();
        fs::create_dir_all(&new).unwrap();
        assert!(!adopt_legacy(&old, &new).unwrap());
        assert!(!new.join("settings.json").exists());
        assert!(!adopt_legacy(&root.path().join("absent"), &root.path().join("x")).unwrap());
        assert!(!root.path().join("x").exists());
    }

    #[test]
    fn an_interrupted_adoption_is_redone() {
        let root = tempfile::tempdir().unwrap();
        let (old, new) = (root.path().join("old"), root.path().join("new"));
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("settings.json"), "old").unwrap();
        let partial = sibling_with_suffix(&new, &format!(".partial-{}", std::process::id()));
        fs::create_dir_all(&partial).unwrap();
        fs::write(partial.join("stale"), "x").unwrap();
        let theirs = sibling_with_suffix(&new, ".partial-0");
        fs::create_dir_all(&theirs).unwrap();
        fs::write(theirs.join("half"), "x").unwrap();
        assert!(adopt_legacy(&old, &new).unwrap());
        assert_eq!(
            fs::read_to_string(new.join("settings.json")).unwrap(),
            "old"
        );
        assert!(!new.join("stale").exists());
        assert!(!new.join("half").exists());
        assert!(theirs.join("half").exists());
    }

    #[test]
    fn a_failed_adoption_leaves_no_copy_behind() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old");
        fs::create_dir_all(old.join("library")).unwrap();
        fs::write(old.join("library").join("0001-a.md"), "a").unwrap();
        let unreadable = old.join("settings.json");
        #[cfg(windows)]
        let _held = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::write(&unreadable, "{}").unwrap();
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&unreadable)
                .unwrap()
        };
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("absent"), &unreadable).unwrap();
        let new = root.path().join("new");
        let partial = sibling_with_suffix(&new, &format!(".partial-{}", std::process::id()));
        let error = adopt_legacy(&old, &new).unwrap_err();
        assert!(format!("{error:#}").contains("failed to copy"), "{error:#}");
        assert!(!partial.exists());
        assert!(!new.exists());
    }

    #[test]
    fn a_written_value_reads_back_identically() {
        let (_dir, paths) = temp_paths();
        let path = paths.settings();
        write_atomic(&path, &sample()).unwrap();
        let read: Sample = read_or_backup(&path);
        assert_eq!(read, sample());
        assert!(!sibling_with_suffix(&path, ".tmp").exists());
    }

    #[test]
    fn a_missing_file_yields_the_default_and_leaves_no_backup() {
        let (_dir, paths) = temp_paths();
        let path = paths.settings();
        let read: Sample = read_or_backup(&path);
        assert_eq!(read, Sample::default());
        assert!(!sibling_with_suffix(&path, ".bak").exists());
    }

    #[test]
    fn a_corrupt_file_is_backed_up_and_reads_as_the_default() {
        let (_dir, paths) = temp_paths();
        let path = paths.settings();
        fs::write(&path, "not json {{{").unwrap();
        let read: Sample = read_or_backup(&path);
        assert_eq!(read, Sample::default());
        let bak = sibling_with_suffix(&path, ".bak");
        assert_eq!(fs::read_to_string(&bak).unwrap(), "not json {{{");
        assert!(!path.exists());
    }
}
