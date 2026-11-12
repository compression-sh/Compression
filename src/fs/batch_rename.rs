//! Renaming several files at once, from a template or by find and replace.
//!
//! Both modes work on the name without its extension, so a file never loses its type by accident.

use std::collections::HashSet;

use crate::i18n::{tr, trf};
use std::path::{Path, PathBuf};

pub const TOKEN_NAME: &str = "[Original name]";
pub const TOKEN_NUM_1: &str = "[1, 2, 3]";
pub const TOKEN_NUM_2: &str = "[01, 02, 03]";
pub const TOKEN_NUM_3: &str = "[001, 002, 003]";
pub const TOKEN_DATE: &str = "[Modified date]";
pub const TOKEN_FOLDER: &str = "[Parent folder]";
pub const TOKEN_EXT: &str = "[Extension]";

/// Tokens offered next to the template field, with the label shown on their buttons.
pub const TOKENS: [(&str, &str); 7] = [
    (TOKEN_NAME, "Original name"),
    (TOKEN_NUM_1, "1, 2, 3"),
    (TOKEN_NUM_2, "01, 02, 03"),
    (TOKEN_NUM_3, "001, 002, 003"),
    (TOKEN_DATE, "Modified date"),
    (TOKEN_FOLDER, "Parent folder"),
    (TOKEN_EXT, "Extension"),
];

/// Letter case applied to the new name, after the template or replacement.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Case {
    Keep,
    Lower,
    Upper,
    Title,
    Sentence,
}

impl Case {
    pub const ALL: [(Case, &'static str); 5] =
        [(Case::Keep, "Keep"), (Case::Lower, "lowercase"), (Case::Upper, "UPPERCASE"), (Case::Title, "Title Case"), (Case::Sentence, "Sentence case")];

    fn apply(self, text: &str) -> String {
        match self {
            Case::Keep => text.to_owned(),
            Case::Lower => text.to_lowercase(),
            Case::Upper => text.to_uppercase(),
            Case::Title => {
                let mut start = true;
                text.chars()
                    .map(|c| {
                        let out = if start { c.to_uppercase().collect::<String>() } else { c.to_lowercase().collect() };
                        start = !c.is_alphanumeric() && c != '\'';
                        out
                    })
                    .collect()
            }
            Case::Sentence => {
                let mut chars = text.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
                    None => String::new(),
                }
            }
        }
    }
}

/// What to do with spaces in the new name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Spaces {
    Keep,
    Underscore,
    Dash,
    Remove,
}

impl Spaces {
    pub const ALL: [(Spaces, &'static str); 4] = [(Spaces::Keep, "Keep"), (Spaces::Underscore, "→ _"), (Spaces::Dash, "→ -"), (Spaces::Remove, "Remove")];
}

/// Order in which files are numbered.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Order {
    AsListed,
    Name,
    Date,
}

impl Order {
    pub const ALL: [(Order, &'static str); 3] = [(Order::AsListed, "As listed"), (Order::Name, "By name"), (Order::Date, "By date")];
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Template,
    Replace,
}

/// One file to rename, with what the tokens need to know about it.
#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
    /// `YYYY-MM-DD`, empty when unknown.
    pub date: String,
}

/// What the user typed in the dialog.
pub struct Rules {
    pub mode: Mode,
    pub template: String,
    pub find: String,
    pub replace: String,
    /// Find and replace only matches the exact letter case.
    pub match_case: bool,
    pub case: Case,
    pub spaces: Spaces,
    pub prefix: String,
    pub suffix: String,
    /// Characters cut from the start / end of the original name, as typed.
    pub trim_start: String,
    pub trim_end: String,
    /// Number the first file gets, and how much each next one adds, as typed.
    pub start: String,
    pub step: String,
    pub order: Order,
    pub lowercase_ext: bool,
}

impl Rules {
    pub fn new() -> Self {
        Self {
            mode: Mode::Template,
            template: TOKEN_NAME.to_owned(),
            find: String::new(),
            replace: String::new(),
            match_case: true,
            case: Case::Keep,
            spaces: Spaces::Keep,
            prefix: String::new(),
            suffix: String::new(),
            trim_start: String::new(),
            trim_end: String::new(),
            start: "1".to_owned(),
            step: "1".to_owned(),
            order: Order::AsListed,
            lowercase_ext: false,
        }
    }

    fn start_number(&self) -> i64 {
        self.start.trim().parse().unwrap_or(1)
    }

    fn step_number(&self) -> i64 {
        self.step.trim().parse().unwrap_or(1)
    }
}

fn trim_chars(text: &str, start: usize, end: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = start.min(chars.len());
    let end = end.min(chars.len() - start);
    chars[start..chars.len() - end].iter().collect()
}

fn replace_text(text: &str, find: &str, with: &str, match_case: bool) -> String {
    if match_case {
        return text.replace(find, with);
    }
    let (lower_text, lower_find) = (text.to_lowercase(), find.to_lowercase());
    // Lowercasing can change byte lengths; fall back to an exact match when it does
    if lower_text.len() != text.len() || lower_find.len() != find.len() {
        return text.replace(find, with);
    }
    let mut out = String::new();
    let mut at = 0;
    while let Some(i) = lower_text[at..].find(&lower_find) {
        // Equal total lengths do not make equal positions (one letter grows, another shrinks):
        // a match that does not sit on letter boundaries of the real text cannot be cut out of it
        if !text.get(at + i..at + i + find.len()).is_some_and(|real| real.to_lowercase() == lower_find) {
            return text.replace(find, with);
        }
        out.push_str(&text[at..at + i]);
        out.push_str(with);
        at += i + find.len();
    }
    out.push_str(&text[at..]);
    out
}

/// Splits a file name into the part that gets renamed and the extension that stays.
pub fn split_name(name: &str, is_dir: bool) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if !is_dir && i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// The name each entry gets, in the same order. Unchanged entries keep their name.
pub fn preview(entries: &[Entry], rules: &Rules) -> Vec<String> {
    // Numbers follow the chosen order, not the order of the list
    let mut ranked: Vec<usize> = (0..entries.len()).collect();
    match rules.order {
        Order::AsListed => {}
        Order::Name => {
            let name = |i: usize| entries[i].path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            ranked.sort_by(|&a, &b| super::scanner::natural_cmp(&name(a), &name(b)));
        }
        Order::Date => ranked.sort_by(|&a, &b| entries[a].date.cmp(&entries[b].date)),
    }
    let mut rank = vec![0usize; entries.len()];
    for (position, &i) in ranked.iter().enumerate() {
        rank[i] = position;
    }
    let (start, step) = (rules.start_number(), rules.step_number());
    let (cut_start, cut_end) = (rules.trim_start.trim().parse().unwrap_or(0), rules.trim_end.trim().parse().unwrap_or(0));

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let (stem, ext) = split_name(&name, entry.is_dir);
            let folder = entry.path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // A start or step typed as a huge number must not overflow
            let number = start.saturating_add((rank[index] as i64).saturating_mul(step));
            let base = trim_chars(stem, cut_start, cut_end);
            let mut new_stem = match rules.mode {
                Mode::Template => expand(&rules.template, &base, number, &entry.date, &folder, ext.trim_start_matches('.')),
                Mode::Replace if rules.find.is_empty() => base,
                Mode::Replace => replace_text(&base, &rules.find, &rules.replace, rules.match_case),
            };
            new_stem = match rules.spaces {
                Spaces::Keep => new_stem,
                Spaces::Underscore => new_stem.replace(' ', "_"),
                Spaces::Dash => new_stem.replace(' ', "-"),
                Spaces::Remove => new_stem.replace(' ', ""),
            };
            new_stem = rules.case.apply(&new_stem);
            let ext = if rules.lowercase_ext { ext.to_lowercase() } else { ext.to_owned() };
            format!("{}{new_stem}{}{ext}", rules.prefix, rules.suffix)
        })
        .collect()
}

fn expand(template: &str, stem: &str, number: i64, date: &str, folder: &str, ext: &str) -> String {
    // The original name goes in last so a name that looks like a token is left alone
    template
        .replace(TOKEN_NUM_3, &format!("{number:03}"))
        .replace(TOKEN_NUM_2, &format!("{number:02}"))
        .replace(TOKEN_NUM_1, &number.to_string())
        .replace(TOKEN_DATE, date)
        .replace(TOKEN_FOLDER, folder)
        .replace(TOKEN_EXT, ext)
        .replace(TOKEN_NAME, stem)
}

/// Why the result cannot be applied, if it cannot. Looks at names only, not at the disk.
pub fn problem(entries: &[Entry], names: &[String]) -> Option<String> {
    let mut seen = HashSet::new();
    for (entry, new) in entries.iter().zip(names) {
        let trimmed = new.trim();
        if trimmed.is_empty() || trimmed == "." || trimmed == ".." || trimmed.contains('/') {
            return Some(tr("Some names would be empty or invalid").to_owned());
        }
        let dir = entry.path.parent().map(Path::to_path_buf).unwrap_or_default();
        if !seen.insert((dir, new.clone())) {
            return Some(trf("More than one file would be named “{}”", &[&new]));
        }
    }
    // A target that already exists on disk is fine only when that file is renamed too
    let renamed: HashSet<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
    for (entry, new) in entries.iter().zip(names) {
        let target = entry.path.with_file_name(new);
        if target != entry.path && target.symlink_metadata().is_ok() && !renamed.contains(target.as_path()) {
            return Some(trf("“{}” already exists", &[&new]));
        }
    }
    None
}

/// An item to rename: where it is, and the path it is to get.
type Change = (PathBuf, PathBuf);

/// What `apply_with` did.
#[derive(Debug, Default)]
pub struct Applied {
    /// The paths that were renamed (old, new).
    pub done: Vec<(PathBuf, PathBuf)>,
    /// The first error, in words.
    pub error: Option<String>,
    /// The batch was called off (see `apply_with` for what that leaves).
    pub cancelled: bool,
}

/// Applies the names where they are: the paths that were renamed (old, new) and the first error, if any.
pub fn apply(entries: &[Entry], names: &[String]) -> (Vec<(PathBuf, PathBuf)>, Option<String>) {
    let applied = apply_with(entries, names, &crate::compat::rename_noreplace, &|| false, &|_, _| {});
    (applied.done, applied.error)
}

/// Applies the names, one `rename` for an item, except where names overlap: an item whose new name is the old name
/// of another item of the batch (or the other way round: names that swap, or follow each other in a chain) goes by a
/// temporary name in between, so that no name is ever taken, or replaced, on the way.
///
/// 1. The overlapping items are moved aside to temporary names. Called off here (`cancelled`) or failed, all of
///    them are put back and nothing has changed.
/// 2. The temporary names are given their new names. This is not called off: it runs to the end, so that no
///    temporary name is left. An item that cannot take its new name (it was taken meanwhile) gets its old name back;
///    the others stay renamed.
/// 3. The other items are renamed. Called off between two of them, the rest stays as it was.
///
/// `progress` is told how many of the items have their new name, and how many there are, after each one.
pub fn apply_with(
    entries: &[Entry],
    names: &[String],
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(usize, usize),
) -> Applied {
    use std::collections::HashSet;
    let changes: Vec<(PathBuf, PathBuf)> = entries
        .iter()
        .zip(names)
        .filter(|(e, n)| e.path.file_name().is_some_and(|old| old.to_string_lossy() != n.as_str()))
        .map(|(e, n)| (e.path.clone(), e.path.with_file_name(n.as_str())))
        .collect();
    let olds: HashSet<&Path> = changes.iter().map(|(old, _)| old.as_path()).collect();
    let targets: HashSet<&Path> = changes.iter().map(|(_, target)| target.as_path()).collect();
    let (overlapping, alone): (Vec<&Change>, Vec<&Change>) = changes.iter().partition(|(old, target)| olds.contains(target.as_path()) || targets.contains(old.as_path()));
    let total = changes.len();
    let mut out = Applied::default();

    // Stage 1: aside
    let mut parked: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new(); // old, temporary, new
    let put_back = |parked: Vec<(PathBuf, PathBuf, PathBuf)>| {
        for (old, temp, _) in parked.into_iter().rev() {
            let _ = crate::compat::rename_noreplace(&temp, &old);
        }
    };
    for (i, (old, target)) in overlapping.iter().enumerate() {
        if cancelled() {
            put_back(parked);
            out.cancelled = true;
            return out;
        }
        let temp = old.with_file_name(format!(".compression-rename-{}-{i}", std::process::id()));
        if let Err(err) = rename(old, &temp) {
            put_back(parked);
            out.error = Some(crate::fs::describe::io_error(&err));
            return out;
        }
        parked.push((old.clone(), temp, target.clone()));
    }
    // Stage 2: to the end
    for (old, temp, target) in parked {
        match rename(&temp, &target) {
            Ok(()) => out.done.push((old, target)),
            Err(err) => {
                // Back to its old name, if that is still free: another item of the batch may have taken it by now, and
                // what is there is never replaced. Then the item stays under its temporary name, and the error says so.
                let why = match crate::compat::rename_noreplace(&temp, &old) {
                    Ok(()) => crate::fs::describe::io_error(&err),
                    Err(_) => crate::fs::describe::failed(&temp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), &err),
                };
                out.error.get_or_insert(why);
            }
        }
        progress(out.done.len(), total);
    }
    // The items that overlap with nobody
    for (old, target) in alone {
        if cancelled() {
            out.cancelled = true;
            break;
        }
        match rename(old, target) {
            Ok(()) => out.done.push((old.clone(), target.clone())),
            Err(err) => {
                out.error.get_or_insert(crate::fs::describe::io_error(&err));
            }
        }
        progress(out.done.len(), total);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_noreplace_support_a_batch_is_refused_and_every_name_is_put_back() {
        let dir = std::env::temp_dir().join(format!("compression_batch_fb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        crate::compat::tests::without_support_under(&dir);
        for (n, c) in [("one", "1"), ("two", "2")] {
            std::fs::write(dir.join(n), c).unwrap();
        }
        let entry = |n: &str, is_dir| Entry { path: dir.join(n), is_dir, date: String::new() };
        let entries = [entry("one", false), entry("two", false), entry("folder", true)];
        let names = ["uno".to_owned(), "dos".to_owned(), "dir".to_owned()];
        let (done, failure) = apply(&entries, &names);
        assert!(done.is_empty(), "{done:?}");
        assert!(failure.is_some_and(|f| f.contains("does not support moving or renaming")));
        assert_eq!(std::fs::read_to_string(dir.join("one")).unwrap(), "1");
        assert_eq!(std::fs::read_to_string(dir.join("two")).unwrap(), "2");
        assert!(dir.join("folder").is_dir());
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, vec!["folder", "one", "two"], "no staging name is left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn entry(path: &str) -> Entry {
        Entry { path: PathBuf::from(path), is_dir: false, date: "2026-01-02".to_owned() }
    }

    fn rules(template: &str) -> Rules {
        Rules { template: template.to_owned(), ..Rules::new() }
    }

    #[test]
    fn template_keeps_extension_and_numbers_files() {
        let entries = [entry("/d/a.pdf"), entry("/d/b.tar.gz"), entry("/d/c")];
        let names = preview(&entries, &rules("Trip [01, 02, 03] [Original name]"));
        assert_eq!(names, ["Trip 01 a.pdf", "Trip 02 b.tar.gz", "Trip 03 c"]);
    }

    #[test]
    fn plain_numbers_and_date() {
        let entries = [entry("/d/a.txt"), entry("/d/b.txt")];
        assert_eq!(preview(&entries, &rules("[Modified date]-[1, 2, 3]")), ["2026-01-02-1.txt", "2026-01-02-2.txt"]);
        assert_eq!(preview(&entries, &rules("x[001, 002, 003]")), ["x001.txt", "x002.txt"]);
    }

    #[test]
    fn numbering_start_step_and_order() {
        let entries = [entry("/d/b.txt"), entry("/d/a.txt")];
        let r = Rules { template: "n[01, 02, 03]".to_owned(), start: "10".to_owned(), step: "5".to_owned(), order: Order::Name, ..Rules::new() };
        assert_eq!(preview(&entries, &r), ["n15.txt", "n10.txt"]);
    }

    #[test]
    fn case_spaces_trim_prefix_and_extension() {
        let entries = [entry("/d/hello big World.JPG")];
        let r = Rules { case: Case::Title, spaces: Spaces::Underscore, prefix: "p_".to_owned(), lowercase_ext: true, ..Rules::new() };
        assert_eq!(preview(&entries, &r), ["p_Hello_Big_World.jpg"]);
        let r = Rules { trim_start: "6".to_owned(), trim_end: "2".to_owned(), suffix: "!".to_owned(), ..Rules::new() };
        assert_eq!(preview(&entries, &r), ["big Wor!.JPG"]);
        let r = Rules { case: Case::Sentence, ..Rules::new() };
        assert_eq!(preview(&entries, &r), ["Hello big world.JPG"]);
    }

    #[test]
    fn folder_and_extension_tokens() {
        let entries = [entry("/d/photos/a.png")];
        assert_eq!(preview(&entries, &rules("[Parent folder]-[Original name]-[Extension]")), ["photos-a-png.png"]);
    }

    #[test]
    fn case_insensitive_replace() {
        let entries = [entry("/d/Foo foo FOO.txt")];
        let r = Rules { mode: Mode::Replace, find: "foo".to_owned(), replace: "x".to_owned(), match_case: false, ..Rules::new() };
        assert_eq!(preview(&entries, &r), ["x x x.txt"]);
    }

    #[test]
    fn find_and_replace_leaves_the_extension_alone() {
        let entries = [entry("/d/IMG_1.jpg"), entry("/d/jpg_notes.txt")];
        let rules = Rules { mode: Mode::Replace, find: "jpg".to_owned(), replace: "pic".to_owned(), ..Rules::new() };
        assert_eq!(preview(&entries, &rules), ["IMG_1.jpg", "pic_notes.txt"]);
        let empty = Rules { mode: Mode::Replace, ..Rules::new() };
        assert_eq!(preview(&entries, &empty), ["IMG_1.jpg", "jpg_notes.txt"]);
    }

    #[test]
    fn problems_are_reported() {
        let entries = [entry("/d/a.txt"), entry("/d/b.txt")];
        assert!(problem(&entries, &["x.txt".into(), "x.txt".into()]).is_some());
        assert!(problem(&entries, &["".into(), "y.txt".into()]).is_some());
        assert!(problem(&entries, &["a/b".into(), "y.txt".into()]).is_some());
        assert!(problem(&entries, &["b.txt".into(), "a.txt".into()]).is_none());
    }

    #[test]
    fn apply_renames_and_swaps() {
        let dir = std::env::temp_dir().join(format!("compression-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        std::fs::write(dir.join("b.txt"), "B").unwrap();
        let entries = [
            Entry { path: dir.join("a.txt"), is_dir: false, date: String::new() },
            Entry { path: dir.join("b.txt"), is_dir: false, date: String::new() },
        ];
        let names = vec!["b.txt".to_owned(), "a.txt".to_owned()];
        assert!(problem(&entries, &names).is_none());
        let (done, err) = apply(&entries, &names);
        assert!(err.is_none());
        assert_eq!(done.len(), 2);
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "B");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_whose_lowercase_form_has_other_positions_does_not_panic() {
        // "İ" (2 bytes) lowercases to 3 bytes and the Kelvin sign U+212A (3 bytes) to the letter k (1 byte):
        // the text and its lowercase form are both 8 bytes long, but a match in one is not at the place it
        // is in the other
        let text = "İaİ\u{212A}";
        // Used to cut the text inside a letter (a panic); now the exact match counts, and finds nothing
        assert_eq!(replace_text(text, "k", "x", false), text);
        // Used to cut it at the wrong place; now the exact match counts
        assert_eq!(replace_text(text, "a", "x", false), "İxİ\u{212A}");
        // A plain case-insensitive replace still works
        assert_eq!(replace_text("Photo PHOTO photo", "photo", "x", false), "x x x");
    }

    #[test]
    fn a_huge_start_or_step_does_not_overflow() {
        let entries = [entry("/d/a.txt"), entry("/d/b.txt"), entry("/d/c.txt")];
        let r = Rules { template: "[1, 2, 3]".to_owned(), step: i64::MAX.to_string(), start: i64::MAX.to_string(), ..Rules::new() };
        let names = preview(&entries, &r);
        assert_eq!(names.len(), 3);
        assert_eq!(names[2], format!("{}.txt", i64::MAX));
    }

    #[test]
    fn applying_never_replaces_a_file_that_is_in_the_way() {
        let dir = std::env::temp_dir().join(format!("compression_batch_noreplace_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (n, text) in [("a.txt", "a"), ("b.txt", "b"), ("c.txt", "not in the batch")] {
            std::fs::write(dir.join(n), text).unwrap();
        }
        let entry = |n: &str| Entry { path: dir.join(n), is_dir: false, date: String::new() };
        // c.txt appeared after the dialog checked: a.txt must not take its name
        let (done, failure) = apply(&[entry("a.txt"), entry("b.txt")], &["c.txt".to_owned(), "d.txt".to_owned()]);
        assert!(failure.is_some(), "the clash must be reported");
        assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "not in the batch");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a", "the file is put back");
        // What did not clash was renamed
        assert_eq!(done, vec![(dir.join("b.txt"), dir.join("d.txt"))]);
        assert_eq!(std::fs::read_to_string(dir.join("d.txt")).unwrap(), "b");

        // Swapping two names of the batch still works
        std::fs::write(dir.join("x"), "x").unwrap();
        std::fs::write(dir.join("y"), "y").unwrap();
        let (done, failure) = apply(&[entry("x"), entry("y")], &["y".to_owned(), "x".to_owned()]);
        assert!(failure.is_none(), "{failure:?}");
        assert_eq!(done.len(), 2);
        assert_eq!(std::fs::read_to_string(dir.join("x")).unwrap(), "y");
        assert_eq!(std::fs::read_to_string(dir.join("y")).unwrap(), "x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_clash_found_while_applying_is_reported_in_plain_words() {
        let dir = std::env::temp_dir().join(format!("compression_batch_words_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("c.txt"), "c").unwrap();
        let entry = Entry { path: dir.join("a.txt"), is_dir: false, date: String::new() };
        let (done, failure) = apply(&[entry], &["c.txt".to_owned()]);
        assert!(done.is_empty());
        assert_eq!(failure.as_deref(), Some("Already exists"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- apply_with: which renames are made, and what is left when one fails or the batch is called off

    use std::cell::{Cell, RefCell};

    /// A folder with the files `names` (each holding its own name as content), and entries for them.
    fn rig(name: &str, names: &[&str]) -> (PathBuf, Vec<Entry>) {
        let dir = std::env::temp_dir().join(format!("compression_batch_with_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in names {
            std::fs::write(dir.join(n), n.to_uppercase()).unwrap();
        }
        let entries = names.iter().map(|n| Entry { path: dir.join(n), is_dir: false, date: String::new() }).collect();
        (dir, entries)
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    fn words(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn is_temp(path: &Path) -> bool {
        path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".compression-rename-"))
    }

    /// `apply_with` with the system's rename, a record of every rename asked for, and a way to make some fail.
    fn run(entries: &[Entry], names: &[&str], fails: impl Fn(&Path, &Path) -> bool, cancelled: &dyn Fn() -> bool) -> (Applied, Vec<(PathBuf, PathBuf)>) {
        let log = RefCell::new(Vec::new());
        let rename = |from: &Path, to: &Path| {
            log.borrow_mut().push((from.to_path_buf(), to.to_path_buf()));
            if fails(from, to) { Err(std::io::Error::from_raw_os_error(libc::EIO)) } else { crate::compat::rename_noreplace(from, to) }
        };
        let applied = apply_with(entries, &words(names), &rename, cancelled, &|_, _| {});
        (applied, log.into_inner())
    }

    #[test]
    fn two_items_that_swap_names_go_by_temporary_names_and_nothing_is_left_of_them() {
        let (dir, entries) = rig("swap", &["a", "b"]);
        let (applied, log) = run(&entries, &["b", "a"], |_, _| false, &|| false);
        assert_eq!(applied.error, None);
        assert_eq!(applied.done.len(), 2);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "B");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "A");
        assert_eq!(log.len(), 4, "two to a temporary name, two to the new name: {log:?}");
        assert_eq!(listing(&dir), ["a", "b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_chain_of_names_goes_by_temporary_names_too() {
        let (dir, entries) = rig("chain", &["a", "b"]);
        // a becomes b while b becomes c
        let (applied, log) = run(&entries, &["b", "c"], |_, _| false, &|| false);
        assert_eq!(applied.error, None);
        assert_eq!((std::fs::read_to_string(dir.join("b")).unwrap(), std::fs::read_to_string(dir.join("c")).unwrap()), ("A".into(), "B".into()));
        assert!(!dir.join("a").exists());
        assert_eq!(log.iter().filter(|(_, to)| is_temp(to)).count(), 2, "both went aside: {log:?}");
        assert_eq!(listing(&dir), ["b", "c"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn items_whose_names_overlap_with_nobody_are_renamed_once_each_and_never_by_a_temporary_name() {
        let (dir, entries) = rig("alone", &["a", "b", "c"]);
        let (applied, log) = run(&entries, &["x", "y", "c"], |_, _| false, &|| false);
        assert_eq!(applied.error, None);
        assert_eq!(applied.done, vec![(dir.join("a"), dir.join("x")), (dir.join("b"), dir.join("y"))], "c keeps its name and is not touched");
        assert_eq!(log, vec![(dir.join("a"), dir.join("x")), (dir.join("b"), dir.join("y"))], "exactly one rename for each");
        assert_eq!(listing(&dir), ["c", "x", "y"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failure_while_moving_aside_puts_everything_back_and_leaves_no_temporary_name() {
        let (dir, entries) = rig("stage1", &["a", "b", "c"]);
        // a→b, b→a: the second one cannot be moved aside; c (alone) must not have been touched either
        let (applied, _) = run(&entries, &["b", "a", "z"], |from, to| from == dir.join("b") && is_temp(to), &|| false);
        assert!(applied.done.is_empty(), "{:?}", applied.done);
        assert_eq!(applied.error.as_deref(), Some("The disk or the connection reported an error"));
        assert!(!applied.cancelled);
        assert_eq!(listing(&dir), ["a", "b", "c"]);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "B");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_item_that_cannot_take_its_new_name_gets_its_old_one_back_and_the_others_stay_renamed() {
        let (dir, entries) = rig("stage2", &["a", "b"]);
        // a→b cannot be done; b→c can. a goes back to a, b is renamed to c.
        let (applied, _) = run(&entries, &["b", "c"], |_, to| to == dir.join("b"), &|| false);
        assert_eq!(applied.done, vec![(dir.join("b"), dir.join("c"))]);
        assert!(applied.error.is_some());
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(dir.join("c")).unwrap(), "B");
        assert_eq!(listing(&dir), ["a", "c"], "no temporary name is left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_item_that_cannot_go_back_is_never_put_over_another_file_and_is_named_in_the_error() {
        let (dir, entries) = rig("stage2_taken", &["a", "b"]);
        // a→b is done (b is free once b went aside); b→c cannot be done, and b's old name now holds a's content
        let (applied, _) = run(&entries, &["b", "c"], |_, to| to == dir.join("c"), &|| false);
        assert_eq!(applied.done, vec![(dir.join("a"), dir.join("b"))]);
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "A", "what is at b must not be replaced");
        let left: Vec<String> = listing(&dir).into_iter().filter(|n| n.starts_with(".compression-rename-")).collect();
        assert_eq!(left.len(), 1, "{:?}", listing(&dir));
        assert_eq!(std::fs::read_to_string(dir.join(&left[0])).unwrap(), "B", "the file is kept, under a name the error gives");
        assert!(applied.error.as_deref().is_some_and(|e| e.contains(&left[0])), "{:?}", applied.error);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn called_off_while_moving_aside_everything_is_back_as_it_was() {
        let (dir, entries) = rig("cancel1", &["a", "b", "c"]);
        let asked = Cell::new(0);
        // Called off at the second item that is to be moved aside
        let (applied, log) = run(&entries, &["b", "c", "a"], |_, _| false, &|| {
            asked.set(asked.get() + 1);
            asked.get() == 2
        });
        assert!(applied.cancelled && applied.done.is_empty() && applied.error.is_none());
        assert_eq!(log.iter().filter(|(_, to)| is_temp(to)).count(), 1, "one had been moved aside");
        assert_eq!(listing(&dir), ["a", "b", "c"]);
        for n in ["a", "b", "c"] {
            assert_eq!(std::fs::read_to_string(dir.join(n)).unwrap(), n.to_uppercase());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn called_off_after_moving_aside_the_names_are_given_out_to_the_end_and_the_rest_waits() {
        let (dir, entries) = rig("cancel2", &["a", "b", "c", "d"]);
        let asked = Cell::new(0);
        // a and b swap; c and d are alone. The batch is called off from the third question on: the two that go aside
        // are asked about first (and let through), then the swap must be completed, then c is asked about
        let (applied, _) = run(&entries, &["b", "a", "x", "y"], |_, _| false, &|| {
            asked.set(asked.get() + 1);
            asked.get() > 2
        });
        assert!(applied.cancelled);
        assert_eq!(applied.done.len(), 2, "the swap is completed: {:?}", applied.done);
        assert_eq!((std::fs::read_to_string(dir.join("a")).unwrap(), std::fs::read_to_string(dir.join("b")).unwrap()), ("B".into(), "A".into()));
        assert_eq!(listing(&dir), ["a", "b", "c", "d"], "no temporary name, and c and d are as they were");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn called_off_between_two_items_that_overlap_with_nobody_the_rest_stays_as_it_was() {
        let (dir, entries) = rig("cancel3", &["a", "b", "c"]);
        let asked = Cell::new(0);
        let (applied, _) = run(&entries, &["x", "y", "z"], |_, _| false, &|| {
            asked.set(asked.get() + 1);
            asked.get() == 2
        });
        assert!(applied.cancelled);
        assert_eq!(applied.done, vec![(dir.join("a"), dir.join("x"))]);
        assert_eq!(listing(&dir), ["b", "c", "x"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_progress_counts_the_items_that_have_their_new_name() {
        let (dir, entries) = rig("progress", &["a", "b", "c"]);
        let seen = RefCell::new(Vec::new());
        let applied = apply_with(&entries, &words(&["b", "a", "z"]), &crate::compat::rename_noreplace, &|| false, &|done, total| seen.borrow_mut().push((done, total)));
        assert_eq!(applied.done.len(), 3);
        assert_eq!(seen.into_inner(), vec![(1, 3), (2, 3), (3, 3)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
