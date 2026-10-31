# Changelog

## 0.1.1

Linux fixes.

- The repository opens under its resolved path, so the window title, the
  remembered repository and the file paths shown carry no `.`, `..` or
  symbolic-link parts.
- Escape cancels every confirmation prompt, without also clearing the search
  box.
- The window keeps drawing while the folder dialog is open, and the open
  buttons are disabled until it closes. A dialog that fails is reported.
- **Copy** works under Wayland through the compositor's data-control protocol
  (`ext-` or `wlr-`), falling back to XWayland where the compositor offers
  neither.
- Not yet confirmed on a native Wayland desktop: **Copy** under Wayland and
  the folder dialog fix.

## 0.1.0

- First release: a desktop editor for Windows and Linux for the markdown
  files of a directory tree, with live search across them and a library of
  text clipped from them.
- Saving writes exactly the edited text; a file changed on disk since it was
  opened is never overwritten without asking.
- Library entries are plain markdown files under `%APPDATA%\mdviewer\library`
  on Windows and `~/.local/share/mdviewer/library` on Linux.
