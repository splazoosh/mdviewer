# mdviewer

A desktop editor for the markdown files in a directory tree, with a library
of text clipped out of them. Runs on Windows and Linux.

## Build

Requires a stable Rust toolchain.

On Linux, first install the build packages (Ubuntu names; the file dialogs
use GTK 3):

```
sudo apt-get install build-essential pkg-config libgtk-3-dev
```

Then, on either platform:

```
cargo build --release   # target/release/mdviewer (mdviewer.exe on Windows)
cargo test
```

`cargo run` gives a debug build that logs to the console (`RUST_LOG=debug`).

On Linux the app runs in an X11 or Wayland desktop session; a minimal
install may also need `libxkbcommon-x11-0`.

## Use

`mdviewer [directory]` opens the given directory, or the last one opened.
**File → Open repository…** picks another. The left pane lists every `*.md`
below the root (`.git`, `target`, `node_modules` and dot-directories are
skipped), sortable by name, size or modified date; the order is remembered.

The search box under the toolbar (**Ctrl+Shift+F**) finds text in every
listed file as you type, case-insensitively. Clicking a hit opens the file
with the match selected. **Esc** clears the search.

Click a file to edit it. **Ctrl+S** saves exactly the edited text. A file
that changed on disk since it was opened is not overwritten silently: the
app asks whether to overwrite or reload. Switching files or closing with
unsaved changes asks first.

Select text and press **Ctrl+Shift+K** (or **Clip**) to store it in the
library on the right with its source file and date. Entries are searchable,
editable in place, copied with **Copy** and removed with **Delete**.

## Data

The app writes nothing under an opened directory except files saved from the
editor. A save writes `<file name>.mdviewer-save.tmp` beside the file and
renames it over the file, so an interrupted save never leaves the file short;
a crash at that moment can leave the `.tmp` behind, safe to delete. The
rename gives the file a new identity: hard links keep the old text. On
Windows, permissions set on the file itself are replaced by its folder's,
and the hidden and system attributes are cleared; on Linux the permission
bits are kept and the file is owned by whoever saved it.

The app's own state lives in `%APPDATA%\mdviewer\` on Windows and in
`$XDG_DATA_HOME/mdviewer/` (by default `~/.local/share/mdviewer/`) on Linux:

- `library/NNNN-slug.md`: one markdown file per library entry, with YAML
  front matter (`title`, `source`, `clipped`) over the clipped text. Hand
  edits that keep the shape load; unreadable files are skipped and left alone.
- `settings.json`: the last directory and the sort order. A file that fails
  to load is moved to `settings.json.bak` and replaced with the default.

## Limits

- Files must be UTF-8. A `\r\n` file is written back as `\r\n`; a file with
  mixed line endings is flagged until it is saved, and comes out uniform.
- Search reads the files as saved and holds them all in memory; the first
  500 hits are listed and the rest counted.
- On Linux, text copied with **Copy** is held by the app and is gone once
  the app closes, unless a clipboard manager keeps it.

## Licence

MIT; see `LICENSE`.
