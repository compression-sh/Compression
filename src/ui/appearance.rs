//! The desktop's own look, as the XDG desktop portal tells it (`org.freedesktop.appearance`): whether it prefers light or
//! dark, and its accent color. The "System" theme follows it where no Serpantinum palette is installed. Asked on a thread
//! of its own, which then waits for the desktop's changes: a portal that does not answer holds nothing up.
use std::sync::mpsc;

use eframe::egui::{self, Color32};
use zbus::zvariant::{OwnedValue, Value};

/// What the desktop prefers; `None` where it says nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Appearance {
    pub light: Option<bool>,
    pub accent: Option<Color32>,
}

const NAMESPACE: &str = "org.freedesktop.appearance";

impl Appearance {
    /// Takes in one setting of the portal (`color-scheme` or `accent-color`); others are left alone.
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

fn follow(tx: &mpsc::Sender<Appearance>, ctx: &egui::Context) -> zbus::Result<()> {
    use zbus::blocking::{Connection, Proxy};
    let connection = Connection::session()?;
    let portal = Proxy::new(&connection, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings")?;
    // Listening first, so a change made while the first answers come is not missed
    let changes = portal.receive_signal("SettingChanged")?;
    let mut now = Appearance::default();
    for key in ["color-scheme", "accent-color"] {
        let value: zbus::Result<OwnedValue> = portal.call("ReadOne", &(NAMESPACE, key)).or_else(|_| portal.call("Read", &(NAMESPACE, key)));
        if let Ok(value) = value {
            now.take(key, &value);
        }
    }
    let send = |appearance: Appearance| {
        let sent = tx.send(appearance).is_ok();
        ctx.request_repaint();
        sent
    };
    if !send(now) {
        return Ok(());
    }
    for message in changes {
        let Ok((namespace, key, value)) = message.body().deserialize::<(String, String, OwnedValue)>() else { continue };
        if namespace == NAMESPACE {
            now.take(&key, &value);
            if !send(now) {
                break;
            }
        }
    }
    Ok(())
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
    }
}
