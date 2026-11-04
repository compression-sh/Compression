//! Servers in the window: the Network section of the sidebar, the Connect to server, login and
//! question dialogs, and connections that run in the background until their folder can be shown.
use super::*;
use crate::fs::mount_health::{self, Health, Probes};
use crate::fs::network::{self, Address, Mount, MountKind, Progress, Prompt, Reply, Server, Servers};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(4);

/// Where a folder goes once its server is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenIn {
    Here,
    NewTab,
    NewWindow,
}

pub(super) enum NetMsg {
    Listed(Vec<Mount>),
    Ask { id: u64, prompt: Prompt },
    Done { id: u64, result: Result<PathBuf, Option<String>> },
    Gone { local: PathBuf, name: String, result: Result<(), String> },
    Forgot { name: String, result: Result<(), String> },
    /// A mount that was let go of to be connected again.
    /// A server's folder was asked about: `local` is it when it is there.
    Checked { id: u64, local: Option<PathBuf>, next: Next },
    Reconnect { local: PathBuf, name: String, address: Address, back: Option<PathBuf>, tabs: Vec<(u64, PathBuf)>, result: Result<(), String> },
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
}

pub(super) struct NetState {
    tx: mpsc::Sender<NetMsg>,
    rx: mpsc::Receiver<NetMsg>,
    pub(super) servers: Servers,
    pub(super) mounts: Vec<Mount>,
    pending: Vec<Pending>,
    checking: Vec<Check>,
    next_id: u64,
    listing: bool,
    last: Instant,
    /// Mount folders being disconnected.
    leaving: HashSet<PathBuf>,
    /// Whether the connected servers still answer.
    probes: Probes,
}

impl NetState {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, servers: Servers::load(), mounts: Vec::new(), pending: Vec::new(), checking: Vec::new(), next_id: 1, listing: false, last: Instant::now(), leaving: HashSet::new(), probes: Probes::new() }
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

    fn paint(self, ui: &egui::Ui, row: egui::Rect, theme: &SerpantinumTheme) {
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

/// What to do once it is known whether a server's folder is there.
pub(super) enum Next {
    /// A folder of a connected server, from the sidebar or the path: it is shown once the server answers.
    Folder { local: PathBuf },
    Open { address: Address, password: Option<String>, open: OpenIn },
    Back { address: Address, back: Option<PathBuf>, tabs: Vec<(u64, PathBuf)> },
    Dialog { address: Address, password: Option<String>, save: bool },
}

impl Next {
    /// The folder to ask about.
    fn local(&self) -> Option<PathBuf> {
        match self {
            Next::Folder { local } => Some(local.clone()),
            Next::Open { address, .. } | Next::Back { address, .. } | Next::Dialog { address, .. } => network::local_path(address),
        }
    }

    /// What two questions about the same thing have in common.
    fn key(&self) -> String {
        match self {
            Next::Folder { local } => local.to_string_lossy().into_owned(),
            Next::Open { address, .. } | Next::Back { address, .. } | Next::Dialog { address, .. } => address.display(),
        }
    }
}

/// A question to a server that has not been answered yet.
struct Check {
    id: u64,
    key: String,
    started: Instant,
    /// The window said "Not answering" and went on; an answer that comes now opens nothing.
    late: bool,
}

/// The checks that have waited longer than `wait` and were not marked yet: they are marked now, and their ids returned.
fn overdue(checks: &mut [Check], wait: Duration) -> Vec<u64> {
    checks.iter_mut().filter(|c| !c.late && c.started.elapsed() > wait).map(|c| {
        c.late = true;
        c.id
    }).collect()
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
                    self.net.mounts = mounts;
                }
                NetMsg::Ask { id, prompt } => self.show_prompt(id, prompt),
                NetMsg::Done { id, result } => self.connected(ctx, id, result),
                NetMsg::Checked { id, local, next } => self.checked(ctx, id, local, next),
                NetMsg::Gone { local, name, result } => {
                    self.net.leaving.remove(&local);
                    match result {
                        Ok(()) => {
                            if self.current_path.starts_with(&local) {
                                self.navigate_to(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
                            }
                            self.set_toast(trf("Disconnected from “{}”", &[&name]));
                        }
                        Err(err) => self.set_toast(trf("Could not disconnect “{}”: {}", &[&name, &err])),
                    }
                    self.request_network_listing(ctx, true);
                }
                NetMsg::Reconnect { local, name, address, back, tabs, result } => {
                    self.net.leaving.remove(&local);
                    self.request_network_listing(ctx, true);
                    match result {
                        Ok(()) => self.open_address_back(address, back, tabs),
                        Err(err) => self.set_toast(trf("Could not disconnect “{}”: {}", &[&name, &err])),
                    }
                }
                NetMsg::Forgot { name, result } => match result {
                    Ok(()) => self.set_toast(trf("Forgot the password of “{}”", &[&name])),
                    Err(err) => self.set_toast(trf("Could not forget the password: {}", &[&err])),
                },
            }
        }
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

    /// Ask the connected servers whether they still answer (see `mount_health`).
    fn probe_servers(&mut self, ctx: &egui::Context) {
        let servers: Vec<PathBuf> = self.net.mounts.iter().filter(|m| m.address.is_some() && !self.net.leaving.contains(&m.local)).map(|m| m.local.clone()).collect();
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

    /// Show `address`: at once when its server is connected, otherwise once connecting is done.
    pub(crate) fn open_address(&mut self, address: Address, password: Option<String>, open: OpenIn) {
        self.check_then(Next::Open { address, password, open });
    }

    /// Ask, on a thread of its own, whether the folder of `next`'s server is there, and carry on with `next` when
    /// it is known. A server that hangs cannot hold the window up: it is the window that stops waiting.
    /// `None` when an earlier question about the same server is still without an answer: asking again would only
    /// add a thread that hangs as well, so the window says at once that it is not answering.
    fn check_then(&mut self, next: Next) -> Option<u64> {
        let key = next.key();
        if self.net.checking.iter().any(|c| c.late && c.key == key) {
            self.set_toast(tr("Not answering"));
            return None;
        }
        let id = self.net.next_id;
        self.net.next_id += 1;
        self.net.checking.push(Check { id, key, started: Instant::now(), late: false });
        let local = next.local();
        let (tx, ctx) = (self.net.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let local = local.filter(|p| std::fs::metadata(p).is_ok());
            let _ = tx.send(NetMsg::Checked { id, local, next });
            ctx.request_repaint();
        });
        Some(id)
    }

    /// The answer to `check_then`: show the server's folder when it is there, else connect it.
    fn checked(&mut self, ctx: &egui::Context, id: u64, found: Option<PathBuf>, next: Next) {
        let Some(i) = self.net.checking.iter().position(|c| c.id == id) else { return };
        let check = self.net.checking.remove(i);
        // Whatever it was, the sidebar learns that the server answers
        self.request_network_listing(ctx, true);
        if check.late {
            return;
        }
        // The dialog was closed meanwhile: the person gave up on this
        if matches!(next, Next::Dialog { .. }) && !matches!(&self.dialog, Some(Dialog::Connect { busy: Some(b), .. }) if *b == id) {
            return;
        }
        match (next, found) {
            (Next::Folder { local }, _) => self.navigate_to(local),
            (Next::Open { open, .. }, Some(local)) => self.show_connected(local, open),
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
                self.show_connected(local, OpenIn::Here);
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
        // A second click on a server that is still connecting waits for the first
        if let Some(p) = self.net.pending.iter().find(|p| p.address == address) {
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
        self.net.pending.push(Pending { id, address, open, replies, asking: false, from_dialog, save, then: None, tabs: Vec::new() });
        id
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
                self.request_network_listing(ctx, true);
                self.show_back(local, pending.then, pending.tabs, pending.open);
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
            return self.show_connected(top, open);
        }
        let insides: Vec<PathBuf> = back.iter().cloned().chain(tabs.iter().map(|(_, inside)| inside.clone())).collect();
        let mut folders = folders_or_top(&top, &insides).into_iter();
        self.show_connected(if back.is_some() { folders.next().unwrap_or_else(|| top.clone()) } else { top.clone() }, open);
        let home = home_folder();
        for ((id, _), folder) in tabs.iter().zip(folders) {
            if let Some(state) = self.tabs.iter_mut().find(|slot| slot.id == *id).and_then(|slot| slot.saved.as_mut()).filter(|state| state.folder() == home) {
                state.set_folder(folder);
            }
        }
    }

    /// Show a folder of a connected server; a file is shown selected in its folder.
    fn show_connected(&mut self, local: PathBuf, open: OpenIn) {
        // The server has just answered (it was checked, or connected): asking again is quick
        let (folder, select) = if local.is_dir() || !local.exists() {
            (local, None)
        } else {
            (local.parent().map(Path::to_path_buf).unwrap_or_else(|| local.clone()), Some(local))
        };
        match open {
            OpenIn::Here => {
                self.close_search();
                self.navigate_to(folder);
                if let Some(file) = select {
                    self.select_path(&file);
                }
            }
            OpenIn::NewTab => self.new_tab(folder),
            OpenIn::NewWindow => self.open_window(&folder),
        }
    }

    /// The login or question dialog for a connection that waits for an answer.
    fn show_prompt(&mut self, id: u64, prompt: Prompt) {
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
        // Nothing may hold the folder open: leave it first
        self.leave_mount(&mount.local);
        let (tx, ctx) = (self.net.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = network::disconnect(&mount);
            let _ = tx.send(NetMsg::Gone { local: mount.local.clone(), name: mount.name.clone(), result });
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
        self.leave_mount(&mount.local);
        let (tx, ctx) = (self.net.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = network::disconnect(&mount);
            let _ = tx.send(NetMsg::Reconnect { local: mount.local.clone(), name: mount.name.clone(), address, back, tabs, result });
            ctx.request_repaint();
        });
    }

    /// The parked tabs that are in the mount: `(tab id, folder below its top)`.
    fn parked_in(&self, local: &Path) -> Vec<(u64, PathBuf)> {
        self.tabs.iter().filter_map(|slot| Some((slot.id, slot.saved.as_ref()?.folder().strip_prefix(local).ok()?.to_path_buf()))).collect()
    }

    /// Take every tab out of the folders of the mount, which is about to be let go of: the one on screen
    /// and the parked ones all go to the home folder.
    pub(super) fn leave_mount(&mut self, local: &Path) {
        let home = home_folder();
        if self.current_path.starts_with(local) {
            self.navigate_to(home.clone());
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
                resp = resp.on_hover_text(state.words());
            }
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
                    // A folder on a server is asked about first: a server that hangs must not hold the window up
                    if network::is_remote(&path) {
                        self.check_then(Next::Folder { local: path });
                    } else {
                        self.navigate_to(path);
                    }
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

