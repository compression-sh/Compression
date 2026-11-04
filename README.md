<p align="center">
  <img src="assets/logo.svg" alt="Compression logo" width="128">
</p>

<h1 align="center">Compression</h1>

<p align="center">A small, fast, keyboard-friendly file manager for Linux, written in Rust.</p>

![Compression showing a Pictures folder as a grid of thumbnails](docs/screenshots/hero.png)

Compression is for people who move through folders with the keyboard and want a file manager that starts at once, stays out of the way and does not need a desktop environment around it. It runs natively on Wayland and X11, previews files as you go, opens folders on other computers like local ones, and can stand in for the system's open and save dialog.

Compression is a fork of [Ski](https://github.com/ilyamiro/Ski) by ilyamiro, developed further independently.

## Features

**Working with files**
- List and grid views with thumbnails, natural sorting, folder sizes on demand and a status line that tells what is selected.
- Tabs, history, bookmarks, pinned places and recent files. Vim-style keys as an option next to the usual ones; every shortcut can be changed.
- Copy, move, trash and delete run as background jobs with progress, pause, cancel and conflict questions, and every change can be undone.
- Rename one file or many at once, by template or by find and replace. Search by name or inside files, filter by type.
- Drag and drop between windows, tabs and other programs, on Wayland and X11.
- A copy leaves out files it cannot read and tells you which; the free room of the disk shows at the bottom; a file with no app set for its type opens the "Open with" list.

**Archives**
- Pack to `.zip`, `.tar.gz` and, with the usual tools installed, `.tar.xz`, `.tar.bz2`, `.tar.zst` and `.7z`; unpack all of those and `.rar`, `.iso`, `.deb` and `.rpm`.
- Packing and unpacking show progress and the time left, can be cancelled at any moment and take back what they made when they are. A job that runs for more than 30 minutes is stopped.
- Quick Look shows what is inside an archive without extracting it.

**Plugins and actions**
- Git status, checksums, tags, a duplicate finder, folder comparison and Quick Look, each one switched on or off in Settings.
- Custom actions: your own commands in the context menu, written as a small JSON file or a script, with a time and an output limit you can set per action.

**Servers and the desktop**
- Open folders on SMB and NFS shares, SSH (sftp), FTP, WebDAV and AFP servers with `Ctrl+K`; passwords can go to your keyring. A dot in the sidebar shows whether each server still answers, and a slow or stuck one can be reconnected from its menu.
- Serves as the system file dialog (xdg-desktop-portal `FileChooser`) and answers "Show in folder" (`FileManager1`).
- Settings live in one JSON file. Edit it by hand and every open window follows at once. Open as administrator through polkit when you need to.

## Install

**Prebuilt packages.** Every [release](https://github.com/compression-sh/Compression/releases/latest) has x86_64 builds and a `SHA256SUMS` file:

```sh
sudo apt install ./compression_*_amd64.deb              # Debian, Ubuntu
sudo dnf install ./compression-*.x86_64.rpm             # Fedora, openSUSE
sudo tar -xzf compression-*-x86_64-linux.tar.gz -C /    # any other distribution
```

The packages install the program, the desktop entry, the icon and the portal files, so Compression shows up in your launcher.

**From source.** You need Rust 1.88 or newer. Compression loads its graphics libraries at run time; the [installation guide](docs/installation.md) lists the packages to install for your distribution.

```sh
git clone https://github.com/compression-sh/Compression
cd Compression
cargo build --release          # the binary is target/release/compression
cargo install --path .         # or install it into ~/.cargo/bin
```

Docker, Podman, Nix and NixOS are covered in the [installation guide](docs/installation.md).

## Usage

```sh
compression                              # open the current folder
compression ~/Downloads                  # open a folder
compression sftp://me@example.com/srv    # open a folder on a server
compression --help
```

```text
Usage:
  compression [FOLDER | ADDRESS]   open a folder (default: the current one) or a server address
                                   such as sftp://me@example.com/srv
  compression --chooser --mode open|folder|save|save-files
                                   the file dialog for one request: prints the chosen file:// URIs
                                   and exits with 0 (chosen), 1 (cancelled) or 2 (failed)
  compression --portal             the xdg-desktop-portal service (FileChooser, FileManager1)
  compression --resident           keep a background instance, so new windows open at once
  compression -V, --version        print the version and exit
  compression -h, --help           print this help and exit
```

Press `?` in the window for every shortcut, `Ctrl+,` for Settings and right-click for the context menu.

## Documentation

- [Installation](docs/installation.md): packages, building, Docker, Nix and NixOS, troubleshooting.
- [Using Compression](docs/usage.md): the window, navigation, selecting, file operations and the keyboard shortcuts.
- [Archives](docs/archives.md): packing, unpacking, cancelling and the time limit.
- [Custom actions](docs/custom-actions.md): your own commands in the context menu.
- [Plugins](docs/plugins.md): Git, checksums, tags, duplicates, folder comparison and Quick Look.
- [Servers](docs/servers.md): network shares and SSH servers.
- [File dialog and desktop integration](docs/portal.md): the portal, the chooser and its exit codes.
- [Settings](docs/settings.md): every option, `settings.json`, live reloading and the migration of older settings.
- [Releasing](docs/releasing.md): how a release is made (for maintainers).

Found a bug? Open an [issue](https://github.com/compression-sh/Compression/issues) with your distribution and whether you use Wayland or X11.

## License

Compression is released under the [MIT license](LICENSE). The bundled fonts keep their own licenses: Adwaita Sans (SIL OFL) and Symbols Nerd Font Mono (MIT).
