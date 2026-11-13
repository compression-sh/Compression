//! Interface font installation and discovery of the fonts installed on the system.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use eframe::egui;

use super::icons;

/// Install the interface font (the bundled Adwaita Sans unless `custom` points at a font file).
/// Adwaita Sans stays behind a custom font as a fallback for glyphs it lacks. When `custom` cannot be used, Adwaita
/// Sans alone is installed, and what was wrong comes back to be said; the setting itself is left as it is.
pub fn install(ctx: &egui::Context, custom: Option<&Path>) -> Option<String> {
    let (fonts, problem) = definitions(custom);
    ctx.set_fonts(fonts);
    problem
}

/// The fonts `install` sets, and what was wrong with `custom` if it could not be used.
fn definitions(custom: Option<&Path>) -> (egui::FontDefinitions, Option<String>) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "adwaita_sans".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!("../../assets/fonts/AdwaitaSans-Regular.ttf"))),
    );
    let proportional = fonts.families.entry(egui::FontFamily::Proportional).or_default();
    proportional.insert(0, "adwaita_sans".to_owned());

    let mut problem = None;
    if let Some(path) = custom {
        match usable_font(path) {
            Ok(data) => {
                fonts.font_data.insert("custom_ui".to_owned(), Arc::new(egui::FontData::from_owned(data)));
                fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "custom_ui".to_owned());
            }
            Err(why) => problem = Some(why),
        }
    }

    // Icons: the Symbols Nerd Font Mono Serpantinum pins, in a family of its own with no
    // fallback, so every icon comes from one font with identical metrics.
    fonts.font_data.insert(
        "nerd_symbols".to_owned(),
        Arc::new(egui::FontData::from_static(icons::FONT_DATA)),
    );
    fonts.families.insert(egui::FontFamily::Name(icons::FAMILY.into()), vec!["nerd_symbols".to_owned()]);
    (fonts, problem)
}

/// The bytes of the font file at `path`, if egui can use them. egui reads a font with skrifa when it first draws text
/// and stops the program when that fails, so the same reading is tried here first: a file that is not there, cannot
/// be read, is empty, damaged or of another kind is turned down with the reason.
fn usable_font(path: &Path) -> Result<Vec<u8>, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let data = std::fs::read(path)
        .map_err(|err| crate::i18n::trf("The font “{}” could not be read ({}). Adwaita Sans is used instead.", &[&name, &crate::fs::describe::io_error(&err)]))?;
    // The face egui takes from the file: the first (`FontData` index 0). One whose tables are cut off reads, but has no
    // letter it can draw: the window would quietly show Adwaita Sans, so that is said too
    let draws = skrifa::FontRef::from_index(&data, 0).is_ok_and(|font| {
        use skrifa::MetadataProvider as _;
        let outlines = font.outline_glyphs();
        font.charmap().mappings().any(|(_, glyph)| outlines.get(glyph).is_some())
    });
    if !draws {
        return Err(crate::i18n::trf("“{}” is not a font Compression can use. Adwaita Sans is used instead.", &[&name]));
    }
    Ok(data)
}

/// One installed font family and the file of its regular style.
#[derive(Debug, Clone)]
pub struct SystemFont {
    pub family: String,
    pub path: PathBuf,
}

/// Installed font families (via fontconfig), regular style only, sorted by name.
pub fn system_fonts() -> &'static [SystemFont] {
    static CACHED: std::sync::OnceLock<Vec<SystemFont>> = std::sync::OnceLock::new();
    CACHED.get_or_init(|| {
        let Some(output) = Command::new("fc-list")
            .args(["--format", "%{file}\t%{family[0]}\t%{style[0]}\n"])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);

        let mut best: std::collections::HashMap<String, (u8, PathBuf)> = std::collections::HashMap::new();
        for line in text.lines() {
            let mut parts = line.split('\t');
            let (Some(file), Some(family), style) = (parts.next(), parts.next(), parts.next().unwrap_or("")) else { continue };
            let ext = Path::new(file).extension().map(|e| e.to_string_lossy().to_lowercase());
            if !matches!(ext.as_deref(), Some("ttf" | "otf")) || family.is_empty() {
                continue;
            }
            let style = style.to_lowercase();
            let score = match style.as_str() {
                "regular" | "book" | "roman" | "normal" => 0,
                "" => 1,
                s if s.contains("bold") || s.contains("italic") || s.contains("oblique") => 3,
                _ => 2,
            };
            let entry = best.entry(family.to_owned()).or_insert((u8::MAX, PathBuf::new()));
            if score < entry.0 {
                *entry = (score, PathBuf::from(file));
            }
        }
        let mut list: Vec<SystemFont> = best
            .into_iter()
            .filter(|(_, (score, _))| *score < 3)
            .map(|(family, (_, path))| SystemFont { family, path })
            .collect();
        list.sort_by_key(|f| f.family.to_lowercase());
        list
    }).as_slice()
}

/// Pre-scan installed fonts on a background thread so opening Appearance settings never stalls.
pub fn preload() {
    std::thread::spawn(|| {
        let _ = system_fonts();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADWAITA: &[u8] = include_bytes!("../../assets/fonts/AdwaitaSans-Regular.ttf");

    /// Draws text with the fonts of `ctx` for a few frames: where egui reads the fonts, and where a font it cannot read
    /// used to stop the program.
    fn draw_text(ctx: &egui::Context) {
        for n in 0..3 {
            let raw = egui::RawInput { time: Some(n as f64 * 0.05), ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                ui.label("Hallo Welt, ünï, Привет");
            });
            out.textures_delta.clear();
        }
    }

    #[test]
    fn a_font_file_that_cannot_be_used_falls_back_to_adwaita_and_says_why_and_never_stops_the_program() {
        let dir = std::env::temp_dir().join(format!("compression_fonts_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let mut bad: Vec<(PathBuf, &str)> = vec![
            (dir.join("not-there.ttf"), "could not be read (Not found)"),
            (write("empty.ttf", b""), "is not a font"),
            (write("text.ttf", b"this is not a font at all"), "is not a font"),
            (dir.clone(), "could not be read"),
            // A real font cut off at its start, inside its table directory, and half way
            (write("cut-12.ttf", &ADWAITA[..12]), "is not a font"),
            (write("cut-100.ttf", &ADWAITA[..100]), "is not a font"),
        ];
        // A collection of the wrong kind: a WOFF2 header, which egui does not read
        let mut woff2 = b"wOF2".to_vec();
        woff2.extend_from_slice(&ADWAITA[4..2000]);
        bad.push((write("font.woff2", &woff2), "is not a font"));
        // SAFETY: geteuid has no preconditions
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let locked = write("locked.ttf", ADWAITA);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            bad.push((locked, "could not be read (Permission denied)"));
        }
        for (path, why) in &bad {
            let ctx = egui::Context::default();
            let problem = install(&ctx, Some(path)).unwrap_or_else(|| panic!("{} was taken for a font", path.display()));
            assert!(problem.contains(why) && problem.ends_with("Adwaita Sans is used instead."), "{}: {problem}", path.display());
            let (fonts, _) = definitions(Some(path));
            assert!(!fonts.font_data.contains_key("custom_ui"), "{}", path.display());
            assert_eq!(fonts.families[&egui::FontFamily::Proportional][0], "adwaita_sans");
            draw_text(&ctx);
        }
        // A file egui reads, even cut short behind its tables, is installed in front of Adwaita Sans and drawn with
        let whole = write("whole.ttf", ADWAITA);
        let half = write("half.ttf", &ADWAITA[..ADWAITA.len() / 2]);
        for path in [&whole, &half] {
            let ctx = egui::Context::default();
            let problem = install(&ctx, Some(path));
            if path == &half && problem.is_some() {
                continue;
            }
            assert_eq!(problem, None, "{}", path.display());
            assert_eq!(definitions(Some(path)).0.families[&egui::FontFamily::Proportional][0], "custom_ui");
            draw_text(&ctx);
        }
        // No font chosen: nothing to say
        assert_eq!(install(&egui::Context::default(), None), None);
        let _ = std::fs::set_permissions(dir.join("locked.ttf"), std::os::unix::fs::PermissionsExt::from_mode(0o600));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
