# Settings

`Ctrl+,` opens the Settings window. Changes take effect at once and are saved as you make them; there is no Apply button. Everything is also in one file you can edit by hand.

![Appearance settings](screenshots/settings-appearance.png)

## The categories

| Category | What is in it |
| --- | --- |
| **General** | Language, where Compression opens (the launch folder, home, the last folder, a folder of your choice, or the tabs of the last session), whether folders open in a new window, whether Compression stays in the background for faster windows, asking before deleting for good, and whether copies to one disk run one at a time. |
| **Appearance** | The theme and the accent colour, interface size, font, how round the corners are, how transparent the window is, the icon style and animations. The theme editor lives here. |
| **Files & View** | List or grid, row height and icon size, the columns of the list, how many lines of a name the grid shows, folders first, hidden files, whether each folder keeps its own view, thumbnails and previews on servers, file name extensions, relative dates, decimal or binary sizes, folder sizes (off, on demand, always), size on disk, and whether the sidebar and the status bar are shown. |
| **Sidebar** | Which places the sidebar lists, and whether it shows every internal disk and partition or only Computer and the drives you plug in. |
| **Behavior** | Opening with a single click, vim-style navigation keys, typing to jump to a name (or to filter, without pressing `/` first), how fast the mouse wheel scrolls (the "Wheel scroll speed" slider under "Mouse and touchpad"), swiping to go back and forward on a touchpad, the terminal command and the folder of file templates. |
| **Keybindings** | Every shortcut, searchable; click one to record a new key. |
| **Applications** | Which program opens each kind of file by default. |
| **Plugins** | Which [plugins](plugins.md) are on, and the options of each (the problems of custom actions, the colours of tags). |

## Themes

Compression ships with a large set of built-in palettes (Midnight, Nord, Gruvbox, Catppuccin, Dracula, Tokyo Night, Rosé Pine, One Dark, Everforest, Solarized and many more, light and dark), and the default theme follows the colours of the [Serpantinum](https://github.com/ilyamiro) shell if you use it.

![Themes](screenshots/themes.png)

The **+** card in Settings → Appearance opens the theme editor: name the theme, set every colour with a picker or a hex code (or start from any built-in palette) and save. Your themes are listed under the built-in ones, each with an edit button. A theme is one JSON file in `~/.local/state/compression/themes/`, named after the theme, and `"theme": "custom:mine"` in `settings.json` selects `mine.json`:

```json
{
  "name": "Mine", "light": false,
  "base": "#0f0d13", "mantle": "#1c1b20", "crust": "#141318",
  "text": "#e6e1e9", "subtext0": "#cac4cf", "subtext1": "#938f99",
  "surface0": "#201f24", "surface1": "#2b292f", "surface2": "#36343a",
  "accent": "#ccbeff", "peach": "#eeb8cb", "green": "#cac3dc", "red": "#ffb4ab", "yellow": "#f5d76e"
}
```

A colour that is missing or not written `#rrggbb` keeps the one of the dark palette. Compression follows that folder while it runs, so a colour generator (matugen, wallust, pywal and the like) can write a theme file from a template and the window changes with it.

## settings.json

Settings are kept in `~/.config/compression/settings.json` (under `$XDG_CONFIG_HOME` if you set it). The file is JSON; a key that is missing has its default, and unknown keys are ignored. Some of the keys:

| Key | Meaning |
| --- | --- |
| `theme`, `accent`, `scale`, `font` | Look: a theme name (`custom:<name>` for your own), an accent colour as `#rrggbb`, the interface size (1.0 is normal) and a font file. |
| `language` | `system`, or one of `en`, `de`, `es`, `fr`, `ru`, `vi`. |
| `view_mode`, `sort_by`, `sort_order`, `folders_first`, `show_hidden` | The view of the main window. |
| `start_location`, `start_path` | Where Compression opens; `start_path` is used with the custom location. |
| `single_click_open`, `vim_keys`, `type_ahead`, `type_to_filter`, `swipe_navigation` | Behavior switches. |
| `confirm_delete`, `queue_copies`, `run_in_background` | File operations and background instance. |
| `terminal`, `templates_dir` | The terminal command (with `{}` for the folder, where it needs one) and the templates folder. |
| `plugins` | An object of plugin ids and `true` or `false`. |
| `keybinds` | Your own shortcuts, see below. |
| `remote_previews`, `show_thumbnails`, `folder_sizes` | Thumbnails and folder sizes. |

Keybindings are stored by action name, each with a list of keys: `{ "keybinds": { "new_tab": ["Ctrl+T", "Alt+T"] } }`. A key is written as modifiers joined by `+` and then the key, such as `Ctrl+Shift+N`, `Alt+ArrowLeft` or `F2`. An action that is not listed keeps its default keys.

Besides the settings file Compression keeps a few other files of its own, in the usual folders of your user:

| Folder | Holds |
| --- | --- |
| `~/.config/compression/` | `settings.json`, the saved servers (`servers.json`), your pins (`pins.json`), folder icons, tag colours (`tags.json`) and `actions/` |
| `~/.local/share/compression/` | The list of recent files and the tag paths it has seen |
| `~/.local/state/compression/` | Your themes (`themes/`) and the size and last folders of the file dialog |
| `~/.cache/compression/` | Thumbnails; they are made again when missing, so the folder can always be deleted |
| `~/.local/share/Trash/` | The trash, in the format every file manager shares |

### Edits take effect at once

Compression watches `settings.json`. When you save the file with an editor, or another tool writes it, every open window reads it again as soon as the write is finished, with no timer, and each changed setting takes effect as if you had switched it in the Settings window; the Settings window, the icon gallery and Copy to / Move to redraw with it. A message says "Settings reloaded".

- A file that is not valid JSON (say, half way through your edit) does not change anything: the current settings stay, and a message tells what is wrong. Fix the file and save again.
- Each window keeps its own tabs; changing the file does not move them between windows.
- Compression writes the file itself whenever you change a setting. It writes a new file next to it and renames it into place, so a reader never sees half a file, and a file you made a symbolic link (a dotfile manager's) stays a link. A change you make by hand to a setting that Compression is also changing at that moment can be lost: the last writer wins.
- An administrator window reads your settings and never writes them.

## Moving over from the earlier name

Compression started as a fork of another file manager, and it takes over the data of that program once, when it first starts:

- The folders `~/.config/ski`, `~/.local/share/ski` and `~/.local/state/ski` are **copied** to `…/compression`, and only where the target folder does not exist yet. Nothing is deleted or overwritten, so the old folders stay as they are; the cache is not copied and builds up again. A copy that is cut short is never left under the final name, and several Compression processes that start together do not get in each other's way. In an administrator window nothing is migrated.
- Settings and pin files that still name the Recent and Bookmarks views `ski:recent` and `ski:bookmarks` are read as `compression:recent` and `compression:bookmarks`. The files are rewritten with the new names the next time they are saved.
- The environment variables `SKI_SELECTED_FILES` and `SKI_CURRENT_DIR` (for [custom actions](custom-actions.md)) and the other `SKI_*` variables are still read, with the `COMPRESSION_*` ones taking precedence; actions get both kinds.
- The portal setting `org.freedesktop.impl.portal.FileChooser=ski` has to be changed to `=compression` in your `portals.conf`, and window rules for the old application ids have to be changed to `compression-file-manager` and `compression-chooser`. See [File dialog and desktop integration](portal.md).

Once the new folders exist, the old ones are not read again. To start over, remove `~/.config/compression`, `~/.local/share/compression` and `~/.local/state/compression`; if the old folders are still there, they are copied again.
