//! Drives in the sidebar: keep the list fresh, mount, unmount and eject without blocking the window.
use super::*;
use crate::fs::drives;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(4);

pub(super) enum DriveMsg {
    Listed(Vec<Place>, HashMap<PathBuf, drives::Space>),
    Mounted { dev: String, name: String, open: bool, result: Result<PathBuf, String> },
    Gone { dev: String, name: String, eject: bool, mount: PathBuf, result: Result<(), String> },
}

pub(super) struct DriveState {
    tx: mpsc::Sender<DriveMsg>,
    rx: mpsc::Receiver<DriveMsg>,
    last: Instant,
    listing: bool,
    /// Devices a mount or eject is working on.
    pub(super) busy: HashSet<String>,
    /// How full each mounted drive and the computer's own disk are, by the folder they are at.
    pub(super) spaces: HashMap<PathBuf, drives::Space>,
}

impl DriveState {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, last: Instant::now(), listing: false, busy: HashSet::new(), spaces: HashMap::new() }
    }
}

/// Tests only (not in the program): waits until the listing of the drives the window asked for at start has come, then
/// holds further listings back (as if one were on its way), so the sidebar's rows stay where a test found them. The
/// window asks again every few seconds (`REFRESH`) by itself. The 30 s are a watchdog.
#[cfg(test)]
impl CompressionApp {
    pub(crate) fn hold_drive_listing_once_it_came(&mut self, ctx: &egui::Context) {
        let limit = Instant::now() + Duration::from_secs(30);
        while self.drive_state.listing {
            assert!(Instant::now() < limit, "the listing of the drives never came");
            self.poll_drives(ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        self.drive_state.listing = true;
    }
}

impl CompressionApp {
    /// Apply finished work and ask for a new listing every few seconds.
    pub(super) fn poll_drives(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.drive_state.rx.try_recv() {
            match msg {
                DriveMsg::Listed(found, spaces) => {
                    self.drive_state.listing = false;
                    self.drive_state.spaces = spaces;
                    self.set_drive_places(found);
                }
                DriveMsg::Mounted { dev, name, open, result } => {
                    self.drive_state.busy.remove(&dev);
                    match result {
                        Ok(path) => {
                            if open {
                                self.close_search();
                                self.navigate_to(path);
                            }
                            self.request_drive_listing(ctx, true);
                        }
                        Err(err) => self.set_toast(match drives::plain_error(&err) {
                            Some(why) => trf("Could not mount “{}”: {}", &[&name, &why]),
                            None => tr("Authentication cancelled").to_owned(),
                        }),
                    }
                }
                DriveMsg::Gone { dev, name, eject, mount, result } => {
                    self.drive_state.busy.remove(&dev);
                    self.let_go(&format!("drive:{dev}"));
                    match result {
                        Ok(()) => {
                            if self.current_path.starts_with(&mount) {
                                self.navigate_known(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
                            }
                            self.set_toast(if eject { trf("“{}” can be removed safely", &[&name]) } else { trf("Unmounted “{}”", &[&name]) });
                            self.request_drive_listing(ctx, true);
                        }
                        Err(err) => self.set_toast(match drives::plain_error(&err) {
                            Some(why) => trf("Could not unmount “{}”: {}", &[&name, &why]),
                            None => tr("Authentication cancelled").to_owned(),
                        }),
                    }
                }
            }
        }
        self.request_drive_listing(ctx, false);
        ctx.request_repaint_after(REFRESH);
    }

    pub(crate) fn request_drive_listing(&mut self, ctx: &egui::Context, now: bool) {
        let state = &mut self.drive_state;
        if state.listing || !now && state.last.elapsed() < REFRESH {
            return;
        }
        state.listing = true;
        state.last = Instant::now();
        let (tx, ctx) = (state.tx.clone(), ctx.clone());
        let all = self.settings.show_all_partitions;
        std::thread::spawn(move || {
            let mut found = drives::list(all);
            let mut spaces = HashMap::new();
            for place in &mut found {
                if place.drive.as_ref().is_none_or(|d| d.mounted) {
                    place.path = place.path.canonicalize().unwrap_or_else(|_| place.path.clone());
                    spaces.extend(drives::Space::of(&place.path).map(|s| (place.path.clone(), s)));
                }
            }
            // The sidebar's Computer
            spaces.extend(drives::Space::of(Path::new("/")).map(|s| (PathBuf::from("/"), s)));
            let _ = tx.send(DriveMsg::Listed(found, spaces));
            ctx.request_repaint();
        });
    }

    /// Mounted drives come with their folder already canonical (see `request_drive_listing`).
    fn set_drive_places(&mut self, found: Vec<Place>) {
        let same = |a: &Place, b: &Place| a.name == b.name && a.path == b.path && a.drive == b.drive;
        let old: Vec<&Place> = self.places.iter().filter(|p| p.drive.is_some()).collect();
        if old.len() == found.len() && old.iter().zip(&found).all(|(a, b)| same(a, b)) {
            return;
        }
        self.places.retain(|p| p.drive.is_none());
        self.places.extend(found);
    }

    /// Mount a device and, when `open`, show it once it is there.
    pub(super) fn mount_drive(&mut self, ctx: &egui::Context, dev: String, name: String, open: bool) {
        if !self.drive_state.busy.insert(dev.clone()) {
            return;
        }
        let (tx, ctx) = (self.drive_state.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = drives::mount(&dev);
            let _ = tx.send(DriveMsg::Mounted { dev, name, open, result });
            ctx.request_repaint();
        });
    }

    /// Unmount a device; `eject` also powers a removable drive off.
    pub(super) fn unmount_drive(&mut self, ctx: &egui::Context, dev: String, name: String, mount: PathBuf, eject: bool) {
        // A job that needs the drive is waited for: the system refuses a drive that is busy, but not between two files.
        // Ejecting powers the whole drive off, so a job on another of its partitions needs it too, and so does one on a
        // partition mounted while it is being ejected: the drive itself is held, by its name, until the answer
        let mut points = vec![mount.clone()];
        let mut whole = None;
        if eject {
            let Some(drive) = drives::drive_of_device(&dev) else {
                self.set_toast(trf("Which drive “{}” is on could not be told, so it was not ejected. Unmount it instead.", &[&name]));
                return;
            };
            points.extend(drives::mount_points_of_drive(&dev));
            whole = Some(crate::fs::resources::Resource::Drive(drive));
        }
        let mut resources = crate::fs::resources::of_all(points.iter().filter(|p| !p.as_os_str().is_empty()));
        resources.extend(whole);
        if let Some(job) = self.job_needing(&resources) {
            self.set_toast(if eject {
                trf("“{}” is still needed by “{}”. Wait until it is done, then eject.", &[&name, &job])
            } else {
                trf("“{}” is still needed by “{}”. Wait until it is done, then unmount.", &[&name, &job])
            });
            return;
        }
        if !self.drive_state.busy.insert(dev.clone()) {
            return;
        }
        self.hold_back(format!("drive:{dev}"), resources, name.clone());
        let (tx, ctx) = (self.drive_state.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = if eject { drives::eject(&dev) } else { drives::unmount(&dev) };
            let _ = tx.send(DriveMsg::Gone { dev, name, eject, mount, result });
            ctx.request_repaint();
        });
    }
}

/// Small round button at the right end of a sidebar row, shown on hover; true when clicked.
pub(super) fn row_button(ui: &mut egui::Ui, row: egui::Rect, row_hovered: bool, id: impl std::hash::Hash + std::fmt::Debug, icon: &str, tip: &str, theme: &SerpantinumTheme) -> bool {
    let size = 28.0;
    let rect = egui::Rect::from_center_size(egui::pos2(row.right() - 8.0 - size / 2.0, row.center().y), egui::vec2(size, size));
    let resp = ui.interact(rect, ui.make_persistent_id(id), egui::Sense::click());
    let show = material::animate(ui.ctx(), resp.id.with("show"), row_hovered || resp.hovered());
    if show > 0.0 {
        let hover = material::animate(ui.ctx(), resp.id.with("hov"), resp.hovered());
        if hover > 0.0 {
            ui.painter().circle_filled(rect.center(), size / 2.0, theme.with_alpha(theme.text, (hover * material::ALPHA_HOVER as f32) as u8));
        }
        let color = material::lerp_color(theme.subtext0, theme.text, hover);
        material::paint_icon(ui.painter(), rect.center(), icon, material::ICON_SM, color.gamma_multiply(show));
    }
    resp.on_hover_text(tip).clicked()
}

/// How full the drive at `path` is, as a bar under its name, and the words for its tooltip.
pub(super) fn space_hint(spaces: &HashMap<PathBuf, drives::Space>, path: &Path, decimal: bool) -> Option<(f32, String)> {
    let space = spaces.get(path)?;
    let fmt = |b| crate::fs::file_item::format_bytes_ext(b, decimal);
    Some((space.fraction(), trf("{} free of {}", &[&fmt(space.free), &fmt(space.total)])))
}
