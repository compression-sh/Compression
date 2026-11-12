mod app;
mod chooser;
mod clipboard;
mod compat;
mod fs;
mod i18n;
mod instance;
mod keymap;
mod plugins;
mod portal;
mod settings;
mod settings_ui;
mod settings_watch;
mod theme_watch;
mod thumbs;
mod ui;
mod wayland_dnd;
mod wayland_parent;
mod x11_parent;

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use app::CompressionApp;
use ui::theme::SerpantinumTheme;

/// What `--version` / `-V` and `--help` / `-h` print, when `first` (the first argument) asks for it.
/// Only that exact first argument counts, so every other argument stays a folder (`./--version` is one).
fn info_for(first: Option<&str>) -> Option<String> {
    match first? {
        "--version" | "-V" => Some(format!("compression {}\n", env!("CARGO_PKG_VERSION"))),
        "--help" | "-h" => Some(format!(
            "\
compression {} - a small, fast, keyboard-friendly file manager

Usage:
  compression [--] [FOLDER | FILE | ADDRESS]...
                                   open folders, files (their folder, with the file selected) or server
                                   addresses such as sftp://me@example.com/srv; by default the current folder.
                                   The first fills the window, up to seven more get windows of their own and
                                   the rest become tabs of the first (at most 50 in all)
  compression --chooser --mode open|folder|save|save-files
                                   the file dialog for one request: prints the chosen file:// URIs
                                   and exits with 0 (chosen), 1 (cancelled) or 2 (failed)
  compression --portal             the xdg-desktop-portal service (FileChooser, FileManager1)
  compression --resident           keep a background instance, so new windows open at once
  compression -V, --version        print the version and exit
  compression -h, --help           print this help and exit

--version and --help count only as the first argument. A name that starts with a dash is given as ./-name,
or after --, which ends the options. Any other option is an error (exit code 2), and so is a name that is
not valid UTF-8; --portal and --resident take no other argument.
",
            env!("CARGO_PKG_VERSION")
        )),
        _ => None,
    }
}

/// The arguments of this run, as the system gave them: a name that is not valid UTF-8 (Linux allows any bytes in
/// one) must reach `parse_launch` as it is, to be refused there, not be turned into another name.
fn arguments() -> Vec<OsString> {
    env::args_os().skip(1).collect()
}

/// The most windows one launch opens; the targets after them become tabs of the first.
const MAX_WINDOWS: usize = 8;
/// The most targets one launch takes at all: `compression *` in a folder of thousands is a mistake, not a wish.
const MAX_TARGETS: usize = 50;

/// What a launch was asked to open.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    /// A path, absolute or relative to where the launch was made.
    Path(String),
    /// A `file://` URI of this computer, as a launcher passes what was dropped on it.
    Uri(String),
    /// A server address (`sftp://…`).
    Address(String),
}

/// What the arguments of a launch mean.
#[derive(Debug, PartialEq)]
enum Launch {
    /// Print this and end (`--version`, `--help`).
    Info(String),
    Portal,
    Resident,
    /// A file dialog; the arguments have been read as a request.
    Chooser(Vec<String>),
    /// A window, with what it is to show (nothing: the usual start folder).
    Open(Vec<Target>),
    /// The arguments make no sense; this says why.
    Error(String),
}

/// Reads the arguments of a launch (the program's name not among them). Pure: it looks at nothing but the words,
/// so what a launch means is decided in one place and the same way for a first start and for a resident process.
/// - `--help`, `-h`, `--version` and `-V` only as the first argument; further on they are unknown options.
/// - `--portal` and `--resident` only alone, and not with `--chooser`.
/// - `--chooser` makes the rest a dialog request, read by `chooser::Spec`, with no targets.
/// - `--` ends the options: what follows is a target, whatever it looks like (`-x`, `--help`).
/// - Any other word that starts with a dash is an unknown option; a folder of that name is `./-name`.
fn parse_launch(args: &[OsString]) -> Launch {
    if let Some(first) = args.first().and_then(|a| a.to_str())
        && let Some(text) = info_for(Some(first))
    {
        return Launch::Info(text);
    }
    let mut words = Vec::with_capacity(args.len());
    for (n, arg) in args.iter().enumerate() {
        match arg.to_str() {
            Some(word) => words.push(word.to_owned()),
            None => return Launch::Error(format!("argument {} ({:?}) is not valid UTF-8, so it cannot be told apart from another name; open it from its folder", n + 1, arg.to_string_lossy())),
        }
    }
    let options = words.iter().position(|w| w == "--").unwrap_or(words.len());
    if words[..options].iter().any(|w| w == "--chooser") {
        if let Some(stray) = chooser::stray_argument(&words) {
            return Launch::Error(format!("a file dialog request takes no other argument than its options, not {stray:?}"));
        }
        return match chooser::Spec::parse(&words) {
            Some(_) => Launch::Chooser(words),
            None => Launch::Error("the file dialog request could not be read".to_owned()),
        };
    }
    for service in ["--portal", "--resident"] {
        if words[..options].iter().any(|w| w == service) {
            return if words.len() == 1 { if service == "--portal" { Launch::Portal } else { Launch::Resident } } else { Launch::Error(format!("{service} takes no other arguments")) };
        }
    }
    let mut targets = Vec::new();
    for (n, word) in words.iter().enumerate() {
        if n < options && word.starts_with('-') {
            return Launch::Error(format!("unknown option {word:?} (a folder of that name is ./{word}, or put -- before the names); see --help"));
        }
        if n == options {
            continue;
        }
        if word.is_empty() {
            return Launch::Error(format!("argument {} is empty", n + 1));
        }
        targets.push(if word.starts_with("file://") || word.starts_with("file:/") {
            if chooser::from_uri(word).is_none() {
                return Launch::Error(format!("{word:?} is not a file of this computer"));
            }
            Target::Uri(word.clone())
        } else if fs::network::Address::looks_like(word) {
            Target::Address(word.clone())
        } else {
            Target::Path(word.clone())
        });
    }
    if targets.len() > MAX_TARGETS {
        return Launch::Error(format!("too many targets ({}, at most {MAX_TARGETS})", targets.len()));
    }
    Launch::Open(targets)
}

/// Where the targets of a launch go: the first fills this window, the next ones (up to `MAX_WINDOWS` in all)
/// each get a window of their own, and the rest become tabs of this window.
fn place(targets: &[Target]) -> (Option<&Target>, &[Target], &[Target]) {
    let windows_end = targets.len().min(MAX_WINDOWS);
    (targets.first(), targets.get(1..windows_end).unwrap_or_default(), targets.get(windows_end..).unwrap_or_default())
}

/// The arguments without the environment an elevated start carries in front of them (see `fs::admin`). That is
/// taken from the words as text, the rest stays as it was given.
fn without_environment(raw: Vec<OsString>) -> Vec<OsString> {
    let text: Vec<String> = raw.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let kept = fs::admin::take_environment(text.clone());
    let skipped = text.len() - kept.len();
    raw.into_iter().skip(skipped).collect()
}

fn main() -> eframe::Result {
    // An elevated start carries its environment in the arguments (see fs::admin): taken out first of all,
    // but not before `--version` and `--help` are answered, which touch nothing: no migration, no instance, no display
    let raw = arguments();
    let args = match parse_launch(&raw) {
        Launch::Info(text) => {
            use std::io::Write;
            // A closed pipe (`compression --help | head -1`) is no reason to panic
            let _ = std::io::stdout().write_all(text.as_bytes());
            return Ok(());
        }
        _ => without_environment(raw),
    };
    let launch = parse_launch(&args);
    if let Launch::Error(why) = &launch {
        eprintln!("compression: {why}");
        std::process::exit(2);
    }
    compat::migrate_user_data();
    match launch {
        Launch::Portal => {
            if let Err(err) = portal::serve() {
                eprintln!("compression: {err}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Launch::Resident => {
            instance::serve(open_window);
            return Ok(());
        }
        _ => {}
    }
    let dialog = matches!(launch, Launch::Chooser(_));
    // Every word is text here: `parse_launch` has refused the others
    let request = instance::Request::current(args.iter().map(|a| a.to_string_lossy().into_owned()).collect());
    // A file dialog belongs to its requester, and an administrator's window to its own process.
    // As a container's first process (the Docker image), leaving would stop the container.
    if !dialog && !fs::admin::is_admin() && std::process::id() != 1 {
        if !settings::Settings::load().run_in_background {
            instance::dismiss(&request);
        } else if instance::hand_off(&request) {
            return Ok(());
        }
    }
    let result = open_window(request);
    // A dialog that could not be opened must not end like a cancel, which is exit code 1
    if dialog && let Err(err) = &result {
        eprintln!("Error: {err}");
        std::process::exit(chooser::EXIT_FAILED);
    }
    result
}

/// The path a target stands for, relative ones resolved against where the launch was made (which a resident
/// process does not share).
fn target_path(target: &Target, cwd: &Path) -> PathBuf {
    let path = match target {
        Target::Address(text) => return PathBuf::from(text),
        Target::Uri(text) | Target::Path(text) => fs::places::normalize(chooser::from_uri(text).unwrap_or_else(|| PathBuf::from(text))),
    };
    if fs::places::is_virtual(&path) { path } else { cwd.join(path) }
}

/// Open a window for one launch and run it until it closes.
fn open_window(mut request: instance::Request) -> eframe::Result {
    let args: Vec<OsString> = std::mem::take(&mut request.args).into_iter().map(OsString::from).collect();
    // A resident process reads what it was sent the same way: a request that makes no sense is told on stderr and
    // gets a plain window, not none, for the launch that handed it over is waiting for one
    let (chooser, targets) = match parse_launch(&args) {
        Launch::Chooser(words) => match chooser::Spec::parse(&words) {
            Some(spec) => (Some(spec), Vec::new()),
            None => {
                eprintln!("compression: the file dialog request could not be read");
                std::process::exit(chooser::EXIT_FAILED);
            }
        },
        Launch::Open(targets) => (None, targets),
        Launch::Error(why) => {
            eprintln!("compression: {why}");
            (None, Vec::new())
        }
        Launch::Info(_) | Launch::Portal | Launch::Resident => (None, Vec::new()),
    };

    let settings = settings::Settings::load();
    i18n::set_language(i18n::resolve(&settings.language));
    ui::theme::reload_user_themes();
    let theme = SerpantinumTheme::from_settings(&settings);

    // A file given instead of a folder (a browser's "Show in folder") opens its folder with the file selected
    let mut select = None;
    // Tabs of the last session, which only a normal launch without a folder argument brings back
    let mut session = None;
    let (first, windows, tabs) = place(&targets);
    // A server address opens the usual start folder and connects; the server's folder follows once it is reachable
    let (given, server) = match first {
        Some(Target::Address(address)) => (None, Some(address.clone())),
        Some(target) => (Some(target.clone()), None),
        None => (None, None),
    };
    // Several items dropped on the launcher at once: the first fills this window, each of the next (up to
    // `MAX_WINDOWS`) gets its own, and the others are tabs of this one
    for target in windows {
        // After `--`, so that a name that looks like an option is still a name there
        let word = match target {
            Target::Path(w) | Target::Uri(w) | Target::Address(w) => w,
        };
        let _ = std::env::current_exe().and_then(|exe| instance::spawn_reaped(std::process::Command::new(exe).arg("--").arg(word).current_dir(&request.cwd)));
    }
    let more_tabs: Vec<PathBuf> = tabs.iter().map(|t| target_path(t, &request.cwd)).collect();
    // Tabs to keep beside a folder given on the command line, so that launch does not replace the saved session
    let mut kept_session = None;
    let initial_path = if let Some(arg) = given {
        if chooser.is_none() && settings.start_location == settings::StartLocation::RestoreTabs {
            kept_session = Some(settings.restorable_session().0);
        }
        let path = target_path(&arg, &request.cwd);
        match path.parent() {
            Some(parent) if path.exists() && !path.is_dir() => {
                select = Some(path.clone());
                parent.to_path_buf()
            }
            _ => path,
        }
    } else if let Some(folder) = chooser.as_ref().and_then(|c| c.folder.clone().or_else(|| chooser::remembered_folder(c.app_id.as_deref()))).filter(|p| p.is_dir()) {
        folder
    } else {
        match settings.start_location {
            settings::StartLocation::Launch => request.cwd.clone(),
            settings::StartLocation::Home => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            settings::StartLocation::LastVisited => settings
                .last_dir
                .clone()
                .filter(|p| p.is_dir())
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))),
            settings::StartLocation::RestoreTabs => {
                let (paths, active) = settings.restorable_session();
                match paths.first().cloned() {
                    Some(first) => {
                        session = Some((paths, active));
                        first
                    }
                    None => request.cwd.clone(),
                }
            }
            settings::StartLocation::Custom => settings
                .start_path
                .clone()
                .filter(|p| p.is_dir())
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))),
        }
    };

    let mut viewport = app::chrome::viewport(chooser.as_ref().and_then(|c| c.title.clone()).unwrap_or_else(|| if fs::admin::is_admin() { format!("Compression ({})", i18n::tr("Administrator")) } else { "Compression".to_owned() }))
        .with_inner_size([1080.0, 720.0])
        .with_min_inner_size([360.0, 420.0]);
    if let Some(spec) = &chooser {
        viewport = viewport.with_inner_size(chooser::saved_size().unwrap_or(chooser::DEFAULT_SIZE)).with_app_id("compression-chooser");
        // Tied to its requester (see wayland_parent.rs) it is floated and centered as a dialog and can
        // stay resizable; without a parent, a fixed size is what makes tiling compositors float it
        if !spec.parent.as_deref().is_some_and(|p| p.starts_with("wayland:") || p.starts_with("x11:")) {
            viewport = viewport.with_resizable(false);
        }
    }
    let native_options = eframe::NativeOptions {
        viewport,
        // Honoured on X11 and Windows; Wayland leaves the position to the compositor
        centered: chooser.is_some(),
        // Starts several times faster than wgpu, which has to bring up Vulkan first
        renderer: eframe::Renderer::Glow,
        // egui-winit would answer Ctrl+V from its own clipboard, which is left out of the build (see
        // wayland_dnd.rs), so it pasted nothing; `CompressionApp::raw_input_hook` turns the shortcuts into
        // clipboard events with the system's text instead, in every window
        clipboard_shortcuts: false,
        ..Default::default()
    };

    eframe::run_native(
        "compression-file-manager",
        native_options,
        Box::new(move |cc| {
            ui::fonts::install(&cc.egui_ctx, settings.font.as_deref());
            ui::theme::apply(&cc.egui_ctx, &theme, &theme);
            cc.egui_ctx.set_zoom_factor(settings.scale);

            let mut app = CompressionApp::new(initial_path, theme, settings, &cc.egui_ctx);
            app.tell_about_theme_problems();
            if fs::admin::is_admin() {
                app.set_toast(i18n::tr("Running as administrator. Changes to settings are not saved."));
            }
            if let Some((paths, active)) = session.filter(|_| chooser.is_none()) {
                app.restore_tabs(&paths, active);
            }
            if let Some(paths) = kept_session.filter(|_| chooser.is_none()) {
                app.add_session_tabs(&paths);
            }
            if !more_tabs.is_empty() && chooser.is_none() {
                app.open_launch_tabs(&more_tabs);
            }
            if let Some(path) = select {
                app.select_path(&path);
            }
            if let Some(address) = server.filter(|_| chooser.is_none()) {
                app.navigate_to(PathBuf::from(address));
            }
            app.chooser = chooser.map(|spec| {
                if let Some(handle) = spec.parent.as_deref() {
                    x11_parent::attach(cc, handle);
                }
                let parent = spec.parent.as_deref().and_then(|handle| wayland_parent::attach(cc, handle));
                // A dialog opens the way the last one was left, whatever the file manager itself uses
                app.view_mode = app.settings.chooser_view_mode;
                let mut chooser = chooser::Chooser::new(spec);
                chooser.parent = parent;
                chooser
            });
            // The folder was listed before the chooser existed, and the chooser's filter applies to it
            if app.chooser.is_some() {
                app.reload_items();
            }
            // The file dialog takes no drags, but its text clipboard goes through this too
            app.wl_dnd = wayland_dnd::WaylandDnd::new(cc, &cc.egui_ctx);
            request.shown();
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn version_and_help_are_answered_only_for_the_exact_first_argument() {
        let version = format!("compression {}\n", env!("CARGO_PKG_VERSION"));
        assert_eq!(info_for(Some("--version")), Some(version.clone()));
        assert_eq!(info_for(Some("-V")), Some(version));
        let help = info_for(Some("--help")).unwrap();
        assert_eq!(info_for(Some("-h")), Some(help.clone()));
        for mode in ["--chooser", "--portal", "--resident", "--version", "--help"] {
            assert!(help.contains(mode), "the help does not mention {mode}");
        }
        // Folders and everything else are not information requests
        for other in ["./--version", "./--help", "--versions", "-v", "-H", "--Version", "", "/tmp", "--chooser", "sftp://h/"] {
            assert_eq!(info_for(Some(other)), None, "{other:?}");
        }
        assert_eq!(info_for(None), None);
    }

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn path(text: &str) -> Target {
        Target::Path(text.to_owned())
    }

    fn error_of(launch: Launch) -> String {
        match launch {
            Launch::Error(why) => why,
            other => panic!("not an error: {other:?}"),
        }
    }

    #[test]
    fn the_information_requests_count_only_as_the_first_argument() {
        assert!(matches!(parse_launch(&os(&["--version"])), Launch::Info(t) if t.starts_with("compression ")));
        assert!(matches!(parse_launch(&os(&["-V"])), Launch::Info(_)));
        assert!(matches!(parse_launch(&os(&["--help"])), Launch::Info(t) if t.contains("Usage:")));
        // The first one answers, whatever follows
        assert!(matches!(parse_launch(&os(&["-h", "/tmp"])), Launch::Info(_)));
        // Further on they are unknown options, and a folder of that name has to say so
        for words in [&["/tmp", "--version"][..], &["/tmp", "-h"], &["--portal", "--help"]] {
            assert!(error_of(parse_launch(&os(words))).contains("unknown option") || words[0] == "--portal", "{words:?}");
        }
        assert_eq!(parse_launch(&os(&["./--version"])), Launch::Open(vec![path("./--version")]));
        assert_eq!(parse_launch(&os(&["--", "--version"])), Launch::Open(vec![path("--version")]));
    }

    #[test]
    fn the_services_run_alone_and_with_nothing_else() {
        assert_eq!(parse_launch(&os(&["--portal"])), Launch::Portal);
        assert_eq!(parse_launch(&os(&["--resident"])), Launch::Resident);
        for words in [&["--portal", "/tmp"][..], &["/tmp", "--portal"], &["--resident", "--"], &["--resident", "x"], &["--portal", "--resident"], &["--portal", "--portal"]] {
            assert!(error_of(parse_launch(&os(words))).contains("takes no other arguments"), "{words:?}");
        }
        // Not alone with a dialog request either
        let why = error_of(parse_launch(&os(&["--portal", "--chooser"])));
        assert!(why.contains("file dialog request"), "{why}");
        assert!(matches!(parse_launch(&os(&["--chooser", "--mode", "open", "--resident"])), Launch::Error(_)));
        // After `--` a name is a name
        assert_eq!(parse_launch(&os(&["--", "--portal"])), Launch::Open(vec![path("--portal")]));
    }

    #[test]
    fn a_file_dialog_request_is_read_by_its_own_parser_and_takes_no_targets() {
        let request = ["--chooser", "--mode", "save", "--name", "a.txt", "--multiple", "--title", "Save", "--filter", "Text\tg:*.txt"];
        assert_eq!(parse_launch(&os(&request)), Launch::Chooser(request.iter().map(|w| w.to_string()).collect()));
        // Anywhere among the words
        assert!(matches!(parse_launch(&os(&["--mode", "open", "--chooser"])), Launch::Chooser(_)));
        // One that cannot be read
        assert!(error_of(parse_launch(&os(&["--chooser", "--mode", "nonsense"]))).contains("could not be read"));
        assert!(error_of(parse_launch(&os(&["--chooser", "--mode"]))).contains("could not be read"));
        assert!(error_of(parse_launch(&os(&["--chooser", "--choice", "broken"]))).contains("could not be read"));
        // A folder, a stray word, an unknown option or `--` is not for a dialog
        for words in [&["--chooser", "--mode", "open", "/tmp"][..], &["--chooser", "stray"], &["--chooser", "--bogus"], &["--chooser", "--", "x"], &["--chooser", "-x"]] {
            assert!(error_of(parse_launch(&os(words))).contains("takes no other argument"), "{words:?}");
        }
        // A value may look like anything
        assert!(matches!(parse_launch(&os(&["--chooser", "--title", "--chooser", "--name", "-x"])), Launch::Chooser(_)));
        // Behind `--` the word is a name
        assert_eq!(parse_launch(&os(&["--", "--chooser"])), Launch::Open(vec![path("--chooser")]));
    }

    #[test]
    fn two_dashes_end_the_options_and_any_other_option_is_refused() {
        assert_eq!(parse_launch(&os(&["--", "-x", "--x", "--help", "--"])), Launch::Open(vec![path("-x"), path("--x"), path("--help"), path("--")]));
        assert_eq!(parse_launch(&os(&["a", "--", "-b"])), Launch::Open(vec![path("a"), path("-b")]));
        for words in [&["-x"][..], &["--foo"], &["-"], &["/tmp", "--foo"], &["--foo", "--", "a"], &["-rf", "a"], &["--chooser2"]] {
            let why = error_of(parse_launch(&os(words)));
            assert!(why.contains("unknown option"), "{words:?}: {why}");
        }
        // The hint says what to do about a folder with such a name
        assert!(error_of(parse_launch(&os(&["-x"]))).contains("./-x"));
        assert_eq!(parse_launch(&os(&[])), Launch::Open(Vec::new()));
        assert_eq!(parse_launch(&os(&["--"])), Launch::Open(Vec::new()));
    }

    #[test]
    fn an_empty_argument_is_refused_before_and_after_the_dashes() {
        for words in [&[""][..], &["a", ""], &["--", ""], &["", "--"]] {
            assert!(error_of(parse_launch(&os(words))).contains("is empty"), "{words:?}");
        }
    }

    #[test]
    fn targets_are_paths_uris_of_this_computer_or_server_addresses() {
        assert_eq!(
            parse_launch(&os(&["a", "/b/c", "file:///tmp/x%20y", "file:/tmp/z", "file://localhost/tmp/w", "sftp://me@host/srv", "file:relative"])),
            Launch::Open(vec![
                path("a"),
                path("/b/c"),
                Target::Uri("file:///tmp/x%20y".into()),
                Target::Uri("file:/tmp/z".into()),
                Target::Uri("file://localhost/tmp/w".into()),
                Target::Address("sftp://me@host/srv".into()),
                path("file:relative"),
            ])
        );
        // A file of another computer is not one to open here
        for uri in ["file://otherhost/tmp/x", "file://example.com/"] {
            let why = error_of(parse_launch(&os(&[uri])));
            assert!(why.contains("not a file of this computer"), "{uri}: {why}");
        }
        assert!(matches!(parse_launch(&os(&["a", "file://otherhost/x"])), Launch::Error(_)));
    }

    #[test]
    fn a_name_that_is_not_utf8_is_refused_with_its_place_and_reason() {
        let bad = OsString::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9"));
        let why = error_of(parse_launch(std::slice::from_ref(&bad)));
        assert!(why.contains("argument 1") && why.contains("not valid UTF-8") && why.contains("caf"), "{why}");
        let why = error_of(parse_launch(&[OsString::from("ok"), OsString::from("--"), bad.clone()]));
        assert!(why.contains("argument 3"), "{why}");
        // Not turned into another name, in a dialog request or after the dashes either
        assert!(matches!(parse_launch(&[OsString::from("--chooser"), OsString::from("--title"), bad.clone()]), Launch::Error(_)));
        // The information requests are answered before anything else is looked at
        assert!(matches!(parse_launch(&[OsString::from("--version"), bad]), Launch::Info(_)));
    }

    #[test]
    fn a_launch_takes_at_most_fifty_targets_and_opens_at_most_eight_windows() {
        let many = |n: usize| -> Vec<OsString> { (0..n).map(|i| OsString::from(format!("/t/{i}"))).collect() };
        assert!(matches!(parse_launch(&many(MAX_TARGETS)), Launch::Open(t) if t.len() == 50));
        assert_eq!(error_of(parse_launch(&many(MAX_TARGETS + 1))), "too many targets (51, at most 50)");
        assert_eq!(error_of(parse_launch(&many(5000))), "too many targets (5000, at most 50)");

        let targets = |n: usize| -> Vec<Target> { (0..n).map(|i| path(&format!("/t/{i}"))).collect() };
        let shape = |n: usize| {
            let list = targets(n);
            let (first, windows, tabs) = place(&list);
            (first.is_some(), windows.len(), tabs.len())
        };
        assert_eq!(shape(0), (false, 0, 0));
        assert_eq!(shape(1), (true, 0, 0));
        assert_eq!(shape(2), (true, 1, 0));
        assert_eq!(shape(8), (true, 7, 0), "eight windows in all: this one and seven more");
        assert_eq!(shape(9), (true, 7, 1));
        assert_eq!(shape(50), (true, 7, 42));
        // Every target is somewhere, once and in order
        let list = targets(20);
        let (first, windows, tabs) = place(&list);
        let all: Vec<&Target> = first.into_iter().chain(windows).chain(tabs).collect();
        assert_eq!(all, list.iter().collect::<Vec<_>>());
    }

    #[test]
    fn the_environment_of_an_elevated_start_comes_off_the_front_and_the_rest_is_kept_as_it_was() {
        let bad = OsString::from(std::ffi::OsStr::from_bytes(b"/srv/\xff"));
        let raw = vec![OsString::from("--admin-environment"), OsString::from("LANG=C"), OsString::from("--"), bad.clone()];
        assert_eq!(without_environment(raw), vec![bad.clone()]);
        // Without it nothing is taken, not a word of it
        let plain = vec![OsString::from("/tmp"), bad];
        assert_eq!(without_environment(plain.clone()), plain);
        assert_eq!(without_environment(Vec::new()), Vec::<OsString>::new());
    }
}
