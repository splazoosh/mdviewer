# Provenance

Where every file in this repository, and every data table its source
declares, comes from, and the licence it is under. Nothing listed as not
shipped is in this repository; it is fetched by Cargo.

| Item | Origin | Licence |
|---|---|---|
| `src/` | Written for this repository | MIT (`LICENSE`) |
| `SKIPPED_DIRS` | Written for this repository: directory names the file list never descends into | MIT (`LICENSE`) |
| `Cargo.toml`, `Cargo.lock` | Generated for this repository | MIT (`LICENSE`) |
| `README.md`, `CHANGELOG.md`, `PROVENANCE.md`, `LICENSE`, `.gitignore`, `clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml` | Written for this repository | MIT (`LICENSE`) |
| Rust dependencies (not shipped) | Fetched from crates.io by Cargo, at the versions `Cargo.lock` pins | Each crate's own licence, as its package states |
