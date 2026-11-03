//! What is inside an archive, for the Quick Look preview. Nothing is written to disk.
//!
//! Zip is read from its central directory (instant). Tar archives are streamed, through `xz`,
//! `bzip2` or `zstd` when compressed that way, and the listing stops after `MAX_ENTRIES` entries or
//! `TIME_LIMIT`, since a tar has no index and reading all of a big one means unpacking it all.
//! Everything else (7z, rar, iso, deb, rpm, ...) is listed by `7z l -slt`. Names inside an archive
//! are untrusted: `sanitize` strips control characters before anything is drawn.
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::ops;

pub const MAX_ENTRIES: usize = 2000;
pub const TIME_LIMIT: Duration = Duration::from_secs(10);
const MAX_NAME: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    Entries,
    Time,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<ArchiveEntry>,
    /// Set when the listing stopped early.
    pub cut: Option<Cut>,
    /// Size of the archive file.
    pub packed: u64,
    /// Sum of the sizes of the entries listed.
    pub unpacked: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The program that reads this kind of archive is not installed.
    NoTool(&'static str),
    Other(String),
}

/// Control and direction-changing characters out, length capped.
pub fn sanitize(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .collect();
    if clean.chars().count() > MAX_NAME {
        format!("{}…", clean.chars().take(MAX_NAME).collect::<String>())
    } else {
        clean
    }
}

/// Whether the archive holds several files that can be listed: a lone compressed file (`log.gz`)
/// has no tree to show.
pub fn is_listable(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let single = [".gz", ".xz", ".bz2", ".zst"].iter().any(|e| name.ends_with(e)) && !name.contains(".tar.");
    ops::is_archive(path) && !single
}

pub fn list(path: &Path, cancel: &AtomicBool) -> Result<Listing, Failure> {
    list_capped(path, MAX_ENTRIES, TIME_LIMIT, cancel)
}

pub fn list_capped(path: &Path, cap: usize, limit: Duration, cancel: &AtomicBool) -> Result<Listing, Failure> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ends = |exts: &[&str]| exts.iter().any(|e| name.ends_with(e));
    let deadline = Instant::now() + limit;
    let fail = |e: io::Error| Failure::Other(e.to_string());
    let mut listing = if ends(&[".zip", ".jar", ".apk"]) {
        list_zip(path, cap).map_err(fail)?
    } else if ends(&[".tar.gz", ".tgz"]) {
        let file = std::fs::File::open(path).map_err(fail)?;
        list_tar(flate2::read::GzDecoder::new(file), cap, deadline, cancel).map_err(fail)?
    } else if ends(&[".tar"]) {
        list_tar(std::fs::File::open(path).map_err(fail)?, cap, deadline, cancel).map_err(fail)?
    } else if let Some(tool) = ends(&[".tar.xz", ".txz"]).then_some("xz").or_else(|| ends(&[".tar.bz2", ".tbz2"]).then_some("bzip2")).or_else(|| ends(&[".tar.zst", ".tzst"]).then_some("zstd")) {
        if !ops::have(tool) {
            return Err(Failure::NoTool(tool));
        }
        let mut child = Command::new(tool).arg("-dc").arg("--").arg(path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(fail)?;
        let out = child.stdout.take().ok_or_else(|| Failure::Other("no output".into()))?;
        let result = list_tar(out, cap, deadline, cancel);
        let _ = child.kill();
        let _ = child.wait();
        result.map_err(fail)?
    } else {
        list_7z(path, cap, deadline, cancel)?
    };
    listing.packed = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    listing.unpacked = listing.entries.iter().fold(0u64, |sum, e| sum.saturating_add(e.size));
    Ok(listing)
}

fn list_zip(path: &Path, cap: usize) -> io::Result<Listing> {
    let mut zip = zip::ZipArchive::new(BufReader::new(std::fs::File::open(path)?)).map_err(io::Error::other)?;
    let mut listing = Listing::default();
    for i in 0..zip.len() {
        if listing.entries.len() >= cap {
            listing.cut = Some(Cut::Entries);
            break;
        }
        let file = zip.by_index_raw(i).map_err(io::Error::other)?;
        listing.entries.push(ArchiveEntry { path: sanitize(&file.name().map(|n| n.replace('\\', "/")).unwrap_or_default()), is_dir: file.is_dir(), size: file.size(), encrypted: file.encrypted() });
    }
    Ok(listing)
}

/// A reader that fails once the time is up or the preview was abandoned.
struct Limited<'a, R> {
    inner: R,
    deadline: Instant,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for Limited<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if Instant::now() > self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "time limit"));
        }
        self.inner.read(buf)
    }
}

fn list_tar<R: Read>(reader: R, cap: usize, deadline: Instant, cancel: &AtomicBool) -> io::Result<Listing> {
    let mut archive = tar::Archive::new(Limited { inner: reader, deadline, cancel });
    let mut listing = Listing::default();
    for entry in archive.entries()? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::TimedOut && !listing.entries.is_empty() => {
                listing.cut = Some(Cut::Time);
                break;
            }
            Err(e) => return Err(e),
        };
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() || kind.is_gnu_longname() || kind.is_gnu_longlink() {
            continue;
        }
        if listing.entries.len() >= cap {
            listing.cut = Some(Cut::Entries);
            break;
        }
        listing.entries.push(ArchiveEntry { path: sanitize(&String::from_utf8_lossy(&entry.path_bytes())), is_dir: kind.is_dir(), size: entry.size(), encrypted: false });
    }
    Ok(listing)
}

/// Builds entries from the text of `7z l -slt`.
#[derive(Default)]
struct SevenZip {
    started: bool,
    fields: HashMap<String, String>,
    entries: Vec<ArchiveEntry>,
}

impl SevenZip {
    fn feed(&mut self, line: &str) {
        if !self.started {
            self.started = line.starts_with("----------");
            return;
        }
        if line.trim().is_empty() {
            self.flush();
        } else if let Some((key, value)) = line.split_once(" = ") {
            self.fields.insert(key.to_owned(), value.to_owned());
        }
    }

    fn flush(&mut self) {
        let fields = std::mem::take(&mut self.fields);
        if let Some(path) = fields.get("Path") {
            let is_dir = fields.get("Folder").is_some_and(|f| f == "+") || fields.get("Attributes").is_some_and(|a| a.starts_with('D'));
            self.entries.push(ArchiveEntry {
                path: sanitize(path),
                is_dir,
                size: fields.get("Size").and_then(|s| s.trim().parse().ok()).unwrap_or(0),
                encrypted: fields.get("Encrypted").is_some_and(|e| e == "+"),
            });
        }
    }
}

fn list_7z(path: &Path, cap: usize, deadline: Instant, cancel: &AtomicBool) -> Result<Listing, Failure> {
    if !(ops::have("7z") || ops::have("7za")) {
        return Err(Failure::NoTool("7z"));
    }
    let mut child = Command::new(ops::seven_zip())
        .args(["l", "-slt", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Failure::Other(e.to_string()))?;
    let out = child.stdout.take().ok_or_else(|| Failure::Other("no output".into()))?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(out).split(b'\n').map_while(Result::ok) {
            if tx.send(String::from_utf8_lossy(&line).trim_end_matches('\r').to_owned()).is_err() {
                break;
            }
        }
    });
    let (mut parser, mut cut, mut all_lines) = (SevenZip::default(), None, Vec::new());
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if Instant::now() > deadline {
            cut = Some(Cut::Time);
            break;
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if !parser.started && all_lines.len() < 20 {
                    all_lines.push(line.clone());
                }
                parser.feed(&line);
                if parser.entries.len() > cap {
                    cut = Some(Cut::Entries);
                    parser.entries.truncate(cap);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                parser.flush();
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    if parser.entries.is_empty() && cut.is_none() {
        let reason = all_lines.iter().rev().find(|l| l.contains("ERROR") || l.contains("Can not")).cloned().unwrap_or_else(|| "nothing to list".to_owned());
        return Err(Failure::Other(sanitize(&reason)));
    }
    Ok(Listing { entries: parser.entries, cut, ..Listing::default() })
}

/// The entries as a tree for display: nodes in display order (folders first, by name), each with
/// its depth; folders carry the sum of what is below them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub size: u64,
    /// Files in the subtree (1 for a file).
    pub files: usize,
    pub encrypted: bool,
    /// Index of the node after this one's subtree.
    pub end: usize,
}

pub fn tree(entries: &[ArchiveEntry]) -> Vec<Node> {
    struct Raw {
        name: String,
        is_dir: bool,
        size: u64,
        encrypted: bool,
        children: Vec<usize>,
    }
    let mut raw = vec![Raw { name: String::new(), is_dir: true, size: 0, encrypted: false, children: Vec::new() }];
    let mut lookup: HashMap<(usize, String), usize> = HashMap::new();
    for entry in entries {
        let parts: Vec<&str> = entry.path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        let mut at = 0;
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let id = *lookup.entry((at, (*part).to_owned())).or_insert_with(|| {
                raw.push(Raw { name: (*part).to_owned(), is_dir: !last || entry.is_dir, size: 0, encrypted: false, children: Vec::new() });
                raw.len() - 1
            });
            if lookup.get(&(at, (*part).to_owned())) == Some(&id) && !raw[at].children.contains(&id) {
                raw[at].children.push(id);
            }
            if !last {
                raw[id].is_dir = true;
            } else if !entry.is_dir {
                raw[id].size = entry.size;
                raw[id].encrypted = entry.encrypted;
            }
            at = id;
        }
    }
    let mut files = vec![0usize; raw.len()];
    // Children always come after their parent, so sums run from the end
    let parent_of: HashMap<usize, usize> = raw.iter().enumerate().flat_map(|(p, r)| r.children.iter().map(move |c| (*c, p))).collect();
    for id in (1..raw.len()).rev() {
        if !raw[id].is_dir {
            files[id] += 1;
        }
        if let Some(&p) = parent_of.get(&id) {
            let (size, count) = (raw[id].size, files[id]);
            raw[p].size = raw[p].size.saturating_add(size);
            files[p] += count;
        }
    }
    let mut out = Vec::new();
    fn walk(raw: &[Raw], files: &[usize], id: usize, depth: usize, out: &mut Vec<Node>) {
        let mut kids = raw[id].children.clone();
        kids.sort_by(|a, b| (!raw[*a].is_dir, &raw[*a].name).cmp(&(!raw[*b].is_dir, &raw[*b].name)));
        for kid in kids {
            let at = out.len();
            out.push(Node { name: raw[kid].name.clone(), depth, is_dir: raw[kid].is_dir, size: raw[kid].size, files: files[kid], encrypted: raw[kid].encrypted, end: 0 });
            walk(raw, files, kid, depth + 1, out);
            out[at].end = out.len();
        }
    }
    walk(&raw, &files, 0, 0, &mut out);
    out
}

