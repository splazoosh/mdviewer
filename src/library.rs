//! The prompt library: one directory of markdown files, one file per clip.
//!
//! `library/NNNN-slug.md` under the data directory
//! ([`crate::persist::DataPaths`]), where `NNNN` counts up from the
//! highest number already there and `slug` comes from the title at clip
//! time; a later retitle changes the front matter, not the file name, so a
//! path once referenced stays valid. The file is YAML front matter over the
//! clipped text:
//!
//! ```text
//! ---
//! title: First
//! source: C:\repo\notes.md
//! clipped: 2026-09-19
//! ---
//! ## First
//! body
//! ```
//!
//! There is no schema number: the shape is the contract. A hand edit that
//! keeps it loads; every key may be omitted (the title defaults to the first
//! line of the body), a value may be plain or double-quoted, front matter
//! lines that are not one of the three keys are kept and written back
//! verbatim, and a file with no front matter is all body. Nothing in the
//! directory is ever moved or rewritten except an entry the user edits or
//! deletes in the app; a file that is unreadable or not UTF-8 is skipped with
//! a warning and left where it is.
//!
//! A `library.json` beside the directory, the store of earlier builds, is
//! read once: each entry becomes a file, then the file is renamed
//! `library.json.migrated`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::persist::{read_or_backup, write_text_atomic};

/// One clipped snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file stem; unique within the store and stable across edits.
    pub id: String,
    /// Shown in the list; defaults to the first line of the text.
    pub title: String,
    /// The snippet.
    pub text: String,
    /// The file the text was clipped from, as an absolute path.
    pub source: String,
    /// `YYYY-MM-DD` of the clip.
    pub clipped: String,
    /// Front matter lines that are none of the keys above, kept verbatim.
    pub extra: Vec<String>,
}

impl Entry {
    /// Whether `needle` appears in the title, text or source, ignoring case.
    #[must_use]
    pub fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let needle = needle.to_lowercase();
        self.title.to_lowercase().contains(&needle)
            || self.text.to_lowercase().contains(&needle)
            || self.source.to_lowercase().contains(&needle)
    }

    /// Parses a file's text; never fails.
    #[must_use]
    pub fn parse(id: &str, raw: &str) -> Self {
        let raw = raw.replace("\r\n", "\n");
        let mut entry = Self {
            id: id.to_owned(),
            title: String::new(),
            text: String::new(),
            source: String::new(),
            clipped: String::new(),
            extra: Vec::new(),
        };
        let (front, body) = split_front_matter(&raw);
        entry.text = body.to_owned();
        for line in front.lines() {
            match line.split_once(':') {
                Some((key, value)) if matches!(key.trim(), "title" | "source" | "clipped") => {
                    let value = unquote(value.trim());
                    match key.trim() {
                        "title" => entry.title = value,
                        "source" => entry.source = value,
                        _ => entry.clipped = value,
                    }
                }
                _ => entry.extra.push(line.to_owned()),
            }
        }
        if entry.title.trim().is_empty() {
            entry.title = default_title(&entry.text);
        }
        entry
    }

    /// The file's text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("title: {}\n", quote_if_needed(&self.title)));
        out.push_str(&format!("source: {}\n", quote_if_needed(&self.source)));
        out.push_str(&format!("clipped: {}\n", quote_if_needed(&self.clipped)));
        for line in &self.extra {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(&self.text);
        out
    }
}

/// `(front matter lines, body)`. Without a closing `---` the whole text is
/// body.
fn split_front_matter(raw: &str) -> (&str, &str) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return ("", raw);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line == "---\n" || line == "---" {
            let front = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (front, body);
        }
        offset += line.len();
    }
    ("", raw)
}

/// A YAML scalar as a string: a double-quoted one with `\\`, `\"`, `\n`,
/// `\t` escapes, a single-quoted one with `''`, or the plain text.
fn unquote(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let mut out = String::new();
        let mut chars = value[1..value.len() - 1].chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        }
        out
    } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        value[1..value.len() - 1].replace("''", "'")
    } else {
        value.to_owned()
    }
}

/// Plain when a YAML reader would take it back as this string, double-quoted
/// otherwise.
fn quote_if_needed(value: &str) -> String {
    let plain_safe = !value.is_empty()
        && value.trim() == value
        && !value.contains('\n')
        && !value.contains(": ")
        && !value.ends_with(':')
        && !value.contains(" #")
        && !value.starts_with([
            '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%',
            '@', '`', '~',
        ])
        && !matches!(value, "true" | "false" | "null" | "yes" | "no")
        && !value.chars().all(|c| c.is_ascii_digit());
    if plain_safe {
        return value.to_owned();
    }
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The store.
#[derive(Debug)]
pub struct Library {
    /// `None` for a store that lives only in memory.
    dir: Option<PathBuf>,
    /// Oldest first.
    entries: Vec<Entry>,
    /// Ids with edits not yet written.
    dirty: BTreeSet<String>,
}

impl Library {
    /// A store that is never written.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            dir: None,
            entries: Vec::new(),
            dirty: BTreeSet::new(),
        }
    }

    /// Loads every `*.md` in `dir`, creating the directory if needed, then
    /// carries over `legacy` — the JSON store of earlier builds — if it
    /// exists.
    ///
    /// A directory that cannot be created or read leaves the store in
    /// memory for the session, with a warning.
    #[must_use]
    pub fn open(dir: &Path, legacy: Option<&Path>) -> Self {
        let mut library = Self::in_memory();
        if let Err(error) = fs::create_dir_all(dir) {
            tracing::warn!(dir = %dir.display(), %error, "cannot create the library directory; the library is in memory for this session");
            return library;
        }
        library.dir = Some(dir.to_path_buf());
        match fs::read_dir(dir) {
            Ok(read) => {
                for entry in read.flatten() {
                    let path = entry.path();
                    if !is_markdown(&path) {
                        continue;
                    }
                    let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned())
                    else {
                        continue;
                    };
                    match fs::read(&path).map(String::from_utf8) {
                        Ok(Ok(raw)) => library.entries.push(Entry::parse(&id, &raw)),
                        Ok(Err(_)) => {
                            tracing::warn!(path = %path.display(), "library skips non-UTF-8 file");
                        }
                        Err(error) => {
                            tracing::warn!(path = %path.display(), %error, "library skips unreadable file");
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(dir = %dir.display(), %error, "cannot read the library directory; the library is in memory for this session");
                library.dir = None;
                return library;
            }
        }
        library
            .entries
            .sort_by(|a, b| (&a.clipped, &a.id).cmp(&(&b.clipped, &b.id)));
        if let Some(legacy) = legacy.filter(|p| p.exists()) {
            library.migrate(legacy);
        }
        library
    }

    /// Writes every entry of the JSON store at `legacy` as a file, then
    /// renames it `.migrated`. An entry already present with the same text,
    /// source and date is not written twice, so a run cut short is safe to
    /// repeat.
    fn migrate(&mut self, legacy: &Path) {
        let store: LegacyStore = read_or_backup(legacy);
        let count = store.entries.len();
        for old in store.entries {
            let present = self
                .entries
                .iter()
                .any(|e| e.text == old.text && e.source == old.source && e.clipped == old.clipped);
            if present {
                continue;
            }
            let title = if old.title.trim().is_empty() {
                default_title(&old.text)
            } else {
                old.title
            };
            let id = self.add(title, old.text, old.source, old.clipped);
            if let Err(error) = self.write(&id) {
                tracing::error!(path = %legacy.display(), %error, "library migration stopped; the JSON store is kept for the next launch");
                return;
            }
        }
        let migrated = legacy.with_extension("json.migrated");
        if !legacy.exists() {
            return;
        }
        match fs::rename(legacy, &migrated) {
            Ok(()) => {
                tracing::info!(entries = count, to = %migrated.display(), "library migrated from JSON")
            }
            Err(error) => {
                tracing::warn!(path = %legacy.display(), %error, "library migrated but the JSON store could not be renamed")
            }
        }
    }

    /// Where the entry with `id` is stored.
    #[must_use]
    pub fn path_of(&self, id: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{id}.md")))
    }

    /// The directory, when the store is on disk.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Every entry, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn next_number(&self) -> u64 {
        self.entries
            .iter()
            .filter_map(|e| leading_number(&e.id))
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Whether `id` is taken: by an entry, or by a file on disk the load
    /// skipped.
    fn is_taken(&self, id: &str) -> bool {
        self.get(id).is_some() || self.path_of(id).is_some_and(|p| p.exists())
    }

    /// Appends an entry, marks it for writing, and returns its id.
    fn add(&mut self, title: String, text: String, source: String, clipped: String) -> String {
        let slug = slug(&title);
        let mut number = self.next_number();
        let id = loop {
            let id = if slug.is_empty() {
                format!("{number:04}")
            } else {
                format!("{number:04}-{slug}")
            };
            if !self.is_taken(&id) || number == u64::MAX {
                break id;
            }
            number += 1;
        };
        self.entries.push(Entry {
            id: id.clone(),
            title,
            text,
            source,
            clipped,
            extra: Vec::new(),
        });
        self.dirty.insert(id.clone());
        id
    }

    /// Appends a clip and returns its id.
    pub fn clip(&mut self, text: &str, source: &Path, clipped: &str) -> String {
        self.add(
            default_title(text),
            text.to_owned(),
            source.display().to_string(),
            clipped.to_owned(),
        )
    }

    /// Deletes the entry with `id` and its file; `Ok(false)` if there was
    /// none.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists and cannot be removed; the entry
    /// then stays.
    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            return Ok(false);
        };
        if let Some(path) = self.path_of(id)
            && let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            bail!("failed to delete {}: {error}", path.display());
        }
        self.entries.remove(index);
        self.dirty.remove(id);
        Ok(true)
    }

    /// The entry with `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// The entry with `id` for editing; it is written on the next
    /// [`Library::flush`].
    pub fn edit(&mut self, id: &str) -> Option<&mut Entry> {
        let entry = self.entries.iter_mut().find(|e| e.id == id)?;
        self.dirty.insert(id.to_owned());
        Some(entry)
    }

    /// Whether an edit or clip is waiting to be written.
    #[must_use]
    pub fn has_unsaved(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Writes every edited or clipped entry.
    ///
    /// # Errors
    ///
    /// Returns the first write that failed; that entry stays marked, the
    /// rest are written.
    pub fn flush(&mut self) -> Result<()> {
        let mut first_error = None;
        for id in std::mem::take(&mut self.dirty) {
            if let Err(error) = self.write(&id) {
                self.dirty.insert(id);
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn write(&mut self, id: &str) -> Result<()> {
        let Some(path) = self.path_of(id) else {
            return Ok(());
        };
        let Some(entry) = self.get(id) else {
            return Ok(());
        };
        write_text_atomic(&path, &entry.render())?;
        self.dirty.remove(id);
        Ok(())
    }

    /// Ids of the entries matching `needle`, newest first.
    #[must_use]
    pub fn search(&self, needle: &str) -> Vec<String> {
        self.entries
            .iter()
            .rev()
            .filter(|e| e.matches(needle))
            .map(|e| e.id.clone())
            .collect()
    }
}

/// The `library.json` of earlier builds; read for migration only.
#[derive(Debug, Default, Deserialize)]
struct LegacyStore {
    #[serde(default)]
    entries: Vec<LegacyEntry>,
}

#[derive(Debug, Deserialize)]
struct LegacyEntry {
    #[serde(default)]
    title: String,
    text: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    clipped: String,
}

/// `*.md`, case-insensitively.
fn is_markdown(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// The digits a file name starts with.
fn leading_number(id: &str) -> Option<u64> {
    let digits: String = id.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Lowercase alphanumeric runs joined by `-`, at most 40 characters.
fn slug(title: &str) -> String {
    const MAX: usize = 40;
    let mut out = String::new();
    let mut count = 0;
    let mut pending_dash = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if !c.is_alphanumeric() {
            pending_dash = true;
            continue;
        }
        let dash = pending_dash && !out.is_empty();
        if count + 1 + usize::from(dash) > MAX {
            break;
        }
        if dash {
            out.push('-');
            count += 1;
        }
        out.push(c);
        count += 1;
        pending_dash = false;
    }
    out
}

/// The first non-blank line, trimmed and capped, as a title.
fn default_title(text: &str) -> String {
    const MAX: usize = 60;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("untitled");
    let line = line.trim_start_matches(['#', '-', '*', '>', ' ']);
    if line.chars().count() > MAX {
        let cut: String = line.chars().take(MAX - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("library");
        (dir, lib)
    }

    #[test]
    fn clips_round_trip_through_files() {
        let (_dir, path) = store();
        let mut lib = Library::open(&path, None);
        let a = lib.clip("## First\nbody", Path::new("C:\\r\\a.md"), "2026-09-19");
        let b = lib.clip(
            "second: with \"quotes\"",
            Path::new("C:\\r\\b.md"),
            "2026-09-19",
        );
        assert!(lib.has_unsaved());
        lib.flush().unwrap();
        assert_eq!(a, "0001-first");
        assert_eq!(b, "0002-second-with-quotes");
        let written = fs::read_to_string(path.join("0001-first.md")).unwrap();
        assert_eq!(
            written,
            "---\ntitle: First\nsource: C:\\r\\a.md\nclipped: 2026-09-19\n---\n## First\nbody"
        );

        let loaded = Library::open(&path, None);
        assert_eq!(loaded.entries(), lib.entries());
        assert_eq!(loaded.search(""), vec![b.clone(), a.clone()]);
        assert_eq!(loaded.search("SECOND"), vec![b]);
        assert_eq!(loaded.search("a.md"), vec![a]);
        assert_eq!(loaded.search("nothing"), Vec::<String>::new());
    }

    #[test]
    fn a_hand_edit_that_keeps_the_shape_loads() {
        let (_dir, path) = store();
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("kept.md"),
            "---\r\ntitle: \"quoted: \\\"t\\\"\"\r\nsource: 's'\r\ntags: [a, b]\r\nclipped: 2026-01-02\r\n---\r\nbody\r\n",
        )
        .unwrap();
        fs::write(path.join("bare.md"), "no front matter\nsecond line\n").unwrap();
        fs::write(path.join("open.md"), "---\ntitle: never closed\nbody\n").unwrap();
        fs::write(path.join("notes.txt"), "ignored").unwrap();
        fs::write(path.join("bad.md"), [0xff, 0xfe, b'x']).unwrap();

        let mut lib = Library::open(&path, None);
        assert_eq!(
            lib.entries()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            vec!["bare", "open", "kept"]
        );
        let kept = lib.get("kept").unwrap();
        assert_eq!(kept.title, "quoted: \"t\"");
        assert_eq!(kept.source, "s");
        assert_eq!(kept.clipped, "2026-01-02");
        assert_eq!(kept.extra, vec!["tags: [a, b]"]);
        assert_eq!(kept.text, "body\n");
        assert_eq!(lib.get("bare").unwrap().title, "no front matter");
        assert_eq!(
            lib.get("open").unwrap().text,
            "---\ntitle: never closed\nbody\n"
        );
        assert!(path.join("bad.md").exists());

        lib.edit("kept").unwrap().title = "retitled".into();
        lib.flush().unwrap();
        assert_eq!(
            fs::read_to_string(path.join("kept.md")).unwrap(),
            "---\ntitle: retitled\nsource: s\nclipped: 2026-01-02\ntags: [a, b]\n---\nbody\n"
        );
        assert_eq!(
            fs::read_to_string(path.join("bare.md")).unwrap(),
            "no front matter\nsecond line\n"
        );
    }

    #[test]
    fn scalars_that_yaml_would_misread_are_quoted() {
        for value in [
            "",
            " lead",
            "a: b",
            "true",
            "123",
            "#x",
            "- item",
            "two\nlines",
            "end:",
        ] {
            let quoted = quote_if_needed(value);
            assert!(quoted.starts_with('"'), "{value:?} → {quoted}");
            assert_eq!(unquote(&quoted), value);
        }
        for value in [
            "plain",
            "C:\\repo\\a.md",
            "2026-09-19",
            "Title, with punctuation!",
            "with \"quote\"",
        ] {
            assert_eq!(quote_if_needed(value), value);
            assert_eq!(unquote(value), value);
        }
    }

    #[test]
    fn the_json_store_is_carried_over_once() {
        let (dir, path) = store();
        let legacy = dir.path().join("library.json");
        fs::write(
            &legacy,
            r#"{ "schema": 1, "entries": [
                { "id": 1, "title": "kept", "text": "t", "source": "s", "clipped": "2026-09-01" },
                { "text": "no id, no title\nsecond line" }
            ] }"#,
        )
        .unwrap();
        let lib = Library::open(&path, Some(&legacy));
        assert_eq!(lib.len(), 2);
        assert_eq!(lib.get("0001-kept").unwrap().text, "t");
        assert_eq!(
            lib.get("0002-no-id-no-title").unwrap().title,
            "no id, no title"
        );
        assert!(path.join("0001-kept.md").exists());
        assert!(!legacy.exists());
        assert!(dir.path().join("library.json.migrated").exists());

        fs::rename(dir.path().join("library.json.migrated"), &legacy).unwrap();
        let again = Library::open(&path, Some(&legacy));
        assert_eq!(again.len(), 2);
    }

    #[test]
    fn a_malformed_json_store_is_backed_up() {
        let (dir, path) = store();
        let legacy = dir.path().join("library.json");
        fs::write(&legacy, "not json").unwrap();
        let lib = Library::open(&path, Some(&legacy));
        assert!(lib.is_empty());
        assert!(dir.path().join("library.json.bak").exists());
    }

    #[test]
    fn removal_deletes_the_file() {
        let (_dir, path) = store();
        let mut lib = Library::open(&path, None);
        let id = lib.clip("x", Path::new("p"), "d");
        lib.flush().unwrap();
        assert!(path.join(format!("{id}.md")).exists());
        assert!(lib.remove(&id).unwrap());
        assert!(!lib.remove(&id).unwrap());
        assert!(!path.join(format!("{id}.md")).exists());
        assert!(lib.is_empty());
    }

    #[test]
    fn numbering_continues_past_hand_added_files() {
        let (_dir, path) = store();
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("0007-old.md"), "old").unwrap();
        fs::write(path.join("prompt.md"), "unnumbered").unwrap();
        let mut lib = Library::open(&path, None);
        assert_eq!(lib.clip("new", Path::new("p"), "d"), "0008-new");
    }

    #[test]
    fn a_clip_never_takes_the_name_of_a_skipped_file() {
        let (_dir, path) = store();
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("0001-notes.md"), [0xff, 0xfe]).unwrap();
        let mut lib = Library::open(&path, None);
        assert!(lib.is_empty());
        assert_eq!(lib.clip("notes", Path::new("p"), "d"), "0002-notes");
        lib.flush().unwrap();
        assert_eq!(fs::read(path.join("0001-notes.md")).unwrap(), [0xff, 0xfe]);
    }

    #[test]
    fn slugs_and_titles_are_derived() {
        assert_eq!(slug("Hello, World! 2"), "hello-world-2");
        assert_eq!(slug("---"), "");
        assert_eq!(slug(&"ab ".repeat(30)).chars().count(), 40);
        assert_eq!(slug("Blåbær"), "blåbær");
        assert_eq!(default_title("\n\n  - item\n"), "item");
        assert_eq!(default_title(""), "untitled");
        let title = default_title(&"a".repeat(80));
        assert_eq!(title.chars().count(), 60);
        assert!(title.ends_with('…'));
    }
}
