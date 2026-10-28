//! The open document: a text buffer bound to a file on disk.
//!
//! Line endings are the one thing the buffer does not hold verbatim: egui's
//! text widget has no notion of `\r`, so a file whose first line break is
//! `\r\n` is normalised to `\n` on load and every line break is written back
//! as `\r\n` on save. A file with uniform endings round-trips byte-for-byte;
//! a file with mixed endings comes out uniform in whichever it opened with,
//! and [`Document::has_mixed_endings`] says so until the save that converts it.
//! Everything else — trailing newline or none, trailing whitespace, BOM-free
//! content — is written exactly as the buffer holds it.
//!
//! A save writes the bytes to a sibling named `<file name>.mdviewer-save.tmp`
//! (see [`temp_path`]; for a symbolic link, beside its target), flushes it to
//! disk and renames it over the file, so an
//! interrupted save leaves the old file whole. Only a crash or power loss
//! between the write and the rename leaves that sibling behind, and it is the
//! one file the app writes under a repository that the user did not save.
//! The next save of the same file reuses and replaces it; the tree and search
//! list only `*.md`, so they never show it. The rename gives the file a new
//! identity: hard links to the old file keep the old bytes. On Windows,
//! permissions set on the file itself, rather than inherited from its
//! directory, are lost, and so are its hidden and system attributes. On Unix
//! the permission bits are kept and the file is owned by whoever saved it.

use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result};

/// How a file's line breaks were written on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n`.
    Lf,
    /// `\r\n`.
    CrLf,
}

impl LineEnding {
    /// The convention of the first line break in `text`; `Lf` if there is
    /// none.
    #[must_use]
    pub fn detect(text: &str) -> Self {
        match text.find('\n') {
            Some(i) if i > 0 && text.as_bytes()[i - 1] == b'\r' => LineEnding::CrLf,
            _ => LineEnding::Lf,
        }
    }
}

/// What the file looked like when the buffer last agreed with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl DiskStamp {
    fn read(path: &Path) -> std::io::Result<Self> {
        let meta = fs::metadata(path)?;
        Ok(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

/// Appended to a file's name to name the sibling a save writes first.
pub const TEMP_SUFFIX: &str = ".mdviewer-save.tmp";

/// The sibling a save of `path` writes before renaming it over `path`.
#[must_use]
pub fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(TEMP_SUFFIX);
    path.with_file_name(name)
}

/// Refuses with [`SaveError::ChangedOnDisk`] unless `path` still matches
/// `stamp`. A file that vanished is not a conflict; the save recreates it.
fn check_unchanged(path: &Path, stamp: DiskStamp) -> std::result::Result<(), SaveError> {
    match DiskStamp::read(path) {
        Ok(now) if now == stamp => Ok(()),
        Ok(_) => Err(SaveError::ChangedOnDisk),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SaveError::Io(
            anyhow::Error::new(error).context(format!("failed to stat {}", path.display())),
        )),
    }
}

/// Writes `bytes` to [`temp_path`], flushed to disk, runs `before_rename`,
/// then renames the temp file over `path`. On any failure the temp file is
/// removed and `path` is untouched.
fn replace_file(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce() -> std::result::Result<(), SaveError>,
) -> std::result::Result<(), SaveError> {
    let resolved;
    let path = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            resolved = fs::canonicalize(path).map_err(|error| {
                SaveError::Io(
                    anyhow::Error::new(error)
                        .context(format!("failed to resolve {}", path.display())),
                )
            })?;
            resolved.as_path()
        }
        _ => path,
    };
    check_writable(path)?;
    let tmp = temp_path(path);
    discard_link(&tmp).map_err(|error| {
        SaveError::Io(
            anyhow::Error::new(error).context(format!("failed to remove {}", tmp.display())),
        )
    })?;
    let mut file = open_temp(&tmp).map_err(|error| {
        SaveError::Io(
            anyhow::Error::new(error).context(format!("failed to create {}", tmp.display())),
        )
    })?;
    let replaced = keep_mode(&file, path)
        .and_then(|()| write_synced(&mut file, &tmp, bytes).map_err(SaveError::Io))
        .and_then(|()| before_rename())
        .and_then(|()| {
            fs::rename(&tmp, path).map_err(|error| {
                SaveError::Io(
                    anyhow::Error::new(error)
                        .context(format!("failed to replace {}", path.display())),
                )
            })
        });
    if replaced.is_err() {
        let _ = fs::remove_file(&tmp);
    } else {
        sync_parent(path);
    }
    drop(file);
    replaced
}

/// Removes a link, symbolic or hard, that has the temp name `tmp`: opening it
/// would write another file. The next save replaces whatever has the name.
///
/// Windows has no stable link count, so whatever has the name is removed
/// unless a handle holds it, as another save's does; that save is then
/// refused here.
#[cfg(windows)]
fn discard_link(tmp: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const DELETE: u32 = 0x0001_0000;
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    match fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(tmp)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes a link, symbolic or hard, that has the temp name `tmp`: opening it
/// would write another file. The next save replaces whatever has the name.
#[cfg(unix)]
fn discard_link(tmp: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    match fs::symlink_metadata(tmp) {
        Ok(meta) if meta.file_type().is_symlink() || meta.nlink() > 1 => fs::remove_file(tmp),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Opens `tmp` empty for writing, held until the returned handle is dropped:
/// another save of the same file (a second instance) cannot use it
/// meanwhile, and the rename can.
#[cfg(windows)]
fn open_temp(tmp: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    /// `FILE_SHARE_DELETE`, from the Windows SDK.
    const FILE_SHARE_DELETE: u32 = 0x4;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(FILE_SHARE_DELETE)
        .open(tmp)
}

/// Opens `tmp` empty for writing, held until the returned handle is dropped:
/// another save of the same file (a second instance) cannot use it
/// meanwhile, and the rename can.
#[cfg(unix)]
fn open_temp(tmp: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::MetadataExt as _;
    #[expect(
        clippy::suspicious_open_options,
        reason = "truncated after the lock is taken"
    )]
    let file = fs::OpenOptions::new().write(true).create(true).open(tmp)?;
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => std::io::Error::new(
            ErrorKind::WouldBlock,
            "another save of this file is in progress",
        ),
        fs::TryLockError::Error(error) => error,
    })?;
    let (held, named) = (file.metadata()?, fs::symlink_metadata(tmp)?);
    if (held.dev(), held.ino()) != (named.dev(), named.ino()) {
        return Err(std::io::Error::new(
            ErrorKind::WouldBlock,
            "another save of this file is in progress",
        ));
    }
    file.set_len(0)?;
    Ok(file)
}

/// Refuses to replace a file the saver may not write. The rename needs only
/// the directory's permission on Unix; Windows' rename refuses a read-only
/// file itself.
fn check_writable(path: &Path) -> std::result::Result<(), SaveError> {
    if cfg!(unix) {
        match fs::OpenOptions::new().write(true).open(path) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(SaveError::Io(
                    anyhow::Error::new(error)
                        .context(format!("failed to open {} for writing", path.display())),
                ));
            }
        }
    }
    Ok(())
}

/// Gives the temp file `file` the permission bits of `path`, if it exists, so
/// the rename keeps them. Windows keeps nothing: a file's ACL is inherited.
fn keep_mode(file: &fs::File, path: &Path) -> std::result::Result<(), SaveError> {
    if cfg!(unix)
        && let Ok(meta) = fs::metadata(path)
    {
        file.set_permissions(meta.permissions()).map_err(|error| {
            SaveError::Io(anyhow::Error::new(error).context(format!(
                "failed to copy the permissions of {}",
                path.display()
            )))
        })?;
    }
    Ok(())
}

/// Flushes the directory entry the rename changed, best effort: the file is
/// already whole either way, and Windows has no handle for it.
fn sync_parent(path: &Path) {
    if cfg!(unix)
        && let Some(dir) = path.parent()
        && let Err(error) = fs::File::open(dir).and_then(|d| d.sync_all())
    {
        tracing::debug!(dir = %dir.display(), %error, "could not flush the directory");
    }
}

/// Writes `bytes` to `file` (named `path`, for errors) and waits for them to
/// reach the disk, so the rename that follows never exposes unwritten data.
fn write_synced(file: &mut fs::File, path: &Path, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to flush {}", path.display()))?;
    Ok(())
}

/// Why a save was refused.
#[derive(Debug)]
pub enum SaveError {
    /// The file on disk is not the one the buffer was loaded from or last
    /// saved to. Nothing was written; the caller decides whether to force it.
    ChangedOnDisk,
    /// The write itself failed.
    Io(anyhow::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::ChangedOnDisk => write!(f, "the file changed on disk since it was opened"),
            SaveError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SaveError::ChangedOnDisk => None,
            SaveError::Io(error) => error.source(),
        }
    }
}

/// A file open in the editor.
#[derive(Debug)]
pub struct Document {
    path: PathBuf,
    /// The editable buffer, `\n`-terminated lines.
    pub text: String,
    /// The buffer as it was at load or last save; the dirty check.
    saved_text: String,
    ending: LineEnding,
    /// The file on disk mixes line endings, so the next save changes more
    /// than the buffer's edits.
    mixed_endings: bool,
    stamp: DiskStamp,
}

impl Document {
    /// Reads `path` into a buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or is not UTF-8.
    pub fn open(path: &Path) -> Result<Self> {
        let stamp =
            DiskStamp::read(path).with_context(|| format!("failed to stat {}", path.display()))?;
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let raw =
            String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
        let ending = LineEnding::detect(&raw);
        let text = match ending {
            LineEnding::Lf => raw.clone(),
            LineEnding::CrLf => raw.replace("\r\n", "\n"),
        };
        let mut doc = Self {
            path: path.to_path_buf(),
            saved_text: text.clone(),
            text,
            ending,
            mixed_endings: false,
            stamp,
        };
        doc.mixed_endings = doc.bytes_to_write() != raw.as_bytes();
        Ok(doc)
    }

    /// Whether the file on disk mixes line endings that the next save will
    /// make uniform. Cleared by that save.
    #[must_use]
    pub fn has_mixed_endings(&self) -> bool {
        self.mixed_endings
    }

    /// The file this buffer belongs to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file name for a title.
    #[must_use]
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Whether the buffer differs from what is on disk.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.text != self.saved_text
    }

    /// The bytes a save would write.
    #[must_use]
    pub fn bytes_to_write(&self) -> Vec<u8> {
        match self.ending {
            LineEnding::Lf => self.text.clone().into_bytes(),
            LineEnding::CrLf => self.text.replace('\n', "\r\n").into_bytes(),
        }
    }

    /// Writes the buffer to its file, unless the file changed underneath it.
    ///
    /// # Errors
    ///
    /// [`SaveError::ChangedOnDisk`] if the file's size or modification time no
    /// longer match what was loaded; [`SaveError::Io`] if the write fails.
    pub fn save(&mut self) -> std::result::Result<(), SaveError> {
        check_unchanged(&self.path, self.stamp)?;
        let (path, stamp) = (self.path.clone(), self.stamp);
        self.replace(move || check_unchanged(&path, stamp))
    }

    /// Writes the buffer to its file whatever is there now.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn save_regardless(&mut self) -> Result<()> {
        self.replace(|| Ok(())).map_err(anyhow::Error::new)
    }

    fn replace(
        &mut self,
        before_rename: impl FnOnce() -> std::result::Result<(), SaveError>,
    ) -> std::result::Result<(), SaveError> {
        replace_file(&self.path, &self.bytes_to_write(), before_rename)?;
        self.stamp = DiskStamp::read(&self.path).map_err(|error| {
            SaveError::Io(
                anyhow::Error::new(error)
                    .context(format!("failed to stat {}", self.path.display())),
            )
        })?;
        self.saved_text = self.text.clone();
        self.mixed_endings = false;
        Ok(())
    }

    /// Discards the buffer and reads the file again.
    ///
    /// # Errors
    ///
    /// As [`Document::open`].
    pub fn reload(&mut self) -> Result<()> {
        *self = Self::open(&self.path)?;
        Ok(())
    }

    /// The characters in `range` (char indices, as egui's cursor reports).
    #[must_use]
    pub fn slice_chars(&self, range: std::ops::Range<usize>) -> String {
        self.text
            .chars()
            .skip(range.start)
            .take(range.end.saturating_sub(range.start))
            .collect()
    }

    /// The line ending the file uses.
    #[must_use]
    pub fn line_ending(&self) -> LineEnding {
        self.ending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with(bytes: &[u8]) -> (tempfile::TempDir, Document) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.md");
        fs::write(&path, bytes).unwrap();
        let doc = Document::open(&path).unwrap();
        (dir, doc)
    }

    #[test]
    fn lf_file_round_trips_byte_for_byte() {
        let raw = b"# t\n\nbody\nno trailing newline";
        let (_dir, mut doc) = doc_with(raw);
        assert!(!doc.is_dirty());
        doc.save().unwrap();
        assert_eq!(fs::read(doc.path()).unwrap(), raw);
    }

    #[test]
    fn crlf_file_round_trips_and_edits_keep_crlf() {
        let raw = b"a\r\nb\r\n";
        let (_dir, mut doc) = doc_with(raw);
        assert_eq!(doc.line_ending(), LineEnding::CrLf);
        assert_eq!(doc.text, "a\nb\n");
        doc.save().unwrap();
        assert_eq!(fs::read(doc.path()).unwrap(), raw);
        doc.text.push_str("c\n");
        assert!(doc.is_dirty());
        doc.save().unwrap();
        assert_eq!(fs::read(doc.path()).unwrap(), b"a\r\nb\r\nc\r\n");
        assert!(!doc.is_dirty());
    }

    #[test]
    fn only_the_typed_bytes_change() {
        let raw = b"line one\nline two  \n\n\n";
        let (_dir, mut doc) = doc_with(raw);
        doc.text.insert(5, 'X');
        doc.save().unwrap();
        assert_eq!(
            fs::read(doc.path()).unwrap(),
            b"line Xone\nline two  \n\n\n"
        );
    }

    #[test]
    fn a_file_changed_on_disk_is_not_overwritten() {
        let (_dir, mut doc) = doc_with(b"original\n");
        fs::write(doc.path(), b"someone else wrote this\n").unwrap();
        doc.text.push_str("mine\n");
        assert!(matches!(doc.save(), Err(SaveError::ChangedOnDisk)));
        assert_eq!(fs::read(doc.path()).unwrap(), b"someone else wrote this\n");
        doc.save_regardless().unwrap();
        assert_eq!(fs::read(doc.path()).unwrap(), b"original\nmine\n");
        doc.reload().unwrap();
        assert_eq!(doc.text, "original\nmine\n");
    }

    #[test]
    fn a_save_error_keeps_its_cause_as_the_source() {
        let cause = std::io::Error::other("disk full");
        let error = SaveError::Io(anyhow::Error::new(cause).context("failed to write x.md"));
        assert_eq!(error.to_string(), "failed to write x.md");
        assert_eq!(
            format!("{:#}", anyhow::Error::new(error)),
            "failed to write x.md: disk full"
        );
    }

    #[test]
    fn a_file_deleted_on_disk_is_written_again() {
        let (_dir, mut doc) = doc_with(b"kept\n");
        fs::remove_file(doc.path()).unwrap();
        doc.save().unwrap();
        assert_eq!(fs::read(doc.path()).unwrap(), b"kept\n");
    }

    #[test]
    fn selection_is_by_char_index() {
        let (_dir, doc) = doc_with("héllo wörld".as_bytes());
        assert_eq!(doc.slice_chars(1..5), "éllo");
        assert_eq!(doc.slice_chars(9..99), "ld");
    }

    #[test]
    fn mixed_endings_are_flagged_until_the_save_that_converts_them() {
        for uniform in [&b"a\r\nb\r\n"[..], b"a\nb\n", b"a\nb\r\n"] {
            assert!(!doc_with(uniform).1.has_mixed_endings());
        }
        let (_dir, mut doc) = doc_with(b"a\r\nb\n");
        assert!(doc.has_mixed_endings());
        doc.save().unwrap();
        assert!(!doc.has_mixed_endings());
        assert_eq!(fs::read(doc.path()).unwrap(), b"a\r\nb\r\n");
    }

    #[test]
    fn a_save_leaves_no_temp_file() {
        let (dir, mut doc) = doc_with(b"a\n");
        doc.text.push_str("b\n");
        doc.save().unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["f.md"]);
        assert_eq!(
            temp_path(doc.path()),
            dir.path().join("f.md.mdviewer-save.tmp")
        );
    }

    #[test]
    fn a_save_failing_before_the_rename_leaves_the_file_intact() {
        let (_dir, doc) = doc_with(b"original\n");
        let result = replace_file(doc.path(), b"half", || {
            assert_eq!(fs::metadata(temp_path(doc.path())).unwrap().len(), 4);
            Err(SaveError::Io(anyhow::anyhow!("power lost")))
        });
        assert!(matches!(result, Err(SaveError::Io(_))));
        assert_eq!(fs::read(doc.path()).unwrap(), b"original\n");
        assert!(!temp_path(doc.path()).exists());
    }

    #[test]
    fn a_save_that_cannot_write_its_temp_file_leaves_the_file_intact() {
        let (_dir, mut doc) = doc_with(b"original\n");
        fs::create_dir(temp_path(doc.path())).unwrap();
        doc.text.push_str("mine\n");
        assert!(matches!(doc.save(), Err(SaveError::Io(_))));
        assert_eq!(fs::read(doc.path()).unwrap(), b"original\n");
        assert!(doc.is_dirty());
    }

    #[test]
    fn a_write_during_the_save_is_not_overwritten() {
        let (_dir, doc) = doc_with(b"original\n");
        let (path, stamp) = (doc.path().to_path_buf(), doc.stamp);
        let result = replace_file(doc.path(), b"mine\n", || {
            fs::write(&path, b"someone else wrote this\n").unwrap();
            check_unchanged(&path, stamp)
        });
        assert!(matches!(result, Err(SaveError::ChangedOnDisk)));
        assert_eq!(fs::read(doc.path()).unwrap(), b"someone else wrote this\n");
        assert!(!temp_path(doc.path()).exists());
    }

    #[test]
    fn a_second_save_of_the_same_file_cannot_touch_the_first_ones_temp_file() {
        let (_dir, doc) = doc_with(b"original\n");
        let path = doc.path().to_path_buf();
        let result = replace_file(doc.path(), b"first\n", || {
            let second = replace_file(&path, b"sec", || Ok(()));
            assert!(matches!(second, Err(SaveError::Io(_))));
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(fs::read(doc.path()).unwrap(), b"first\n");
        assert!(!temp_path(doc.path()).exists());
    }

    #[test]
    fn a_save_through_a_symbolic_link_writes_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.md");
        let link = dir.path().join("f.md");
        fs::write(&target, b"a\n").unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &link) {
            assert_eq!(error.raw_os_error(), Some(1314), "{error}");
            return;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut doc = Document::open(&link).unwrap();
        doc.text.push_str("b\n");
        doc.save().unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"a\nb\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_save_keeps_the_permission_bits() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, mut doc) = doc_with(b"a\n");
        fs::set_permissions(doc.path(), fs::Permissions::from_mode(0o600)).unwrap();
        doc.text.push_str("b\n");
        doc.save().unwrap();
        let mode = fs::metadata(doc.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_is_not_saved() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, mut doc) = doc_with(b"a\n");
        fs::set_permissions(doc.path(), fs::Permissions::from_mode(0o444)).unwrap();
        if fs::OpenOptions::new().write(true).open(doc.path()).is_ok() {
            return;
        }
        doc.text.push_str("b\n");
        assert!(matches!(doc.save(), Err(SaveError::Io(_))));
        assert_eq!(fs::read(doc.path()).unwrap(), b"a\n");
        assert!(!temp_path(doc.path()).exists());
        assert!(doc.is_dirty());
    }

    #[test]
    fn a_link_at_the_temp_path_is_removed_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        let (existing, absent) = (outside.path().join("kept"), outside.path().join("absent"));
        fs::write(&existing, b"kept").unwrap();
        for target in [&existing, &absent] {
            let (_dir, mut doc) = doc_with(b"a\n");
            let tmp = temp_path(doc.path());
            #[cfg(windows)]
            if let Err(error) = std::os::windows::fs::symlink_file(target, &tmp) {
                assert_eq!(error.raw_os_error(), Some(1314), "{error}");
                return;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &tmp).unwrap();
            doc.text.push_str("b\n");
            doc.save().unwrap();
            assert_eq!(fs::read(doc.path()).unwrap(), b"a\nb\n");
            assert!(fs::symlink_metadata(&tmp).is_err());
        }
        assert_eq!(fs::read(&existing).unwrap(), b"kept");
        assert!(!absent.exists());
    }

    #[test]
    fn a_hard_link_at_the_temp_path_is_removed_not_written() {
        let (dir, mut doc) = doc_with(b"a\n");
        let other = dir.path().join("other.txt");
        fs::write(&other, b"kept").unwrap();
        fs::hard_link(&other, temp_path(doc.path())).unwrap();
        doc.text.push_str("b\n");
        doc.save().unwrap();
        assert_eq!(fs::read(doc.path()).unwrap(), b"a\nb\n");
        assert_eq!(fs::read(&other).unwrap(), b"kept");
        assert!(!temp_path(doc.path()).exists());
    }

    #[test]
    fn a_leftover_temp_file_is_replaced_by_the_next_save() {
        let (_dir, mut doc) = doc_with(b"a\n");
        fs::write(temp_path(doc.path()), b"from a crashed save").unwrap();
        doc.save().unwrap();
        assert_eq!(fs::read(doc.path()).unwrap(), b"a\n");
        assert!(!temp_path(doc.path()).exists());
    }
}
