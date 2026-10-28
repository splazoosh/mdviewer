# Changelog

## 0.1.0

- First release: a desktop editor for Windows and Linux for the markdown
  files of a directory tree, with live search across them and a library of
  text clipped from them.
- Saving writes exactly the edited text; a file changed on disk since it was
  opened is never overwritten without asking.
- Library entries are plain markdown files under `%APPDATA%\mdviewer\library`
  on Windows and `~/.local/share/mdviewer/library` on Linux.
