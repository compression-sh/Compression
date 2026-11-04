# Archives

Compression packs files into archives, unpacks them and shows what is inside without unpacking. Everything that takes time runs as a job in the background (`Ctrl+J` lists them), so the window stays usable.

## Formats

`.zip` and `.tar.gz` are handled by Compression itself and always work. The other formats use programs of your system and only appear when those are installed:

| Format | Pack | Unpack | Needs |
| --- | --- | --- | --- |
| `.zip` (also `.jar`, `.apk`) | yes | yes | nothing |
| `.tar.gz` (`.tgz`), `.tar` | yes | yes | nothing |
| `.tar.xz` | yes | yes | `tar` and `xz` |
| `.tar.bz2` | yes | yes | `tar` and `bzip2` |
| `.tar.zst` | yes | yes | `tar` and `zstd` |
| `.7z` | yes | yes | `7z` (or `7za`) |
| `.rar`, `.iso`, `.deb`, `.rpm`, `.cab` | no | yes | `7z` (or `7za`) |

On Debian or Ubuntu: `sudo apt install xz-utils bzip2 zstd p7zip-full`.

## Packing

Select files or folders in the open folder, right-click and choose **Compress…**. Pick a name and a format (only formats that work on your system are offered) and confirm. The archive is made next to the files; if the name is taken, " (copy)" is added to it. When the job is done, the archive can be undone with `Ctrl+Z`, which moves it to the trash.

With `.zip` and `.tar.gz`, symbolic links are stored as links, not followed.

## Unpacking

Right-click an archive and choose:

- **Extract here**: unpacks into a new folder next to the archive, named after it. Several archives at once get a folder each.
- **Extract to…**: unpacks into a folder you choose.

"Extract here" can be undone like any other change: undo moves the new folder to the trash.

Before anything is written, Compression lists what the archives hold, adds it up per target folder and compares it with the free space there. If it may not fit, the job stops and asks whether to continue anyway ("Cancel" is the default). An archive that cannot be listed in full (a damaged one, or one whose list is cut short because it is very large or slow to read) is not counted, so it brings no warning, and on servers the check is skipped.

## Looking inside

Select an archive and press `Space` for Quick Look (see [Plugins](plugins.md)). It shows the contents as a folder tree with sizes, the unpacked size, the compression ratio and a lock on encrypted entries, and extracts nothing. A `.zip` is read from its index; tar archives are read as a stream, and listing stops after 2,000 entries or 10 seconds.

## Progress and cancelling

An archive job shows how much it has done, how fast it goes and how long it will take. Press the cancel button of the job in the operations list (`Ctrl+J`) to stop it at any moment, even in the middle of a very large file or while an external program such as `7z` is running; the program is stopped with everything it started. Packing and unpacking cannot be paused.

A cancelled job takes back what it made:

- **Packing** leaves no archive. An archive that is not whole would look finished and is not.
- **Unpacking into a new folder** (Extract here) removes that folder.
- **Unpacking into an existing folder** (Extract to…) removes the files and folders it added at the top of that folder and leaves everything that was there before.

If you cancel while several archives are being unpacked, the ones that were finished before stay, and so does the undo step for them.

An archive that is damaged is not a cancel: the job reports the error, and what was unpacked up to that point stays where it is.

## The time limit

A packing or unpacking job is stopped after **30 minutes** for each archive. It then reports "It took longer than 30 minutes and was stopped" and takes back its work like a cancelled job does. The limit is fixed; it keeps a stuck external program from holding a job for ever. If you regularly handle archives that take longer, pack and unpack them from a terminal.
