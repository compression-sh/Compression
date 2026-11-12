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
use std::path::PathBuf;
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
  compression [FOLDER | ADDRESS]   open a folder (default: the current one) or a server address
                                   such as sftp://me@example.com/srv
  compression --chooser --mode open|folder|save|save-files
                                   the file dialog for one request: prints the chosen file:// URIs
                                   and exits with 0 (chosen), 1 (cancelled) or 2 (failed)
  compression --portal             the xdg-desktop-portal service (FileChooser, FileManager1)
  compression --resident           keep a background instance, so new windows open at once
  compression -V, --version        print the version and exit
  compression -h, --help           print this help and exit

A folder whose name starts with a dash is given as ./-name.
",
            env!("CARGO_PKG_VERSION")
        )),
        _ => None,
    }
}

/// The arguments of this run. A name that is not valid UTF-8 (Linux allows any bytes in one) is read
/// with replacement characters: `env::args` would panic on it, and a program that is handed such a
/// file by another program must not end with a panic.
fn arguments() -> Vec<String> {
    env::args_os().skip(1).map(|arg| arg.to_string_lossy().into_owned()).collect()
}

fn main() -> eframe::Result {
    // Answered before anything else is touched: no migration, no instance, no display, no socket
    if let Some(text) = info_for(arguments().first().map(String::as_str)) {
        use std::io::Write;
        // A closed pipe (`compression --help | head -1`) is no reason to panic
        let _ = std::io::stdout().write_all(text.as_bytes());
        return Ok(());
    }
    // An elevated start carries its environment in the arguments (see fs::admin); first of all
    let args = fs::admin::take_environment(arguments());
    compat::migrate_user_data();
    if args.first().is_some_and(|a| a == "--portal") {
        if let Err(err) = portal::serve() {
            eprintln!("compression: {err}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.first().is_some_and(|a| a == "--resident") {
        instance::serve(open_window);
        return Ok(());
    }
    let request = instance::Request::current(args);
    // A file dialog belongs to its requester, and an administrator's window to its own process.
    // As a container's first process (the Docker image), leaving would stop the container.
    if !request.args.iter().any(|a| a == "--chooser") && !fs::admin::is_admin() && std::process::id() != 1 {
        if !settings::Settings::load().run_in_background {
            instance::dismiss(&request);
        } else if instance::hand_off(&request) {
            return Ok(());
        }
    }
    let dialog = request.args.iter().any(|a| a == "--chooser");
    let result = open_window(request);
    // A dialog that could not be opened must not end like a cancel, which is exit code 1
    if dialog && let Err(err) = &result {
        eprintln!("Error: {err}");
        std::process::exit(chooser::EXIT_FAILED);
    }
    result
}

/// Open a window for one launch and run it until it closes.
fn open_window(mut request: instance::Request) -> eframe::Result {
    let args = std::mem::take(&mut request.args);
    let chooser = if args.iter().any(|a| a == "--chooser") { chooser::Spec::parse(&args) } else { None };
    // A dialog request that cannot be read is an error for whoever asked, not a reason to open a file manager
    if chooser.is_none() && args.iter().any(|a| a == "--chooser") {
        eprintln!("compression: the file dialog request could not be read");
        std::process::exit(chooser::EXIT_FAILED);
    }

    let settings = settings::Settings::load();
    i18n::set_language(i18n::resolve(&settings.language));
    ui::theme::reload_user_themes();
    let theme = SerpantinumTheme::from_settings(&settings);

    // A file given instead of a folder (a browser's "Show in folder") opens its folder with the file selected
    let mut select = None;
    // Tabs of the last session, which only a normal launch without a folder argument brings back
    let mut session = None;
    let given = if chooser.is_some() { None } else { args.first().filter(|a| !a.starts_with("--")).filter(|a| !a.starts_with("file://") || chooser::from_uri(a).is_some()).cloned() };
    // A server address opens the usual start folder and connects; the server's folder follows once it is reachable
    let (given, server) = match given {
        Some(arg) if fs::network::Address::looks_like(&arg) && !arg.starts_with("file://") => (None, Some(arg)),
        other => (other, None),
    };
    // Several items dropped on the launcher at once: the first fills this window, each other gets its own
    if chooser.is_none() {
        for extra in args.iter().skip(1).filter(|a| !a.starts_with("--")) {
            let _ = std::env::current_exe().and_then(|exe| instance::spawn_reaped(std::process::Command::new(exe).arg(extra).current_dir(&request.cwd)));
        }
    }
    // Tabs to keep beside a folder given on the command line, so that launch does not replace the saved session
    let mut kept_session = None;
    let initial_path = if let Some(arg) = given {
        if chooser.is_none() && settings.start_location == settings::StartLocation::RestoreTabs {
            kept_session = Some(settings.restorable_session().0);
        }
        // A launcher passes what was dropped on it as `file://` URIs
        let path = fs::places::normalize(chooser::from_uri(&arg).unwrap_or_else(|| PathBuf::from(arg)));
        // Relative to where the launch was made, which a resident process does not share
        let path = if fs::places::is_virtual(&path) { path } else { request.cwd.join(path) };
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
    use super::info_for;

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
}
