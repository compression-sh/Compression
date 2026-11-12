//! Words for the errors of the file system that people meet most, so a message does not read
//! "Permission denied (os error 13)" in English on a screen set to another language.
use std::io;

use crate::i18n::{tr, trf};

/// What went wrong, for a message. The errors of the system are put in the language of the window, and
/// one that has no words of ours says only its number (`System error 19`), never the system's English. An
/// error that Compression made itself (it has no number) has its words already (`tr`) and keeps them.
pub fn io_error(err: &io::Error) -> String {
    let Some(code) = err.raw_os_error() else { return err.to_string() };
    let words = match code {
        libc::EACCES | libc::EPERM => tr("Permission denied"),
        libc::ENOENT => tr("Not found"),
        libc::EEXIST => tr("Already exists"),
        libc::ENOTEMPTY => tr("The folder is not empty"),
        libc::ENOSPC | libc::EDQUOT => tr("Not enough space on the disk"),
        libc::EROFS => tr("The file system is read-only"),
        libc::ENAMETOOLONG => tr("The name is too long"),
        libc::EBUSY => tr("It is in use"),
        libc::EIO => tr("The disk or the connection reported an error"),
        libc::ENOTDIR => tr("It is not a folder"),
        libc::EISDIR => tr("It is a folder"),
        libc::ELOOP => tr("There are too many links in a row"),
        libc::EXDEV => tr("It is on another drive"),
        libc::ENODEV | libc::ENXIO => tr("The device is not there"),
        libc::EMFILE | libc::ENFILE => tr("Too many files are open"),
        libc::ETIMEDOUT => tr("The connection timed out"),
        libc::ECONNREFUSED => tr("The connection was refused"),
        libc::ECONNRESET | libc::EPIPE | libc::ENOTCONN => tr("The connection was closed"),
        libc::EHOSTUNREACH | libc::ENETUNREACH | libc::ENETDOWN | libc::EHOSTDOWN => tr("The server cannot be reached"),
        libc::ESTALE => tr("The server no longer knows this item"),
        libc::EFBIG => tr("The file is too big"),
        libc::ENOMEM => tr("Not enough memory"),
        libc::EINVAL => tr("That is not possible here"),
        libc::EOPNOTSUPP => tr("This is not supported here"),
        _ => return trf("System error {}", &[&code]),
    };
    words.to_owned()
}

/// `what: why`, for a message about one item or one program.
pub fn failed(what: &str, err: &io::Error) -> String {
    format!("{what}: {}", io_error(err))
}

/// What is wrong with a JSON text, in words of the window (not `serde`'s English): not JSON at all, or JSON with
/// a value of the wrong kind, and the line it was found on.
pub fn json_error(err: &serde_json::Error) -> String {
    json_words(err.is_data(), err.line())
}

/// `json_error`, for a reader that has kept only the kind and the line.
pub fn json_words(wrong_value: bool, line: usize) -> String {
    if wrong_value { trf("a value has the wrong type (line {})", &[&line]) } else { trf("not valid JSON (line {})", &[&line]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usual_errors_of_the_system_come_in_plain_words() {
        let words = |errno| io_error(&io::Error::from_raw_os_error(errno));
        assert_eq!(words(libc::EACCES), "Permission denied");
        assert_eq!(words(libc::EPERM), "Permission denied");
        assert_eq!(words(libc::ENOENT), "Not found");
        assert_eq!(words(libc::EEXIST), "Already exists");
        assert_eq!(words(libc::ENOTEMPTY), "The folder is not empty");
        assert_eq!(words(libc::ENOSPC), "Not enough space on the disk");
        assert_eq!(words(libc::EDQUOT), "Not enough space on the disk");
        assert_eq!(words(libc::EROFS), "The file system is read-only");
        assert_eq!(words(libc::ENAMETOOLONG), "The name is too long");
        assert_eq!(words(libc::EBUSY), "It is in use");
        for errno in [libc::EACCES, libc::ENOENT, libc::EEXIST, libc::ENOSPC, libc::EROFS] {
            assert!(!words(errno).contains("os error"), "{errno}");
        }
    }

    #[test]
    fn other_errors_keep_their_own_text() {
        // An error that Compression made itself has its words already, whatever its kind
        let own = io::Error::new(io::ErrorKind::PermissionDenied, "Administrator mode only starts a program that nobody else can change");
        assert_eq!(io_error(&own), "Administrator mode only starts a program that nobody else can change");
        assert_eq!(io_error(&io::Error::other("it broke")), "it broke");
        // A system error with no words of ours says its number and nothing of the system's English
        let other = io::Error::from_raw_os_error(libc::ENOTBLK);
        assert_eq!(io_error(&other), "System error 15");
        assert!(!io_error(&other).contains("os error"));
    }

    #[test]
    fn every_system_error_with_words_of_ours_is_free_of_the_systems_english() {
        for code in [
            libc::EACCES, libc::EPERM, libc::ENOENT, libc::EEXIST, libc::ENOTEMPTY, libc::ENOSPC, libc::EDQUOT, libc::EROFS, libc::ENAMETOOLONG, libc::EBUSY,
            libc::EIO, libc::ENOTDIR, libc::EISDIR, libc::ELOOP, libc::EXDEV, libc::ENODEV, libc::ENXIO, libc::EMFILE, libc::ENFILE, libc::ETIMEDOUT,
            libc::ECONNREFUSED, libc::ECONNRESET, libc::EPIPE, libc::ENOTCONN, libc::EHOSTUNREACH, libc::ENETUNREACH, libc::ENETDOWN, libc::EHOSTDOWN,
            libc::ESTALE, libc::EFBIG, libc::ENOMEM, libc::EINVAL, libc::EOPNOTSUPP,
        ] {
            let text = io_error(&io::Error::from_raw_os_error(code));
            assert!(!text.contains("os error") && !text.starts_with("System error"), "{code}: {text}");
            assert!(!text.is_empty());
        }
        assert_eq!(io_error(&io::Error::from_raw_os_error(libc::EIO)), "The disk or the connection reported an error");
        assert_eq!(io_error(&io::Error::from_raw_os_error(libc::ENOTDIR)), "It is not a folder");
        assert_eq!(io_error(&io::Error::from_raw_os_error(libc::ETIMEDOUT)), "The connection timed out");
        assert_eq!(io_error(&io::Error::from_raw_os_error(libc::EHOSTUNREACH)), "The server cannot be reached");
    }

    #[test]
    fn an_error_with_no_words_of_ours_says_only_its_number() {
        for (code, text) in [(libc::ENOTBLK, "System error 15"), (libc::ENOTTY, "System error 25"), (libc::ENOEXEC, "System error 8")] {
            assert_eq!(io_error(&io::Error::from_raw_os_error(code)), text);
        }
        // An error of our own has no number and keeps its words
        assert_eq!(io_error(&io::Error::other("already in words")), "already in words");
    }

    #[test]
    fn what_and_why_are_put_together_with_the_reason_in_words() {
        let err = io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(failed("photo.png", &err), "photo.png: Permission denied");
        assert_eq!(failed("sshfs", &io::Error::from_raw_os_error(libc::ENOTBLK)), "sshfs: System error 15");
        assert!(!failed("x", &io::Error::from_raw_os_error(libc::ENOENT)).contains("os error"));
    }

    #[test]
    fn a_json_error_is_told_as_a_kind_and_a_line_and_never_in_serdes_english() {
        let syntax = serde_json::from_str::<serde_json::Value>("{\n  \"a\": 1,\n  oops\n}").unwrap_err();
        assert_eq!(json_error(&syntax), "not valid JSON (line 3)");
        let wrong = serde_json::from_str::<std::collections::HashMap<String, u8>>("{\n\"a\": \"text\"\n}").unwrap_err();
        assert_eq!(json_error(&wrong), "a value has the wrong type (line 2)");
        let eof = serde_json::from_str::<serde_json::Value>("{ \"a\": ").unwrap_err();
        assert_eq!(json_error(&eof), "not valid JSON (line 1)");
        for err in [&syntax, &wrong, &eof] {
            let text = json_error(err);
            assert!(!text.contains("expected") && !text.contains("column") && !text.contains("invalid type"), "{text}");
        }
        assert_eq!(json_words(false, 7), "not valid JSON (line 7)");
        assert_eq!(json_words(true, 7), "a value has the wrong type (line 7)");
    }

    // What this scan looks for: the places where an error would reach a person in the system's own English or as
    // a bare number (`err.to_string()`, `{err}`), and errors Compression makes with an English word of its own that
    // is not put through `tr`. It reads the code of every source file up to its test module.
    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    // The test module closes the code (other `cfg(test)` items stand in the middle of it and are code too)
                    let lines: Vec<&str> = text.lines().collect();
                    let end = (0..lines.len())
                        .find(|&i| lines[i] == "#[cfg(test)]" && lines.get(i + 1).is_some_and(|next| ["mod ", "pub(crate) mod ", "pub mod "].iter().any(|m| next.starts_with(m))))
                        .unwrap_or(lines.len());
                    let code = lines[..end].join("\n");
                    out.push((path.to_string_lossy().replace('\\', "/"), code));
                }
            }
        }
        let mut out = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        out
    }

    const ERROR_NAMES: &[&str] = &["err", "e", "error"];

    fn is_ident(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// Whether `needle` stands in `line` as a whole word: not the end of a longer name (`mtime.to_string()` is not `e.to_string()`),
    /// and not a field of something (`.e`).
    fn has_word(line: &str, needle: &str) -> bool {
        line.match_indices(needle).any(|(at, _)| !line[..at].chars().next_back().is_some_and(|c| is_ident(c) || c == '.'))
    }

    /// The rule a line of code breaks, if it does. What is put through `describe` is as it should be.
    fn broken_rule(line: &str) -> Option<&'static str> {
        let code = line.trim();
        if code.starts_with("//") || code.contains("describe::failed(") {
            return None;
        }
        let mut code = code.to_owned();
        for name in ERROR_NAMES {
            for call in ["io_error", "json_error"] {
                code = code.replace(&format!("{call}(&{name})"), "").replace(&format!("{call}({name})"), "");
            }
        }
        for name in ERROR_NAMES {
            if has_word(&code, &format!("{name}.to_string()")) {
                return Some("an error as plain text (`.to_string()`)");
            }
            if code.contains("trf(") || code.contains("format!(") {
                if ["]", ",", ")"].iter().any(|after| has_word(&code, &format!("&{name}{after}"))) {
                    return Some("an error put in a message as it is (`&err`)");
                }
                if ["}", ":"].iter().any(|after| code.contains(&format!("{{{name}{after}"))) {
                    return Some("an error put in a message as it is (`{err}`)");
                }
            }
        }
        // `io::Error::other("English")`, `Error::new(kind, "English")`, `Failure::Other("English".into())`
        for start in ["Error::other(\"", "Failure::Other(\"", "Failure::Message(\"", "Error::new(io::ErrorKind::", "Error::new(std::io::ErrorKind::"] {
            if let Some(at) = code.find(start) {
                let rest = &code[at + start.len()..];
                let literal = if start.ends_with('"') { Some(rest) } else { rest.split_once(", \"").map(|(_, r)| r) };
                if literal.is_some_and(|l| l.chars().next().is_some_and(char::is_alphabetic)) {
                    return Some("an error with an English word of its own");
                }
            }
        }
        None
    }

    /// Lines that break a rule on purpose, each with why. A line is named by the text it contains; one that matches
    /// no line any more is a mistake (see the test below).
    const ALLOWED: &[(&str, &str, &str)] = &[
        // Words that end with the program, not on a screen: the portal's console and the log of the service
        ("src/portal.rs", "could not claim", "printed on the console by the D-Bus service"),
        ("src/portal.rs", "e.to_string()", "the error of the D-Bus service, printed on the console"),
        // Signals between threads, which no message ever shows
        // Texts that already are words: an error that went through `describe`, or came as a message
        ("src/app/jobs_ui.rs", "kept for another try: {}", "the error is the text of the step that failed, in words"),
        ("src/app/jobs_ui.rs", "were given up on: {}", "the error is the text of the step that failed, in words"),
        ("src/app/network_ui.rs", "Could not disconnect", "the error is the text the connection thread made, in words"),
        ("src/app/network_ui.rs", "Could not forget the password", "the error is the text the keyring thread made"),
        ("src/app/network_ui.rs", "Could not connect to", "the error is the text the connection thread made, in words"),
        ("src/app.rs", "Renamed {} items, then", "the error is the text `batch_rename::apply` made through `describe`"),
        ("src/app.rs", "old_ext.filter", "`e` is a file extension, not an error"),
        ("src/fs/describe.rs", "return err.to_string() }", "an error with no number is in words already, which is the rule of `io_error`"),
        ("src/ui/theme.rs", "err.raw_os_error(), err.to_string()", "kept for an error with no number, which is in words already (see `describe`)"),
        // What another program said, which stays as it said it and is shown in a frame of our words
        ("src/fs/secrets.rs", "e.to_string()", "the text of the keyring service (D-Bus), shown inside a translated message"),
        ("src/fs/secrets.rs", "last = e.to_string()", "the text of the keyring service (D-Bus), shown inside a translated message"),
        ("src/plugins/actions.rs", "&[&label, &error]", "the last line a script printed on stderr, in a translated frame"),
        ("src/fs/ops.rs", "io::ErrorKind::Interrupted, \"cancelled\"", "a stop, told by its kind"),
        ("src/fs/engine.rs", "io::ErrorKind::Interrupted, \"cancelled\"", "a stop, told by its kind"),
        ("src/fs/archive_list.rs", "\"cancelled\"", "a stop, told by its kind"),
        ("src/fs/archive_list.rs", "\"time limit\"", "a stop, told by its kind"),
        ("src/plugins/progress.rs", "\"cancelled\"", "a stop, told by its kind"),
    ];

    fn allowed(path: &str, line: &str) -> bool {
        ALLOWED.iter().any(|(file, needle, _)| path.ends_with(file) && line.contains(needle))
    }

    #[test]
    fn no_error_reaches_a_screen_in_the_systems_english_or_as_a_bare_text() {
        let mut found = Vec::new();
        for (path, code) in sources() {
            for (n, line) in code.lines().enumerate() {
                if let Some(rule) = broken_rule(line)
                    && !allowed(&path, line)
                {
                    found.push(format!("{path}:{}: {rule}: {}", n + 1, line.trim()));
                }
            }
        }
        assert!(found.is_empty(), "errors that would reach the screen untold:\n{}", found.join("\n"));
    }

    #[test]
    fn the_scan_finds_the_bad_lines_it_is_there_for_and_leaves_the_good_ones() {
        for bad in [
            "self.set_toast(err.to_string());",
            "Err(e) => failed.push(format!(\"{}: {err}\", name)),",
            "self.set_toast(trf(\"Could not: {}\", &[&err]));",
            "trf(\"Could not: {}\", &[&name, &e])",
            ".map_err(|e| e.to_string())?",
            "Err(io::Error::other(\"no free name\"))",
            "io::Error::new(io::ErrorKind::InvalidInput, \"no file name\")",
            "Outcome::failed(error.to_string())",
            "ok_or_else(|| Failure::Other(\"no output\".into()))",
            "Failure::Message(\"gio\".into())",
        ] {
            assert!(broken_rule(bad).is_some(), "not found: {bad}");
        }
        for good in [
            "self.set_toast(crate::fs::describe::io_error(&err));",
            "failed.push(crate::fs::describe::failed(&name, &err))",
            "trf(\"Copied {}\", &[&name])",
            "Err(io::Error::other(tr(\"It has no name\")))",
            "io::Error::new(io::ErrorKind::TimedOut, trf(\"It took longer than {} minutes\", &[&minutes]))",
            "let one_error = other.error.to_string();",
            "// self.set_toast(err.to_string());",
            "trf(\"{} items\", &[&entries])",
        ] {
            assert_eq!(broken_rule(good), None, "wrongly found: {good}");
        }
    }

    #[test]
    fn every_allowed_line_is_still_there_and_still_breaks_a_rule() {
        let sources = sources();
        let mut stale = Vec::new();
        for (file, needle, why) in ALLOWED {
            assert!(!why.is_empty());
            let hit = sources.iter().filter(|(path, _)| path.ends_with(file)).flat_map(|(_, code)| code.lines()).any(|line| line.contains(needle) && broken_rule(line).is_some());
            if !hit {
                stale.push(format!("{file}: {needle:?}"));
            }
        }
        assert!(stale.is_empty(), "nothing matches these any more (or it no longer breaks a rule): take them off the list\n{}", stale.join("\n"));
    }
}
