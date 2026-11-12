use std::fs;
use eframe::egui::{self, Color32};
use std::path::{Path, PathBuf};
use crate::i18n::{tr, trf};
use std::sync::{OnceLock, RwLock};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct SerpantinumColorsRaw {
    pub base: Option<String>,
    pub mantle: Option<String>,
    pub crust: Option<String>,
    pub text: Option<String>,
    pub subtext0: Option<String>,
    pub subtext1: Option<String>,
    pub surface0: Option<String>,
    pub surface1: Option<String>,
    pub surface2: Option<String>,
    pub blue: Option<String>,
    pub mauve: Option<String>,
    pub peach: Option<String>,
    pub green: Option<String>,
    pub red: Option<String>,
    pub yellow: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SerpantinumTheme {
    pub base: Color32,
    pub mantle: Color32,
    pub crust: Color32,
    pub text: Color32,
    pub subtext0: Color32,
    pub subtext1: Color32,
    pub surface0: Color32,
    pub surface1: Color32,
    pub surface2: Color32,
    pub blue: Color32,
    pub mauve: Color32,
    pub peach: Color32,
    pub green: Color32,
    pub red: Color32,
    pub yellow: Color32,
    pub border_radius: f32,
    pub light: bool,
}

impl Default for SerpantinumTheme {
    fn default() -> Self {
        ThemeColors::default().to_theme()
    }
}

/// The `border_radius` of the unchanged Material look; the corner setting scales it.
pub const BASE_RADIUS: f32 = 16.0;

/// The colors of a palette as `#rrggbb`: the shape of every file in `assets/themes` and of
/// `custom_theme` in the settings. A color that is missing or unreadable keeps the Dark palette's.
/// `accent` colors both the accent slots of the theme.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeColors {
    pub light: bool,
    pub base: String,
    pub mantle: String,
    pub crust: String,
    pub text: String,
    pub subtext0: String,
    pub subtext1: String,
    pub surface0: String,
    pub surface1: String,
    pub surface2: String,
    pub accent: String,
    pub peach: String,
    pub green: String,
    pub red: String,
    pub yellow: String,
}

impl Default for ThemeColors {
    /// The Dark palette; kept in code as the fallback every other palette is read against.
    fn default() -> Self {
        let hex = |s: &str| s.to_owned();
        Self {
            light: false,
            base: hex("#0f0d13"),
            mantle: hex("#1c1b20"),
            crust: hex("#141318"),
            text: hex("#e6e1e9"),
            subtext0: hex("#cac4cf"),
            subtext1: hex("#938f99"),
            surface0: hex("#201f24"),
            surface1: hex("#2b292f"),
            surface2: hex("#36343a"),
            accent: hex("#ccbeff"),
            peach: hex("#eeb8cb"),
            green: hex("#cac3dc"),
            red: hex("#ffb4ab"),
            yellow: hex("#f5d76e"),
        }
    }
}

impl ThemeColors {
    pub fn to_theme(&self) -> SerpantinumTheme {
        let dark = Self::default();
        let col = |mine: &str, dark: &str| parse_hex(mine).or_else(|| parse_hex(dark)).unwrap_or(Color32::GRAY);
        let accent = col(&self.accent, &dark.accent);
        SerpantinumTheme {
            base: col(&self.base, &dark.base),
            mantle: col(&self.mantle, &dark.mantle),
            crust: col(&self.crust, &dark.crust),
            text: col(&self.text, &dark.text),
            subtext0: col(&self.subtext0, &dark.subtext0),
            subtext1: col(&self.subtext1, &dark.subtext1),
            surface0: col(&self.surface0, &dark.surface0),
            surface1: col(&self.surface1, &dark.surface1),
            surface2: col(&self.surface2, &dark.surface2),
            blue: accent,
            mauve: accent,
            peach: col(&self.peach, &dark.peach),
            green: col(&self.green, &dark.green),
            red: col(&self.red, &dark.red),
            yellow: col(&self.yellow, &dark.yellow),
            border_radius: BASE_RADIUS,
            light: self.light,
        }
    }

    /// The palette of a finished theme, to start a custom one from.
    pub fn from_theme(t: &SerpantinumTheme) -> Self {
        Self {
            light: t.light,
            base: to_hex(t.base),
            mantle: to_hex(t.mantle),
            crust: to_hex(t.crust),
            text: to_hex(t.text),
            subtext0: to_hex(t.subtext0),
            subtext1: to_hex(t.subtext1),
            surface0: to_hex(t.surface0),
            surface1: to_hex(t.surface1),
            surface2: to_hex(t.surface2),
            accent: to_hex(t.mauve),
            peach: to_hex(t.peach),
            green: to_hex(t.green),
            red: to_hex(t.red),
            yellow: to_hex(t.yellow),
        }
    }

    /// The color fields by their key in the file, with the name the editor shows for each.
    pub fn fields(&mut self) -> [(&'static str, &'static str, &mut String); 14] {
        [
            ("base", "Background", &mut self.base),
            ("mantle", "Panels", &mut self.mantle),
            ("crust", "Deepest background", &mut self.crust),
            ("surface0", "Cards", &mut self.surface0),
            ("surface1", "Raised cards", &mut self.surface1),
            ("surface2", "Hover fill", &mut self.surface2),
            ("text", "Text", &mut self.text),
            ("subtext0", "Secondary text", &mut self.subtext0),
            ("subtext1", "Faint text", &mut self.subtext1),
            ("accent", "Accent", &mut self.accent),
            ("peach", "Warm accent", &mut self.peach),
            ("green", "Green", &mut self.green),
            ("red", "Red", &mut self.red),
            ("yellow", "Yellow", &mut self.yellow),
        ]
    }
}

/// A palette the user made: a file in the state directory (see `user_themes_dir`) whose name,
/// without `.json`, is its id; it is selected as `custom:<id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomTheme {
    /// Not in the file: the file's name.
    #[serde(default, skip_serializing)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub colors: ThemeColors,
}

/// Why a theme file could not be used.
#[derive(Debug, Clone, PartialEq)]
pub enum Why {
    /// The folder or the file could not be read: the system's error number, and its text when it has none of ours.
    Unreadable(Option<i32>, String),
    /// Not JSON at all (a typo); the line where it stopped making sense.
    NotJson(usize),
    /// JSON, but a value of the wrong kind; the line.
    WrongValue(usize),
    /// A `.json` file whose name cannot be an id.
    Name,
}

/// A theme file that was there but could not be loaded, so it is missing from the list.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeProblem {
    pub file: String,
    pub why: Why,
}

impl ThemeProblem {
    fn unreadable(file: String, err: &std::io::Error) -> Self {
        Self { file, why: Why::Unreadable(err.raw_os_error(), err.to_string()) }
    }

    fn broken(file: String, err: &serde_json::Error) -> Self {
        let why = if err.is_data() { Why::WrongValue(err.line()) } else { Why::NotJson(err.line()) };
        Self { file, why }
    }

    /// `name.json (reason)`, in the language of the window.
    pub fn describe(&self) -> String {
        let reason = match &self.why {
            Why::Unreadable(Some(errno), _) => crate::fs::describe::io_error(&std::io::Error::from_raw_os_error(*errno)),
            Why::Unreadable(None, text) => text.clone(),
            Why::NotJson(line) => crate::fs::describe::json_words(false, *line),
            Why::WrongValue(line) => crate::fs::describe::json_words(true, *line),
            Why::Name => tr("its name cannot be used").to_owned(),
        };
        format!("{} ({reason})", self.file)
    }
}

/// The user's themes as last read from their folder, for `from_settings` and the settings page.
static USER_THEMES: RwLock<Vec<CustomTheme>> = RwLock::new(Vec::new());
/// The files of that folder that could not be loaded, as of the same reading.
static USER_THEME_PROBLEMS: RwLock<Vec<ThemeProblem>> = RwLock::new(Vec::new());
/// What was wrong with the Serpantinum files at the last `SerpantinumTheme::load`.
static SERPANTINUM_PROBLEMS: RwLock<Vec<ThemeProblem>> = RwLock::new(Vec::new());

thread_local! {
    /// Lets a test name the themes folder without touching the environment, which the other tests share.
    /// (Not behind `cfg(test)`: the translation scan reads a source file only up to its first one.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) static THEMES_DIR_FOR_TESTS: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

thread_local! {
    /// What `problems` reports in a test, which then needs no files and no shared state.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) static PROBLEMS_FOR_TESTS: std::cell::RefCell<Option<Vec<ThemeProblem>>> = const { std::cell::RefCell::new(None) };
}

/// Held by the tests that read or replace `USER_THEMES`, which every test shares.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where the user's themes are kept, a JSON file each: `~/.local/state/compression/themes`.
pub fn user_themes_dir() -> Option<PathBuf> {
    if let Some(dir) = THEMES_DIR_FOR_TESTS.with(|d| d.borrow().clone()) {
        return Some(dir);
    }
    let state = match crate::fs::admin::config_override() {
        // As administrator: the state folder next to the config folder of whoever started Compression
        Some(config) => config.parent().map(|home| home.join(".local/state")),
        None => dirs::state_dir().or_else(dirs::data_local_dir),
    };
    state.map(|dir| dir.join("compression").join("themes"))
}

/// The themes in `dir` and the files in it that could not be loaded. A folder that is not there is
/// no problem (nothing was saved yet), and neither is a file that is gone by the time it is read;
/// a file that does not end in `.json` is not a theme. Everything else that goes wrong is reported.
fn read_themes_in(dir: &Path) -> (Vec<CustomTheme>, Vec<ThemeProblem>) {
    let mut themes = Vec::new();
    let mut problems = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (themes, problems),
        Err(err) => {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return (themes, vec![ThemeProblem::unreadable(name, &err)]);
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                problems.push(ThemeProblem::unreadable(String::new(), &err));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") || path.is_dir() {
            continue;
        }
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            problems.push(ThemeProblem { file, why: Why::Name });
            continue;
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                problems.push(ThemeProblem::unreadable(file, &err));
                continue;
            }
        };
        match serde_json::from_slice::<CustomTheme>(&bytes) {
            Ok(mut theme) => {
                if theme.name.trim().is_empty() {
                    theme.name = id.clone();
                }
                theme.id = id;
                themes.push(theme);
            }
            Err(err) => problems.push(ThemeProblem::broken(file, &err)),
        }
    }
    themes.sort_by_key(|t| (t.name.to_lowercase(), t.id.clone()));
    problems.sort_by(|a, b| a.file.cmp(&b.file));
    (themes, problems)
}

/// Reads the folder again; true when the themes (or what is wrong with the files) are not what they were.
pub fn reload_user_themes() -> bool {
    let (read, problems) = match user_themes_dir() {
        Some(dir) => read_themes_in(&dir),
        None => (Vec::new(), Vec::new()),
    };
    let mut current = USER_THEMES.write().unwrap_or_else(|e| e.into_inner());
    let mut known = USER_THEME_PROBLEMS.write().unwrap_or_else(|e| e.into_inner());
    let changed = *current != read || *known != problems;
    *current = read;
    *known = problems;
    changed
}

pub fn user_themes() -> Vec<CustomTheme> {
    USER_THEMES.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Everything that is wrong with the files themes come from, as of the last reading.
pub fn problems() -> Vec<ThemeProblem> {
    if let Some(forced) = PROBLEMS_FOR_TESTS.with(|p| p.borrow().clone()) {
        return forced;
    }
    let mut all = USER_THEME_PROBLEMS.read().unwrap_or_else(|e| e.into_inner()).clone();
    all.extend(SERPANTINUM_PROBLEMS.read().unwrap_or_else(|e| e.into_inner()).iter().cloned());
    all
}

/// One line for the settings page and the toast: the first three files with the reason, then `+N`.
pub fn problems_notice(problems: &[ThemeProblem]) -> Option<String> {
    if problems.is_empty() {
        return None;
    }
    let mut files: Vec<String> = problems.iter().take(3).map(ThemeProblem::describe).collect();
    if problems.len() > 3 {
        files.push(format!("+{}", problems.len() - 3));
    }
    Some(trf("Could not load these theme files: {}", &[&files.join(", ")]))
}

/// True when the chosen theme is neither one of the user's, nor a bundled one, nor "system":
/// the window then shows the system colors, and the page says so.
pub fn selected_is_missing(id: &str) -> bool {
    id != "system" && preset(id).is_none() && !id.strip_prefix("custom:").is_some_and(|c| user_themes().iter().any(|t| t.id == c))
}

fn theme_file(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn not_as_administrator() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, tr("Themes cannot be changed in administrator mode"))
}

/// Writes the theme to its file, whole or not at all; the old file stays as it was when this fails.
pub fn save_user_theme(theme: &CustomTheme) -> std::io::Result<()> {
    // As administrator the folder belongs to someone else
    if crate::fs::admin::is_admin() {
        return Err(not_as_administrator());
    }
    let dir = user_themes_dir().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, tr("There is no folder for the themes")))?;
    fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(theme).map_err(std::io::Error::other)?;
    crate::fs::atomic::write(&theme_file(&dir, &theme.id), json)
}

/// Removes the theme's file. A file that is gone already is what was asked for.
pub fn delete_user_theme(id: &str) -> std::io::Result<()> {
    if crate::fs::admin::is_admin() {
        return Err(not_as_administrator());
    }
    let Some(dir) = user_themes_dir() else { return Ok(()) };
    match fs::remove_file(theme_file(&dir, id)) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// A palette bundled in `assets/themes`; the file name is its id.
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// Listed under "Browse more themes" instead of right in the settings.
    pub more: bool,
    pub colors: ThemeColors,
}

#[derive(Deserialize)]
struct ThemeFile {
    name: String,
    #[serde(default)]
    group: String,
    #[serde(flatten)]
    colors: ThemeColors,
}

/// Dark and Light first, then the curated palettes, then those behind "Browse more themes".
const BUNDLED: &[(&str, &str)] = &[
    ("dark", include_str!("../../assets/themes/dark.json")),
    ("light", include_str!("../../assets/themes/light.json")),
    ("midnight", include_str!("../../assets/themes/midnight.json")),
    ("nord", include_str!("../../assets/themes/nord.json")),
    ("gruvbox", include_str!("../../assets/themes/gruvbox.json")),
    ("catppuccin", include_str!("../../assets/themes/catppuccin.json")),
    ("dracula", include_str!("../../assets/themes/dracula.json")),
    ("tokyonight", include_str!("../../assets/themes/tokyonight.json")),
    ("rosepine", include_str!("../../assets/themes/rosepine.json")),
    ("onedark", include_str!("../../assets/themes/onedark.json")),
    ("everforest", include_str!("../../assets/themes/everforest.json")),
    ("latte", include_str!("../../assets/themes/latte.json")),
    ("catppuccin-frappe", include_str!("../../assets/themes/catppuccin-frappe.json")),
    ("catppuccin-macchiato", include_str!("../../assets/themes/catppuccin-macchiato.json")),
    ("tokyonight-storm", include_str!("../../assets/themes/tokyonight-storm.json")),
    ("tokyonight-moon", include_str!("../../assets/themes/tokyonight-moon.json")),
    ("rosepine-moon", include_str!("../../assets/themes/rosepine-moon.json")),
    ("gruvbox-material", include_str!("../../assets/themes/gruvbox-material.json")),
    ("kanagawa", include_str!("../../assets/themes/kanagawa.json")),
    ("kanagawa-dragon", include_str!("../../assets/themes/kanagawa-dragon.json")),
    ("nightfox", include_str!("../../assets/themes/nightfox.json")),
    ("carbonfox", include_str!("../../assets/themes/carbonfox.json")),
    ("duskfox", include_str!("../../assets/themes/duskfox.json")),
    ("terafox", include_str!("../../assets/themes/terafox.json")),
    ("nordfox", include_str!("../../assets/themes/nordfox.json")),
    ("monokai", include_str!("../../assets/themes/monokai.json")),
    ("monokai-pro", include_str!("../../assets/themes/monokai-pro.json")),
    ("ayu-dark", include_str!("../../assets/themes/ayu-dark.json")),
    ("ayu-mirage", include_str!("../../assets/themes/ayu-mirage.json")),
    ("material-ocean", include_str!("../../assets/themes/material-ocean.json")),
    ("material-darker", include_str!("../../assets/themes/material-darker.json")),
    ("palenight", include_str!("../../assets/themes/palenight.json")),
    ("night-owl", include_str!("../../assets/themes/night-owl.json")),
    ("nightfly", include_str!("../../assets/themes/nightfly.json")),
    ("moonfly", include_str!("../../assets/themes/moonfly.json")),
    ("oceanic-next", include_str!("../../assets/themes/oceanic-next.json")),
    ("base16-ocean", include_str!("../../assets/themes/base16-ocean.json")),
    ("horizon", include_str!("../../assets/themes/horizon.json")),
    ("synthwave84", include_str!("../../assets/themes/synthwave84.json")),
    ("cobalt2", include_str!("../../assets/themes/cobalt2.json")),
    ("github-dark", include_str!("../../assets/themes/github-dark.json")),
    ("github-dimmed", include_str!("../../assets/themes/github-dimmed.json")),
    ("sonokai", include_str!("../../assets/themes/sonokai.json")),
    ("edge-dark", include_str!("../../assets/themes/edge-dark.json")),
    ("oxocarbon", include_str!("../../assets/themes/oxocarbon.json")),
    ("vesper", include_str!("../../assets/themes/vesper.json")),
    ("poimandres", include_str!("../../assets/themes/poimandres.json")),
    ("andromeda", include_str!("../../assets/themes/andromeda.json")),
    ("iceberg", include_str!("../../assets/themes/iceberg.json")),
    ("zenburn", include_str!("../../assets/themes/zenburn.json")),
    ("tomorrow-night", include_str!("../../assets/themes/tomorrow-night.json")),
    ("doom-one", include_str!("../../assets/themes/doom-one.json")),
    ("melange-dark", include_str!("../../assets/themes/melange-dark.json")),
    ("mellow", include_str!("../../assets/themes/mellow.json")),
    ("spaceduck", include_str!("../../assets/themes/spaceduck.json")),
    ("cyberdream", include_str!("../../assets/themes/cyberdream.json")),
    ("flexoki-dark", include_str!("../../assets/themes/flexoki-dark.json")),
    ("everblush", include_str!("../../assets/themes/everblush.json")),
    ("solarized-dark", include_str!("../../assets/themes/solarized-dark.json")),
    ("gruvbox-light", include_str!("../../assets/themes/gruvbox-light.json")),
    ("rosepine-dawn", include_str!("../../assets/themes/rosepine-dawn.json")),
    ("ayu-light", include_str!("../../assets/themes/ayu-light.json")),
    ("github-light", include_str!("../../assets/themes/github-light.json")),
    ("one-light", include_str!("../../assets/themes/one-light.json")),
    ("tokyonight-day", include_str!("../../assets/themes/tokyonight-day.json")),
    ("everforest-light", include_str!("../../assets/themes/everforest-light.json")),
    ("kanagawa-lotus", include_str!("../../assets/themes/kanagawa-lotus.json")),
    ("dayfox", include_str!("../../assets/themes/dayfox.json")),
    ("flexoki-light", include_str!("../../assets/themes/flexoki-light.json")),
    ("tomorrow", include_str!("../../assets/themes/tomorrow.json")),
    ("light-owl", include_str!("../../assets/themes/light-owl.json")),
    ("melange-light", include_str!("../../assets/themes/melange-light.json")),
    ("material-lighter", include_str!("../../assets/themes/material-lighter.json")),
    ("nord-light", include_str!("../../assets/themes/nord-light.json")),
    ("papercolor", include_str!("../../assets/themes/papercolor.json")),
    ("solarized-light", include_str!("../../assets/themes/solarized-light.json")),
];

/// Every bundled palette, in the order of `BUNDLED`.
pub fn presets() -> &'static [Preset] {
    static PRESETS: OnceLock<Vec<Preset>> = OnceLock::new();
    PRESETS.get_or_init(|| {
        BUNDLED
            .iter()
            .filter_map(|(id, json)| {
                let file: ThemeFile = serde_json::from_str(json).ok()?;
                // Built once and kept for the life of the program
                let name: &'static str = Box::leak(file.name.into_boxed_str());
                Some(Preset { id, name, more: file.group == "more", colors: file.colors })
            })
            .collect()
    })
}

pub fn preset(id: &str) -> Option<&'static Preset> {
    presets().iter().find(|p| p.id == id)
}

/// Quick accent choices offered in the settings.
pub const ACCENTS: [&str; 9] = ["#ccbeff", "#8ab4f8", "#88c0d0", "#a3d9a5", "#f5d76e", "#ffb784", "#ff9aa2", "#e0a3ff", "#f28b82"];

impl SerpantinumTheme {
    /// The same palette with every surface one step down, for controls that sit straight on a
    /// popup instead of on a card.
    pub fn lowered(&self) -> Self {
        Self { surface2: self.surface1, surface1: self.surface0, surface0: self.mantle, ..*self }
    }

    /// Palette picked in the settings (`system` follows Serpantinum, `custom:<id>` is one of the user's own), with the accent applied.
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        let custom = settings.theme.strip_prefix("custom:").and_then(|id| user_themes().into_iter().find(|c| c.id == id));
        let mut theme = if let Some(custom) = &custom {
            custom.colors.to_theme()
        } else if let Some(preset) = preset(&settings.theme) {
            preset.colors.to_theme()
        } else {
            Self::load()
        };
        if let Some(accent) = settings.accent.as_deref().and_then(parse_hex) {
            theme.mauve = accent;
            theme.blue = accent;
        }
        theme.border_radius = BASE_RADIUS * settings.corner_scale;
        // A custom palette is shown exactly as written
        if custom.is_none() {
            theme.ensure_readable();
        }
        theme
    }

    /// Nudges text and accent colors that are too faint on this palette's backgrounds toward the
    /// strongest end (white on dark palettes, black on light ones). Colors that already read well
    /// are left as the palette defines them.
    pub fn ensure_readable(&mut self) {
        let all = [self.base, self.mantle, self.crust, self.surface0, self.surface1, self.surface2];
        // Secondary text and accents sit on the panels and rows, not on the lightest hover fill
        let main = [self.base, self.mantle, self.surface0, self.surface1];
        let toward = if self.light { Color32::BLACK } else { Color32::WHITE };
        let fix = |color: Color32, min: f32, backgrounds: &[Color32]| {
            let mut out = color;
            for step in 1..=20 {
                if backgrounds.iter().all(|bg| contrast(out, *bg) >= min) {
                    break;
                }
                out = super::material::lerp_color(color, toward, step as f32 * 0.05);
            }
            out
        };
        self.text = fix(self.text, 5.5, &all);
        self.subtext0 = fix(self.subtext0, 4.5, &main);
        self.subtext1 = fix(self.subtext1, 3.5, &main);
        for accent in [&mut self.mauve, &mut self.blue, &mut self.peach, &mut self.green, &mut self.red, &mut self.yellow] {
            *accent = fix(*accent, 3.5, &main);
        }
    }

    /// Takes the colors Serpantinum wrote, keeping this palette's for any that are missing.
    fn take_colors(&mut self, raw: SerpantinumColorsRaw) {
        if let Some(c) = raw.base.and_then(|s| parse_hex(&s)) { self.base = c; }
        if let Some(c) = raw.mantle.and_then(|s| parse_hex(&s)) { self.mantle = c; }
        if let Some(c) = raw.crust.and_then(|s| parse_hex(&s)) { self.crust = c; }
        if let Some(c) = raw.text.and_then(|s| parse_hex(&s)) { self.text = c; }
        if let Some(c) = raw.subtext0.and_then(|s| parse_hex(&s)) { self.subtext0 = c; }
        if let Some(c) = raw.subtext1.and_then(|s| parse_hex(&s)) { self.subtext1 = c; }
        if let Some(c) = raw.surface0.and_then(|s| parse_hex(&s)) { self.surface0 = c; }
        if let Some(c) = raw.surface1.and_then(|s| parse_hex(&s)) { self.surface1 = c; }
        if let Some(c) = raw.surface2.and_then(|s| parse_hex(&s)) { self.surface2 = c; }
        if let Some(c) = raw.blue.and_then(|s| parse_hex(&s)) { self.blue = c; }
        if let Some(c) = raw.mauve.and_then(|s| parse_hex(&s)) { self.mauve = c; }
        if let Some(c) = raw.peach.and_then(|s| parse_hex(&s)) { self.peach = c; }
        if let Some(c) = raw.green.and_then(|s| parse_hex(&s)) { self.green = c; }
        if let Some(c) = raw.red.and_then(|s| parse_hex(&s)) { self.red = c; }
        if let Some(c) = raw.yellow.and_then(|s| parse_hex(&s)) { self.yellow = c; }
        // The file has no mode of its own: a light palette is one whose text is darker than its background
        self.light = luminance(self.base) > luminance(self.text);
    }

    /// The palette Serpantinum wrote, and the plain default where there is none. A file that is
    /// there but cannot be used is not silent: it is kept for `problems`.
    pub fn load() -> Self {
        let (theme, problems) = Self::load_from(&serpantinum_dirs());
        *SERPANTINUM_PROBLEMS.write().unwrap_or_else(|e| e.into_inner()) = problems;
        theme
    }

    fn load_from(dirs: &SerpantinumDirs) -> (Self, Vec<ThemeProblem>) {
        let mut theme = Self::default();
        let mut problems = Vec::new();

        // The colors: the first `qs_colors.json` that can be used, in the state folders and then the config folders
        for dir in dirs.state.iter().chain(&dirs.config) {
            if let Some(raw) = read_serpantinum_file::<SerpantinumColorsRaw>(&dir.join("qs_colors.json"), &mut problems) {
                theme.take_colors(raw);
                break;
            }
        }

        // The corner radius: from `settings.json` of the first config folder that has a usable one
        for dir in &dirs.config {
            if let Some(val) = read_serpantinum_file::<serde_json::Value>(&dir.join("settings.json"), &mut problems) {
                if let Some(r) = val.get("theme").and_then(|t| t.get("borderRadius")).and_then(|b| b.as_f64()) {
                    theme.border_radius = r as f32;
                }
                break;
            }
        }

        (theme, problems)
    }

    /// The accent softened toward the text color, for the selection, focus and hover tints: the
    /// same calm shade everywhere instead of the full accent at several strengths.
    pub fn tint(&self, alpha: u8) -> Color32 {
        self.with_alpha(super::material::lerp_color(self.mauve, self.text, 0.22), alpha)
    }

    /// Makes the backgrounds translucent: `opacity` is a percentage, 100 leaves them solid.
    pub fn fade(&mut self, opacity: u8) {
        let a = (opacity.clamp(1, 100) as f32 * 2.55).round() as u8;
        // Whatever shows through is unknown, so the dimmer texts move toward the main one to stay legible
        let see_through = (100 - opacity.clamp(1, 100)) as f32 / 100.0;
        self.subtext0 = super::material::lerp_color(self.subtext0, self.text, see_through);
        self.subtext1 = super::material::lerp_color(self.subtext1, self.text, see_through);
        for c in [&mut self.base, &mut self.mantle, &mut self.crust, &mut self.surface0, &mut self.surface1, &mut self.surface2] {
            *c = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a);
        }
    }

    /// Semi-transparent color
    pub fn with_alpha(&self, color: Color32, alpha: u8) -> Color32 {
        // r/g/b of a Color32 are premultiplied, which only matches the color itself while it is solid
        let [r, g, b, _] = color.to_srgba_unmultiplied();
        Color32::from_rgba_unmultiplied(r, g, b, alpha)
    }

    /// Smoothly blends between two palettes.
    pub fn lerp(&self, other: &Self, t: f32) -> Self {
        use super::material::{lerp, lerp_color};
        let t = t.clamp(0.0, 1.0);
        Self {
            base: lerp_color(self.base, other.base, t),
            mantle: lerp_color(self.mantle, other.mantle, t),
            crust: lerp_color(self.crust, other.crust, t),
            text: lerp_color(self.text, other.text, t),
            subtext0: lerp_color(self.subtext0, other.subtext0, t),
            subtext1: lerp_color(self.subtext1, other.subtext1, t),
            surface0: lerp_color(self.surface0, other.surface0, t),
            surface1: lerp_color(self.surface1, other.surface1, t),
            surface2: lerp_color(self.surface2, other.surface2, t),
            blue: lerp_color(self.blue, other.blue, t),
            mauve: lerp_color(self.mauve, other.mauve, t),
            peach: lerp_color(self.peach, other.peach, t),
            green: lerp_color(self.green, other.green, t),
            red: lerp_color(self.red, other.red, t),
            yellow: lerp_color(self.yellow, other.yellow, t),
            border_radius: lerp(self.border_radius, other.border_radius, t),
            light: if t < 0.5 { self.light } else { other.light },
        }
    }
}

/// The folders Serpantinum keeps its files in, the ones the desktop's settings name first and the
/// fixed `~/.local/state` and `~/.config` after them (where older setups put them).
#[derive(Debug, Default, PartialEq)]
pub struct SerpantinumDirs {
    pub state: Vec<PathBuf>,
    pub config: Vec<PathBuf>,
}

impl SerpantinumDirs {
    /// Every folder, for watching.
    pub fn all(&self) -> impl Iterator<Item = &PathBuf> {
        self.state.iter().chain(&self.config)
    }
}

fn serpantinum_dirs_from(state: Option<PathBuf>, config: Option<PathBuf>, home: Option<PathBuf>) -> SerpantinumDirs {
    let list = |named: Option<PathBuf>, fixed: Option<PathBuf>| {
        let mut out: Vec<PathBuf> = Vec::new();
        for dir in named.into_iter().chain(fixed).map(|d| d.join("serpantinum")) {
            if !out.contains(&dir) {
                out.push(dir);
            }
        }
        out
    };
    SerpantinumDirs {
        state: list(state, home.as_ref().map(|h| h.join(".local/state"))),
        config: list(config, home.map(|h| h.join(".config"))),
    }
}

thread_local! {
    /// Lets a test name Serpantinum's folders without touching the environment.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) static SERPANTINUM_DIRS_FOR_TESTS: std::cell::RefCell<Option<SerpantinumDirs>> = const { std::cell::RefCell::new(None) };
}

/// Where Serpantinum's files are, for loading them and for watching them.
pub fn serpantinum_dirs() -> SerpantinumDirs {
    if let Some(dirs) = SERPANTINUM_DIRS_FOR_TESTS.with(|d| d.borrow().as_ref().map(|d| SerpantinumDirs { state: d.state.clone(), config: d.config.clone() })) {
        return dirs;
    }
    match crate::fs::admin::config_override() {
        // As administrator the home folder is root's: what counts is the folder of whoever started Compression
        Some(config) => {
            let state = config.parent().map(|home| home.join(".local/state"));
            serpantinum_dirs_from(state, Some(config), None)
        }
        None => serpantinum_dirs_from(dirs::state_dir(), dirs::config_dir(), dirs::home_dir()),
    }
}

/// Reads and parses one of Serpantinum's files. A file that is not there is normal and says nothing;
/// one that is there and cannot be used is added to `problems`.
fn read_serpantinum_file<T: serde::de::DeserializeOwned>(path: &Path, problems: &mut Vec<ThemeProblem>) -> Option<T> {
    let name = format!("serpantinum/{}", path.file_name()?.to_string_lossy());
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => Some(value),
            Err(err) => {
                problems.push(ThemeProblem::broken(name, &err));
                None
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            problems.push(ThemeProblem::unreadable(name, &err));
            None
        }
    }
}

fn luminance(color: Color32) -> f32 {
    let channel = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

/// WCAG contrast ratio of two opaque colors, 1 (identical) to 21.
fn contrast(a: Color32, b: Color32) -> f32 {
    let (hi, lo) = (luminance(a).max(luminance(b)), luminance(a).min(luminance(b)));
    (hi + 0.05) / (lo + 0.05)
}

pub fn to_hex(color: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b())
}

pub fn parse_hex(hex: &str) -> Option<Color32> {
    let clean = hex.trim().trim_start_matches('#');
    if clean.len() == 6 {
        let r = u8::from_str_radix(&clean[0..2], 16).ok()?;
        let g = u8::from_str_radix(&clean[2..4], 16).ok()?;
        let b = u8::from_str_radix(&clean[4..6], 16).ok()?;
        Some(Color32::from_rgb(r, g, b))
    } else if clean.len() == 8 {
        let r = u8::from_str_radix(&clean[0..2], 16).ok()?;
        let g = u8::from_str_radix(&clean[2..4], 16).ok()?;
        let b = u8::from_str_radix(&clean[4..6], 16).ok()?;
        let a = u8::from_str_radix(&clean[6..8], 16).ok()?;
        Some(Color32::from_rgba_unmultiplied(r, g, b, a))
    } else {
        None
    }
}

/// Flat Material look: no strokes anywhere, state is shown by fills only.
/// `popup` is the solid palette that menus and tooltips are drawn in, whatever the window's opacity.
pub fn apply(ctx: &egui::Context, theme: &SerpantinumTheme, popup: &SerpantinumTheme) {
    super::material::set_radius_scale(theme.border_radius / BASE_RADIUS);
    let mut visuals = if theme.light { egui::Visuals::light() } else { egui::Visuals::dark() };
    visuals.window_corner_radius = egui::CornerRadius::same(super::material::radius_window());
    visuals.menu_corner_radius = egui::CornerRadius::same(super::material::radius_container());
    visuals.window_stroke = egui::Stroke::NONE;
    visuals.popup_shadow = egui::Shadow::NONE;
    visuals.window_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 32,
        spread: 0,
        // Only popups cast it, and they stay solid
        color: Color32::from_black_alpha(if popup.light { 50 } else { 110 }),
    };
    let w = &mut visuals.widgets;
    for state in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        state.bg_stroke = egui::Stroke::NONE;
        state.fg_stroke.color = theme.text;
    }
    w.noninteractive.bg_fill = theme.surface0;
    w.inactive.bg_fill = theme.with_alpha(theme.subtext1, 64);
    w.hovered.bg_fill = theme.with_alpha(theme.subtext0, 110);
    w.active.bg_fill = theme.with_alpha(theme.mauve, 150);
    w.open.bg_fill = theme.surface2;
    // Flat look: no outline, but the stroke color is also what selected text is drawn in
    visuals.selection.stroke = egui::Stroke::new(0.0, theme.text);
    visuals.selection.bg_fill = theme.tint(super::material::ALPHA_SEL_BG);
    visuals.panel_fill = theme.base;
    visuals.window_fill = popup.mantle;
    visuals.extreme_bg_color = theme.surface1;
    visuals.override_text_color = Some(theme.text);
    ctx.set_visuals(visuals);
    super::material::set_focus_color(ctx, theme.mauve);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 6.0);
        style.interaction.selectable_labels = false;
        for (ts, size) in [
            (egui::TextStyle::Body, super::material::FONT_BODY),
            (egui::TextStyle::Button, super::material::FONT_BODY),
            (egui::TextStyle::Small, super::material::FONT_CAPTION),
            (egui::TextStyle::Heading, super::material::FONT_TITLE),
        ] {
            if let Some(f) = style.text_styles.get_mut(&ts) {
                f.size = size;
            }
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn every_preset_is_readable() {
        for preset in presets() {
            let id = preset.id;
            let settings = crate::settings::Settings { theme: id.to_owned(), ..Default::default() };
            let t = SerpantinumTheme::from_settings(&settings);
            for bg in [t.base, t.mantle, t.surface0, t.surface1] {
                assert!(contrast(t.text, bg) >= 5.5, "{id} text");
                assert!(contrast(t.subtext1, bg) >= 3.5, "{id} subtext1");
                assert!(contrast(t.mauve, bg) >= 3.5, "{id} accent");
                assert!(contrast(t.yellow, bg) >= 3.5, "{id} yellow");
            }
            assert!(contrast(t.text, t.surface2) >= 5.5, "{id} text on surface2");
        }
    }

    #[test]
    fn a_light_serpantinum_palette_keeps_its_dark_text() {
        let raw: SerpantinumColorsRaw = serde_json::from_str(r##"{"base":"#f4eff4","mantle":"#ebe6eb","crust":"#e5e0e5","text":"#1c1b1f","subtext0":"#49454f","subtext1":"#79747e","surface0":"#e6e0e9","surface1":"#ddd8dd","surface2":"#d3ced3"}"##).unwrap();
        let mut t = SerpantinumTheme::default();
        t.take_colors(raw);
        assert!(t.light);
        let text = t.text;
        t.ensure_readable();
        assert!(contrast(t.text, t.base) >= contrast(text, t.base), "text was washed out toward white");
        assert!(contrast(t.subtext1, t.base) >= 3.5);
    }

    #[test]
    fn a_custom_theme_is_picked_by_its_id() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let file: CustomTheme = serde_json::from_str(r##"{ "name": "Mine", "base": "#102030" }"##).unwrap();
        USER_THEMES.write().unwrap().push(CustomTheme { id: "test-mine".to_owned(), ..file });
        let settings = crate::settings::Settings { theme: "custom:test-mine".to_owned(), ..Default::default() };
        let t = SerpantinumTheme::from_settings(&settings);
        assert_eq!(t.base, parse_hex("#102030").unwrap());
        // What the file leaves out keeps the Dark palette's
        assert_eq!(t.text, parse_hex(&ThemeColors::default().text).unwrap());
    }

    #[test]
    fn every_bundled_file_loads() {
        assert_eq!(presets().len(), BUNDLED.len());
        let mut ids: Vec<_> = presets().iter().map(|p| p.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), BUNDLED.len());
        for p in presets() {
            let mut colors = p.colors.clone();
            for (key, _, hex) in colors.fields() {
                assert!(parse_hex(hex).is_some(), "{} {key}", p.id);
            }
        }
    }

    /// A folder of the test's own that is given back, whatever the test did to its rights.
    pub(crate) struct Scratch(pub(crate) PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("compression_theme_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn put(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::process::Command::new("chmod").arg("-R").arg("u+rwx").arg(&self.0).status();
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn can_ignore_rights() -> bool {
        // As root a file with no rights can be read anyway, so nothing could be shown
        unsafe { libc::geteuid() == 0 }
    }

    const GOOD: &str = r##"{ "name": "Good", "base": "#102030" }"##;

    #[test]
    fn a_broken_theme_file_is_reported_and_the_others_still_load() {
        let dir = Scratch::new("broken");
        dir.put("good.json", GOOD);
        dir.put("typo.json", r##"{ "name": "Typo", "base": "#102030", "##);
        dir.put("wrong.json", r##"{ "name": 5 }"##);
        let (themes, problems) = read_themes_in(&dir.0);
        assert_eq!(themes.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["good"]);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(problems[0].file, "typo.json");
        assert!(matches!(problems[0].why, Why::NotJson(_)), "{:?}", problems[0].why);
        assert_eq!(problems[1].file, "wrong.json");
        assert!(matches!(problems[1].why, Why::WrongValue(1)), "{:?}", problems[1].why);
        assert!(problems[0].describe().starts_with("typo.json (not valid JSON (line "), "{}", problems[0].describe());
        assert_eq!(problems[1].describe(), "wrong.json (a value has the wrong type (line 1))");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_reported_with_the_systems_reason() {
        if can_ignore_rights() {
            return;
        }
        let dir = Scratch::new("unreadable");
        dir.put("good.json", GOOD);
        let secret = dir.put("secret.json", GOOD);
        std::fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let (themes, problems) = read_themes_in(&dir.0);
        assert_eq!(themes.len(), 1);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].why, Why::Unreadable(Some(libc::EACCES), "Permission denied (os error 13)".to_owned()));
        assert_eq!(problems[0].describe(), "secret.json (Permission denied)");
    }

    #[test]
    fn a_folder_that_is_not_there_and_a_file_that_is_gone_are_no_problem() {
        let dir = Scratch::new("absent");
        let (themes, problems) = read_themes_in(&dir.0.join("not-made-yet"));
        assert!((themes.is_empty(), problems.is_empty()) == (true, true));
        // A link to nothing: listed, but gone when it is read
        std::os::unix::fs::symlink(dir.0.join("nowhere.json"), dir.0.join("vanished.json")).unwrap();
        let (themes, problems) = read_themes_in(&dir.0);
        assert!(themes.is_empty() && problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_folder_that_cannot_be_listed_is_reported() {
        if can_ignore_rights() {
            return;
        }
        let dir = Scratch::new("unlistable");
        let themes = dir.0.join("themes");
        std::fs::create_dir(&themes).unwrap();
        std::fs::set_permissions(&themes, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let (found, problems) = read_themes_in(&themes);
        assert!(found.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].describe(), "themes (Permission denied)");
    }

    #[test]
    fn what_is_not_a_theme_file_is_ignored_without_a_word() {
        let dir = Scratch::new("ignored");
        dir.put("notes.txt", "not json");
        dir.put("half.json.tmp", "{ ");
        dir.put("README", "{ ");
        std::fs::create_dir(dir.0.join("folder.json")).unwrap();
        let (themes, problems) = read_themes_in(&dir.0);
        assert!(themes.is_empty() && problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_json_file_whose_name_cannot_be_an_id_is_reported() {
        use std::os::unix::ffi::OsStrExt;
        let dir = Scratch::new("name");
        let odd = dir.0.join(std::ffi::OsStr::from_bytes(b"\xff.json"));
        std::fs::write(odd, GOOD).unwrap();
        let (themes, problems) = read_themes_in(&dir.0);
        assert!(themes.is_empty());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].why, Why::Name);
        assert!(problems[0].describe().ends_with(".json (its name cannot be used)"), "{}", problems[0].describe());
    }

    #[test]
    fn a_repaired_file_is_a_theme_again_at_the_next_reading() {
        let dir = Scratch::new("repaired");
        let file = dir.put("mine.json", "{ oops");
        let (themes, problems) = read_themes_in(&dir.0);
        assert!(themes.is_empty() && problems.len() == 1);
        std::fs::write(&file, GOOD).unwrap();
        let (themes, problems) = read_themes_in(&dir.0);
        assert_eq!(themes.len(), 1);
        assert!(problems.is_empty());
    }

    /// Takes the themes folder of this test, and gives it back empty so that no other test sees these themes.
    pub(crate) struct Reading(pub(crate) Scratch, #[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Reading {
        pub(crate) fn new(name: &str) -> Self {
            let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = Scratch::new(name);
            THEMES_DIR_FOR_TESTS.with(|d| *d.borrow_mut() = Some(dir.0.clone()));
            Self(dir, lock)
        }
    }

    impl Drop for Reading {
        fn drop(&mut self) {
            let empty = std::env::temp_dir().join(format!("compression_theme_none_{}", std::process::id()));
            THEMES_DIR_FOR_TESTS.with(|d| *d.borrow_mut() = Some(empty));
            reload_user_themes();
            THEMES_DIR_FOR_TESTS.with(|d| *d.borrow_mut() = None);
        }
    }

    #[test]
    fn reloading_keeps_the_problems_for_the_page_and_drops_them_once_repaired() {
        let reading = Reading::new("reload");
        let file = reading.0.put("mine.json", "{ oops");
        assert!(reload_user_themes(), "a new problem is a change");
        assert_eq!(problems().iter().filter(|p| p.file == "mine.json").count(), 1);
        assert!(!reload_user_themes(), "the same reading is no change");
        std::fs::write(&file, GOOD).unwrap();
        assert!(reload_user_themes());
        assert!(problems().iter().all(|p| p.file != "mine.json"));
        assert!(user_themes().iter().any(|t| t.id == "mine"));
    }

    #[test]
    fn the_notice_names_three_files_and_counts_the_rest() {
        let problem = |n: usize| ThemeProblem { file: format!("t{n}.json"), why: Why::Name };
        assert_eq!(problems_notice(&[]), None);
        assert_eq!(problems_notice(&[problem(1)]).unwrap(), "Could not load these theme files: t1.json (its name cannot be used)");
        let five: Vec<_> = (1..=5).map(problem).collect();
        let notice = problems_notice(&five).unwrap();
        assert!(notice.contains("t3.json") && !notice.contains("t4.json"), "{notice}");
        assert!(notice.ends_with(", +2"), "{notice}");
        let four = problems_notice(&five[..4]).unwrap();
        assert!(four.ends_with(", +1"), "{four}");
        assert!(!problems_notice(&five[..3]).unwrap().contains('+'));
    }

    #[test]
    fn a_chosen_theme_that_is_gone_is_told_apart_from_one_that_is_there() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        USER_THEMES.write().unwrap().push(CustomTheme { id: "test-there".to_owned(), name: "There".to_owned(), colors: ThemeColors::default() });
        assert!(selected_is_missing("custom:test-gone"));
        assert!(!selected_is_missing("custom:test-there"));
        assert!(!selected_is_missing("system"));
        assert!(!selected_is_missing("nord"));
        assert!(selected_is_missing("no-such-preset"));
        // The choice itself is not touched: the settings still say custom:test-gone, and the colors are the system ones
        let settings = crate::settings::Settings { theme: "custom:test-gone".to_owned(), ..Default::default() };
        assert_eq!(settings.theme, "custom:test-gone");
    }

    #[test]
    fn the_folders_of_the_desktop_come_before_the_fixed_ones_and_are_not_listed_twice() {
        let p = |s: &str| Some(PathBuf::from(s));
        let dirs = serpantinum_dirs_from(p("/x/state"), p("/x/config"), p("/home/me"));
        let list = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(dirs.state, list(&["/x/state/serpantinum", "/home/me/.local/state/serpantinum"]));
        assert_eq!(dirs.config, list(&["/x/config/serpantinum", "/home/me/.config/serpantinum"]));
        // The usual setup: the desktop's folders are the fixed ones
        let same = serpantinum_dirs_from(p("/home/me/.local/state"), p("/home/me/.config"), p("/home/me"));
        assert_eq!((same.state.len(), same.config.len()), (1, 1));
        // No home folder, no desktop folders: nothing to look in
        assert_eq!(serpantinum_dirs_from(None, None, None), SerpantinumDirs::default());
        assert_eq!(serpantinum_dirs_from(None, p("/c"), None).config, list(&["/c/serpantinum"]));
    }

    const COLORS: &str = r##"{"base":"#010203"}"##;
    const OTHER_COLORS: &str = r##"{"base":"#0a0b0c"}"##;

    fn serpantinum_in(state: &Scratch, config: &Scratch, home: &Scratch) -> SerpantinumDirs {
        for dir in [state, config, home] {
            std::fs::create_dir_all(dir.0.join("serpantinum")).unwrap();
        }
        let fixed_state = home.0.join(".local/state/serpantinum");
        let fixed_config = home.0.join(".config/serpantinum");
        std::fs::create_dir_all(&fixed_state).unwrap();
        std::fs::create_dir_all(&fixed_config).unwrap();
        serpantinum_dirs_from(Some(state.0.clone()), Some(config.0.clone()), Some(home.0.clone()))
    }

    #[test]
    fn serpantinums_files_are_found_in_the_desktops_folders() {
        let (state, config, home) = (Scratch::new("sx_state"), Scratch::new("sx_config"), Scratch::new("sx_home"));
        let dirs = serpantinum_in(&state, &config, &home);
        std::fs::write(state.0.join("serpantinum/qs_colors.json"), COLORS).unwrap();
        std::fs::write(config.0.join("serpantinum/settings.json"), r#"{"theme":{"borderRadius":9}}"#).unwrap();
        let (theme, problems) = SerpantinumTheme::load_from(&dirs);
        assert_eq!(theme.base, parse_hex("#010203").unwrap());
        assert_eq!(theme.border_radius, 9.0);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn the_fixed_folders_still_work_when_the_desktops_have_nothing() {
        let (state, config, home) = (Scratch::new("sf_state"), Scratch::new("sf_config"), Scratch::new("sf_home"));
        let dirs = serpantinum_in(&state, &config, &home);
        std::fs::write(home.0.join(".config/serpantinum/qs_colors.json"), COLORS).unwrap();
        std::fs::write(home.0.join(".config/serpantinum/settings.json"), r#"{"theme":{"borderRadius":7}}"#).unwrap();
        let (theme, problems) = SerpantinumTheme::load_from(&dirs);
        assert_eq!(theme.base, parse_hex("#010203").unwrap());
        assert_eq!(theme.border_radius, 7.0);
        assert!(problems.is_empty());
    }

    #[test]
    fn the_desktops_folder_wins_over_the_fixed_one() {
        let (state, config, home) = (Scratch::new("sw_state"), Scratch::new("sw_config"), Scratch::new("sw_home"));
        let dirs = serpantinum_in(&state, &config, &home);
        std::fs::write(state.0.join("serpantinum/qs_colors.json"), COLORS).unwrap();
        std::fs::write(home.0.join(".local/state/serpantinum/qs_colors.json"), OTHER_COLORS).unwrap();
        std::fs::write(config.0.join("serpantinum/settings.json"), r#"{"theme":{"borderRadius":9}}"#).unwrap();
        std::fs::write(home.0.join(".config/serpantinum/settings.json"), r#"{"theme":{"borderRadius":3}}"#).unwrap();
        let (theme, _) = SerpantinumTheme::load_from(&dirs);
        assert_eq!(theme.base, parse_hex("#010203").unwrap());
        assert_eq!(theme.border_radius, 9.0);
    }

    #[test]
    fn a_broken_colors_file_is_reported_and_the_next_one_is_still_used() {
        let (state, config, home) = (Scratch::new("sb_state"), Scratch::new("sb_config"), Scratch::new("sb_home"));
        let dirs = serpantinum_in(&state, &config, &home);
        std::fs::write(state.0.join("serpantinum/qs_colors.json"), "{ oops").unwrap();
        std::fs::write(home.0.join(".local/state/serpantinum/qs_colors.json"), OTHER_COLORS).unwrap();
        let (theme, problems) = SerpantinumTheme::load_from(&dirs);
        assert_eq!(theme.base, parse_hex("#0a0b0c").unwrap());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].file, "serpantinum/qs_colors.json");
        assert!(matches!(problems[0].why, Why::NotJson(1)));
        // Alone, the broken file leaves the default palette and still says so
        std::fs::remove_file(home.0.join(".local/state/serpantinum/qs_colors.json")).unwrap();
        let (theme, problems) = SerpantinumTheme::load_from(&dirs);
        assert_eq!(theme.base, SerpantinumTheme::default().base);
        assert_eq!(problems.len(), 1);
        // And a broken settings.json is reported too
        std::fs::write(config.0.join("serpantinum/settings.json"), "[").unwrap();
        let (_, problems) = SerpantinumTheme::load_from(&dirs);
        assert!(problems.iter().any(|p| p.file == "serpantinum/settings.json"), "{problems:?}");
    }

    #[test]
    fn missing_serpantinum_files_say_nothing() {
        let (state, config, home) = (Scratch::new("sm_state"), Scratch::new("sm_config"), Scratch::new("sm_home"));
        let dirs = serpantinum_in(&state, &config, &home);
        let (theme, problems) = SerpantinumTheme::load_from(&dirs);
        assert_eq!(theme.base, SerpantinumTheme::default().base);
        assert!(problems.is_empty(), "{problems:?}");
        // No folders at all
        assert!(SerpantinumTheme::load_from(&SerpantinumDirs::default()).1.is_empty());
    }

    #[test]
    fn saving_and_deleting_report_what_went_wrong() {
        let reading = Reading::new("write");
        let theme = CustomTheme { id: "mine".to_owned(), name: "Mine".to_owned(), colors: ThemeColors::default() };
        save_user_theme(&theme).unwrap();
        let file = reading.0 .0.join("mine.json");
        assert!(std::fs::read_to_string(&file).unwrap().contains("\"name\": \"Mine\""));
        // Nothing half-written is left beside it
        assert_eq!(std::fs::read_dir(&reading.0 .0).unwrap().count(), 1);
        delete_user_theme("mine").unwrap();
        assert!(!file.exists());
        delete_user_theme("mine").unwrap();

        // A folder that cannot be made: something that is not a folder is in the way
        let blocker = reading.0.put("blocker", "a file");
        THEMES_DIR_FOR_TESTS.with(|d| *d.borrow_mut() = Some(blocker.join("themes")));
        assert!(save_user_theme(&theme).is_err());
        assert!(!blocker.join("themes").exists());
    }

    #[test]
    fn a_theme_file_that_cannot_be_removed_is_an_error_and_stays() {
        if can_ignore_rights() {
            return;
        }
        let reading = Reading::new("remove");
        let file = reading.0.put("mine.json", GOOD);
        std::fs::set_permissions(&reading.0 .0, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let err = delete_user_theme("mine").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EACCES));
        assert!(file.exists());
        // A write that cannot happen leaves the old file as it was
        let theme = CustomTheme { id: "mine".to_owned(), name: "Changed".to_owned(), colors: ThemeColors::default() };
        assert!(save_user_theme(&theme).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), GOOD);
    }

    #[test]
    fn load_keeps_what_was_wrong_for_the_page_and_forgets_it_once_repaired() {
        let reading = Reading::new("load_wiring");
        let dir = reading.0.0.join("serpantinum");
        std::fs::create_dir_all(&dir).unwrap();
        SERPANTINUM_DIRS_FOR_TESTS.with(|d| *d.borrow_mut() = Some(SerpantinumDirs { state: vec![dir.clone()], config: Vec::new() }));
        std::fs::write(dir.join("qs_colors.json"), "{ oops").unwrap();
        SerpantinumTheme::load();
        assert!(problems().iter().any(|p| p.file == "serpantinum/qs_colors.json"), "{:?}", problems());
        std::fs::write(dir.join("qs_colors.json"), COLORS).unwrap();
        assert_eq!(SerpantinumTheme::load().base, parse_hex("#010203").unwrap());
        assert!(problems().iter().all(|p| p.file != "serpantinum/qs_colors.json"));
        SERPANTINUM_DIRS_FOR_TESTS.with(|d| *d.borrow_mut() = None);
    }
}
