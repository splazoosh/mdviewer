//! Full-text search over the open repository's markdown files.
//!
//! The corpus is the files the tree lists, held in memory with a lowercase
//! copy of each, so a query is one substring scan per file and costs
//! milliseconds on a few megabytes. [`Corpus::refresh`] follows the tree:
//! a file whose size and modification time are unchanged is kept, the rest
//! are read again. Nothing is written anywhere.
//!
//! Matching is a case-insensitive substring match; hits come back in tree
//! order, then line order, capped at a count the caller chooses.

use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::tree::Node;

/// One file's text as read from disk.
#[derive(Debug)]
struct FileText {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    text: String,
    /// `text.to_lowercase()`, kept only when it has the same byte length as
    /// `text`, so byte offsets in one are byte offsets in the other. The rare
    /// file where lowercasing changes the length is searched line by line.
    lower: Option<String>,
}

/// One match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The file.
    pub path: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// 0-based character offset of the match within the line.
    pub column: usize,
    /// Characters in the match.
    pub len: usize,
    /// The line's text without its line break.
    pub text: String,
    /// Byte range of the match within `text`.
    pub range: Range<usize>,
}

/// What a query found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Results {
    /// The first `limit` hits, in tree order then line order.
    pub hits: Vec<Hit>,
    /// Every hit, counted past the limit.
    pub total: usize,
    /// Files with at least one hit.
    pub files: usize,
}

/// The searchable text of a repository.
#[derive(Debug, Default)]
pub struct Corpus {
    files: Vec<FileText>,
}

impl Corpus {
    /// Brings the corpus in line with `tree`: files are ordered as the tree
    /// lists them, unchanged files keep their text, changed or new files are
    /// read, files no longer listed are dropped.
    ///
    /// A file that cannot be read or is not UTF-8 is left out with a warning.
    pub fn refresh(&mut self, tree: &Node) {
        let mut previous = std::mem::take(&mut self.files);
        let mut files = Vec::with_capacity(previous.len());
        visit_files(tree, &mut |node| {
            let kept = previous
                .iter()
                .position(|f| {
                    f.path == node.path && f.size == node.size && f.modified == node.modified
                })
                .map(|i| previous.swap_remove(i));
            let file = kept.or_else(|| read_file(node));
            files.extend(file);
        });
        self.files = files;
    }

    /// Bytes of text held.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.files.iter().map(|f| f.text.len()).sum()
    }

    /// Files held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Finds `query`, ignoring case, keeping at most `limit` hits.
    ///
    /// An empty or blank query finds nothing.
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Results {
        let mut results = Results::default();
        if query.trim().is_empty() {
            return results;
        }
        let needle = query.to_lowercase();
        for file in &self.files {
            let before = results.total;
            match &file.lower {
                Some(lower) => search_aligned(file, lower, &needle, limit, &mut results),
                None => search_by_line(file, &needle, limit, &mut results),
            }
            if results.total != before {
                results.files += 1;
            }
        }
        results
    }
}

/// Every file node below `node`, in tree order.
fn visit_files(node: &Node, f: &mut impl FnMut(&Node)) {
    match &node.children {
        Some(children) => children.iter().for_each(|c| visit_files(c, f)),
        None => f(node),
    }
}

fn read_file(node: &Node) -> Option<FileText> {
    let bytes = match fs::read(&node.path) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(path = %node.path.display(), %error, "search skips unreadable file");
            return None;
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        tracing::warn!(path = %node.path.display(), "search skips non-UTF-8 file");
        return None;
    };
    let lower = text.to_lowercase();
    let lower = (lower.len() == text.len()).then_some(lower);
    Some(FileText {
        path: node.path.clone(),
        size: node.size,
        modified: node.modified,
        text,
        lower,
    })
}

/// Scans `lower` whole, mapping each match back to its line in `file.text`
/// by byte offset.
fn search_aligned(file: &FileText, lower: &str, needle: &str, limit: usize, out: &mut Results) {
    let mut from = 0;
    let mut line_no = 1;
    let mut line_start = 0;
    while let Some(found) = lower[from..].find(needle) {
        if out.hits.len() >= limit {
            out.total += lower[from..].matches(needle).count();
            return;
        }
        let at = from + found;
        line_no += lower[line_start..at].matches('\n').count();
        if let Some(nl) = lower[line_start..at].rfind('\n') {
            line_start += nl + 1;
        }
        let line_end = lower[at..].find('\n').map_or(lower.len(), |nl| at + nl);
        push_hit(
            file,
            line_no,
            line_start,
            line_end,
            at,
            needle.len(),
            limit,
            out,
        );
        from = at + needle.len().max(1);
    }
}

/// Lowercases each line separately; for the file whose lowercase form is a
/// different length from its text.
fn search_by_line(file: &FileText, needle: &str, limit: usize, out: &mut Results) {
    let mut line_start = 0;
    for (index, raw) in file.text.split_inclusive('\n').enumerate() {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let lower = line.to_lowercase();
        if lower.len() == line.len() {
            let mut from = 0;
            while let Some(found) = lower[from..].find(needle) {
                let at = line_start + from + found;
                push_hit(
                    file,
                    index + 1,
                    line_start,
                    line_start + line.len(),
                    at,
                    needle.len(),
                    limit,
                    out,
                );
                from += found + needle.len().max(1);
            }
        } else if lower.contains(needle) {
            push_hit(
                file,
                index + 1,
                line_start,
                line_start + line.len(),
                line_start,
                0,
                limit,
                out,
            );
        }
        line_start += raw.len();
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "a private helper on one call path"
)]
fn push_hit(
    file: &FileText,
    line: usize,
    line_start: usize,
    line_end: usize,
    at: usize,
    len: usize,
    limit: usize,
    out: &mut Results,
) {
    out.total += 1;
    if out.hits.len() >= limit {
        return;
    }
    let text = file.text[line_start..line_end]
        .strip_suffix('\r')
        .unwrap_or(&file.text[line_start..line_end]);
    let start = at - line_start;
    let end = (start + len).min(text.len());
    out.hits.push(Hit {
        path: file.path.clone(),
        line,
        column: text[..start].chars().count(),
        len: text[start..end].chars().count(),
        text: text.to_owned(),
        range: start..end,
    });
}

/// A window of `text` around `range`, at most `before` characters ahead of
/// it and `after` characters past it, with ellipses where cut. Returns the
/// window and the match's byte range within it.
#[must_use]
pub fn excerpt(
    text: &str,
    range: &Range<usize>,
    before: usize,
    after: usize,
) -> (String, Range<usize>) {
    let head: String = text[..range.start]
        .chars()
        .rev()
        .take(before)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let cut_head = head.len() < range.start;
    let tail: String = text[range.end..].chars().take(after).collect();
    let cut_tail = tail.len() < text.len() - range.end;
    let mut window = String::new();
    if cut_head {
        window.push('…');
    }
    window.push_str(head.trim_start());
    let start = window.len();
    window.push_str(&text[range.clone()]);
    let end = window.len();
    window.push_str(tail.trim_end());
    if cut_tail {
        window.push('…');
    }
    (window, start..end)
}

/// The character index in `buffer` of `column` characters into line `line`
/// (1-based), clamped to the buffer's end.
#[must_use]
pub fn char_index(buffer: &str, line: usize, column: usize) -> usize {
    let mut index = 0;
    for (n, raw) in buffer.split_inclusive('\n').enumerate() {
        if n + 1 == line {
            let in_line = raw.strip_suffix('\n').unwrap_or(raw).chars().count();
            return index + column.min(in_line);
        }
        index += raw.chars().count();
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(files: &[(&str, &str)]) -> (tempfile::TempDir, Corpus) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            fs::write(dir.path().join(name), text).unwrap();
        }
        let mut tree = Node::scan(dir.path()).unwrap();
        tree.sort(crate::tree::SortOrder::default());
        let mut corpus = Corpus::default();
        corpus.refresh(&tree);
        (dir, corpus)
    }

    #[test]
    fn hits_carry_line_column_and_range() {
        let (_dir, corpus) = corpus(&[
            (
                "a.md",
                "# Title\n\nsome Needle here\nneedle again, NEEDLE twice\n",
            ),
            ("b.md", "nothing\n"),
        ]);
        let results = corpus.search("needle", 10);
        assert_eq!(results.total, 3);
        assert_eq!(results.files, 1);
        let hit = &results.hits[0];
        assert_eq!(hit.line, 3);
        assert_eq!(hit.column, 5);
        assert_eq!(hit.len, 6);
        assert_eq!(&hit.text[hit.range.clone()], "Needle");
        assert_eq!(results.hits[1].line, 4);
        assert_eq!(results.hits[1].column, 0);
        assert_eq!(results.hits[2].column, 14);
        assert!(corpus.search("", 10).hits.is_empty());
        assert!(corpus.search("absent", 10).hits.is_empty());
    }

    #[test]
    fn the_limit_caps_hits_but_not_the_count() {
        let (_dir, corpus) = corpus(&[("a.md", "x x x\nx\n"), ("b.md", "x\n")]);
        let results = corpus.search("x", 2);
        assert_eq!(results.hits.len(), 2);
        assert_eq!(results.total, 5);
        assert_eq!(results.files, 2);
    }

    #[test]
    fn crlf_and_non_ascii_lines_report_char_columns() {
        let (_dir, corpus) = corpus(&[("a.md", "héllo wörld\r\nnext Wörld\r\n")]);
        let results = corpus.search("WÖRLD", 10);
        assert_eq!(results.total, 2);
        assert_eq!(results.hits[0].column, 6);
        assert_eq!(results.hits[0].text, "héllo wörld");
        assert_eq!(results.hits[1].line, 2);
        assert_eq!(results.hits[1].column, 5);
    }

    #[test]
    fn a_file_whose_lowercase_form_changes_length_is_still_searched() {
        let (_dir, corpus) = corpus(&[("a.md", "İstanbul\nplain match\n")]);
        let results = corpus.search("match", 10);
        assert_eq!(results.total, 1);
        assert_eq!(results.hits[0].line, 2);
        assert_eq!(results.hits[0].column, 6);
        assert_eq!(corpus.search("i̇stanbul", 10).total, 1);
    }

    #[test]
    fn refresh_keeps_unchanged_files_and_follows_edits() {
        let (dir, mut corpus) = corpus(&[("a.md", "before\n")]);
        assert_eq!(corpus.search("before", 10).total, 1);
        fs::write(dir.path().join("a.md"), "after, longer\n").unwrap();
        fs::write(dir.path().join("b.md"), "before too\n").unwrap();
        let tree = Node::scan(dir.path()).unwrap();
        corpus.refresh(&tree);
        assert_eq!(corpus.len(), 2);
        assert_eq!(corpus.search("after", 10).total, 1);
        assert_eq!(corpus.search("before", 10).total, 1);
    }

    #[test]
    fn excerpts_window_around_the_match() {
        let text = "0123456789 match 0123456789";
        let (window, range) = excerpt(text, &(11..16), 4, 4);
        assert_eq!(window, "…789 match 012…");
        assert_eq!(&window[range], "match");
        let (window, range) = excerpt(text, &(11..16), 100, 100);
        assert_eq!(window, text);
        assert_eq!(range, 11..16);
    }

    #[test]
    fn char_index_walks_lines_by_character() {
        let buffer = "héllo\nwörld\nend";
        assert_eq!(char_index(buffer, 1, 0), 0);
        assert_eq!(char_index(buffer, 2, 1), 7);
        assert_eq!(char_index(buffer, 2, 99), 11);
        assert_eq!(char_index(buffer, 3, 1), 13);
        assert_eq!(char_index(buffer, 9, 0), 15);
    }
}
