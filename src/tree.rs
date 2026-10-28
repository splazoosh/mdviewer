//! The repository tree: every `*.md` under the opened directory, with the
//! sort order the user chose applied at every level.
//!
//! The scan is synchronous and runs when a repository is opened or rescanned.
//! `.git`, `target`, `node_modules` and dot-directories are skipped; a
//! directory with no markdown anywhere below it is pruned. Only `*.md` files
//! are listed — other files are hidden, not greyed out.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Directory names never descended into.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// What the file list is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SortKey {
    /// File name, case-insensitive.
    #[default]
    Name,
    /// Size in bytes; a directory's size is the sum of its markdown files.
    Size,
    /// Last-modified time; a directory's is the newest below it.
    Modified,
}

impl SortKey {
    /// Every key, for the picker.
    pub const ALL: [SortKey; 3] = [SortKey::Name, SortKey::Size, SortKey::Modified];

    /// The label shown in the picker.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Size => "Size",
            SortKey::Modified => "Modified",
        }
    }
}

/// Ascending or descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SortDir {
    /// A before Z, small before large, old before new.
    #[default]
    Ascending,
    /// The reverse.
    Descending,
}

impl SortDir {
    /// The other direction.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            SortDir::Ascending => SortDir::Descending,
            SortDir::Descending => SortDir::Ascending,
        }
    }
}

/// Key and direction together: the one setting applied at every level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SortOrder {
    /// What to order by.
    pub key: SortKey,
    /// Which way.
    pub dir: SortDir,
}

/// A directory or a markdown file in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The last path component.
    pub name: String,
    /// The absolute path.
    pub path: PathBuf,
    /// Bytes for a file; the sum of markdown bytes below for a directory.
    pub size: u64,
    /// Modification time for a file; the newest below for a directory.
    pub modified: SystemTime,
    /// `Some` for a directory, `None` for a file.
    pub children: Option<Vec<Node>>,
}

impl Node {
    /// Whether this node is a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.children.is_some()
    }

    /// Walks `root` and builds the tree, unsorted.
    ///
    /// # Errors
    ///
    /// Returns an error if `root` cannot be read at all. Unreadable entries
    /// below it are skipped with a warning.
    pub fn scan(root: &Path) -> Result<Node> {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let mut node = scan_dir(root, name)
            .with_context(|| format!("failed to read {}", root.display()))?
            .unwrap_or_else(|| Node {
                name: root.display().to_string(),
                path: root.to_path_buf(),
                size: 0,
                modified: SystemTime::UNIX_EPOCH,
                children: Some(Vec::new()),
            });
        node.path = root.to_path_buf();
        Ok(node)
    }

    /// Sorts every level of the tree by `order`, directories first.
    pub fn sort(&mut self, order: SortOrder) {
        let Some(children) = &mut self.children else {
            return;
        };
        children.sort_by(|a, b| {
            let by_kind = b.is_dir().cmp(&a.is_dir());
            if by_kind != std::cmp::Ordering::Equal {
                return by_kind;
            }
            let by_key = match order.key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified.cmp(&b.modified),
            };
            let by_key = match order.dir {
                SortDir::Ascending => by_key,
                SortDir::Descending => by_key.reverse(),
            };
            by_key.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        for child in children {
            child.sort(order);
        }
    }

    /// The number of markdown files below this node.
    #[must_use]
    pub fn file_count(&self) -> usize {
        match &self.children {
            None => 1,
            Some(children) => children.iter().map(Node::file_count).sum(),
        }
    }
}

/// Reads one directory; `None` if nothing markdown lives below it.
fn scan_dir(dir: &Path, name: String) -> Result<Option<Node>> {
    let mut children = Vec::new();
    let mut size = 0;
    let mut modified = SystemTime::UNIX_EPOCH;

    for entry in fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(dir = %dir.display(), %error, "skipping unreadable entry");
                continue;
            }
        };
        let path = entry.path();
        let child_name = entry.file_name().to_string_lossy().into_owned();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        let child = if file_type.is_dir() {
            if child_name.starts_with('.') || SKIPPED_DIRS.contains(&child_name.as_str()) {
                continue;
            }
            match scan_dir(&path, child_name) {
                Ok(Some(node)) => node,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(dir = %path.display(), %error, "skipping unreadable directory");
                    continue;
                }
            }
        } else if file_type.is_file() && is_markdown(&path) {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            Node {
                name: child_name,
                path,
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                children: None,
            }
        } else {
            continue;
        };

        size += child.size;
        modified = modified.max(child.modified);
        children.push(child);
    }

    if children.is_empty() {
        return Ok(None);
    }
    Ok(Some(Node {
        name,
        path: dir.to_path_buf(),
        size,
        modified,
        children: Some(children),
    }))
}

/// `*.md`, case-insensitively.
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("b.md"), "bb").unwrap();
        fs::write(root.join("A.md"), "a").unwrap();
        fs::write(root.join("notes.txt"), "not markdown").unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs").join("plan.MD"), "plan!").unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git").join("x.md"), "hidden").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target").join("y.md"), "skipped").unwrap();
        dir
    }

    fn names(node: &Node) -> Vec<&str> {
        node.children
            .as_ref()
            .unwrap()
            .iter()
            .map(|n| n.name.as_str())
            .collect()
    }

    #[test]
    fn scan_lists_markdown_only_and_prunes_skipped_and_empty_dirs() {
        let dir = fixture();
        let mut tree = Node::scan(dir.path()).unwrap();
        tree.sort(SortOrder::default());
        assert_eq!(names(&tree), vec!["docs", "A.md", "b.md"]);
        assert_eq!(tree.file_count(), 3);
        assert_eq!(tree.size, 2 + 1 + 5);
    }

    #[test]
    fn sort_by_size_descending_keeps_directories_first() {
        let dir = fixture();
        let mut tree = Node::scan(dir.path()).unwrap();
        tree.sort(SortOrder {
            key: SortKey::Size,
            dir: SortDir::Descending,
        });
        assert_eq!(names(&tree), vec!["docs", "b.md", "A.md"]);
    }

    #[test]
    fn sort_by_modified_uses_the_file_times() {
        let dir = fixture();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let f = fs::File::options()
            .write(true)
            .open(dir.path().join("b.md"))
            .unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let mut tree = Node::scan(dir.path()).unwrap();
        tree.sort(SortOrder {
            key: SortKey::Modified,
            dir: SortDir::Ascending,
        });
        assert_eq!(names(&tree), vec!["docs", "b.md", "A.md"]);
    }

    #[test]
    fn a_directory_without_markdown_scans_to_an_empty_root() {
        let dir = tempfile::tempdir().unwrap();
        let tree = Node::scan(dir.path()).unwrap();
        assert_eq!(tree.file_count(), 0);
        assert_eq!(tree.path, dir.path());
    }
}
