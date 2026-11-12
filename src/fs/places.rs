use std::collections::HashMap;
use std::path::PathBuf;
use crate::ui::icons;

/// Stand-in location of the Recent view, which lists files from many folders.
pub const RECENT_PATH: &str = "compression:recent";
/// Stand-in location of the Bookmarks view, which lists the hearted files and folders.
pub const BOOKMARKS_PATH: &str = "compression:bookmarks";

/// The names the Recent and Bookmarks views had in Ski, still found in saved settings and given on the command line.
const LEGACY_RECENT_PATH: &str = "ski:recent";
const LEGACY_BOOKMARKS_PATH: &str = "ski:bookmarks";

/// `path` with a legacy Ski view name replaced by the current one.
pub fn normalize(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(LEGACY_RECENT_PATH) => PathBuf::from(RECENT_PATH),
        Some(LEGACY_BOOKMARKS_PATH) => PathBuf::from(BOOKMARKS_PATH),
        _ => path,
    }
}

/// Whether `path` names a view that lists items from many folders instead of a folder.
pub fn is_virtual(path: &std::path::Path) -> bool {
    path.as_os_str() == RECENT_PATH || path.as_os_str() == BOOKMARKS_PATH
}

/// What a sidebar destination opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceKind {
    Folder,
    /// Files and folders used lately, from every folder.
    Recent,
    /// Files and folders the user marked with a heart.
    Bookmarks,
}

/// A place the sidebar always offers, which the user can hide and show again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SidebarPlace {
    Home,
    Recent,
    Bookmarks,
    Trash,
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
    Computer,
    /// The Connect to server entry under the servers.
    ConnectToServer,
}

impl SidebarPlace {
    /// Off until the user turns it on, so it is kept in the list of places shown, not hidden.
    pub fn opt_in(self) -> bool {
        self == SidebarPlace::ConnectToServer
    }
}

/// Sidebar sections, separated by dividers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceGroup {
    Main,
    Folders,
    Devices,
}

#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub path: PathBuf,
    pub icon: &'static str,
    pub kind: PlaceKind,
    pub group: PlaceGroup,
    /// The device behind a drive entry.
    pub drive: Option<super::drives::Drive>,
    /// Which built-in place this is; drives and the like are `None`.
    pub builtin: Option<SidebarPlace>,
}

impl Place {
    pub fn new(name: impl Into<String>, path: PathBuf, icon: &'static str, group: PlaceGroup) -> Self {
        Self { name: name.into(), path, icon, kind: PlaceKind::Folder, group, drive: None, builtin: None }
    }

    /// Pinned folders and files, saved in `~/.config/compression/pins.json`.
    pub fn load_pins() -> Vec<PathBuf> {
        load_paths("pins.json")
    }

    pub fn save_pins(pins: &[PathBuf]) {
        save_paths("pins.json", pins);
    }

    /// Bookmarked files and folders, saved in `$XDG_STATE_HOME/compression/bookmarks.json`.
    pub fn load_bookmarks() -> Vec<PathBuf> {
        bookmarks_file().map(|f| load_paths_from(&f)).unwrap_or_default()
    }

    pub fn save_bookmarks(bookmarks: &[PathBuf]) {
        if let Some(file) = bookmarks_file() {
            save_paths_to(&file, bookmarks);
        }
    }

    pub fn default_places() -> Vec<Place> {
        use PlaceGroup::{Devices, Folders, Main};
        let builtin = |place: Place, id: SidebarPlace| Place { builtin: Some(id), ..place };
        let mut places = Vec::new();

        if let Some(home) = dirs::home_dir() {
            places.push(builtin(Place::new("Home", home, icons::HOME, Main), SidebarPlace::Home));
        }
        places.push(Place {
            kind: PlaceKind::Recent,
            ..builtin(Place::new("Recent", PathBuf::from(RECENT_PATH), icons::RECENT, Main), SidebarPlace::Recent)
        });
        places.push(Place {
            kind: PlaceKind::Bookmarks,
            ..builtin(Place::new("Bookmarks", PathBuf::from(BOOKMARKS_PATH), icons::HEART, Main), SidebarPlace::Bookmarks)
        });
        if let Some(trash) = super::trash::files_dir() {
            // Created on demand, so the destination always exists
            let _ = std::fs::create_dir_all(&trash);
            places.push(builtin(Place::new("Trash", trash, icons::TRASH, Main), SidebarPlace::Trash));
        }

        let user_dirs = [
            ("Desktop", dirs::desktop_dir(), icons::DESKTOP, SidebarPlace::Desktop),
            ("Documents", dirs::document_dir(), icons::DOCUMENTS, SidebarPlace::Documents),
            ("Downloads", dirs::download_dir(), icons::DOWNLOAD, SidebarPlace::Downloads),
            ("Pictures", dirs::picture_dir(), icons::IMAGE, SidebarPlace::Pictures),
            ("Music", dirs::audio_dir(), icons::AUDIO, SidebarPlace::Music),
            ("Videos", dirs::video_dir(), icons::VIDEO, SidebarPlace::Videos),
        ];
        for (name, dir, icon, id) in user_dirs {
            if let Some(dir) = dir.filter(|d| d.exists()) {
                places.push(builtin(Place::new(name, dir, icon, Folders), id));
            }
        }

        // Drives follow from a background listing (see `drives_ui`), as `lsblk` would hold up the first frame
        places.push(builtin(Place::new("Computer", PathBuf::from("/"), icons::STORAGE, Devices), SidebarPlace::Computer));

        // Compare against canonical paths so symlinked folders still highlight
        for place in &mut places {
            if place.kind == PlaceKind::Folder
                && place.drive.as_ref().is_none_or(|d| d.mounted)
                && let Ok(canonical) = place.path.canonicalize() {
                    place.path = canonical;
                }
        }
        places
    }
}

/// Bookmarks are state rather than settings; platforms without a state directory use the data one.
fn bookmarks_file() -> Option<PathBuf> {
    dirs::state_dir().or_else(dirs::data_dir).map(|d| d.join("compression").join("bookmarks.json"))
}

fn load_paths(name: &str) -> Vec<PathBuf> {
    config_file(name).map(|f| load_paths_from(&f)).unwrap_or_default()
}

fn load_paths_from(file: &std::path::Path) -> Vec<PathBuf> {
    Some(file)
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| serde_json::from_str::<Vec<PathBuf>>(&s).ok())
        // Folders on a server stay while it is not connected; opening one connects
        .map(|paths| paths.into_iter().filter(|p| p.exists() || super::network::address_of(p).is_some()).collect())
        .unwrap_or_default()
}

/// Where the person's own list is written: nowhere in a test, which must never touch the files of whoever runs it.
fn user_file_to_write(name: &str) -> Option<PathBuf> {
    config_file_to_write(name).filter(|_| !cfg!(test))
}

fn save_paths(name: &str, paths: &[PathBuf]) {
    if let Some(file) = user_file_to_write(name) {
        save_paths_to(&file, paths);
    }
}

fn save_paths_to(file: &std::path::Path, paths: &[PathBuf]) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(paths) {
        let _ = crate::fs::atomic::write(file, json);
    }
}

/// Custom folder icons: folder path -> image path, saved in `~/.config/compression/folder_icons.json`.
pub fn load_folder_icons() -> HashMap<PathBuf, PathBuf> {
    config_file("folder_icons.json")
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_folder_icons(icons: &HashMap<PathBuf, PathBuf>) {
    let Some(file) = user_file_to_write("folder_icons.json") else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(icons) {
        let _ = crate::fs::atomic::write(&file, json);
    }
}

static FOLDER_GLYPHS: std::sync::RwLock<Option<HashMap<PathBuf, char>>> = std::sync::RwLock::new(None);

/// Custom folder glyphs: folder path -> a glyph of the bundled icon font, saved in
/// `~/.config/compression/folder_glyphs.json`. Held globally, like the icon style, so every place a folder's
/// icon is drawn can look it up.
fn with_glyphs<R>(f: impl FnOnce(&mut HashMap<PathBuf, char>) -> R) -> R {
    let mut guard = FOLDER_GLYPHS.write().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(|| {
        config_file("folder_glyphs.json")
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    });
    f(map)
}

pub fn folder_glyph(folder: &std::path::Path) -> Option<char> {
    with_glyphs(|m| m.get(folder).copied())
}

/// Set (or with `None` remove) a folder's glyph; true when something changed.
pub fn set_folder_glyph(folder: &std::path::Path, glyph: Option<char>) -> bool {
    with_glyphs(|m| {
        let changed = match glyph {
            Some(c) => m.insert(folder.to_path_buf(), c) != Some(c),
            None => m.remove(folder).is_some(),
        };
        if changed && let Some(file) = user_file_to_write("folder_glyphs.json") {
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_string_pretty(&*m) {
                let _ = crate::fs::atomic::write(&file, json);
            }
        }
        changed
    })
}

/// Where `path` is after `moves` (pairs of where an item was and where it is): the pair whose old path is the closest
/// one above `path` (or `path` itself) decides, so a folder that moved takes what is below it along. `None` when `path`
/// did not move. Every path is looked up in the original pairs, so items that swapped places do not undo each other.
pub fn moved_to(moves: &[(PathBuf, PathBuf)], path: &std::path::Path) -> Option<PathBuf> {
    moves
        .iter()
        .filter(|(old, _)| path.starts_with(old))
        .max_by_key(|(old, _)| old.components().count())
        .map(|(old, new)| new.join(path.strip_prefix(old).unwrap_or(std::path::Path::new(""))))
}

/// Folders that were renamed or moved keep their glyph.
pub fn move_folder_glyphs(moves: &[(PathBuf, PathBuf)]) {
    with_glyphs(|m| {
        let moved: HashMap<PathBuf, char> = m.iter().map(|(folder, glyph)| (moved_to(moves, folder).unwrap_or_else(|| folder.clone()), *glyph)).collect();
        if moved == *m {
            return;
        }
        *m = moved;
        if let Some(file) = user_file_to_write("folder_glyphs.json") {
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_string_pretty(&*m) {
                let _ = crate::fs::atomic::write(&file, json);
            }
        }
    });
}

/// A file of the settings folder, to read. An administrator window reads the folder of the person who
/// started it, like the settings do, and so shows the same pins.
fn config_file(name: &str) -> Option<PathBuf> {
    config_file_in(crate::fs::admin::config_override(), dirs::config_dir(), name)
}

fn config_file_in(overridden: Option<PathBuf>, default: Option<PathBuf>, name: &str) -> Option<PathBuf> {
    overridden.or(default).map(|d| d.join("compression").join(name))
}

/// The same file, to write. Root only reads the person's files: it must not leave any in their home.
fn config_file_to_write(name: &str) -> Option<PathBuf> {
    config_file(name).filter(|_| !crate::fs::admin::is_admin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_moves_with_the_closest_folder_above_it_that_moved_and_only_by_whole_names() {
        let pair = |a: &str, b: &str| (PathBuf::from(a), PathBuf::from(b));
        let moves = [pair("/a/dir", "/b/dir"), pair("/a/dir/inner", "/c/inner")];
        let to = |p: &str| moved_to(&moves, std::path::Path::new(p));
        assert_eq!(to("/a/dir"), Some(PathBuf::from("/b/dir")));
        assert_eq!(to("/a/dir/file"), Some(PathBuf::from("/b/dir/file")));
        // The closer pair decides
        assert_eq!(to("/a/dir/inner"), Some(PathBuf::from("/c/inner")));
        assert_eq!(to("/a/dir/inner/deep/x"), Some(PathBuf::from("/c/inner/deep/x")));
        // Letters alone do not make a path inside; what did not move stays
        assert_eq!(to("/a/dir2"), None);
        assert_eq!(to("/a"), None);
        assert_eq!(to("/z"), None);
        assert_eq!(moved_to(&[], std::path::Path::new("/a")), None);
    }
    use std::path::Path;

    #[test]
    fn an_administrator_window_reads_the_settings_folder_of_the_person_who_started_it() {
        let user = Some(PathBuf::from("/home/me/.config"));
        let root = Some(PathBuf::from("/root/.config"));
        assert_eq!(config_file_in(user.clone(), root.clone(), "pins.json"), Some(PathBuf::from("/home/me/.config/compression/pins.json")));
        // Without the override (an ordinary start) it is the folder of whoever runs the program
        assert_eq!(config_file_in(None, root.clone(), "pins.json"), Some(PathBuf::from("/root/.config/compression/pins.json")));
        assert_eq!(config_file_in(None, None, "pins.json"), None);
        // This test process is not root: it may write what it reads
        if !crate::fs::admin::is_admin() {
            assert_eq!(config_file_to_write("pins.json"), config_file("pins.json"));
        }
    }

    #[test]
    fn legacy_view_names_map_to_the_current_ones() {
        assert_eq!(normalize(PathBuf::from("ski:recent")), PathBuf::from("compression:recent"));
        assert_eq!(normalize(PathBuf::from("ski:bookmarks")), PathBuf::from("compression:bookmarks"));
        assert_eq!(normalize(PathBuf::from("compression:recent")), PathBuf::from(RECENT_PATH));
        assert_eq!(normalize(PathBuf::from("/home/me/ski:recent")), PathBuf::from("/home/me/ski:recent"));
        assert!(is_virtual(&normalize(PathBuf::from("ski:recent"))));
        assert!(is_virtual(&normalize(PathBuf::from("ski:bookmarks"))));
        assert!(!is_virtual(Path::new("ski:recent")));
    }
}
