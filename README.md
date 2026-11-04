<p align="center">
  <img src="assets/logo.svg" alt="Compression logo" width="128">
</p>

<h1 align="center">Compression</h1>

A small, fast Rust file manager. Wayland and X11, list and grid views with thumbnails, tabs, vim keys, archives, plugins, themes, network shares and SSH servers, and a system file dialog.

Compression is a fork of [Ski](https://github.com/ilyamiro/Ski) by ilyamiro, developed further independently.

![Compression showing a Pictures folder as a grid of thumbnails](docs/screenshots/hero.png)

## Install

Every [release](https://github.com/compression-sh/Compression/releases/latest) has x86_64 packages:

```sh
sudo apt install ./compression_*_amd64.deb              # Debian, Ubuntu
sudo dnf install ./compression-*.x86_64.rpm             # Fedora, openSUSE
sudo tar -xzf compression-*-x86_64-linux.tar.gz -C /    # any other distribution
```

Docker, building from source, Nix and the NixOS module are covered in [Installation](https://github.com/compression-sh/Compression/wiki/Installation).

## Documentation

The [wiki](https://github.com/compression-sh/Compression/wiki/) documents every feature:

- [Installation](https://github.com/compression-sh/Compression/wiki/Installation) and [Desktop Integration](https://github.com/compression-sh/Compression/wiki/Desktop-Integration) (default file manager, system file dialog)
- [Navigation](https://github.com/compression-sh/Compression/wiki/Navigation), [File Operations](https://github.com/compression-sh/Compression/wiki/File-Operations) (undo, free-space check, the jobs list), [Search](https://github.com/compression-sh/Compression/wiki/Search), [Archives](https://github.com/compression-sh/Compression/wiki/Archives), [Servers](https://github.com/compression-sh/Compression/wiki/Servers) (status dot, Reconnect, phones and cameras)
- [Plugins](https://github.com/compression-sh/Compression/wiki/Plugins): Git, Checksums, Tags, Duplicate Finder, Compare Folders, Quick Look, [Custom Actions](https://github.com/compression-sh/Compression/wiki/Custom-Actions) (your own commands, with a time and output limit)
- [Settings](https://github.com/compression-sh/Compression/wiki/Settings), [Keyboard Shortcuts](https://github.com/compression-sh/Compression/wiki/Keyboard-Shortcuts), [Themes and Appearance](https://github.com/compression-sh/Compression/wiki/Themes-and-Appearance) (built-in themes, theme editor, transparency, languages)
- [Troubleshooting](https://github.com/compression-sh/Compression/wiki/Troubleshooting)

Found a bug? Open an [issue](https://github.com/compression-sh/Compression/issues) with your distribution and whether you use Wayland or X11.

## License

[MIT](LICENSE). The bundled fonts keep their own licenses: Adwaita Sans (SIL OFL) and Symbols Nerd Font Mono (MIT).
