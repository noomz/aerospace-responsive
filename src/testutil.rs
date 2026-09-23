//! Test fixtures mirroring the user's real desk.

use crate::display::{Display, Rect, Screen};

fn screen(cg_id: u32, name: &str, bounds: Rect, px: (u64, u64), rotation: f64, main: bool, builtin: bool) -> Screen {
    Screen {
        cg_id,
        name: name.into(),
        bounds,
        px_w: px.0,
        px_h: px.1,
        rotation,
        main,
        builtin,
        vendor: if builtin { 1552 } else { 12 },
        model: if builtin { 41152 } else { 25003 },
        serial: if builtin { 0 } else { 16843009 },
    }
}

pub fn builtin(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "Built-in Retina Display".into(),
        screen: screen(1, "Built-in Retina Display", Rect { x: 0.0, y: 0.0, w: 1728.0, h: 1117.0 }, (3456, 2234), 0.0, true, true),
    }
}

pub fn len_portrait(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "LEN T23i-10 (2)".into(),
        screen: screen(5, "LEN T23i-10", Rect { x: -1080.0, y: -803.0, w: 1080.0, h: 1920.0 }, (1080, 1920), 270.0, false, false),
    }
}

pub fn len_landscape(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "LEN T23i-10 (1)".into(),
        screen: screen(6, "LEN T23i-10", Rect { x: 1728.0, y: 0.0, w: 1920.0, h: 1080.0 }, (1920, 1080), 0.0, false, false),
    }
}

/// portrait (monitor 1, leftmost) | built-in (2) | landscape (3), as attached today.
pub fn desk3() -> Vec<Display> {
    vec![len_portrait(1), builtin(2), len_landscape(3)]
}
