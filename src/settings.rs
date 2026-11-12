//! User preferences, stored as JSON in `~/.config/compression/settings.json`.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::app::ViewMode;
use crate::i18n::tr;
use crate::fs::places::SidebarPlace;
use crate::fs::scanner::{SortBy, SortOrder};
use crate::ui::icon_style::IconStyle;
use crate::ui::material::Column;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Palette name: `system` follows the Serpantinum colors, `custom:<id>` is the user's theme in
    /// `~/.local/state/compression/themes/<id>.json`, otherwise the id of a file in `assets/themes`.
    pub theme: String,
    /// `system` follows the desktop language, otherwise a code from `i18n::Language`.
    pub language: String,
    /// Accent color as `#rrggbb`; `None` keeps the palette's own.
    pub accent: Option<String>,
    /// Font file for the interface; `None` is the bundled Adwaita Sans.
    pub font: Option<PathBuf>,
    pub font_name: Option<String>,
    pub scale: f32,
    /// How round the corners are, relative to the default: 1.0 is the Material look, 0.0 square.
    pub corner_scale: f32,
    /// How opaque the window is, 1 to 100 percent.
    pub opacity: u8,
    /// How folders, file icons and glyphs are drawn.
    pub icon_style: IconStyle,

    pub view_mode: ViewMode,
    /// The file dialog's own list or grid, kept apart from the main window's.
    pub chooser_view_mode: ViewMode,
    pub sort_by: SortBy,
    pub sort_order: SortOrder,
    pub show_hidden: bool,
    pub folders_first: bool,
    pub columns: Vec<Column>,
    /// Dragged list column widths, indexed like `Column::ALL`; 0 keeps the default.
    pub column_widths: [f32; 6],

    // Startup
    pub start_location: StartLocation,
    pub start_path: Option<PathBuf>,
    pub last_dir: Option<PathBuf>,
    /// The folders of the open tabs and which one was shown, kept for `StartLocation::RestoreTabs`.
    pub session_tabs: Vec<PathBuf>,
    pub session_active: usize,
    // Behavior
    pub single_click_open: bool,
    pub vim_keys: bool,
    pub open_in_new_window: bool,
    /// Stay running after the window closes, so the next one opens at once.
    pub run_in_background: bool,
    pub confirm_delete: bool,
    /// Copies to the same disk run one after the other.
    pub queue_copies: bool,
    /// Command that opens a terminal; `{}` stands for the folder. Empty finds one by itself.
    pub terminal: String,
    /// Folder whose files are offered as templates for new files; `None` is the desktop's own.
    pub templates_dir: Option<PathBuf>,
    pub type_to_filter: bool,
    /// Typing a name selects the matching file of the folder (unless `type_to_filter` takes the keys).
    pub type_ahead: bool,
    /// A sideways two-finger swipe over the listing goes back or forward.
    pub swipe_navigation: bool,
    /// How far one notch of the mouse wheel scrolls, as a multiple of `SCROLL_STEP` points.
    pub scroll_speed: f32,
    /// Switches of the bundled plugins by id; a plugin not listed keeps its default.
    pub plugins: std::collections::BTreeMap<String, bool>,
    /// Where searches look, and whether they match case or look inside files.
    pub search_scope: crate::fs::search::SearchScope,
    pub search_case: bool,
    pub search_contents: bool,
    // View
    pub density: Density,
    pub grid_size: GridSize,
    pub grid_name_lines: u8,
    pub show_sidebar: bool,
    /// Every disk and partition in the sidebar's Devices, internal ones too; off lists only what is
    /// plugged in or mounted under `/run/media`, `/media` or `/mnt`, beside Computer.
    pub show_all_partitions: bool,
    /// Built-in sidebar places the user hid, from the sidebar's menu or Settings → Sidebar.
    pub hidden_places: Vec<SidebarPlace>,
    /// Places off by default (`SidebarPlace::opt_in`) that the user turned on.
    pub shown_places: Vec<SidebarPlace>,
    pub show_status_bar: bool,
    pub show_thumbnails: bool,
    /// Previews and folder item counts also for files on servers, which reads them over the network.
    pub remote_previews: bool,
    /// Folders (mount points) the person chose to treat as servers, though Compression does not know them as such:
    /// nothing in the window waits for them, they get no previews, and nothing on them is replaced. Only set by hand.
    pub remote_folders: Vec<PathBuf>,
    /// Programs (AppImages) the person said to run without asking, each as it was then: a file that changed since is
    /// asked about again.
    pub trusted_programs: Vec<TrustedProgram>,
    pub show_extensions: bool,
    pub relative_dates: bool,
    pub decimal_units: bool,
    pub folder_sizes: FolderSizeMode,
    /// Folder sizes show the space used on disk instead of the sum of file lengths.
    pub size_on_disk: bool,
    pub animations: bool,
    pub remember_view_per_folder: bool,
    pub folder_views: std::collections::HashMap<PathBuf, ViewMode>,
    // Keybindings: overrides only; missing actions use `Action::default_binding`.
    pub keybinds: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StartLocation {
    #[default]
    Launch,
    Home,
    LastVisited,
    Custom,
    /// Every tab that was open when Compression closed.
    RestoreTabs,
}

/// When folders get a size in the listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FolderSizeMode {
    Off,
    /// Only the folders you ask about ("Calculate size").
    #[default]
    OnDemand,
    /// Every folder in the listing.
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Density {
    Compact,
    #[default]
    Comfortable,
    Spacious,
}

impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Comfortable, Density::Spacious];

    pub fn label(self) -> &'static str {
        match self {
            Density::Compact => tr("Compact"),
            Density::Comfortable => tr("Comfortable"),
            Density::Spacious => tr("Spacious"),
        }
    }

    /// One step larger (`1`) or smaller (`-1`), stopping at the ends.
    pub fn step(self, delta: i32) -> Self {
        let at = Self::ALL.iter().position(|d| *d == self).unwrap_or(1) as i32;
        Self::ALL[(at + delta).clamp(0, Self::ALL.len() as i32 - 1) as usize]
    }

    pub fn row_height(self) -> f32 {
        match self {
            Density::Compact => 30.0,
            Density::Comfortable => 38.0,
            Density::Spacious => 48.0,
        }
    }
}

/// Size of the picture box of a grid tile, in points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct GridSize(u32);

impl GridSize {
    pub const MIN: u32 = 32;
    pub const MAX: u32 = 256;
    const STEP: u32 = 16;

    /// Older settings stored the size as a name.
    fn from_name(name: &str) -> Option<Self> {
        Some(Self(match name {
            "Small" => 56,
            "Medium" => 72,
            "Large" => 96,
            _ => return None,
        }))
    }

    pub fn label(self) -> String {
        format!("{} px", self.0)
    }

    /// One step larger (`1`) or smaller (`-1`), stopping at the ends.
    pub fn step(self, delta: i32) -> Self {
        let next = self.0 as i32 + delta * Self::STEP as i32;
        Self(next.clamp(Self::MIN as i32, Self::MAX as i32) as u32)
    }

    pub fn can_step(self, delta: i32) -> bool {
        self.step(delta) != self
    }

    pub fn cell_size(self) -> (f32, f32) {
        let w = (self.0 as f32 + 44.0).max(92.0);
        (w, w + 12.0)
    }

    pub fn icon_size(self) -> f32 {
        self.0 as f32 * 0.92
    }

    pub fn picture_size(self) -> f32 {
        self.0 as f32
    }
}

impl Default for GridSize {
    fn default() -> Self {
        Self(72)
    }
}

impl<'de> Deserialize<'de> for GridSize {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Px(u32),
            Name(String),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Px(px) => Self(px.clamp(Self::MIN, Self::MAX)),
            Raw::Name(name) => Self::from_name(&name).unwrap_or_default(),
        })
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "system".to_owned(),
            language: "system".to_owned(),
            terminal: String::new(),
            templates_dir: None,
            accent: None,
            font: None,
            font_name: None,
            scale: 1.0,
            corner_scale: 1.0,
            opacity: 100,
            icon_style: IconStyle::default(),
            view_mode: ViewMode::List,
            chooser_view_mode: ViewMode::List,
            sort_by: SortBy::Name,
            sort_order: SortOrder::Ascending,
            show_hidden: false,
            folders_first: true,
            columns: vec![Column::Size, Column::Modified],
            column_widths: [0.0; 6],
            start_location: StartLocation::Launch,
            start_path: None,
            last_dir: None,
            session_tabs: Vec::new(),
            session_active: 0,
            single_click_open: false,
            vim_keys: false,
            open_in_new_window: false,
            run_in_background: true,
            confirm_delete: true,
            queue_copies: true,
            type_to_filter: false,
            type_ahead: true,
            swipe_navigation: false,
            scroll_speed: SCROLL_SPEED_DEFAULT,
            plugins: Default::default(),
            search_scope: Default::default(),
            search_case: false,
            search_contents: false,
            density: Density::Comfortable,
            grid_size: GridSize::default(),
            grid_name_lines: 2,
            show_sidebar: true,
            show_all_partitions: true,
            hidden_places: Vec::new(),
            shown_places: Vec::new(),
            show_status_bar: true,
            show_thumbnails: true,
            remote_previews: false,
            remote_folders: Vec::new(),
            trusted_programs: Vec::new(),
            show_extensions: true,
            relative_dates: true,
            decimal_units: false,
            folder_sizes: FolderSizeMode::default(),
            size_on_disk: false,
            animations: true,
            remember_view_per_folder: false,
            folder_views: Default::default(),
            keybinds: Default::default(),
        }
    }
}

/// Points one wheel notch scrolls at speed 1. egui's own 40 felt slow beside other file managers.
pub const SCROLL_STEP: f32 = 60.0;
pub const SCROLL_SPEED_DEFAULT: f32 = 1.0;
/// What the slider in the settings covers; `settings.json` may hold any value in `SCROLL_SPEED_RANGE`.
pub const SCROLL_SPEED_SLIDER: std::ops::RangeInclusive<f32> = 0.25..=2.25;
pub const SCROLL_SPEED_RANGE: std::ops::RangeInclusive<f32> = 0.1..=5.0;

/// A program file the person trusts to run without a question, as it was when they said so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedProgram {
    pub path: PathBuf,
    pub size: u64,
    /// Modification time in seconds since the epoch.
    pub modified: i64,
}

impl TrustedProgram {
    /// The program at `path` as it is now; `None` when it cannot be looked at.
    pub fn of(path: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = path.symlink_metadata().ok()?;
        Some(Self { path: path.to_path_buf(), size: meta.len(), modified: meta.mtime() })
    }
}

pub(crate) fn file() -> Option<PathBuf> {
    crate::fs::admin::config_override().or_else(dirs::config_dir).map(|d| d.join("compression").join("settings.json"))
}

#[cfg(test)]
thread_local! {
    /// Set by a test that runs a whole window on its thread: its tabs and layout are not kept in the
    /// settings file.
    pub(crate) static NO_SAVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set by a test that wants every save to fail, and how many saves were asked for on this thread.
    pub(crate) static FAIL_SAVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static SAVES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Settings {
    pub fn load() -> Self {
        Self::read().and_then(Result::ok).unwrap_or_default().for_session(crate::fs::admin::is_admin())
    }

    /// The settings as the file has them now: `None` when there is no file to read, an error when
    /// it is not valid JSON (say, half way through an edit by hand).
    pub fn read() -> Option<Result<Self, serde_json::Error>> {
        let text = std::fs::read_to_string(file()?).ok()?;
        Some(Self::parse(&text))
    }

    /// Settings from the text of a file, with the names and the rights of this session applied.
    fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str::<Settings>(text).map(|s| s.with_current_view_names().for_session(crate::fs::admin::is_admin()))
    }

    /// The parts of the settings that change as a window is used and not because a person chose
    /// them: where it was last, how wide its columns are, which view each folder has.
    fn without_state(&self) -> Self {
        Self { last_dir: None, column_widths: [0.0; 6], folder_views: Default::default(), ..self.clone() }
    }

    /// Whether `other` holds the same choices, leaving the state of the window out of it.
    pub fn same_choices_as(&self, other: &Self) -> bool {
        self.without_state() == other.without_state()
    }

    /// The administrator's window reads the settings of the person who started it, but runs as
    /// root: a command in them, which that person (or a program of theirs) can write, must not
    /// run with root's rights. The terminal falls back to the one found on this system.
    fn for_session(mut self, admin: bool) -> Self {
        if admin {
            self.terminal.clear();
        }
        self
    }

    /// Settings saved by Ski name the Recent and Bookmarks views `ski:recent` and `ski:bookmarks`.
    fn with_current_view_names(mut self) -> Self {
        use crate::fs::places::normalize;
        self.start_path = self.start_path.map(normalize);
        self.last_dir = self.last_dir.map(normalize);
        self.session_tabs = self.session_tabs.into_iter().map(normalize).collect();
        self.folder_views = self.folder_views.into_iter().map(|(path, view)| (normalize(path), view)).collect();
        self
    }

    /// The tabs of the last session that can still be opened, and the position of the one that was
    /// shown among them. Folders that no longer exist are dropped.
    pub fn restorable_session(&self) -> (Vec<PathBuf>, usize) {
        let kept: Vec<PathBuf> = self.session_tabs.iter().filter(|p| p.is_dir()).cloned().collect();
        let active = self.session_tabs.get(self.session_active).filter(|p| p.is_dir());
        let shown = active.map_or(0, |_| self.session_tabs[..self.session_active].iter().filter(|p| p.is_dir()).count());
        let shown = shown.min(kept.len().saturating_sub(1));
        (kept, shown)
    }

    /// Writes the settings file. An error means the file was not written (full disk, no rights, a folder in the way);
    /// the caller must not count the settings as saved then.
    /// Writes the settings. `Some` when the file that was there could not be read and was first kept aside, under
    /// the name given (see `set_aside_if_broken`).
    pub fn save(&self) -> std::io::Result<Option<PathBuf>> {
        // As administrator the settings belong to someone else's home
        if crate::fs::admin::is_admin() {
            return Ok(None);
        }
        #[cfg(test)]
        #[allow(clippy::needless_return)]
        {
            SAVES.with(|n| n.set(n.get() + 1));
            if FAIL_SAVE.with(std::cell::Cell::get) {
                return Err(std::io::Error::from_raw_os_error(libc::EACCES));
            }
            // A test never writes the settings of whoever runs it (`NO_SAVE` stays for the tests that set it)
            let _ = NO_SAVE.with(std::cell::Cell::get);
            return Ok(None);
        }
        #[cfg(not(test))]
        {
            let Some(file) = file() else { return Ok(None) };
            self.save_to(&file)
        }
    }

    /// Writes the settings to `file`. A file there that cannot be read is never written over: it is kept aside
    /// first, and when that fails nothing is written.
    pub(crate) fn save_to(&self, file: &std::path::Path) -> std::io::Result<Option<PathBuf>> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let kept = set_aside_if_broken(file)?.map(|(kept, _)| kept);
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::fs::atomic::write(file, json)?;
        Ok(kept)
    }
}

/// A settings file that is there but cannot be read (not valid JSON, a value of the wrong type, not UTF-8) is moved to
/// `settings.json.broken-<time>` beside it, so that a slip in an edit by hand costs no settings: what would be written
/// next goes to a file of its own. Returns where it went and why it could not be read; `None` when there is no file or
/// it reads well.
pub(crate) fn set_aside_if_broken(file: &std::path::Path) -> std::io::Result<Option<(PathBuf, String)>> {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let Err(err) = serde_json::from_slice::<Settings>(&bytes) else { return Ok(None) };
    let why = crate::fs::describe::json_error(&err);
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stamp = crate::fs::trash::now().replace(':', "");
    for n in 1..100 {
        let tag = if n == 1 { stamp.clone() } else { format!("{stamp}-{n}") };
        let kept = file.with_file_name(format!("{name}.broken-{tag}"));
        match crate::compat::rename_noreplace(file, &kept) {
            Ok(()) => return Ok(Some((kept, why))),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_administrator_session_ignores_the_terminal_command_of_the_settings() {
        let settings = Settings { terminal: "sh -c evil {}".into(), ..Default::default() };
        assert_eq!(settings.clone().for_session(true).terminal, "");
        assert_eq!(settings.for_session(false).terminal, "sh -c evil {}");
    }

    #[test]
    fn settings_saved_by_ski_name_the_views_by_their_current_names() {
        let json = r#"{"start_path":"ski:recent","last_dir":"ski:bookmarks","session_tabs":["ski:recent","/tmp"],"folder_views":{"ski:bookmarks":"List"}}"#;
        let settings = serde_json::from_str::<Settings>(json).unwrap().with_current_view_names();
        assert_eq!(settings.start_path, Some(PathBuf::from("compression:recent")));
        assert_eq!(settings.last_dir, Some(PathBuf::from("compression:bookmarks")));
        assert_eq!(settings.session_tabs, [PathBuf::from("compression:recent"), PathBuf::from("/tmp")]);
        assert!(settings.folder_views.contains_key(&PathBuf::from("compression:bookmarks")));
    }

    #[test]
    fn a_settings_file_is_read_with_the_current_names_and_a_broken_one_is_an_error() {
        let read = Settings::parse(r#"{"start_path":"ski:recent","session_tabs":["ski:bookmarks"]}"#).unwrap();
        assert_eq!(read.start_path, Some(PathBuf::from("compression:recent")));
        assert_eq!(read.session_tabs, [PathBuf::from("compression:bookmarks")]);
        // Half way through an edit by hand
        assert!(Settings::parse(r#"{"show_hidden": tr"#).is_err());
        assert!(Settings::parse("").is_err());
    }

    #[test]
    fn where_a_window_was_and_how_wide_its_columns_are_is_no_choice() {
        let base = Settings::default();
        let moved = Settings { last_dir: Some(PathBuf::from("/tmp")), column_widths: [120.0; 6], folder_views: [(PathBuf::from("/x"), crate::app::ViewMode::Grid)].into(), ..base.clone() };
        assert!(base.same_choices_as(&moved));
        // Anything a person picks is
        assert!(!base.same_choices_as(&Settings { show_hidden: !base.show_hidden, ..moved.clone() }));
        assert!(!base.same_choices_as(&Settings { theme: "nord".into(), ..base.clone() }));
    }

    #[test]
    fn the_wheel_speed_defaults_round_trips_and_may_be_missing_from_an_old_file() {
        assert_eq!(Settings::default().scroll_speed, SCROLL_SPEED_DEFAULT);
        assert_eq!(SCROLL_SPEED_DEFAULT, 1.0);
        assert!(SCROLL_SPEED_SLIDER.start() >= SCROLL_SPEED_RANGE.start() && SCROLL_SPEED_SLIDER.end() <= SCROLL_SPEED_RANGE.end());
        // A file from before the setting existed
        let old: Settings = serde_json::from_str(r#"{ "theme": "nord" }"#).unwrap();
        assert_eq!(old.scroll_speed, SCROLL_SPEED_DEFAULT);
        assert_eq!(old.theme, "nord");
        // It is kept
        let s = Settings { scroll_speed: 1.75, ..Settings::default() };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.scroll_speed, 1.75);
        // Whatever a hand-edited file holds is loaded as it is (the window clamps it when it applies it)
        let wild: Settings = serde_json::from_str(r#"{ "scroll_speed": 99.0 }"#).unwrap();
        assert_eq!(wild.scroll_speed, 99.0);
        // Not a number at all: the file is refused as a whole, like any other wrong type
        assert!(serde_json::from_str::<Settings>(r#"{ "scroll_speed": "fast" }"#).is_err());
    }

    #[test]
    fn saving_to_a_place_that_cannot_hold_the_file_is_an_error_and_saving_to_a_good_one_is_not() {
        let base = std::env::temp_dir().join(format!("compression_settings_save_to_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let settings = Settings { show_hidden: true, ..Settings::default() };
        // A file where the folder should be
        std::fs::write(base.join("blocked"), "x").unwrap();
        assert!(settings.save_to(&base.join("blocked").join("settings.json")).is_err());
        // A folder that is made on the way
        let good = base.join("new").join("compression").join("settings.json");
        settings.save_to(&good).unwrap();
        assert!(Settings::parse(&std::fs::read_to_string(&good).unwrap()).unwrap().show_hidden);
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn broken_kept(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut kept: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().contains("settings.json.broken-")).collect();
        kept.sort();
        kept
    }

    #[test]
    fn a_settings_file_that_cannot_be_read_is_kept_aside_and_one_that_can_is_left_alone() {
        let base = std::env::temp_dir().join(format!("compression_settings_broken_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("settings.json");
        // No file, and a good one: nothing to do
        assert!(set_aside_if_broken(&file).unwrap().is_none());
        std::fs::write(&file, r#"{ "show_hidden": true }"#).unwrap();
        assert!(set_aside_if_broken(&file).unwrap().is_none());
        assert!(file.exists() && broken_kept(&base).is_empty());
        // Not valid JSON, a value of the wrong type, not UTF-8: each is kept whole under a name of its own
        let mut seen = Vec::new();
        for bytes in [&b"{ \"show_hidden\": tru"[..], br#"{ "show_hidden": "yes" }"#, b"\xff\xfe{}"] {
            std::fs::write(&file, bytes).unwrap();
            let (kept, why) = set_aside_if_broken(&file).unwrap().expect("kept aside");
            assert!(!file.exists(), "the broken file is moved, not copied");
            assert_eq!(std::fs::read(&kept).unwrap(), bytes, "kept as it was");
            assert!(kept.file_name().unwrap().to_string_lossy().starts_with("settings.json.broken-"), "{kept:?}");
            assert!(!why.is_empty());
            seen.push(kept);
        }
        assert_eq!(broken_kept(&base), { let mut s = seen.clone(); s.sort(); s }, "three files, none written over");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn saving_over_a_file_that_cannot_be_read_keeps_it_first() {
        let base = std::env::temp_dir().join(format!("compression_settings_save_broken_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("settings.json");
        std::fs::write(&file, "{ \"language\": \"de\", oops").unwrap();
        let kept = Settings { show_hidden: true, ..Settings::default() }.save_to(&file).unwrap().expect("kept aside");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "{ \"language\": \"de\", oops");
        assert!(Settings::parse(&std::fs::read_to_string(&file).unwrap()).unwrap().show_hidden, "the new file is whole");
        // A good file is simply written over
        assert!(Settings::default().save_to(&file).unwrap().is_none());
        assert_eq!(broken_kept(&base), [kept]);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
