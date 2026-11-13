//! Installed applications (freedesktop `.desktop` entries), MIME lookup and "open with".
use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::fs::process;
use crate::i18n::tr;

#[derive(Debug, Clone)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    exec: String,
    mimes: Vec<String>,
    /// Raw `Icon=` value: an absolute path or a theme icon name.
    pub icon: Option<String>,
}

fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(data) = dirs::data_dir() {
        dirs.push(data.join("applications"));
    }
    let system = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
    dirs.extend(system.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("applications")));
    dirs
}

static APPS: RwLock<Option<Arc<Vec<AppEntry>>>> = RwLock::new(None);

/// The installed applications as last scanned. Blocks the calling thread when there is no scan yet.
pub fn installed() -> Arc<Vec<AppEntry>> {
    if let Some(apps) = APPS.read().ok().and_then(|a| a.clone()) {
        return apps;
    }
    rescan()
}

fn rescan() -> Arc<Vec<AppEntry>> {
    let apps = Arc::new(load_apps());
    if let Ok(mut slot) = APPS.write() {
        *slot = Some(apps.clone());
    }
    apps
}

/// Scan the `.desktop` files in the background so the first use does not stall the UI. Each window
/// rescans, as a resident Compression would otherwise miss the applications installed since it started.
pub fn preload() {
    std::thread::spawn(rescan);
}

/// Every graphical application in the XDG data dirs. Earlier dirs win.
pub fn load_apps() -> Vec<AppEntry> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in application_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(app) = parse_desktop(&id, &path) {
                apps.push(app);
            }
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn parse_desktop(id: &str, path: &Path) -> Option<AppEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut fields: HashMap<&str, &str> = HashMap::new();
    let mut in_entry = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry
            && let Some((k, v)) = line.split_once('=') {
                fields.entry(k.trim()).or_insert(v.trim());
            }
    }
    let is_true = |key: &str| fields.get(key) == Some(&"true");
    if fields.get("Type") != Some(&"Application") || is_true("NoDisplay") || is_true("Hidden") || is_true("Terminal") {
        return None;
    }
    let mimes: Vec<String> = fields
        .get("MimeType")
        .map(|m| m.split(';').filter(|m| !m.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default();
    Some(AppEntry {
        id: id.to_owned(),
        name: fields.get("Name")?.to_string(),
        exec: fields.get("Exec")?.to_string(),
        icon: fields.get("Icon").map(|i| i.to_string()).filter(|i| !i.is_empty()),
        mimes,
    })
}

/// `child parent` / `alias canonical` pairs from the shared-mime-info database.
fn mime_pairs(file: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for dir in application_dirs() {
        let Some(share) = dir.parent() else { continue };
        let Ok(content) = std::fs::read_to_string(share.join("mime").join(file)) else { continue };
        for line in content.lines() {
            if let Some((a, b)) = line.split_once(' ') {
                pairs.push((a.trim().to_owned(), b.trim().to_owned()));
            }
        }
    }
    pairs
}

/// `mime` followed by everything it is a kind of (aliases and parent types, most specific first).
/// A .docx is also an `application/zip`, which is why file managers offer archive tools for it.
pub fn mime_chain(mime: &str) -> Vec<String> {
    static ALIASES: OnceLock<HashMap<String, String>> = OnceLock::new();
    static PARENTS: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
    let aliases = ALIASES.get_or_init(|| mime_pairs("aliases").into_iter().collect());
    let parents = PARENTS.get_or_init(|| {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (child, parent) in mime_pairs("subclasses") {
            map.entry(child).or_default().push(parent);
        }
        map
    });

    let start = aliases.get(mime).map(String::as_str).unwrap_or(mime);
    let mut chain = vec![start.to_owned()];
    let mut next = 0;
    while next < chain.len() {
        for parent in parents.get(&chain[next]).into_iter().flatten() {
            if !chain.contains(parent) {
                chain.push(parent.clone());
            }
        }
        next += 1;
    }
    if mime.starts_with("text/") && !chain.iter().any(|m| m == "text/plain") {
        chain.push("text/plain".to_owned());
    }
    chain
}

impl AppEntry {
    /// How specifically this app handles `chain` (0 = the exact type); `None` if it does not.
    fn rank(&self, chain: &[String]) -> Option<usize> {
        chain.iter().position(|c| self.mimes.iter().any(|m| m == c))
    }
}

/// Applications able to open `mime` (or any type it derives from): the system default first,
/// then the ones that handle the type most specifically.
pub fn apps_for<'a>(apps: &'a [AppEntry], mime: &str, default_id: Option<&str>) -> Vec<&'a AppEntry> {
    let chain = mime_chain(mime);
    let mut list: Vec<(usize, &AppEntry)> = apps.iter().filter_map(|a| a.rank(&chain).map(|r| (r, a))).collect();
    list.sort_by_key(|(rank, a)| (Some(a.id.as_str()) != default_id, *rank));
    list.into_iter().map(|(_, a)| a).collect()
}

/// Whether something is set up to open `mime`: a default app, or any installed app that lists the type
/// (or one it derives from). Without either, `xdg-open` would fail without a word.
pub fn has_handler(apps: &[AppEntry], mime: &str, default_id: Option<&str>) -> bool {
    default_id.is_some() || !apps_for(apps, mime, None).is_empty()
}

/// Locate the image file of a desktop entry's `Icon=` value.
pub fn resolve_icon(icon: &str) -> Option<PathBuf> {
    let direct = Path::new(icon);
    if direct.is_absolute() {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    const SIZES: [&str; 6] = ["64x64", "48x48", "128x128", "96x96", "256x256", "32x32"];
    static THEMES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    let themes = THEMES.get_or_init(|| {
        let mut themes = Vec::new();
        for dir in application_dirs() {
            let Some(share) = dir.parent() else { continue };
            let icons = share.join("icons");
            themes.push(icons.join("hicolor"));
            if let Ok(rd) = std::fs::read_dir(&icons) {
                themes.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.ends_with("hicolor")));
            }
        }
        themes
    });
    for theme in themes {
        for size in SIZES {
            for sub in ["apps", "mimetypes"] {
                let png = theme.join(size).join(sub).join(format!("{icon}.png"));
                if png.is_file() {
                    return Some(png);
                }
            }
        }
        for sub in ["scalable/apps", "apps/scalable", "scalable/mimetypes"] {
            let svg = theme.join(sub).join(format!("{icon}.svg"));
            if svg.is_file() {
                return Some(svg);
            }
        }
    }
    for dir in application_dirs() {
        let Some(share) = dir.parent() else { continue };
        for ext in ["png", "svg"] {
            let pix = share.join("pixmaps").join(format!("{icon}.{ext}"));
            if pix.is_file() {
                return Some(pix);
            }
        }
    }
    None
}

/// Decode a PNG / SVG icon file into a square-ish bitmap about `px` pixels on its long side.
pub fn load_icon_image(path: &Path, px: u32) -> Option<eframe::egui::ColorImage> {
    use eframe::egui::ColorImage;
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
        let data = std::fs::read(path).ok()?;
        let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
        let size = tree.size();
        let scale = px as f32 / size.width().max(size.height());
        let mut pixmap = resvg::tiny_skia::Pixmap::new((size.width() * scale).ceil().max(1.0) as u32, (size.height() * scale).ceil().max(1.0) as u32)?;
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        Some(ColorImage::from_rgba_premultiplied([pixmap.width() as usize, pixmap.height() as usize], pixmap.data()))
    } else {
        let img = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?;
        let img = img.thumbnail(px, px).to_rgba8();
        Some(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
    }
}

/// What was found out about the default application of one category (or one type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Known {
    /// Still being asked.
    Pending,
    /// The system answered: the application's id, or `None` when it names none.
    Is(Option<String>),
    /// It could not be asked, and why.
    Failed(Why),
}

type Slots = Arc<Mutex<Vec<Known>>>;

static CATEGORY_DEFAULTS: Mutex<Option<Slots>> = Mutex::new(None);

/// Ask for the current default of every category, each on a thread of its own and each within `call`: an answer that
/// does not come holds up nobody else. The slots fill in as the answers come.
fn start_preload(program: Vec<String>, call: Duration) -> Slots {
    let slots: Slots = Arc::new(Mutex::new(vec![Known::Pending; CATEGORIES.len()]));
    for (idx, category) in CATEGORIES.iter().enumerate() {
        let (slots, program) = (slots.clone(), program.clone());
        std::thread::spawn(move || {
            let found = match default_with(&program, category.mimes[0], call) {
                Ok(id) => Known::Is(id),
                Err(why) => Known::Failed(why),
            };
            if let Ok(mut all) = slots.lock() {
                all[idx] = found;
            }
        });
    }
    slots
}

/// Query the current default of every category in the background (`xdg-mime` is a slow script, and
/// one run per category on the UI thread made the settings tab stall).
pub fn preload_category_defaults(program: Vec<String>) {
    let slots = start_preload(program, CALL_LIMIT);
    if let Ok(mut current) = CATEGORY_DEFAULTS.lock() {
        *current = Some(slots);
    }
}

/// The defaults found by `preload_category_defaults` so far, in the order of `CATEGORIES`; `Pending` for the ones
/// that are not in yet (all of them before a preload was started).
pub fn category_defaults() -> Vec<Known> {
    let slots = CATEGORY_DEFAULTS.lock().ok().and_then(|c| c.clone());
    slots.and_then(|s| s.lock().ok().map(|a| a.clone())).unwrap_or_else(|| vec![Known::Pending; CATEGORIES.len()])
}

fn split_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(cur);
    }
    args
}

pub fn launch(app: &AppEntry, file: &Path) -> std::io::Result<()> {
    // The path goes in as it is, byte for byte: a name that is not UTF-8 reaches the program unchanged
    let mut used_file = false;
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    for arg in split_exec(&app.exec) {
        let mut out = std::ffi::OsString::new();
        let mut chars = arg.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c.encode_utf8(&mut [0; 4]));
                continue;
            }
            match chars.next() {
                Some('f' | 'F' | 'u' | 'U') => {
                    out.push(file.as_os_str());
                    used_file = true;
                }
                Some('%') => out.push("%"),
                _ => {}
            }
        }
        if !out.is_empty() {
            args.push(out);
        }
    }
    if !used_file {
        args.push(file.as_os_str().to_owned());
    }
    let Some((program, rest)) = args.split_first() else {
        return Err(std::io::Error::other(crate::i18n::tr("The launcher has no command")));
    };
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Per-file "always open with" choices: file path -> desktop entry id.
fn overrides_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("compression").join("open_with.json"))
}

pub fn load_overrides() -> HashMap<PathBuf, String> {
    overrides_file().map(|f| crate::fs::stored::load_json(&f)).unwrap_or_default()
}

pub fn save_overrides(map: &HashMap<PathBuf, String>) {
    // A test must never write the person's own file
    let Some(file) = overrides_file().filter(|_| !cfg!(test)) else { return };
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = crate::fs::stored::save(&file, json);
    }
}

/// Kinds of files the user picks one default application for.
pub struct Category {
    pub label: &'static str,
    pub icon: &'static str,
    pub mimes: &'static [&'static str],
}

pub const CATEGORIES: [Category; 6] = [
    Category {
        label: "Images",
        icon: crate::ui::icons::IMAGE,
        mimes: &["image/png", "image/jpeg", "image/gif", "image/webp", "image/bmp", "image/svg+xml", "image/tiff"],
    },
    Category {
        label: "Video",
        icon: crate::ui::icons::VIDEO,
        mimes: &["video/mp4", "video/x-matroska", "video/webm", "video/quicktime", "video/x-msvideo", "video/mpeg"],
    },
    Category {
        label: "Music",
        icon: crate::ui::icons::AUDIO,
        mimes: &["audio/mpeg", "audio/flac", "audio/ogg", "audio/x-wav", "audio/mp4", "audio/x-opus+ogg"],
    },
    Category {
        label: "Documents",
        icon: crate::ui::icons::DOCUMENT,
        mimes: &["application/pdf", "application/epub+zip"],
    },
    Category {
        label: "Text",
        icon: crate::ui::icons::CODE,
        mimes: &["text/plain", "text/markdown", "text/x-python", "text/x-csrc", "application/json", "application/x-shellscript"],
    },
    Category {
        label: "Archives",
        icon: crate::ui::icons::ARCHIVE,
        mimes: &["application/zip", "application/x-tar", "application/gzip", "application/x-7z-compressed", "application/vnd.rar", "application/x-xz-compressed-tar"],
    },
];

/// How long one `xdg-mime` call may take, and how long all the calls for one category may take together.
pub const CALL_LIMIT: Duration = Duration::from_secs(10);
pub const CATEGORY_LIMIT: Duration = Duration::from_secs(20);

/// Why a type did not become the application's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// `xdg-mime` is not installed.
    Missing,
    /// It did not answer within the time.
    TimedOut,
    /// It ended with an error, or printed too much.
    Refused,
    /// It said yes, but asking afterwards shows another default (or none).
    NotKept,
}

/// What the default was set for: one category of the settings (by its place in `CATEGORIES`), or one type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultFor {
    Category(usize),
    Type(String),
}

/// What setting a default came to. `now` is what `xdg-mime` says for each type afterwards (in the order asked),
/// `failed` the types that did not become `app_id`'s, with the reason.
#[derive(Debug, Clone)]
pub struct DefaultsReport {
    pub target: DefaultFor,
    pub app_id: String,
    pub app_name: String,
    pub failed: Vec<(String, Why)>,
    pub now: Vec<Option<String>>,
}

/// `program` (a program and its leading arguments, `xdg-mime` outside tests) with `args`, within `timeout`.
fn run_tool<S: AsRef<std::ffi::OsStr>>(program: &[String], args: &[S], timeout: Duration) -> Result<process::Output, Why> {
    let Some((first, leading)) = program.split_first() else { return Err(Why::Missing) };
    let mut command = Command::new(first);
    command.args(leading).args(args);
    match process::run_capped(command, 64 * 1024, timeout) {
        Ok(output) => Ok(output),
        Err(process::Failure::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Err(Why::Missing),
        Err(process::Failure::TimedOut) => Err(Why::TimedOut),
        Err(_) => Err(Why::Refused),
    }
}

/// Make `app_id` the default of every type in `mimes`, and ask afterwards what the system says: only an answer that
/// is exactly `app_id` counts, an exit status of 0 alone does not. One call may take `call`, all of them `total`;
/// what is not done by then is a failure, never a success.
pub(crate) fn set_defaults_with(program: &[String], mimes: &[String], app_id: &str, call: Duration, total: Duration) -> (Vec<(String, Why)>, Vec<Option<String>>) {
    let start = Instant::now();
    let (mut failed, mut now) = (Vec::new(), Vec::new());
    for (i, mime) in mimes.iter().enumerate() {
        let left = total.saturating_sub(start.elapsed());
        if left.is_zero() {
            failed.push((mime.clone(), Why::TimedOut));
            now.push(None);
            continue;
        }
        let set = match run_tool(program, &["default", app_id, mime], call.min(left)) {
            Ok(out) if out.success => None,
            Ok(_) => Some(Why::Refused),
            Err(why) => Some(why),
        };
        if set == Some(Why::Missing) {
            // Nothing will work: the rest is not tried one by one
            for rest in &mimes[i..] {
                failed.push((rest.clone(), Why::Missing));
                now.push(None);
            }
            break;
        }
        let left = total.saturating_sub(start.elapsed());
        let readback = if left.is_zero() { Err(Why::TimedOut) } else { default_with(program, mime, call.min(left)) };
        match (set, &readback) {
            (Some(why), _) => failed.push((mime.clone(), why)),
            (None, Ok(Some(found))) if found == app_id => {}
            (None, Ok(_)) => failed.push((mime.clone(), Why::NotKept)),
            (None, Err(why)) => failed.push((mime.clone(), *why)),
        }
        now.push(readback.ok().flatten());
    }
    (failed, now)
}

/// The default of `mime` as `xdg-mime query default` says: `Ok(None)` when it names none, `Err` when it could not be asked.
pub fn default_with(program: &[String], mime: &str, timeout: Duration) -> Result<Option<String>, Why> {
    let out = run_tool(program, &["query", "default", mime], timeout)?;
    if !out.success {
        return Err(Why::Refused);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty()))
}

impl Why {
    /// What to tell the person, as the end of a sentence in parentheses.
    pub fn text(self) -> &'static str {
        match self {
            Why::Missing => tr("xdg-mime is not installed"),
            Why::TimedOut => tr("xdg-mime did not answer in time"),
            Why::Refused => tr("xdg-mime reported an error"),
            Why::NotKept => tr("the system did not keep the change"),
        }
    }
}

/// MIME type of a file. The name decides when the system's pattern table knows it (no process, no reading of the file,
/// which matters on servers); only otherwise is `xdg-mime query filetype` asked, within `timeout`. A failure is an
/// error, never a made-up type.
pub fn mime_with(program: &[String], path: &Path, timeout: Duration) -> Result<String, Why> {
    if let Some(mime) = path.file_name().and_then(|n| mime_of_name(&n.to_string_lossy())) {
        return Ok(mime);
    }
    let out = run_tool(program, &[std::ffi::OsStr::new("query"), std::ffi::OsStr::new("filetype"), path.as_os_str()], timeout)?;
    let mime = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.success || mime.is_empty() {
        return Err(Why::Refused);
    }
    Ok(mime)
}

/// The type of a file and the application the system opens it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileType {
    pub mime: String,
    pub default: Option<String>,
}

/// `mime_with`, then `default_with` for that type; each call may take `call`.
pub fn file_type_with(program: &[String], path: &Path, call: Duration) -> Result<FileType, Why> {
    let mime = mime_with(program, path, call)?;
    let default = default_with(program, &mime, call)?;
    Ok(FileType { mime, default })
}

/// Set the default of `mimes` on a worker and send what came of it to `tx`; the window never waits for `xdg-mime`.
pub fn set_defaults_in_background(program: Vec<String>, target: DefaultFor, mimes: Vec<String>, app: (String, String), tx: std::sync::mpsc::Sender<DefaultsReport>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let (app_id, app_name) = app;
        let (failed, now) = set_defaults_with(&program, &mimes, &app_id, CALL_LIMIT, CATEGORY_LIMIT);
        let _ = tx.send(DefaultsReport { target, app_id, app_name, failed, now });
        ctx.request_repaint();
    });
}

/// Name patterns of the shared-mime-info database (`globs2`): `*.ext` ones by extension, the rest as globs.
struct MimeGlobs {
    by_extension: HashMap<String, Vec<(u32, String)>>,
    other: Vec<(u32, String, String)>,
}

fn mime_globs() -> &'static MimeGlobs {
    static GLOBS: OnceLock<MimeGlobs> = OnceLock::new();
    GLOBS.get_or_init(|| {
        let mut globs = MimeGlobs { by_extension: HashMap::new(), other: Vec::new() };
        let mut seen = std::collections::HashSet::new();
        for dir in application_dirs() {
            let Some(share) = dir.parent() else { continue };
            let Ok(content) = std::fs::read_to_string(share.join("mime").join("globs2")) else { continue };
            for line in content.lines().filter(|l| !l.starts_with('#')) {
                let mut parts = line.splitn(4, ':');
                let (Some(weight), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next()) else { continue };
                let Ok(weight) = weight.parse::<u32>() else { continue };
                if !seen.insert((mime.to_owned(), glob.to_owned())) {
                    continue;
                }
                match glob.strip_prefix("*.") {
                    Some(ext) if !ext.contains(['*', '?', '[']) => globs.by_extension.entry(ext.to_lowercase()).or_default().push((weight, mime.to_owned())),
                    _ => globs.other.push((weight, mime.to_owned(), glob.to_lowercase())),
                }
            }
        }
        globs
    })
}

/// MIME type of a file from its name alone, the way the shared-mime-info database assigns it
/// (longest matching extension first, then the heaviest pattern); `None` when no pattern matches.
pub fn mime_of_name(name: &str) -> Option<String> {
    mimes_of_name(name).into_iter().next()
}

/// The MIME types the name of a file can stand for, the best first. More than one when patterns of the same weight and
/// length match (`.ts` is a video and a Qt translation): then only the content can tell (see `mime_of_file`).
fn mimes_of_name(name: &str) -> Vec<String> {
    let globs = mime_globs();
    let lower = name.to_lowercase();
    let mut best: Option<(u32, usize)> = None;
    let mut mimes: Vec<&str> = Vec::new();
    let mut offer = |weight: u32, len: usize, mime: &'static str| match best.map(|b| (weight, len).cmp(&b)) {
        None | Some(std::cmp::Ordering::Greater) => {
            best = Some((weight, len));
            mimes = vec![mime];
        }
        Some(std::cmp::Ordering::Equal) if !mimes.contains(&mime) => mimes.push(mime),
        _ => {}
    };
    for (weight, mime, glob) in &globs.other {
        // A pattern of the database is one pattern on the whole name: no commas, and plain text is no substring
        if super::glob::wildcard(glob, &lower) {
            offer(*weight, glob.len(), mime);
        }
    }
    // `a.tar.gz` is tried as `tar.gz` before `gz`
    for (i, _) in lower.match_indices('.') {
        if let Some(found) = globs.by_extension.get(&lower[i + 1..]) {
            let len = lower.len() - i;
            for (weight, mime) in found {
                offer(*weight, len, mime);
            }
            break;
        }
    }
    mimes.into_iter().map(str::to_owned).collect()
}

/// How much of a file is read to tell its type by its content.
const MAGIC_BYTES: u64 = 64 * 1024;

/// The MIME type of the file at `path`: from its name when that is clear, and from its first bytes (the magic of the
/// shared-mime-info database) when the name has no type or more than one, as GTK does. Only regular files on this
/// computer are read; a server is not asked for the bytes of every unnamed file.
pub fn mime_of_file(path: &Path) -> Option<String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut named = mimes_of_name(&name);
    if named.len() == 1 || super::network::is_remote(path) {
        return named.into_iter().next();
    }
    let Some(bytes) = first_bytes(path) else { return named.into_iter().next() };
    if named.is_empty() {
        return Some(tree_magic_mini::from_u8(&bytes).to_owned());
    }
    // Of the types the name allows, the one the content has; the first when none fits
    let fitting = named.iter().position(|mime| tree_magic_mini::match_u8(mime, &bytes)).unwrap_or(0);
    Some(named.swap_remove(fitting))
}

/// The start of a regular file, without waiting on a pipe or following a link (see `ops::open_regular_file`).
fn first_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    #[cfg(test)]
    super::mount_health::tests::hang_if_asked_to(path);
    let file = super::ops::open_regular_file(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAGIC_BYTES).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Whether the file at `path` is of the type `pattern` (`image/png`, or `image/*` for a family), counting the types it
/// derives from (a `.docx` is an `application/zip`); its content tells its type where the name does not (`mime_of_file`).
pub fn file_is_mime(path: &Path, pattern: &str) -> bool {
    mime_is(mime_of_file(path), pattern)
}

fn mime_is(mime: Option<String>, pattern: &str) -> bool {
    // Without a database nothing can be told apart, and hiding every file is worse than showing all
    if mime_globs().by_extension.is_empty() {
        return true;
    }
    let mime = mime.unwrap_or_else(|| "application/octet-stream".to_owned());
    let chain = mime_chain(&mime);
    match pattern.strip_suffix("/*") {
        Some(family) => chain.iter().any(|m| m.split_once('/').is_some_and(|(f, _)| f == family)),
        None => {
            let wanted = mime_chain(pattern).into_iter().next().unwrap_or_else(|| pattern.to_owned());
            chain.contains(&wanted)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn open_with_hands_the_program_the_path_byte_for_byte() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let dir = std::env::temp_dir().join(format!("compression_open_with_bytes_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(std::ffi::OsString::from_vec(b"Bericht-\xe4 x.txt".to_vec()));
        std::fs::write(&file, "x").unwrap();
        let got = dir.join("got");
        // A launcher whose program writes down the argument it was given
        let exec = format!("sh -c \"printf %%s \\\"$1\\\" > {}\" opener %f", got.display());
        let app = AppEntry { id: "t.desktop".into(), name: "T".into(), exec, mimes: Vec::new(), icon: None };
        launch(&app, &file).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::metadata(&got).map_or(true, |m| m.len() == 0) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read(&got).unwrap(), file.as_os_str().as_bytes(), "the program got another path");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An entry for tests of other modules.
    pub(crate) fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry { id: id.into(), name: name.into(), exec: "true".into(), mimes: Vec::new(), icon: None }
    }

    /// A stand-in for `xdg-mime` that keeps its defaults in `dir/state` and logs each call in `dir/calls`. It sleeps `delay`
    /// (seconds) first; `on_default` is the shell that runs for `default <app> <mime>` (`$2` app, `$3` type). Run through
    /// `/bin/sh`, so that no freshly written program is executed (that races with other tests starting programs).
    pub(crate) fn fake_tool(dir: &Path, delay: &str, on_default: &str) -> Vec<String> {
        let script = dir.join("xdg-mime");
        let body = format!(
            "D='{}'\necho \"$1\" >> \"$D/calls\"\nsleep {delay}\ncase \"$1\" in\ndefault) {on_default}; exit 0;;\nquery) grep \"^$3=\" \"$D/state\" 2>/dev/null | tail -1 | cut -d= -f2-; exit 0;;\nesac\n",
            dir.display()
        );
        std::fs::write(&script, body).unwrap();
        vec!["/bin/sh".to_owned(), script.display().to_string()]
    }

    pub(crate) const KEEPS: &str = "echo \"$3=$2\" >> \"$D/state\"";

    fn app(id: &str, mimes: &[&str]) -> AppEntry {
        AppEntry { id: id.into(), name: id.into(), exec: "true".into(), mimes: mimes.iter().map(|m| m.to_string()).collect(), icon: None }
    }

    #[test]
    fn a_type_needs_a_default_or_an_app_that_lists_it() {
        let apps = [app("viewer.desktop", &["image/png"])];
        // Nothing is set up for it
        assert!(!has_handler(&apps, "application/x-unknown-thing", None));
        assert!(!has_handler(&[], "image/png", None));
        // An app lists the type, or the system names a default
        assert!(has_handler(&apps, "image/png", None));
        assert!(has_handler(&apps, "application/x-unknown-thing", Some("other.desktop")));
        assert!(has_handler(&[], "image/png", Some("viewer.desktop")));
    }

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_apps_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|m| (*m).to_owned()).collect()
    }

    const SHORT: Duration = Duration::from_millis(400);

    #[test]
    fn a_default_that_the_system_confirms_for_every_type_is_set() {
        let dir = sandbox("all");
        let tool = fake_tool(&dir, "0", KEEPS);
        let (failed, now) = set_defaults_with(&tool, &mimes(&["image/png", "image/gif"]), "v.desktop", CALL_LIMIT, CATEGORY_LIMIT);
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(now, vec![Some("v.desktop".to_owned()), Some("v.desktop".to_owned())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_type_that_does_not_come_out_as_asked_is_a_failure_and_the_others_still_count() {
        let dir = sandbox("part");
        // `image/gif` is refused with an error; `image/bmp` is answered with another application afterwards
        let on_default = "[ \"$3\" = image/gif ] && exit 1; if [ \"$3\" = image/bmp ]; then echo \"$3=other.desktop\" >> \"$D/state\"; else echo \"$3=$2\" >> \"$D/state\"; fi";
        let tool = fake_tool(&dir, "0", on_default);
        let (failed, now) = set_defaults_with(&tool, &mimes(&["image/png", "image/gif", "image/bmp"]), "v.desktop", CALL_LIMIT, CATEGORY_LIMIT);
        assert_eq!(failed, vec![("image/gif".to_owned(), Why::Refused), ("image/bmp".to_owned(), Why::NotKept)]);
        assert_eq!(now, vec![Some("v.desktop".to_owned()), None, Some("other.desktop".to_owned())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_exit_status_of_zero_without_the_change_is_no_success() {
        let dir = sandbox("lie");
        let tool = fake_tool(&dir, "0", ":");
        let (failed, now) = set_defaults_with(&tool, &mimes(&["image/png"]), "v.desktop", CALL_LIMIT, CATEGORY_LIMIT);
        assert_eq!(failed, vec![("image/png".to_owned(), Why::NotKept)]);
        assert_eq!(now, vec![None]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_that_is_not_there_fails_every_type_at_once() {
        let started = Instant::now();
        let (failed, now) = set_defaults_with(&["/nonexistent/xdg-mime".to_owned()], &mimes(&["image/png", "image/gif", "image/bmp"]), "v.desktop", CALL_LIMIT, CATEGORY_LIMIT);
        assert_eq!(failed.len(), 3);
        assert!(failed.iter().all(|(_, why)| *why == Why::Missing), "{failed:?}");
        assert_eq!(now, vec![None, None, None]);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_program_that_hangs_is_stopped_after_the_time_of_one_call() {
        let dir = sandbox("hang");
        let tool = fake_tool(&dir, "30", KEEPS);
        let started = Instant::now();
        let (failed, _) = set_defaults_with(&tool, &mimes(&["image/png"]), "v.desktop", SHORT, Duration::from_secs(10));
        assert_eq!(failed, vec![("image/png".to_owned(), Why::TimedOut)]);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stand-in for `xdg-mime query …` that runs `body` (a shell; `$1 $2` are the words of the question, `$3` its
    /// argument) and logs each question in `dir/asked`.
    pub(crate) fn query_tool(dir: &Path, body: &str) -> Vec<String> {
        let script = dir.join("xdg-mime");
        std::fs::write(&script, format!("echo \"$1 $2 $3\" >> '{}/asked'\n{body}\n", dir.display())).unwrap();
        vec!["/bin/sh".to_owned(), script.display().to_string()]
    }

    pub(crate) const FILETYPE: &str = "case \"$1 $2\" in \"query filetype\") echo application/x-compression-test;; \"query default\") echo viewer.desktop;; esac";
    pub(crate) const UNKNOWN: &str = "thing.zzqq";

    #[test]
    fn a_name_the_pattern_table_knows_needs_no_process() {
        if mime_of_name("a.png").is_none() {
            return;
        }
        // The program does not exist, and nothing is read: the name decides
        let found = mime_with(&["/nonexistent/xdg-mime".to_owned()], Path::new("/nonexistent/dir/a.png"), CALL_LIMIT);
        assert_eq!(found, Ok("image/png".to_owned()));
    }

    #[test]
    fn a_name_the_table_does_not_know_is_asked_of_xdg_mime() {
        let dir = sandbox("ask");
        let tool = query_tool(&dir, FILETYPE);
        assert_eq!(mime_with(&tool, &dir.join(UNKNOWN), CALL_LIMIT), Ok("application/x-compression-test".to_owned()));
        let asked = std::fs::read_to_string(dir.join("asked")).unwrap();
        assert!(asked.starts_with("query filetype "), "{asked}");
        let found = file_type_with(&tool, &dir.join(UNKNOWN), CALL_LIMIT).unwrap();
        assert_eq!(found, FileType { mime: "application/x-compression-test".to_owned(), default: Some("viewer.desktop".to_owned()) });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pattern_of_the_database_is_matched_on_the_whole_name_and_a_name_it_does_not_know_gets_no_type() {
        if mime_of_name("a.png").is_none() {
            return;
        }
        // `*,v` is one pattern (RCS files), not the two patterns `*` and `v`
        assert_eq!(mime_of_name("thing.zzqq"), None);
        assert_eq!(mime_of_name("notes without an extension"), None);
        assert_eq!(mime_of_name("file,v"), Some("text/plain".to_owned()));
        // A plain name is the whole name, not a part of it
        assert_eq!(mime_of_name("Makefile"), Some("text/x-makefile".to_owned()));
        assert_eq!(mime_of_name("my makefile notes.zzqq"), None);
    }

    #[test]
    fn a_hanging_xdg_mime_ends_in_timed_out_and_is_killed() {
        let dir = sandbox("hang_lookup");
        let tool = query_tool(&dir, "sleep 30");
        let started = Instant::now();
        assert_eq!(mime_with(&tool, &dir.join(UNKNOWN), SHORT), Err(Why::TimedOut));
        assert_eq!(default_with(&tool, "image/png", SHORT), Err(Why::TimedOut));
        assert_eq!(file_type_with(&tool, &dir.join(UNKNOWN), SHORT), Err(Why::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_failing_xdg_mime_is_an_error_and_never_a_made_up_type() {
        let dir = sandbox("fail_lookup");
        let gone = vec!["/nonexistent/xdg-mime".to_owned()];
        assert_eq!(mime_with(&gone, &dir.join(UNKNOWN), CALL_LIMIT), Err(Why::Missing));
        assert_eq!(default_with(&gone, "image/png", CALL_LIMIT), Err(Why::Missing));
        let fails = query_tool(&dir, "exit 1");
        assert_eq!(mime_with(&fails, &dir.join(UNKNOWN), CALL_LIMIT), Err(Why::Refused));
        assert_eq!(default_with(&fails, "image/png", CALL_LIMIT), Err(Why::Refused));
        // Success without an answer says nothing about the type
        let silent = query_tool(&dir, "exit 0");
        assert_eq!(mime_with(&silent, &dir.join(UNKNOWN), CALL_LIMIT), Err(Why::Refused));
        // A type with no default is an answer, not an error
        assert_eq!(default_with(&silent, "image/png", CALL_LIMIT), Ok(None));
        // The default is only asked when the type is known
        let half = query_tool(&dir, "case \"$2\" in filetype) echo a/b;; *) exit 1;; esac");
        assert_eq!(file_type_with(&half, &dir.join(UNKNOWN), CALL_LIMIT), Err(Why::Refused));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_category_that_does_not_answer_holds_up_no_other_one() {
        let dir = sandbox("preload");
        let first = CATEGORIES[0].mimes[0];
        let tool = query_tool(&dir, &format!("case \"$3\" in {first}) sleep 30;; *) echo viewer.desktop;; esac"));
        let slots = start_preload(tool, Duration::from_millis(1500));
        let snapshot = || slots.lock().unwrap().clone();
        let started = Instant::now();
        while snapshot().iter().skip(1).any(|k| *k == Known::Pending) {
            assert!(started.elapsed() < Duration::from_secs(1), "the others waited: {:?}", snapshot());
            std::thread::sleep(Duration::from_millis(20));
        }
        // The others are in; the first is still being asked
        let now = snapshot();
        assert_eq!(now[0], Known::Pending);
        assert!(now.iter().skip(1).all(|k| *k == Known::Is(Some("viewer.desktop".to_owned()))), "{now:?}");
        while snapshot()[0] == Known::Pending {
            assert!(started.elapsed() < Duration::from_secs(6), "no end of the wait");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(snapshot()[0], Known::Failed(Why::TimedOut));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_is_not_done_when_the_time_of_the_whole_category_is_up_is_a_failure() {
        let dir = sandbox("total");
        // Each call takes 0.4 s; the category gets 1 s in all: the first type is done, the later ones are not
        let tool = fake_tool(&dir, "0.4", KEEPS);
        let started = Instant::now();
        let (failed, now) = set_defaults_with(&tool, &mimes(&["a/1", "a/2", "a/3", "a/4", "a/5"]), "v.desktop", Duration::from_secs(5), Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        assert!(failed.len() >= 3, "{failed:?}");
        assert!(failed.iter().all(|(_, why)| *why == Why::TimedOut), "{failed:?}");
        assert_eq!(now.len(), 5);
        assert!(failed.iter().all(|(m, _)| m != "a/1"), "the first type was done in time: {failed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

}
