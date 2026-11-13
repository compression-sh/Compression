//! Open a terminal in a folder. Terminal emulators disagree on how to be told where to start, so the
//! common ones are listed with their flag; anything else starts in the working directory of the
//! process we launch, which most terminals use.
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// Name, and how its "start in this folder" option is spelled (`{}` is the folder; empty means
/// the terminal starts in the working directory of the process).
const TERMINALS: &[(&str, &[&str])] = &[
    ("ptyxis", &["--working-directory={}", "--new-window"]),
    ("kgx", &["--working-directory={}"]),
    ("gnome-terminal", &["--working-directory={}"]),
    ("konsole", &["--workdir", "{}"]),
    ("xfce4-terminal", &["--working-directory={}"]),
    ("kitty", &["--directory", "{}"]),
    ("alacritty", &["--working-directory", "{}"]),
    ("foot", &["--working-directory={}"]),
    ("wezterm", &["start", "--cwd", "{}"]),
    ("tilix", &["--working-directory={}"]),
    ("terminator", &["--working-directory={}"]),
    ("lxterminal", &["--working-directory={}"]),
    ("mate-terminal", &["--working-directory={}"]),
    ("urxvt", &[]),
    ("xterm", &[]),
    ("st", &[]),
    ("x-terminal-emulator", &[]),
];

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// The terminal that would be started, for the settings hint.
pub fn detect(custom: &str) -> Option<String> {
    if let Some(program) = custom.split_whitespace().next() {
        return Some(program.to_owned());
    }
    if let Ok(env) = std::env::var("TERMINAL")
        && let Some(program) = env.split_whitespace().next()
        && on_path(program)
    {
        return Some(program.to_owned());
    }
    TERMINALS.iter().map(|(name, _)| *name).find(|name| on_path(name)).map(str::to_owned)
}

/// The terminal command of the settings, unless this runs as root: a command the person wrote
/// into their settings must not run with root's rights, so the terminal is found by itself.
fn trusted(custom: &str, admin: bool) -> &str {
    if admin { "" } else { custom }
}

fn fill(arg: &str, dir: &Path) -> String {
    arg.replace("{}", &dir.to_string_lossy())
}

/// Start a terminal in `dir`. `custom` is the command from the settings: `{}` stands for the
/// folder, and without it the terminal is started inside the folder.
pub fn open_in(dir: &Path, custom: &str) -> io::Result<()> {
    let custom = trusted(custom, crate::fs::admin::is_admin());
    let mut parts: Vec<String> = custom.split_whitespace().map(|p| fill(p, dir)).collect();
    if parts.is_empty() {
        let env = std::env::var("TERMINAL").ok().filter(|t| t.split_whitespace().next().is_some_and(on_path));
        let known = |program: &str| TERMINALS.iter().find(|(name, _)| *name == program).map(|(_, args)| *args);
        if let Some(env) = env {
            parts = env.split_whitespace().map(str::to_owned).collect();
            if let Some(args) = parts.first().and_then(|p| known(p)) {
                parts.extend(args.iter().map(|a| fill(a, dir)));
            }
        } else if let Some((name, args)) = TERMINALS.iter().find(|(name, _)| on_path(name)) {
            parts = std::iter::once((*name).to_owned()).chain(args.iter().map(|a| fill(a, dir))).collect();
        }
    }
    let Some((program, args)) = parts.split_first() else {
        return Err(io::Error::other(crate::i18n::tr("No terminal found. Set one in Settings → Behavior")));
    };
    start(Command::new(program).args(args).current_dir(dir)).map(|_| ())
}

/// Starts a terminal with no input or output of ours, and collects its exit when the person closes
/// it: a terminal nobody waits for stays a zombie for as long as this process runs. Does not
/// wait for the terminal. Returns the terminal's process id.
fn start(command: &mut Command) -> io::Result<u32> {
    crate::instance::spawn_reaped(command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()))
}

/// How each terminal is told to run a command instead of a shell: the arguments before the command.
const RUNS: &[(&str, &[&str])] = &[
    ("ptyxis", &["--new-window", "--"]),
    ("kgx", &["--"]),
    ("gnome-terminal", &["--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("kitty", &[]),
    ("alacritty", &["-e"]),
    ("foot", &[]),
    ("wezterm", &["start", "--"]),
    // Tilix takes the command as one line it splits (g_shell_parse_argv), see `tilix_line`
    ("tilix", &["-e"]),
    ("terminator", &["-x"]),
    ("mate-terminal", &["-x"]),
    ("urxvt", &["-e"]),
    ("xterm", &["-e"]),
    ("st", &["-e"]),
    ("x-terminal-emulator", &["-e"]),
];

/// Start a terminal that runs `command`: the one of the settings, of `$TERMINAL`, or the first one
/// installed. `None` when that terminal is not known to take a command. Each word of `command` reaches the program as
/// the argument it is, byte for byte: the terminals listed take the words that follow as the command, none of them is
/// handed a line for a shell.
pub fn run<S: AsRef<std::ffi::OsStr>>(command: &[S], custom: &str) -> Option<io::Result<()>> {
    let custom = trusted(custom, crate::fs::admin::is_admin());
    let env = std::env::var("TERMINAL").ok().and_then(|t| t.split_whitespace().next().map(str::to_owned)).filter(|t| on_path(t));
    let chosen = custom
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .or(env)
        .or_else(|| RUNS.iter().map(|(name, _)| *name).find(|name| on_path(name)).map(str::to_owned))?;
    let name = Path::new(&chosen).file_name()?.to_string_lossy().into_owned();
    let before = runs_with(&name)?;
    // Refused before it is started: the terminal would fail by itself, unseen, or be handed another name
    if TEXT_ONLY.contains(&name.as_str()) && command.iter().any(|word| word.as_ref().to_str().is_none()) {
        return Some(Err(io::Error::other(crate::i18n::trf("{} can only be handed names that are valid text (UTF-8), and one of these is not. Choose another terminal in Settings → Behavior.", &[&name]))));
    }
    if name == "tilix" {
        return Some(start(Command::new(&chosen).args(before).arg(tilix_line(command))).map(|_| ()));
    }
    Some(start(Command::new(&chosen).args(before).args(command)).map(|_| ()))
}

/// Terminals that take their arguments only as UTF-8 text. kitty (0.50) stops with a UnicodeDecodeError before it runs
/// anything, and its window never comes; Tilix's `-e` is a text option (GOptionArg.STRING).
const TEXT_ONLY: &[&str] = &["kitty", "tilix"];

/// The one line Tilix is given after `-e`: each word in single quotes (a quote in it as `'\''`). Tilix takes a single
/// argument after `-e` as it is (several it joins, quoting only those with a space) and splits it with GLib's
/// `g_shell_parse_argv`, which turns this back into exactly these words; nothing in it is run by a shell. Words that are
/// not UTF-8 never come here (see `TEXT_ONLY`).
fn tilix_line<S: AsRef<std::ffi::OsStr>>(command: &[S]) -> String {
    command.iter().map(|word| format!("'{}'", word.as_ref().to_string_lossy().replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ")
}

/// The arguments that tell the terminal `name` to run the command that follows them; `None` for one not known to.
fn runs_with(name: &str) -> Option<&'static [&'static str]> {
    RUNS.iter().find(|(known, _)| *known == name).map(|(_, before)| *before)
}

/// The ssh command that logs in to `host` and opens a shell in `dir` there.
pub fn ssh_command(user: Option<&str>, host: &str, port: Option<u16>, dir: &str) -> Vec<String> {
    let mut command = vec!["ssh".to_owned(), "-t".to_owned()];
    if let Some(port) = port {
        command.extend(["-p".to_owned(), port.to_string()]);
    }
    command.push(match user {
        Some(user) => format!("{user}@{host}"),
        None => host.to_owned(),
    });
    let quoted = format!("'{}'", dir.replace('\'', "'\\''"));
    command.push(format!("cd {quoted} 2>/dev/null; exec \"$SHELL\" -l"));
    command
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Terminals that run a command, and how: each takes the words after these as the command.
    const CONVENTIONS: &[(&str, &[&str])] = &[
        ("ptyxis", &["--new-window", "--"]),
        ("kgx", &["--"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("xfce4-terminal", &["-x"]),
        ("kitty", &[]),
        ("alacritty", &["-e"]),
        ("foot", &[]),
        ("wezterm", &["start", "--"]),
        ("terminator", &["-x"]),
        ("mate-terminal", &["-x"]),
        ("urxvt", &["-e"]),
        ("xterm", &["-e"]),
        ("st", &["-e"]),
        ("x-terminal-emulator", &["-e"]),
    ];

    #[test]
    fn each_terminal_is_told_to_run_a_command_its_own_way_and_one_that_takes_a_line_of_text_is_not_used() {
        for (name, before) in CONVENTIONS {
            assert_eq!(runs_with(name), Some(*before), "{name}");
        }
        // Tilix is told with -e and one line of its own (tested apart)
        assert_eq!(runs_with("tilix"), Some(&["-e"][..]));
        assert_eq!(RUNS.len(), CONVENTIONS.len() + 1, "a terminal without a tested convention");
        // lxterminal takes the command as one line it splits on spaces; an unknown terminal is not guessed at
        for name in ["lxterminal", "my-own-terminal"] {
            assert_eq!(runs_with(name), None, "{name}");
        }
    }

    /// A stand-in for the terminal `name` in `dir`: it writes down the arguments it was started with, each ended by NUL,
    /// to `<name>.argv`.
    fn recording_terminal(dir: &Path, name: &str) -> std::path::PathBuf {
        install_script(&dir.join(name), "printf '%s\\0' \"$@\" > \"$0.argv.part\" && mv \"$0.argv.part\" \"$0.argv\"")
    }

    /// A shell script at `path`, put there by `cp`: a file this process wrote itself may still be open for writing in
    /// a child another test is just starting, and then cannot be run (ETXTBSY) until that child has started.
    pub(crate) fn install_script(path: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let draft = path.with_extension("draft");
        std::fs::write(&draft, format!("#!/bin/sh\n{body}\n")).unwrap();
        assert!(Command::new("cp").arg(&draft).arg(path).status().unwrap().success());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_path_buf()
    }

    fn wait_for(file: &Path) -> Vec<u8> {
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !file.exists() {
            assert!(std::time::Instant::now() < limit, "{} was not started", file.display());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::read(file).unwrap()
    }

    /// The words GLib's `g_shell_parse_argv` (the splitting Tilix does) makes of `line`, through Python's GLib binding;
    /// `None` where that is not installed.
    fn glib_split(line: &str) -> Option<Vec<String>> {
        use std::io::Write;
        let script = "import sys, gi\ngi.require_version('GLib', '2.0')\nfrom gi.repository import GLib\nok, argv = GLib.shell_parse_argv(sys.stdin.read())\nsys.stdout.write('\\0'.join(argv))";
        let mut child = Command::new("python3").args(["-I", "-c", script]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        child.stdin.take()?.write_all(line.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        out.status.success().then(|| String::from_utf8(out.stdout).unwrap().split('\0').map(str::to_owned).collect())
    }

    #[test]
    fn tilix_is_given_one_line_that_glib_splits_back_into_exactly_the_words_and_no_name_that_is_not_text() {
        let dir = std::env::temp_dir().join(format!("compression_terminal_tilix_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tilix = recording_terminal(&dir, "tilix");
        let open_with: Vec<String> = ["/usr/bin/vim", "+set nu", "", " lead space", "it's", "say \"hi\"", "a\\b", "end\\", "line1\nline2", "tab\there", "-rf", "$(rm -rf ~)", "`id`", "a;b|c&d>e", "*?[x] ~ #x", "ünï €"].map(str::to_owned).to_vec();
        let ssh = ssh_command(Some("me"), "host.example", Some(2222), "/srv/it's \"here\"/a b");
        for command in [open_with, ssh] {
            let _ = std::fs::remove_file(dir.join("tilix.argv"));
            run(&command, &tilix.to_string_lossy()).expect("tilix was not taken").unwrap();
            let got = wait_for(&dir.join("tilix.argv"));
            let args: Vec<&[u8]> = got.strip_suffix(&[0]).unwrap().split(|b| *b == 0).collect();
            // One argument after -e: the one Tilix takes as it is, without joining words itself
            assert_eq!(args.len(), 2, "{args:?}");
            assert_eq!(args[0], b"-e");
            let line = std::str::from_utf8(args[1]).unwrap();
            assert_eq!(line, tilix_line(&command));
            match glib_split(line) {
                Some(words) => assert_eq!(words, command, "GLib splits the line into other words"),
                None => eprintln!("SKIPPED: no Python GLib binding, the line was not split by GLib here"),
            }
        }
        // A name that is not text: said, and Tilix is not started
        let _ = std::fs::remove_file(dir.join("tilix.argv"));
        let odd = [std::ffi::OsString::from("vim"), std::os::unix::ffi::OsStringExt::from_vec(b"/x/\xe4.txt".to_vec())];
        let err = run(&odd, &tilix.to_string_lossy()).expect("tilix was not taken").unwrap_err();
        assert!(err.to_string().starts_with("tilix can only be handed names that are valid text"), "{err}");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!dir.join("tilix.argv").exists(), "Tilix was started with a name it cannot take");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn kitty_gets_text_as_it_is_and_a_name_that_is_not_text_is_refused_before_it_is_started() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let dir = std::env::temp_dir().join(format!("compression_terminal_kitty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let kitty = recording_terminal(&dir, "kitty");
        // Valid text, however odd: handed over word for word, kitty takes no flags before the command
        let text = [std::ffi::OsString::from("/usr/bin/vim"), std::ffi::OsString::from("/home/me/-r \"x\" 'y'\n$(id) ü.txt")];
        run(&text, &kitty.to_string_lossy()).expect("kitty was not taken").unwrap();
        let mut expected = Vec::new();
        for word in &text {
            expected.extend_from_slice(word.as_bytes());
            expected.push(0);
        }
        assert_eq!(wait_for(&dir.join("kitty.argv")), expected);
        // Bytes that are not UTF-8, alone or in the middle of text: refused, and kitty is not started
        for bad in [b"/x/caf\xe9.txt".to_vec(), b"\xff".to_vec(), b"/x/\xc3(broken\xc3".to_vec()] {
            let _ = std::fs::remove_file(dir.join("kitty.argv"));
            let command = [std::ffi::OsString::from("/usr/bin/vim"), std::ffi::OsString::from_vec(bad)];
            let err = run(&command, &kitty.to_string_lossy()).expect("kitty was not taken").unwrap_err();
            assert_eq!(err.to_string(), "kitty can only be handed names that are valid text (UTF-8), and one of these is not. Choose another terminal in Settings → Behavior.");
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(!dir.join("kitty.argv").exists(), "kitty was started with a name it cannot take");
        }
        // Another terminal takes such a name as it is
        let xterm = recording_terminal(&dir, "xterm");
        let command = [std::ffi::OsString::from("/usr/bin/vim"), std::ffi::OsString::from_vec(b"/x/caf\xe9.txt".to_vec())];
        run(&command, &xterm.to_string_lossy()).expect("xterm was not taken").unwrap();
        assert_eq!(wait_for(&dir.join("xterm.argv")), b"-e\0/usr/bin/vim\0/x/caf\xe9.txt\0");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_terminal_is_really_started_with_the_command_word_for_word_and_byte_for_byte() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let dir = std::env::temp_dir().join(format!("compression_terminal_runs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A program and a file whose name is not UTF-8 and has spaces, a line break, quotes and a leading dash
        let file = std::ffi::OsString::from_vec(b"/home/me/-r \"x\" 'y'\n$(z) \xe4.txt".to_vec());
        let command = [std::ffi::OsString::from("/usr/bin/vim"), std::ffi::OsString::from("+set nu"), file];
        for (name, before) in CONVENTIONS {
            let terminal = recording_terminal(&dir, name);
            let started = run(&command, &terminal.to_string_lossy()).unwrap_or_else(|| panic!("{name} was not taken"));
            // A terminal that takes only text refuses this name before it is started (see the kitty test)
            if TEXT_ONLY.contains(name) {
                assert!(started.is_err(), "{name} was handed a name that is not text");
                std::thread::sleep(std::time::Duration::from_millis(100));
                assert!(!dir.join(format!("{name}.argv")).exists(), "{name} was started");
                continue;
            }
            started.unwrap();
            let mut expected = Vec::new();
            for word in before.iter().map(std::ffi::OsStr::new).chain(command.iter().map(|w| w.as_os_str())) {
                expected.extend_from_slice(word.as_bytes());
                expected.push(0);
            }
            assert_eq!(wait_for(&dir.join(format!("{name}.argv"))), expected, "{name}");
        }
        // A terminal not known to run a command is not started at all
        let unknown = recording_terminal(&dir, "lxterminal");
        assert!(run(&command, &unknown.to_string_lossy()).is_none());
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!dir.join("lxterminal.argv").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_terminal_that_was_closed_is_collected_and_starting_it_does_not_wait() {
        let started = std::time::Instant::now();
        // A program that runs for a while stands in for the terminal
        let id = start(Command::new("sleep").arg("0.4")).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(300), "starting waited for the terminal");
        assert!(Path::new(&format!("/proc/{id}")).exists(), "still running");
        let waited = std::time::Instant::now();
        while Path::new(&format!("/proc/{id}")).exists() && waited.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!Path::new(&format!("/proc/{id}")).exists(), "the terminal was left as a zombie");
        assert!(start(&mut Command::new("/nonexistent/compression-test-terminal")).is_err());
    }

    #[test]
    fn folder_placeholder_is_filled() {
        assert_eq!(fill("--working-directory={}", Path::new("/a b")), "--working-directory=/a b");
        assert_eq!(detect("mycterm --flag").as_deref(), Some("mycterm"));
    }

    #[test]
    fn root_does_not_use_the_terminal_command_of_the_settings() {
        assert_eq!(trusted("sh -c evil {}", true), "");
        assert_eq!(trusted("kitty --directory {}", false), "kitty --directory {}");
    }

    #[test]
    fn ssh_opens_the_folder_on_the_server() {
        let command = ssh_command(Some("me"), "example.com", Some(2222), "/srv/it's here");
        assert_eq!(command[..5], ["ssh", "-t", "-p", "2222", "me@example.com"]);
        assert_eq!(command[5], "cd '/srv/it'\\''s here' 2>/dev/null; exec \"$SHELL\" -l");
    }
}
