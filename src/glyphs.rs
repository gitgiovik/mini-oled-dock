//! Hand-drawn vector icons, shared by the bar (`render`) and the popups (`surface`/`popup_*`).
//!
//! Each shape is a PURE function `(u, v) -> bool` in the 16x16 logical space (u = x, v = y, origin
//! at the top left): true if the point is inside the shape. The caller evaluates it with 4x4
//! supersampling (`surface::Canvas::glyph`, used by the bar and by `surface::Surface::glyph`), so there is no antialiasing here; the only state is the scale (`set_scale`), which makes strokes whole physical pixels.
//! No resources, no icon font, no GDI dependency.

use std::cell::Cell;

use crate::audio::DeviceKind;

/// Nominal thickness of "flush" strokes (16x16 units): in physical pixels it becomes a whole number
/// (`stroke_dev`), so at 100% strokes are crisp at 1 px instead of smearing over 2.
const STROKE_NOMINAL: f32 = 1.3;

thread_local! {
    /// Current scale (physical pixels per 16x16 unit), set by the caller before evaluating the shape.
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
}

/// Sets the scale for the following shapes: `size` = side in physical pixels of the 16x16 square.
/// Called by `surface::Canvas::glyph` (same thread that evaluates the shapes).
pub fn set_scale(size: i32) {
    if size > 0 {
        SCALE.with(|c| c.set(size as f32 / 16.0));
    }
}

fn scale() -> f32 {
    SCALE.with(|c| c.get())
}

/// Stroke thickness in physical pixels (integer, at least 1).
fn stroke_dev(s: f32) -> f32 {
    (STROKE_NOMINAL * s).round().max(1.0)
}

/// Half stroke thickness in 16x16 units.
fn half() -> f32 {
    let s = scale();
    stroke_dev(s) / (2.0 * s)
}

/// Moves the center line `c` (16x16 units) to where a whole-number-thickness stroke is crisp: at the pixel
/// center if the thickness is odd, on the boundary between two pixels if it is even.
fn snap(c: f32) -> f32 {
    let s = scale();
    if stroke_dev(s) as i32 % 2 == 1 {
        ((c * s).floor() + 0.5) / s
    } else {
        (c * s).round() / s
    }
}

/// Moves `c` to the nearest pixel edge (for the outer edges of filled areas).
fn edge(c: f32) -> f32 {
    let s = scale();
    (c * s).round() / s
}

/// Vertical symmetry axis of the glyphs (8.0, snapped to the stroke grid).
fn ax() -> f32 {
    snap(8.0)
}

/// Center of a dot snapped to the pixel center.
fn pix_center(c: f32) -> f32 {
    let s = scale();
    ((c * s).floor() + 0.5) / s
}

/// Distance from point `(px, py)` to segment `(ax, ay)-(bx, by)`.
pub fn dist_seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (vx, vy) = (bx - ax, by - ay);
    let (wx, wy) = (px - ax, py - ay);
    let len2 = vx * vx + vy * vy;
    let t = if len2 > 0.0 { ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (dx, dy) = (wx - t * vx, wy - t * vy);
    (dx * dx + dy * dy).sqrt()
}

/// Polyline `pts` with half thickness `half`.
fn polyline(u: f32, v: f32, pts: &[(f32, f32)], half: f32) -> bool {
    pts.windows(2).any(|w| dist_seg(u, v, w[0].0, w[0].1, w[1].0, w[1].1) <= half)
}

/// Outline bell (same stroke as the other icons): dome, flaring sides,
/// rim, knob and small clapper arc.
pub fn bell_shape(u: f32, v: f32) -> bool {
    let st = half();
    let c = ax();
    let (dx, dy) = (u - c, v - 6.6);
    let dome = v <= 6.6 && ((dx * dx + dy * dy).sqrt() - 3.4).abs() <= st;
    let rim_y = snap(11.6);
    let (l0, l1, l2) = (c - 3.4, c - 3.1, c - 4.8);
    let (r0, r1, r2) = (c + 3.4, c + 3.1, c + 4.8);
    let side_l = dist_seg(u, v, l0, 6.6, l1, 9.4) <= st || dist_seg(u, v, l1, 9.4, l2, rim_y) <= st;
    let side_r = dist_seg(u, v, r0, 6.6, r1, 9.4) <= st || dist_seg(u, v, r1, 9.4, r2, rim_y) <= st;
    let rim = dist_seg(u, v, l2, rim_y, r2, rim_y) <= st;
    let knob = dist_seg(u, v, c, 1.8, c, 2.6) <= st;
    let (cx, cy) = (u - c, v - (rim_y + 0.6));
    let clapper = v >= rim_y + 0.6 && ((cx * cx + cy * cy).sqrt() - 1.6).abs() <= st;
    dome || side_l || side_r || rim || knob || clapper
}

/// Center and radius of the notification badge in 16x16 space (the center is snapped to the pixel).
const BADGE_CX: f32 = 12.75;
const BADGE_CY: f32 = 3.25;
const BADGE_R: f32 = 2.25;
/// Empty ring around the badge and the diagonal bars (16x16 units).
const CUT_GAP: f32 = 1.25;

/// Notification badge dot (to be drawn on top of the bell).
pub fn badge_shape(u: f32, v: f32) -> bool {
    let (dx, dy) = (u - pix_center(BADGE_CX), v - pix_center(BADGE_CY));
    dx * dx + dy * dy <= BADGE_R * BADGE_R
}

/// Bell with the empty ring for the badge (same technique as the bar of `bell_off_shape`).
pub fn bell_badge_shape(u: f32, v: f32) -> bool {
    let (dx, dy) = (u - pix_center(BADGE_CX), v - pix_center(BADGE_CY));
    let gap = CUT_GAP.max(1.0 / scale());
    bell_shape(u, v) && (dx * dx + dy * dy).sqrt() > BADGE_R + gap
}

/// Crossed-out bell ("Do not disturb" active): the same bell with a diagonal bar and an
/// empty halo around the bar (like the muted speaker). With `badge` the badge ring too.
pub fn bell_off_shape(u: f32, v: f32, badge: bool) -> bool {
    let st = half();
    let d = dist_seg(u, v, 2.0, 2.0, 14.0, 14.0);
    let base = if badge { bell_badge_shape(u, v) } else { bell_shape(u, v) };
    (base && d > st + 1.0) || d <= st
}

/// Outline speaker with `waves` waves (0..=3); if `muted`, no waves and a diagonal bar
/// with an empty halo around it.
pub fn speaker_shape(u: f32, v: f32, waves: u32, muted: bool) -> bool {
    let st = half();
    let (x_back, x_cone, x_tip) = (snap(2.0), snap(4.4), snap(8.0));
    let (y_top, y_bot) = (snap(6.2), snap(9.8));
    let pts = [
        (x_back, y_top),
        (x_cone, y_top),
        (x_tip, 3.2),
        (x_tip, 12.8),
        (x_cone, y_bot),
        (x_back, y_bot),
        (x_back, y_top),
    ];
    let mut shape = polyline(u, v, &pts, st);
    if muted {
        let d = dist_seg(u, v, 2.5, 2.5, 13.5, 13.5);
        return (shape && d > st + 1.0) || d <= st;
    }
    let (ddx, dy) = (u - (x_tip + 0.5), v - 8.0);
    for n in 0..waves {
        let r = 2.9 + 1.9 * n as f32;
        let d = (ddx * ddx + dy * dy).sqrt();
        if (d - r).abs() <= st && ddx > 0.0 && ddx >= dy.abs() * 0.9 {
            shape = true;
        }
    }
    shape
}

/// Number of speaker waves for a 0..=100 level.
pub fn waves_for(level: u8) -> u32 {
    match level {
        0 => 0,
        1..=33 => 1,
        34..=66 => 2,
        _ => 3,
    }
}

/// Headphones: band (upper half ring) + two solid ear cups; with `mic` a short boom microphone.
fn headphones_shape(u: f32, v: f32, mic: bool) -> bool {
    let st = half();
    let c = ax();
    let (cy, r) = (8.0, 5.0);
    let (dx, dy) = (u - c, v - cy);
    let band = dy <= 0.0 && ((dx * dx + dy * dy).sqrt() - r).abs() <= st;
    let (xl, xr) = (c - r, c + r);
    let sides = dist_seg(u, v, xl, cy, xl, 9.6) <= st || dist_seg(u, v, xr, cy, xr, 9.6) <= st;
    let cups = dist_seg(u, v, xl, 10.0, xl, 11.6) <= 1.6 || dist_seg(u, v, xr, 10.0, xr, 11.6) <= 1.6;
    let boom = mic && polyline(u, v, &[(xl, 12.0), (xl + 0.2, 13.8), (c + 1.0, 13.8)], st);
    band || sides || cups || boom
}

/// Line-level output: jack port (ring with a center dot).
fn line_out_shape(u: f32, v: f32) -> bool {
    let c = ax();
    let (dx, dy) = (u - c, v - 8.0);
    let d = (dx * dx + dy * dy).sqrt();
    (d - 4.8).abs() <= half() || d <= 1.5
}

/// Digital (S/PDIF): outline of an optical port with a square lens.
fn digital_shape(u: f32, v: f32) -> bool {
    let (l, t, r, b) = (edge(2.0), edge(4.0), edge(14.0), edge(12.0));
    let w = 2.0 * half();
    let outer = u >= l && u <= r && v >= t && v <= b;
    let inner = u > l + w && u < r - w && v > t + w && v < b - w;
    let c = ax();
    let lens = (c - 1.6..=c + 1.6).contains(&u) && (7.0..=9.0).contains(&v);
    (outer && !inner) || lens
}

/// Icon of an output device in the audio popup (and the bar), by form factor.
pub fn device_shape(u: f32, v: f32, kind: DeviceKind) -> bool {
    match kind {
        DeviceKind::Speaker => speaker_shape(u, v, 2, false),
        DeviceKind::Headphones => headphones_shape(u, v, false),
        DeviceKind::Headset => headphones_shape(u, v, true),
        DeviceKind::LineOut => line_out_shape(u, v),
        DeviceKind::Display => show_desktop_shape(u, v),
        DeviceKind::Digital => digital_shape(u, v),
    }
}

/// Power symbol (IEC 5009): ring open at the top + vertical stem.
pub fn power_shape(u: f32, v: f32) -> bool {
    let st = half();
    let c = ax();
    let (cx, cy, r) = (c, 8.8, 5.2);
    let (dx, dy) = (u - cx, v - cy);
    let d = (dx * dx + dy * dy).sqrt();
    // Ring opening: a ~70 degree sector centered at the top (dy < 0).
    let gap = dy < 0.0 && dx.abs() < -dy * 0.72;
    let ring = (d - r).abs() <= st && !gap;
    let stem = dist_seg(u, v, c, 1.6, c, 8.2) <= st;
    ring || stem
}

/// "Show desktop": stylized 12x9 monitor (outline) with a stand, same stroke as the other icons.
pub fn show_desktop_shape(u: f32, v: f32) -> bool {
    let (l, t, r, b) = (edge(2.0), edge(2.2), edge(14.0), edge(11.2));
    let w = 2.0 * half();
    let inside_outer = u >= l && u <= r && v >= t && v <= b;
    let inside_inner = u > l + w && u < r - w && v > t + w && v < b - w;
    let frame = inside_outer && !inside_inner;
    let c = ax();
    let stem = (c - half()..=c + half()).contains(&u) && v > b && v <= edge(12.7);
    let base_y = edge(12.7);
    let base = (c - 3.0..=c + 3.0).contains(&u) && (base_y..=base_y + w).contains(&v);
    frame || stem || base
}

/// "Start": four filled squares (2x2) separated by a cross-shaped empty channel, centered in the 16x16 grid.
pub fn windows_logo_shape(u: f32, v: f32) -> bool {
    let (l, m1, m2, r) = (edge(2.4), edge(7.4), edge(8.6), edge(13.6));
    let in_cols = (u >= l && u <= m1) || (u >= m2 && u <= r);
    let in_rows = (v >= l && v <= m1) || (v >= m2 && v <= r);
    in_cols && in_rows
}

/// Center and radius of the network status badge (bottom right) and the empty halo around it.
const NET_BADGE_CX: f32 = 12.6;
const NET_BADGE_CY: f32 = 12.6;
const NET_BADGE_R: f32 = 3.0;
const NET_BADGE_GAP: f32 = 1.0;

/// true outside the network badge and its halo (the base glyphs are cut there).
fn outside_net_badge(u: f32, v: f32) -> bool {
    let (dx, dy) = (u - pix_center(NET_BADGE_CX), v - pix_center(NET_BADGE_CY));
    (dx * dx + dy * dy).sqrt() > NET_BADGE_R + NET_BADGE_GAP.max(1.0 / scale())
}

/// Wi-Fi fan with `bars` (0..=4) parts lit: 1 = dot, 2..=4 = dot plus 1..=3 arcs of 90 degrees,
/// cut around the status badge. `wifi_shape(u, v, 4)` is the full fan (drawn dim under the lit part).
pub fn wifi_shape(u: f32, v: f32, bars: u8) -> bool {
    if bars == 0 || !outside_net_badge(u, v) {
        return false;
    }
    let st = half();
    let c = ax();
    let cy = 12.8;
    let (dx, dy) = (u - c, v - cy);
    let dot = {
        let (ddx, ddy) = (u - c, v - 11.9);
        ddx * ddx + ddy * ddy <= 1.3 * 1.3
    };
    if dot {
        return true;
    }
    // Arcs: only the 90-degree sector pointing up.
    if dy >= 0.0 || dx.abs() > -dy {
        return false;
    }
    let d = (dx * dx + dy * dy).sqrt();
    [3.6_f32, 6.6, 9.6].iter().take(bars.saturating_sub(1) as usize).any(|r| (d - r).abs() <= st)
}

/// Ethernet: front of an RJ45 port (outline with the latch tab at the bottom and four contacts),
/// cut around the status badge.
pub fn ethernet_shape(u: f32, v: f32) -> bool {
    if !outside_net_badge(u, v) {
        return false;
    }
    let st = half();
    let (l, t, r, b) = (snap(2.0), snap(3.0), snap(14.0), snap(10.5));
    let (tl, tr, tb) = (snap(5.5), snap(10.5), snap(13.0));
    let outline = polyline(u, v, &[(l, t), (r, t), (r, b), (tr, b), (tr, tb), (tl, tb), (tl, b), (l, b), (l, t)], st);
    let pins = [5.5, 8.0, 10.5].iter().any(|&x| dist_seg(u, v, snap(x), t, snap(x), 5.0) <= st);
    outline || pins
}

/// Network status badge at the bottom right: a small globe (ring, equator, meridian) when `online`,
/// a small x otherwise.
pub fn net_badge_shape(u: f32, v: f32, online: bool) -> bool {
    let st = half();
    let (cx, cy) = (pix_center(NET_BADGE_CX), pix_center(NET_BADGE_CY));
    let (dx, dy) = (u - cx, v - cy);
    if online {
        // Globe reduced to what survives at 6-8 px: ring, equator and central meridian.
        let d = (dx * dx + dy * dy).sqrt();
        let ring = (d - NET_BADGE_R).abs() <= st;
        let cross = d <= NET_BADGE_R && (dx.abs() <= st || dy.abs() <= st);
        ring || cross
    } else {
        let k = 1.9;
        dist_seg(u, v, cx - k, cy - k, cx + k, cy + k) <= st || dist_seg(u, v, cx + k, cy - k, cx - k, cy + k) <= st
    }
}

/// Chevron pointing up (tray overflow with the bar at the bottom).
pub fn chevron_up_shape(u: f32, v: f32) -> bool {
    let c = ax();
    polyline(u, v, &[(c - 4.0, 10.25), (c, 5.75), (c + 4.0, 10.25)], half())
}

/// Chevron pointing down (tray overflow with the bar at the top).
pub fn chevron_down_shape(u: f32, v: f32) -> bool {
    let c = ax();
    polyline(u, v, &[(c - 4.0, 5.75), (c, 10.25), (c + 4.0, 5.75)], half())
}

/// "Previous month" arrow of the calendar.
pub fn chevron_left_shape(u: f32, v: f32) -> bool {
    polyline(u, v, &[(10.0, 4.0), (6.0, 8.0), (10.0, 12.0)], half())
}

/// "Next month" arrow of the calendar.
pub fn chevron_right_shape(u: f32, v: f32) -> bool {
    polyline(u, v, &[(6.0, 4.0), (10.0, 8.0), (6.0, 12.0)], half())
}

/// Close X (dismissing a notification).
pub fn close_shape(u: f32, v: f32) -> bool {
    let st = half();
    dist_seg(u, v, 4.5, 4.5, 11.5, 11.5) <= st || dist_seg(u, v, 11.5, 4.5, 4.5, 11.5) <= st
}

/// Crescent moon (Sleep): a disc with an offset disc cut out of it.
pub fn moon_shape(u: f32, v: f32) -> bool {
    let (dx, dy) = (u - 7.6, v - 8.4);
    let (ex, ey) = (u - 11.0, v - 6.0);
    dx * dx + dy * dy <= 5.6 * 5.6 && ex * ex + ey * ey > 4.6 * 4.6
}

/// Padlock (Lock): body outline, shackle arc and a keyhole dot.
pub fn lock_shape(u: f32, v: f32) -> bool {
    let st = half();
    let c = ax();
    let (l, t, r, b) = (edge(3.0), edge(7.0), edge(13.0), edge(14.0));
    let w = 2.0 * st;
    let outer = u >= l && u <= r && v >= t && v <= b;
    let inner = u > l + w && u < r - w && v > t + w && v < b - w;
    let (dx, dy) = (u - c, v - t);
    let shackle = dy <= 0.0 && ((dx * dx + dy * dy).sqrt() - 3.0).abs() <= st;
    let legs = dist_seg(u, v, c - 3.0, t - 3.0 + 3.0, c - 3.0, t) <= st || dist_seg(u, v, c + 3.0, t - 3.0 + 3.0, c + 3.0, t) <= st;
    let (kx, ky) = (u - c, v - 10.5);
    (outer && !inner) || shackle || legs || kx * kx + ky * ky <= 1.1 * 1.1
}

/// Circular arrow (Restart): ring open at the top right, arrowhead on the clockwise end.
pub fn restart_shape(u: f32, v: f32) -> bool {
    let st = half();
    let c = ax();
    let (dx, dy) = (u - c, v - 8.6);
    let d = (dx * dx + dy * dy).sqrt();
    let gap = dx > 0.0 && dy < 0.0 && -dy > dx * 0.4;
    let ring = (d - 4.8).abs() <= st && !gap;
    let head = polyline(u, v, &[(c + 0.2, 1.4), (c + 2.6, 3.8), (c + 0.2, 6.2)], st);
    ring || head
}

/// Gear (Settings): ring with eight radial teeth.
pub fn gear_shape(u: f32, v: f32) -> bool {
    let st = half();
    let c = ax();
    let (dx, dy) = (u - c, v - snap(8.0));
    let d = (dx * dx + dy * dy).sqrt();
    let ring = (d - 3.2).abs() <= st;
    let teeth = (0..8).any(|i| {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (ca, sa) = (a.cos(), a.sin());
        dist_seg(dx, dy, ca * 3.2, sa * 3.2, ca * 6.0, sa * 6.0) <= st + 0.2
    });
    ring || teeth
}

/// Startup (Start with Windows): window outline with an up arrow inside.
pub fn startup_shape(u: f32, v: f32) -> bool {
    let st = half();
    let c = ax();
    let (l, t, r, b) = (edge(2.0), edge(2.5), edge(14.0), edge(13.5));
    let w = 2.0 * st;
    let outer = u >= l && u <= r && v >= t && v <= b;
    let inner = u > l + w && u < r - w && v > t + w && v < b - w;
    let stem = dist_seg(u, v, c, 11.0, c, 6.2) <= st;
    let head = polyline(u, v, &[(c - 2.2, 8.4), (c, 6.2), (c + 2.2, 8.4)], st);
    (outer && !inner) || stem || head
}

/// Exit (Exit dock): open box with an arrow pointing out of it.
pub fn exit_shape(u: f32, v: f32) -> bool {
    let st = half();
    let (l, t, b) = (snap(3.0), snap(3.0), snap(13.0));
    let y = snap(8.0);
    let frame = polyline(u, v, &[(9.0, t), (l, t), (l, b), (9.0, b)], st);
    let arrow = dist_seg(u, v, 7.0, y, 14.0, y) <= st;
    let head = polyline(u, v, &[(11.0, y - 3.0), (14.0, y), (11.0, y + 3.0)], st);
    frame || arrow || head
}

/// Check mark (default audio device).
pub fn check_shape(u: f32, v: f32) -> bool {
    polyline(u, v, &[(3.5, 8.5), (6.5, 11.5), (12.5, 4.5)], half())
}

/// Pencil (audio popup "Customize"): diagonal body outline with a pointed tip at the bottom left.
pub fn pencil_shape(u: f32, v: f32) -> bool {
    let st = half();
    // Body: two parallel edges from the eraser end (top right) down to the tip cone.
    let body = polyline(u, v, &[(10.2, 2.4), (13.6, 5.8), (6.0, 13.4), (2.6, 13.4), (2.6, 10.0), (10.2, 2.4)], st);
    let ferrule = dist_seg(u, v, 8.6, 4.0, 12.0, 7.4) <= st;
    body || ferrule
}

/// Eye (audio popup "Show / hide"): almond outline with a pupil; `off` adds a diagonal bar with an
/// empty halo (same technique as the muted speaker).
pub fn eye_shape(u: f32, v: f32, off: bool) -> bool {
    let st = half();
    let c = ax();
    // Almond: two circle arcs meeting at the corners (1.5, 8) and (14.5, 8).
    let (dx, dt, db) = (u - c, v - 13.2, v - 2.8);
    let upper = v <= 8.0 && ((dx * dx + dt * dt).sqrt() - 8.0).abs() <= st;
    let lower = v >= 8.0 && ((dx * dx + db * db).sqrt() - 8.0).abs() <= st;
    let (px, py) = (u - c, v - 8.0);
    let pupil = px * px + py * py <= 2.0 * 2.0;
    let shape = upper || lower || pupil;
    if !off {
        return shape;
    }
    let d = dist_seg(u, v, 2.5, 2.5, 13.5, 13.5);
    (shape && d > st + 1.0) || d <= st
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_hit_expected_points() {
        assert!(pencil_shape(2.6, 12.0) && !pencil_shape(13.0, 13.0));
        assert!(eye_shape(8.0, 8.0, false) && !eye_shape(1.0, 1.0, false));
        assert!(eye_shape(2.5, 2.5, true));
        assert!(bell_shape(8.0, 11.6)); // rim
        assert!(!bell_shape(8.0, 8.0)); // empty interior
        assert!(badge_shape(12.75, 3.25));
        assert!(!bell_badge_shape(12.75, 3.25));
        assert!(!bell_shape(0.5, 0.5));
        assert!(power_shape(8.0, 4.0)); // stem
        assert!(!power_shape(8.0, 8.8)); // empty center
        assert!(show_desktop_shape(2.2, 6.0)); // left side of the monitor
        assert!(!show_desktop_shape(8.0, 7.0)); // empty screen
        assert!(windows_logo_shape(4.0, 4.0) && windows_logo_shape(12.0, 12.0)); // outer squares
        // channel
        assert!(!windows_logo_shape(8.0, 4.0) && !windows_logo_shape(4.0, 8.0) && !windows_logo_shape(8.0, 8.0));
        assert!(!windows_logo_shape(1.0, 1.0)); // outside
        assert!(chevron_up_shape(8.0, 6.0));
        assert!(close_shape(8.0, 8.0));
        assert!(check_shape(6.5, 11.5));
        assert!(headphones_shape(8.0, 3.0, false)); // band
        assert!(!headphones_shape(8.0, 8.0, true)); // open center
        assert!(wifi_shape(8.0, 11.9, 1)); // dot
        assert!(!wifi_shape(8.0, 3.2, 3) && wifi_shape(8.0, 3.2, 4)); // outer arc only with 4 bars
        assert!(!wifi_shape(8.0, 11.9, 0));
        assert!(ethernet_shape(8.0, 3.5) && !ethernet_shape(8.0, 8.0));
        assert!(net_badge_shape(12.6, 12.6, false) && !wifi_shape(12.6, 12.6, 4));
        assert_eq!(waves_for(0), 0);
        assert_eq!(waves_for(100), 3);
    }
}
