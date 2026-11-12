//! The program as a process: what `compression` prints and the code it ends with, for the arguments that end
//! before any window, instance or service is touched.
//!
//! Each run has a home, config, data, state, cache and runtime folder of its own, empty, and no display, so that nothing
//! of the machine is read or changed, and each test checks that the run made nothing there either.
//!
//! No test here starts `--portal` (it claims a name on the session bus) or `--resident` (it stays running), nor any
//! launch that would open a window or hand over to a resident process.
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Sandbox(PathBuf);

const FOLDERS: [&str; 6] = ["home", "config", "data", "state", "cache", "runtime"];

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("compression_cli_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for folder in FOLDERS {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        // Data of the old name (Ski) is there: a run that gets as far as starting the program for real copies it
        // (`compat::migrate_user_data`), so a copy shows how far a run got
        std::fs::create_dir_all(root.join("config/ski")).unwrap();
        std::fs::write(root.join("config/ski/settings.json"), "{}").unwrap();
        Self(root)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_compression"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .stdin(Stdio::null());
        command
    }

    /// Runs the program with `args` and waits for it (at most 30 s: a run that does not end is a failure, not a hang).
    fn run(&self, args: &[OsString]) -> Output {
        let child = self.command().args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("the program starts");
        let pid = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(output) => return output.expect("the output"),
                Err(_) if Instant::now() > deadline => {
                    // SAFETY: kill takes a process id and a signal
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                    panic!("the program did not end in 30 s");
                }
                Err(_) => {}
            }
        }
    }

    /// Nothing was made in any of the folders: only the old data that was put there is there, as it was, and was not
    /// migrated.
    fn assert_untouched(&self) {
        self.assert_only(&["ski"]);
    }

    /// What is in the config folder is `config` (the old data, and what a start migrated it to), and nothing else was made.
    fn assert_only(&self, config: &[&str]) {
        for folder in FOLDERS {
            let mut left: Vec<String> = std::fs::read_dir(self.0.join(folder)).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            left.sort();
            let expected: Vec<&str> = if folder == "config" { config.to_vec() } else { Vec::new() };
            assert_eq!(left, expected, "the run changed {folder}");
        }
        assert_eq!(std::fs::read_to_string(self.0.join("config/ski/settings.json")).unwrap(), "{}");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Ended with `code` and a line on stderr that starts with `compression: ` and has `reason` in it, said nothing on
/// stdout, and left the sandbox as it was.
fn refused(sandbox: &Sandbox, args: &[OsString], reason: &str) {
    let out = sandbox.run(args);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
    assert!(stderr.starts_with("compression: ") && stderr.contains(reason), "{args:?}: {stderr:?}");
    assert!(stdout.is_empty(), "{args:?}: {stdout:?}");
    sandbox.assert_untouched();
}

#[test]
fn the_version_is_printed_with_exit_0_and_nothing_else_happens() {
    let sandbox = Sandbox::new("version");
    for args in [&["--version"][..], &["-V"], &["-V", "extra", "words"], &["--version", "--nonsense"]] {
        let out = sandbox.run(&os(args));
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert_eq!(text(&out.stdout), format!("compression {}\n", env!("CARGO_PKG_VERSION")), "{args:?}");
        assert!(out.stderr.is_empty(), "{args:?}: {:?}", text(&out.stderr));
        sandbox.assert_untouched();
    }
}

#[test]
fn the_help_is_printed_with_exit_0_and_names_every_way_to_start_the_program() {
    let sandbox = Sandbox::new("help");
    let long = sandbox.run(&os(&["--help"]));
    assert_eq!(long.status.code(), Some(0));
    assert!(long.stderr.is_empty());
    let help = text(&long.stdout);
    for part in ["Usage:", "--chooser", "--portal", "--resident", "--version", "--help", "--", "exit code 2"] {
        assert!(help.contains(part), "the help does not mention {part}");
    }
    // The short form says the same
    let short = sandbox.run(&os(&["-h", "more"]));
    assert_eq!((short.status.code(), short.stdout), (Some(0), long.stdout));
    sandbox.assert_untouched();
}

#[test]
fn a_reader_that_goes_away_before_the_end_of_the_help_is_no_reason_to_fail() {
    let sandbox = Sandbox::new("pipe");
    let mut child = sandbox.command().arg("--help").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // Like `compression --help | head -1`: one byte is read, the pipe is closed
    {
        use std::io::Read;
        let mut stdout = child.stdout.take().unwrap();
        let mut one = [0u8; 1];
        stdout.read_exact(&mut one).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
    sandbox.assert_untouched();
}

#[test]
fn an_option_that_is_not_known_ends_the_program_with_exit_2_and_says_so() {
    let sandbox = Sandbox::new("option");
    for args in [&["--foo"][..], &["-x"], &["-"], &["/tmp", "--foo"], &["--foo", "--", "a"]] {
        refused(&sandbox, &os(args), "unknown option");
    }
}

#[test]
fn the_information_options_count_only_as_the_first_argument() {
    let sandbox = Sandbox::new("second");
    for args in [&["/tmp", "--version"][..], &["/tmp", "-h"], &["a", "--help"], &["-x", "--version"]] {
        refused(&sandbox, &os(args), "unknown option");
    }
}

#[test]
fn the_services_take_no_other_arguments_and_the_refusal_comes_before_anything_is_started() {
    let sandbox = Sandbox::new("services");
    for args in [&["--portal", "x"][..], &["--resident", "x"], &["--portal", "--resident"], &["/tmp", "--portal"], &["--resident", "--"]] {
        refused(&sandbox, &os(args), "takes no other arguments");
    }
}

#[test]
fn an_empty_argument_is_refused() {
    let sandbox = Sandbox::new("empty");
    for args in [&[""][..], &["a", ""], &["--", ""]] {
        refused(&sandbox, &os(args), "is empty");
    }
}

#[test]
fn a_folder_whose_name_is_not_utf8_is_taken_as_it_is_and_only_the_missing_display_stops_it() {
    let sandbox = Sandbox::new("utf8");
    let folder = sandbox.0.join(OsString::from_vec(b"caf\xe9".to_vec()));
    std::fs::create_dir(&folder).unwrap();
    let out = sandbox.run(&[folder.into_os_string()]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("no Wayland or X11 display") && !stderr.contains("UTF-8"), "{stderr:?}");
}

#[test]
fn without_a_display_a_window_says_so_in_words_and_ends_with_exit_2() {
    let sandbox = Sandbox::new("nodisplay_window");
    let out = sandbox.run(&os(&["/tmp"]));
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert_eq!(stderr, "compression: there is no Wayland or X11 display to open a window on (neither WAYLAND_DISPLAY nor DISPLAY is set)\n");
    assert!(stdout.is_empty());
}

#[test]
fn more_than_fifty_targets_are_refused_at_once() {
    let sandbox = Sandbox::new("many");
    let many: Vec<OsString> = (0..51).map(|n| OsString::from(format!("/target/{n}"))).collect();
    refused(&sandbox, &many, "too many targets (51, at most 50)");
}

#[test]
fn a_file_of_another_computer_is_refused() {
    let sandbox = Sandbox::new("uri");
    refused(&sandbox, &os(&["file://otherhost/tmp/x"]), "not a file of this computer");
    refused(&sandbox, &os(&["/tmp", "file://example.com/"]), "not a file of this computer");
}

#[test]
fn a_dialog_request_with_a_target_or_with_a_mode_that_does_not_exist_is_refused() {
    let sandbox = Sandbox::new("dialog");
    refused(&sandbox, &os(&["--chooser", "--mode", "open", "/tmp"]), "takes no other argument");
    refused(&sandbox, &os(&["--chooser", "--bogus"]), "takes no other argument");
    refused(&sandbox, &os(&["--chooser", "--mode", "nonsense"]), "could not be read");
    refused(&sandbox, &os(&["--chooser", "--mode"]), "could not be read");
    refused(&sandbox, &os(&["--portal", "--chooser"]), "file dialog request");
}

#[test]
fn a_dialog_that_cannot_open_its_window_ends_with_exit_2_which_is_not_the_exit_of_a_cancel() {
    // There is no display here, so the window cannot open. 1 would say "cancelled" to whoever asked (see the
    // README): a dialog that failed is 2.
    let sandbox = Sandbox::new("nodisplay");
    let out = sandbox.run(&os(&["--chooser", "--mode", "open"]));
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.starts_with("compression: there is no Wayland or X11 display"), "{stderr:?}");
    assert!(stdout.is_empty(), "nothing chosen: {stdout:?}");
    // It got as far as a start for real: the old data was copied (and nothing else was made)
    sandbox.assert_only(&["compression", "ski"]);
}

#[test]
fn the_program_binary_and_the_sandbox_are_what_the_tests_think_they_are() {
    // The runs above rely on this: a folder nobody has used, and a program that is this package's
    let sandbox = Sandbox::new("self");
    sandbox.assert_untouched();
    assert!(Path::new(env!("CARGO_BIN_EXE_compression")).is_file());
    assert!(sandbox.command().get_envs().any(|(k, v)| k == "XDG_RUNTIME_DIR" && v.is_some()));
    assert!(!sandbox.command().get_envs().any(|(k, _)| k == "DISPLAY" || k == "WAYLAND_DISPLAY"));
}
