//! Servers in the window: the Network section of the sidebar, the Connect to server, login and
//! question dialogs, and connections that run in the background until their folder can be shown.
use super::*;
use crate::fs::mount_health::{self, Health, Probes};
use crate::fs::network::{self, Address, Mount, MountKind, Progress, Prompt, Reply, Server, Servers};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(4);

/// Where a folder goes once its server is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenIn {
    Here,
    NewTab,
    NewWindow,
}

/// Where the windows were before `leave_mount` took them out of a mount, to bring them back when the server
/// could not be let go of.
#[derive(Clone, Default)]
pub(super) struct Left {
    /// The folder of the tab on screen, when it was inside the mount.
    current: Option<PathBuf>,
    /// The parked tabs that were inside it: `(tab id, folder)`.
    tabs: Vec<(u64, PathBuf)>,
}

pub(super) enum NetMsg {
    Listed(Vec<Mount>),
    Ask { id: u64, prompt: Prompt },
    Done { id: u64, result: Result<PathBuf, Option<String>> },
    Gone { local: PathBuf, name: String, left: Left, result: Result<(), String> },
    Forgot { name: String, result: Result<(), String> },
    /// A mount that was let go of to be connected again.
    /// A server's folder was asked about: `local` is it when it is there.
    Checked { id: u64, local: Option<PathBuf>, is_dir: bool, next: Next },
    /// An item on a server was renamed (or not): `path` is the old path.
    /// `item` is the renamed item as the thread read it, so that the window need not look at the server.
    Renamed { path: PathBuf, target: PathBuf, new_name: String, item: Option<FileItem>, result: Result<(), String> },
    Reconnect { local: PathBuf, name: String, address: Address, back: Option<PathBuf>, tabs: Vec<(u64, PathBuf)>, left: Left, result: Result<(), String> },
}

/// A connection on its way.
struct Pending {
    id: u64,
    address: Address,
    open: OpenIn,
    replies: mpsc::Sender<Reply>,
    /// A login or question dialog waits for the person.
    asking: bool,
    /// Started from the Connect to server dialog, which comes back with the error if it fails.
    from_dialog: bool,
    /// Add the server to the sidebar once connected.
    save: bool,
    /// A folder below the top of the mount to show instead of the top, if it is there: where the window
    /// was before the server was connected again.
    then: Option<PathBuf>,
    /// The same for the tabs that were parked in the mount: `(tab id, folder below the top)`.
    tabs: Vec<(u64, PathBuf)>,
    /// Other requests for the same mount that came while it was connecting (another folder, a new tab or window):
    /// they are carried out once it is there, one connection serving them all.
    later: Vec<(Address, OpenIn)>,
}

/// How an item on a server is renamed; the tests put a slow one here.
#[cfg(test)]
type Renamer = Arc<dyn Fn(&Path, &Path) -> std::io::Result<()> + Send + Sync>;

pub(super) struct NetState {
    pub(super) tx: mpsc::Sender<NetMsg>,
    rx: mpsc::Receiver<NetMsg>,
    pub(super) servers: Servers,
    /// Why the saved servers started empty, until the window has said so.
    pub(super) servers_problem: Option<network::LoadProblem>,
    pub(super) mounts: Vec<Mount>,
    pending: Vec<Pending>,
    checking: Vec<Check>,
    /// The number of each tab's latest navigation: an answer that carries an older one is out of date.
    navs: HashMap<u64, u64>,
    /// Which connection of each mount folder is the current one: counted up whenever it is connected, listed as
    /// gone or let go of. An answer from an earlier connection is out of date.
    generations: HashMap<PathBuf, u64>,
    /// What a check does on its thread; the tests put a slow one here.
    ask: fn(&Next) -> (Option<PathBuf>, bool),
    next_id: u64,
    listing: bool,
    last: Instant,
    /// Mount folders being disconnected.
    pub(super) leaving: HashSet<PathBuf>,
    /// Whether the connected servers still answer.
    probes: Probes,
    /// Logins and questions of connections that wait for the window to have room for them: no dialog is shown over
    /// another one, since the one underneath (a question of a copy, a name being typed) would be lost.
    waiting_prompts: VecDeque<(u64, Prompt)>,
    /// Renames that have been asked of a server and not answered: the old path, and the one it is to get.
    pub(super) renaming: HashMap<PathBuf, PathBuf>,
    /// What the window knows about paths on servers without having to ask in a frame.
    pub(super) known: Known,
    #[cfg(test)]
    rename: Renamer,
}

impl NetState {
    pub(super) fn connection_changed(&mut self, mount: &Path) {
        *self.generations.entry(mount.to_path_buf()).or_insert(0) += 1;
    }

    pub(super) fn is_being_renamed(&self, path: &Path) -> bool {
        self.renaming.contains_key(path)
    }

    fn generation_of(&self, mount: &Option<PathBuf>) -> u64 {
        mount.as_ref().and_then(|m| self.generations.get(m)).copied().unwrap_or(0)
    }

    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let servers = Servers::load();
        Self { tx, rx, servers: servers.0, servers_problem: servers.1, mounts: Vec::new(), pending: Vec::new(), checking: Vec::new(), navs: HashMap::new(), generations: HashMap::new(), ask: ask_server, next_id: 1, listing: false, last: Instant::now(), leaving: HashSet::new(), probes: Probes::new(), waiting_prompts: VecDeque::new(), renaming: HashMap::new(), known: Known::default(), #[cfg(test)] rename: Arc::new(crate::compat::rename_noreplace) }
    }
}

/// What the dot in front of a server in the sidebar says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkState {
    Connected,
    /// The server answers slowly, or not yet.
    Slow,
    /// The mount is there but answers with an error.
    Broken,
    /// A saved server that is not connected: normal, not a fault.
    Offline,
}

impl LinkState {
    /// The state of a row: `None` for what is no server (a phone, a camera).
    pub(super) fn of(mounted: bool, has_address: bool, health: Health) -> Option<LinkState> {
        match (mounted, has_address, health) {
            (false, _, _) => Some(LinkState::Offline),
            (true, false, _) => None,
            (true, true, Health::Connected) => Some(LinkState::Connected),
            (true, true, Health::Slow) => Some(LinkState::Slow),
            (true, true, Health::Broken) => Some(LinkState::Broken),
        }
    }

    /// The words of the tooltip, so the state is not told by color alone.
    pub(super) fn words(self) -> &'static str {
        match self {
            LinkState::Connected => tr("Connected"),
            LinkState::Slow => tr("Answering slowly"),
            LinkState::Broken => tr("Not answering"),
            LinkState::Offline => tr("Not connected"),
        }
    }

    /// The color of the dot, and whether it is filled; a ring for a server that is just not connected.
    pub(super) fn look(self, theme: &SerpantinumTheme) -> (egui::Color32, bool) {
        match self {
            LinkState::Connected => (theme.green, true),
            LinkState::Slow => (theme.yellow, true),
            LinkState::Broken => (theme.red, true),
            LinkState::Offline => (theme.subtext0, false),
        }
    }

    pub(super) fn paint(self, ui: &egui::Ui, row: egui::Rect, theme: &SerpantinumTheme) {
        let center = egui::pos2(row.left() + material::INSET_ICON + 8.0, row.center().y + 8.0);
        let (color, filled) = self.look(theme);
        let painter = ui.painter();
        // A rim in the sidebar's color keeps the dot apart from the icon
        painter.circle_filled(center, 5.5, theme.crust);
        if filled {
            painter.circle_filled(center, 3.5, color);
        } else {
            painter.circle_stroke(center, 3.0, egui::Stroke::new(1.5, color));
        }
    }
}

/// Whether a server's menu offers to connect it again: only one that is slow or broken, and only one that
/// Compression can let go of itself (not a mount of the system's, which takes more rights).
pub(super) fn offers_reconnect(mount: &Mount, state: Option<LinkState>) -> bool {
    mount.address.is_some() && mount.kind != MountKind::System && matches!(state, Some(LinkState::Slow | LinkState::Broken))
}

/// What a path on a server is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Dir,
    Other,
    Missing,
}

/// The most the window waits, right after a server connected, for it to say what its paths are.
const ANSWER_WAIT: Duration = Duration::from_millis(500);

/// How long a click on a server may go without an answer before the window says "Not answering". Nothing waits
/// for it: the question is asked on a thread of its own and the window carries on; this is only when to tell.
/// A thread whose server hangs stays in its call until the server answers or the connection breaks, and what
/// it learns then is used to refresh the sidebar, not to open anything the person has stopped waiting for.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// How long what a server said about a path is trusted before it is asked again (the old answer is still used meanwhile).
const KNOWN_FOR: Duration = Duration::from_secs(30);

/// What paths on servers are, for what the window draws every frame (the pinned folders, a drop target): a frame
/// must not ask a server, which may hang. An answer is looked up here; if there is none, or it is old, one question
/// goes out on a thread of its own and the window repaints when it comes back. At most one question is on its way
/// per mount, so a server that hangs holds one thread, not one per frame.
#[cfg_attr(not(test), derive(Default))]
pub(super) struct Known {
    state: Arc<Mutex<KnownState>>,
    /// What a test puts in place of the question to the file system.
    #[cfg(test)]
    ask: Arc<dyn Fn(&Path) -> Kind + Send + Sync>,
}

#[cfg(test)]
impl Default for Known {
    fn default() -> Self {
        Self::with_ask(|path| match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => Kind::Dir,
            Ok(_) => Kind::Other,
            Err(_) => Kind::Missing,
        })
    }
}

#[cfg(test)]
impl Known {
    fn with_ask(ask: impl Fn(&Path) -> Kind + Send + Sync + 'static) -> Self {
        Self { state: Arc::default(), ask: Arc::new(ask) }
    }
}

#[derive(Default)]
struct KnownState {
    kinds: HashMap<PathBuf, (Kind, Instant)>,
    /// The mounts with a question on their way.
    asking: HashSet<PathBuf>,
}

impl Known {
    /// What `path` was last said to be, if it was; asks again in the background when that is old or missing.
    pub(super) fn get(&self, path: &Path, ctx: &egui::Context) -> Option<Kind> {
        let mount = network::mount_root(path).unwrap_or_else(|| path.to_path_buf());
        let Ok(mut state) = self.state.lock() else { return None };
        let known = state.kinds.get(path).copied();
        if known.is_some_and(|(_, at)| at.elapsed() < KNOWN_FOR) || state.asking.contains(&mount) {
            return known.map(|(kind, _)| kind);
        }
        state.asking.insert(mount.clone());
        let (shared, ctx, path) = (self.state.clone(), ctx.clone(), path.to_path_buf());
        #[cfg(test)]
        let ask = self.ask.clone();
        drop(state);
        let started = std::thread::Builder::new().name("known-kind".into()).spawn(move || {
            #[cfg(test)]
            let kind = ask(&path);
            #[cfg(not(test))]
            let kind = match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => Kind::Dir,
                Ok(_) => Kind::Other,
                Err(_) => Kind::Missing,
            };
            if let Ok(mut state) = shared.lock() {
                state.kinds.insert(path, (kind, Instant::now()));
                state.asking.remove(&mount);
            }
            ctx.request_repaint();
        });
        if started.is_err() && let Ok(mut state) = self.state.lock() {
            state.asking.clear();
        }
        known.map(|(kind, _)| kind)
    }
}

/// What each of `paths` is, or `None` where the server did not say within `wait`. The asking runs on a thread of
/// its own: a server that hangs costs the window at most `wait`, not the whole time it hangs.
fn kinds(paths: &[PathBuf], wait: Duration) -> Vec<Option<Kind>> {
    kinds_with(paths, wait, |path| match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Kind::Dir,
        Ok(_) => Kind::Other,
        Err(_) => Kind::Missing,
    })
}

fn kinds_with(paths: &[PathBuf], wait: Duration, ask: impl Fn(&Path) -> Kind + Send + 'static) -> Vec<Option<Kind>> {
    let list = paths.to_vec();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (i, path) in list.iter().enumerate() {
            if tx.send((i, ask(path))).is_err() {
                break;
            }
        }
    });
    let mut kinds = vec![None; paths.len()];
    let deadline = Instant::now() + wait;
    while kinds.iter().any(Option::is_none) {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        match rx.recv_timeout(left) {
            Ok((i, kind)) => kinds[i] = Some(kind),
            Err(_) => break,
        }
    }
    kinds
}

/// What a folder check is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum How {
    /// Go there.
    Go,
    /// The folder may be gone: go to the nearest one that is left (stay if it is there).
    Nearest,
    /// A tab was opened at a path that may lead onto a server: show it as what it resolves to, without a history entry.
    Replace,
}

/// What to do once it is known whether a server's folder is there.
pub(super) enum Next {
    /// A folder, from the sidebar, the path or a tab: it is shown once a thread has said what it is. `tab` and `serial`
    /// are the tab it was asked for and that tab's navigation number then; `how` says what is wanted of it (go there,
    /// the nearest folder at or above `target` that is there, or the tab shown as what the path resolves to).
    Folder { target: PathBuf, tab: u64, serial: u64, how: How },
    Open { address: Address, password: Option<String>, open: OpenIn },
    Back { address: Address, back: Option<PathBuf>, tabs: Vec<(u64, PathBuf)> },
    Dialog { address: Address, password: Option<String>, save: bool },
}

impl Next {
    /// The folder to ask about.
    fn local(&self) -> Option<PathBuf> {
        match self {
            Next::Folder { target, .. } => Some(target.clone()),
            Next::Open { address, .. } | Next::Back { address, .. } | Next::Dialog { address, .. } => network::local_path(address),
        }
    }

    /// What two questions about the same thing have in common.
    fn key(&self) -> String {
        match self {
            Next::Folder { target, .. } => target.to_string_lossy().into_owned(),
            Next::Open { address, .. } | Next::Back { address, .. } | Next::Dialog { address, .. } => address.display(),
        }
    }
}

/// A question to a server that has not been answered yet.
struct Check {
    id: u64,
    key: String,
    /// The mount it is about: one mount has at most one question on its way at a time.
    mount: Option<PathBuf>,
    /// The tab a folder was asked for.
    tab: Option<u64>,
    /// The connection of the mount when it was asked.
    generation: u64,
    started: Instant,
    /// The window said "Not answering" and went on; an answer that comes now opens nothing.
    late: bool,
    /// Not sent yet, because another question about the same mount is still on its way.
    waiting: Option<Next>,
}

/// What became of a new question.
enum Admit {
    /// The mount did not answer an earlier one in time: say so at once instead of adding to a thread that hangs.
    Refused,
    Start(Next),
    Queued,
}

fn same_thing(c: &Check, key: &str, mount: &Option<PathBuf>) -> bool {
    c.key == key || mount.is_some() && &c.mount == mount
}

/// Register `check` for `next`. A mount has one question on its way at a time, so a hanging server costs one
/// thread, not one per click; the others wait (a tab's newer folder replaces its older one, so the waiting
/// cannot pile up beyond the tabs and dialogs there are).
fn admit(checks: &mut Vec<Check>, mut check: Check, next: Next) -> Admit {
    if checks.iter().any(|c| c.late && same_thing(c, &check.key, &check.mount)) {
        return Admit::Refused;
    }
    if let Some(tab) = check.tab {
        checks.retain(|c| !(c.waiting.is_some() && c.tab == Some(tab)));
    }
    let busy = checks.iter().any(|c| c.waiting.is_none() && same_thing(c, &check.key, &check.mount));
    if busy {
        check.waiting = Some(next);
        checks.push(check);
        Admit::Queued
    } else {
        checks.push(check);
        Admit::Start(next)
    }
}

/// When the question about `key`/`mount` is over: the next one waiting for it, if the mount is free.
fn take_waiting(checks: &mut [Check], key: &str, mount: &Option<PathBuf>) -> Option<(u64, Next)> {
    if checks.iter().any(|c| c.waiting.is_none() && same_thing(c, key, mount)) {
        return None;
    }
    let c = checks.iter_mut().find(|c| !c.late && c.waiting.is_some() && same_thing(c, key, mount))?;
    Some((c.id, c.waiting.take()?))
}

/// What the person asked for is still wanted: the tab is the one on screen and has not gone anywhere since.
fn nav_current(navs: &HashMap<u64, u64>, shown_tab: Option<u64>, tab: u64, serial: u64) -> bool {
    shown_tab == Some(tab) && navs.get(&tab) == Some(&serial)
}

/// The checks that have waited longer than `wait` and were not marked yet: they are marked now, and their ids returned.
fn overdue(checks: &mut [Check], wait: Duration) -> Vec<u64> {
    checks.iter_mut().filter(|c| !c.late && c.started.elapsed() > wait).map(|c| {
        c.late = true;
        c.id
    }).collect()
}

/// What `target` is, as the window will show it: the folder itself (or the nearest one that is there when
/// `nearest`), and whether it is a folder. Asked on a thread of its own, since each step may wait for a server.
fn resolve_folder(target: &Path, nearest: bool, canon: impl Fn(&Path) -> Option<PathBuf>, is_dir: impl Fn(&Path) -> bool) -> (Option<PathBuf>, bool) {
    if nearest {
        let mut up = target.to_path_buf();
        while !is_dir(&up) && let Some(parent) = up.parent() {
            up = parent.to_path_buf();
        }
        return (Some(up), true);
    }
    let found = canon(target).unwrap_or_else(|| target.to_path_buf());
    let dir = is_dir(&found);
    (Some(found), dir)
}

/// The real question, for the thread of a check.
fn ask_server(next: &Next) -> (Option<PathBuf>, bool) {
    match next {
        Next::Folder { target, how, .. } => resolve_folder(target, *how == How::Nearest, |p| p.canonicalize().ok(), |p| p.is_dir()),
        other => match other.local().and_then(|p| std::fs::metadata(&p).ok().map(|m| (p, m.is_dir()))) {
            Some((p, dir)) => (Some(p), dir),
            None => (None, false),
        },
    }
}

/// Ask on a thread of its own and send the answer to the window.
fn spawn_check(id: u64, next: Next, tx: mpsc::Sender<NetMsg>, ctx: egui::Context, ask: impl FnOnce(&Next) -> (Option<PathBuf>, bool) + Send + 'static) {
    std::thread::spawn(move || {
        let (local, is_dir) = ask(&next);
        let _ = tx.send(NetMsg::Checked { id, local, is_dir, next });
        ctx.request_repaint();
    });
}

/// Where each of `insides` (folders below the top of a mount) is to be shown once the server is connected
/// again: there if it is still a folder, else at `top`.
fn folders_or_top(top: &Path, insides: &[PathBuf]) -> Vec<PathBuf> {
    let wanted: Vec<PathBuf> = insides.iter().map(|inside| top.join(inside)).collect();
    kinds(&wanted, ANSWER_WAIT).into_iter().zip(wanted).map(|(kind, folder)| if kind == Some(Kind::Dir) { folder } else { top.to_path_buf() }).collect()
}

/// Where the tabs that were taken out of a mount wait.
fn home_folder() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// A row of the Network section: a saved server, a connected one, or both.
#[derive(Clone)]
pub(super) struct NetRow {
    pub(super) name: String,
    pub(super) icon: &'static str,
    pub(super) address: Option<Address>,
    /// Where a click goes while connected.
    pub(super) open: Option<PathBuf>,
    pub(super) mount: Option<Mount>,
    /// What the dot in front of the name says.
    pub(super) state: Option<LinkState>,
    /// Index into the saved servers.
    pub(super) saved: Option<usize>,
}

/// Where `address` shows inside `mount`.
fn local_in(mount: &Mount, address: &Address) -> PathBuf {
    let base = mount.address.as_ref().map_or("/", |a| a.path.as_str());
    let rest = address.path.strip_prefix(base).unwrap_or("").trim_start_matches('/');
    if rest.is_empty() { mount.local.clone() } else { mount.local.join(rest) }
}

/// What the dialogs and the sidebar asked for, carried out after drawing.
pub(super) enum NetEvent {
    Open(PathBuf),
    Connect(Address, OpenIn),
    NewTab(PathBuf),
    Disconnect(Mount),
    Menu(egui::Pos2, NetRow),
    ConnectDialog,
    /// The Connect to server entry's tick box, while the hidden places are listed.
    ShowConnect(bool),
    ConnectMenu(egui::Pos2),
}

impl CompressionApp {
    /// Apply what finished, ask for a new listing every few seconds, and cancel the questions whose dialog was dismissed.
    pub(super) fn poll_network(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.net.rx.try_recv() {
            match msg {
                NetMsg::Listed(mounts) => {
                    self.net.listing = false;
                    // A server that appeared or went away since the last listing is a different connection
                    let (before, after): (HashSet<&PathBuf>, HashSet<&PathBuf>) = (self.net.mounts.iter().map(|m| &m.local).collect(), mounts.iter().map(|m| &m.local).collect());
                    let changed: Vec<PathBuf> = before.symmetric_difference(&after).map(|m| (*m).clone()).collect();
                    for mount in changed {
                        self.net.connection_changed(&mount);
                    }
                    self.net.mounts = mounts;
                }
                NetMsg::Ask { id, prompt } => self.show_prompt(id, prompt),
                NetMsg::Done { id, result } => self.connected(ctx, id, result),
                NetMsg::Checked { id, local, is_dir, next } => self.checked(ctx, id, local, is_dir, next),
                NetMsg::Gone { local, name, left, result } => {
                    self.net.leaving.remove(&local);
                    match result {
                        Ok(()) => {
                            if self.current_path.starts_with(&local) {
                                self.navigate_known(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
                            }
                            self.set_toast(trf("Disconnected from “{}”", &[&name]));
                        }
                        Err(err) => {
                            // The server is still there: the windows go back to it
                            self.bring_back(left);
                            self.set_toast(trf("Could not disconnect “{}”: {}", &[&name, &err]));
                        }
                    }
                    self.request_network_listing(ctx, true);
                }
                NetMsg::Reconnect { local, name, address, back, tabs, left, result } => {
                    self.net.leaving.remove(&local);
                    self.request_network_listing(ctx, true);
                    match result {
                        Ok(()) => self.open_address_back(address, back, tabs),
                        Err(err) => {
                            self.bring_back(left);
                            self.set_toast(trf("Could not disconnect “{}”: {}", &[&name, &err]));
                        }
                    }
                }
                NetMsg::Renamed { path, target, new_name, item, result } => {
                    self.net.renaming.remove(&path);
                    // The dialog that asked, if it is still there, takes the answer; otherwise it comes as a message
                    let asking = matches!(&self.dialog, Some(Dialog::Rename { path: asked, .. }) if *asked == path);
                    match result {
                        Ok(()) => {
                            if asking {
                                self.dialog = None;
                            }
                            self.renamed(&path, target, &new_name, item);
                        }
                        Err(message) => match &mut self.dialog {
                            Some(Dialog::Rename { waiting, error, first_frame, .. }) if asking => {
                                *waiting = false;
                                *error = Some(message);
                                // The field was locked and lost the focus: it gets it back, to be typed in again
                                *first_frame = true;
                            }
                            _ => self.set_toast(message),
                        },
                    }
                }
                NetMsg::Forgot { name, result } => match result {
                    Ok(()) => self.set_toast(trf("Forgot the password of “{}”", &[&name])),
                    Err(err) => self.set_toast(trf("Could not forget the password: {}", &[&err])),
                },
            }
        }
        self.show_waiting_prompt();
        // A login or question that was closed some other way (Escape, a click outside) is a cancel
        let shown = match &self.dialog {
            Some(Dialog::Login { id, .. } | Dialog::NetQuestion { id, .. }) => Some(*id),
            _ => None,
        };
        for pending in self.net.pending.iter_mut().filter(|p| p.asking && Some(p.id) != shown) {
            pending.asking = false;
            let _ = pending.replies.send(Reply::Cancel);
        }
        if !self.net.pending.is_empty() || !self.net.leaving.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let late = overdue(&mut self.net.checking, CHECK_TIMEOUT);
        // A question that never left the window is not answered later
        self.net.checking.retain(|c| !(c.late && c.waiting.is_some()));
        if !late.is_empty() {
            for id in &late {
                if let Some(Dialog::Connect { busy, error, .. }) = &mut self.dialog
                    && *busy == Some(*id)
                {
                    *busy = None;
                    *error = Some(tr("Not answering").to_owned());
                }
            }
            self.set_toast(tr("Not answering"));
        }
        if !self.net.checking.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        self.probe_servers(ctx);
        self.request_network_listing(ctx, false);
    }

    /// Say, once, that the list of saved servers could not be read (and where the unreadable file is).
    pub(super) fn announce_servers_problem(&mut self) {
        let message = match self.net.servers_problem.take() {
            Some(network::LoadProblem::Kept(name)) => trf("The list of servers could not be read. The file was kept as “{}” and a new list is used.", &[&name]),
            Some(network::LoadProblem::NotKept) => tr("The list of servers could not be read, and the file could not be put aside. It is left as it is and not written over.").to_owned(),
            None => return,
        };
        self.set_toast(message);
    }

    /// Ask the connected servers whether they still answer (see `mount_health`).
    fn probe_servers(&mut self, ctx: &egui::Context) {
        let mut servers: Vec<PathBuf> = self.net.mounts.iter().filter(|m| m.address.is_some() && !self.net.leaving.contains(&m.local)).map(|m| m.local.clone()).collect();
        // The drives the person treats as servers are watched like them, as long as they are mounted
        servers.extend(self.places.iter().filter(|p| p.drive.as_ref().is_some_and(|d| d.mounted) && network::is_user_remote(&p.path)).map(|p| p.path.clone()));
        let ctx_asked = ctx.clone();
        self.net.probes.tick(&servers, Instant::now(), move |local, tx| {
            let ctx = ctx_asked.clone();
            std::thread::spawn(move || {
                let ok = mount_health::answers(&local);
                let _ = tx.send((local, ok));
                ctx.request_repaint();
            });
        });
        // A question in flight may turn "slow" with no other reason to draw a frame
        if self.net.probes.asking() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// How the drive at `folder`, which the person treats as a server, is doing.
    pub(super) fn folder_health(&self, folder: &Path) -> Health {
        self.net.probes.health(folder, Instant::now())
    }

    /// How the server mounted at `mount` is doing.
    pub(super) fn mount_health(&self, mount: &Mount) -> Health {
        self.net.probes.health(&mount.local, Instant::now())
    }

    pub(super) fn request_network_listing(&mut self, ctx: &egui::Context, now: bool) {
        let state = &mut self.net;
        if state.listing || !now && state.last.elapsed() < REFRESH {
            return;
        }
        state.listing = true;
        state.last = Instant::now();
        let (tx, ctx) = (state.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(NetMsg::Listed(network::list_mounts()));
            ctx.request_repaint();
        });
    }

    /// The shown tab goes somewhere new: answers still on their way for an earlier navigation of it are out of date.
    /// Returns the tab and its navigation number.
    pub(super) fn note_navigation(&mut self) -> Option<(u64, u64)> {
        let tab = self.tabs.get(self.active_tab)?.id;
        Some((tab, self.note_navigation_of(tab)))
    }

    fn note_navigation_of(&mut self, tab: u64) -> u64 {
        let n = self.net.navs.entry(tab).or_insert(0);
        *n += 1;
        *n
    }

    /// Go to `target` on a server (or see `How`), once the server has said what it is.
    pub(super) fn check_folder(&mut self, target: PathBuf, how: How) {
        if let Some(tab) = self.tabs.get(self.active_tab).map(|t| t.id) {
            self.check_folder_in(tab, target, how);
        }
    }

    /// The same for tab `tab`, shown or parked.
    pub(super) fn check_folder_in(&mut self, tab: u64, target: PathBuf, how: How) {
        let serial = self.note_navigation_of(tab);
        let open: Vec<u64> = self.tabs.iter().map(|t| t.id).collect();
        self.net.navs.retain(|t, _| open.contains(t));
        self.check_then(Next::Folder { target, tab, serial, how });
    }

    /// Show `address`: at once when its server is connected, otherwise once connecting is done.
    pub(crate) fn open_address(&mut self, address: Address, password: Option<String>, open: OpenIn) {
        self.check_then(Next::Open { address, password, open });
    }

    /// Ask, on a thread of its own, whether the folder of `next`'s server is there, and carry on with `next` when
    /// it is known. A server that hangs cannot hold the window up: it is the window that stops waiting.
    /// `None` when an earlier question about the same server is still without an answer: asking again would only
    /// add a thread that hangs as well, so the window says at once that it is not answering.
    fn check_then(&mut self, next: Next) -> Option<u64> {
        let id = self.net.next_id;
        self.net.next_id += 1;
        let mount = next.local().and_then(|p| network::mount_of(&p).map(|m| m.local).or_else(|| network::remote_root(&p)));
        let generation = self.net.generation_of(&mount);
        let tab = if let Next::Folder { tab, .. } = &next { Some(*tab) } else { None };
        let check = Check { id, key: next.key(), mount, tab, generation, started: Instant::now(), late: false, waiting: None };
        match admit(&mut self.net.checking, check, next) {
            Admit::Refused => {
                self.set_toast(tr("Not answering"));
                None
            }
            Admit::Start(next) => {
                spawn_check(id, next, self.net.tx.clone(), self.ctx.clone(), self.net.ask);
                Some(id)
            }
            Admit::Queued => Some(id),
        }
    }

    /// The answer to `check_then`: show the server's folder when it is there, else connect it.
    fn checked(&mut self, ctx: &egui::Context, id: u64, found: Option<PathBuf>, is_dir: bool, next: Next) {
        let Some(i) = self.net.checking.iter().position(|c| c.id == id) else { return };
        let check = self.net.checking.remove(i);
        // The mount is free again for the question that waited for it
        if let Some((waiting, next)) = take_waiting(&mut self.net.checking, &check.key, &check.mount) {
            spawn_check(waiting, next, self.net.tx.clone(), self.ctx.clone(), self.net.ask);
        }
        // Whatever it was, the sidebar learns that the server answers
        self.request_network_listing(ctx, true);
        if check.late {
            return;
        }
        // The mount was connected again (or let go of) since: this is about the connection before
        if check.generation != self.net.generation_of(&check.mount) {
            return;
        }
        // The dialog was closed meanwhile: the person gave up on this
        if matches!(next, Next::Dialog { .. }) && !matches!(&self.dialog, Some(Dialog::Connect { busy: Some(b), .. }) if *b == id) {
            return;
        }
        match (next, found) {
            (Next::Folder { target, tab, serial, how }, answer) => {
                let shown = self.tabs.get(self.active_tab).map(|t| t.id);
                // Out of date (another folder was asked for, the tab was left or closed), or the server went away meanwhile
                if self.net.navs.get(&tab) != Some(&serial) || Self::server_target(&target).is_some() {
                    return;
                }
                let found = answer.unwrap_or(target);
                match how {
                    How::Replace => self.replace_tab_folder(tab, found),
                    // A tab that is not shown is put there without a history entry, like a replaced one
                    How::Nearest if shown != Some(tab) => self.replace_tab_folder(tab, found),
                    _ if !nav_current(&self.net.navs, shown, tab, serial) => {}
                    How::Nearest if found == self.current_path => {}
                    _ => self.go_to(found, is_dir),
                }
            }
            (Next::Open { open, .. }, Some(local)) => self.show_connected(local, is_dir, open),
            (Next::Open { address, password, open }, None) => {
                self.start_connecting(address, password, open, false, false);
            }
            (Next::Back { back, tabs, .. }, Some(local)) => self.show_back(local, back, tabs, OpenIn::Here),
            (Next::Back { address, back, tabs }, None) => {
                let id = self.start_connecting(address, None, OpenIn::Here, false, false);
                if let Some(pending) = self.net.pending.iter_mut().find(|p| p.id == id) {
                    pending.then = back;
                    pending.tabs = tabs;
                }
            }
            (Next::Dialog { address, save, .. }, Some(local)) => {
                if save {
                    self.save_server(address);
                }
                if matches!(&self.dialog, Some(Dialog::Connect { busy: Some(b), .. }) if *b == id) {
                    self.dialog = None;
                }
                self.show_connected(local, is_dir, OpenIn::Here);
            }
            (Next::Dialog { address, password, save }, None) => {
                let new = self.start_connecting(address, password, OpenIn::Here, true, save);
                if let Some(Dialog::Connect { busy, .. }) = &mut self.dialog
                    && *busy == Some(id)
                {
                    *busy = Some(new);
                }
            }
        }
    }

    /// `open_address` for a server that was connected again: shows `back` (a folder below its top) when it is there.
    fn open_address_back(&mut self, address: Address, back: Option<PathBuf>, tabs: Vec<(u64, PathBuf)>) {
        self.check_then(Next::Back { address, back, tabs });
    }

    fn start_connecting(&mut self, address: Address, password: Option<String>, open: OpenIn, from_dialog: bool, save: bool) -> u64 {
        // A second request for a server that is still connecting waits for the first: two connections to the same
        // mount would get in each other's way. What it asks for beyond what the first brings (another folder, a new
        // tab or window) comes when the server is there.
        if let Some(p) = self.net.pending.iter_mut().find(|p| p.address.same_mount(&address)) {
            let request = (address, open);
            if (p.address != request.0 || p.open != request.1) && !p.later.contains(&request) {
                p.later.push(request);
            }
            return p.id;
        }
        let id = self.net.next_id;
        self.net.next_id += 1;
        let (tx, ctx) = (self.net.tx.clone(), self.ctx.clone());
        let replies = network::connect(address.clone(), password, move |progress| {
            let msg = match progress {
                Progress::Ask(prompt) => NetMsg::Ask { id, prompt },
                Progress::Done(result) => NetMsg::Done { id, result },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
        self.net.pending.push(Pending { id, address, open, replies, asking: false, from_dialog, save, then: None, tabs: Vec::new(), later: Vec::new() });
        id
    }

    /// The requests that waited for a connection are carried out now that the server is there.
    fn replay_later(&mut self, later: Vec<(Address, OpenIn)>) {
        for (address, open) in later {
            self.open_address(address, None, open);
        }
    }

    /// Whether a connection to the server of `address` is on its way.
    fn connecting(&self, address: &Address) -> bool {
        self.net.pending.iter().any(|p| p.address.same_mount(address))
    }

    fn connected(&mut self, ctx: &egui::Context, id: u64, result: Result<PathBuf, Option<String>>) {
        let Some(at) = self.net.pending.iter().position(|p| p.id == id) else { return };
        let pending = self.net.pending.remove(at);
        let dialog_busy = matches!(&self.dialog, Some(Dialog::Connect { busy: Some(b), .. }) if *b == id);
        match result {
            Ok(local) => {
                // The account that logged in may differ from the one in the address
                let address = network::address_of(&local).unwrap_or_else(|| pending.address.clone());
                self.net.servers.used(&address);
                if pending.save && self.net.servers.find(&address).is_none() {
                    self.net.servers.saved.push(Server { name: String::new(), address });
                }
                self.net.servers.save();
                if dialog_busy {
                    self.dialog = None;
                }
                self.net.connection_changed(&local);
                self.request_network_listing(ctx, true);
                self.show_back(local, pending.then, pending.tabs, pending.open);
                self.replay_later(pending.later);
            }
            Err(None) => {
                if let Some(Dialog::Connect { busy, .. }) = &mut self.dialog {
                    *busy = None;
                }
            }
            Err(Some(err)) => {
                if pending.from_dialog {
                    match &mut self.dialog {
                        Some(Dialog::Connect { busy, error, .. }) => {
                            *busy = None;
                            *error = Some(err);
                        }
                        _ => {
                            self.dialog = Some(Dialog::Connect { text: pending.address.display(), save: pending.save, error: Some(err), first_frame: true, busy: None });
                        }
                    }
                } else {
                    self.set_toast(trf("Could not connect to “{}”: {}", &[&pending.address.name(), &err]));
                }
            }
        }
    }


    /// Show a server that was connected again: the window in the folder it was in, the tabs that were parked in
    /// it in theirs, where those are still folders, else at the top of the server. A tab that has been moved
    /// since is left alone.
    fn show_back(&mut self, top: PathBuf, back: Option<PathBuf>, tabs: Vec<(u64, PathBuf)>, open: OpenIn) {
        if back.is_none() && tabs.is_empty() {
            return self.show_connected(top, true, open);
        }
        let insides: Vec<PathBuf> = back.iter().cloned().chain(tabs.iter().map(|(_, inside)| inside.clone())).collect();
        let mut folders = folders_or_top(&top, &insides).into_iter();
        self.show_connected(if back.is_some() { folders.next().unwrap_or_else(|| top.clone()) } else { top.clone() }, true, open);
        let home = home_folder();
        for ((id, _), folder) in tabs.iter().zip(folders) {
            if let Some(state) = self.tabs.iter_mut().find(|slot| slot.id == *id).and_then(|slot| slot.saved.as_mut()).filter(|state| state.folder() == home) {
                state.set_folder(folder);
            }
        }
    }

    /// The tab `tab` (shown or parked) is in `folder` now, as the path it was opened at resolves to.
    fn replace_tab_folder(&mut self, tab: u64, folder: PathBuf) {
        if self.tabs.get(self.active_tab).map(|t| t.id) == Some(tab) {
            if folder != self.current_path {
                self.arrive(folder);
            }
        } else if let Some(state) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(|t| t.saved.as_mut())
            && state.folder() != folder
        {
            state.set_folder(folder);
        }
    }

    /// Show a folder of a connected server; a file is shown selected in its folder.
    /// `is_dir` is what the check found out: the server is not asked again, since it may have stopped answering.
    fn show_connected(&mut self, local: PathBuf, is_dir: bool, open: OpenIn) {
        let (folder, select) = if is_dir {
            (local, None)
        } else {
            (local.parent().map(Path::to_path_buf).unwrap_or_else(|| local.clone()), Some(local))
        };
        match open {
            OpenIn::Here => {
                self.close_search();
                self.navigate_known(folder);
                if let Some(file) = select {
                    self.select_path(&file);
                }
            }
            OpenIn::NewTab => self.new_tab(folder),
            OpenIn::NewWindow => self.open_window(&folder),
        }
    }

    /// The login or question dialog for a connection that waits for an answer. Another dialog on screen is not
    /// replaced (a question of a running copy that is dropped ends that copy; a name being typed is lost): the
    /// prompt waits for its turn (`show_waiting_prompt`), and the connection with it.
    fn show_prompt(&mut self, id: u64, prompt: Prompt) {
        if !self.net.pending.iter().any(|p| p.id == id) {
            return;
        }
        // The Connect to server dialog that started this very connection is the one place a login belongs in front of
        // (it only waits for it); any other dialog is somebody else's
        let theirs = self.net.pending.iter().find(|p| p.id == id).is_some_and(|p| p.from_dialog) && matches!(self.dialog, Some(Dialog::Connect { .. }));
        if self.dialog.is_some() && !theirs {
            self.net.waiting_prompts.push_back((id, prompt));
            return;
        }
        let Some(pending) = self.net.pending.iter_mut().find(|p| p.id == id) else { return };
        pending.asking = true;
        let address = pending.address.clone();
        self.dialog = Some(match prompt {
            Prompt::Login { user, domain, fixed_user: _, anonymous, retry } => Dialog::Login {
                id,
                server: address.name(),
                user: user.or_else(|| address.user.clone()).unwrap_or_else(|| if address.scheme == network::Scheme::Sftp { std::env::var("USER").unwrap_or_default() } else { String::new() }),
                domain: domain.or_else(|| address.domain.clone()).unwrap_or_default(),
                password: String::new(),
                show_domain: address.scheme.has_domain(),
                anonymous: anonymous.then(|| address.scheme.anonymous_label()),
                retry,
                remember: true,
                reveal: false,
                first_frame: true,
            },
            Prompt::Question { message, choices } => {
                let (title, body) = describe_question(&message);
                Dialog::NetQuestion { id, title, body, choices: choices.iter().map(|c| choice_label(c)).collect() }
            }
        });
    }

    /// Once nothing else is on screen: the next prompt that waited. One whose connection is over is dropped.
    fn show_waiting_prompt(&mut self) {
        while self.dialog.is_none() {
            let Some((id, prompt)) = self.net.waiting_prompts.pop_front() else { return };
            self.show_prompt(id, prompt);
        }
    }

    fn reply(&mut self, id: u64, reply: Reply) {
        if let Some(pending) = self.net.pending.iter_mut().find(|p| p.id == id) {
            pending.asking = false;
            let _ = pending.replies.send(reply);
        }
    }

    pub(super) fn disconnect(&mut self, ctx: &egui::Context, mount: Mount) {
        if !self.net.leaving.insert(mount.local.clone()) {
            return;
        }
        // Nothing may hold the folder open: leave it first (and come back if it cannot be let go of)
        let left = self.leave_mount_remembering(&mount.local);
        let (tx, ctx) = (self.net.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = network::disconnect(&mount);
            let _ = tx.send(NetMsg::Gone { local: mount.local.clone(), name: mount.name.clone(), left, result });
            ctx.request_repaint();
        });
    }

    /// Let go of a server that is slow or broken and connect it again. Nothing is forced: a mount that
    /// is in use stays, and the reason comes as a message. Not while a job runs, which may be using it.
    pub(super) fn reconnect(&mut self, ctx: &egui::Context, mount: Mount) {
        let Some(address) = mount.address.clone().filter(|_| mount.kind != MountKind::System) else { return };
        if self.engine.running() {
            self.set_toast(tr("Wait for the running jobs to finish, then reconnect"));
            return;
        }
        if !self.net.leaving.insert(mount.local.clone()) {
            return;
        }
        // Come back to the folder the window is in, not to the top of the server
        let back = self.current_path.strip_prefix(&mount.local).ok().filter(|inside| !inside.as_os_str().is_empty()).map(Path::to_path_buf);
        // ... and the tabs parked in it to theirs
        let tabs = self.parked_in(&mount.local);
        let left = self.leave_mount_remembering(&mount.local);
        let (tx, ctx) = (self.net.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = network::disconnect(&mount);
            let _ = tx.send(NetMsg::Reconnect { local: mount.local.clone(), name: mount.name.clone(), address, back, tabs, left, result });
            ctx.request_repaint();
        });
    }

    /// The parked tabs that are in the mount: `(tab id, folder below its top)`.
    fn parked_in(&self, local: &Path) -> Vec<(u64, PathBuf)> {
        self.tabs.iter().filter_map(|slot| Some((slot.id, slot.saved.as_ref()?.folder().strip_prefix(local).ok()?.to_path_buf()))).collect()
    }

    /// `leave_mount`, noting where the windows were so that `bring_back` can return them.
    pub(super) fn leave_mount_remembering(&mut self, local: &Path) -> Left {
        let left = Left {
            current: self.current_path.starts_with(local).then(|| self.current_path.clone()),
            tabs: self.tabs.iter().filter_map(|slot| slot.saved.as_ref().filter(|state| state.is_inside(local)).map(|state| (slot.id, state.folder().to_path_buf()))).collect(),
        };
        self.leave_mount(local);
        left
    }

    /// The mount could not be let go of and is still there: the windows `leave_mount` moved go back into it. A window
    /// the person has used since (it is no longer where it was put) stays where it is.
    fn bring_back(&mut self, left: Left) {
        let home = home_folder();
        if let Some(folder) = left.current
            && self.current_path == home
            && self.back_history.last() == Some(&folder)
        {
            self.go_back();
        }
        for (id, folder) in left.tabs {
            if let Some(state) = self.tabs.iter_mut().find(|slot| slot.id == id).and_then(|slot| slot.saved.as_mut()).filter(|state| state.folder() == home) {
                state.set_folder(folder);
            }
        }
    }

    /// Take every tab out of the folders of the mount, which is about to be let go of: the one on screen
    /// and the parked ones all go to the home folder.
    pub(super) fn leave_mount(&mut self, local: &Path) {
        self.net.connection_changed(local);
        let home = home_folder();
        if self.current_path.starts_with(local) {
            self.navigate_known(home.clone());
        }
        for slot in &mut self.tabs {
            if let Some(state) = slot.saved.as_mut().filter(|state| state.is_inside(local)) {
                state.set_folder(home.clone());
            }
        }
    }

    pub(super) fn forget_password(&mut self, address: Address) {
        let (tx, ctx) = (self.net.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = network::forget_password(&address);
            let _ = tx.send(NetMsg::Forgot { name: address.name(), result });
            ctx.request_repaint();
        });
    }

    /// Rename an item on a server on a thread of its own. The item stays marked as being renamed until the answer is in.
    pub(super) fn rename_on_server(&mut self, path: PathBuf, target: PathBuf, new_name: String) {
        self.net.renaming.insert(path.clone(), target.clone());
        let (tx, ctx) = (self.net.tx.clone(), self.ctx.clone());
        #[cfg(test)]
        let rename = self.net.rename.clone();
        std::thread::spawn(move || {
            #[cfg(test)]
            let result = rename(&path, &target).map_err(|err| rename_error(&err, &new_name));
            #[cfg(not(test))]
            let result = crate::compat::rename_noreplace(&path, &target).map_err(|err| rename_error(&err, &new_name));
            // Read here, where waiting for the server does no harm
            let item = if result.is_ok() { FileItem::from_path(&target) } else { None };
            let _ = tx.send(NetMsg::Renamed { path, target, new_name, item, result });
            ctx.request_repaint();
        });
    }

    pub(super) fn save_server(&mut self, address: Address) {
        if self.net.servers.find(&address).is_none() {
            self.net.servers.saved.push(Server { name: String::new(), address });
            self.net.servers.save();
        }
    }

    pub(super) fn remove_server(&mut self, index: usize) {
        if index < self.net.servers.saved.len() {
            self.net.servers.saved.remove(index);
            self.net.servers.save();
        }
    }

    pub(crate) fn open_connect_dialog(&mut self) {
        let text = network::display_path(&self.current_path).filter(|_| self.chooser.is_none()).unwrap_or_default();
        self.dialog = Some(Dialog::Connect { text, save: true, error: None, first_frame: true, busy: None });
    }

    pub(super) fn open_edit_server(&mut self, index: usize) {
        let Some(server) = self.net.servers.saved.get(index) else { return };
        self.dialog = Some(Dialog::EditServer { index, name: server.label(), text: server.address.display(), error: None, first_frame: true });
    }

    /// The server's own name for the sidebar or the trail of folders: a saved name, else the address's.
    pub(super) fn server_label(&self, mount: &Mount) -> String {
        mount.address.as_ref().and_then(|a| self.net.servers.find(a)).map(|i| self.net.servers.saved[i].label()).unwrap_or_else(|| mount.name.clone())
    }

    /// The Network section: saved servers, connected ones, and the way to add one.
    pub(super) fn network_rows(&self) -> Vec<NetRow> {
        let mounts = &self.net.mounts;
        let mut used = vec![false; mounts.len()];
        let mut rows = Vec::new();
        for (index, server) in self.net.servers.saved.iter().enumerate() {
            let found = mounts.iter().position(|m| m.address.as_ref().is_some_and(|a| a.same_mount(&server.address)));
            if let Some(i) = found {
                used[i] = true;
            }
            let mount = found.map(|i| mounts[i].clone());
            rows.push(NetRow {
                name: server.label(),
                icon: icons::SERVER_NETWORK,
                address: Some(server.address.clone()),
                open: mount.as_ref().map(|m| local_in(m, &server.address)),
                state: LinkState::of(mount.is_some(), true, mount.as_ref().map_or(Health::Connected, |m| self.mount_health(m))),
                mount,
                saved: Some(index),
            });
        }
        for (mount, _) in mounts.iter().zip(used).filter(|(_, used)| !used) {
            rows.push(NetRow {
                name: mount.name.clone(),
                icon: if mount.is_device() { icons::CELLPHONE } else { icons::SERVER_NETWORK },
                address: mount.address.clone(),
                open: Some(mount.local.clone()),
                state: LinkState::of(true, mount.address.is_some(), self.mount_health(mount)),
                mount: Some(mount.clone()),
                saved: None,
            });
        }
        rows
    }

    /// Draw the Network section of the sidebar; what was clicked comes back as events.
    pub(super) fn network_sidebar(&self, ui: &mut egui::Ui, browsing: bool, zones: &mut Vec<dnd::DropZone>) -> Vec<NetEvent> {
        let mut events = Vec::new();
        let theme = self.theme;
        for (idx, row) in self.network_rows().into_iter().enumerate() {
            let busy = row.address.as_ref().is_some_and(|a| self.connecting(a)) || row.mount.as_ref().is_some_and(|m| self.net.leaving.contains(&m.local));
            let reserve = if row.mount.is_some() || busy { material::NAV_TRAILING } else { 0.0 };
            let here = browsing && row.open.as_ref().is_some_and(|p| *p == self.current_path);
            let mut resp = material::nav_item_with(ui, ("net", idx, &row.name), row.icon, &row.name, &theme, here, reserve);
            if let Some(state) = row.state {
                state.paint(ui, resp.rect, &theme);
            }
            // Name and address in full, however narrow the sidebar is, then how the connection is
            let mut tip = vec![row.name.clone()];
            tip.extend(row.address.as_ref().map(|a| a.display()));
            tip.extend(row.state.map(|s| s.words().to_owned()));
            resp = resp.on_hover_text(tip.join("\n"));
            if busy {
                let spot = egui::Rect::from_center_size(egui::pos2(resp.rect.right() - 8.0 - 14.0, resp.rect.center().y), egui::vec2(16.0, 16.0));
                ui.put(spot, egui::Spinner::new().size(14.0).color(theme.subtext0));
            } else if let Some(mount) = &row.mount
                && drives_ui::row_button(ui, resp.rect, resp.hovered(), ("net_off", idx), icons::UNMOUNT, tr("Disconnect"), &theme)
            {
                events.push(NetEvent::Disconnect(mount.clone()));
                continue;
            }
            if let Some(open) = &row.open {
                zones.push(dnd::DropZone { rect: resp.rect, dest: open.clone() });
            }
            if resp.clicked() {
                match (&row.open, &row.address) {
                    (Some(open), _) => events.push(NetEvent::Open(open.clone())),
                    (None, Some(address)) if !busy => events.push(NetEvent::Connect(address.clone(), OpenIn::Here)),
                    _ => {}
                }
            }
            if resp.middle_clicked() {
                match (&row.open, &row.address) {
                    (Some(open), _) => events.push(NetEvent::NewTab(open.clone())),
                    (None, Some(address)) => events.push(NetEvent::Connect(address.clone(), OpenIn::NewTab)),
                    _ => {}
                }
            }
            if resp.secondary_clicked()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                events.push(NetEvent::Menu(pos, row.clone()));
            }
        }
        // Off by default (Settings → Sidebar); listed faded with a tick box while the hidden places are
        let shown = self.place_shown(SidebarPlace::ConnectToServer);
        if shown || self.sidebar_editing {
            let editing = self.sidebar_editing;
            let reserve = if editing { material::NAV_TRAILING } else { 0.0 };
            let resp = ui
                .scope(|ui| {
                    if !shown {
                        ui.multiply_opacity(0.45);
                    }
                    material::nav_item_with(ui, "net_connect", icons::LAN_CONNECT, tr("Connect to server…"), &theme, false, reserve)
                })
                .inner;
            let resp = material::full_text_if_shortened(ui.ctx(), resp, tr("Connect to server…"));
            if editing {
                let tick = egui::Rect::from_center_size(egui::pos2(resp.rect.right() - 22.0, resp.rect.center().y), egui::Vec2::splat(30.0));
                let ticked = ui.scope_builder(egui::UiBuilder::new().max_rect(tick), |ui| material::checkbox(ui, "net_connect_tick", shown, true, &theme)).inner;
                if ticked.on_hover_text(if shown { tr("Hide from sidebar") } else { tr("Show in sidebar") }).clicked() {
                    events.push(NetEvent::ShowConnect(!shown));
                    return events;
                }
            }
            if resp.clicked() {
                events.push(NetEvent::ConnectDialog);
            }
            if resp.secondary_clicked()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                events.push(NetEvent::ConnectMenu(pos));
            }
        }
        events
    }

    /// Whether the Network section has anything to list.
    pub(super) fn network_listed(&self) -> bool {
        self.sidebar_editing || self.place_shown(SidebarPlace::ConnectToServer) || !self.net.servers.saved.is_empty() || !self.net.mounts.is_empty()
    }

    pub(super) fn apply_network_events(&mut self, ctx: &egui::Context, events: Vec<NetEvent>) {
        for event in events {
            match event {
                NetEvent::Open(path) => {
                    self.close_search();
                    self.navigate_to(path);
                }
                NetEvent::Connect(address, open) => self.open_address(address, None, open),
                NetEvent::NewTab(path) => self.new_tab(path),
                NetEvent::Disconnect(mount) => self.disconnect(ctx, mount),
                NetEvent::Menu(pos, row) => self.open_menu(ctx, pos, MenuTarget::Server(Box::new(row))),
                NetEvent::ConnectDialog => {
                    // The click that opened it is not a click outside the dialog
                    self.menu_click_consumed = true;
                    self.open_connect_dialog();
                }
                NetEvent::ShowConnect(shown) => self.show_place(SidebarPlace::ConnectToServer, shown, false),
                NetEvent::ConnectMenu(pos) => self.open_menu(ctx, pos, MenuTarget::ConnectEntry),
            }
        }
    }

    /// Context menu of a Network row.
    pub(super) fn server_menu(&self, row: &NetRow) -> Vec<Vec<MenuEntry>> {
        let mut groups = Vec::new();
        match (&row.open, &row.address) {
            (Some(open), _) => groups.push(vec![
                (icons::OPEN, tr("Open"), MenuAction::OpenPath(open.clone())),
                (icons::NEW_TAB, tr("Open in new tab"), MenuAction::OpenInNewTab(open.clone())),
                (icons::NEW_WINDOW, tr("Open in new window"), MenuAction::OpenInNewWindow(open.clone())),
            ]),
            (None, Some(address)) => groups.push(vec![
                (icons::LAN_CONNECT, tr("Connect"), MenuAction::Connect(address.clone(), OpenIn::Here)),
                (icons::NEW_TAB, tr("Open in new tab"), MenuAction::Connect(address.clone(), OpenIn::NewTab)),
                (icons::NEW_WINDOW, tr("Open in new window"), MenuAction::Connect(address.clone(), OpenIn::NewWindow)),
            ]),
            (None, None) => {}
        }
        if let Some(mount) = &row.mount {
            let mut group = vec![(icons::UNMOUNT, tr("Disconnect"), MenuAction::Disconnect(mount.local.clone()))];
            if offers_reconnect(mount, row.state) {
                group.push((icons::REFRESH, tr("Reconnect"), MenuAction::Reconnect(mount.local.clone())));
            }
            groups.push(group);
        }
        match (row.saved, &row.address) {
            (Some(index), _) => groups.push(vec![
                (icons::RENAME, tr("Edit…"), MenuAction::EditServer(index)),
                (icons::PIN_OFF, tr("Remove from sidebar"), MenuAction::RemoveServer(index)),
            ]),
            (None, Some(address)) => groups.push(vec![(icons::PIN, tr("Keep in sidebar"), MenuAction::SaveServer(address.clone()))]),
            (None, None) => {}
        }
        let mut last = Vec::new();
        if let Some(address) = &row.address {
            last.push((icons::COPY, tr("Copy address"), MenuAction::CopyPath(PathBuf::from(address.display()))));
            last.push((icons::KEY, tr("Forget password"), MenuAction::ForgetPassword(address.clone())));
        }
        if let Some(open) = &row.open {
            last.push((icons::INFO, tr("Properties"), MenuAction::Properties(open.clone())));
        }
        groups.push(last);
        groups.retain(|g| !g.is_empty());
        groups
    }

    // --- Dialogs --------------------------------------------------------------------------------

    pub(super) fn connect_dialog_contents(&mut self, ui: &mut egui::Ui, dialog: &mut Dialog) -> bool {
        let Dialog::Connect { text, save, error, first_frame, busy } = dialog else { return false };
        let theme = self.theme;
        let mut close = material::dialog_header(ui, tr("Connect to server"), Some(tr("Windows shares, SSH, FTP and WebDAV")), &theme, "conn_close");

        let mut submit = false;
        material::input_frame(&theme).show(ui, |ui| {
            let out = egui::TextEdit::singleline(text)
                .frame(egui::Frame::NONE)
                .desired_width(f32::INFINITY)
                .hint_text("smb://server/share")
                .font(egui::FontId::proportional(material::FONT_SUBTITLE))
                .interactive(busy.is_none())
                .show(ui);
            if std::mem::take(first_frame) {
                out.response.request_focus();
            }
            if out.response.changed() {
                *error = None;
            }
            submit = out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        });

        // The kinds of server as chips: a click puts its scheme at the start of the address
        let kinds = [network::Scheme::Smb, network::Scheme::Sftp, network::Scheme::Ftp, network::Scheme::Davs];
        let current = Address::parse(text).map(|a| a.scheme);
        ui.horizontal_wrapped(|ui| {
            for scheme in kinds {
                let label = match scheme {
                    network::Scheme::Smb => "SMB",
                    network::Scheme::Sftp => "SFTP",
                    network::Scheme::Ftp => "FTP",
                    _ => "WebDAV",
                };
                if chip(ui, ("conn_kind", scheme.name()), None, label, &theme, current == Some(scheme)).clicked() && busy.is_none() {
                    let rest = match text.split_once("://") {
                        Some((_, rest)) => rest.to_owned(),
                        None => text.trim_start_matches(['\\', '/']).replace('\\', "/"),
                    };
                    *text = format!("{}://{rest}", scheme.name());
                    *error = None;
                    *first_frame = true;
                }
            }
        });

        let parsed = Address::parse_login(text);
        let note = match &parsed {
            Some((address, _)) => format!("{} · {}", address.scheme.label(), address.name()),
            None if text.trim().is_empty() => tr("Type an address such as sftp://me@example.com or smb://nas/media").to_owned(),
            None => tr("Start the address with smb://, sftp://, ftp:// or davs://").to_owned(),
        };
        ui.label(egui::RichText::new(note).size(material::FONT_CAPTION).color(theme.subtext1));
        if let Some(err) = error.as_ref() {
            ui.label(egui::RichText::new(err).size(material::FONT_CAPTION).color(theme.red));
        }

        if material::toggle_row(ui, "conn_save", tr("Keep in sidebar"), None, &theme, *save).clicked() {
            *save = !*save;
        }

        if !self.net.servers.recent.is_empty() {
            material::section_label(ui, tr("Recent servers"), &theme);
            let mut picked = None;
            material::group_card(ui, &theme, |ui| {
                egui::ScrollArea::vertical().max_height(material::ENTRY_HEIGHT * 4.0 + 8.0).auto_shrink([false, true]).id_salt("conn_recent").show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (i, address) in self.net.servers.recent.iter().enumerate() {
                        let resp = nav_item(ui, ("conn_recent", i), icons::SERVER_NETWORK, &address.display(), &theme, false);
                        if resp.double_clicked() {
                            picked = Some((address.display(), true));
                        } else if resp.clicked() {
                            picked = Some((address.display(), false));
                        }
                    }
                });
            });
            if let Some((address, go)) = picked.filter(|_| busy.is_none()) {
                *text = address;
                *error = None;
                submit |= go;
            }
        }

        let label = if busy.is_some() { tr("Connecting…") } else { tr("Connect") };
        let footer = material::dialog_footer(ui, &theme, FooterSpec { enabled: parsed.is_some() && busy.is_none(), ..FooterSpec::new("conn", icons::LAN_CONNECT, label) });
        close |= footer.cancel;
        if (footer.primary || submit) && busy.is_none() {
            match parsed {
                Some((address, password)) => {
                    *error = None;
                    match self.check_then(Next::Dialog { address, password, save: *save }) {
                        Some(id) => *busy = Some(id),
                        None => *error = Some(tr("Not answering").to_owned()),
                    }
                }
                None => *error = Some(tr("This is not a server address").to_owned()),
            }
        }
        if busy.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        close
    }

    pub(super) fn login_dialog_contents(&mut self, ui: &mut egui::Ui, dialog: &mut Dialog) -> bool {
        let Dialog::Login { id, server, user, domain, password, show_domain, anonymous, retry, remember, reveal, first_frame } = dialog else { return false };
        let theme = self.theme;
        let id = *id;
        let mut close = material::dialog_header(ui, tr("Log in"), Some(server.as_str()), &theme, "login_close");
        if *retry {
            ui.label(egui::RichText::new(tr("The server did not accept this user name or password. Try again.")).size(material::FONT_CAPTION).color(theme.red));
        }

        let focus = std::mem::take(first_frame);
        let mut submit = false;
        let field = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str, secret: bool, focus: bool| -> (bool, egui::Rect) {
            ui.label(egui::RichText::new(label).size(material::FONT_CAPTION).color(theme.subtext1));
            let mut entered = false;
            let frame = material::input_frame(&theme).show(ui, |ui| {
                let out = egui::TextEdit::singleline(value)
                    .frame(egui::Frame::NONE)
                    .desired_width(if secret { ui.available_width() - 28.0 } else { f32::INFINITY })
                    .hint_text(hint)
                    .password(secret)
                    .font(egui::FontId::proportional(material::FONT_SUBTITLE))
                    .show(ui);
                if focus {
                    out.response.request_focus();
                }
                entered = out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            });
            (entered, frame.response.rect)
        };
        submit |= field(ui, tr("User name"), user, "", false, focus && user.is_empty()).0;
        if *show_domain {
            submit |= field(ui, tr("Domain"), domain, "WORKGROUP", false, false).0;
        }
        let (entered, rect) = field(ui, tr("Password"), password, "", !*reveal, focus && !user.is_empty());
        submit |= entered;
        // The eye at the end of the password field shows what was typed
        let eye = egui::Rect::from_min_size(egui::pos2(rect.right() - 6.0 - 28.0, rect.center().y - 14.0), egui::vec2(28.0, 28.0));
        let (icon, tip) = if *reveal { (icons::VISIBILITY_OFF, tr("Hide password")) } else { (icons::VISIBILITY, tr("Show password")) };
        if ui.scope_builder(egui::UiBuilder::new().max_rect(eye), |ui| icon_button(ui, "login_reveal", icon, &theme, true, tip)).inner.clicked() {
            *reveal = !*reveal;
        }

        ui.add_space(2.0);
        if material::toggle_row(ui, "login_remember", tr("Remember password"), None, &theme, *remember).clicked() {
            *remember = !*remember;
        }

        let footer = material::dialog_footer(ui, &theme, FooterSpec { extra: *anonymous, ..FooterSpec::new("login", icons::LOCK_OPEN, tr("Connect")) });
        if footer.primary || submit {
            let reply = Reply::Login { user: user.trim().to_owned(), domain: domain.trim().to_owned(), password: std::mem::take(password), remember: *remember };
            self.reply(id, reply);
            return true;
        }
        if footer.extra {
            self.reply(id, Reply::Anonymous);
            return true;
        }
        close |= footer.cancel;
        if close {
            self.reply(id, Reply::Cancel);
        }
        close
    }

    pub(super) fn net_question_contents(&mut self, ui: &mut egui::Ui, dialog: &mut Dialog) -> bool {
        let Dialog::NetQuestion { id, title, body, choices } = dialog else { return false };
        let theme = self.theme;
        let id = *id;
        let close = material::dialog_header(ui, title, None, &theme, "netq_close");
        ui.label(egui::RichText::new(body.as_str()).size(material::FONT_BODY).color(theme.subtext0));
        ui.add_space(8.0);
        let mut picked = None;
        ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 36.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // The first choice goes ahead; it stands at the right, as the main button of every dialog does
            for (i, choice) in choices.iter().enumerate() {
                let kind = if i == 0 { ButtonKind::Filled } else { ButtonKind::Text };
                let resp = material::button(ui, ("netq", i), None, choice, &theme, kind, true);
                if material::dialog_fresh(ui.ctx()) && i + 1 == choices.len() {
                    // Like a destructive action, an unknown server starts on the cautious answer
                    resp.request_focus();
                }
                if resp.clicked() {
                    picked = Some(i);
                }
            }
        });
        if let Some(i) = picked {
            self.reply(id, Reply::Choice(i));
            return true;
        }
        if close {
            self.reply(id, Reply::Cancel);
        }
        close
    }

    pub(super) fn edit_server_contents(&mut self, ui: &mut egui::Ui, dialog: &mut Dialog) -> bool {
        let Dialog::EditServer { index, name, text, error, first_frame } = dialog else { return false };
        let theme = self.theme;
        let mut close = material::dialog_header(ui, tr("Edit server"), None, &theme, "edsrv_close");
        let mut submit = false;
        let focus = std::mem::take(first_frame);
        for (label, value, hint, first) in [(tr("Name"), &mut *name, "", true), (tr("Address"), &mut *text, "smb://server/share", false)] {
            ui.label(egui::RichText::new(label).size(material::FONT_CAPTION).color(theme.subtext1));
            material::input_frame(&theme).show(ui, |ui| {
                let out = egui::TextEdit::singleline(value)
                    .frame(egui::Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .hint_text(hint)
                    .font(egui::FontId::proportional(material::FONT_SUBTITLE))
                    .show(ui);
                if focus && first {
                    out.response.request_focus();
                }
                if out.response.changed() {
                    *error = None;
                }
                submit |= out.response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            });
        }
        if let Some(err) = error.as_ref() {
            ui.label(egui::RichText::new(err).size(material::FONT_CAPTION).color(theme.red));
        }
        let footer = material::dialog_footer(ui, &theme, FooterSpec::new("edsrv", icons::CHECK, tr("Save")));
        close |= footer.cancel;
        if footer.primary || submit {
            match Address::parse(text) {
                Some(address) if *index < self.net.servers.saved.len() => {
                    let label = name.trim();
                    // A name left as the address would show keeps following the address
                    let name = if label == address.name() { String::new() } else { label.to_owned() };
                    self.net.servers.saved[*index] = Server { name, address };
                    self.net.servers.save();
                    close = true;
                }
                Some(_) => close = true,
                None => *error = Some(tr("This is not a server address").to_owned()),
            }
        }
        close
    }
}

/// Title and text for a question a server asks. GVfs asks in English; the ones known here are
/// told in the interface's language.
fn describe_question(message: &str) -> (String, String) {
    if message.contains("identity of the remote computer") {
        let host = message.split_once('(').and_then(|(_, r)| r.split_once(')')).map(|(h, _)| h.to_owned()).unwrap_or_default();
        let print = message.split("remote computer is ").nth(1).and_then(|r| r.split_whitespace().next()).map(|p| p.trim_end_matches('.').to_owned()).unwrap_or_default();
        let body = format!(
            "{}\n\n{}",
            trf("Compression has not connected to {} before, so it can't be sure the server is the one it claims to be. Its key fingerprint is {}.", &[&host, &print]),
            tr("Connect only if you trust this network, or if the fingerprint matches the one the server's administrator gave you."),
        );
        return (tr("Unknown server").to_owned(), body);
    }
    (tr("The server asks").to_owned(), message.to_owned())
}

/// A choice GVfs offers, in the interface's language when it is a known one.
fn choice_label(choice: &str) -> String {
    match choice {
        "Log In Anyway" => tr("Connect anyway").to_owned(),
        "Cancel Login" | "Cancel" => tr("Cancel").to_owned(),
        "Yes" => tr("Yes").to_owned(),
        "No" => tr("No").to_owned(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_server_that_does_not_answer_holds_the_window_up_for_the_wait_and_no_longer() {
        let paths: Vec<PathBuf> = ["fast", "hangs", "fast2"].iter().map(PathBuf::from).collect();
        let started = Instant::now();
        let got = kinds_with(&paths, Duration::from_millis(300), |p| {
            if p.ends_with("hangs") {
                std::thread::sleep(Duration::from_secs(3));
            }
            Kind::Dir
        });
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
        // What was asked before the hang is known, the hung one and what waits behind it are not
        assert_eq!(got, vec![Some(Kind::Dir), None, None]);
        let got = kinds_with(&paths, Duration::from_secs(5), |_| Kind::Missing);
        assert_eq!(got, vec![Some(Kind::Missing); 3]);
    }

    #[test]
    fn a_server_connected_again_shows_the_folder_the_window_was_in_if_it_is_still_there() {
        let top = std::env::temp_dir().join(format!("compression_back_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(top.join("a/b c")).unwrap();
        std::fs::write(top.join("file"), "x").unwrap();
        let rel = |p: &str| PathBuf::from(p);
        let got = folders_or_top(&top, &[rel("a/b c"), rel("a/gone"), rel("file"), rel("")]);
        assert_eq!(got, vec![top.join("a/b c"), top.clone(), top.clone(), top.clone()]);
        std::fs::remove_dir_all(&top).unwrap();
    }

    /// An app with `n` tabs, tab `i` in `top/t{i}/sub`, the last one shown; `top` stands in for a mount.
    fn app_in_mount(n: usize, name: &str) -> (CompressionApp, PathBuf) {
        let top = std::env::temp_dir().join(format!("compression_reconnect_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        let top = top.canonicalize().unwrap();
        for i in 0..n {
            std::fs::create_dir_all(top.join(format!("t{i}/sub"))).unwrap();
        }
        let ctx = egui::Context::default();
        let mut app = CompressionApp::new(top.join("t0/sub"), SerpantinumTheme::default(), Settings::default(), &ctx);
        for i in 1..n {
            app.new_tab(top.join(format!("t{i}/sub")));
        }
        // The threads that settle where those tabs are (a new tab is resolved on one) are done with before a test starts
        while !app.net.checking.is_empty() {
            let (id, local, is_dir, next) = next_check(&app);
            app.checked(&egui::Context::default(), id, local, is_dir, next);
        }
        network::declare_remote_for_tests(&top);
        (app, top)
    }

    fn folders(app: &CompressionApp) -> Vec<PathBuf> {
        app.session().0
    }

    #[test]
    fn every_tab_that_was_in_the_server_goes_back_to_its_own_folder() {
        let (mut app, top) = app_in_mount(3, "all");
        let home = home_folder();
        let tabs = app.parked_in(&top);
        assert_eq!(tabs.len(), 2, "{tabs:?}");
        let back = app.current_path.strip_prefix(&top).ok().map(Path::to_path_buf);
        app.leave_mount(&top);
        assert!(folders(&app).iter().all(|f| *f == home), "{:?}", folders(&app));
        app.show_back(top.clone(), back, tabs, OpenIn::Here);
        assert_eq!(folders(&app), vec![top.join("t0/sub"), top.join("t1/sub"), top.join("t2/sub")]);
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_folder_that_is_gone_sends_only_its_own_tab_to_the_top_and_a_moved_tab_stays() {
        let (mut app, top) = app_in_mount(4, "gone");
        let tabs = app.parked_in(&top);
        let back = app.current_path.strip_prefix(&top).ok().map(Path::to_path_buf);
        app.leave_mount(&top);
        std::fs::remove_dir_all(top.join("t1")).unwrap();
        let moved = app.tabs[2].id;
        app.tabs[2].saved.as_mut().unwrap().set_folder(top.join("t2"));
        app.show_back(top.clone(), back, tabs, OpenIn::Here);
        let got = folders(&app);
        assert_eq!(got[0], top.join("t0/sub"));
        assert_eq!(got[1], top, "a folder that is gone: the top of the server");
        assert_eq!(app.tabs.iter().find(|t| t.id == moved).and_then(|t| t.saved.as_ref()).map(|s| s.folder().to_path_buf()), Some(top.join("t2")));
        assert_eq!(got[3], top.join("t3/sub"), "the others are not held up by it");
        std::fs::remove_dir_all(&top).unwrap();
    }

    fn check(id: u64, key: &str, mount: Option<&str>, tab: Option<u64>) -> Check {
        Check { id, key: key.into(), mount: mount.map(PathBuf::from), tab, generation: 0, started: Instant::now(), late: false, waiting: None }
    }

    fn folder(path: &str, tab: u64, serial: u64) -> Next {
        Next::Folder { target: PathBuf::from(path), tab, serial, how: How::Go }
    }

    #[test]
    fn a_check_is_called_late_once_after_the_wait_and_never_before() {
        let old = |id| Check { started: Instant::now() - Duration::from_secs(10), ..check(id, "", None, None) };
        let mut checks = vec![check(1, "", None, None), old(2), Check { late: true, ..old(3) }];
        assert_eq!(overdue(&mut checks, CHECK_TIMEOUT), vec![2]);
        assert_eq!(overdue(&mut checks, CHECK_TIMEOUT), Vec::<u64>::new());
        assert!(!checks[0].late && checks[1].late);
    }

    #[test]
    fn a_mount_has_one_question_on_its_way_and_the_others_wait_without_piling_up() {
        let mut checks = Vec::new();
        let ask = |checks: &mut Vec<Check>, id, path: &str, tab| admit(checks, check(id, path, Some("/m"), Some(tab)), folder(path, tab, id));
        assert!(matches!(ask(&mut checks, 1, "/m/a", 1), Admit::Start(_)));
        // Many clicks while it hangs: one thread, and each tab keeps only its newest wish
        for id in 2..100 {
            assert!(matches!(ask(&mut checks, id, "/m/b", 1 + id % 2), Admit::Queued));
        }
        assert_eq!(checks.iter().filter(|c| c.waiting.is_none()).count(), 1);
        assert_eq!(checks.iter().filter(|c| c.waiting.is_some()).count(), 2, "one per tab");
        // Another mount is not held up by it
        assert!(matches!(admit(&mut checks, check(200, "/n/a", Some("/n"), Some(1)), folder("/n/a", 1, 200)), Admit::Start(_)));
        // The mount is free once its question is over, and then hands out one waiting question
        assert!(take_waiting(&mut checks, "/m/a", &Some(PathBuf::from("/m"))).is_none(), "still busy");
        checks.retain(|c| c.id != 1);
        let (id, _) = take_waiting(&mut checks, "/m/a", &Some(PathBuf::from("/m"))).expect("a waiting question");
        assert!(id >= 98);
        assert!(take_waiting(&mut checks, "/m/a", &Some(PathBuf::from("/m"))).is_none(), "one at a time");
    }

    #[test]
    fn a_mount_that_did_not_answer_in_time_refuses_at_once_and_late_questions_are_not_sent() {
        let mut checks = Vec::new();
        assert!(matches!(admit(&mut checks, check(1, "/m/a", Some("/m"), Some(1)), folder("/m/a", 1, 1)), Admit::Start(_)));
        assert!(matches!(admit(&mut checks, check(2, "/m/b", Some("/m"), Some(2)), folder("/m/b", 2, 1)), Admit::Queued));
        // Both went unanswered for too long
        for c in checks.iter_mut() {
            c.late = true;
        }
        assert!(matches!(admit(&mut checks, check(3, "/m/c", Some("/m"), Some(1)), folder("/m/c", 1, 2)), Admit::Refused));
        assert_eq!(checks.len(), 2, "a refused question leaves nothing behind");
        checks.retain(|c| c.id != 1);
        assert!(take_waiting(&mut checks, "/m/a", &Some(PathBuf::from("/m"))).is_none(), "a late question is not sent");
    }

    #[test]
    fn a_navigation_counts_for_its_tab_alone() {
        let navs: HashMap<u64, u64> = [(1, 5), (2, 9)].into();
        assert!(nav_current(&navs, Some(1), 1, 5));
        assert!(!nav_current(&navs, Some(1), 1, 4), "overtaken in the same tab");
        assert!(!nav_current(&navs, Some(2), 1, 5), "that tab is not shown");
        assert!(!nav_current(&navs, Some(2), 3, 1), "a tab that is closed");
        assert!(nav_current(&navs, Some(2), 2, 9), "the other tab's newer number does not matter here");
    }

    #[test]
    fn a_hanging_mount_costs_a_thread_and_the_window_is_not_held_up() {
        let asked = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let mut checks = Vec::new();
        let started = Instant::now();
        for id in 1..=20u64 {
            if let Admit::Start(next) = admit(&mut checks, check(id, "/m/a", Some("/m"), Some(id)), folder("/m/a", id, id)) {
                let asked = asked.clone();
                spawn_check(id, next, tx.clone(), egui::Context::default(), move |_| {
                    asked.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(400));
                    (Some(PathBuf::from("/m/a")), true)
                });
            }
        }
        assert!(started.elapsed() < Duration::from_millis(200), "{:?}", started.elapsed());
        assert!(rx.try_recv().is_err(), "nothing has answered yet");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(asked.load(Ordering::SeqCst), 1, "20 clicks, one question");
        assert!(matches!(rx.recv_timeout(Duration::from_secs(3)), Ok(NetMsg::Checked { id: 1, is_dir: true, .. })));
    }

    #[test]
    fn a_folder_is_resolved_like_before_symlinks_missing_and_files() {
        let top = std::env::temp_dir().join(format!("compression_resolve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(top.join("real/sub")).unwrap();
        std::fs::write(top.join("file"), "x").unwrap();
        std::os::unix::fs::symlink(top.join("real"), top.join("link")).unwrap();
        let ask = |p: &Path, nearest| resolve_folder(p, nearest, |p| p.canonicalize().ok(), |p| p.is_dir());
        let real = top.canonicalize().unwrap().join("real");
        assert_eq!(ask(&top.join("link"), false), (Some(real.clone()), true), "a link is followed");
        assert_eq!(ask(&top.join("real/sub"), false), (Some(real.join("sub")), true));
        assert_eq!(ask(&top.join("none"), false), (Some(top.join("none")), false), "not there: not a folder");
        let file = top.canonicalize().unwrap().join("file");
        assert_eq!(ask(&top.join("file"), false), (Some(file), false), "a file is opened, not entered");
        // The nearest folder that is left
        assert_eq!(ask(&top.join("real/gone/deeper"), true), (Some(top.join("real")), true));
        assert_eq!(ask(&top.join("real/sub"), true), (Some(top.join("real/sub")), true));
        std::fs::remove_dir_all(&top).unwrap();
    }

    /// The next answer to a question on the channel the window reads (other messages are skipped).
    fn next_check(app: &CompressionApp) -> (u64, Option<PathBuf>, bool, Next) {
        loop {
            if let NetMsg::Checked { id, local, is_dir, next } = app.net.rx.recv_timeout(Duration::from_secs(5)).expect("an answer") {
                return (id, local, is_dir, next);
            }
        }
    }

    fn apply(app: &mut CompressionApp) {
        let (id, local, is_dir, next) = next_check(app);
        app.checked(&egui::Context::default(), id, local, is_dir, next);
    }

    #[test]
    fn going_into_a_folder_of_a_server_is_asked_on_a_thread_and_then_entered_without_asking_again() {
        let (mut app, top) = app_in_mount(2, "folder");
        let link = top.join("link");
        std::os::unix::fs::symlink(top.join("t0"), &link).unwrap();
        let started = Instant::now();
        app.navigate_to(link);
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(app.current_path, top.join("t1/sub"), "nothing moves before the server has answered");
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t0"), "the link was resolved on the thread");
        assert_eq!(app.back_history.last(), Some(&top.join("t1/sub")));
        assert!(app.net.checking.is_empty());
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn an_answer_for_a_folder_that_was_overtaken_navigates_nowhere() {
        let (mut app, top) = app_in_mount(2, "overtaken");
        app.navigate_to(top.join("t0"));
        app.navigate_to(top.join("t1"));
        assert_eq!(app.net.checking.iter().filter(|c| c.waiting.is_some()).count(), 1, "the second waits for the first: one question per mount");
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t1/sub"), "the first answer is out of date");
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t1"));
        // Going somewhere by other means counts as well
        app.navigate_to(top.join("t0"));
        app.go_back();
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t1/sub"));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn an_answer_for_a_tab_that_is_not_shown_or_closed_navigates_nothing() {
        let (mut app, top) = app_in_mount(3, "tabs");
        app.navigate_to(top.join("t0"));
        app.switch_tab(0);
        apply(&mut app);
        assert_eq!(folders(&app), vec![top.join("t0/sub"), top.join("t1/sub"), top.join("t2/sub")], "no tab was moved by it");
        // The other tab's own navigation still works, and a closed tab's does not
        app.navigate_to(top.join("t1"));
        let ctx = egui::Context::default();
        app.close_tab(&ctx, 0);
        apply(&mut app);
        assert_eq!(folders(&app), vec![top.join("t1/sub"), top.join("t2/sub")]);
        app.navigate_to(top.join("t2"));
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t2"));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_new_tab_on_a_server_opens_at_once_and_is_moved_to_what_the_path_resolves_to() {
        let (mut app, top) = app_in_mount(2, "newtab");
        let link = top.join("link");
        std::os::unix::fs::symlink(top.join("t0"), &link).unwrap();
        app.new_tab(link.clone());
        assert_eq!(app.current_path, link, "not resolved on the window's thread");
        let tab = app.tabs[app.active_tab].id;
        let history = app.back_history.len();
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t0"), "the link was followed, as for every other tab");
        assert_eq!(app.back_history.len(), history, "the tab did not go anywhere: no history entry");
        assert_eq!(app.tabs[app.active_tab].id, tab);
        // The same for a tab that is parked by then (a restored session opens several at once)
        app.new_tab(link.clone());
        app.switch_tab(0);
        apply(&mut app);
        assert_eq!(app.tabs[2].saved.as_ref().map(|s| s.folder().to_path_buf()), Some(top.join("t0")));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_new_tab_whose_path_was_left_meanwhile_is_not_moved_back() {
        let (mut app, top) = app_in_mount(2, "newtab_left");
        let link = top.join("link");
        std::os::unix::fs::symlink(top.join("t0"), &link).unwrap();
        app.new_tab(link.clone());
        // The person goes on before the server answers
        app.navigate_to(top.join("t1"));
        apply(&mut app);
        assert_eq!(app.current_path, link, "the first answer (about the tab's old path) is out of date and moves nothing");
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t1"));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn an_answer_from_an_earlier_connection_of_the_mount_navigates_nothing() {
        let (mut app, top) = app_in_mount(2, "generation");
        let before = app.current_path.clone();
        // Generation A: asked
        app.navigate_to(top.join("t0"));
        let a = app.net.checking[0].generation;
        // The server goes away and is connected again: generation B (no navigation of the tab happens in between)
        app.net.connection_changed(&top);
        apply(&mut app);
        assert_eq!(app.current_path, before, "the answer of connection A is not used");
        assert!(app.net.checking.is_empty());
        // Asked again, now in B: that one counts
        app.navigate_to(top.join("t0"));
        assert_ne!(app.net.checking[0].generation, a);
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t0"));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_closed_tab_is_never_taken_for_a_tab_opened_later() {
        let (mut app, top) = app_in_mount(2, "identity");
        let closed = app.tabs[app.active_tab].id;
        app.navigate_to(top.join("t0"));
        app.close_tab(&egui::Context::default(), app.active_tab);
        app.new_tab(top.join("t1/sub"));
        let fresh = app.tabs[app.active_tab].id;
        assert_ne!(closed, fresh, "ids are not reused");
        // The new tab was opened in a folder of the mount without asking; navigating it must not be taken for the old one's
        let shown = app.current_path.clone();
        apply(&mut app);
        assert_eq!(app.current_path, shown, "the closed tab's answer navigates nothing");
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_link_in_a_local_folder_that_leads_onto_a_server_is_asked_about_on_a_thread_too() {
        let (mut app, top) = app_in_mount(2, "locallink");
        let local = std::env::temp_dir().join(format!("compression_locallink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(&local).unwrap();
        std::os::unix::fs::symlink(top.join("t0"), local.join("to_server")).unwrap();
        std::os::unix::fs::symlink(local.join("to_server"), local.join("twice")).unwrap();
        let before = app.current_path.clone();
        // Not under the server by name, but it leads there: nothing is resolved on the window's thread
        app.navigate_to(local.join("twice/sub"));
        assert_eq!(app.current_path, before);
        assert_eq!(app.net.checking.len(), 1);
        apply(&mut app);
        assert_eq!(app.current_path, top.join("t0/sub"));
        // A plain local folder goes the same way and arrives as what it resolves to
        app.navigate_to(local.clone());
        apply(&mut app);
        assert_eq!(app.current_path, local.canonicalize().unwrap());
        std::fs::remove_dir_all(&local).unwrap();
        std::fs::remove_dir_all(&top).unwrap();
    }

    fn slow_resolution(next: &Next) -> (Option<PathBuf>, bool) {
        // The step that used to run on the window's thread: following links and asking what the path is
        std::thread::sleep(Duration::from_millis(700));
        ask_server(next)
    }

    #[test]
    fn when_following_the_path_itself_is_slow_no_navigation_entry_point_waits_for_it() {
        let (mut app, top) = app_in_mount(2, "slow_resolution");
        let local = std::env::temp_dir().join(format!("compression_slowres_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(local.join("real/sub")).unwrap();
        std::os::unix::fs::symlink(local.join("real"), local.join("link")).unwrap();
        app.net.ask = slow_resolution;
        let before = app.current_path.clone();
        let started = Instant::now();
        app.navigate_to(local.join("link"));
        app.navigate_to(local.join("link/sub"));
        app.new_tab(local.join("link"));
        app.go_up();
        app.navigate_to(PathBuf::from("/nonexistent_for_compression_test"));
        let spent = started.elapsed();
        assert!(spent < Duration::from_millis(400), "the entry points waited for the resolution: {spent:?}");
        // Only what needs no asking has happened: a new tab (as given) and the step up from it; the typed paths wait
        assert_ne!(app.current_path, local.join("link"), "{before:?}");
        // The answers arrive later, each from its own thread, and only then is anything resolved
        let mut resolved = 0;
        while !app.net.checking.is_empty() {
            let (id, local, is_dir, next) = next_check(&app);
            resolved += usize::from(local.is_some());
            app.checked(&egui::Context::default(), id, local, is_dir, next);
        }
        assert!(resolved >= 2);
        std::fs::remove_dir_all(&local).unwrap();
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn the_rename_dialog_and_its_batch_know_what_is_a_folder_from_the_listing() {
        let (mut app, top) = app_in_mount(1, "isdir");
        let file = top.join("t0/f.txt");
        std::fs::write(&file, "x").unwrap();
        let link = top.join("link");
        std::os::unix::fs::symlink(top.join("t0"), &link).unwrap();
        // From the listing; what it does not show is not guessed
        app.items = vec![FileItem::from_path(&top.join("t0/sub")).unwrap(), FileItem::from_path(&file).unwrap()];
        assert_eq!(app.item_is_dir(&top.join("t0/sub")), Some(true));
        assert_eq!(app.item_is_dir(&file), Some(false));
        assert_eq!(app.item_is_dir(&top.join("t0")), None, "unlisted: unknown, and no file system access to find out");
        // A batch with an item the list does not show is not opened with a guess
        app.open_batch_rename(vec![file.clone(), top.join("t0")]);
        assert!(app.dialog.is_none());
        app.open_batch_rename(vec![file.clone()]);
        assert!(matches!(app.dialog, Some(Dialog::BatchRename { .. })));
        // The single rename dialog keeps "unknown" as it is
        app.dialog = None;
        app.open_rename(top.join("t0"));
        assert!(matches!(app.dialog, Some(Dialog::Rename { is_dir: None, .. })));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_click_on_a_server_whose_earlier_answer_is_late_is_refused_and_a_late_answer_opens_nothing() {
        let (mut app, top) = app_in_mount(2, "late");
        let before = app.current_path.clone();
        app.navigate_to(top.join("t0"));
        app.net.checking[0].late = true;
        let other = app.net.checking[0].id;
        app.navigate_to(top.join("t1"));
        assert_eq!(app.net.checking.len(), 1, "no second thread, even for another folder of the same mount");
        let (id, local, is_dir, next) = next_check(&app);
        assert_eq!(id, other);
        app.checked(&egui::Context::default(), id, local, is_dir, next);
        assert_eq!(app.current_path, before);
        assert!(app.net.checking.is_empty());
        app.navigate_to(top.join("t0"));
        assert_eq!(app.net.checking.len(), 1, "once it has answered, the server can be asked again");
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_connect_dialog_that_was_closed_while_the_check_ran_opens_nothing() {
        let (mut app, top) = app_in_mount(2, "cancel");
        let before = app.current_path.clone();
        let address = Address::parse("sftp://me@nowhere.invalid/x").unwrap();
        app.net.checking.push(check(77, "", None, None));
        app.dialog = None;
        app.checked(&egui::Context::default(), 77, Some(top.join("t0")), false, Next::Dialog { address, password: None, save: true });
        assert_eq!(app.current_path, before);
        assert!(app.net.servers.saved.is_empty(), "a server the person gave up on is not saved");
        std::fs::remove_dir_all(&top).unwrap();
    }

    /// An app with two tabs on plain local folders `base/a/b` (shown) and `base/c/d`; every thread asked about is slow.
    fn local_app(name: &str) -> (CompressionApp, PathBuf) {
        let base = std::env::temp_dir().join(format!("compression_gone_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::create_dir_all(base.join("c/d")).unwrap();
        let base = base.canonicalize().unwrap();
        let ctx = egui::Context::default();
        let mut app = CompressionApp::new(base.join("a/b"), SerpantinumTheme::default(), Settings::default(), &ctx);
        app.new_tab(base.join("c/d"));
        while !app.net.checking.is_empty() {
            let (id, local, is_dir, next) = next_check(&app);
            app.checked(&ctx, id, local, is_dir, next);
        }
        app.net.ask = slow_resolution;
        (app, base)
    }

    fn waiting_connection(app: &mut CompressionApp, id: u64) -> mpsc::Receiver<Reply> {
        let (replies, answers) = mpsc::channel();
        app.net.pending.push(Pending { id, address: Address::parse("sftp://me@host/").unwrap(), open: OpenIn::Here, replies, asking: false, from_dialog: false, save: false, then: None, tabs: Vec::new(), later: Vec::new() });
        answers
    }

    fn login_prompt() -> Prompt {
        Prompt::Login { user: Some("me".into()), domain: None, fixed_user: false, anonymous: false, retry: false }
    }

    #[test]
    fn a_login_waits_for_the_dialog_on_screen_instead_of_replacing_it() {
        let (mut app, base) = local_app("prompt_waits");
        let ctx = app.ctx.clone();
        let answers = waiting_connection(&mut app, 7);
        // A question of a running copy is on screen: it is not the login's to take over
        app.dialog = Some(Dialog::Conflict { request: None, all: false });
        app.show_prompt(7, login_prompt());
        assert!(matches!(app.dialog, Some(Dialog::Conflict { .. })), "the dialog underneath stays");
        assert_eq!(app.net.waiting_prompts.len(), 1);
        // The login waits, and the connection with it: nobody answered "cancel" for it
        for _ in 0..3 {
            app.poll_network(&ctx);
        }
        assert!(answers.try_recv().is_err(), "a waiting login is not a cancelled one");
        assert!(matches!(app.dialog, Some(Dialog::Conflict { .. })));
        // Once the room is free, the login comes
        app.dialog = None;
        app.poll_network(&ctx);
        assert!(matches!(app.dialog, Some(Dialog::Login { id: 7, .. })));
        assert!(app.net.pending[0].asking);
        assert!(app.net.waiting_prompts.is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_login_of_a_connection_started_in_the_connect_dialog_comes_in_front_of_that_dialog() {
        let (mut app, base) = local_app("prompt_connect_dialog");
        let _answers = waiting_connection(&mut app, 4);
        app.net.pending[0].from_dialog = true;
        app.dialog = Some(Dialog::Connect { text: "sftp://me@host/".into(), save: true, error: None, first_frame: false, busy: Some(4) });
        app.show_prompt(4, login_prompt());
        assert!(matches!(app.dialog, Some(Dialog::Login { id: 4, .. })), "the dialog that waits for it must not hold it back");
        assert!(app.net.waiting_prompts.is_empty());
        // A connection that was not started there does not take the dialog over
        let _other = waiting_connection(&mut app, 8);
        app.dialog = Some(Dialog::Connect { text: String::new(), save: true, error: None, first_frame: false, busy: None });
        app.show_prompt(8, login_prompt());
        assert!(matches!(app.dialog, Some(Dialog::Connect { .. })));
        assert_eq!(app.net.waiting_prompts.len(), 1);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_login_whose_connection_is_over_is_dropped_and_several_come_one_after_the_other() {
        let (mut app, base) = local_app("prompts_in_turn");
        let ctx = app.ctx.clone();
        let _first = waiting_connection(&mut app, 1);
        let _second = waiting_connection(&mut app, 2);
        app.dialog = Some(Dialog::Conflict { request: None, all: false });
        app.show_prompt(1, login_prompt());
        app.show_prompt(2, login_prompt());
        app.show_prompt(3, login_prompt());
        // 3 has no connection: nothing to wait for
        assert_eq!(app.net.waiting_prompts.len(), 2);
        // The first connection ends while its login waits
        app.net.pending.retain(|p| p.id != 1);
        app.dialog = None;
        app.poll_network(&ctx);
        assert!(matches!(app.dialog, Some(Dialog::Login { id: 2, .. })), "the dead one is skipped, the next is shown");
        assert!(app.net.waiting_prompts.is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_login_that_is_shown_and_then_closed_by_another_dialog_still_cancels_its_connection() {
        let (mut app, base) = local_app("prompt_closed");
        let ctx = app.ctx.clone();
        let answers = waiting_connection(&mut app, 5);
        app.show_prompt(5, login_prompt());
        assert!(matches!(app.dialog, Some(Dialog::Login { id: 5, .. })));
        app.dialog = None;
        app.poll_network(&ctx);
        assert!(matches!(answers.try_recv(), Ok(Reply::Cancel)), "a login that was dismissed is a cancel, as before");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_list_of_servers_that_could_not_be_read_is_said_once_with_the_name_it_was_kept_under() {
        let (mut app, base) = local_app("servers_problem");
        app.status_toast = None;
        app.net.servers_problem = Some(network::LoadProblem::Kept("servers.json.broken".into()));
        app.announce_servers_problem();
        let said = app.status_toast.as_ref().map(|t| t.message.clone()).unwrap_or_default();
        assert!(said.contains("could not be read") && said.contains("servers.json.broken"), "{said:?}");
        app.status_toast = None;
        app.announce_servers_problem();
        assert!(app.status_toast.is_none(), "once");
        app.net.servers_problem = Some(network::LoadProblem::NotKept);
        app.announce_servers_problem();
        assert!(app.status_toast.as_ref().is_some_and(|t| t.message.contains("not written over")));
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn drain(app: &mut CompressionApp) {
        while !app.net.checking.is_empty() {
            apply(app);
        }
    }

    #[test]
    fn a_folder_the_monitor_calls_gone_is_checked_on_a_thread_and_the_window_moves_to_the_nearest_one_left() {
        let (mut app, base) = local_app("current");
        let shown = app.current_path.clone();
        std::fs::remove_dir_all(&shown).unwrap();
        let started = Instant::now();
        app.apply_disk_changes(Default::default(), false, vec![shown.clone()]);
        assert!(started.elapsed() < Duration::from_millis(400), "{:?}", started.elapsed());
        assert_eq!(app.current_path, shown, "not decided before the thread has answered");
        drain(&mut app);
        assert_eq!(app.current_path, base.join("c"));
        // A false alarm moves nothing
        let here = app.current_path.clone();
        app.apply_disk_changes(Default::default(), false, vec![here.clone()]);
        drain(&mut app);
        assert_eq!(app.current_path, here);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_parked_tab_whose_folder_is_gone_is_moved_by_a_thread_and_the_shown_tab_is_left_alone() {
        let (mut app, base) = local_app("parked");
        let shown = app.current_path.clone();
        let parked_idx = 1 - app.active_tab;
        std::fs::remove_dir_all(base.join("a/b")).unwrap();
        let started = Instant::now();
        app.apply_disk_changes(Default::default(), false, vec![base.join("a/b")]);
        assert!(started.elapsed() < Duration::from_millis(400), "{:?}", started.elapsed());
        drain(&mut app);
        assert_eq!(app.current_path, shown);
        assert_eq!(app.tabs[parked_idx].saved.as_ref().map(|s| s.folder().to_path_buf()), Some(base.join("a")));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn reopening_a_closed_tab_does_not_ask_about_its_folder_and_a_vanished_one_is_moved_after() {
        let (mut app, base) = local_app("reopen");
        let ctx = egui::Context::default();
        let gone = app.current_path.clone();
        app.close_tab(&ctx, app.active_tab);
        std::fs::remove_dir_all(&gone).unwrap();
        let started = Instant::now();
        app.reopen_tab();
        assert!(started.elapsed() < Duration::from_millis(400), "{:?}", started.elapsed());
        assert_eq!(app.tabs.len(), 2, "the tab is back at once");
        assert_eq!(app.current_path, gone);
        drain(&mut app);
        assert_eq!(app.current_path, base.join("c"), "and then moved to what is left");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_other_tabs_folder_is_offered_to_plugins_without_asking_the_file_system() {
        let (app, base) = local_app("others");
        let parked = 1 - app.active_tab;
        let there = app.tabs[parked].saved.as_ref().unwrap().folder().to_path_buf();
        std::fs::remove_dir_all(&there).unwrap();
        assert_eq!(app.other_tab_folders(), vec![there], "no is_dir: it is a folder the tab was in");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn without_noreplace_support_renaming_says_so_and_changes_nothing_while_a_taken_name_is_still_a_conflict() {
        let (mut app, base) = local_app("rename");
        crate::compat::tests::without_support_under(&base);
        let (file, folder) = (base.join("a/file.txt"), base.join("a/folder"));
        std::fs::write(&file, "data").unwrap();
        std::fs::create_dir_all(folder.join("inner")).unwrap();
        std::fs::write(base.join("a/taken"), "keep").unwrap();
        app.navigate_to(base.join("a"));
        drain(&mut app);
        app.net.ask = ask_server;
        assert_eq!(app.rename_entry(&file, "taken").unwrap_err(), "“taken” already exists");
        for (path, new) in [(&file, "renamed.txt"), (&folder, "dir")] {
            let err = app.rename_entry(path, new).unwrap_err();
            assert!(err.contains("does not support moving or renaming"), "{err}");
            assert!(!err.to_lowercase().contains("already exists"), "{err}");
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "data");
        assert!(folder.join("inner").is_dir() && !base.join("a/renamed.txt").exists() && !base.join("a/dir").exists());
        assert!(app.history.pop_undo().is_none(), "nothing happened, so there is nothing to undo");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A fake server folder that does not exist, so that only a stat could say anything about it.
    fn fake_server(name: &str) -> PathBuf {
        let top = PathBuf::from(format!("/nonexistent/compression_known_{name}_{}", std::process::id()));
        network::declare_remote_for_tests(&top);
        top
    }

    fn wait_for<T>(mut look: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(found) = look() {
                return found;
            }
            assert!(Instant::now() < deadline, "no answer in time");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// An app whose server questions wait for `release` and are counted.
    fn app_with_hanging_server() -> (CompressionApp, mpsc::Sender<()>, Arc<AtomicUsize>) {
        let (release, hold) = mpsc::channel::<()>();
        let (hold, calls) = (Mutex::new(hold), Arc::new(AtomicUsize::new(0)));
        let counted = calls.clone();
        let mut app = CompressionApp::new(std::env::temp_dir(), SerpantinumTheme::default(), Settings::default(), &egui::Context::default());
        app.net.known = Known::with_ask(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = hold.lock().unwrap().recv();
            Kind::Missing
        });
        (app, release, calls)
    }

    #[test]
    fn a_server_that_hangs_is_asked_once_and_a_frame_never_waits_for_it() {
        let (release, hold) = mpsc::channel::<()>();
        let (hold, calls) = (Mutex::new(hold), Arc::new(AtomicUsize::new(0)));
        let counted = calls.clone();
        let known = Known::with_ask(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = hold.lock().unwrap().recv();
            Kind::Dir
        });
        let (top, ctx) = (fake_server("hang"), egui::Context::default());
        let started = Instant::now();
        for i in 0..200 {
            assert_eq!(known.get(&top.join(format!("p{}", i % 3)), &ctx), None);
        }
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
        wait_for(|| (calls.load(Ordering::SeqCst) > 0).then_some(()));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one question per mount, however many frames ask");
        release.send(()).unwrap();
        assert_eq!(wait_for(|| known.get(&top.join("p0"), &ctx)), Kind::Dir);
    }

    #[test]
    fn an_old_answer_is_used_while_a_new_one_is_asked_for_and_a_fresh_one_is_not_asked_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let known = Known::with_ask(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Kind::Missing
        });
        let (path, ctx) = (fake_server("old").join("a"), egui::Context::default());
        known.state.lock().unwrap().kinds.insert(path.clone(), (Kind::Dir, Instant::now() - KNOWN_FOR * 2));
        assert_eq!(known.get(&path, &ctx), Some(Kind::Dir), "the old answer, at once");
        assert_eq!(wait_for(|| known.get(&path, &ctx).filter(|k| *k == Kind::Missing)), Kind::Missing);
        for _ in 0..50 {
            known.get(&path, &ctx);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a fresh answer is not asked for again");
    }

    #[test]
    fn a_pin_on_a_server_is_a_folder_until_told_otherwise_and_is_never_looked_at_in_the_frame() {
        let (app, release, calls) = app_with_hanging_server();
        let (pin, ctx) = (fake_server("pin").join("projects"), egui::Context::default());
        assert!(!pin.is_dir(), "a stat would say it is nothing");
        let started = Instant::now();
        for _ in 0..100 {
            assert!(app.pin_is_folder(&pin, &ctx));
        }
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(app.is_folder(&pin), "and a drop target on a server is taken for a folder too");
        release.send(()).unwrap();
        wait_for(|| (!app.pin_is_folder(&pin, &ctx)).then_some(()));
        assert!(!app.is_folder(&pin), "once the server has said it is not");
        assert!(calls.load(Ordering::SeqCst) >= 1);
        // A local pin is still looked at
        assert!(app.pin_is_folder(&std::env::temp_dir(), &ctx));
        assert!(!app.pin_is_folder(&std::env::temp_dir().join("compression_no_such_pin"), &ctx));
    }

    #[test]
    fn pasting_does_not_resolve_a_path_on_a_server() {
        // A link under a server's folder would be resolved only by asking the server
        let top = std::env::temp_dir().join(format!("compression_resolve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(top.join("real")).unwrap();
        std::os::unix::fs::symlink(top.join("real"), top.join("link")).unwrap();
        network::declare_remote_for_tests(&top);
        assert_eq!(CompressionApp::resolved(&top.join("link")), top.join("link"));
        std::fs::remove_dir_all(&top).unwrap();
        let local = std::env::temp_dir();
        assert_eq!(CompressionApp::resolved(&local), local.canonicalize().unwrap());
    }

    #[test]
    fn bookmarks_and_recent_files_on_a_server_are_asked_about_on_a_thread_and_then_listed() {
        let top = std::env::temp_dir().join(format!("compression_bookmarks_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        std::fs::write(top.join("on_server.txt"), "x").unwrap();
        let local = std::env::temp_dir().join(format!("compression_bookmarks_local_{}.txt", std::process::id()));
        std::fs::write(&local, "y").unwrap();
        network::declare_remote_for_tests(&top);
        let mut app = CompressionApp::new(std::env::temp_dir(), SerpantinumTheme::default(), Settings::default(), &egui::Context::default());
        app.bookmarks = vec![top.join("on_server.txt"), local.clone()];
        app.navigate_known(PathBuf::from(crate::fs::places::BOOKMARKS_PATH));
        // Nothing was asked of the server in this call: the listing is on its way
        app.reload_items();
        assert!(app.pending_reload.is_some(), "a listing with a server in it is read on a thread");
        wait_for(|| (!app.poll_reload()).then_some(()));
        let mut names: Vec<String> = app.items.iter().map(|i| i.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec![local.file_name().unwrap().to_string_lossy().into_owned(), "on_server.txt".to_owned()]);
        // Without a server in it the listing is there at once, as before
        app.bookmarks = vec![local.clone()];
        app.reload_items();
        assert!(app.pending_reload.is_none() && app.items.len() == 1);
        std::fs::remove_dir_all(&top).unwrap();
        std::fs::remove_file(&local).unwrap();
    }

    /// An app showing `dir` (which stands in for a folder on a server).
    fn app_on_server_folder(name: &str, files: &[&str]) -> (CompressionApp, PathBuf, egui::Context) {
        let dir = std::env::temp_dir().join(format!("compression_server_rename_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for file in files {
            std::fs::write(dir.join(file), file).unwrap();
        }
        let dir = dir.canonicalize().unwrap();
        let ctx = egui::Context::default();
        let app = CompressionApp::new(dir.clone(), SerpantinumTheme::default(), Settings::default(), &ctx);
        network::declare_remote_for_tests(&dir);
        (app, dir, ctx)
    }

    #[test]
    fn renaming_on_a_server_does_not_wait_for_it_and_then_shows_the_new_name_with_an_undo() {
        let (mut app, dir, ctx) = app_on_server_folder("slow", &["a.txt"]);
        let (release, hold) = mpsc::channel::<()>();
        let hold = Mutex::new(hold);
        app.net.rename = Arc::new(move |from, to| {
            let _ = hold.lock().unwrap().recv();
            crate::compat::rename_noreplace(from, to)
        });
        let started = Instant::now();
        assert_eq!(app.rename_entry(&dir.join("a.txt"), "b.txt"), Ok(()));
        assert!(started.elapsed() < Duration::from_millis(500), "the call waited for the server");
        assert!(dir.join("a.txt").exists() && app.history.undo_label().is_none(), "nothing happened yet");
        release.send(()).unwrap();
        wait_for(|| {
            app.poll_network(&ctx);
            app.history.undo_label().map(drop)
        });
        assert!(dir.join("b.txt").exists() && !dir.join("a.txt").exists());
        assert!(app.items.iter().any(|i| i.name == "b.txt") && !app.items.iter().any(|i| i.name == "a.txt"));
        assert!(app.history.undo_label().unwrap().starts_with("Renaming of"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_that_is_taken_on_a_server_is_told_and_nothing_is_replaced() {
        let (mut app, dir, ctx) = app_on_server_folder("taken", &["a.txt", "b.txt"]);
        assert_eq!(app.rename_entry(&dir.join("a.txt"), "b.txt"), Ok(()), "the answer comes from the server, later");
        wait_for(|| {
            app.poll_network(&ctx);
            app.status_toast.as_ref().map(|t| t.message.clone())
        });
        assert_eq!(app.status_toast.as_ref().unwrap().message, "“b.txt” already exists");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b.txt", "not replaced");
        assert!(dir.join("a.txt").exists() && app.history.undo_label().is_none());
        // Bad names are still refused on the spot
        assert!(app.rename_entry(&dir.join("a.txt"), "x/y").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Puts a renamer in the app that counts its calls and answers only when told to: `None` renames for real, a kind is an error.
    fn gated_rename(app: &mut CompressionApp) -> (Arc<AtomicUsize>, mpsc::Sender<Option<std::io::ErrorKind>>) {
        let (answer, hold) = mpsc::channel::<Option<std::io::ErrorKind>>();
        let (hold, calls) = (Mutex::new(hold), Arc::new(AtomicUsize::new(0)));
        let counted = calls.clone();
        app.net.rename = Arc::new(move |from, to| {
            counted.fetch_add(1, Ordering::SeqCst);
            match hold.lock().unwrap().recv_timeout(Duration::from_secs(10)) {
                Ok(Some(kind)) => Err(std::io::Error::from(kind)),
                _ => crate::compat::rename_noreplace(from, to),
            }
        });
        (calls, answer)
    }

    fn names(app: &CompressionApp) -> Vec<String> {
        let mut names: Vec<String> = app.items.iter().map(|i| i.name.clone()).collect();
        names.sort();
        names
    }

    fn press(key: egui::Key) -> Vec<egui::Event> {
        let m = egui::Modifiers::NONE;
        vec![egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: m }]
    }

    fn release(key: egui::Key) -> Vec<egui::Event> {
        let m = egui::Modifiers::NONE;
        vec![egui::Event::Key { key, physical_key: Some(key), pressed: false, repeat: false, modifiers: m }]
    }

    /// The rename dialog of `a.txt` on a server, drawn, with `new_name` typed in.
    fn rename_dialog(name: &str, files: &[&str], new_name: &str) -> (CompressionApp, PathBuf, egui::Context, eframe::Frame, f64) {
        let (mut app, dir, ctx) = app_on_server_folder(name, files);
        app.open_rename(dir.join("a.txt"));
        let (mut frame, mut time) = (eframe::Frame::_new_kittest(), 0.0);
        frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 6);
        if let Some(Dialog::Rename { name, .. }) = &mut app.dialog {
            *name = new_name.to_owned();
        }
        frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 2);
        (app, dir, ctx, frame, time)
    }

    #[test]
    fn an_answer_that_comes_after_a_change_of_folder_leaves_the_folder_shown_alone() {
        let (mut app, dir, ctx) = app_on_server_folder("elsewhere", &["a.txt"]);
        let other = std::env::temp_dir().join(format!("compression_server_rename_other_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other);
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("x.txt"), "x").unwrap();
        let other = other.canonicalize().unwrap();
        let (calls, answer) = gated_rename(&mut app);
        assert_eq!(app.rename_entry(&dir.join("a.txt"), "b.txt"), Ok(()));
        wait_for(|| (calls.load(Ordering::SeqCst) == 1).then_some(()));

        // The person goes to another folder while the server thinks
        app.current_path = other.clone();
        app.reload_items();
        wait_for(|| (!app.poll_reload()).then_some(()));
        assert_eq!(names(&app), ["x.txt"]);
        answer.send(None).unwrap();
        wait_for(|| {
            app.poll_network(&ctx);
            app.history.undo_label().map(drop)
        });
        // Nothing of the other folder's list is touched, nothing is selected there; the rename is still recorded and told
        assert_eq!(names(&app), ["x.txt"], "an item of another folder appeared in the list");
        assert!(app.selection.is_empty() && app.selected_index.is_none());
        assert_eq!(app.status_toast.as_ref().unwrap().message, "Renamed to “b.txt”");
        assert!(app.history.undo_label().unwrap().starts_with("Renaming of"));
        assert!(app.net.renaming.is_empty());
        assert!(dir.join("b.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn the_renamed_item_comes_from_the_thread_and_the_window_does_not_look_at_the_server() {
        let (mut app, dir, ctx) = app_on_server_folder("fromthread", &["a.txt"]);
        app.reload_items();
        wait_for(|| (!app.poll_reload()).then_some(()));
        // An item that exists nowhere: if the window read it itself, it would find nothing and show nothing
        let mut ghost = FileItem::sample("ghost.txt", crate::fs::file_item::FileType::Document);
        ghost.path = dir.join("ghost.txt");
        app.net.tx.send(NetMsg::Renamed { path: dir.join("a.txt"), target: dir.join("ghost.txt"), new_name: "ghost.txt".into(), item: Some(ghost), result: Ok(()) }).unwrap();
        app.poll_network(&ctx);
        assert_eq!(names(&app), ["ghost.txt"], "the item the thread read is the one shown");
        assert_eq!(app.selected_index.and_then(|i| app.items.get(i)).map(|i| i.name.as_str()), Some("ghost.txt"));
        // When the thread could not read it, nothing is looked up here either: the old name is gone, the next refresh has the new one
        app.net.tx.send(NetMsg::Renamed { path: dir.join("ghost.txt"), target: dir.join("a.txt"), new_name: "a.txt".into(), item: None, result: Ok(()) }).unwrap();
        app.poll_network(&ctx);
        assert!(names(&app).is_empty(), "{:?}", names(&app));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_search_listing_replaces_the_renamed_item_where_it_is() {
        let (mut app, dir, ctx) = app_on_server_folder("mixed", &["a.txt", "keep.txt"]);
        app.reload_items();
        wait_for(|| (!app.poll_reload()).then_some(()));
        app.search_query = "txt".to_owned();
        assert!(app.is_mixed_listing());
        let place = app.items.iter().position(|i| i.name == "a.txt").unwrap();
        let mut fresh = FileItem::sample("b.txt", crate::fs::file_item::FileType::Document);
        fresh.path = dir.join("b.txt");
        app.net.tx.send(NetMsg::Renamed { path: dir.join("a.txt"), target: dir.join("b.txt"), new_name: "b.txt".into(), item: Some(fresh.clone()), result: Ok(()) }).unwrap();
        app.poll_network(&ctx);
        assert_eq!(app.items.len(), 2);
        assert_eq!(app.items[place].name, "b.txt", "replaced where it was");
        // An item that is not in the results is not added to them
        app.net.tx.send(NetMsg::Renamed { path: dir.join("zzz.txt"), target: dir.join("yyy.txt"), new_name: "yyy.txt".into(), item: Some(fresh), result: Ok(()) }).unwrap();
        app.poll_network(&ctx);
        assert_eq!(app.items.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_rename_dialog_stays_for_the_server_and_a_second_enter_asks_nothing_more() {
        let (mut app, dir, ctx, mut frame, mut time) = rename_dialog("enter", &["a.txt"], "b.txt");
        let (calls, answer) = gated_rename(&mut app);
        // Enter twice in the same frame, and again later
        let mut twice = press(egui::Key::Enter);
        twice.extend(release(egui::Key::Enter));
        twice.extend(press(egui::Key::Enter));
        frames(&mut app, &ctx, &mut frame, &mut time, twice, 3);
        wait_for(|| (calls.load(Ordering::SeqCst) >= 1).then_some(()));
        frames(&mut app, &ctx, &mut frame, &mut time, press(egui::Key::Enter), 3);
        let shown = frames(&mut app, &ctx, &mut frame, &mut time, release(egui::Key::Enter), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a second request went out");
        assert!(matches!(app.dialog, Some(Dialog::Rename { waiting: true, error: None, .. })), "the dialog must wait for the answer, with nothing said against it");
        // The name cannot be typed over while it waits
        frames(&mut app, &ctx, &mut frame, &mut time, vec![egui::Event::Text("zz".into())], 3);
        assert!(matches!(&app.dialog, Some(Dialog::Rename { name, .. }) if name == "b.txt"), "the name was changed while waiting");
        assert!(shown.iter().any(|(t, _)| t == "Renaming…"), "{:?}", shown.iter().map(|s| &s.0).collect::<Vec<_>>());
        // A click on the button, which says what is going on, sends nothing either
        let button = shown.iter().find(|(t, _)| t == "Renaming…").unwrap().1.center();
        let m = egui::Modifiers::NONE;
        let primary = egui::PointerButton::Primary;
        frames(&mut app, &ctx, &mut frame, &mut time, vec![egui::Event::PointerMoved(button)], 3);
        frames(&mut app, &ctx, &mut frame, &mut time, vec![egui::Event::PointerButton { pos: button, button: primary, pressed: true, modifiers: m }], 2);
        frames(&mut app, &ctx, &mut frame, &mut time, vec![egui::Event::PointerButton { pos: button, button: primary, pressed: false, modifiers: m }], 4);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the button sent a second request");
        assert!(matches!(app.dialog, Some(Dialog::Rename { waiting: true, error: None, .. })), "a click on the waiting button must do nothing");
        assert!(app.net.is_being_renamed(&dir.join("a.txt")));

        // The answer closes the dialog, and the item is there once, under its new name
        answer.send(None).unwrap();
        wait_for(|| {
            frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 1);
            app.history.undo_label().map(drop)
        });
        frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 12);
        assert!(app.dialog.is_none() && app.closing_dialog.is_none() || app.dialog.is_none());
        assert_eq!(names(&app), ["b.txt"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!app.net.is_being_renamed(&dir.join("a.txt")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_rename_of_the_same_item_is_refused_until_the_first_is_answered() {
        let (mut app, dir, ctx) = app_on_server_folder("twice", &["a.txt"]);
        let (calls, answer) = gated_rename(&mut app);
        assert_eq!(app.rename_entry(&dir.join("a.txt"), "b.txt"), Ok(()));
        wait_for(|| (calls.load(Ordering::SeqCst) == 1).then_some(()));
        assert_eq!(app.rename_entry(&dir.join("a.txt"), "c.txt"), Err("“a.txt” is already being renamed".to_owned()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        answer.send(None).unwrap();
        wait_for(|| {
            app.poll_network(&ctx);
            app.history.undo_label().map(drop)
        });
        // Answered: the old name is gone, and so is the mark; another item is free to be renamed
        assert!(!app.net.is_being_renamed(&dir.join("a.txt")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_error_from_the_server_stays_in_the_dialog_with_the_name_and_can_be_sent_again() {
        let (mut app, dir, ctx, mut frame, mut time) = rename_dialog("error", &["a.txt", "b.txt"], "b.txt");
        let (calls, answer) = gated_rename(&mut app);
        frames(&mut app, &ctx, &mut frame, &mut time, press(egui::Key::Enter), 2);
        frames(&mut app, &ctx, &mut frame, &mut time, release(egui::Key::Enter), 2);
        wait_for(|| (calls.load(Ordering::SeqCst) == 1).then_some(()));
        answer.send(Some(std::io::ErrorKind::AlreadyExists)).unwrap();
        let shown = wait_for(|| {
            let shown = frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 1);
            (!app.net.is_being_renamed(&dir.join("a.txt"))).then_some(shown)
        });
        let shown = if shown.iter().any(|(t, _)| t == "“b.txt” already exists") { shown } else { frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 3) };
        // Open, with the reason in it, the name still typed, nothing said as a message, nothing changed
        match &app.dialog {
            Some(Dialog::Rename { name, error, waiting, .. }) => {
                assert_eq!(name, "b.txt");
                assert_eq!(error.as_deref(), Some("“b.txt” already exists"));
                assert!(!waiting, "the dialog must be editable again");
            }
            other => panic!("the dialog is gone: {}", other.is_some()),
        }
        assert!(shown.iter().any(|(t, _)| t == "“b.txt” already exists"), "the reason is drawn in the dialog");
        assert!(app.status_toast.is_none(), "an error that has a dialog is not also a message");
        assert!(dir.join("a.txt").exists() && app.history.undo_label().is_none());

        // Another name, sent again: a new request, and this time it goes through
        if let Some(Dialog::Rename { name, .. }) = &mut app.dialog {
            *name = "c.txt".to_owned();
        }
        frames(&mut app, &ctx, &mut frame, &mut time, press(egui::Key::Enter), 2);
        frames(&mut app, &ctx, &mut frame, &mut time, release(egui::Key::Enter), 2);
        wait_for(|| (calls.load(Ordering::SeqCst) == 2).then_some(()));
        answer.send(None).unwrap();
        wait_for(|| {
            frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 1);
            app.history.undo_label().map(drop)
        });
        assert!(dir.join("c.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn escape_while_the_server_works_closes_the_dialog_and_the_answer_comes_as_a_message() {
        for (outcome, expect) in [(None, "Renamed to “b.txt”"), (Some(std::io::ErrorKind::AlreadyExists), "“b.txt” already exists")] {
            let (mut app, dir, ctx, mut frame, mut time) = rename_dialog("escape", &["a.txt"], "b.txt");
            let (calls, answer) = gated_rename(&mut app);
            frames(&mut app, &ctx, &mut frame, &mut time, press(egui::Key::Enter), 2);
            frames(&mut app, &ctx, &mut frame, &mut time, release(egui::Key::Enter), 2);
            wait_for(|| (calls.load(Ordering::SeqCst) == 1).then_some(()));
            assert!(matches!(app.dialog, Some(Dialog::Rename { waiting: true, .. })));
            frames(&mut app, &ctx, &mut frame, &mut time, press(egui::Key::Escape), 2);
            frames(&mut app, &ctx, &mut frame, &mut time, release(egui::Key::Escape), 12);
            assert!(app.dialog.is_none(), "Escape must close the dialog while the server works");
            assert!(app.net.is_being_renamed(&dir.join("a.txt")), "the rename goes on");

            answer.send(outcome).unwrap();
            wait_for(|| {
                app.poll_network(&ctx);
                app.status_toast.as_ref().map(|t| t.message.clone())
            });
            assert_eq!(app.status_toast.as_ref().unwrap().message, expect);
            assert_eq!(app.history.undo_label().is_some(), outcome.is_none(), "an undo exists only for a rename that happened");
            assert!(app.dialog.is_none() && app.net.renaming.is_empty());
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn an_item_that_is_being_renamed_on_a_server_is_drawn_dimmed_until_the_answer() {
        let (mut app, dir, ctx) = app_on_server_folder("dim", &["a.txt", "other.txt"]);
        let (mut frame, mut time) = (eframe::Frame::_new_kittest(), 0.0);
        let name_color = |app: &mut CompressionApp, frame: &mut eframe::Frame, time: &mut f64, name: &str| -> egui::Color32 {
            fn find(shape: &egui::Shape, name: &str) -> Option<egui::Color32> {
                match shape {
                    egui::Shape::Text(t) if t.galley.text() == name => t.galley.job.sections.first().map(|s| s.format.color),
                    egui::Shape::Vec(v) => v.iter().find_map(|s| find(s, name)),
                    _ => None,
                }
            }
            let ctx = app.ctx.clone();
            let mut found = None;
            for _ in 0..25 {
                *time += 0.05;
                let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 700.0))), time: Some(*time), ..Default::default() };
                let mut out = ctx.run_ui(raw, |ui| eframe::App::ui(app, ui, frame));
                out.textures_delta.clear();
                found = out.shapes.iter().find_map(|c| find(&c.shape, name));
            }
            found.unwrap_or_else(|| panic!("{name} is not drawn"))
        };
        app.reload_items();
        wait_for(|| (!app.poll_reload()).then_some(()));
        // In the list and in the grid
        for (mode, from, to) in [(crate::app::ViewMode::List, "a.txt", "b.txt"), (crate::app::ViewMode::Grid, "b.txt", "a.txt")] {
            app.view_mode = mode;
            let normal = name_color(&mut app, &mut frame, &mut time, from);
            let (calls, answer) = gated_rename(&mut app);
            assert_eq!(app.rename_entry(&dir.join(from), to), Ok(()));
            wait_for(|| (calls.load(Ordering::SeqCst) == 1).then_some(()));
            let dimmed = name_color(&mut app, &mut frame, &mut time, from);
            let untouched = name_color(&mut app, &mut frame, &mut time, "other.txt");
            assert_ne!(dimmed, normal, "{mode:?}: the item being renamed looks like any other");
            assert_eq!(untouched, normal, "{mode:?}: another item is dimmed too");
            answer.send(None).unwrap();
            wait_for(|| {
                app.poll_network(&ctx);
                (!app.net.is_being_renamed(&dir.join(from))).then_some(())
            });
            assert_eq!(name_color(&mut app, &mut frame, &mut time, to), normal, "{mode:?}: the renamed item stays dimmed");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_size_of_a_folder_is_asked_for_without_looking_at_it_when_a_server_has_it() {
        let (mut app, dir, _ctx) = app_on_server_folder("size", &["file.txt"]);
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        app.reload_items();
        wait_for(|| (!app.poll_reload()).then_some(()));
        // In the listing: a folder is asked for, a file is not
        app.calculate_sizes(&[dir.join("folder"), dir.join("file.txt")]);
        assert_eq!(app.sizes.asked, vec![dir.join("folder")]);
        // Not in the listing, on a server: taken for a folder, and not looked at (it does not exist here at all)
        let elsewhere = dir.join("not_listed_and_not_there");
        app.calculate_sizes(std::slice::from_ref(&elsewhere));
        assert_eq!(app.sizes.asked, vec![elsewhere]);
        // Not in the listing and local: looked at, as before
        let local = std::env::temp_dir().join(format!("compression_size_local_{}", std::process::id()));
        std::fs::create_dir_all(&local).unwrap();
        app.calculate_sizes(&[local.clone(), local.join("missing")]);
        assert_eq!(app.sizes.asked, vec![local.clone()]);
        std::fs::remove_dir_all(&local).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn host_key_questions_are_retold() {
        let message = "The identity of the remote computer (example.com) is unknown.\nThe identity sent by the remote computer is SHA256:abc123.";
        let (title, body) = describe_question(message);
        assert_eq!(title, "Unknown server");
        assert!(body.contains("example.com") && body.contains("SHA256:abc123") && !body.contains("abc123.."));
        assert_eq!(choice_label("Log In Anyway"), "Connect anyway");
        assert_eq!(choice_label("Something"), "Something");
    }

    #[test]
    fn saved_servers_find_their_folder_inside_a_mount() {
        let mount = Mount {
            name: "x".into(),
            address: Address::parse("smb://nas/media"),
            local: PathBuf::from("/run/user/1/gvfs/smb-share:server=nas,share=media"),
            kind: network::MountKind::Gvfs,
        };
        let deep = Address::parse("smb://nas/media/films/2024").unwrap();
        assert_eq!(local_in(&mount, &deep), mount.local.join("films/2024"));
        assert_eq!(local_in(&mount, &Address::parse("smb://nas/media").unwrap()), mount.local);
    }

    #[test]
    fn kinds_tells_folders_files_and_what_is_not_there() {
        let top = std::env::temp_dir().join(format!("compression_kinds_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(top.join("d")).unwrap();
        std::fs::write(top.join("f"), "x").unwrap();
        let got = kinds(&[top.join("d"), top.join("f"), top.join("none")], ANSWER_WAIT);
        assert_eq!(got, vec![Some(Kind::Dir), Some(Kind::Other), Some(Kind::Missing)]);
        std::fs::remove_dir_all(&top).unwrap();
    }





    #[test]
    fn a_second_request_for_a_server_that_is_connecting_waits_for_it_and_is_carried_out_when_it_is_there() {
        let (mut app, base) = local_app("same_mount");
        let _answers = waiting_connection(&mut app, 7);
        let parse = |text: &str| Address::parse(text).unwrap();
        let (top, a, b) = (parse("sftp://me@host/"), parse("sftp://me@host/a"), parse("sftp://me@host/b"));

        // Another folder of the same mount: no second connection, the folder is kept for later
        assert_eq!(app.start_connecting(a.clone(), None, OpenIn::Here, false, false), 7);
        assert_eq!(app.net.pending.len(), 1);
        assert_eq!(app.net.pending[0].later, vec![(a.clone(), OpenIn::Here)]);
        // ... each one once, with the way it was asked for (a new window is not lost)
        assert_eq!(app.start_connecting(a.clone(), None, OpenIn::Here, false, false), 7);
        assert_eq!(app.start_connecting(a.clone(), None, OpenIn::NewWindow, false, false), 7);
        assert_eq!(app.start_connecting(b.clone(), None, OpenIn::NewTab, false, false), 7);
        assert_eq!(app.net.pending.len(), 1);
        assert_eq!(app.net.pending[0].later, vec![(a.clone(), OpenIn::Here), (a, OpenIn::NewWindow), (b, OpenIn::NewTab)]);
        // The very same request as the one on its way adds nothing
        assert_eq!(app.start_connecting(top, None, OpenIn::Here, false, false), 7);
        assert_eq!(app.net.pending[0].later.len(), 3);

        // Once the server is there they are asked for, one after the other
        let waiting = std::mem::take(&mut app.net.pending[0].later);
        let before = app.net.checking.len();
        app.replay_later(waiting);
        assert_eq!(app.net.checking.len(), before + 3);
        app.replay_later(Vec::new());
        assert_eq!(app.net.checking.len(), before + 3);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_connection_that_arrives_carries_out_what_waited_for_it() {
        let (mut app, base) = local_app("arrives");
        let ctx = app.ctx.clone();
        app.net.servers = crate::fs::network::Servers::locked_for_tests();
        let top = base.join("a");
        let a = Address::parse("sftp://me@host/a").unwrap();
        let mut asked = Vec::new();
        for later in [Vec::new(), vec![(a.clone(), OpenIn::Here), (a, OpenIn::NewTab)]] {
            let _answers = waiting_connection(&mut app, 7);
            app.net.pending[0].later = later.clone();
            let before = app.net.checking.len();
            app.connected(&ctx, 7, Ok(top.clone()));
            assert!(app.net.pending.is_empty());
            asked.push(app.net.checking.len() - before);
        }
        // Each request that waited is asked for once the server is there
        assert_eq!(asked[1] - asked[0], 2, "{asked:?}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_folder_treated_as_a_server_gets_the_rules_of_one_for_renaming_and_deleting_and_loses_them_when_treated_as_local() {
        let dir = std::env::temp_dir().join(format!("compression_treat_rules_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for file in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(file), file).unwrap();
        }
        let dir = dir.canonicalize().unwrap();
        let ctx = egui::Context::default();
        let mut app = CompressionApp::new(dir.clone(), SerpantinumTheme::default(), Settings::default(), &ctx);
        app.treat_folder_as_server(dir.clone(), true);

        // Renaming goes the way of a server: the call does not wait, the name changes when the thread has done it
        let (release, hold) = mpsc::channel::<()>();
        let hold = Mutex::new(hold);
        app.net.rename = Arc::new(move |from, to| {
            let _ = hold.lock().unwrap().recv();
            crate::compat::rename_noreplace(from, to)
        });
        assert_eq!(app.rename_entry(&dir.join("a.txt"), "a2.txt"), Ok(()));
        assert!(dir.join("a.txt").exists() && !dir.join("a2.txt").exists(), "nothing has happened yet");
        release.send(()).unwrap();
        wait_for(|| {
            app.poll_network(&ctx);
            app.history.undo_label().map(drop)
        });
        assert!(dir.join("a2.txt").exists());

        // Deleting asks, as for a server: there is no trash, the file is deleted for good or not at all
        app.trash_paths(vec![dir.join("b.txt")]);
        assert!(matches!(&app.dialog, Some(Dialog::Confirm { action: ConfirmAction::DeleteForever(paths), .. }) if *paths == [dir.join("b.txt")]));
        assert!(dir.join("b.txt").exists());

        // As local again: renaming is done on the spot, deleting goes to the trash without a question
        app.dialog = None;
        app.treat_folder_as_server(dir.clone(), false);
        assert_eq!(app.rename_entry(&dir.join("c.txt"), "c2.txt"), Ok(()));
        assert!(dir.join("c2.txt").exists() && !dir.join("c.txt").exists(), "a local rename happens at once");
        app.trash_paths(vec![dir.join("b.txt")]);
        assert!(app.dialog.is_none() && !dir.join("b.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs `n` frames of the window with the events given in the first one.
    fn frames(app: &mut CompressionApp, ctx: &egui::Context, frame: &mut eframe::Frame, time: &mut f64, events: Vec<egui::Event>, n: usize) -> Vec<(String, egui::Rect)> {
        fn texts(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.galley.rect.translate(t.pos.to_vec2()))),
                egui::Shape::Vec(v) => v.iter().for_each(|s| texts(s, out)),
                _ => {}
            }
        }
        let mut seen = Vec::new();
        for i in 0..n {
            *time += 0.05;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 700.0))),
                time: Some(*time),
                events: if i == 0 { events.clone() } else { Vec::new() },
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| eframe::App::ui(app, ui, frame));
            out.textures_delta.clear();
            seen.clear();
            for clipped in &out.shapes {
                texts(&clipped.shape, &mut seen);
            }
        }
        seen
    }

    #[test]
    fn a_click_beside_the_login_neither_closes_it_nor_cancels_the_connection_and_esc_cancels_it() {
        let (mut app, base) = local_app("login_modal");
        let ctx = app.ctx.clone();
        let (mut frame, mut time) = (eframe::Frame::_new_kittest(), 0.0);
        let answers = waiting_connection(&mut app, 7);
        app.show_prompt(7, login_prompt());
        frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 15);
        assert!(matches!(app.dialog, Some(Dialog::Login { id: 7, .. })) && app.net.pending[0].asking);
        // Something typed into the focused field, then a click in the empty corner of the window
        frames(&mut app, &ctx, &mut frame, &mut time, vec![egui::Event::Text("zzq".into())], 3);
        let typed = |app: &CompressionApp| match &app.dialog {
            Some(Dialog::Login { user, password, .. }) => Some(format!("{user}{password}")),
            _ => None,
        };
        assert!(typed(&app).is_some_and(|t| t.contains("zzq")), "{:?}", typed(&app));
        let (pos, none) = (egui::pos2(1090.0, 690.0), egui::Modifiers::NONE);
        frames(&mut app, &ctx, &mut frame, &mut time, vec![egui::Event::PointerMoved(pos)], 3);
        frames(&mut app, &ctx, &mut frame, &mut time, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: none }], 2);
        frames(&mut app, &ctx, &mut frame, &mut time, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: none }], 8);
        for _ in 0..3 {
            app.poll_network(&ctx);
        }
        // Still there, still holding what was typed, and the connection is not cancelled
        assert!(matches!(app.dialog, Some(Dialog::Login { id: 7, .. })), "the login closed by a click beside it");
        assert!(typed(&app).is_some_and(|t| t.contains("zzq")), "what was typed is gone");
        assert!(app.net.pending[0].asking, "the connection is no longer waiting for an answer");
        assert!(answers.try_recv().is_err(), "a Cancel was sent for a click that missed");
        // Esc is the way to cancel: the dialog closes and the connection is told
        let key = |pressed| egui::Event::Key { key: egui::Key::Escape, physical_key: Some(egui::Key::Escape), pressed, repeat: false, modifiers: none };
        frames(&mut app, &ctx, &mut frame, &mut time, vec![key(true)], 1);
        frames(&mut app, &ctx, &mut frame, &mut time, vec![key(false)], 5);
        app.poll_network(&ctx);
        assert!(app.dialog.is_none());
        assert!(matches!(answers.try_recv(), Ok(Reply::Cancel)), "Esc must cancel the connection");
        assert!(!app.net.pending[0].asking);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Opens the question of a server whose key was never seen, and runs it for a few frames.
    fn question_on_screen(name: &str) -> (CompressionApp, PathBuf, egui::Context, eframe::Frame, f64, mpsc::Receiver<Reply>) {
        let (mut app, base) = local_app(name);
        let ctx = app.ctx.clone();
        let (mut frame, mut time) = (eframe::Frame::_new_kittest(), 0.0);
        let answers = waiting_connection(&mut app, 7);
        let choices = vec!["Trust and connect".to_owned(), "Do not connect".to_owned()];
        app.show_prompt(7, Prompt::Question { message: "The key of this server was never seen before.".into(), choices });
        frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 15);
        assert!(matches!(app.dialog, Some(Dialog::NetQuestion { id: 7, .. })) && app.net.pending[0].asking);
        (app, base, ctx, frame, time, answers)
    }

    fn click(app: &mut CompressionApp, ctx: &egui::Context, frame: &mut eframe::Frame, time: &mut f64, pos: egui::Pos2) {
        let none = egui::Modifiers::NONE;
        frames(app, ctx, frame, time, vec![egui::Event::PointerMoved(pos)], 3);
        frames(app, ctx, frame, time, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: none }], 2);
        frames(app, ctx, frame, time, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: none }], 8);
    }

    #[test]
    fn the_security_question_of_a_server_is_modal_a_click_beside_it_answers_nothing_and_esc_declines() {
        let (mut app, base, ctx, mut frame, mut time, answers) = question_on_screen("question_modal");
        click(&mut app, &ctx, &mut frame, &mut time, egui::pos2(1090.0, 690.0));
        for _ in 0..3 {
            app.poll_network(&ctx);
        }
        assert!(matches!(app.dialog, Some(Dialog::NetQuestion { id: 7, .. })), "the question closed by a click beside it");
        assert!(app.net.pending[0].asking, "the connection is no longer waiting for an answer");
        assert!(answers.try_recv().is_err(), "an answer was sent for a click that missed");
        // Esc declines, and the connection is told
        let key = |pressed| egui::Event::Key { key: egui::Key::Escape, physical_key: Some(egui::Key::Escape), pressed, repeat: false, modifiers: egui::Modifiers::NONE };
        frames(&mut app, &ctx, &mut frame, &mut time, vec![key(true)], 1);
        frames(&mut app, &ctx, &mut frame, &mut time, vec![key(false)], 5);
        app.poll_network(&ctx);
        assert!(app.dialog.is_none());
        assert!(matches!(answers.try_recv(), Ok(Reply::Cancel)), "Esc must decline");
        assert!(!app.net.pending[0].asking);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_buttons_of_the_security_question_still_accept_and_decline() {
        for (label, expected) in [("Trust and connect", 0), ("Do not connect", 1)] {
            let (mut app, base, ctx, mut frame, mut time, answers) = question_on_screen("question_buttons");
            let shown = frames(&mut app, &ctx, &mut frame, &mut time, Vec::new(), 2);
            let at = shown.iter().find(|(t, _)| t == label).unwrap_or_else(|| panic!("{label} is not shown: {shown:?}")).1.center();
            click(&mut app, &ctx, &mut frame, &mut time, at);
            app.poll_network(&ctx);
            assert!(app.dialog.is_none(), "{label}: the question stays");
            assert!(matches!(answers.try_recv(), Ok(Reply::Choice(i)) if i == expected), "{label}");
            std::fs::remove_dir_all(&base).unwrap();
        }
    }
}
