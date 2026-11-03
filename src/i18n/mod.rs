//! Interface translations. The English text is the key: `tr("Open")` returns the English string
//! unchanged, or its translation in the language chosen in Settings. Anything a catalog does not
//! cover falls back to English, so a missing entry never breaks the interface.
//!
//! * `tr(key)` – a plain label.
//! * `trf(key, args)` – a label with `{}` (next argument) or `{0}`, `{1}` (explicit argument) holes.
//! * `trn(n, one, other)` – a count: `trn(3, "{} item", "{} items")`. Catalogs store the plural
//!   forms of the `other` text separated by `|`, in the order of the language's plural rule.
//!
//! Each language's catalog is a JSON file next to this module (`ru.json`, `de.json`, …): one object
//! whose keys are the English texts and whose values are their translations. The files are built
//! into the binary and parsed the first time the language is used. Adding a language takes its JSON
//! file plus its arms in the `Language` matches below.
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    English,
    Russian,
    German,
    Spanish,
    French,
    Vietnamese,
}

impl Language {
    pub const ALL: [Language; 6] = [Language::English, Language::Russian, Language::German, Language::Spanish, Language::French, Language::Vietnamese];

    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::Russian => "ru",
            Language::German => "de",
            Language::Spanish => "es",
            Language::French => "fr",
            Language::Vietnamese => "vi",
        }
    }

    /// The language's own name, as it is listed in Settings.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::Russian => "Русский",
            Language::German => "Deutsch",
            Language::Spanish => "Español",
            Language::French => "Français",
            Language::Vietnamese => "Tiếng Việt",
        }
    }

    pub fn from_code(code: &str) -> Option<Language> {
        let code = code.trim().to_ascii_lowercase();
        let code = code.split(['_', '-', '.', '@']).next().unwrap_or("");
        Language::ALL.into_iter().find(|l| l.code() == code)
    }

    fn index(self) -> usize {
        Language::ALL.iter().position(|l| *l == self).unwrap_or(0)
    }

    /// The language's JSON catalog as built into the binary.
    fn source(self) -> &'static str {
        match self {
            Language::English => "{}",
            Language::Russian => include_str!("ru.json"),
            Language::German => include_str!("de.json"),
            Language::Spanish => include_str!("es.json"),
            Language::French => include_str!("fr.json"),
            Language::Vietnamese => include_str!("vi.json"),
        }
    }

    /// The catalog's entries in file order.
    fn catalog(self) -> Result<Vec<(String, String)>, serde_json::Error> {
        serde_json::from_str::<Entries>(self.source()).map(|e| e.0)
    }

    /// The catalog as a lookup table, parsed once. A broken file leaves the language in English.
    fn table(self) -> &'static HashMap<String, String> {
        static TABLES: [OnceLock<HashMap<String, String>>; Language::ALL.len()] = [const { OnceLock::new() }; Language::ALL.len()];
        TABLES[self.index()].get_or_init(|| match self.catalog() {
            Ok(entries) => entries.into_iter().filter(|(_, t)| !t.is_empty()).collect(),
            Err(e) => {
                eprintln!("compression: the {} translation is broken: {e}", self.code());
                HashMap::new()
            }
        })
    }

    /// Index into the `|`-separated plural forms for a count of `n`.
    fn plural_form(self, n: usize) -> usize {
        match self {
            Language::English | Language::German | Language::Spanish => usize::from(n != 1),
            Language::French => usize::from(n > 1),
            Language::Vietnamese => 0,
            Language::Russian => {
                if n % 10 == 1 && n % 100 != 11 {
                    0
                } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
                    1
                } else {
                    2
                }
            }
        }
    }
}

/// A catalog's entries as a list. Reading the JSON object straight into a map would quietly keep only
/// the last of two equal keys; the list lets the tests catch duplicates.
struct Entries(Vec<(String, String)>);

impl<'de> serde::Deserialize<'de> for Entries {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Entries;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object mapping English texts to their translations")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Entries, A::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_language(language: Language) {
    CURRENT.store(language.index() as u8, Ordering::Relaxed);
}

pub fn language() -> Language {
    Language::ALL[(CURRENT.load(Ordering::Relaxed) as usize).min(Language::ALL.len() - 1)]
}

/// The language of the desktop session (`LC_ALL`, `LC_MESSAGES`, `LANG`), English when unknown.
pub fn system_language() -> Language {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .and_then(|v| Language::from_code(&v))
        .unwrap_or(Language::English)
}

/// The language a settings value stands for: `system` follows the desktop, otherwise a code.
pub fn resolve(setting: &str) -> Language {
    if setting == "system" { system_language() } else { Language::from_code(setting).unwrap_or(Language::English) }
}

fn lookup(key: &str) -> Option<&'static str> {
    match language() {
        Language::English => None,
        lang => lang.table().get(key).map(String::as_str),
    }
}

/// Translate a text that is only known at run time (a place name stored as English). Unknown
/// texts, such as the name of a drive, come back unchanged.
pub fn tr_dyn(text: &str) -> String {
    lookup(text).unwrap_or(text).to_owned()
}

/// Translate a label.
pub fn tr(key: &'static str) -> &'static str {
    lookup(key).unwrap_or(key)
}

/// Fill the holes of a (translated) template: `{}` takes the next argument, `{n}` the n-th.
pub fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if after[..end].chars().all(|c| c.is_ascii_digit()) => {
                let index = if end == 0 {
                    next += 1;
                    next - 1
                } else {
                    after[..end].parse().unwrap_or(0)
                };
                if let Some(arg) = args.get(index) {
                    out.push_str(&arg.to_string());
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Translate a label that has holes and fill them.
pub fn trf(key: &'static str, args: &[&dyn Display]) -> String {
    fill(tr(key), args)
}

/// A count with the right plural form: `trn(n, "{} item", "{} items")`.
pub fn trn(n: usize, one: &'static str, other: &'static str) -> String {
    let lang = language();
    let template = match lookup(other) {
        Some(forms) => {
            let forms: Vec<&str> = forms.split('|').collect();
            forms.get(lang.plural_form(n)).or(forms.last()).copied().unwrap_or(other)
        }
        None if n == 1 => one,
        None => other,
    };
    fill(template, &[&n])
}

