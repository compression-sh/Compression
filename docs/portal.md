# File dialog and desktop integration

Compression can do three jobs for the desktop besides being a file manager:

- be the program that opens folders (the default file manager),
- be the **open and save dialog** of other programs, through the xdg-desktop-portal `FileChooser`,
- answer **"Show in folder"** of browsers and other programs, through `org.freedesktop.FileManager1`.

The packages install everything these need; this page tells how to switch them on and what the pieces are.

## Default file manager

Install the desktop entry and the icon (the packages do), then:

```sh
xdg-mime default compression.desktop inode/directory
```

The desktop entry's window class is `compression-file-manager`.

## The system file dialog

### On NixOS

`nix/module.nix` does all of this for you; see [Installation](installation.md#the-nixos-module). It sets `inode/directory`, registers Compression as a backend for `org.freedesktop.impl.portal.FileChooser` and sets `GTK_USE_PORTAL=1`. Log out and in afterwards.

### Home-manager

If you also use home-manager's `xdg.portal`, it takes over: it writes `~/.config/xdg-desktop-portal/portals.conf`, which beats the system one, and points `NIX_XDG_DESKTOP_PORTAL_DIR` at the per-user profile, which does not contain Compression's portal file. Add Compression there too:

```nix
xdg.portal = {
  enable = true;
  extraPortals = [ pkgs.xdg-desktop-portal-gtk osConfig.programs.compression.package ];  # osConfig is the NixOS configuration
  config.common = {
    default = "*";
    "org.freedesktop.impl.portal.FileChooser" = "compression";
  };
};
```

### On other distributions

The `.deb`, `.rpm` and tarball install the portal files. If you build from source, install the files in `assets/` yourself:

| File | Goes to |
| --- | --- |
| `compression.portal` | `share/xdg-desktop-portal/portals/` |
| `org.freedesktop.impl.portal.desktop.compression.service` | `share/dbus-1/services/`, with `@out@` replaced by your install prefix |
| `org.freedesktop.FileManager1.service` | `share/dbus-1/services/`, with `@out@` replaced the same way |

Then tell the portal to prefer Compression in `~/.config/xdg-desktop-portal/portals.conf`:

```ini
[preferred]
org.freedesktop.impl.portal.FileChooser=compression
```

(`compression.portal` also lists the desktops it may serve in `UseIn=`, but when several installed portals offer a file dialog the one named in `portals.conf` wins, so this line is what makes Compression the choice.) Restart the portal or log out and in.

If you used the earlier name of this program, change the old value of that line to `compression`, and rename window rules that matched the old application ids.

### Programs and the portal

Programs that use the portal, such as most GTK and Qt programs, Electron applications and Flatpak apps, show Compression for "Open" and "Save as". Some need to be told:

- **Firefox:** set `widget.use-xdg-desktop-portal.file-picker` to `1` (on NixOS, `programs.firefox.preferences`).
- **Chromium and Chrome:** start with `--enable-features=UseXdgDesktopPortal` unless the browser does it itself.
- **GTK programs outside the portal:** set `GTK_USE_PORTAL=1`.

Programs with a dialog of their own do not use the portal and keep it.

## The dialog

`compression --portal` is the service itself; D-Bus starts it when a program asks for a file. Each dialog is then a Compression window of its own:

```sh
compression --chooser --mode open|folder|save|save-files
```

The dialog **only browses and chooses**: no moving, renaming or deleting (making a folder is allowed when saving or choosing a folder). It

- starts in the folder where the program last chose something, and opens at the size you last left it at (760×520 the first time),
- attaches itself to the window that asked for it (on Wayland through xdg-foreign, on X11 as a transient window), so compositors float it and centre it on that window; when the program names no window it is a fixed-size window, which most compositors float and centre as well,
- asks before replacing each existing file when saving, and goes into a folder when you type its name into the name field,
- understands the file type filters of the program, including `*/*` MIME filters (they use the system's shared-mime-info database),
- closes by itself when the program withdraws its request.

The dialog's application id is `compression-chooser`. If your compositor tiles it, give it a rule, for example in sway: `for_window [app_id="compression-chooser"] floating enable, move position center`.

### Options and exit codes

`--chooser` takes these options, which the portal fills in from the request:

| Option | Meaning |
| --- | --- |
| `--mode open\|folder\|save\|save-files` | What is chosen: files, a folder, a new file name, or names for several new files. |
| `--multiple` | More than one item may be chosen. |
| `--title TEXT`, `--accept TEXT` | The title and the label of the confirm button. |
| `--folder PATH` | The folder to start in. |
| `--name NAME`, `--file NAME` | The suggested file name when saving; `--file` once per name for `save-files` (only the last part of a name counts). |
| `--filter 'LABEL<TAB>g:*.png<TAB>m:image/png'` | A file type filter with glob (`g:`) and MIME (`m:`) patterns; `--current-filter N` picks the active one. |
| `--choice` | An extra option of the request (a check box or a choice). A `--choice` that cannot be read is skipped, the rest of the request stands. |
| `--parent`, `--app-id` | The window the dialog belongs to, and the asking program. |

The dialog prints the chosen items as `file://` URIs, one per line, and ends with:

| Exit code | Meaning |
| --- | --- |
| `0` | Something was chosen. |
| `1` | The person cancelled (or the program withdrew the request). |
| `2` | The dialog could not be shown, or the request could not be read. |

The portal turns these into the answers the asking program expects: chosen, cancelled, or failed.

## "Show in folder"

`org.freedesktop.FileManager1` is what browsers call for "Show in folder". Compression answers `ShowFolders`, `ShowItems` and `ShowItemProperties` by opening a new window for each item it is given. It accepts local `file://` URIs and absolute paths, and the address of a server; a URI that names another computer, or an argument that starts with a dash, is not taken as a folder.
