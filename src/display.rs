//! Display facts (ticket 01 R5) and intrinsic roles (01 R1).

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// What `visibleFrame` keeps off the top of `frame`, in points: the menu bar, the notch, or
/// both. Zero means the visible area starts at the physical top edge.
pub fn top_inset(frame: &Rect, visible: &Rect) -> f64 {
    ((frame.y + frame.h) - (visible.y + visible.h)).max(0.0)
}

/// The notch height in points, `0.0` when there is no notch.
///
/// Measured, never guessed from a name: `auxiliaryTopLeftArea` and `auxiliaryTopRightArea`
/// are the menu-bar strips either side of the notch, so a notch is a strip pair that is
/// taller than nothing and separated by a gap. A plain menu bar fills the width — the two
/// strips meet, and there is no gap, so nothing is compensated. An older Retina built-in
/// (no notch) and a notched external panel are both read correctly this way, while
/// "is it Retina" or "is it built-in" would get one of them wrong.
///
/// The result is clamped to `top_inset`: AeroSpace lays out inside `visibleFrame` and adds
/// the gap on top of it, so only area `visibleFrame` actually excludes is double-counted.
pub fn notch_height(frame: &Rect, visible: &Rect, aux_left: (f64, f64), aux_right: (f64, f64)) -> f64 {
    let (lw, lh) = aux_left;
    let (rw, rh) = aux_right;
    let strip = lh.min(rh);
    if strip <= 0.0 {
        return 0.0;
    }
    if frame.w - (lw + rw) <= 1.0 {
        return 0.0;
    }
    strip.min(top_inset(frame, visible))
}

/// CoreGraphics facts about one display, in `NSScreen.screens` order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Screen {
    pub cg_id: u32,
    /// `NSScreen.localizedName`, e.g. "LEN T23i-10". AeroSpace appends " (n)" to duplicates.
    pub name: String,
    /// Points (`CGDisplayBounds`). Orientation roles come from here.
    pub bounds: Rect,
    /// Native pixels of the current mode (01 R2). Ranking uses these, never points.
    pub px_w: u64,
    pub px_h: u64,
    pub rotation: f64,
    pub main: bool,
    pub builtin: bool,
    pub vendor: u32,
    pub model: u32,
    pub serial: u32,
    /// Notch height in points (`0.0` = no notch), measured by [`notch_height`] from AppKit's
    /// own geometry. This is the top area a gap would be double-counted against.
    pub notch: f64,
}

/// A `Screen` joined to the AeroSpace monitor that shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Display {
    /// AeroSpace `%{monitor-id}`: rank by origin (minX, minY) — the placement tiebreak (01 R6).
    pub aero_id: u32,
    /// AeroSpace `%{monitor-name}`, e.g. "LEN T23i-10 (2)".
    pub aero_name: String,
    pub screen: Screen,
}

impl Display {
    pub fn portrait(&self) -> bool {
        self.screen.bounds.h > self.screen.bounds.w
    }
    pub fn landscape(&self) -> bool {
        !self.portrait()
    }
    pub fn builtin(&self) -> bool {
        self.screen.builtin
    }
    pub fn external(&self) -> bool {
        !self.screen.builtin
    }
    pub fn pixel_area(&self) -> u64 {
        self.screen.px_w * self.screen.px_h
    }

    pub fn notch_height(&self) -> f64 {
        self.screen.notch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{builtin, len_landscape, len_portrait};

    #[test]
    fn roles_are_intrinsic_and_non_exclusive() {
        let p = len_portrait(1);
        assert!(p.portrait() && p.external() && !p.landscape() && !p.builtin());
        let b = builtin(2);
        assert!(b.landscape() && b.builtin() && !b.external());
        assert!(len_landscape(3).landscape());
    }

    /// The built-in panel of this machine as AppKit reported it: 1728x1117 pt, the two aux
    /// strips 771.5x32 each, `visibleFrame` 32 pt short of the top.
    fn builtin_frame() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 1728.0, h: 1117.0 }
    }

    fn visible_below_notch() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 1728.0, h: 1085.0 }
    }

    #[test]
    fn a_notch_is_measured_as_the_strip_pair_separated_by_a_gap() {
        let n = notch_height(&builtin_frame(), &visible_below_notch(), (771.5, 32.0), (771.5, 32.0));
        assert_eq!(n, 32.0);
        assert_eq!(top_inset(&builtin_frame(), &visible_below_notch()), 32.0);
    }

    #[test]
    fn a_plain_menu_bar_is_not_a_notch() {
        // Strips that fill the width meet: no notch, so no compensation.
        let n = notch_height(&len_frame(), &Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1056.0 }, (960.0, 24.0), (960.0, 24.0));
        assert_eq!(n, 0.0, "a 24 pt menu bar is not a gap to take back");
        // No aux area at all (menu bar hidden, no notch).
        assert_eq!(notch_height(&len_frame(), &len_frame(), (0.0, 0.0), (0.0, 0.0)), 0.0);
    }

    #[test]
    fn compensation_never_exceeds_what_visible_frame_excludes() {
        // A notch AppKit does not carve out of `visibleFrame` is not double-counted by the
        // gap, so there is nothing to compensate.
        let n = notch_height(&builtin_frame(), &builtin_frame(), (771.5, 32.0), (771.5, 32.0));
        assert_eq!(n, 0.0);
    }

    fn len_frame() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 }
    }

    #[test]
    fn pixel_area_uses_native_pixels_not_points() {
        // 01 R2: in points the built-in is smaller; in native pixels it is larger.
        let (b, l) = (builtin(2), len_landscape(3));
        assert!(b.screen.bounds.w * b.screen.bounds.h < l.screen.bounds.w * l.screen.bounds.h);
        assert!(b.pixel_area() > l.pixel_area());
    }
}
