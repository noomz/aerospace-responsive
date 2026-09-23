//! Display facts (ticket 01 R5) and intrinsic roles (01 R1).

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
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
}

#[cfg(test)]
mod tests {
    use crate::testutil::{builtin, len_landscape, len_portrait};

    #[test]
    fn roles_are_intrinsic_and_non_exclusive() {
        let p = len_portrait(1);
        assert!(p.portrait() && p.external() && !p.landscape() && !p.builtin());
        let b = builtin(2);
        assert!(b.landscape() && b.builtin() && !b.external());
        assert!(len_landscape(3).landscape());
    }

    #[test]
    fn pixel_area_uses_native_pixels_not_points() {
        // 01 R2: in points the built-in is smaller; in native pixels it is larger.
        let (b, l) = (builtin(2), len_landscape(3));
        assert!(b.screen.bounds.w * b.screen.bounds.h < l.screen.bounds.w * l.screen.bounds.h);
        assert!(b.pixel_area() > l.pixel_area());
    }
}
