//! The "System" icon style: folders and file types drawn with the icon theme the desktop has installed, found the way the
//! freedesktop icon theme specification says (the theme, then the themes it inherits, then hicolor; the size nearest to
//! the one drawn; SVG or PNG). What the theme does not have is drawn in the classic style. Themes are read from the
//! local disk only, once, and every icon is rasterized once per size and palette.
//!
//! Icons that are meant to take the colors of the desktop (Breeze and the themes made like it mark their shapes with
//! the classes `ColorScheme-Text`, `ColorScheme-Accent` and so on and give them light-theme colors only as a default)
//! get the colors of the palette in use, as KDE gives them its color scheme; `currentColor` with no color of its own is
//! the text color. Icons drawn in colors of their own keep them.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use eframe::egui;

/// One folder of a theme with the sizes of the icons in it.
#[derive(Debug, Clone)]
struct SubDir {
    path: String,
    min: u32,
    max: u32,
}

#[derive(Debug, Clone, Default)]
struct Theme {
    /// The folders of this theme, one per base folder it is found in.
    roots: Vec<PathBuf>,
    dirs: Vec<SubDir>,
    inherits: Vec<String>,
}

/// Where icon themes are installed: the person's own, then the system's.
fn base_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    out.extend(dirs::data_dir().map(|d| d.join("icons")));
    out.extend(dirs::home_dir().map(|h| h.join(".icons")));
    let shared = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    out.extend(shared.split(':').filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("icons")));
    out.push(PathBuf::from("/usr/share/pixmaps"));
    out
}

static DESKTOP_THEME: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// The icon theme the desktop names through the portal (GNOME's setting, see `ui::appearance`); `None` where it names none.
pub fn set_desktop_icon_theme(name: Option<String>) {
    *DESKTOP_THEME.write().unwrap_or_else(|e| e.into_inner()) = name;
}

/// The name of the icon theme the desktop uses, as GTK finds it: what the portal says (GNOME's dconf setting), else
/// GTK's own `settings.ini`, else KDE's, else Adwaita.
/// It is asked for every icon drawn, so the settings files are read once (as GTK reads them at its start); the portal's
/// name, which the window keeps up to date, is taken each time.
pub fn theme_name() -> String {
    static FROM_FILES: OnceLock<String> = OnceLock::new();
    let from_files = FROM_FILES.get_or_init(|| {
        let config = dirs::config_dir();
        let read = |file: &str| {
            #[cfg(test)]
            tests::SETTINGS_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            config.as_ref().and_then(|c| std::fs::read_to_string(c.join(file)).ok())
        };
        theme_name_from(None, &[read("gtk-4.0/settings.ini"), read("gtk-3.0/settings.ini")], read("kdeglobals"))
    });
    match DESKTOP_THEME.read().unwrap_or_else(|e| e.into_inner()).as_deref() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => from_files.clone(),
    }
}

/// `theme_name` from what each source says: the portal's name, the texts of GTK's settings files and of `kdeglobals`.
fn theme_name_from(portal: Option<String>, gtk: &[Option<String>], kde: Option<String>) -> String {
    portal
        .filter(|n| !n.is_empty())
        .or_else(|| gtk.iter().flatten().find_map(|t| ini_value(t, "Settings", "gtk-icon-theme-name")))
        .or_else(|| kde.and_then(|t| ini_value(&t, "Icons", "Theme")))
        .unwrap_or_else(|| "Adwaita".to_owned())
}

/// The value of `key` in the `[section]` of an ini text, unquoted.
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == format!("[{section}]");
        } else if inside && let Some((k, v)) = line.split_once('=') && k.trim() == key {
            let v = v.trim().trim_matches('"');
            return (!v.is_empty()).then(|| v.to_owned());
        }
    }
    None
}

fn read_theme(name: &str, bases: &[PathBuf]) -> Option<Theme> {
    let roots: Vec<PathBuf> = bases.iter().map(|b| b.join(name)).filter(|r| r.is_dir()).collect();
    let index = roots.iter().find_map(|r| std::fs::read_to_string(r.join("index.theme")).ok())?;
    let inherits = ini_value(&index, "Icon Theme", "Inherits").map(|v| v.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let directories = ini_value(&index, "Icon Theme", "Directories").unwrap_or_default();
    let dirs = directories
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .filter_map(|d| {
            let size: u32 = ini_value(&index, d, "Size")?.parse().ok()?;
            let kind = ini_value(&index, d, "Type").unwrap_or_else(|| "Threshold".to_owned());
            let threshold: u32 = ini_value(&index, d, "Threshold").and_then(|t| t.parse().ok()).unwrap_or(2);
            let (min, max) = match kind.as_str() {
                "Fixed" => (size, size),
                "Scalable" => (
                    ini_value(&index, d, "MinSize").and_then(|v| v.parse().ok()).unwrap_or(size),
                    ini_value(&index, d, "MaxSize").and_then(|v| v.parse().ok()).unwrap_or(size),
                ),
                _ => (size.saturating_sub(threshold), size + threshold),
            };
            Some(SubDir { path: d.to_owned(), min, max })
        })
        .collect();
    Some(Theme { roots, dirs, inherits })
}

/// The themes to look in, in order: `name`, what it inherits (depth first, each once), and hicolor last.
fn chain(name: &str, bases: &[PathBuf]) -> Vec<Theme> {
    fn walk(name: &str, bases: &[PathBuf], seen: &mut Vec<String>, out: &mut Vec<Theme>) {
        if seen.iter().any(|s| s == name) {
            return;
        }
        seen.push(name.to_owned());
        if let Some(theme) = read_theme(name, bases) {
            let parents = theme.inherits.clone();
            out.push(theme);
            for parent in parents {
                walk(&parent, bases, seen, out);
            }
        }
    }
    let (mut seen, mut out) = (Vec::new(), Vec::new());
    walk(name, bases, &mut seen, &mut out);
    walk("hicolor", bases, &mut seen, &mut out);
    out
}

/// How far an icon folder is from `size`: 0 when it holds that size.
fn distance(dir: &SubDir, size: u32) -> u32 {
    if size < dir.min {
        dir.min - size
    } else {
        size.saturating_sub(dir.max)
    }
}

/// The file of the first of `names` that the themes have, the theme first and each name in turn (the first name is
/// the most particular), in the folder nearest to `size` (of folders as near, the one the theme lists first).
fn find_in(themes: &[Theme], names: &[String], size: u32) -> Option<PathBuf> {
    for theme in themes {
        for name in names {
            let mut best: Option<(u32, bool, PathBuf)> = None;
            for dir in &theme.dirs {
                for root in &theme.roots {
                    for (ext, svg) in [("svg", true), ("png", false)] {
                        let file = root.join(&dir.path).join(format!("{name}.{ext}"));
                        if !file.is_file() {
                            continue;
                        }
                        let d = distance(dir, size);
                        // As near: the folder listed first wins, as the specification says (SVG first in one folder)
                        if best.as_ref().is_none_or(|(bd, _, _)| d < *bd) {
                            best = Some((d, svg, file));
                        }
                    }
                }
            }
            if let Some((_, _, file)) = best {
                return Some(file);
            }
        }
    }
    None
}

/// The themes of the desktop, read once for each theme name (the desktop may switch to another while Compression runs).
fn installed() -> (String, std::sync::Arc<Vec<Theme>>) {
    static THEMES: Mutex<Option<(String, std::sync::Arc<Vec<Theme>>)>> = Mutex::new(None);
    let name = theme_name();
    let mut themes = THEMES.lock().unwrap_or_else(|e| e.into_inner());
    match themes.as_ref() {
        Some((read, chain)) if *read == name => (name, chain.clone()),
        _ => {
            let chain = std::sync::Arc::new(chain(&name, &base_dirs()));
            *themes = Some((name.clone(), chain.clone()));
            (name, chain)
        }
    }
}

/// `find_in` the desktop's themes, remembered per theme, names and size.
fn find(names: &[String], size: u32) -> Option<PathBuf> {
    type Found = HashMap<(String, Vec<String>, u32), Option<PathBuf>>;
    static FOUND: OnceLock<Mutex<Found>> = OnceLock::new();
    let found = FOUND.get_or_init(Mutex::default);
    let (theme, themes) = installed();
    let key = (theme, names.to_vec(), size);
    if let Some(hit) = found.lock().ok()?.get(&key) {
        return hit.clone();
    }
    let hit = find_in(&themes, names, size);
    if let Ok(mut map) = found.lock() {
        map.insert(key, hit.clone());
    }
    hit
}

/// The icon names for a file of type `mime`, the most particular first: `image-png`, the database's generic icon for
/// it, `image-x-generic`, and the icon of a file of unknown type.
pub fn names_for_mime(mime: &str) -> Vec<String> {
    let mut names = vec![mime.replace('/', "-")];
    if let Some(generic) = generic_icons().get(mime) {
        names.push(generic.clone());
    }
    if let Some((media, _)) = mime.split_once('/') {
        names.push(format!("{media}-x-generic"));
    }
    names.push("application-x-generic".to_owned());
    names.push("unknown".to_owned());
    names.dedup();
    names
}

/// The `generic-icon`s of the shared-mime-info database.
fn generic_icons() -> &'static HashMap<String, String> {
    static ICONS: OnceLock<HashMap<String, String>> = OnceLock::new();
    ICONS.get_or_init(|| {
        let mut map = HashMap::new();
        let shared = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
        for dir in dirs::data_dir().into_iter().chain(shared.split(':').map(PathBuf::from)) {
            if let Ok(text) = std::fs::read_to_string(dir.join("mime/generic-icons")) {
                for (mime, icon) in text.lines().filter_map(|l| l.split_once(':')) {
                    map.entry(mime.to_owned()).or_insert_with(|| icon.to_owned());
                }
            }
        }
        map
    })
}

/// The icon names for an item of the listing: a folder, or a file by its type.
pub fn names_for(is_dir: bool, name: &str) -> Vec<String> {
    if is_dir {
        return vec!["folder".to_owned()];
    }
    names_for_mime(&crate::fs::apps::mime_of_name(name).unwrap_or_else(|| "application/octet-stream".to_owned()))
}

/// Sizes the icons are rasterized at: the first one that covers the drawn size.
const TIERS: [u32; 5] = [24, 48, 96, 192, 384];

/// The colors an icon is drawn in (see `IconColors::sheet`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IconColors {
    text: String,
    classes: String,
}

impl IconColors {
    /// The CSS for an icon: the classes of KDE's color scheme (what KDE's own icon loader fills in), `!important`
    /// because the icon's own sheet holds light-theme defaults for them; and, unless the icon's root names a `color`
    /// of its own (CSS would win over that attribute), the text color for `currentColor`.
    fn sheet(&self, own_color: bool) -> String {
        if own_color { self.classes.clone() } else { format!("svg {{ color: {}; }}\n{}", self.text, self.classes) }
    }
}

/// Whether the root of the SVG `bytes` gives `currentColor` a color of its own.
fn names_own_color(bytes: &[u8]) -> bool {
    let Ok(doc) = std::str::from_utf8(bytes).map_err(|_| ()).and_then(|t| resvg::usvg::roxmltree::Document::parse(t).map_err(|_| ())) else { return false };
    let root = doc.root_element();
    root.has_attribute("color") || root.attribute("style").is_some_and(|s| s.split(';').any(|d| d.split(':').next().is_some_and(|n| n.trim() == "color")))
}

/// The colors of `theme` for the icons.
pub fn icon_colors(theme: &super::theme::SerpantinumTheme) -> IconColors {
    let hex = |c: egui::Color32| format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
    let (text, base, accent) = (hex(theme.text), hex(theme.base), hex(theme.mauve));
    let classes = [
        ("Text", &text),
        ("ViewText", &text),
        ("ButtonText", &text),
        ("Contrast", &text),
        ("Background", &base),
        ("ViewBackground", &base),
        ("ButtonBackground", &base),
        ("Complement", &base),
        // Text drawn on the accent
        ("HighlightedText", &base),
        ("Highlight", &accent),
        ("Accent", &accent),
        ("ActiveText", &accent),
        ("ViewFocus", &accent),
        ("ViewHover", &accent),
        ("ButtonFocus", &accent),
        ("ButtonHover", &accent),
        ("PositiveText", &hex(theme.green)),
        ("NeutralText", &hex(theme.yellow)),
        ("NegativeText", &hex(theme.red)),
    ];
    let classes = classes.iter().map(|(class, color)| format!(".ColorScheme-{class} {{ color: {color} !important; }}\n")).collect();
    IconColors { text, classes }
}

fn sheet_id() -> egui::Id {
    egui::Id::NULL.with("system_icon_colors")
}

/// The palette the icons are drawn in from now on (`ui::theme::apply` sets it with every palette it applies).
pub fn set_colors(ctx: &egui::Context, theme: &super::theme::SerpantinumTheme) {
    let colors = std::sync::Arc::new(icon_colors(theme));
    ctx.data_mut(|d| d.insert_temp(sheet_id(), colors));
}

fn colors(ctx: &egui::Context) -> std::sync::Arc<IconColors> {
    ctx.data(|d| d.get_temp::<std::sync::Arc<IconColors>>(sheet_id())).unwrap_or_else(|| std::sync::Arc::new(icon_colors(&Default::default())))
}

/// The textures made for one palette; another palette starts afresh, and the old textures are freed with it.
#[derive(Default)]
struct Textures {
    colors: std::sync::Arc<IconColors>,
    made: HashMap<(PathBuf, u32), Option<egui::TextureHandle>>,
}

/// The icon of `path` as a texture of `px` pixels in the palette in use, made once.
fn texture(ctx: &egui::Context, path: &Path, px: u32) -> Option<egui::TextureHandle> {
    let colors = colors(ctx);
    let cache = ctx.data_mut(|d| d.get_temp_mut_or_default::<std::sync::Arc<Mutex<Textures>>>(egui::Id::NULL.with("system_icon_textures")).clone());
    let key = (path.to_path_buf(), px);
    {
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.colors != colors {
            *cache = Textures { colors: colors.clone(), made: HashMap::new() };
        }
        if let Some(tex) = cache.made.get(&key) {
            return tex.clone();
        }
    }
    let tex = rasterize(path, px, &colors).map(|image| ctx.load_texture(format!("system-icon-{}-{px}", path.display()), image, egui::TextureOptions::LINEAR));
    // A file that cannot be drawn is not tried again in every frame
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.colors == colors {
        cache.made.insert(key, tex.clone());
    }
    tex
}

/// An SVG or PNG icon as a square picture of `px` pixels, its proportions kept; an SVG drawn in `colors`.
fn rasterize(path: &Path, px: u32, colors: &IconColors) -> Option<egui::ColorImage> {
    let bytes = std::fs::read(path).ok()?;
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
        let options = resvg::usvg::Options { style_sheet: Some(colors.sheet(names_own_color(&bytes))), ..Default::default() };
        let tree = resvg::usvg::Tree::from_data(&bytes, &options).ok()?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(px, px)?;
        let (w, h) = (tree.size().width(), tree.size().height());
        let scale = px as f32 / w.max(h);
        let shift = ((px as f32 - w * scale) / 2.0, (px as f32 - h * scale) / 2.0);
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(shift.0, shift.1), &mut pixmap.as_mut());
        // egui wants premultiplied alpha, which is what tiny-skia produces
        return Some(egui::ColorImage::from_rgba_premultiplied([px as usize; 2], pixmap.data()));
    }
    let image = image::load_from_memory(&bytes).ok()?.resize(px, px, image::imageops::FilterType::Triangle).to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([image.width() as usize, image.height() as usize], image.as_raw()))
}

/// Draws the first of `names` the desktop's themes have, `size` points wide at `center`; false when none has any.
pub fn paint(painter: &egui::Painter, names: &[String], center: egui::Pos2, size: f32, fade: f32) -> bool {
    let px = size * painter.ctx().pixels_per_point();
    let tier = TIERS.into_iter().find(|t| *t as f32 >= px).unwrap_or(TIERS[TIERS.len() - 1]);
    let Some(tex) = find(names, tier).and_then(|file| texture(painter.ctx(), &file, tier)) else { return false };
    let [w, h] = tex.size();
    let scale = size / w.max(h) as f32;
    let shown = egui::Rect::from_center_size(center, egui::vec2(w as f32 * scale, h as f32 * scale));
    painter.image(tex.id(), shown, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), egui::Color32::WHITE.gamma_multiply(fade));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#3070ff"/></svg>"##;

    /// Two themes in `base`: "Mine" (48 px PNGs and scalable SVGs), which inherits "Parent" (scalable), and hicolor.
    fn themes(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("compression_icons_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let put = |rel: &str, text: &[u8]| {
            let file = base.join(rel);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        };
        put("Mine/index.theme", b"[Icon Theme]\nName=Mine\nInherits=Parent\nDirectories=48x48/mimetypes,scalable/mimetypes\n\n[48x48/mimetypes]\nSize=48\nType=Fixed\n\n[scalable/mimetypes]\nSize=64\nMinSize=8\nMaxSize=512\nType=Scalable\n");
        put("Parent/index.theme", b"[Icon Theme]\nName=Parent\nDirectories=scalable/places\n\n[scalable/places]\nSize=64\nMinSize=8\nMaxSize=512\nType=Scalable\n");
        put("hicolor/index.theme", b"[Icon Theme]\nName=hicolor\nDirectories=16x16/mimetypes\n\n[16x16/mimetypes]\nSize=16\nType=Fixed\n");
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(48, 48, image::Rgba([200, 30, 30, 255])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        put("Mine/48x48/mimetypes/text-x-python.png", &png);
        put("Mine/scalable/mimetypes/text-x-python.svg", SVG.as_bytes());
        put("Mine/scalable/mimetypes/text-x-generic.svg", SVG.as_bytes());
        put("Parent/scalable/places/folder.svg", SVG.as_bytes());
        put("hicolor/16x16/mimetypes/application-x-generic.png", &png);
        base
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_icon_is_found_in_the_theme_what_it_inherits_and_hicolor_at_the_nearest_size() {
        let base = themes("find");
        let chain = chain("Mine", std::slice::from_ref(&base));
        assert_eq!(chain.len(), 3, "Mine, Parent and hicolor");
        let python = names(&["text-x-python", "text-x-generic"]);
        // 48 px: the PNG made for that size; far from it, the scalable SVG
        assert_eq!(find_in(&chain, &python, 48).unwrap(), base.join("Mine/48x48/mimetypes/text-x-python.png"));
        assert_eq!(find_in(&chain, &python, 192).unwrap(), base.join("Mine/scalable/mimetypes/text-x-python.svg"));
        // What the theme lacks comes from the theme it inherits, and from hicolor last
        assert_eq!(find_in(&chain, &names(&["folder"]), 48).unwrap(), base.join("Parent/scalable/places/folder.svg"));
        assert_eq!(find_in(&chain, &names(&["x-nothing", "application-x-generic"]), 48).unwrap(), base.join("hicolor/16x16/mimetypes/application-x-generic.png"));
        // The theme's own generic icon before an inherited particular one: theme first, then name
        assert_eq!(find_in(&chain, &names(&["image-png", "text-x-generic"]), 48).unwrap(), base.join("Mine/scalable/mimetypes/text-x-generic.svg"));
        assert!(find_in(&chain, &names(&["x-nothing"]), 48).is_none());
        // A theme that inherits itself in a circle is read once
        assert_eq!(chain.iter().filter(|t| t.roots.iter().any(|r| r.ends_with("Mine"))).count(), 1);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn svg_and_png_icons_become_square_pictures_of_the_size_asked_for() {
        let base = themes("raster");
        for file in ["Mine/scalable/mimetypes/text-x-python.svg", "Mine/48x48/mimetypes/text-x-python.png"] {
            let image = rasterize(&base.join(file), 96, &IconColors::default()).unwrap_or_else(|| panic!("{file}"));
            assert_eq!(image.size, [96, 96], "{file}");
            assert!(image.pixels.iter().any(|p| p.a() > 0), "{file} is empty");
        }
        std::fs::write(base.join("broken.svg"), "not svg").unwrap();
        assert!(rasterize(&base.join("broken.svg"), 48, &IconColors::default()).is_none());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// An icon made like Breeze's: the left half in the text color, the right half in the accent, both with the
    /// light-theme defaults the icon itself carries.
    const SCHEME_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><defs><style type="text/css" id="current-color-scheme">.ColorScheme-Text { color:#232629; } .ColorScheme-Accent { color:#3daee9; }</style></defs><rect width="8" height="16" style="fill:currentColor" class="ColorScheme-Text"/><rect x="8" width="8" height="16" style="fill:currentColor" class="ColorScheme-Accent"/></svg>"##;

    fn palette(light: bool, accent: egui::Color32) -> crate::ui::theme::SerpantinumTheme {
        crate::ui::theme::SerpantinumTheme {
            light,
            text: if light { egui::Color32::from_rgb(20, 20, 30) } else { egui::Color32::from_rgb(230, 225, 233) },
            base: if light { egui::Color32::from_rgb(250, 250, 250) } else { egui::Color32::from_rgb(15, 13, 19) },
            mauve: accent,
            ..Default::default()
        }
    }

    fn rgb(image: &egui::ColorImage, x: usize, y: usize) -> [u8; 3] {
        let p = image.pixels[y * image.size[0] + x];
        [p.r(), p.g(), p.b()]
    }

    fn icon_file(name: &str, svg: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compression_icon_colors_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, svg).unwrap();
        file
    }

    #[test]
    fn icons_made_for_the_color_scheme_take_the_text_and_accent_of_the_palette_light_or_dark() {
        let file = icon_file("scheme.svg", SCHEME_SVG);
        for (light, accent) in [(false, egui::Color32::from_rgb(204, 190, 255)), (true, egui::Color32::from_rgb(200, 40, 60))] {
            let theme = palette(light, accent);
            let image = rasterize(&file, 32, &icon_colors(&theme)).unwrap();
            assert_eq!(rgb(&image, 4, 16), [theme.text.r(), theme.text.g(), theme.text.b()], "text, light {light}");
            assert_eq!(rgb(&image, 28, 16), [accent.r(), accent.g(), accent.b()], "accent, light {light}");
        }
        // Without a palette the icon's own defaults would be drawn: dark gray text, Breeze blue
        let own = rasterize(&file, 32, &IconColors::default()).unwrap();
        assert_eq!((rgb(&own, 4, 16), rgb(&own, 28, 16)), ([0x23, 0x26, 0x29], [0x3d, 0xae, 0xe9]));
    }

    #[test]
    fn current_color_with_no_color_of_its_own_is_the_text_color_and_icons_in_their_own_colors_keep_them() {
        let bare = icon_file("bare.svg", r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect width="16" height="16" fill="currentColor"/></svg>"##);
        let own = icon_file("own.svg", SVG);
        let named = icon_file("named.svg", r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" color="#10a020"><rect width="16" height="16" fill="currentColor"/></svg>"##);
        for light in [false, true] {
            let theme = palette(light, egui::Color32::from_rgb(200, 40, 60));
            let sheet = icon_colors(&theme);
            assert_eq!(rgb(&rasterize(&bare, 16, &sheet).unwrap(), 8, 8), [theme.text.r(), theme.text.g(), theme.text.b()], "light {light}");
            assert_eq!(rgb(&rasterize(&own, 16, &sheet).unwrap(), 8, 8), [0x30, 0x70, 0xff], "light {light}");
            assert_eq!(rgb(&rasterize(&named, 16, &sheet).unwrap(), 8, 8), [0x10, 0xa0, 0x20], "light {light}");
        }
    }

    #[test]
    fn another_palette_draws_the_icons_again_and_the_same_palette_keeps_them() {
        let file = icon_file("cached.svg", SCHEME_SVG);
        let ctx = egui::Context::default();
        let dark = palette(false, egui::Color32::from_rgb(204, 190, 255));
        set_colors(&ctx, &dark);
        let first = texture(&ctx, &file, 24).unwrap();
        assert_eq!(texture(&ctx, &file, 24).unwrap().id(), first.id(), "one texture per palette");
        set_colors(&ctx, &palette(true, egui::Color32::from_rgb(200, 40, 60)));
        let light = texture(&ctx, &file, 24).unwrap();
        assert_ne!(light.id(), first.id(), "the light palette draws the icon again");
        set_colors(&ctx, &dark);
        assert_ne!(texture(&ctx, &file, 24).unwrap().id(), light.id());
    }

    #[test]
    fn applying_a_palette_gives_it_to_the_icons_in_its_solid_colors() {
        let ctx = egui::Context::default();
        let solid = palette(true, egui::Color32::from_rgb(200, 40, 60));
        let mut faded = solid;
        faded.text = egui::Color32::from_rgb(1, 2, 3);
        crate::ui::theme::apply(&ctx, &faded, &solid);
        assert_eq!(*colors(&ctx), icon_colors(&solid));
    }

    /// How often a settings file of GTK or KDE was read for the theme's name.
    pub(super) static SETTINGS_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[test]
    fn the_settings_files_are_read_once_and_not_for_every_icon_drawn() {
        use std::sync::atomic::Ordering::Relaxed;
        let _ = theme_name();
        let after_first = SETTINGS_READS.load(Relaxed);
        // As often as icons are drawn in a few frames
        for _ in 0..200 {
            let _ = theme_name();
        }
        assert_eq!(SETTINGS_READS.load(Relaxed), after_first, "the files were read again for every name asked for");
        // The portal's name still counts at once, and goes back to the files' when the portal names none
        set_desktop_icon_theme(Some("From-The-Portal".into()));
        assert_eq!(theme_name(), "From-The-Portal");
        set_desktop_icon_theme(None);
        assert_ne!(theme_name(), "From-The-Portal");
        assert_eq!(SETTINGS_READS.load(Relaxed), after_first);
    }

    #[test]
    fn the_portal_names_the_theme_before_gtk_and_kde_as_gtk_itself_does() {
        let gtk = Some("[Settings]\ngtk-icon-theme-name=Papirus\n".to_owned());
        let kde = Some("[Icons]\nTheme=breeze\n".to_owned());
        assert_eq!(theme_name_from(Some("Yaru".into()), std::slice::from_ref(&gtk), kde.clone()), "Yaru", "GNOME's setting, through the portal");
        assert_eq!(theme_name_from(None, &[None, gtk.clone()], kde.clone()), "Papirus", "then GTK's settings.ini, 4.0 or 3.0");
        assert_eq!(theme_name_from(Some(String::new()), &[None, None], kde), "breeze", "then KDE's");
        assert_eq!(theme_name_from(None, &[], None), "Adwaita");
    }

    #[test]
    fn the_theme_name_comes_from_the_settings_of_the_desktop() {
        let gtk = "[Settings]\ngtk-theme-name=Adwaita-dark\ngtk-icon-theme-name=Papirus-Dark\n";
        assert_eq!(ini_value(gtk, "Settings", "gtk-icon-theme-name").as_deref(), Some("Papirus-Dark"));
        let kde = "[General]\nTheme=x\n\n[Icons]\nTheme=breeze-dark\n";
        assert_eq!(ini_value(kde, "Icons", "Theme").as_deref(), Some("breeze-dark"));
        assert_eq!(ini_value("[Settings]\ngtk-icon-theme-name=\n", "Settings", "gtk-icon-theme-name"), None);
    }

    #[test]
    fn a_file_type_asks_for_its_own_icon_then_ever_more_general_ones() {
        let names = names_for_mime("image/png");
        assert_eq!(names.first().map(String::as_str), Some("image-png"));
        assert!(names.contains(&"image-x-generic".to_owned()));
        assert_eq!(names.last().map(String::as_str), Some("unknown"));
        assert_eq!(names_for(true, "Music"), ["folder"]);
    }
}
