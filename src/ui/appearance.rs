//! The desktop's own look, as the XDG desktop portal tells it (`org.freedesktop.appearance`): whether it prefers light or
//! dark, and its accent color. The "System" theme follows it where no Serpantinum palette is installed. Asked on a thread
//! of its own, which then waits for the desktop's changes: a portal that does not answer holds nothing up.
use std::sync::mpsc;

use eframe::egui::{self, Color32};
use zbus::zvariant::{OwnedValue, Value};

/// What the desktop prefers; `None` where it says nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Appearance {
    pub light: Option<bool>,
    pub accent: Option<Color32>,
    /// The icon theme of the desktop, as GNOME keeps it (`org.gnome.desktop.interface icon-theme`), which the portal
    /// passes on and GTK itself follows under Wayland.
    pub icon_theme: Option<String>,
}

const NAMESPACE: &str = "org.freedesktop.appearance";
const GNOME: &str = "org.gnome.desktop.interface";
/// The settings of the portal that are asked for, by namespace and key.
const ASKED: [(&str, &str); 3] = [(NAMESPACE, "color-scheme"), (NAMESPACE, "accent-color"), (GNOME, "icon-theme")];

impl Appearance {
    /// Takes in one setting of the portal (`color-scheme`, `accent-color`, GNOME's `icon-theme`); others are left alone.
    pub fn take_from(&mut self, namespace: &str, key: &str, value: &Value<'_>) {
        if namespace == GNOME {
            if key == "icon-theme" {
                self.icon_theme = <&str>::try_from(unwrap(value)).ok().filter(|t| !t.is_empty()).map(str::to_owned);
            }
        } else if namespace == NAMESPACE {
            self.take(key, value);
        }
    }

    /// Takes in one setting of `org.freedesktop.appearance`.
    pub fn take(&mut self, key: &str, value: &Value<'_>) {
        let value = unwrap(value);
        match key {
            // 0: no preference, 1: dark, 2: light
            "color-scheme" => self.light = u32::try_from(value).ok().and_then(|v| match v {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            }),
            // Three doubles from 0 to 1; anything out of that range means "no accent"
            "accent-color" => self.accent = <(f64, f64, f64)>::try_from(value).ok().and_then(|(r, g, b)| {
                let byte = |c: f64| (0.0..=1.0).contains(&c).then(|| (c * 255.0).round() as u8);
                Some(Color32::from_rgb(byte(r)?, byte(g)?, byte(b)?))
            }),
            _ => {}
        }
    }
}

/// The value inside the variants the portal's older `Read` wraps it in.
fn unwrap<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(inner) => unwrap(inner),
        other => other,
    }
}

/// Starts asking the portal; each answer, and each change the desktop makes later, comes through the receiver (and the
/// window is woken for it). Nothing comes where there is no portal or no session bus.
pub fn watch(ctx: &egui::Context) -> mpsc::Receiver<Appearance> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("compression-appearance".into()).spawn(move || {
        let _ = follow(&tx, &ctx);
    });
    rx
}

/// The portal's answers to what is asked (`ASKED`), as they are now.
fn read(portal: &zbus::blocking::Proxy<'_>) -> Appearance {
    let mut now = Appearance::default();
    for (namespace, key) in ASKED {
        let value: zbus::Result<OwnedValue> = portal.call("ReadOne", &(namespace, key)).or_else(|_| portal.call("Read", &(namespace, key)));
        if let Ok(value) = value {
            now.take_from(namespace, key, &value);
        }
    }
    now
}

fn portal(connection: &zbus::blocking::Connection) -> zbus::Result<zbus::blocking::Proxy<'_>> {
    zbus::blocking::Proxy::new(connection, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings")
}

fn follow(tx: &mpsc::Sender<Appearance>, ctx: &egui::Context) -> zbus::Result<()> {
    let connection = zbus::blocking::Connection::session()?;
    let portal = portal(&connection)?;
    // Listening first, so a change made while the first answers come is not missed
    let changes = portal.receive_signal("SettingChanged")?;
    let mut now = read(&portal);
    let send = |appearance: &Appearance| {
        let sent = tx.send(appearance.clone()).is_ok();
        ctx.request_repaint();
        sent
    };
    if !send(&now) {
        return Ok(());
    }
    for message in changes {
        let Ok((namespace, key, value)) = message.body().deserialize::<(String, String, OwnedValue)>() else { continue };
        if ASKED.iter().any(|(n, k)| *n == namespace && *k == key) {
            now.take_from(&namespace, &key, &value);
            if !send(&now) {
                break;
            }
        }
    }
    Ok(())
}

/// What the desktop prefers, asked once and waited for at most `wait`: for the first frame, so a light desktop does not
/// see a dark window flash first. Without an answer in time, nothing is known (and the window takes the answer in later).
pub fn read_within(wait: std::time::Duration) -> Appearance {
    answer_within(wait, || {
        let connection = zbus::blocking::Connection::session().ok()?;
        Some(read(&portal(&connection).ok()?))
    })
}

/// `read_within` with the asking given, so the wait can be checked on its own.
fn answer_within(wait: std::time::Duration, ask: impl FnOnce() -> Option<Appearance> + Send + 'static) -> Appearance {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("compression-appearance-start".into()).spawn(move || {
        let _ = tx.send(ask());
    });
    rx.recv_timeout(wait).ok().flatten().unwrap_or_default()
}

static INITIAL: std::sync::RwLock<Option<Appearance>> = std::sync::RwLock::new(None);

/// Keeps what the desktop said at the start (see `read_within`), for the first theme and the first window.
pub fn set_initial(appearance: Appearance) {
    *INITIAL.write().unwrap_or_else(|e| e.into_inner()) = Some(appearance);
}

/// What the desktop said at the start; nothing known when it was not asked (tests, the portal service).
pub fn initial() -> Appearance {
    INITIAL.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portal_values_say_light_dark_and_the_accent() {
        let mut a = Appearance::default();
        a.take("color-scheme", &Value::U32(2));
        assert_eq!(a.light, Some(true));
        a.take("color-scheme", &Value::U32(1));
        assert_eq!(a.light, Some(false));
        a.take("color-scheme", &Value::U32(0));
        assert_eq!(a.light, None, "no preference");
        // The older `Read` answers inside one more variant
        a.take("color-scheme", &Value::Value(Box::new(Value::U32(2))));
        assert_eq!(a.light, Some(true));
        a.take("accent-color", &Value::from((1.0f64, 0.5f64, 0.0f64)));
        assert_eq!(a.accent, Some(Color32::from_rgb(255, 128, 0)));
        a.take("accent-color", &Value::from((-1.0f64, 0.5f64, 0.0f64)));
        assert_eq!(a.accent, None, "out of range is no accent");
        a.take("contrast", &Value::U32(1));
        assert_eq!(a.light, Some(true), "other keys change nothing");
        // GNOME's icon theme, in its own namespace
        a.take_from("org.gnome.desktop.interface", "icon-theme", &Value::from("Papirus-Dark"));
        assert_eq!(a.icon_theme.as_deref(), Some("Papirus-Dark"));
        a.take_from("org.freedesktop.appearance", "icon-theme", &Value::from("Other"));
        assert_eq!(a.icon_theme.as_deref(), Some("Papirus-Dark"), "only from GNOME's namespace");
        a.take_from("org.gnome.desktop.interface", "icon-theme", &Value::from(""));
        assert_eq!(a.icon_theme, None);
    }

    #[test]
    fn the_start_waits_for_the_desktop_only_a_moment() {
        let light = Appearance { light: Some(true), ..Appearance::default() };
        let quick = light.clone();
        assert_eq!(answer_within(std::time::Duration::from_secs(2), move || Some(quick)), light);
        let started = std::time::Instant::now();
        let slow = answer_within(std::time::Duration::from_millis(100), || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            Some(Appearance { light: Some(true), ..Appearance::default() })
        });
        assert_eq!(slow, Appearance::default(), "no answer in time: nothing known");
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
        assert_eq!(answer_within(std::time::Duration::from_secs(1), || None), Appearance::default(), "no portal");
    }
}
