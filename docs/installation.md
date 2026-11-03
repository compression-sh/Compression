# Installation

Compression is built for x86_64 Linux and runs on Wayland and X11. There are packages for Debian and Fedora families, a tarball for everything else, a container image and a Nix package.

## Prebuilt packages

Each [release](https://github.com/compression-sh/Compression/releases/latest) carries a `.deb`, an `.rpm`, a `.tar.gz` and a `SHA256SUMS` file. Check a download with `sha256sum -c --ignore-missing SHA256SUMS`.

```sh
sudo apt install ./compression_*_amd64.deb              # Debian, Ubuntu
sudo dnf install ./compression-*.x86_64.rpm             # Fedora, openSUSE
sudo tar -xzf compression-*-x86_64-linux.tar.gz -C /    # any other distribution, Arch included
```

The `.deb` and `.rpm` also pull in the graphics libraries. The tarball unpacks into `/usr` and does not: install them yourself (see below).

Every package puts the program, the desktop entry, the icon and the files for the portal and for "Show in folder" in place. To make Compression the program that opens folders, or the system's file dialog, see [File dialog and desktop integration](portal.md).

## Building from source

You need Rust 1.85 or newer (the project uses the 2024 edition).

```sh
git clone https://github.com/compression-sh/Compression
cd Compression
cargo build --release          # target/release/compression
cargo test                     # optional: the test suite needs no display
cargo install --path .         # optional: installs into ~/.cargo/bin
```

Compression opens its graphics libraries when it starts, so nothing has to be installed to compile it, but it will not start without them:

```sh
# Debian, Ubuntu
sudo apt install libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libx11-6 libxcursor1 libxi6 libxrandr2 libgl1 libegl1 xdg-utils
# Fedora
sudo dnf install libwayland-client libxkbcommon libxkbcommon-x11 libX11 libXcursor libXi libXrandr mesa-libGL mesa-libEGL xdg-utils
# Arch
sudo pacman -S wayland libxkbcommon libxkbcommon-x11 libx11 libxcursor libxi libxrandr libglvnd xdg-utils
```

A binary built this way has no desktop entry or icon. Install them by hand if you want a launcher entry:

```sh
install -Dm644 assets/compression.desktop ~/.local/share/applications/compression.desktop
install -Dm644 assets/logo.svg ~/.local/share/icons/hicolor/scalable/apps/compression.svg
```

Archive formats beyond `.zip` and `.tar.gz` use tools of your system, and servers use GVfs, `sshfs` or `rclone`; see [Archives](archives.md) and [Servers](servers.md).

## Docker and Podman

The image is `ghcr.io/compression-sh/compression`. A small script starts it with your display and your home folder:

```sh
curl -fsSL https://raw.githubusercontent.com/compression-sh/Compression/master/docker/compression-docker -o compression-docker
chmod +x compression-docker
./compression-docker [folder]
```

The script finds out whether you use Wayland or X11 and mounts your home folder, so you see your real files. `COMPRESSION_ENGINE=podman` makes it use Podman. Inside a container Compression cannot start your other programs, so "Open with" is of little use there; for daily work install it natively.

To build the image yourself:

```sh
docker build -t compression .
COMPRESSION_IMAGE=compression ./docker/compression-docker
```

## Nix and NixOS

Try it from a checkout:

```sh
git clone https://github.com/compression-sh/Compression && cd Compression
nix-shell --run 'cargo run --release'
```

To install it, import the package in `configuration.nix`:

```nix
environment.systemPackages = [ (pkgs.callPackage /path/to/Compression { }) ];
```

By default this installs the binary of the latest release, patched for Nix, which takes seconds. To compile your checkout instead, which takes several minutes the first time, pass `fromSource = true`. Other architectures than x86_64 always compile.

### The NixOS module

`nix/module.nix` installs Compression and, if you want, makes it the default file manager and the system's open and save dialog:

```nix
{
  imports = [ /path/to/Compression/nix/module.nix ];
  programs.compression.enable = true;
  # programs.compression.defaultFileManager = false;  # keep your current file manager
  # programs.compression.filePicker = false;          # keep your current file dialog
  # programs.compression.package = pkgs.callPackage /path/to/Compression { fromSource = true; };
}
```

It sets `inode/directory` to Compression, adds Compression as a portal backend for `org.freedesktop.impl.portal.FileChooser` and sets `GTK_USE_PORTAL=1`. Log out and in after `nixos-rebuild switch`. The option names used before the project was renamed still work as aliases. Home-manager's `xdg.portal` needs one more setting; it is described in [File dialog and desktop integration](portal.md#home-manager).

## Troubleshooting

- **No window, or errors about Wayland or libxkbcommon:** install the libraries listed above. On Nix, start Compression from `nix-shell`.
- **A blank window in a virtual machine:** try `LIBGL_ALWAYS_SOFTWARE=1 compression`.
- **You want X11 inside a Wayland session:** `env -u WAYLAND_DISPLAY compression`.
- **The interface is too small or too large:** Settings → Appearance → Size.
- **A format is missing when packing or unpacking:** install the tools listed in [Archives](archives.md#formats).
- **"Install GVfs…" when connecting to a server:** install GVfs with its FUSE part and its backends (`gvfs-backends` and `gvfs-fuse` on Debian and Ubuntu, `gvfs-fuse` and `gvfs-smb` on Fedora, `services.gvfs.enable = true` on NixOS), or `sshfs` and `rclone`. Log out and in afterwards.
- **A window rule for Compression:** the main window's application id is `compression-file-manager`, the file dialog's is `compression-chooser`.
