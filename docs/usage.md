# Using Compression

Compression can be used with the keyboard alone, and with the mouse as you would expect. Press `?` in the window for the list of shortcuts, and right-click anything for a menu that shows the shortcut of each action.

![The list view](screenshots/list-view.png)

## The window

- **The path bar** at the top shows where you are as a row of folders you can click. `Ctrl+L` turns it into a text field: type or paste a path (with completion) or the address of a server, and press `Enter`.
- **The sidebar** on the left holds the places you use: Home, Recent, Bookmarks, Trash, your own pins, the drives, the servers you are connected to and the tags in use. Pin a folder or a file with `Ctrl+D`, and drag pinned rows to reorder them. What the sidebar shows can be changed in Settings → Sidebar.
- **The file area** shows the open folder as a list or a grid. `Ctrl+1` and `Ctrl+2` switch between the two; `Ctrl+=` and `Ctrl+-` change the size of the icons.
- **The status line** at the bottom tells how many items are selected and how large they are.
- **The main menu** (the button in the top corner) has the commands that are not tied to a file, such as a new window, Settings and "Open as administrator".

## Moving around

| To | Do this |
| --- | --- |
| Open a folder or a file | Double-click, or `Enter` (`l`). With "Open with a single click" a single click opens. |
| Go up | `Backspace`, `h` or `Alt+↑` |
| Go back and forward | `Alt+←` and `Alt+→`, or the mouse buttons; on a touchpad, swipe sideways with two fingers (off by default) |
| Go home | `Alt+Home` |
| Jump to the first or last item | `Home` (`g`) and `End` (`Shift+G`) |
| Jump to a name | Just type it: the selection goes to the first file whose name starts with what you typed |
| Search | `/` or `Ctrl+F`, then type; `Esc` leaves the results |
| Open a folder in a new tab | Middle-click it, or press `Ctrl+T` for a new tab |
| Open a folder in a new window | `Ctrl+N` for a new window; Settings → General can make every folder open in its own |

Right-click a tab for its menu: duplicate it, close the other tabs, close the tabs to its right, or close it. Closed tabs can be brought back like any other.

Tabs can be switched with `Ctrl+Tab` and `Ctrl+Shift+Tab` (or `Ctrl+PageDown` and `Ctrl+PageUp`), closed with `Ctrl+W` and brought back with `Ctrl+Shift+T`. Settings → General → Open Compression in → Restore last tabs opens the tabs of your last session again at the next start.

One notch of the mouse wheel scrolls 60 points. Settings → Behavior → Mouse and touchpad has a "Wheel scroll speed" slider with marks for Slow, Default, Fast and Faster; the button before the value (shown as `1×` at the default) puts the default back.

## Selecting

Click, or move with the arrow keys (or `j` and `k` with the vim keys on). `Shift` with the arrows, and `Shift` or `Ctrl` with a click, select several items. `Ctrl+A` selects everything, `Ctrl+Shift+A` clears the selection and `Ctrl+Shift+I` inverts it. `Ctrl+S` asks for a pattern such as `*.png` and selects every item whose name matches.

## Copying, moving and deleting

| To | Do this |
| --- | --- |
| Copy, cut and paste | `Ctrl+C`, `Ctrl+X`, `Ctrl+V` (other programs see the files on the system clipboard too) |
| Move by dragging | Drag items onto a folder, a place in the sidebar, a part of the path bar or a tab. Hold `Ctrl` to copy instead. |
| Duplicate | `Ctrl+Shift+D` |
| Make a link | `Ctrl+Shift+M` |
| Make a folder or a file | `Ctrl+Shift+N` and `Ctrl+Alt+N`. Files can be made from your own templates (Settings → Behavior → Templates). |
| Rename | `F2`. With several items selected it asks for a template or for text to find and replace. |
| Move to the trash | `Delete` |
| Delete for good | `Shift+Delete`. Asks first unless you turned that off in Settings → General. |
| Undo and redo | `Ctrl+Z` and `Ctrl+Shift+Z` (or `Ctrl+Y`) |

Copying, moving, deleting, packing and unpacking run as jobs in the background. `Ctrl+J` shows them: each has a progress bar with the speed and the time left, and a cancel button. Copies and cross-disk moves have a pause button too. When a name is taken, a job asks whether to replace, skip, keep both or (for folders) merge, and offers to do the same for all following conflicts. A cancelled copy removes the half-made copy; a failed or cancelled job never leaves a file that looks finished but is not.

Before a copy, a move to another disk or an unpacking starts writing, Compression compares what it needs with the free space of the target. If it may not fit, the job stops and asks whether to continue anyway ("Cancel" is the default). The check is skipped on servers and where the system gives no sizes, and replaced files are not subtracted, so it can warn a little early.

Copies that write to the same disk run one after the other, because several at once slow each other down on a spinning disk (Settings → General, "Run copies to the same disk one at a time"). A queued job shows "Waiting for the disk"; in the list you can cancel it, start it now or move it up or down. Moves inside one file system and deletes are never queued.

Compression refuses to copy pipes, sockets and device files, and a move across file systems that fails halfway removes the target it had made, so the original stays whole.

## The trash

Deleted items go to the trash of your home folder (`~/.local/share/Trash`); the trash view also shows what was deleted on mounted drives. Select an item there and choose "Restore" to put it back where it came from, or "Empty trash" to remove everything for good. Servers have no trash: deleting there asks first and removes the files immediately.

## Search

`/` opens the search bar. The search runs in the background, shows results as they are found, looks in recently used files first and never goes into folders on servers unless you started in one (or turned on "Previews on servers" in Settings → Files & View). The chips on the bar choose where it looks ("This folder" or "Everywhere", which is your home folder), narrow it to folders, documents, images, audio, video, archives or code, and set two options: "Match case" and "File contents" (text files up to 1 MiB are searched inside, too). `tag:work` finds the files carrying a tag.

In the search field `↓` moves on to the first result (`Shift+↓` does the same). From there the arrow keys walk through the results as they do in a folder, and `Enter` opens the one you reached. Typing more letters narrows the results on screen instead of emptying the list first.

Patterns understand `*` and `?`, so `*.png` or `report-??` work as you would expect.

## Properties

`Alt+Enter` shows the details of an item: size, dates, owner, group, permissions (which you can change), and the contents of folders. With the Checksums plugin on, it also shows the MD5, SHA-1, SHA-256 and SHA-512 sums. See [Plugins](plugins.md).

## Opening files

Files open with the default application of their type. "Open with…" in the context menu lists the other programs that can open them and can set a new default. `F4` opens a terminal in the open folder; Settings → Behavior chooses which one. On a folder of an SSH server it logs in with `ssh` and opens the same folder there.

## Open as administrator

"Open as administrator" in the main menu and the folder menus asks for your password through polkit (`pkexec`) and opens a second window running as root on that folder, so you can browse and edit system files. That window reads your settings and never writes them; custom actions and the terminal command from your settings are not used in it. You can give the command a shortcut under Settings → Keybindings.

## Keyboard shortcuts

These are the defaults. Every one can be changed in Settings → Keybindings. The bare letters (`j`, `k`, `h`, `l`, `g`, `Shift+G` and `y`) are the vim-style keys: they work once "Vim navigation keys" is switched on in Settings → Behavior, and typing a name to jump to it is a separate option there too.

| Keys | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move the selection |
| `Enter` / `l` | Open |
| `Backspace` / `h` / `Alt+↑` | Parent folder |
| `Alt+←` / `Alt+→` | Back / forward |
| `Home` / `g`, `End` / `Shift+G` | First / last item |
| `Alt+Home` | Home folder |
| `Ctrl+L` | Edit the path |
| `/` / `Ctrl+F` | Search |
| `Ctrl+K` | Connect to a server |
| `Ctrl+H` | Show or hide hidden files |
| `Ctrl+1` / `Ctrl+2` | List / grid view |
| `Ctrl+=` / `Ctrl+-` | Larger / smaller icons |
| `Ctrl+D` | Pin the selection (or the open folder) |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste |
| `Ctrl+A` | Select all |
| `Ctrl+Shift+A` / `Ctrl+Shift+I` | Deselect all / invert the selection |
| `Ctrl+S` | Select items matching a pattern |
| `F2` | Rename |
| `Delete` / `Shift+Delete` | Move to the trash / delete for good |
| `Ctrl+Z` / `Ctrl+Shift+Z`, `Ctrl+Y` | Undo / redo |
| `Ctrl+Shift+N` / `Ctrl+Alt+N` | New folder / new file |
| `Ctrl+Shift+D` / `Ctrl+Shift+M` | Duplicate / create a link |
| `y` | Copy the path |
| `Alt+Enter` | Properties |
| `Space` | Quick Look |
| `Alt+T` | Add a tag |
| `F4` | Open a terminal here |
| `F5` / `Ctrl+R` | Refresh |
| `Ctrl+J` | Show the running jobs |
| `Ctrl+T` / `Ctrl+W` / `Ctrl+Shift+T` | New tab / close tab / reopen the closed tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+N` | New window |
| `Menu` / `Shift+F10` | Context menu for the selection |
| `Ctrl+,` | Settings |
| `?` | Show the shortcuts |
| `Esc` | Close a dialog, clear the search, then deselect |
| `Ctrl+Q` | Close the window |

### Menus and dialogs

Settings, the context menu, the job list and every dialog work without a mouse, and a ring shows where the focus is.

| Keys | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Next / previous control (stays inside the open dialog or menu) |
| Arrow keys | Move to the control in that direction; in a menu, `Home` and `End` jump to the first and last entry |
| `←` / `→` on a choice such as "Sort by" | The previous / next option |
| `Enter` / `Space` | Use the focused control |
| `↑` / `↓` in Settings' category list | Switch the category; `→` or `Tab` enters it, `←` comes back |
| `Ctrl+PageUp` / `Ctrl+PageDown` | The previous / next Settings category |
| `Esc` | Close the menu or dialog |
