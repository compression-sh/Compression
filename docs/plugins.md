# Plugins

Plugins are optional features that are built into Compression. They are all on by default and each one can be switched off in **Settings → Plugins**; a plugin that is off adds nothing to the menus and does no work. A plugin that is switched off closes its windows at once.

| Plugin | What it does |
| --- | --- |
| [Git](#git) | Marks changed files and shows the branch |
| [Checksums](#checksums) | MD5, SHA-1, SHA-256 and SHA-512, and checking downloads |
| [Tags](#tags) | Coloured labels on files |
| [Duplicate finder](#duplicate-finder) | Finds files with identical contents |
| [Compare folders](#compare-folders) | Shows how two folders differ |
| [Quick Look](#quick-look) | Previews a file with `Space` |
| [Custom actions](custom-actions.md) | Your own commands in the context menu |

## Git

In a Git repository Compression marks files by state with a letter next to the name: `M` changed, `A`, `S` and `R` staged, `D` deleted, `!` conflicted and `?` untracked. Folders that contain changes are marked too, and the status line shows the branch and the number of changes.

The context menu gets a Git group in a repository:

- **Stage** and **Unstage** the selection.
- **Show diff** opens a window with the staged and unstaged changes against the last commit.
- **Discard changes** throws changes away after asking. They cannot be brought back, and they are not part of Undo.

Git runs on a worker thread, so a large repository never stalls the window, and each command is stopped after 10 seconds or when it prints more than 64 MiB. A repository's own configuration cannot make Compression run programs: file system monitors, external diff programs and text converters named there are not used.

## Checksums

Properties (`Alt+Enter`) shows the MD5, SHA-1, SHA-256 and SHA-512 sums of a file. It also says whether one of them matches a hash on the clipboard, or the checksum file next to a download (`name.sha256`, `name.sha1`, `name.md5`, `SHA256SUMS`, `sha256sums.txt`, `MD5SUMS` and similar, in the GNU or BSD format).

On a checksum file, **Verify files listed** checks every file it names. That is a job like any other (progress, pause, cancel) and ends with the number of files that are fine, failed or missing. A name that points outside the folder of the checksum file is refused.

## Tags

Tags are coloured labels on files. Right-click → **Tags** → **New tag…**, or press `Alt+T`, adds one; **Remove all tags** takes them off again. The colours show as dots on the icon, and Settings → Plugins lets you pick the colour of each tag. The sidebar lists the tags in use, and `tag:work` in the search finds the files carrying a tag.

The tag names are stored in the extended attribute `user.xdg.tags`, which other file managers read as well, so they see the names (not the colours, which are Compression's own and live in `~/.config/compression/tags.json`). Copies and moves to another disk keep the tags; archives do not. The sidebar and the `tag:` search know the files Compression tagged itself or has seen tagged when it showed their folder, so a tag put on a file by another program appears there once you have opened its folder.

File systems without extended attributes (FAT, exFAT and some network shares) cannot hold tags, and links cannot carry them.

## Duplicate finder

Right-click a folder, or the empty space of the open folder, and choose **Find duplicate files…**. The search runs as a job (progress, pause, cancel). It skips empty files, links and hard links to the same file, compares sizes first, then the start and the end of the files, and reads a file whole (SHA-256) only when it still looks like another.

Nothing is selected for you. Tick the copies to remove by hand, or let a rule pick them: keep the newest, the oldest, the shortest path, or the copy inside a folder you name. One copy of every group always stays, a copy that changed since the search is left alone, and the rest goes to the trash as one undo step.

## Compare folders

Select two folders and choose **Compare**, or right-click empty space and choose **Compare with the other tab** (when exactly two tabs are open) or **Compare folders…** (two path fields). Every item is listed as only on the left, only on the right, newer on one side, different with the same time, or identical (hidden until you show them). A folder that exists on one side only is one row.

Items count as equal when their size and modification time match; two seconds apart still counts as equal, for the sake of FAT. **Compare contents** reads files of the same size instead.

Tick rows and copy them to the other side (conflicts ask, and progress and Undo work as for any copy), move one side's copy to the trash, or open it. Compare never deletes on its own: it is a compare-and-copy tool, not a mirror.

## Quick Look

`Space` previews the selected item without opening it: text and code with line numbers, pictures, documents, and the contents of archives (see [Archives](archives.md)). `↑` and `↓` move to the neighbouring file, and `Space` or `Esc` closes it again. Folders are not previewed.

## Custom actions

Your own commands in the context menu, as JSON files or scripts. See [Custom actions](custom-actions.md).
