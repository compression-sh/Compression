//! Chooser mode: `compression --chooser …` runs the file manager as an open / save dialog. The chosen
//! paths go to stdout as `file://` URIs, one per line; the exit code is 0 for a choice, 1 for
//! a cancel and 2 when the dialog could not be opened at all. The portal backend (`portal.rs`) starts it for every FileChooser request.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Pick existing files.
    Open,
    /// Pick existing folders.
    Folder,
    /// Name one new file.
    Save,
    /// Pick the folder that several named files go into.
    SaveFiles,
}

#[derive(Clone, Debug)]
pub enum Pattern {
    Glob(String),
    Mime(String),
}

#[derive(Clone, Debug)]
pub struct Filter {
    pub label: String,
    pub patterns: Vec<Pattern>,
}

impl Filter {
    pub fn accepts(&self, path: &Path) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.patterns.iter().any(|p| match p {
            // One pattern on the whole name, as GTK reads a filter's glob: a comma is a character (`*,v` is RCS files),
            // and text without wildcards is the name itself, not part of it. Letters match in either case.
            Pattern::Glob(g) => crate::fs::glob::wildcard(&g.to_lowercase(), &name.to_lowercase()),
            Pattern::Mime(m) => crate::fs::apps::file_is_mime(path, m),
        })
    }
}

/// An extra setting the requesting program asked for: a checkbox (no options) or a drop-down.
#[derive(Clone, Debug)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub options: Vec<(String, String)>,
    pub initial: String,
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub mode: Mode,
    pub multiple: bool,
    pub title: Option<String>,
    pub accept_label: Option<String>,
    pub folder: Option<PathBuf>,
    pub name: Option<String>,
    /// Names for `SaveFiles`, which the chosen folder gets one URI each.
    pub files: Vec<String>,
    pub filters: Vec<Filter>,
    pub current_filter: usize,
    /// The requester's window as the portal names it (`wayland:<handle>`).
    pub parent: Option<String>,
    /// The requesting program, to start where it last chose something.
    pub app_id: Option<String>,
    pub choices: Vec<Choice>,
}

/// The options of a file dialog request that stand alone, and those that take the next argument as their value.
/// What the portal builds (`portal.rs`) uses these and no others.
const FLAG_OPTIONS: &[&str] = &["--chooser", "--multiple"];
const VALUE_OPTIONS: &[&str] = &["--mode", "--title", "--accept", "--folder", "--name", "--file", "--parent", "--app-id", "--choice", "--current-filter", "--filter"];

/// The first argument of a dialog request that is not one of its options or the value of one: a folder or a stray
/// word, which a dialog has no use for.
pub fn stray_argument(args: &[String]) -> Option<&String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if VALUE_OPTIONS.contains(&arg.as_str()) {
            it.next();
        } else if !FLAG_OPTIONS.contains(&arg.as_str()) {
            return Some(arg);
        }
    }
    None
}

impl Spec {
    /// Reads the arguments after `--chooser`; `None` when they are not a chooser request.
    pub fn parse(args: &[String]) -> Option<Self> {
        let mut spec = Spec {
            mode: Mode::Open,
            multiple: false,
            title: None,
            accept_label: None,
            folder: None,
            name: None,
            files: Vec::new(),
            filters: Vec::new(),
            current_filter: 0,
            parent: None,
            app_id: None,
            choices: Vec::new(),
        };
        let mut it = args.iter();
        let mut asked_for_choices = 0;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--chooser" => {}
                "--multiple" => spec.multiple = true,
                "--mode" => {
                    spec.mode = match it.next()?.as_str() {
                        "open" => Mode::Open,
                        "folder" => Mode::Folder,
                        "save" => Mode::Save,
                        "save-files" => Mode::SaveFiles,
                        _ => return None,
                    }
                }
                "--title" => spec.title = it.next().cloned().filter(|s| !s.is_empty()),
                "--accept" => spec.accept_label = it.next().cloned().filter(|s| !s.is_empty()),
                "--folder" => spec.folder = it.next().map(PathBuf::from),
                // Only the last component counts, for a suggested name as for a file: what a program proposes is never a
                // way out of the folder shown. A path the person types into the field later still goes where it says
                "--name" => spec.name = it.next().and_then(|n| Path::new(n).file_name()).map(|n| n.to_string_lossy().into_owned()),
                "--file" => spec.files.extend(it.next().and_then(|f| Path::new(f).file_name()).map(|n| n.to_string_lossy().into_owned())),
                "--parent" => spec.parent = it.next().cloned().filter(|s| !s.is_empty()),
                "--app-id" => spec.app_id = it.next().cloned().filter(|s| !s.is_empty()),
                // id, label, initial value, then option id and label pairs
                "--choice" => {
                    asked_for_choices += 1;
                    // One that cannot be read is skipped; the rest of the request still stands
                    match it.next().and_then(|raw| Choice::parse(raw)) {
                        Some(choice) => spec.choices.push(choice),
                        None => eprintln!("compression: ignoring a --choice that cannot be read"),
                    }
                }
                "--current-filter" => spec.current_filter = it.next()?.parse().unwrap_or(0),
                // label, then patterns written `g:*.png` (glob) or `m:image/png` (MIME type)
                "--filter" => {
                    let raw = it.next()?;
                    let mut parts = raw.split('\t');
                    let label = parts.next().unwrap_or_default().to_owned();
                    let patterns = parts
                        .filter_map(|p| match p.split_at_checked(2)? {
                            ("g:", g) => Some(Pattern::Glob(g.to_owned())),
                            ("m:", m) => Some(Pattern::Mime(m.to_owned())),
                            _ => None,
                        })
                        .collect();
                    spec.filters.push(Filter { label, patterns });
                }
                _ => {}
            }
        }
        // A request whose every choice is broken is not one this dialog can answer as asked
        if asked_for_choices > 0 && spec.choices.is_empty() {
            return None;
        }
        spec.current_filter = spec.current_filter.min(spec.filters.len().saturating_sub(1));
        Some(spec)
    }
}

impl Choice {
    /// `id`, `label`, `initial`, then pairs of option id and label, separated by tabs.
    fn parse(raw: &str) -> Option<Choice> {
        let mut parts = raw.split('\t').map(str::to_owned);
        let (id, label, initial) = (parts.next()?, parts.next()?, parts.next()?);
        if id.is_empty() {
            return None;
        }
        let rest: Vec<String> = parts.collect();
        let options = rest.chunks(2).filter(|pair| pair.len() == 2).map(|pair| (pair[0].clone(), pair[1].clone())).collect();
        Some(Choice { id, label, options, initial })
    }
}

pub struct Chooser {
    pub spec: Spec,
    pub name_input: String,
    pub filter: usize,
    /// The current value of each of the spec's choices.
    pub choice_values: Vec<String>,
    /// The file whose name was last copied into the name field, so that is done once per click.
    pub filled: Option<PathBuf>,
    /// Set until the name field has taken focus and selected the part of the name before the dot.
    pub focus_name: bool,
    /// Keeps the window tied to its parent while the dialog is open.
    pub parent: Option<crate::wayland_parent::Parent>,
    /// The window size last seen and since when, so a resize is stored once it settles.
    size_seen: Option<([f32; 2], std::time::Instant)>,
    size_stored: Option<[f32; 2]>,
}

impl Chooser {
    pub fn new(spec: Spec) -> Self {
        let name_input = spec.name.clone().unwrap_or_default();
        let filter = spec.current_filter;
        let choice_values = spec.choices.iter().map(|c| c.initial.clone()).collect();
        let focus_name = spec.mode == Mode::Save;
        Self { spec, name_input, filter, choice_values, filled: None, focus_name, parent: None, size_seen: None, size_stored: None }
    }

    /// Remembers the size the user gave the dialog, once a resize has settled. Kept in a file of its
    /// own: the settings file is whole-file rewritten by whichever Compression window saves last.
    pub fn remember_size(&mut self, ctx: &eframe::egui::Context) {
        let Some(rect) = ctx.input(|i| i.viewport().inner_rect) else { return };
        let zoom = ctx.zoom_factor();
        let size = [(rect.width() * zoom).round(), (rect.height() * zoom).round()];
        match self.size_seen {
            Some((seen, _)) if seen == size => {}
            _ => self.size_seen = Some((size, std::time::Instant::now())),
        }
        let Some((seen, since)) = self.size_seen else { return };
        if self.size_stored.is_none() {
            // The first frame is the size the dialog opened with
            self.size_stored = Some(seen);
        } else if self.size_stored != Some(seen) {
            if since.elapsed() >= SIZE_SETTLE {
                save_size(seen);
                self.size_stored = Some(seen);
            } else {
                ctx.request_repaint_after(SIZE_SETTLE);
            }
        }
    }

    pub fn takes_name(&self) -> bool {
        self.spec.mode == Mode::Save
    }
}

/// The size a dialog opens at when none was remembered, and the smallest it is restored to.
pub const DEFAULT_SIZE: [f32; 2] = [760.0, 520.0];
const SIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

fn size_file() -> Option<PathBuf> {
    dirs::state_dir().or_else(dirs::config_dir).map(|d| d.join("compression").join("chooser-size"))
}

/// The size the dialog was last left at.
pub fn saved_size() -> Option<[f32; 2]> {
    let text = std::fs::read_to_string(size_file()?).ok()?;
    let (w, h) = text.trim().split_once('x')?;
    let (w, h) = (w.parse::<f32>().ok()?, h.parse::<f32>().ok()?);
    (w >= 360.0 && h >= 420.0 && w <= 8192.0 && h <= 8192.0).then_some([w, h])
}

fn save_size([w, h]: [f32; 2]) {
    let Some(file) = size_file() else { return };
    let _ = crate::fs::stored::save(&file, format!("{w}x{h}"));
}

fn folders_file() -> Option<PathBuf> {
    dirs::state_dir().or_else(dirs::config_dir).map(|d| d.join("compression").join("chooser-folders"))
}

/// The folder a program last chose something in (`app_id` is empty for programs the portal cannot name).
pub fn remembered_folder(app_id: Option<&str>) -> Option<PathBuf> {
    let text = std::fs::read_to_string(folders_file()?).ok()?;
    let key = app_id.unwrap_or("");
    text.lines().find_map(|l| l.split_once('\t').filter(|(id, _)| *id == key).map(|(_, p)| PathBuf::from(p))).filter(|p| p.is_dir())
}

fn remember_folder(app_id: Option<&str>, folder: &Path) {
    let Some(file) = folders_file() else { return };
    let key = app_id.unwrap_or("");
    // The folders of the other programs stay: a file that cannot be read is not written over (`stored`)
    let text = crate::fs::stored::load_with(&file, |bytes| String::from_utf8(bytes.to_vec()).ok()).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().filter(|l| l.split_once('\t').is_some_and(|(id, _)| id != key)).map(str::to_owned).collect();
    lines.push(format!("{key}\t{}", folder.display()));
    let _ = crate::fs::stored::save(&file, lines.join("\n") + "\n");
}

impl Chooser {
    /// Answers with `paths`, the filter and the choices as they stand, remembering where it was done.
    pub fn answer(&self, paths: &[PathBuf], folder: &Path) -> ! {
        remember_folder(self.spec.app_id.as_deref(), folder);
        let mut extra = Vec::new();
        if !self.spec.filters.is_empty() {
            extra.push(format!("filter\t{}", self.filter));
        }
        for (choice, value) in self.spec.choices.iter().zip(&self.choice_values) {
            extra.push(format!("choice\t{}\t{value}", choice.id));
        }
        finish(paths, &extra)
    }
}

/// `file://` URI with the characters a URI cannot hold escaped.
pub fn uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Path behind a `file://` URI (or a plain path); `None` for other schemes, and for a `file://`
/// URI that names another computer.
pub fn from_uri(text: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let (host, rest) = if let Some(rest) = text.strip_prefix("file://") {
        // `file://host/path`: only the local host is ours
        rest.find('/').map_or((rest, ""), |i| rest.split_at(i))
    } else if let Some(path) = text.strip_prefix("file:").filter(|p| p.starts_with('/')) {
        // `file:/path` has no host at all, and means the same as `file:///path`
        ("", path)
    } else {
        return (!text.contains("://")).then(|| PathBuf::from(text));
    };
    if !crate::clipboard::is_local_host(host) {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = rest.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Prints the choice and ends the process; the portal backend reads it from stdout. Lines that
/// are not URIs carry the rest of the answer: `filter<TAB>index` and `choice<TAB>id<TAB>value`.
pub fn finish(paths: &[PathBuf], extra: &[String]) -> ! {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for p in paths {
        let _ = writeln!(out, "{}", uri(p));
    }
    for line in extra {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
    std::process::exit(0);
}

/// The exit code of a dialog the user cancelled.
pub const EXIT_CANCELLED: i32 = 1;
/// The exit code of a dialog that could not be opened (no display, no graphics): not a cancel.
pub const EXIT_FAILED: i32 = 2;

pub fn cancel() -> ! {
    std::process::exit(EXIT_CANCELLED);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trips() {
        let p = PathBuf::from("/home/a b/ü#1.txt");
        assert_eq!(from_uri(&uri(&p)), Some(p));
        assert_eq!(uri(Path::new("/a b")), "file:///a%20b");
    }

    #[test]
    fn a_file_without_a_telling_name_is_told_apart_by_its_content() {
        // Skipped where no shared-mime-info is installed, which then shows everything by design
        if crate::fs::apps::mime_of_name("a.png").is_none() || !Path::new("/usr/share/mime/magic").exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("compression_chooser_magic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mime = |m: &str| Filter { label: String::new(), patterns: vec![Pattern::Mime(m.into())] };
        // No extension: a PNG by its signature, and text that is not one
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        png.extend([0u8; 32]);
        std::fs::write(dir.join("scan"), &png).unwrap();
        std::fs::write(dir.join("notes"), "just some words\n").unwrap();
        assert!(mime("image/png").accepts(&dir.join("scan")));
        assert!(mime("image/*").accepts(&dir.join("scan")));
        assert!(!mime("image/*").accepts(&dir.join("notes")));
        // `.ts` is a video and a Qt translation alike by its name: the content decides
        let mut video = Vec::new();
        for _ in 0..8 {
            video.push(0x47);
            video.extend([0u8; 187]);
        }
        std::fs::write(dir.join("clip.ts"), &video).unwrap();
        std::fs::write(dir.join("app_de.ts"), "<?xml version=\"1.0\"?>\n<!DOCTYPE TS>\n<TS version=\"2.1\" language=\"de\">\n</TS>\n").unwrap();
        assert!(mime("video/*").accepts(&dir.join("clip.ts")));
        assert!(!mime("video/*").accepts(&dir.join("app_de.ts")), "the translation is no video");
        assert!(mime("text/vnd.trolltech.linguist").accepts(&dir.join("app_de.ts")));
        // A name that tells is taken at its word, without reading the file
        assert!(mime("image/png").accepts(&dir.join("not-there.png")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_glob_of_a_filter_is_one_pattern_on_the_whole_name() {
        let glob = |g: &str| Filter { label: String::new(), patterns: vec![Pattern::Glob(g.into())] };
        // `*,v` names RCS files; it is not `*` and `v`
        assert!(glob("*,v").accepts(Path::new("/x/main.c,v")));
        assert!(!glob("*,v").accepts(Path::new("/x/main.c")));
        // A name without wildcards is that name, not every name that contains it
        assert!(glob("Makefile").accepts(Path::new("/x/Makefile")));
        assert!(!glob("Makefile").accepts(Path::new("/x/Makefile.am")));
        assert!(!glob("Makefile").accepts(Path::new("/x/GNUmakefile")));
        // Wildcards, classes and either case
        assert!(glob("*.png").accepts(Path::new("/x/A.PNG")));
        assert!(!glob("*.png").accepts(Path::new("/x/a.png.txt")));
        assert!(glob("img_[0-9]*.jpg").accepts(Path::new("/x/IMG_7.JPG")));
    }

    #[test]
    fn filters_match_globs_and_mimes() {
        let f = Filter { label: "Images".into(), patterns: vec![Pattern::Mime("image/*".into()), Pattern::Glob("*.xyz".into())] };
        assert!(f.accepts(Path::new("/x/a.xyz")));
        // Without a shared-mime-info database (a build sandbox) MIME patterns show everything
        if crate::fs::apps::mime_of_name("a.png").is_some() {
            assert!(f.accepts(Path::new("/x/a.PNG")));
            assert!(!f.accepts(Path::new("/x/a.txt")));
        }
    }

    #[test]
    fn mime_filters_follow_the_shared_database() {
        let mime = |m: &str| Filter { label: String::new(), patterns: vec![Pattern::Mime(m.into())] };
        // Skipped where no shared-mime-info is installed, which then shows everything by design
        if crate::fs::apps::mime_of_name("a.png").is_none() {
            return;
        }
        assert!(mime("application/zip").accepts(Path::new("/x/report.docx")));
        assert!(mime("text/*").accepts(Path::new("/x/main.rs")));
        assert!(!mime("image/*").accepts(Path::new("/x/a.tar.gz")));
        assert!(!mime("application/x-foo").accepts(Path::new("/x/a.png")));
    }

    #[test]
    fn plain_paths_and_other_schemes_in_from_uri() {
        assert_eq!(from_uri("file:///home/a%20b"), Some(PathBuf::from("/home/a b")));
        assert_eq!(from_uri("rel/dir"), Some(PathBuf::from("rel/dir")));
        assert_eq!(from_uri("https://x/y"), None);
        // A file on another computer is not opened as a local one
        assert_eq!(from_uri("file://other-host/pfad"), None);
        assert_eq!(from_uri("file://nas/"), None);
        assert_eq!(from_uri("file://localhost/home/a%20b"), Some(PathBuf::from("/home/a b")));
        assert_eq!(from_uri("file://LOCALHOST/x"), Some(PathBuf::from("/x")));
        assert_eq!(from_uri("file:///x"), Some(PathBuf::from("/x")));
        assert_eq!(from_uri("file://"), Some(PathBuf::new()));
        // One slash is a local path too, and means the same as three
        assert_eq!(from_uri("file:/tmp/example"), Some(PathBuf::from("/tmp/example")));
        assert_eq!(from_uri("file:/tmp/a%20b"), from_uri("file:///tmp/a%20b"));
        assert_eq!(from_uri("file:///tmp/example"), Some(PathBuf::from("/tmp/example")));
        assert_eq!(from_uri("file://other-host/tmp/example"), None);
        // `file:` without a slash is not a URI of ours
        assert_eq!(from_uri("file:relative"), Some(PathBuf::from("file:relative")));
    }

    #[test]
    fn file_names_cannot_leave_the_folder() {
        let args: Vec<String> = ["--chooser", "--mode", "save-files", "--file", "../../etc/passwd", "--file", "/abs/x.txt", "--file", "..", "--file", "ok"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(Spec::parse(&args).unwrap().files, ["passwd", "x.txt", "ok"]);
    }

    fn parse(args: &[&str]) -> Option<Spec> {
        Spec::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_choice_that_cannot_be_read_is_skipped_and_the_rest_of_the_request_stands() {
        let spec = parse(&[
            "--chooser", "--mode", "save", "--name", "a.txt",
            "--choice", "encoding\tEncoding\tutf8\tutf8\tUTF-8\tlatin1\tLatin-1",
            "--choice", "only-an-id",
            "--choice", "",
            "--choice", "\tno id\tx",
            "--choice", "bom\tAdd BOM\tfalse",
            "--filter", "Text\tg:*.txt",
        ])
        .expect("the request is still readable");
        assert_eq!(spec.mode, Mode::Save);
        assert_eq!(spec.name.as_deref(), Some("a.txt"));
        assert_eq!(spec.filters.len(), 1);
        let ids: Vec<_> = spec.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["encoding", "bom"]);
        assert_eq!(spec.choices[0].options, [("utf8".to_owned(), "UTF-8".to_owned()), ("latin1".to_owned(), "Latin-1".to_owned())]);
        assert_eq!(spec.choices[1].initial, "false");
        // A --choice with nothing after it is one that cannot be read too
        assert_eq!(parse(&["--chooser", "--choice", "ok\tOK\tx", "--choice"]).unwrap().choices.len(), 1);
    }

    #[test]
    fn a_request_whose_choices_are_all_broken_is_refused_and_one_without_choices_is_fine() {
        assert!(parse(&["--chooser", "--mode", "open", "--choice", "broken"]).is_none());
        assert!(parse(&["--chooser", "--choice"]).is_none());
        assert!(parse(&["--chooser", "--choice", "a", "--choice", "b\tonly label"]).is_none());
        let plain = parse(&["--chooser", "--mode", "open"]).expect("no choices asked for");
        assert!(plain.choices.is_empty());
    }

    #[test]
    fn parses_arguments() {
        let args: Vec<String> = ["--chooser", "--mode", "save", "--name", "a.txt", "--filter", "Text\tg:*.txt\tm:text/plain"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let spec = Spec::parse(&args).unwrap();
        assert_eq!(spec.mode, Mode::Save);
        assert_eq!(spec.name.as_deref(), Some("a.txt"));
        assert_eq!(spec.filters[0].patterns.len(), 2);
    }

    #[test]
    fn the_words_of_a_dialog_request_are_its_options_and_their_values() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for option in VALUE_OPTIONS {
            assert_eq!(stray_argument(&words(&["--chooser", option, "any value"])), None, "{option}");
            // A value that looks like an option is a value
            assert_eq!(stray_argument(&words(&["--chooser", option, "--chooser"])), None, "{option}");
        }
        assert_eq!(stray_argument(&words(&["--chooser", "--multiple"])), None);
        assert_eq!(stray_argument(&words(&["--chooser", "--mode"])), None, "a missing value is for `Spec::parse` to refuse");
        assert_eq!(stray_argument(&words(&["--chooser", "--bogus", "--mode", "open", "/tmp"])), Some(&"--bogus".to_owned()));
        assert_eq!(stray_argument(&words(&["--chooser", "--mode", "open", "/tmp"])), Some(&"/tmp".to_owned()));
    }
}
