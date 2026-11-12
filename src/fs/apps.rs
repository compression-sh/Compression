//! Installed applications (freedesktop `.desktop` entries), MIME lookup and "open with".
use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::fs::process;

#[derive(Debug, Clone)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    exec: String,
    mimes: Vec<String>,
    /// Raw `Icon=` value: an absolute path or a theme icon name.
    pub icon: Option<String>,
}

/// MIME type of a file, as the system's shared-mime-info database sees it.
pub fn mime_of(path: &Path) -> String {
    let queried = Command::new("xdg-mime")
        .args(["query", "filetype"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|m| !m.is_empty());
    queried.unwrap_or_else(|| "application/octet-stream".to_owned())
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

pub fn default_for(mime: &str) -> Option<String> {
    Command::new("xdg-mime")
        .args(["query", "default", mime])
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

static CATEGORY_DEFAULTS: RwLock<Option<Vec<Option<String>>>> = RwLock::new(None);

/// Query the current default of every category in the background (`xdg-mime` is a slow script, and
/// one run per category on the UI thread made the settings tab stall).
pub fn preload_category_defaults() {
    if let Ok(mut slot) = CATEGORY_DEFAULTS.write() {
        *slot = None;
    }
    std::thread::spawn(|| {
        let found = std::thread::scope(|s| {
            let jobs: Vec<_> = CATEGORIES.iter().map(|c| s.spawn(|| default_for(c.mimes[0]))).collect();
            jobs.into_iter().map(|j| j.join().ok().flatten()).collect::<Vec<_>>()
        });
        if let Ok(mut slot) = CATEGORY_DEFAULTS.write() {
            *slot = Some(found);
        }
    });
}

/// The defaults found by `preload_category_defaults`, once they are in.
pub fn category_defaults() -> Option<Vec<Option<String>>> {
    CATEGORY_DEFAULTS.read().ok().and_then(|d| d.clone())
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
    let file = file.to_string_lossy();
    let mut used_file = false;
    let mut args: Vec<String> = Vec::new();
    for arg in split_exec(&app.exec) {
        let mut out = String::new();
        let mut chars = arg.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('f' | 'F' | 'u' | 'U') => {
                    out.push_str(&file);
                    used_file = true;
                }
                Some('%') => out.push('%'),
                _ => {}
            }
        }
        if !out.is_empty() {
            args.push(out);
        }
    }
    if !used_file {
        args.push(file.into_owned());
    }
    let Some((program, rest)) = args.split_first() else {
        return Err(std::io::Error::other("empty Exec line"));
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
    overrides_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_overrides(map: &HashMap<PathBuf, String>) {
    // A test must never write the person's own file
    let Some(file) = overrides_file().filter(|_| !cfg!(test)) else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = crate::fs::atomic::write(&file, json);
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
fn run_tool(program: &[String], args: &[&str], timeout: Duration) -> Result<process::Output, Why> {
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
fn default_with(program: &[String], mime: &str, timeout: Duration) -> Result<Option<String>, Why> {
    let out = run_tool(program, &["query", "default", mime], timeout)?;
    if !out.success {
        return Err(Why::Refused);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty()))
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
    let globs = mime_globs();
    let lower = name.to_lowercase();
    let mut best: Option<(u32, usize, &str)> = None;
    for (weight, mime, glob) in &globs.other {
        if super::glob::matches(glob, &lower, true) && best.is_none_or(|(w, l, _)| (*weight, glob.len()) > (w, l)) {
            best = Some((*weight, glob.len(), mime));
        }
    }
    // `a.tar.gz` is tried as `tar.gz` before `gz`
    for (i, _) in lower.match_indices('.') {
        if let Some(found) = globs.by_extension.get(&lower[i + 1..]) {
            let (weight, mime) = found.iter().max_by_key(|(w, _)| *w)?;
            let len = lower.len() - i;
            if best.is_none_or(|(w, l, _)| (*weight, len) > (w, l)) {
                best = Some((*weight, len, mime));
            }
            break;
        }
    }
    best.map(|(_, _, mime)| mime.to_owned())
}

/// Whether a file named `name` is of the type `pattern` (`image/png`, or `image/*` for a family),
/// counting the types it derives from: a `.docx` is an `application/zip`.
pub fn name_is_mime(name: &str, pattern: &str) -> bool {
    // Without a database nothing can be told apart, and hiding every file is worse than showing all
    if mime_globs().by_extension.is_empty() {
        return true;
    }
    let mime = mime_of_name(name).unwrap_or_else(|| "application/octet-stream".to_owned());
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
