# Servers

Compression opens folders on other computers like any other folder. You browse, copy, rename and open files there the same way, and the files you open are handed to your programs like local ones.

Supported are Windows shares and NAS devices (SMB), SSH servers (SFTP), FTP and FTPS, WebDAV (Nextcloud and others), Apple shares (AFP) and NFS.

## Connecting

Press `Ctrl+K` or click **Connect to server…** at the bottom of the sidebar, type an address and press `Enter`. The same addresses work in the path bar (`Ctrl+L`) and on the command line:

```sh
compression sftp://me@example.com/srv
```

| Server | Address |
| --- | --- |
| Windows share, NAS | `smb://nas/media`, `smb://WORKGROUP;me@nas/media` or `\\nas\media` |
| SSH | `sftp://me@example.com/home/me`, `sftp://me@example.com:2222/` or `me@example.com:/srv` |
| FTP | `ftp://ftp.example.com/pub`; `ftps://` for FTP over TLS |
| WebDAV | `davs://cloud.example.com/remote.php/dav/files/me` |
| NFS, AFP | `nfs://host/export`, `afp://host/share` |

A user name or host that starts with a dash or contains control characters is refused: it could be taken for an option of the program that makes the connection.

### Passwords

When a server wants a password, Compression asks for it. **Remember password** keeps it in your keyring (GNOME Keyring, KWallet or KeePassXC), filed the way the GNOME file manager files it, so a password saved by one is found by the other. Without a keyring the password is remembered only until Compression ends. Passwords are cleared from Compression's memory after use, but they cannot be wiped from the memory of the programs that receive them or from your keyring.

FTP servers and shares that allow it can be opened anonymously or as a guest.

### SSH keys and host keys

SSH logs in with your keys and `ssh-agent` first, and asks for a password only when that fails. A server Compression has not seen before shows its key fingerprint, and you decide whether to trust it before anything is sent.

### When the server does not answer

Connecting gives up after 30 seconds without a sign of life, and the dialog can be closed while it waits: the connection is then ended with everything it started. Unmounting has its own limit, so a server that has gone away does not hold the window.

## Servers in the sidebar

Connected servers appear under the drives, each with a button to disconnect. **Keep in sidebar** leaves a server there after you disconnect, and a click connects again. Right-click a server to rename it, change its address, forget its saved password or remove it. Folders on a server can be pinned too, and opening such a pin reconnects.

A small dot in front of each server tells how it is doing; hover over it for the words. Green: connected. Yellow: it answers slowly. Red: the mount is there but does not answer properly. A ring: a saved server that is not connected, which is normal. Compression asks every connected server about every 15 seconds, in the background, and calls it slow when one question stays unanswered for 3 seconds.

For a yellow or red server the right-click menu has **Reconnect**. It lets go of the mount the gentle way (never forced, with a time limit), then connects the server again with the saved login, or asks for it, and returns to the folder you were in. Tabs that were inside the server are taken to your home folder first, so nothing holds the mount. It is not offered for mounts that belong to the system, and it waits while a job is running, since the job may be using the mount. If the mount cannot be released, the reason is shown and the server stays as it was.

## How servers differ from local folders

- **No trash.** "Move to trash" asks and then deletes for good.
- **No thumbnails and no folder item counts** by default: both read every file over the network. Settings → Files & View → **Previews on servers** turns them on.
- **Search stays local.** A search that starts on a local folder does not go into server folders unless previews on servers are on; a search started inside a server folder searches there.
- **A terminal** (`F4`) on an SSH server logs in with `ssh` and opens a shell in the same folder there.

## What Compression needs

Servers are mounted through GVfs, the service of the GNOME desktop that most desktops have. Without it, Compression uses `sshfs` for SSH and `rclone` for the other kinds, if they are installed. If none is available, the connection dialog says which to install.

Mounts live under `$XDG_RUNTIME_DIR/compression-net` and are gone when you log out. To make Compression use `sshfs` and `rclone` even where GVfs is installed, start it with `COMPRESSION_NETWORK_BACKEND=fuse`.

On Debian or Ubuntu install `gvfs-backends` and `gvfs-fuse`; on Fedora `gvfs-fuse` and `gvfs-smb`; on NixOS set `services.gvfs.enable = true`. Log out and in afterwards.
