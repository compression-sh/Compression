//! Open a folder in a second Compression that runs as root. The person asks for the password through
//! polkit (`pkexec`), the same way graphical tools elevate, so Compression never sees it.
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

/// Tells the elevated Compression where the settings of the person who started it live. They are read
/// from there but never written, so root does not leave files in someone else's home.
pub const CONFIG_ENV: &str = "COMPRESSION_ADMIN_CONFIG";

/// Variables the elevated process needs to reach the display and the session. Nothing that names
/// places to load programs or libraries from (`LD_LIBRARY_PATH`) goes along: root must not take
/// those from the environment of the person who started it.
const PASS_ENV: &[&str] = &[
    "DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP",
    "XDG_DATA_DIRS", "LANG", "LANGUAGE", "LC_ALL", "XCURSOR_THEME", "XCURSOR_SIZE", "GDK_SCALE",
];

/// First argument of the elevated start: the environment follows as `NAME=VALUE` words, up to `--`.
/// `pkexec` clears the environment, and passing it through `env` would make polkit authorize
/// running `env`, which runs anything; Compression sets it itself instead.
pub const ENV_FLAG: &str = "--admin-environment";

/// Whether this process already runs as root.
pub fn is_admin() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Whether the elevated Compression was started from another one, and so reads that one's settings.
pub fn config_override() -> Option<PathBuf> {
    crate::compat::env_var("ADMIN_CONFIG").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

fn pkexec() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    // NixOS and a few other systems keep the setuid wrapper outside the usual PATH of a desktop session
    let extra = ["/run/wrappers/bin", "/usr/bin", "/usr/local/bin"].map(PathBuf::from);
    std::env::split_paths(&paths).chain(extra).map(|dir| dir.join("pkexec")).find(|p| p.is_file())
}

/// Whether administrator access can be asked for at all.
pub fn available() -> bool {
    !is_admin() && pkexec().is_some()
}

/// The `NAME=VALUE` words that carry the environment to the elevated process: the variables of
/// `PASS_ENV` that are set (and are text), and where the person's settings are.
fn environment_words(get: impl Fn(&str) -> Option<std::ffi::OsString>, config: Option<PathBuf>) -> Vec<String> {
    let mut words: Vec<String> = PASS_ENV.iter().filter_map(|name| Some(format!("{name}={}", get(name)?.into_string().ok()?))).collect();
    if let Some(config) = config.and_then(|c| c.into_os_string().into_string().ok()) {
        words.push(format!("{CONFIG_ENV}={config}"));
    }
    words
}

/// `pkexec <this program> --admin-environment NAME=VALUE… -- <folder>`: polkit is asked to run
/// this program itself, by its own path, and not a program that can run anything. The folder
/// comes after `--`, so a name that starts with a dash is still a folder.
fn elevation_command(pkexec: &Path, exe: &Path, dir: &Path, environment: Vec<String>) -> Command {
    let mut command = Command::new(pkexec);
    command.arg(exe).arg(ENV_FLAG).args(environment).arg("--").arg(dir);
    command
}

/// `NAME` and `VALUE` of each variable an elevated start carries.
type Environment = Vec<(String, String)>;

/// Splits the arguments of the elevated start into the environment it carries and the rest.
/// Only the names the start passes are taken; anything else (`LD_PRELOAD`, `PATH`, …) is dropped.
/// `None` when the arguments do not begin with `ENV_FLAG`.
fn split_environment(args: &[String]) -> Option<(Environment, Vec<String>)> {
    if args.first().map(String::as_str) != Some(ENV_FLAG) {
        return None;
    }
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let allowed = |name: &str| name == CONFIG_ENV || PASS_ENV.contains(&name);
    let pairs = args[1..end].iter().filter_map(|word| word.split_once('=')).filter(|(name, _)| allowed(name)).map(|(n, v)| (n.to_owned(), v.to_owned())).collect();
    Some((pairs, args.get(end + 1..).unwrap_or_default().to_vec()))
}

/// Called first thing in `main`: takes the environment that an elevated start carries out of the
/// arguments, and sets it when this really runs as root; the rest of the arguments is returned.
pub fn take_environment(args: Vec<String>) -> Vec<String> {
    let Some((pairs, rest)) = split_environment(&args) else { return args };
    if is_admin() {
        for (name, value) in pairs {
            // SAFETY: this runs at the very start of the process, before any thread exists
            unsafe { std::env::set_var(name, value) };
        }
    }
    rest
}

/// Start Compression as root on `dir`. The result arrives once polkit is done: `Err` carries what went
/// wrong, and nothing is sent when the person simply dismissed the password prompt.
pub fn open_folder(dir: &Path) -> io::Result<mpsc::Receiver<io::Error>> {
    let pkexec = pkexec().ok_or_else(|| io::Error::other(crate::i18n::tr("pkexec was not found. Install polkit to use administrator mode")))?;
    let exe = crate::portal::own_exe()?;
    let environment = environment_words(|name| std::env::var_os(name), dirs::config_dir());
    let mut child = elevation_command(&pkexec, &exe, dir, environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // pkexec: 126 is a dismissed or refused prompt, 127 is no agent or a failure to start
        if let Ok(status) = child.wait()
            && status.code() == Some(127)
        {
            let _ = tx.send(io::Error::other(crate::i18n::tr("Could not get administrator rights. Is a polkit agent running?")));
        }
    });
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_elevated_start_runs_this_program_itself_and_not_env() {
        let environment = environment_words(
            |name| match name {
                "WAYLAND_DISPLAY" => Some(OsString::from("wayland-1")),
                "XDG_RUNTIME_DIR" => Some(OsString::from("/run/user/1000")),
                // Not passed on, however it is set
                "LD_LIBRARY_PATH" | "LD_PRELOAD" | "PATH" | "HOME" => Some(OsString::from("/home/me/evil")),
                _ => None,
            },
            Some(PathBuf::from("/home/me/.config")),
        );
        assert_eq!(environment, words(&["WAYLAND_DISPLAY=wayland-1", "XDG_RUNTIME_DIR=/run/user/1000", "COMPRESSION_ADMIN_CONFIG=/home/me/.config"]));

        let command = elevation_command(Path::new("/usr/bin/pkexec"), Path::new("/usr/bin/compression"), Path::new("/etc"), environment);
        assert_eq!(command.get_program(), "/usr/bin/pkexec");
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            ["/usr/bin/compression", "--admin-environment", "WAYLAND_DISPLAY=wayland-1", "XDG_RUNTIME_DIR=/run/user/1000", "COMPRESSION_ADMIN_CONFIG=/home/me/.config", "--", "/etc"]
        );
        // polkit is asked about this program, never about env
        assert!(!args.iter().any(|a| a == "env" || a.ends_with("/env")));
        // A folder whose name starts with a dash is still only a folder
        let command = elevation_command(Path::new("pkexec"), Path::new("/usr/bin/compression"), Path::new("-oops"), Vec::new());
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(&args[args.len() - 2..], ["--", "-oops"]);
        // Values that are not text are left out rather than mangled
        let odd = environment_words(|name| (name == "XAUTHORITY").then(|| std::os::unix::ffi::OsStringExt::from_vec(vec![0xff, b'x'])), None);
        assert!(odd.is_empty());
    }

    #[test]
    fn the_elevated_program_takes_only_the_environment_it_passes_and_returns_the_rest() {
        let args = words(&["--admin-environment", "DISPLAY=:0", "LD_PRELOAD=/tmp/x.so", "PATH=/tmp", "COMPRESSION_ADMIN_CONFIG=/home/me/.config", "LD_LIBRARY_PATH=/tmp", "=x", "novalue", "--", "-folder"]);
        let (pairs, rest) = split_environment(&args).expect("carries an environment");
        assert_eq!(pairs, [("DISPLAY".to_owned(), ":0".to_owned()), ("COMPRESSION_ADMIN_CONFIG".to_owned(), "/home/me/.config".to_owned())]);
        assert_eq!(rest, ["-folder"]);
        // A value may hold an equals sign, and a missing `--` leaves no folder
        let (pairs, rest) = split_environment(&words(&["--admin-environment", "LANG=a=b"])).unwrap();
        assert_eq!(pairs, [("LANG".to_owned(), "a=b".to_owned())]);
        assert!(rest.is_empty());
        // An ordinary start is not touched, and does not set anything
        assert!(split_environment(&words(&["/home/me"])).is_none());
        assert!(split_environment(&[]).is_none());
        assert_eq!(take_environment(words(&["/home/me", "--admin-environment"])), words(&["/home/me", "--admin-environment"]));
        // Not root: the arguments are stripped, and the environment of this process stays as it is
        if !is_admin() {
            let before = std::env::var_os("GDK_SCALE");
            assert_eq!(take_environment(words(&["--admin-environment", "GDK_SCALE=7", "--", "/srv"])), ["/srv"]);
            assert_eq!(std::env::var_os("GDK_SCALE"), before);
        }
    }
}
