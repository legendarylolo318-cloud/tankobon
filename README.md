# Tankobon

A fast, native manga reader written in Rust with [Slint](https://slint.dev). No webviews, Electron or GTK.
Read from MangaDex or from your own folders and CBZ/CBR/CB7 archives, offline included.

License: [GPL-3.0-or-later](LICENSE).

## Download

| Platform | File |
| --- | --- |
| Linux x86_64 | [Tankobon-x86_64.AppImage](https://github.com/legendarylolo318-cloud/tankobon/releases/latest/download/Tankobon-x86_64.AppImage) |
| Windows x86_64 | [Tankobon-x86_64.exe](https://github.com/legendarylolo318-cloud/tankobon/releases/latest/download/Tankobon-x86_64.exe) |
| Android (arm64, x86_64) | [Tankobon.apk](https://github.com/legendarylolo318-cloud/tankobon/releases/latest/download/Tankobon.apk) |

All releases: <https://github.com/legendarylolo318-cloud/tankobon/releases>

- **Linux**: `chmod +x Tankobon-x86_64.AppImage && ./Tankobon-x86_64.AppImage`
- **Windows**: run the exe. It is not code-signed, so SmartScreen may ask for confirmation.
- **Android** (experimental): allow installing from unknown sources. The APK is signed with a
  throwaway release key, so uninstall before installing a newer build. Local manga go in
  `Android/data/org.tankobon.app/files/local`. The layout is the desktop one with a compact
  sidebar; some headers are cramped on narrow phones.

## Features

- **Library**: lazily loaded cover grid, search, sort (title, last read, last updated, date added, unread),
  categories, unread and NEW badges, multi-select (mark read/unread, download, move to category, remove).
- **Explorer**: MangaDex (official API, rate limited) and Local (folders, CBZ/CBR/CB7). Latest, popular and
  search, then add to the library.
- **Manga page**: cover, synopsis, authors, genres, status; chapter list with read, downloaded and progress
  state; sorting; a resume button.
- **Reader**: right-to-left, left-to-right, vertical and webtoon modes. Fit width, fit height, original size
  or fit screen. Double-page spreads, fullscreen, keyboard/wheel/click-zone navigation, page overlay,
  background colour, white-border cropping, and per-manga settings.
- **Downloads**: queue with pause, resume and cancel, a concurrency limit, offline reading, and resume after a
  restart or crash.
- **Updates**: manual or on-startup checks, NEW badges, optional auto-download.
- **History**, **preferences** (theme, reader defaults, download folder, cache limit, clear cache) and
  single-file **backup/restore**.
- Progress is saved per chapter and page.

Reader keys: arrows / Space / PageUp / PageDown, `Home` / `End`, `F` or `F11` fullscreen, `M` menu, `Esc` back.
Click zones: the left and right 30% turn pages, the centre toggles the menu.

## Performance

Pages are decoded at display size and shared with the renderer without extra copies. Only the next two pages
and the previous one are preloaded, the page cache is sized from the window, and nothing polls while idle.
The desktop build uses Slint's software renderer by default, which needs no GPU memory and kept webtoon
scrolling at display rate in testing. To use the OpenGL renderer instead, start with
`SLINT_BACKEND=winit-femtovg`.

## Building

Requires stable Rust and a C/C++ compiler (bundled SQLite and libunrar are compiled from source).

### Linux

```bash
# Debian/Ubuntu: sudo apt install build-essential pkg-config libfontconfig1-dev libxkbcommon-dev
# Arch:          sudo pacman -S base-devel fontconfig libxkbcommon
cargo build --release -p tankobon
./target/release/tankobon
```

AppImage, as built in CI (`assets/` has the desktop file and icon):

```bash
linuxdeploy --appdir AppDir -e target/release/tankobon -d assets/tankobon.desktop -i assets/tankobon.png --output appimage
```

### Windows

With the MSVC build tools installed:

```bash
cargo build --release -p tankobon --target x86_64-pc-windows-msvc
```

The result is `target\x86_64-pc-windows-msvc\release\tankobon.exe`: a GUI app (no console window) with an
embedded icon.

### Android

Needs the Android SDK and NDK plus [`cargo-apk`](https://crates.io/crates/cargo-apk):

```bash
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-apk
cargo apk build --release -p tankobon-android
```

### Optional: AVIF

`cargo build --features avif` enables AVIF decoding (needs the system `dav1d` library).

## Architecture

```
core/    database, models, sources (MangaDex, Local), archives, image pipeline + caches, downloads, updates, backup
ui/      Slint UI (ui/*.slint), compiled by build.rs
app/     wires the UI to core (lib.rs) plus the desktop binary (main.rs); network, decoding and database
         work run off the UI thread
android/ Android shared library packaged with cargo-apk
assets/  icon, desktop file, Android launcher icons
```

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`--features autopilot` plus `TANKOBON_AUTOPILOT_OUT=<dir>` runs a scripted UI tour that saves screenshots.
