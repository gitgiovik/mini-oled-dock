//! SETTINGS popup (from "Settings" in the power popup): the dock's own options on three short pages.
//!
//!  * Main page. Appearance: position and display mode (segmented choices drawn by hand), then
//!    "Background ›" and "Sizes ›" (sub-pages). Behavior: auto-hide, window previews, clock seconds (switches).
//!    Modules: the Start button switch, one switch per system module (`config::MODULE_NAMES` mechanism)
//!    and, with the tray on, "Choose tray icons" (switches to the tray popup in chooser mode).
//!  * Background page: preset swatches (`config::BACKGROUND_PRESETS`) plus a custom swatch that opens
//!    a hand-drawn colour picker (saturation/value square, hue strip, preview and hex code), and the
//!    background opacity slider (0-100 %).
//!  * Sizes page: app icons, app names, tray icons and modules sliders, and "Reset sizes".
//!
//! Every change applies LIVE. Clicks, wheel steps and the end of a drag save through `config::modify`
//! (the commented template survives); while a slider or the picker is being dragged the change is only
//! set in memory (`config::set`) and the file is written on release. Either way `WM_DOCK_CONFIG_APPLY`
//! (throttled while dragging) asks the dock window to rebuild the bar outside this popup's wnd_proc.
//! If the file cannot be written the change still applies for this session.
//! No timer, thread or system object: when the popup closes nothing of it keeps running (a drag still
//! unsaved at that point is saved by `closing`).

use std::sync::Arc;

use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::config::{
    self, Background, Config, DisplayMode, Position, SizeRange, APP_ICON_SIZE, APP_TEXT_SIZE, BACKGROUND_PRESETS,
    MODULE_SIZE, TRAY_ICON_SIZE,
};
use crate::glyphs;
use crate::log;
use crate::msg::WM_DOCK_CONFIG_APPLY;
use crate::popup::{self, center_y, hline, lift, row_plate, tick_ms, toggle_switch, Content, HitMap, PopupKind, Reply};
use crate::surface::{rect_wh, Font, Surface, PRESS_ALPHA, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, TEXT_COL, WHITE};
use crate::ui::Screen;

/// Popup width (logical px).
const WIDTH: i32 = 260;
/// Height of a control row / of a section label (logical px).
const ROW_H: i32 = 28;
const SECTION_H: i32 = 26;
/// Height of a slider row: label and value on top, track below (logical px).
const SLIDER_ROW_H: i32 = 48;
/// Swatch row height and swatch diameter (logical px).
const SWATCH_ROW_H: i32 = 36;
const SWATCH_D: i32 = 20;
/// Colour picker: saturation/value square height, hue strip height, gap between the parts (logical px).
const SV_H: i32 = 112;
const HUE_H: i32 = 12;
const PICKER_GAP: i32 = 10;
/// Segmented control: height, horizontal text padding of a segment (logical px).
const SEG_H: i32 = 20;
const SEG_PAD: i32 = 7;
/// Segmented control alphas: track fill and outline, hovered segment.
const SEG_TRACK_ALPHA: u32 = 14;
const SEG_RIM_ALPHA: u32 = 40;
const SEG_HOT_ALPHA: u32 = 26;
/// Outline of the swatches and of the colour preview (visible on a panel of the same colour).
const SWATCH_RIM_ALPHA: u32 = 70;
/// Slider: track thickness and knob radius, as in the audio popup (logical px).
const TRACK_H: i32 = 4;
const KNOB_R: i32 = 7;
/// Minimum interval between two live bar rebuilds while dragging (ms).
const APPLY_THROTTLE_MS: u64 = 30;
/// Ids: item `i` -> `(i + 1) * ID_STRIDE`; part `j` of an item (segment, swatch, picker area,
/// slider track) -> that + 1 + j.
const ID_STRIDE: u32 = 16;
/// Picker parts.
const PART_SV: usize = 0;
const PART_HUE: usize = 1;

/// Page of the popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Main,
    Background,
    Sizes,
}

/// A setting changed by a choice or a switch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    Position,
    /// Display the bar sits on (`config.monitor`); offered only with two or more displays.
    Monitor,
    Display,
    AutoHide,
    Previews,
    Seconds,
    /// `Config::module_visible` name.
    Module(&'static str),
}

/// A value set by a slider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slider {
    Opacity,
    AppIcon,
    AppText,
    TrayIcon,
    Module,
}

#[derive(Clone, Copy)]
enum Item {
    Section(&'static str),
    /// "Colour" section label with the current colour's name (or hex code) on the right.
    ColourSection,
    /// Label + segmented choice.
    Choice(Setting, &'static str),
    /// Label + switch (the whole row is clickable).
    Toggle(Setting, &'static str),
    /// "›" row opening a sub-page.
    Open(Page, &'static str),
    /// "‹" row back to the main page, with the page title.
    Back(&'static str),
    /// "Choose tray icons" link.
    TrayChooser,
    /// Preset swatches + the custom swatch (opens / closes the picker).
    Swatches,
    /// Colour picker (only while open).
    Picker,
    /// Label + value + draggable track.
    Slider(Slider, &'static str),
    /// Back to the default sizes.
    ResetSizes,
}

/// Rows of page `page` for the config `c` (the tray link only while the tray is on, the screen
/// choice only with two or more displays `sc`).
fn items(c: &Config, page: Page, picker_open: bool, sc: &[Screen]) -> Vec<Item> {
    match page {
        Page::Main => {
            let mut v = vec![Item::Section("Appearance"), Item::Choice(Setting::Position, "Position")];
            if sc.len() > 1 {
                v.push(Item::Choice(Setting::Monitor, "Screen"));
            }
            v.extend([
                Item::Choice(Setting::Display, "Display"),
                Item::Open(Page::Background, "Background"),
                Item::Open(Page::Sizes, "Sizes"),
                Item::Section("Behavior"),
                Item::Toggle(Setting::AutoHide, "Auto-hide"),
                Item::Toggle(Setting::Previews, "Window previews"),
                Item::Toggle(Setting::Seconds, "Clock seconds"),
                Item::Section("Modules"),
                // The Start button is a fixed item at the left end of the bar, not part of `MODULE_NAMES`.
                Item::Toggle(Setting::Module("start"), module_label("start")),
            ]);
            v.extend(config::MODULE_NAMES.iter().map(|n| Item::Toggle(Setting::Module(n), module_label(n))));
            if c.show_tray {
                v.push(Item::TrayChooser);
            }
            v
        }
        Page::Background => {
            let mut v = vec![Item::Back("Background"), Item::ColourSection, Item::Swatches];
            if picker_open {
                v.push(Item::Picker);
            }
            v.push(Item::Slider(Slider::Opacity, "Opacity"));
            v
        }
        Page::Sizes => vec![
            Item::Back("Sizes"),
            Item::Slider(Slider::AppIcon, "App icons"),
            Item::Slider(Slider::AppText, "App names"),
            Item::Slider(Slider::TrayIcon, "Tray icons"),
            Item::Slider(Slider::Module, "Modules"),
            Item::ResetSizes,
        ],
    }
}

/// Row label of a `config::MODULE_NAMES` entry (or "start").
fn module_label(name: &str) -> &'static str {
    match name {
        "notifications" => "Notifications",
        "volume" => "Volume",
        "network" => "Network",
        "metrics" => "System metrics",
        "clock" => "Clock",
        "show_desktop" => "Show desktop",
        "start" => "Start button",
        "tray" => "Tray icons",
        _ => "Module",
    }
}

/// Labels of the segments of a choice (`sc`: the displays, for the screen choice).
fn options(s: Setting, sc: &[Screen]) -> Vec<&'static str> {
    match s {
        Setting::Position => vec!["Top", "Bottom"],
        Setting::Monitor => {
            // Last segment: follow the pointer (`config::MONITOR_POINTER`).
            sc.iter().map(|d| if d.primary { "Main" } else { d.label }).chain(std::iter::once("Pointer")).collect()
        }
        Setting::Display => vec!["Icon + name", "Icon", "Name"],
        _ => Vec::new(),
    }
}

/// Selected segment of a choice. Screen: the chosen display, or "Main" (the primary, first in `sc`)
/// when none is chosen or the chosen one is not connected (that is where the bar is).
fn selected(c: &Config, s: Setting, sc: &[Screen]) -> Option<usize> {
    Some(match s {
        Setting::Position => usize::from(c.position == Position::Bottom),
        Setting::Monitor => {
            if c.monitor.eq_ignore_ascii_case(config::MONITOR_POINTER) {
                sc.len()
            } else {
                sc.iter().position(|d| !d.primary && d.device.eq_ignore_ascii_case(&c.monitor)).unwrap_or(0)
            }
        }
        Setting::Display => match c.display_mode {
            DisplayMode::IconText => 0,
            DisplayMode::Icon => 1,
            DisplayMode::Text => 2,
        },
        _ => return None,
    })
}

/// Sets segment `j` of choice `s`. Screen: the primary is stored as "" (the bar then follows the
/// primary monitor even if Windows changes which one it is).
fn choose(c: &mut Config, s: Setting, j: usize, sc: &[Screen]) {
    match s {
        Setting::Position => c.position = if j == 0 { Position::Top } else { Position::Bottom },
        Setting::Monitor => {
            c.monitor = if j == sc.len() {
                config::MONITOR_POINTER.to_string()
            } else {
                sc.get(j).filter(|d| !d.primary).map(|d| d.device.clone()).unwrap_or_default()
            };
        }
        Setting::Display => {
            c.display_mode = [DisplayMode::IconText, DisplayMode::Icon, DisplayMode::Text][j.min(2)];
        }
        _ => {}
    }
}

fn is_on(c: &Config, s: Setting) -> bool {
    match s {
        Setting::AutoHide => c.autohide,
        Setting::Previews => c.window_previews,
        Setting::Seconds => c.show_seconds,
        Setting::Module(n) => c.module_visible(n),
        _ => false,
    }
}

fn flip(c: &mut Config, s: Setting) {
    match s {
        Setting::AutoHide => c.autohide = !c.autohide,
        Setting::Previews => c.window_previews = !c.window_previews,
        Setting::Seconds => c.show_seconds = !c.show_seconds,
        Setting::Module(n) => {
            let on = c.module_visible(n);
            c.set_module_visible(n, !on);
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ sliders

/// `(min, max, wheel step, unit)` of a slider.
fn slider_range(sl: Slider) -> (i32, i32, i32, &'static str) {
    let r = |s: SizeRange, step: i32, unit| (s.min as i32, s.max as i32, step, unit);
    match sl {
        Slider::Opacity => (0, 100, 5, "%"),
        Slider::AppIcon => r(APP_ICON_SIZE, 1, " px"),
        Slider::AppText => r(APP_TEXT_SIZE, 1, " px"),
        Slider::TrayIcon => r(TRAY_ICON_SIZE, 1, " px"),
        Slider::Module => r(MODULE_SIZE, 5, "%"),
    }
}

fn slider_value(c: &Config, sl: Slider) -> i32 {
    let (icon, text, tray, module) = c.sizes();
    (match sl {
        Slider::Opacity => c.bar_opacity.min(100),
        Slider::AppIcon => icon,
        Slider::AppText => text,
        Slider::TrayIcon => tray,
        Slider::Module => module,
    }) as i32
}

/// Sets slider `sl` to `v` (clamped to its range).
fn set_slider(c: &mut Config, sl: Slider, v: i32) {
    let (lo, hi, _, _) = slider_range(sl);
    let v = v.clamp(lo, hi) as u8;
    match sl {
        Slider::Opacity => c.bar_opacity = v,
        Slider::AppIcon => c.app_icon_size = v,
        Slider::AppText => c.app_text_size = v,
        Slider::TrayIcon => c.tray_icon_size = v,
        Slider::Module => c.module_size = v,
    }
}

fn reset_sizes(c: &mut Config) {
    c.app_icon_size = APP_ICON_SIZE.default;
    c.app_text_size = APP_TEXT_SIZE.default;
    c.tray_icon_size = TRAY_ICON_SIZE.default;
    c.module_size = MODULE_SIZE.default;
}

fn sizes_are_default(c: &Config) -> bool {
    c.sizes() == (APP_ICON_SIZE.default, APP_TEXT_SIZE.default, TRAY_ICON_SIZE.default, MODULE_SIZE.default)
}

// ------------------------------------------------------------------ colour

/// HSV (hue 0..360, saturation and value 0..1) -> RGB 0..=255.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u32, u32, u32) {
    let h = h.rem_euclid(360.0) / 60.0;
    let (s, v) = (s.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let q = |f: f32| ((f + m) * 255.0 + 0.5).clamp(0.0, 255.0) as u32;
    (q(r), q(g), q(b))
}

/// RGB -> HSV (hue 0 for greys).
fn rgb_to_hsv((r, g, b): (u32, u32, u32)) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max <= 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

fn background_of((h, s, v): (f32, f32, f32)) -> Background {
    let (r, g, b) = hsv_to_rgb(h, s, v);
    Background(r << 16 | g << 8 | b)
}

/// Index of the preset equal to `bg`.
fn preset_index(bg: Background) -> Option<usize> {
    BACKGROUND_PRESETS.iter().position(|(_, p)| *p == bg)
}

/// Swatch `k` (presets, then the custom one) is the current colour.
fn swatch_selected(bg: Background, k: usize) -> bool {
    match preset_index(bg) {
        Some(p) => p == k,
        None => k == BACKGROUND_PRESETS.len(),
    }
}

// ------------------------------------------------------------------ popup

pub(crate) struct SettingsPopup {
    /// Config snapshot shown (re-read after every change).
    cfg: Arc<Config>,
    page: Page,
    picker_open: bool,
    /// Connected displays, read when the popup opens (`ui::screens`).
    screens: Vec<Screen>,
    /// Picker colour as HSV, kept apart from the RGB value so the hue survives greys and black.
    hsv: (f32, f32, f32),
    items: Vec<Item>,
    hits: HitMap,
    /// Top of each item and its height (physical px), same order as `items`.
    rows: Vec<(i32, i32)>,
    width: i32,
    /// Picker areas (physical px): saturation/value square, hue strip, top of the hex row.
    sv: RECT,
    hue: RECT,
    hex_y: i32,
    /// Slider knob radius (physical px).
    knob_r: i32,
    /// A dragged value set in memory only: saved on release (or when the popup closes).
    unsaved: bool,
    /// Time of the last `WM_DOCK_CONFIG_APPLY`, and a rebuild skipped by the throttle.
    last_apply: u64,
    apply_pending: bool,
}

/// Creates the content from the current config, on the main page.
pub(crate) fn new() -> Box<dyn Content> {
    Box::new(SettingsPopup::at(Page::Main))
}

// PREVIEW-HOOK (used by `preview.rs`): the content opened on `page`, with the picker open or closed.
pub(crate) fn preview_new(page: Page, picker_open: bool) -> Box<dyn Content> {
    let mut p = SettingsPopup::at(page);
    p.picker_open = picker_open && page == Page::Background;
    p.items = items(&p.cfg, p.page, p.picker_open, &p.screens);
    Box::new(p)
}

// PREVIEW-HOOK (used by `preview.rs`): the main page with `screens` in place of the connected
// displays, so the "Screen" row (shown only with two or more) is the same on every machine.
pub(crate) fn preview_with_screens(screens: Vec<Screen>) -> Box<dyn Content> {
    let mut p = SettingsPopup::at(Page::Main);
    p.screens = screens;
    p.items = items(&p.cfg, p.page, p.picker_open, &p.screens);
    Box::new(p)
}

/// Id of item `i` (and of its part `part`, if any).
fn item_id(i: usize, part: Option<usize>) -> u32 {
    (i as u32 + 1) * ID_STRIDE + part.map_or(0, |j| j as u32 + 1)
}

/// `(item index, part)` of an id.
fn decode(id: u32) -> (usize, Option<usize>) {
    let i = (id / ID_STRIDE).saturating_sub(1) as usize;
    let j = id % ID_STRIDE;
    (i, if j == 0 { None } else { Some(j as usize - 1) })
}

fn post_apply() {
    // SAFETY: posting to the dock window; if it no longer exists the call just fails.
    if let Err(e) = unsafe { PostMessageW(Some(crate::hwnd()), WM_DOCK_CONFIG_APPLY, WPARAM(0), LPARAM(0)) } {
        log!("settings: PostMessageW(WM_DOCK_CONFIG_APPLY) failed: {e}");
    }
}

impl SettingsPopup {
    fn at(page: Page) -> SettingsPopup {
        let cfg = config::get();
        let picker_open = page == Page::Background && preset_index(cfg.background).is_none();
        let screens = crate::ui::screens();
        SettingsPopup {
            items: items(&cfg, page, picker_open, &screens),
            screens,
            hsv: rgb_to_hsv(cfg.background.rgb()),
            cfg,
            page,
            picker_open,
            hits: HitMap::default(),
            rows: Vec::new(),
            width: 0,
            sv: RECT::default(),
            hue: RECT::default(),
            hex_y: 0,
            knob_r: 7,
            unsaved: false,
            last_apply: 0,
            apply_pending: false,
        }
    }

    /// Segment widths of choice `s` (physical px).
    fn seg_widths(&self, s: &Surface, set: Setting) -> Vec<i32> {
        let m = s.metrics();
        options(set, &self.screens)
            .iter()
            .map(|o| (s.text_width(o, Font::Small) + 2 * m.px(SEG_PAD)).max(m.px(30)))
            .collect()
    }

    /// Applies `f` to the config. `save`: written now through the commented template; otherwise set
    /// in memory only (a drag in progress) and saved by `finish`. Then the bar is rebuilt (throttled
    /// while dragging).
    fn change(&mut self, f: impl Fn(&mut Config), save: bool) -> Reply {
        if save {
            if let Err(e) = config::modify(&f) {
                // Not writable (or the write failed): the change still applies for this session.
                log!("settings: saving failed ({e}), applied in memory only");
                let mut c = (*config::get()).clone();
                f(&mut c);
                config::set(c);
            }
            self.unsaved = false;
        } else {
            let mut c = (*config::get()).clone();
            f(&mut c);
            config::set(c);
            self.unsaved = true;
        }
        let now = tick_ms();
        if save || now.saturating_sub(self.last_apply) >= APPLY_THROTTLE_MS {
            self.last_apply = now;
            self.apply_pending = false;
            post_apply();
        } else {
            self.apply_pending = true;
            // Holding still after the last move: flush the skipped rebuild soon (see `tick`).
            crate::popup::tick_soon((APPLY_THROTTLE_MS - now.saturating_sub(self.last_apply)) as u32 + 5);
        }
        self.cfg = config::get();
        self.items = items(&self.cfg, self.page, self.picker_open, &self.screens);
        Reply::Relayout
    }

    /// End of a drag: saves the value set in memory and sends the rebuild the throttle skipped.
    fn finish(&mut self) {
        if self.unsaved {
            self.change(|_| {}, true);
        } else if self.apply_pending {
            self.apply_pending = false;
            post_apply();
        }
    }

    /// Shows `page` (the picker starts open when the colour is not a preset).
    fn goto(&mut self, page: Page) -> Reply {
        self.finish();
        self.page = page;
        self.cfg = config::get();
        self.picker_open = page == Page::Background && preset_index(self.cfg.background).is_none();
        self.hsv = rgb_to_hsv(self.cfg.background.rgb());
        self.items = items(&self.cfg, page, self.picker_open, &self.screens);
        Reply::Relayout
    }

    /// Value of slider item `i` at the client abscissa `x`.
    fn slider_at(&self, i: usize, sl: Slider, x: i32) -> Option<i32> {
        let rc = self.hits.rect(item_id(i, Some(0)))?;
        let (l, r) = (rc.left + self.knob_r, rc.right - self.knob_r);
        let (lo, hi, _, _) = slider_range(sl);
        let span = (r - l).max(1);
        Some(lo + ((x - l).clamp(0, span) * (hi - lo) + span / 2) / span)
    }

    /// Sets the colour from the picker state (`hsv`), in memory while dragging.
    fn pick(&mut self) -> Reply {
        let bg = background_of(self.hsv);
        if bg != self.cfg.background {
            self.change(move |c| c.background = bg, false);
        }
        // Same size while dragging: a plain redraw keeps the popup where it is (see `popup::reanchor`).
        Reply::Redraw
    }

    fn paint_slider(&self, s: &mut Surface, i: usize, sl: Slider, label: &str, hot: bool) {
        let Some(&(y, _)) = self.rows.get(i) else { return };
        let Some(rc) = self.hits.rect(item_id(i, Some(0))) else { return };
        let m = s.metrics();
        let col = m.px(TEXT_COL);
        s.text(label, col, y + m.px(6), Font::Normal, WHITE, lift(TEXT_ALPHA, hot));
        let (lo, hi, _, unit) = slider_range(sl);
        let v = slider_value(&self.cfg, sl);
        let val = format!("{v}{unit}");
        let vw = s.text_width(&val, Font::Normal);
        s.text(&val, self.width - col - vw, y + m.px(6), Font::Normal, WHITE, lift(TEXT2_ALPHA, hot));
        // Track as in the audio popup: dim rail, bright fill up to the knob, white knob.
        let r = self.knob_r;
        let (tl, tr) = (rc.left + r, rc.right - r);
        let cy = (rc.top + rc.bottom) / 2;
        let th = m.px(TRACK_H);
        let rail = RECT { left: rc.left, top: cy - th / 2, right: rc.right, bottom: cy - th / 2 + th };
        s.fill_rr(rail, th / 2, WHITE, 60);
        let kx = tl + (tr - tl) * (v - lo) / (hi - lo).max(1);
        s.fill_rr(RECT { right: kx, ..rail }, th / 2, WHITE, 230);
        s.fill_rr(RECT { left: kx - r, top: cy - r, right: kx + r, bottom: cy + r }, r, WHITE, 255);
    }

    fn paint_swatches(&self, s: &mut Surface, i: usize, hot: Option<u32>) {
        let m = s.metrics();
        let bw = m.px(1).max(1);
        let bg = self.cfg.background;
        for k in 0..=BACKGROUND_PRESETS.len() {
            let id = item_id(i, Some(k));
            let Some(hit) = self.hits.rect(id) else { continue };
            let g = m.px(3);
            let rc = RECT { left: hit.left + g, top: hit.top + g, right: hit.right - g, bottom: hit.bottom - g };
            let d = rc.right - rc.left;
            if let Some((_, p)) = BACKGROUND_PRESETS.get(k) {
                s.fill_rr(rc, d / 2, p.rgb(), 255);
            } else {
                // Custom: hue wheel around the centre, white towards the middle.
                let (cx, cy) = (rc.left as f32 + d as f32 / 2.0, rc.top as f32 + d as f32 / 2.0);
                let rad = d as f32 / 2.0;
                s.fill_fn(rc, d / 2, &|x, y| {
                    let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                    let h = dy.atan2(dx).to_degrees() + 90.0;
                    let sat = ((dx * dx + dy * dy).sqrt() / rad).clamp(0.0, 1.0).powf(0.6);
                    hsv_to_rgb(h, sat, 1.0)
                });
            }
            s.stroke_rr(rc, d / 2, bw, SWATCH_RIM_ALPHA);
            // Selection ring outside the swatch; a dimmer one on hover.
            let ring = if swatch_selected(bg, k) {
                235
            } else if hot == Some(id) {
                90
            } else {
                0
            };
            s.stroke_rr(hit, (hit.right - hit.left) / 2, m.px(2).max(1), ring);
        }
    }

    fn paint_picker(&self, s: &mut Surface) {
        let m = s.metrics();
        let (h, sat, val) = self.hsv;
        let (sv, hue) = (self.sv, self.hue);
        let (sw, sh) = ((sv.right - sv.left - 1).max(1) as f32, (sv.bottom - sv.top - 1).max(1) as f32);
        s.fill_fn(sv, m.px(6), &|x, y| hsv_to_rgb(h, (x - sv.left) as f32 / sw, 1.0 - (y - sv.top) as f32 / sh));
        let hw = (hue.right - hue.left - 1).max(1) as f32;
        s.fill_fn(hue, (hue.bottom - hue.top) / 2, &|x, _| hsv_to_rgb((x - hue.left) as f32 / hw * 360.0, 1.0, 1.0));
        // Knobs: dark halo, white ring, the colour inside (readable on light and dark areas).
        let knob = |s: &mut Surface, x: i32, y: i32, r: i32, rgb: (u32, u32, u32)| {
            let c = |r: i32| RECT { left: x - r, top: y - r, right: x + r, bottom: y + r };
            s.fill_rr(c(r + m.px(1).max(1)), r + 1, (0, 0, 0), 110);
            s.fill_rr(c(r), r, WHITE, 255);
            let ri = r - m.px(2).max(1);
            s.fill_rr(c(ri), ri, rgb, 255);
        };
        let kx = sv.left + (sat * sw + 0.5) as i32;
        let ky = sv.top + ((1.0 - val) * sh + 0.5) as i32;
        knob(s, kx, ky, m.px(7), hsv_to_rgb(h, sat, val));
        let hx = hue.left + (h.rem_euclid(360.0) / 360.0 * hw + 0.5) as i32;
        knob(s, hx, (hue.top + hue.bottom) / 2, m.px(8), hsv_to_rgb(h, 1.0, 1.0));
        // Preview and hex code.
        let pv = rect_wh(sv.left, self.hex_y + (m.px(ROW_H) - m.px(20)) / 2, m.px(36), m.px(20));
        s.fill_rr(pv, m.px(5), self.cfg.background.rgb(), 255);
        s.stroke_rr(pv, m.px(5), m.px(1).max(1), SWATCH_RIM_ALPHA);
        let lh = s.line_height(Font::Normal);
        let hex = self.cfg.background.hex();
        s.text(&hex, pv.right + m.px(10), center_y(&pv, lh), Font::Normal, WHITE, TEXT_ALPHA);
    }
}

impl Content for SettingsPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::Settings
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let inset = m.px(8);
        let col = m.px(TEXT_COL);
        self.width = m.px(WIDTH);
        self.knob_r = m.px(KNOB_R);
        self.hits.clear();
        self.rows.clear();
        let mut y = inset;
        for (i, it) in self.items.iter().enumerate() {
            let h = match it {
                Item::Section(_) => m.px(SECTION_H) - if i == 0 { m.px(6) } else { 0 },
                Item::ColourSection => m.px(SECTION_H),
                Item::Swatches => m.px(SWATCH_ROW_H),
                Item::Picker => m.px(SV_H + PICKER_GAP + HUE_H + PICKER_GAP / 2 + ROW_H),
                Item::Slider(..) => m.px(SLIDER_ROW_H),
                _ => m.px(ROW_H),
            };
            let rc = rect_wh(inset, y, self.width - 2 * inset, h);
            match *it {
                Item::Section(_) | Item::ColourSection => {}
                Item::Choice(set, _) => {
                    // Segments right-aligned on the switches' right edge.
                    let ws = self.seg_widths(s, set);
                    let sh = m.px(SEG_H);
                    let mut x = rc.right - m.px(12) - ws.iter().sum::<i32>();
                    let sy = y + (h - sh) / 2;
                    for (j, w) in ws.iter().enumerate() {
                        self.hits.add(item_id(i, Some(j)), rect_wh(x, sy, *w, sh));
                        x += w;
                    }
                }
                Item::Swatches => {
                    // Evenly spread over the text column; each hit square includes the selection ring.
                    let n = BACKGROUND_PRESETS.len() as i32 + 1;
                    let d = m.px(SWATCH_D) + 2 * m.px(3);
                    let span = self.width - 2 * col + 2 * m.px(3);
                    let gap = (span - n * d) / (n - 1);
                    let x0 = col - m.px(3) + (span - n * d - (n - 1) * gap) / 2;
                    let sy = y + (h - d) / 2;
                    for k in 0..n {
                        self.hits.add(item_id(i, Some(k as usize)), rect_wh(x0 + k * (d + gap), sy, d, d));
                    }
                }
                Item::Picker => {
                    self.sv = rect_wh(col, y, self.width - 2 * col, m.px(SV_H));
                    self.hue = rect_wh(col, self.sv.bottom + m.px(PICKER_GAP), self.width - 2 * col, m.px(HUE_H));
                    self.hex_y = self.hue.bottom + m.px(PICKER_GAP / 2);
                    // Grabbable a little beyond the edges (the value is clamped).
                    let g = m.px(4);
                    let grow = |r: RECT| RECT { left: r.left - g, top: r.top - g, right: r.right + g, bottom: r.bottom + g };
                    self.hits.add(item_id(i, Some(PART_SV)), grow(self.sv));
                    self.hits.add(item_id(i, Some(PART_HUE)), grow(self.hue));
                }
                Item::Slider(..) => {
                    let cy = y + h - m.px(15);
                    // Rail on the text column; the knob centre travels inside it.
                    let band = RECT { left: col, top: cy - m.px(10), right: self.width - col, bottom: cy + m.px(10) };
                    self.hits.add(item_id(i, Some(0)), band);
                }
                Item::Toggle(..) | Item::Open(..) | Item::Back(_) | Item::TrayChooser | Item::ResetSizes => {
                    self.hits.add(item_id(i, None), rc)
                }
            }
            self.rows.push((y, h));
            y += h;
            if matches!(it, Item::Back(_)) {
                y += m.px(4); // room for the separator under the title
            }
        }
        (self.width, y + inset)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let col = m.px(TEXT_COL);
        let ln = s.line_height(Font::Normal);
        let lsm = s.line_height(Font::Small);
        let chevron = |s: &mut Surface, row: &RECT, glyph: &str, x: i32, lit: bool| {
            let ty = center_y(row, s.line_height(Font::Normal));
            s.text(glyph, x, ty, Font::Normal, WHITE, lift(TEXT3_ALPHA, lit));
        };
        for (i, it) in self.items.iter().enumerate() {
            let Some(&(y, h)) = self.rows.get(i) else { continue };
            let row = rect_wh(m.px(8), y, self.width - 2 * m.px(8), h);
            match *it {
                Item::Section(label) => {
                    // Dim label sitting on the bottom of its band, close to the rows it names.
                    s.text(label, col, y + h - lsm - m.px(4), Font::Small, WHITE, TEXT3_ALPHA);
                }
                Item::ColourSection => {
                    let ty = y + h - lsm - m.px(4);
                    s.text("Colour", col, ty, Font::Small, WHITE, TEXT3_ALPHA);
                    let bg = self.cfg.background;
                    let name = preset_index(bg).map_or_else(|| "Custom".to_string(), |k| BACKGROUND_PRESETS[k].0.to_string());
                    let nw = s.text_width(&name, Font::Small);
                    s.text(&name, self.width - col - nw, ty, Font::Small, WHITE, TEXT2_ALPHA);
                }
                Item::Choice(set, label) => {
                    s.text(label, col, center_y(&row, ln), Font::Normal, WHITE, TEXT_ALPHA);
                    let sel = selected(&self.cfg, set, &self.screens);
                    let opts = options(set, &self.screens);
                    let n = opts.len();
                    let (Some(first), Some(last)) =
                        (self.hits.rect(item_id(i, Some(0))), self.hits.rect(item_id(i, Some(n - 1))))
                    else {
                        continue;
                    };
                    let track = RECT { right: last.right, ..first };
                    let r = (track.bottom - track.top) / 2;
                    s.fill_rr(track, r, WHITE, SEG_TRACK_ALPHA);
                    s.stroke_rr(track, r, m.px(1).max(1), SEG_RIM_ALPHA);
                    for (j, opt) in opts.iter().enumerate() {
                        let id = item_id(i, Some(j));
                        let Some(rc) = self.hits.rect(id) else { continue };
                        let ty = center_y(&rc, lsm);
                        let tx = rc.left + (rc.right - rc.left - s.text_width(opt, Font::Small)) / 2;
                        if sel == Some(j) {
                            s.fill_rr(rc, r, WHITE, 235);
                            s.text(opt, tx, ty, Font::Small, (0, 0, 0), 255);
                        } else {
                            let is_hot = hot == Some(id);
                            if is_hot {
                                let a = if pressed == Some(id) { PRESS_ALPHA } else { SEG_HOT_ALPHA };
                                s.fill_rr(rc, r, WHITE, a);
                            }
                            s.text(opt, tx, ty, Font::Small, WHITE, lift(TEXT2_ALPHA, is_hot));
                        }
                    }
                }
                Item::Toggle(set, label) => {
                    let lit = row_plate(s, row, item_id(i, None), hot, pressed);
                    s.text(label, row.left + m.px(12), center_y(&row, ln), Font::Normal, WHITE, lift(TEXT_ALPHA, lit));
                    let (sw, sh) = (m.px(32), m.px(18));
                    toggle_switch(s, row.right - m.px(12) - sw, row.top + (h - sh) / 2, is_on(&self.cfg, set));
                }
                Item::Open(page, label) => {
                    let lit = row_plate(s, row, item_id(i, None), hot, pressed);
                    s.text(label, row.left + m.px(12), center_y(&row, ln), Font::Normal, WHITE, lift(TEXT_ALPHA, lit));
                    let cw = s.text_width("\u{203A}", Font::Normal);
                    let cx = row.right - m.px(12) - cw;
                    chevron(s, &row, "\u{203A}", cx, lit);
                    if page == Page::Background {
                        // Current colour and opacity next to the chevron.
                        let d = m.px(14);
                        let sx = cx - m.px(8) - d;
                        let sw = rect_wh(sx, row.top + (h - d) / 2, d, d);
                        s.fill_rr(sw, d / 2, self.cfg.background.rgb(), 255);
                        s.stroke_rr(sw, d / 2, m.px(1).max(1), SWATCH_RIM_ALPHA);
                        let pct = format!("{}%", self.cfg.bar_opacity.min(100));
                        let pw = s.text_width(&pct, Font::Small);
                        s.text(&pct, sx - m.px(8) - pw, center_y(&row, lsm), Font::Small, WHITE, lift(TEXT2_ALPHA, lit));
                    }
                }
                Item::Back(title) => {
                    let lit = row_plate(s, row, item_id(i, None), hot, pressed);
                    // Back chevron (calendar glyph) in a 14 px slot, the title after it.
                    let g = m.px(14);
                    let x = row.left + m.px(9);
                    s.glyph(x, row.top + (h - g) / 2, g, lift(TEXT2_ALPHA, lit), &glyphs::chevron_left_shape);
                    let tx = x + g + m.px(6);
                    s.text(title, tx, center_y(&row, ln), Font::Normal, WHITE, lift(TEXT_ALPHA, lit));
                    hline(s, col, self.width - col, row.bottom + m.px(2));
                }
                Item::TrayChooser => {
                    let lit = row_plate(s, row, item_id(i, None), hot, pressed);
                    let ty = center_y(&row, ln);
                    s.text("Choose tray icons", row.left + m.px(12), ty, Font::Normal, WHITE, lift(TEXT_ALPHA, lit));
                    let cw = s.text_width("\u{203A}", Font::Normal);
                    chevron(s, &row, "\u{203A}", row.right - m.px(12) - cw, lit);
                }
                Item::Swatches => self.paint_swatches(s, i, hot),
                Item::Picker => self.paint_picker(s),
                Item::Slider(sl, label) => {
                    let is_hot = hot == Some(item_id(i, Some(0)));
                    self.paint_slider(s, i, sl, label, is_hot);
                }
                Item::ResetSizes => {
                    let enabled = !sizes_are_default(&self.cfg);
                    let lit = enabled && row_plate(s, row, item_id(i, None), hot, pressed);
                    let a = if enabled { lift(TEXT_ALPHA, lit) } else { TEXT3_ALPHA };
                    s.text("Reset sizes", row.left + m.px(12), center_y(&row, ln), Font::Normal, WHITE, a);
                }
            }
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn clickable(&self, id: u32) -> bool {
        let (i, part) = decode(id);
        match (self.items.get(i), part) {
            // The segment already selected does nothing.
            (Some(Item::Choice(set, _)), Some(j)) => selected(&self.cfg, *set, &self.screens) != Some(j),
            (Some(Item::Swatches), Some(k)) => k == BACKGROUND_PRESETS.len() || !swatch_selected(self.cfg.background, k),
            (Some(Item::ResetSizes), _) => !sizes_are_default(&self.cfg),
            _ => true,
        }
    }

    fn click(&mut self, id: u32) -> Reply {
        let (i, part) = decode(id);
        match (self.items.get(i).copied(), part) {
            (Some(Item::Choice(set, _)), Some(j)) if selected(&self.cfg, set, &self.screens) != Some(j) => {
                let sc = self.screens.clone();
                self.change(move |c| choose(c, set, j, &sc), true)
            }
            (Some(Item::Toggle(set, _)), None) => self.change(|c| flip(c, set), true),
            (Some(Item::Open(page, _)), None) => self.goto(page),
            (Some(Item::Back(_)), None) => self.goto(Page::Main),
            (Some(Item::TrayChooser), None) => {
                popup::next_tray_chooser();
                Reply::Switch(PopupKind::TrayOverflow)
            }
            (Some(Item::Swatches), Some(k)) => match BACKGROUND_PRESETS.get(k) {
                Some(&(_, bg)) if bg != self.cfg.background => {
                    self.hsv = rgb_to_hsv(bg.rgb());
                    self.change(move |c| c.background = bg, true)
                }
                Some(_) => Reply::None,
                None => {
                    // Custom swatch: opens (from the current colour) or closes the picker.
                    self.picker_open = !self.picker_open;
                    self.hsv = rgb_to_hsv(self.cfg.background.rgb());
                    self.items = items(&self.cfg, self.page, self.picker_open, &self.screens);
                    Reply::Relayout
                }
            },
            (Some(Item::ResetSizes), None) if !sizes_are_default(&self.cfg) => self.change(reset_sizes, true),
            _ => Reply::None,
        }
    }

    fn drag(&mut self, id: u32, x: i32, y: i32) -> Reply {
        let (i, part) = decode(id);
        match (self.items.get(i).copied(), part) {
            (Some(Item::Slider(sl, _)), Some(0)) => {
                let Some(v) = self.slider_at(i, sl, x) else { return Reply::None };
                if v == slider_value(&self.cfg, sl) {
                    return Reply::None;
                }
                // Same size while dragging: a plain redraw keeps the popup where it is.
                self.change(move |c| set_slider(c, sl, v), false);
                Reply::Redraw
            }
            (Some(Item::Picker), Some(PART_SV)) => {
                let sv = self.sv;
                let fx = |a: i32, lo: i32, hi: i32| ((a - lo) as f32 / (hi - lo - 1).max(1) as f32).clamp(0.0, 1.0);
                self.hsv.1 = fx(x, sv.left, sv.right);
                self.hsv.2 = 1.0 - fx(y, sv.top, sv.bottom);
                self.pick()
            }
            (Some(Item::Picker), Some(PART_HUE)) => {
                let hue = self.hue;
                let t = ((x - hue.left) as f32 / (hue.right - hue.left - 1).max(1) as f32).clamp(0.0, 1.0);
                // 360 would wrap to 0: keep the knob at the right end.
                self.hsv.0 = t * 359.9;
                self.pick()
            }
            _ => Reply::None,
        }
    }

    fn tick(&mut self) -> Reply {
        if self.apply_pending {
            self.apply_pending = false;
            self.last_apply = tick_ms();
            post_apply();
        }
        Reply::None
    }

    fn release(&mut self, _id: u32) -> Reply {
        if self.unsaved || self.apply_pending {
            // Saved and re-applied: the bar's answer re-anchors the popup (`popup::reanchor`).
            self.finish();
            return Reply::Redraw;
        }
        Reply::None
    }

    fn wheel(&mut self, notches: i32, _x: i32, y: i32) -> Reply {
        // Over a slider row: steps of the slider (opacity 5 %, sizes 1 px, modules 5 %).
        let row = self.rows.iter().position(|&(top, h)| y >= top && y < top + h);
        let Some(Item::Slider(sl, _)) = row.and_then(|i| self.items.get(i)).copied() else { return Reply::None };
        let (_, _, step, _) = slider_range(sl);
        let cur = slider_value(&self.cfg, sl);
        let v = cur + notches * step;
        let (lo, hi, _, _) = slider_range(sl);
        if v.clamp(lo, hi) == cur {
            return Reply::None;
        }
        self.change(move |c| set_slider(c, sl, v), false)
    }

    fn closing(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        assert_eq!(decode(item_id(0, None)), (0, None));
        assert_eq!(decode(item_id(5, Some(3))), (5, Some(3)));
        assert!(BACKGROUND_PRESETS.len() < ID_STRIDE as usize - 1);
    }

    #[test]
    fn choices_roundtrip() {
        let mut c = Config::default();
        let sc = test_screens();
        for set in [Setting::Position, Setting::Monitor, Setting::Display] {
            for j in 0..options(set, &sc).len() {
                choose(&mut c, set, j, &sc);
                assert_eq!(selected(&c, set, &sc), Some(j));
            }
        }
    }

    fn test_screens() -> Vec<Screen> {
        let d = |device: &str, label, primary| Screen { device: device.into(), label, primary };
        vec![d("\\\\.\\DISPLAY1", "1", true), d("\\\\.\\DISPLAY2", "2", false), d("\\\\.\\DISPLAY3", "3", false)]
    }

    #[test]
    fn screen_choice() {
        let sc = test_screens();
        assert_eq!(options(Setting::Monitor, &sc), ["Main", "2", "3", "Pointer"]);
        // "Pointer" is stored as `MONITOR_POINTER` and selected back.
        let mut p = Config::default();
        choose(&mut p, Setting::Monitor, 3, &sc);
        assert_eq!(p.monitor, config::MONITOR_POINTER);
        assert_eq!(selected(&p, Setting::Monitor, &sc), Some(3));
        let mut c = Config::default();
        assert_eq!(selected(&c, Setting::Monitor, &sc), Some(0));
        choose(&mut c, Setting::Monitor, 2, &sc);
        assert_eq!(c.monitor, "\\\\.\\DISPLAY3");
        // The primary is stored as "" (follow the primary monitor).
        choose(&mut c, Setting::Monitor, 0, &sc);
        assert!(c.monitor.is_empty());
        // A chosen display that is not connected: the bar is on the primary, "Main" is shown.
        c.monitor = "\\\\.\\DISPLAY9".into();
        assert_eq!(selected(&c, Setting::Monitor, &sc), Some(0));
        // One display: no screen row, the page is the same as before.
        let one = &sc[..1];
        let has = |s: &[Screen]| {
            items(&c, Page::Main, false, s).iter().any(|i| matches!(i, Item::Choice(Setting::Monitor, _)))
        };
        assert!(!has(one) && has(&sc));
    }

    #[test]
    fn toggles_flip() {
        let mut c = Config::default();
        for set in [
            Setting::AutoHide,
            Setting::Previews,
            Setting::Seconds,
            Setting::Module("metrics"),
            Setting::Module("tray"),
        ] {
            let was = is_on(&c, set);
            flip(&mut c, set);
            assert_eq!(is_on(&c, set), !was);
        }
        assert!(!items(&c, Page::Main, false, &[]).iter().any(|i| matches!(i, Item::TrayChooser)));
    }

    #[test]
    fn sliders_clamp_and_reset() {
        let mut c = Config::default();
        for sl in [Slider::Opacity, Slider::AppIcon, Slider::AppText, Slider::TrayIcon, Slider::Module] {
            let (lo, hi, _, _) = slider_range(sl);
            set_slider(&mut c, sl, hi + 50);
            assert_eq!(slider_value(&c, sl), hi);
            set_slider(&mut c, sl, lo - 50);
            assert_eq!(slider_value(&c, sl), lo);
        }
        assert_eq!(c.bar_opacity, 0);
        assert!(!sizes_are_default(&c));
        reset_sizes(&mut c);
        assert!(sizes_are_default(&c));
    }

    #[test]
    fn hsv_roundtrip() {
        for (_, bg) in BACKGROUND_PRESETS {
            assert_eq!(background_of(rgb_to_hsv(bg.rgb())), bg);
        }
        for rgb in [0xFF0000u32, 0x00FF00, 0x0000FF, 0x123456, 0xFFFFFF, 0x808080] {
            let bg = Background(rgb);
            assert_eq!(background_of(rgb_to_hsv(bg.rgb())), bg);
        }
        assert_eq!(hsv_to_rgb(359.9, 1.0, 1.0).0, 255);
    }

    #[test]
    fn swatch_selection() {
        assert!(swatch_selected(Background::BLACK, 0));
        assert!(swatch_selected(Background::DARK, 1));
        assert!(swatch_selected(Background(0x123456), BACKGROUND_PRESETS.len()));
        assert!(!swatch_selected(Background(0x123456), 0));
    }
}
