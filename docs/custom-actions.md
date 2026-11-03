# Custom actions

A custom action adds your own command to the context menu. It can be a small JSON file or any executable script. Actions are one of the [plugins](plugins.md); switch the plugin on in Settings → Plugins.

## Where actions live

Every `*.json` file and every executable file in `~/.config/compression/actions/` is one entry in the menu. Compression watches the folder, so a new or edited action shows up without a restart. Settings → Plugins lists the files that could not be used and says why, and has a **New action** button that writes a starting point for you.

Files that are neither JSON nor executable (notes, for example) are ignored. In an administrator window no action is loaded at all: those commands would run with root's rights.

## A JSON action

```json
{
  "name": "Convert to WebP",
  "command": "for f in \"$@\"; do cwebp \"$f\" -o \"${f%.*}.webp\"; done",
  "extensions": ["jpg", "png"]
}
```

| Key | Meaning |
| --- | --- |
| `name` | The text in the menu. Without it the file name is used. |
| `command` | Required. Runs with `sh -c`; the selected files are its arguments (`"$@"`). |
| `extensions` | Show the action only when every selected file has one of these extensions. |
| `dirs_only` | Show it only for folders. |
| `files_only` | Show it only for files. |
| `single` | Show it only when exactly one item is selected. |
| `timeout` | Seconds the action may run before it is stopped. 1 to 3600, default 30. |
| `max_output_mb` | MiB the action may print on each of its two streams before it is stopped. 1 to 64, default 1. |

A script is run by its own path with the selected files as arguments. It has no filters and no limits to set: it runs under the defaults.

## How an action runs

- It starts in the open folder, in the background, and the window stays responsive. Its standard input is closed.
- Besides the arguments, it gets two environment variables: `COMPRESSION_SELECTED_FILES` (one path per line) and `COMPRESSION_CURRENT_DIR`.
- When it ends, a message tells whether it finished or failed, and shows the last line it printed on its error stream if it failed. The folder is read again afterwards, so new files show up.

## Time and output limits

An action is stopped when it runs longer than its limit or prints more than its output limit on either stream, together with every program it started. By default that is 30 seconds and 1 MiB, and a message names the limit that was hit ("it took longer than 30 s", "it printed more than 1 MiB").

For a long job, ask for more in its own file:

```json
{
  "name": "Back up",
  "command": "rsync -a \"$@\" /mnt/backup/",
  "timeout": 900,
  "max_output_mb": 8
}
```

A value outside the range, or one that is not a finite number, is ignored and the default stands for that limit; the other limit is still used. A value that is not a number at all (`"timeout": "soon"`) makes the file unusable, and it shows up in the list of problems in Settings → Plugins.

## Examples

Open the selection in an editor:

```json
{ "name": "Open in Code", "command": "code \"$@\"" }
```

Compare two selected files:

```json
{ "name": "Compare in Meld", "command": "meld \"$1\" \"$2\"", "files_only": true }
```

A script, saved as `~/.config/compression/actions/count-lines` and made executable:

```sh
#!/bin/sh
wc -l "$@" | tail -n 1
```
