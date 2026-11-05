//! The command line of the built program. Every run gets a home, XDG folders and a runtime folder of
//! its own and no display, so it never reaches real user data, a socket, a bus or a clipboard.
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_compression");

/// A fresh sandbox: `home` (with the XDG folders under it) and `run` (the runtime folder).
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("compression_cli_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::set_permissions(root.join("run"), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Runs the program with `args` in `dir`, waits for it (a hung run is stopped and fails the test).
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        let args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        self.run_os(dir, &args)
    }

    /// `run` for arguments that need not be text.
    fn run_os(&self, dir: &Path, args: &[&std::ffi::OsStr]) -> Output {
        let home = self.home();
        let mut child = Command::new(EXE)
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("compression {args:?} did not end within 30 seconds");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait_with_output().unwrap()
    }

    /// Everything the runs left in the sandbox, apart from what the test put there.
    fn leftovers(&self) -> Vec<String> {
        let mut found = Vec::new();
        for folder in ["home", "run"] {
            for entry in std::fs::read_dir(self.root.join(folder)).unwrap() {
                found.push(format!("{folder}/{}", entry.unwrap().file_name().to_string_lossy()));
            }
        }
        found
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn version_prints_the_version_of_cargo_toml_and_ends_with_0() {
    let sandbox = Sandbox::new("version");
    let expected = format!("compression {}\n", env!("CARGO_PKG_VERSION"));
    for flag in ["--version", "-V"] {
        let out = sandbox.run(&sandbox.root, &[flag]);
        assert!(out.status.success(), "{flag}: {:?}", out.status);
        assert_eq!(text(&out.stdout), expected, "{flag}");
        assert_eq!(text(&out.stderr), "", "{flag}");
    }
    // Neither the migration nor an instance nor a socket was started
    assert_eq!(sandbox.leftovers(), Vec::<String>::new());
}

#[test]
fn help_prints_the_usage_and_ends_with_0() {
    let sandbox = Sandbox::new("help");
    let long = sandbox.run(&sandbox.root, &["--help"]);
    let short = sandbox.run(&sandbox.root, &["-h"]);
    for out in [&long, &short] {
        assert!(out.status.success(), "{:?}", out.status);
        assert_eq!(text(&out.stderr), "");
    }
    let help = text(&long.stdout);
    assert_eq!(text(&short.stdout), help);
    assert!(help.contains("Usage:"));
    for mode in ["--chooser", "--portal", "--resident", "--version", "--help"] {
        assert!(help.contains(mode), "the help does not mention {mode}");
    }
    assert!(help.contains(env!("CARGO_PKG_VERSION")));
    assert_eq!(sandbox.leftovers(), Vec::<String>::new());
}

#[test]
fn only_the_first_argument_asks_for_information() {
    let sandbox = Sandbox::new("later");
    // Without a display this ends in an error about the display, which is all that is needed to
    // know that the second argument was not taken for a request for the version
    let out = sandbox.run(&sandbox.root, &["--chooser", "--version"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(!text(&out.stdout).contains("compression 0."));
}

#[test]
fn a_folder_named_like_a_flag_is_still_a_folder() {
    let sandbox = Sandbox::new("folder");
    let folder = sandbox.root.join("--version");
    std::fs::create_dir_all(&folder).unwrap();
    // No resident instance is wanted from a test: a start that stays in the background would leave one
    let config = sandbox.home().join(".config/compression");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings.json"), r#"{"run_in_background":false}"#).unwrap();

    let out = sandbox.run(&sandbox.root, &["./--version"]);
    // It tried to open a window (and found no display); it did not print a version or the help
    assert!(!text(&out.stdout).contains("compression 0."), "{}", text(&out.stdout));
    assert!(!text(&out.stdout).contains("Usage:"));
    assert!(!out.status.success(), "no display, so no window");
    assert!(!text(&out.stderr).contains("Usage:"));
}

#[test]
fn a_name_that_is_not_utf8_is_no_reason_to_panic() {
    use std::os::unix::ffi::OsStrExt;
    let sandbox = Sandbox::new("bytes");
    let config = sandbox.home().join(".config/compression");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings.json"), r#"{"run_in_background":false}"#).unwrap();
    // A name with bytes that are no UTF-8, as another program may hand over for "open with"
    let odd = std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe-compression");
    for args in [vec![odd], vec![std::ffi::OsStr::new("--chooser"), odd]] {
        let out = sandbox.run_os(&sandbox.root, &args);
        let stderr = text(&out.stderr);
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert_ne!(out.status.code(), Some(101), "{stderr}");
    }
    // The information requests still work with such a name behind them
    let out = sandbox.run_os(&sandbox.root, &[std::ffi::OsStr::new("--version"), odd]);
    assert_eq!(text(&out.stdout), format!("compression {}\n", env!("CARGO_PKG_VERSION")));
}
