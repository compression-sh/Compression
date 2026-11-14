<p align="center">
  <img src="assets/logo.svg" alt="Compression logo" width="128">
</p>

<h1 align="center">Compression</h1>

<p align="center">
  <a href="https://github.com/compression-sh/Compression/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/compression-sh/Compression"></a>
  <a href="https://github.com/compression-sh/Compression/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/compression-sh/Compression/actions/workflows/ci.yml/badge.svg?branch=master"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue"></a>
</p>

A small, fast Rust file manager. Wayland and X11, list and grid views with thumbnails, tabs, vim keys, archives, plugins, themes, network shares and SSH servers, and a system file dialog.

Compression is an independent project based on [Ski](https://github.com/ilyamiro/Ski), originally created by [ilyamiro](https://github.com/ilyamiro). Huge thanks to them for building the foundation this project is built on.

![Compression showing a Pictures folder as a grid of thumbnails](docs/screenshots/hero.png)

## Install

Every [release](https://github.com/compression-sh/Compression/releases/latest) has x86_64 packages:

```sh
sudo apt install ./compression_*_amd64.deb              # Debian, Ubuntu
sudo dnf install ./compression-*.x86_64.rpm             # Fedora, openSUSE
sudo tar -xzf compression-*-x86_64-linux.tar.gz -C /    # any other distribution
```

Check a download with `sha256sum -c SHA256SUMS --ignore-missing`. Docker, building from source, Nix, the NixOS module and the home-manager module are covered in [Installation](https://github.com/compression-sh/Compression/wiki/Installation).

## Documentation

The [wiki](https://github.com/compression-sh/Compression/wiki/) documents every feature:

| Topic | Pages |
| --- | --- |
| Getting started | [Installation](https://github.com/compression-sh/Compression/wiki/Installation), [Desktop Integration](https://github.com/compression-sh/Compression/wiki/Desktop-Integration) (default file manager, system file dialog) |
| Everyday use | [Navigation](https://github.com/compression-sh/Compression/wiki/Navigation), [File Operations](https://github.com/compression-sh/Compression/wiki/File-Operations) (undo, safe replace, the jobs list), [Search](https://github.com/compression-sh/Compression/wiki/Search), [Archives](https://github.com/compression-sh/Compression/wiki/Archives) |
| Servers and drives | [Servers](https://github.com/compression-sh/Compression/wiki/Servers) (status dot, Reconnect), [Drives and Devices](https://github.com/compression-sh/Compression/wiki/Drives-and-Devices) (eject, phones and cameras) |
| Plugins | [Plugins](https://github.com/compression-sh/Compression/wiki/Plugins): Git, Checksums, Tags, Duplicate Finder, Compare Folders, Quick Look, [Custom Actions](https://github.com/compression-sh/Compression/wiki/Custom-Actions) (your own commands) |
| Make it yours | [Settings](https://github.com/compression-sh/Compression/wiki/Settings), [Keyboard Shortcuts](https://github.com/compression-sh/Compression/wiki/Keyboard-Shortcuts), [Themes and Appearance](https://github.com/compression-sh/Compression/wiki/Themes-and-Appearance) (themes, icon styles, transparency, languages) |
| Help | [Troubleshooting](https://github.com/compression-sh/Compression/wiki/Troubleshooting) |

[docs/guide.md](docs/guide.md) is a compact summary of recent behaviour that lives in the repository.

Found a bug? Open an [issue](https://github.com/compression-sh/Compression/issues) with your distribution and whether you use Wayland or X11.

## License

[MIT](LICENSE). The bundled fonts keep their own licenses: Adwaita Sans (SIL OFL) and Symbols Nerd Font Mono (MIT).
