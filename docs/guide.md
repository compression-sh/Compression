# Notes on recent behaviour

A compact summary, kept in the repository, of recent behaviour and of setting Compression up with home-manager. The [wiki](https://github.com/compression-sh/Compression/wiki/) has the full guide.

## home-manager

The repository has a home-manager module, `nix/home-manager.nix`:

```nix
imports = [ /path/to/Compression/nix/home-manager.nix ];
programs.compression = {
  enable = true;
  settings = { show_hidden = true; theme = "nord"; };
};
```

- `package` is the latest release binary by default; `pkgs.callPackage /path/to/Compression { fromSource = true; }` builds the checkout instead.
- `settings` is written to `~/.config/compression/managed.json`, which Compression lays over its own `settings.json`. The keys are the names `settings.json` uses.
- A key set there wins at every start. A change made in the window to such a key is not saved as your own choice, and when the key is taken out of `settings`, the choice you had made before comes back.

## Icons

- Settings → Appearance → Icon style offers **System** first: folders and file types come from the icon theme of your desktop (the theme it inherits and hicolor included). What the theme lacks is drawn in the classic style. "Show all icons" lists the styles in the same order.
- Icons of themes made like Breeze, which mark their shapes for the desktop's colors, are drawn in the colors of the palette in use: text, accent, background and the status colors, light or dark. Icons drawn in colors of their own keep them.
- The card of the System style under Icon style shows the theme as it looks itself, in its own colors, so that themes can be told apart; the file list draws it in the palette's colors as above.

## Settings → Context menu

The page sits directly below Sidebar. It arranges the menus of a file, of a folder and of the empty space of a folder: drag an entry by its row (a submenu by its folder icon) between groups and into or out of submenus, hide it with the eye, rename a submenu in its field. Reset brings the built-in menu back.

## Files and folders

- **Moving to the trash** is a job: it shows its progress in the jobs list and can be stopped; what was moved can be undone, also after a stop. What has no trash (on a server, or on a drive without one) is asked about once, at the end.
- **Replace** only replaces with a complete copy. If a file of the new item cannot be read, nothing is replaced: the old item and the source stay as they were, and the message names what could not be read. A plain copy or a merge still leaves such files out and says so.
- **Extract here** unpacks into a hidden folder of its own (`.compression-extract-…`) and gives it the archive's name only when it is whole; when that name was taken meanwhile, it takes the next free one. A stop or failure takes away that hidden folder alone, never something someone else put beside it. After a crash such a folder may be left; it is not removed by itself.
- **Paste** goes into the folder it was asked in, also when the clipboard answers late and the window shows another folder or tab by then. If that folder is gone meanwhile, nothing is pasted anywhere.
- **Cut and paste on X11** moves what was cut in Compression, also names that are not valid UTF-8. X11 does not tell whether files were cut in another program, so pasting those copies them; on Wayland, a cut made in another program is moved.
- **Renaming** a file whose name is not valid UTF-8 keeps its name byte for byte when it is left as it is. A new name that still holds the "�" the window shows for such bytes is refused; type the new name in full.
- **Unmount, Eject and Disconnect** wait only for the jobs that need that drive or server, and say which one; meanwhile no job that needs it starts.
- **Eject** unmounts every partition before it powers the drive off; if one cannot be unmounted, the drive stays on and the message says which. It is offered for drives the system calls removable or hot-pluggable, and does nothing when the drive also holds something the system mounted (`/`, `/boot/efi`, `/home`, swap) or what else is on it cannot be told.
- **Open with** also lists programs that run in a terminal (`Terminal=true`, such as Vim) and starts them in one: the terminal of Settings → Behavior, else `$TERMINAL`, else the first one installed. Tilix and kitty cannot take a name that is not valid UTF-8, so such a name is refused before the terminal starts, and the window says so. LXTerminal is not used, since it splits the program's line on spaces.
- **Open as administrator** runs Compression as root only when nobody but root and you can change it: the program and every folder above it, up to `/`, must belong to root or to you and not be writable by others (a sticky folder like `/tmp` is fine). Otherwise it is not started, and the message names the file or folder and its owner or permissions.
- **Search** goes eight folders deep and stops at 400 results; when it leaves out deeper folders or stops at the limit, the window says so.

## Tags

- A tagged file that cannot be reached now (its drive is unplugged, its server does not answer, it cannot be read) keeps its tags.
- A file counts as **no longer there** only when the file system it was on is connected and the file is not; another drive mounted at the same place does not count. The tag counts tell available files from those that are not.
- Settings → Plugins → Tags offers to forget the tags of files that are no longer there, after you confirm; each file is looked at again first, and files are never touched.
- A tag renamed or deleted while a file is away is written to that file when it is back, so the old tag does not return.

## Custom actions

The selected files are the arguments of the command (`"$@"`): that is the one way that holds every name byte for byte. `COMPRESSION_SELECTED_FILES` lists them one per line, which cannot be split right for a name with a line break and shows "�" for a name that is not UTF-8; `COMPRESSION_SELECTED_COUNT` says how many files there are, so a script can tell.

## Files Compression keeps

Pins, bookmarks, folder icons and glyphs, the tag index and colors, "Open with" choices and the history of opened files live in `~/.config/compression`, `~/.local/state/compression` and `~/.local/share/compression`. One that cannot be read is kept as `<name>.broken` (or `.broken.1`, …) and the list starts empty; one that cannot be read at all, or cannot be kept aside, is left as it is and not written. A save that fails is said in the window.

## System file dialog

A file name that a program suggests when saving (for example through the portal) is used as a name in the folder shown; a path in it does not move the target elsewhere. A path you type into the field yourself still goes where it says.
