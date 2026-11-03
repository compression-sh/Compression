//! Sideways two-finger swipes on a touchpad, read from the horizontal scroll the toolkit reports.
//! Wayland and X11 do not hand over real gesture events, so a swipe is a burst of mostly horizontal
//! scrolling: enough of it in one direction goes back or forward once, then input is ignored for a
//! moment so the long tail of a swipe doesn't navigate twice.
use eframe::egui::Vec2;

/// Scrolled points that make a swipe.
const THRESHOLD: f32 = 150.0;
/// After a swipe fired, input is ignored this long (seconds).
const COOLDOWN: f64 = 0.7;
/// A pause this long starts a new gesture (seconds).
const GAP: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swipe {
    Back,
    Forward,
}

#[derive(Debug, Default)]
pub struct SwipeDetector {
    accum: f32,
    last_input: f64,
    quiet_until: f64,
}

impl SwipeDetector {
    /// Feeds the scroll of one frame at time `now`; returns a swipe once it is long enough.
    /// Scrolling to the right (positive x) goes back, as in a browser.
    pub fn feed(&mut self, scroll: Vec2, now: f64) -> Option<Swipe> {
        // Mostly vertical scrolling is the list moving, not a swipe
        if scroll.x == 0.0 || scroll.x.abs() < scroll.y.abs() * 2.0 {
            if scroll.y != 0.0 {
                self.accum = 0.0;
            }
            return None;
        }
        if now < self.quiet_until {
            // The tail of a swipe that already fired: keep ignoring until the input stops for a moment
            self.quiet_until = self.quiet_until.max(now + GAP);
            return None;
        }
        if now - self.last_input > GAP {
            self.accum = 0.0;
        }
        self.last_input = now;
        // A swipe the other way restarts the count
        if self.accum != 0.0 && self.accum.signum() != scroll.x.signum() {
            self.accum = 0.0;
        }
        self.accum += scroll.x;
        if self.accum.abs() < THRESHOLD {
            return None;
        }
        let swipe = if self.accum > 0.0 { Swipe::Back } else { Swipe::Forward };
        self.accum = 0.0;
        self.quiet_until = now + COOLDOWN;
        Some(swipe)
    }
}

