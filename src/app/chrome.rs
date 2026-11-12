//! The frame every Compression window is built from: the main window, the file dialog and the Copy to /
//! Move to picker all take their window settings and their panels from here, so a change to how
//! the window looks reaches all of them at once.

use eframe::egui;
use crate::ui::theme::SerpantinumTheme;
use super::{CONTENT_INSET, HEADER_MARGIN, PAGE_MARGIN};

/// Width of the places column on the left.
pub(super) const SIDEBAR_WIDTH: f32 = 212.0;

/// A window without a frame of its own: the header carries the title, the close button and the
/// drag, and the backgrounds may be translucent.
pub fn viewport(title: impl Into<String>) -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_title(title)
        // Compositors without server-side decorations (niri) would add a bar
        .with_decorations(false)
        .with_transparent(true)
}

/// Closes the deferred window whose pass this is, without waiting on the window that declares it.
///
/// eframe only drops a deferred window when a pass of its parent no longer declares it, and the
/// parent (the main window) may be on another workspace, where it gets no frame callbacks and
/// runs no pass; Wayland cannot hide a window meanwhile, so it stays up as an empty pane. So the
/// window declares itself as its own child with nothing to draw, and asks for one more pass of
/// its own: that pass does not declare it, and egui drops it at the end like any unused child.
/// If its parent declares it again first (it was opened again), the parent takes it back.
pub(super) fn close_own_viewport(ctx: &egui::Context) {
    // An empty builder: none of the window's settings change, so it is neither altered nor remade
    ctx.show_viewport_deferred(ctx.viewport_id(), egui::ViewportBuilder::default(), |_, _| {});
    ctx.request_repaint();
}

/// Lets go of a deferred window that was closed but is still around.
///
/// A window that closed itself (`close_own_viewport`) is left as its own parent, and egui keeps a
/// window whose parent is alive. A repaint asked for it after it was dropped (its close asks for
/// two) brings it back with that parent, so it then never goes: an empty window stays up, the main
/// window redraws without pause, and closing the main window leaves it hanging. Its parent takes
/// it back for one pass with nothing to draw, and egui drops it on the next as an unused child.
pub(super) fn reap_closed_viewport(ctx: &egui::Context, id: egui::ViewportId) {
    if !ctx.input(|i| i.raw.viewports.contains_key(&id)) {
        return;
    }
    let pass = ctx.cumulative_pass_nr();
    let key = egui::Id::unique("compression_reaped_viewport").with(id);
    // Declared every pass it would be kept; every other pass lets it go
    if ctx.data(|d| d.get_temp::<u64>(key)).is_some_and(|last| last + 1 == pass) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(key, pass));
    ctx.show_viewport_deferred(id, egui::ViewportBuilder::default(), |_, _| {});
    ctx.request_repaint();
}

/// The places column, `width` wide while it slides in or out.
pub(super) fn sidebar<R>(ui: &mut egui::Ui, id: &str, width: f32, theme: &SerpantinumTheme, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let panel = egui::Panel::left(id)
        .show_separator_line(false)
        .exact_size(width.max(1.0))
        .resizable(false)
        // The top matches the header's, so the search and menu buttons sit on its centre line
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: PAGE_MARGIN, right: PAGE_MARGIN, top: 4, bottom: 8 }));
    backdrop(ui, theme.mantle, |ui| panel.show(ui, add))
}

/// The row of controls along the top.
pub(super) fn header<R>(ui: &mut egui::Ui, id: &str, theme: &SerpantinumTheme, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let panel = egui::Panel::top(id)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: HEADER_MARGIN, right: 8, top: 4, bottom: 4 }));
    backdrop(ui, theme.base, |ui| panel.show(ui, add))
}

/// A header on a `fill` of its own, taller than the plain one, for windows whose title needs
/// room above it and a band that sets it apart from the page.
pub(super) fn banner<R>(ui: &mut egui::Ui, id: &str, fill: egui::Color32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let panel = egui::Panel::top(id)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: HEADER_MARGIN, right: 8, top: BANNER_TOP, bottom: 6 }));
    backdrop(ui, fill, |ui| panel.show(ui, add))
}

/// Space above a banner's row; whatever sits beside it must start as far down.
pub(super) const BANNER_TOP: i8 = 8;

/// A strip under the header of a fixed `height`, with the header's side margins.
pub(super) fn header_strip<R>(ui: &mut egui::Ui, id: &str, height: f32, theme: &SerpantinumTheme, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let panel = egui::Panel::top(id)
        .show_separator_line(false)
        .exact_size(height.max(1.0))
        .resizable(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(HEADER_MARGIN, 0)));
    backdrop(ui, theme.base, |ui| panel.show(ui, add))
}

/// The bar of a dialog's buttons along the bottom.
pub(super) fn footer<R>(ui: &mut egui::Ui, id: &str, theme: &SerpantinumTheme, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let panel = egui::Panel::bottom(id)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(CONTENT_INSET, 10)));
    backdrop(ui, theme.mantle, |ui| panel.show(ui, add))
}

/// What is left in the middle: the listing.
pub(super) fn content<R>(ui: &mut egui::Ui, theme: &SerpantinumTheme, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
    let panel = egui::CentralPanel::default()
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: PAGE_MARGIN, right: PAGE_MARGIN, top: 0, bottom: 6 }));
    let slot = ui.painter().add(egui::Shape::Noop);
    let response = panel.show(ui, add);
    ui.painter().set(slot, flat_rect(ui, response.response.rect, theme.base));
    response
}

/// Paints a panel's background under what `show` draws. A frame's own fill is anti-aliased, so
/// where two translucent panels meet both half-cover the same pixels and the desktop shows
/// through as a seam; these rects are snapped to whole pixels and meet edge to edge instead.
fn backdrop<R>(ui: &mut egui::Ui, fill: egui::Color32, show: impl FnOnce(&mut egui::Ui) -> egui::InnerResponse<R>) -> R {
    let slot = ui.painter().add(egui::Shape::Noop);
    let response = show(ui);
    ui.painter().set(slot, flat_rect(ui, response.response.rect, fill));
    response.inner
}

fn flat_rect(ui: &egui::Ui, rect: egui::Rect, fill: egui::Color32) -> egui::Shape {
    let rect = crate::ui::material::snap(ui.painter(), rect);
    // The corners of the window are rounded where this backdrop reaches them (see `window_rounding`)
    let ctx = ui.ctx();
    let (maximized, fullscreen) = ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport().fullscreen.unwrap_or(false)));
    let r = window_rounding(compositor_rounds_windows(), maximized, fullscreen, crate::ui::material::radius_window());
    let corners = corners_at_window_edge(rect, ctx.content_rect(), r);
    if corners != egui::CornerRadius::ZERO {
        return egui::Shape::rect_filled(rect, corners, fill);
    }
    let mut mesh = egui::Mesh::default();
    mesh.add_colored_rect(rect, fill);
    egui::Shape::mesh(mesh)
}

/// Whether the compositor rounds the corners of windows itself: Hyprland does (its `decoration:rounding`), so the window
/// stays square there and the two roundings do not fight.
fn compositor_rounds_windows() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|v| !v.is_empty())
}

/// How round the corners of the window are: `radius`, or square where the compositor rounds windows or the window fills
/// the screen (maximized or fullscreen), as a window with its own decorations does.
fn window_rounding(compositor_rounds: bool, maximized: bool, fullscreen: bool, radius: u8) -> u8 {
    if compositor_rounds || maximized || fullscreen { 0 } else { radius }
}

/// The rounding of `rect` where its corners are the window's (`window`), `r` there and none elsewhere.
fn corners_at_window_edge(rect: egui::Rect, window: egui::Rect, r: u8) -> egui::CornerRadius {
    let near = |a: f32, b: f32| (a - b).abs() <= 0.5;
    let (left, right, top, bottom) = (near(rect.left(), window.left()), near(rect.right(), window.right()), near(rect.top(), window.top()), near(rect.bottom(), window.bottom()));
    let at = |yes: bool| if yes { r } else { 0 };
    egui::CornerRadius { nw: at(left && top), ne: at(right && top), sw: at(left && bottom), se: at(right && bottom) }
}


/// Makes the empty part of a header around `rect` move the window, as a title bar would.
pub(super) fn drag_area(ui: &egui::Ui, rect: egui::Rect, id: egui::Id) {
    let drag = ui.interact(rect, id, egui::Sense::click_and_drag());
    if drag.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_round_at_its_own_corners_unless_the_compositor_rounds_it_or_it_fills_the_screen() {
        assert_eq!(window_rounding(false, false, false, 12), 12);
        assert_eq!(window_rounding(true, false, false, 12), 0, "Hyprland rounds windows itself");
        assert_eq!(window_rounding(false, true, false, 12), 0, "maximized");
        assert_eq!(window_rounding(false, false, true, 12), 0, "fullscreen");
        let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0));
        // The sidebar: the left corners; the header beside it: the top right; the listing: the bottom right
        let side = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(200.0, 700.0));
        assert_eq!(corners_at_window_edge(side, window, 12), egui::CornerRadius { nw: 12, ne: 0, sw: 12, se: 0 });
        let header = egui::Rect::from_min_max(egui::pos2(200.0, 0.0), egui::pos2(1000.0, 50.0));
        assert_eq!(corners_at_window_edge(header, window, 12), egui::CornerRadius { nw: 0, ne: 12, sw: 0, se: 0 });
        let middle = egui::Rect::from_min_max(egui::pos2(200.0, 50.0), egui::pos2(1000.0, 700.0));
        assert_eq!(corners_at_window_edge(middle, window, 12), egui::CornerRadius { nw: 0, ne: 0, sw: 0, se: 12 });
        assert_eq!(corners_at_window_edge(middle, window, 0), egui::CornerRadius::ZERO);
    }

    #[test]
    fn a_backdrop_at_a_corner_of_the_window_is_drawn_round_there() {
        let ctx = egui::Context::default();
        let size = egui::vec2(800.0, 600.0);
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() }, |ui| {
            let shape = flat_rect(ui, egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 600.0)), egui::Color32::RED);
            match (compositor_rounds_windows(), shape) {
                (false, egui::Shape::Rect(r)) => assert!(r.corner_radius.nw > 0 && r.corner_radius.sw > 0 && r.corner_radius.ne == 0),
                (true, egui::Shape::Mesh(_)) => {}
                (rounds, other) => panic!("compositor rounds {rounds}: {other:?}"),
            }
        });
        out.textures_delta.clear();
    }
}
