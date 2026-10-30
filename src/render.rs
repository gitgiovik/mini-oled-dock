//! Bar rendering: premultiplied 32-bit DIB + `UpdateLayeredWindow`.
//!
//! The module has NO domain state: the UI passes it a `Scene` (what to draw) and gets back
//! a `Layout` (where each item sits, in physical pixels). Primitives:
//!  * rounded rect with manual antialiasing (per-pixel coverage at the edges), 1 px border;
//!  * blend of premultiplied `IconBitmap`s (with opacity and, transitionally, nearest-neighbour scaling);
//!  * hand-composited grayscale GDI text: WHITE `DrawTextW` onto a black scratch DIB, the
//!    coverage is read from the G channel and composited with the desired colour/alpha into the main DIB;
//!  * bell and speaker (0-3 waves, crossed out when muted) drawn with geometric primitives
//!    evaluated with 4x4 supersampling (no resources, no icon font).
//!
//! Style:
//! bar height 36 at the default sizes (it grows with the tallest content, see `BarSizes` and
//! `bar_height`), radius 11, background `background` colour at `bar_opacity` (default black at 95 %,
//! never below alpha 1 inside the pill so the bar keeps receiving the mouse), 1 px border
//! RGBA(255,255,255,22), text RGBA(255,255,255,230) "Segoe UI Variable Text" 13 px (fallback
//! "Segoe UI"; app names at `app_text_size`, modules scaled by `module_size`), secondary text
//! RGBA(255,255,255,170), hover plate radius 7 (height 26, with a 1 px inner border), vertical separator 1 px wide, 16 high, RGBA(255,255,255,30),
//! 16 px app icon centred with the name on `CONTENT_CY`, indicator: 2-high pill centred under the
//! item content (icon and/or name): open = 50% of the content width at alpha 120, foreground = 75%
//! at full white, attention = 75% orange; pinned and closed: none and the item at 70%. The
//! open <-> foreground change is animated by the UI through `AppView::active_t`.
//! Right zone: `sep | tray icons [chevron] | sep | system modules` (metrics, bell, network, volume,
//! clock, show desktop, power: only the visible ones, in `Scene::modules` order).
//!
//! Reordering: the UI hands `layout` the orders (`Scene::modules`; apps and tray icons arrive
//! already sorted) and, during a drag, moves the rectangles of the zone being reordered; `paint` draws the
//! lifted item (`Scene::lift`) last, on top of its plate.
//!
//! If the scene is wider than `Scene::max_width`, `layout` degrades in 3 steps: (0) icon + name,
//! (1) icon only, (2) icon only with reduced padding; if that is not enough, the last apps are
//! left out of the layout (so they are neither visible nor clickable).
//!
//! The GDI backing (DCs, DIBs), the fonts helpers, the text compositing, the presentation and the
//! pixel primitives (`Canvas`) are the ones of `surface.rs`, shared with the popups: this module
//! owns the bar state (`Gfx`), the layout and what to paint where.
//!
//! THREADING: UI thread only (thread-local GDI state). DIBs are recreated by `resize`.

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{HDC, HFONT};

use crate::audio::VolumeState;
use crate::config::DisplayMode;
use crate::glyphs;
use crate::icons::IconBitmap;
use crate::log;
use crate::metrics::Usage;
use crate::net::{NetKind, NetStatus};
use crate::surface::{
    argb, create_font, delete_font, div255, line_height, over, rect_wh, rr_cov, white, Canvas, Gdi, TEXT_FACES, WHITE,
};

// ------------------------------------------------------------ logical measures

/// Bar height (logical px).
pub const BAR_HEIGHT: i32 = 36;
/// Margin from the screen edge (logical px).
pub const EDGE_MARGIN: i32 = 8;
/// Bar corner radius (logical px).
pub const BAR_RADIUS: i32 = 11;
/// Hover plate radius (logical px).
pub const PLATE_RADIUS: i32 = 7;
/// App icon side (logical px).
pub const ICON_SIZE: i32 = 16;
/// Maximum program name length (characters); beyond that: truncated with "…".
pub const NAME_MAX_CHARS: usize = 20;

/// Horizontal bar padding (logical px).
const PAD_BAR: i32 = 6;
/// Gap between two app items (logical px): 0, hover plates are never visible on two adjacent items at once. In the right zone (tray + system) items are adjacent.
const ITEM_GAP: i32 = 0;
/// Inner padding of an item on the left and right (logical px).
const ITEM_PAD: i32 = 8;
/// Reduced padding (mode 2: icon only, bar too wide).
const ITEM_PAD_TIGHT: i32 = 6;
/// Horizontal clock padding (logical px).
const CLOCK_PAD: i32 = 6;
/// Gap between icon and name (logical px).
const NAME_GAP: i32 = 6;
/// Gap on each side of the separator (logical px).
const ZONE_GAP: i32 = 8;
/// Vertical margin between the bar edge and the plate (logical px): the plate is `BAR_HEIGHT - 2 * margin` high.
const ITEM_VMARGIN: i32 = 5;
/// Vertical axis of the icons and text of a row (bar midpoint, logical px).
const CONTENT_CY: i32 = 18;
/// Vertical axis of the two-line clock (bar midpoint, logical px).
const CLOCK_CY: i32 = 18;
/// Upward shift of the two-line clock, in half logical px (3 = 1.5 px): optically centres
/// the block, whose date line looks heavy at the bottom.
const CLOCK_UP_HALVES: i32 = 3;
/// Indicator width as a fraction of the content width: app open / app in the foreground (and attention).
const INDICATOR_OPEN_FRAC: f32 = 0.5;
const INDICATOR_ACTIVE_FRAC: f32 = 0.75;
/// Distance of the indicator from the bar edge (logical px).
const INDICATOR_EDGE: i32 = 2;
/// Indicator height (logical px).
const INDICATOR_H: i32 = 2;
/// Colour of the attention indicator (a window asked for attention): the only colour in the UI.
const ATTENTION_RGB: (u32, u32, u32) = (255, 140, 0);
/// Distance of the indicator from the icon's bottom edge with the bar at the top (logical px).
const INDICATOR_BELOW_ICON: i32 = 2;
/// Radius of the app icon placeholder (logical px).
const PLACEHOLDER_RADIUS: i32 = 4;
/// Horizontal padding of every "square" glyph-only item (tray, chevron, bell, volume without
/// percentage, show desktop, power): same width for all (logical px).
const TRAY_PAD: i32 = 5;
/// Overflow chevron padding (logical px): narrower item, the ink keeps pace with the icons.
const CHEV_PAD: i32 = 2;
/// Side of the vector system icons (logical px).
const SYS_ICON: i32 = 16;
/// Gap between speaker and percentage (logical px).
const SYS_TEXT_GAP: i32 = 4;
/// Separator height (logical px).
const SEP_HEIGHT: i32 = 16;
/// Opacity (out of 255, relative to the glyph alpha) of the unlit Wi-Fi arcs.
const NET_DIM_ALPHA: u32 = 80;
/// Icon opacity of a pinned app without windows (idle / hover).
const DIM_OPACITY: u32 = 178;
const DIM_OPACITY_HOT: u32 = 230;

// Colours (not premultiplied) and alpha. The background colour and alpha come from the config
// (`background`, `bar_opacity`: `Config::bar_alpha`, 242 by default).
const BORDER_ALPHA: u32 = 22;
const TEXT_ALPHA: u32 = 230;
const TEXT2_ALPHA: u32 = 170;
const SEP_ALPHA: u32 = 30;
/// Alpha of the "open" indicator.
const OPEN_ALPHA: u32 = 120;
/// Text alpha of a pinned app without windows (230 * 0.7), idle / hover.
const DIM_TEXT_ALPHA: u32 = 161;
const DIM_TEXT_ALPHA_HOT: u32 = 215;
const ICON_ALPHA: u32 = 230;
/// Alpha of text and glyphs under the mouse (they brighten to the maximum).
const TEXT_ALPHA_HOT: u32 = 255;
const ICON_ALPHA_HOT: u32 = 255;
const TEXT2_ALPHA_HOT: u32 = 215;
/// 1 px inner border of the hover plate (white alpha) and the reference fill alpha it
/// corresponds to: while the plate fades, the border scales proportionally.
const PLATE_RIM_ALPHA: u32 = 18;
const PLATE_REF_ALPHA: u32 = 20;

/// DPI scale: converts logical measures to physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// Effective DPI of the monitor (96 = 100%).
    pub dpi: u32,
}

impl Metrics {
    /// Scale factor (dpi / 96).
    pub fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    /// Logical measure -> physical pixels (rounded).
    pub fn px(&self, logical: i32) -> i32 {
        ((logical as f32) * self.scale()).round() as i32
    }
}

/// The configured sizes of the bar content (`Config::sizes`), in logical px / percent. The bar is
/// drawn at the monitor DPI; each part is then sized on its own: app icons (requested from the shell
/// at that exact pixel size, never stretched), app names (own font), tray icons (rescaled by the tray
/// host), system modules (laid out at `module_metrics`: glyphs, fonts and spacing scale together).
/// The bar height follows the tallest content (`bar_height`). The defaults reproduce the original
/// 36 px bar exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarSizes {
    /// App icon side (logical px).
    pub icon: i32,
    /// App name font height (logical px).
    pub text: i32,
    /// Tray icon side (logical px).
    pub tray: i32,
    /// Scale of the system modules (percent).
    pub module_pct: u32,
    /// The names can be shown (display mode other than icon only): their height counts.
    pub text_on: bool,
    /// The tray is on: its icons' height counts.
    pub tray_on: bool,
}

impl Default for BarSizes {
    fn default() -> Self {
        BarSizes::from_config(&crate::config::Config::default())
    }
}

impl BarSizes {
    /// Sizes of the config `c` (clamped to their ranges).
    pub fn from_config(c: &crate::config::Config) -> BarSizes {
        let (icon, text, tray, module) = c.sizes();
        BarSizes {
            icon: icon as i32,
            text: text as i32,
            tray: tray as i32,
            module_pct: module as u32,
            text_on: c.display_mode != DisplayMode::Icon,
            tray_on: c.show_tray,
        }
    }

    /// Metrics of the system modules: the bar DPI scaled by `module_pct` (100 % = the bar metrics).
    pub fn module_metrics(self, m: Metrics) -> Metrics {
        Metrics { dpi: ((m.dpi * self.module_pct + 50) / 100).max(1) }
    }

    /// Physical side of the app icons.
    pub fn icon_px(self, m: Metrics) -> i32 {
        m.px(self.icon).max(1)
    }

    /// Physical side of the tray icons.
    pub fn tray_px(self, m: Metrics) -> i32 {
        m.px(self.tray).max(1)
    }

    /// Visual height (logical px) of an app name of font height `text`: 16 for the default 13 px,
    /// the same box as a default icon.
    fn text_box(self) -> i32 {
        (self.text * ICON_SIZE + 6) / 13
    }
}

/// Bar height (physical px): the tallest content plus the original vertical air (10 logical px above
/// and below a 16 px icon; the modules scale their whole 36 px band). The defaults give 36 px.
pub fn bar_height(m: Metrics, s: BarSizes) -> i32 {
    let air = m.px(BAR_HEIGHT) - m.px(ICON_SIZE);
    let mut h = (s.icon_px(m) + air).max(s.module_metrics(m).px(BAR_HEIGHT));
    if s.text_on {
        h = h.max(m.px(s.text_box()) + air);
    }
    if s.tray_on {
        h = h.max(s.tray_px(m) + air);
    }
    h
}

/// Centre line (physical px) of content laid out at metrics `m` in a bar `h` high: the original axis
/// (`CONTENT_CY`) moved down by half the extra height.
fn axis(m: Metrics, h: i32, logical_cy: i32) -> i32 {
    m.px(logical_cy) + (h - m.px(BAR_HEIGHT)) / 2
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics { dpi: 96 }
    }
}

// ------------------------------------------------------------------ scene

/// Identifies a clickable bar item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemId {
    /// App item: index into the `Scene::apps` slice (same visual order, from the left).
    App(usize),
    /// Tray icon of another app: stable id from `tray_host::TrayView::id`.
    Tray(u64),
    /// Chevron that opens the tray overflow popup.
    TrayOverflow,
    Notifications,
    Volume,
    Clock,
    /// "Start" button: fixed item at the left end of the bar (see `Scene::start_button`), not a module.
    Start,
    /// "Show desktop" button.
    ShowDesktop,
    /// Power button: opens the power popup.
    Power,
    /// Network status (Wi-Fi bars / ethernet + internet badge): opens the network popup.
    Network,
    /// CPU / RAM usage (two small text lines).
    Metrics,
}

/// Status indicator under the icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indicator {
    /// Pinned without windows.
    None,
    /// Has open windows (bar 2 high, alpha 110).
    Open,
    /// A window is in the foreground (bar 2 high, alpha 255).
    Active,
    /// A window requested attention (visible phase of the blink): solid orange bar.
    Attention,
}

/// An app item to draw.
#[derive(Clone, Copy, Debug)]
pub struct AppView<'a> {
    /// Full program name; `render` truncates it to `NAME_MAX_CHARS` with "…".
    pub name: &'a str,
    /// Icon already at the physical size (`Metrics::px(ICON_SIZE)`); `None` = placeholder
    /// (grey rounded square, radius 4).
    pub icon: Option<&'a IconBitmap>,
    pub indicator: Indicator,
    /// Draws icon AND name at 70% opacity (pinned without windows).
    pub dimmed: bool,
    /// 0..=1: how "foreground" the indicator is (0 = open width / dim, 1 = foreground width / white).
    /// Animated by the UI; used for `Indicator::Open` and `Indicator::Active`.
    pub active_t: f32,
}

/// Content of the system zone.
#[derive(Clone, Copy, Debug)]
pub struct SystemView<'a> {
    pub volume: VolumeState,
    /// Shows "NN%" next to the speaker.
    pub show_volume_percent: bool,
    /// "HH:mm"
    pub time: &'a str,
    /// Short date; `None` = do not show it (`show_date` false).
    pub date: Option<&'a str>,
    /// Badge dot on the bell (there are notifications).
    pub bell_badge: bool,
    /// Crossed-out bell ("Do not disturb" active).
    pub bell_dnd: bool,
    /// Network glyph state (`ItemId::Network`).
    pub net: NetStatus,
    /// CPU / RAM percentages (`ItemId::Metrics`).
    pub usage: Usage,
}

/// A tray icon to draw (bitmap already at `Metrics::px(SYS_ICON)`).
#[derive(Clone, Copy, Debug)]
pub struct TrayIcon<'a> {
    /// Stable id (`tray_host::TrayView::id`).
    pub id: u64,
    pub icon: &'a IconBitmap,
}

/// Names of the system modules in `config.modules_order`, with their items, in the default order
/// (`config::DEFAULT_MODULES_ORDER`).
pub const MODULES: [(&str, ItemId); 7] = [
    ("metrics", ItemId::Metrics),
    ("notifications", ItemId::Notifications),
    ("network", ItemId::Network),
    ("volume", ItemId::Volume),
    ("clock", ItemId::Clock),
    ("show_desktop", ItemId::ShowDesktop),
    ("power", ItemId::Power),
];

/// Validated order of ALL the system modules: names in `names` first (unknown and repeated ones are
/// ignored), then each missing module right after its nearest preceding default neighbour that is
/// placed (at the front if none), so a module added in a new version lands where it belongs.
pub fn modules_from_names(names: &[String]) -> Vec<ItemId> {
    let mut out: Vec<ItemId> = Vec::with_capacity(MODULES.len());
    for name in names {
        if let Some((_, id)) = MODULES.iter().find(|(m, _)| m.eq_ignore_ascii_case(name.trim())) {
            if !out.contains(id) {
                out.push(*id);
            }
        }
    }
    for (k, (_, id)) in MODULES.iter().enumerate() {
        if out.contains(id) {
            continue;
        }
        let at = MODULES[..k]
            .iter()
            .rev()
            .find_map(|(_, prev)| out.iter().position(|o| o == prev))
            .map_or(0, |p| p + 1);
        out.insert(at, *id);
    }
    out
}

/// The visible system modules, in order: `modules_from_names` without the ones switched off in the
/// config (`Config::module_visible`; power is always visible).
pub fn visible_modules(cfg: &crate::config::Config) -> Vec<ItemId> {
    let mut v = modules_from_names(&cfg.modules_order);
    v.retain(|id| module_name(*id).is_none_or(|n| cfg.module_visible(n)));
    v
}

/// Config name of a system module item.
pub fn module_name(id: ItemId) -> Option<&'static str> {
    MODULES.iter().find(|(_, m)| *m == id).map(|(n, _)| *n)
}

/// An item lifted by a drag: drawn last, with a plate of alpha `alpha` behind it.
#[derive(Clone, Copy, Debug)]
pub struct Lift {
    pub id: ItemId,
    pub alpha: u8,
}

/// Hover/press plate: rounded rectangle (radius `PLATE_RADIUS`) behind an item, with a
/// 1 px inner border.
#[derive(Clone, Copy, Debug)]
pub struct Plate {
    /// Rectangle in bar client coordinates (physical pixels). The UI interpolates it between items.
    pub rect: RECT,
    /// Final white alpha 0..=255 (hover 20, press 36, multiplied by the animation).
    pub alpha: u8,
}

/// Everything needed to draw a frame.
#[derive(Clone, Copy, Debug)]
pub struct Scene<'a> {
    pub metrics: Metrics,
    /// Content sizes (`BarSizes::from_config`); must match the ones given to `init` / `set_metrics`.
    pub sizes: BarSizes,
    pub apps: &'a [AppView<'a>],
    /// Tray icons in registration order.
    pub tray: &'a [TrayIcon<'a>],
    /// false = no overflow (popup unavailable): all icons are shown, with no chevron.
    pub overflow_enabled: bool,
    /// Show the chevron even without count-based overflow (the user hid some icons).
    pub force_chevron: bool,
    /// Bar at the top: the overflow chevron points down.
    pub bar_top: bool,
    pub system: SystemView<'a>,
    pub plate: Option<Plate>,
    /// Visible system items, in order (`visible_modules`).
    pub modules: &'a [ItemId],
    /// Show the Start button as the first item of the bar (`Config::show_start_button`). Not draggable.
    pub start_button: bool,
    /// Item being dragged (its rectangle in the layout already follows the cursor).
    pub lift: Option<Lift>,
    /// Item under the mouse (or of the open popup): text and glyphs brighten.
    pub hover: Option<ItemId>,
    /// Maximum bar width in physical pixels (0 = unlimited). Above this value
    /// `layout` shrinks the names / the items (see the module doc).
    pub max_width: i32,
}

/// Rectangle of an item, in client coordinates (physical pixels).
#[derive(Clone, Copy, Debug)]
pub struct ItemRect {
    pub id: ItemId,
    pub rect: RECT,
}

/// Result of `layout`: bar size and item positions.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// Bar width in physical pixels (content + padding).
    pub width: i32,
    /// Bar height in physical pixels (`bar_height`).
    pub height: i32,
    /// Items in visual order: the Start button (if shown), apps, tray icons, chevron, then the visible system modules.
    pub items: Vec<ItemRect>,
    /// X coordinate (client, physical px) of the separator between the app zone and the tray/system zone; -1 if absent
    /// (no apps).
    pub separator_x: i32,
    /// X coordinate of the separator between the tray zone and the system items; -1 if absent (no tray icons).
    pub separator2_x: i32,
    /// Union of the tray items + chevron (client); if there are none, a 0-wide rect to the left of
    /// the system items.
    pub tray_area: RECT,
    /// Ids of the tray icons that ended up in the overflow (not in the layout).
    pub tray_hidden: Vec<u64>,
    /// true if the overflow chevron is in the layout.
    pub has_chevron: bool,
    /// Degradation mode used (0 = icon + name, 1 = icon only, 2 = compact icon only).
    pub mode: u8,
}

impl Layout {
    /// Item under the client point `(x, y)`, if any.
    pub fn hit_test(&self, x: i32, y: i32) -> Option<ItemId> {
        self.items
            .iter()
            .find(|i| x >= i.rect.left && x < i.rect.right && y >= i.rect.top && y < i.rect.bottom)
            .map(|i| i.id)
    }

    /// Rectangle of item `id`.
    pub fn rect_of(&self, id: ItemId) -> Option<RECT> {
        self.items.iter().find(|i| i.id == id).map(|i| i.rect)
    }
}

/// Hover plate of an item: the hit rectangle (as tall as the bar, for easy clicking) narrowed
/// vertically by `ITEM_VMARGIN` (scaled with the bar height). Square items (width 26) give a 26x26
/// plate at the default sizes.
pub fn plate_rect(item: RECT, m: Metrics) -> RECT {
    let base = m.px(BAR_HEIGHT).max(1);
    let v = m.px(ITEM_VMARGIN) * (item.bottom - item.top) / base;
    RECT { left: item.left, top: item.top + v, right: item.right, bottom: item.bottom - v }
}

// ------------------------------------------------------------ GDI state

struct Gfx {
    metrics: Metrics,
    /// Memory DCs, main DIB (source of `UpdateLayeredWindow`) and text scratch DIB.
    gdi: Gdi,
    sizes: BarSizes,
    fonts: Fonts,
    /// Width (physical px) of the pill during the width animation; 0 = full.
    pill_w: i32,
}

thread_local! {
    static GFX: RefCell<Option<Gfx>> = const { RefCell::new(None) };
}

/// Fonts of the bar: app names (`app_text_size`), module text and small module text (13 and 11 px
/// at the module scale), with their line heights (physical px).
struct Fonts {
    app: HFONT,
    module: HFONT,
    small: HFONT,
    lh_app: i32,
    lh: i32,
    lh_small: i32,
}

impl Fonts {
    fn get(&self, t: Txt) -> (HFONT, i32) {
        match t {
            Txt::App => (self.app, self.lh_app),
            Txt::Module => (self.module, self.lh),
            Txt::Small => (self.small, self.lh_small),
        }
    }

    fn max_lh(&self) -> i32 {
        self.lh_app.max(self.lh).max(self.lh_small)
    }

    fn delete(&self) {
        for f in [self.app, self.module, self.small] {
            delete_font(f);
        }
    }
}

/// Which bar font a text uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Txt {
    /// App names.
    App,
    /// Module text (clock time, volume percentage).
    Module,
    /// Small module text (date, metrics).
    Small,
}

// ------------------------------------------------------------------ API

/// Creates the GDI resources (memory DCs, Segoe UI Variable Text fonts -> Segoe UI fallback, see
/// `Fonts`) for the given `metrics` and `sizes`. The DIB is born with the first `resize`.
pub fn init(metrics: Metrics, sizes: BarSizes) -> Result<(), String> {
    destroy();
    let gdi = Gdi::new()?;
    // On error `gdi` is dropped, which deletes the DCs.
    let fonts = build_fonts(gdi.dc_txt(), metrics, sizes)?;
    GFX.with(|c| {
        *c.borrow_mut() = Some(Gfx { metrics, gdi, sizes, fonts, pill_w: 0 });
    });
    log!("render::init: dpi={} {sizes:?}", metrics.dpi);
    Ok(())
}

fn build_fonts(dc: HDC, m: Metrics, s: BarSizes) -> Result<Fonts, String> {
    let mm = s.module_metrics(m);
    let mut made: Vec<HFONT> = Vec::with_capacity(3);
    for px in [m.px(s.text), mm.px(13), mm.px(11)] {
        match create_font(dc, &TEXT_FACES, px.max(1), 400) {
            Ok(f) => made.push(f),
            Err(e) => {
                for f in made {
                    delete_font(f);
                }
                return Err(e);
            }
        }
    }
    let (app, module, small) = (made[0], made[1], made[2]);
    Ok(Fonts {
        app,
        module,
        small,
        lh_app: line_height(dc, app),
        lh: line_height(dc, module),
        lh_small: line_height(dc, small),
    })
}

/// New DPI or new content sizes: recreates the fonts. The caller must then redo `layout`, `resize`,
/// `paint`, `present`.
pub fn set_metrics(metrics: Metrics, sizes: BarSizes) -> Result<(), String> {
    GFX.with(|c| {
        let mut b = c.borrow_mut();
        let Some(g) = b.as_mut() else { return Err("render not initialized".to_string()) };
        if g.metrics == metrics && g.sizes == sizes {
            return Ok(());
        }
        let fonts = build_fonts(g.gdi.dc_txt(), metrics, sizes)?;
        // The old fonts are not selected in the DCs (they are only selected temporarily).
        g.fonts.delete();
        g.fonts = fonts;
        g.metrics = metrics;
        g.sizes = sizes;
        // The DIBs depend on the DPI (height of the text DIB): they are recreated by the next `resize`.
        g.gdi.free_dibs();
        log!("render::set_metrics: dpi={} {sizes:?}", metrics.dpi);
        Ok(())
    })
}

/// Width in pixels of text `s` with font `t`. Without `init` it estimates 7 px/char (6 for `Small`).
fn measure(s: &str, t: Txt, m: Metrics) -> i32 {
    if s.is_empty() {
        return 0;
    }
    let measured = GFX.with(|c| {
        let b = c.borrow();
        let g = b.as_ref()?;
        Some(text_width(g, s, t))
    });
    measured.unwrap_or_else(|| s.chars().count() as i32 * m.px(if t == Txt::Small { 6 } else { 7 }))
}

/// Width in pixels of text `s` with font `t` (7 px per UTF-16 unit if GDI fails).
fn text_width(g: &Gfx, s: &str, t: Txt) -> i32 {
    g.gdi.text_width(s, g.fonts.get(t).0, 7)
}

/// Widths of the system items (physical pixels).
struct SysWidths {
    bell: i32,
    volume: i32,
    clock: i32,
    desk: i32,
    start: i32,
    power: i32,
    net: i32,
    metrics: i32,
}

fn sys_widths(scene: &Scene<'_>) -> SysWidths {
    // The modules are laid out at their own scale.
    let m = scene.sizes.module_metrics(scene.metrics);
    let pad = m.px(CLOCK_PAD);
    let sys = &scene.system;
    let bell = 2 * m.px(TRAY_PAD) + m.px(SYS_ICON);
    let mut volume = bell;
    if sys.show_volume_percent && sys.volume.available {
        // Percentage slot: two digits usually, three only at 100% (rarely changes).
        let slot = if sys.volume.level >= 100 { "100%" } else { "88%" };
        volume += m.px(SYS_TEXT_GAP) + measure(slot, Txt::Module, m);
    }
    let tw = measure(sys.time, Txt::Module, m).max(measure("00:00", Txt::Module, m));
    let dw = sys.date.map(|d| measure(d, Txt::Small, m)).unwrap_or(0);
    let clock = 2 * pad + tw.max(dw);
    // Metrics are measured only when the module is visible.
    let metrics = if scene.modules.contains(&ItemId::Metrics) {
        let label = measure(METRIC_LABELS[0], Txt::Small, m).max(measure(METRIC_LABELS[1], Txt::Small, m));
        2 * pad + label + m.px(METRIC_GAP) + measure(metric_slot(sys.usage), Txt::Small, m)
    } else {
        0
    };
    SysWidths { bell, volume, clock, desk: bell, start: bell, power: bell, net: bell, metrics }
}

/// Labels of the two metric lines.
const METRIC_LABELS: [&str; 2] = ["CPU", "RAM"];
/// Gap between a metric label and its value (logical px).
const METRIC_GAP: i32 = 4;
/// Alpha of the metric labels (the values use the brighter secondary alpha).
const METRIC_LABEL_ALPHA: u32 = 120;

/// Width slot of the metric values: two digits usually, three only at 100% (like the volume).
fn metric_slot(u: Usage) -> &'static str {
    if u.cpu >= 100 || u.ram >= 100 {
        "100%"
    } else {
        "88%"
    }
}

/// Width of a system module item.
fn module_width(sw: &SysWidths, id: ItemId) -> i32 {
    match id {
        ItemId::Notifications => sw.bell,
        ItemId::Volume => sw.volume,
        ItemId::Clock => sw.clock,
        ItemId::ShowDesktop => sw.desk,
        ItemId::Start => sw.start,
        ItemId::Network => sw.net,
        ItemId::Metrics => sw.metrics,
        _ => sw.power,
    }
}

/// Computes the layout (measures text with the current font; draws nothing).
/// Pure with respect to the scene: same input -> same output.
/// Degradation by width: first the 3 modes of the app zone, then fewer tray icons (the excess goes
/// into the overflow), finally fewer apps.
pub fn layout(scene: &Scene<'_>) -> Layout {
    let m = scene.metrics;
    let sw = sys_widths(scene);
    let dm = crate::config::get().display_mode;
    // Names are only measured when some mode can show them.
    let name_w: Vec<i32> = if dm == DisplayMode::Icon {
        vec![0; scene.apps.len()]
    } else {
        scene.apps.iter().map(|a| measure(&truncate_name(a.name), Txt::App, m)).collect()
    };
    let n = scene.tray.len();
    let base_vis = n;
    let fits = |l: &Layout| scene.max_width <= 0 || l.width <= scene.max_width;
    for mode in 0..3u8 {
        if mode == 1 && dm == DisplayMode::Icon {
            continue; // identical to mode 0
        }
        let l = build_layout(scene, mode, dm, &name_w, &sw, base_vis, false);
        if fits(&l) {
            return l;
        }
    }
    let mut vis = base_vis;
    if scene.overflow_enabled {
        for v in (0..base_vis).rev() {
            let l = build_layout(scene, 2, dm, &name_w, &sw, v, false);
            if fits(&l) {
                return l;
            }
        }
        vis = 0;
    }
    build_layout(scene, 2, dm, &name_w, &sw, vis, true)
}

/// What an app item shows: `(icon, text)`. Only layout mode 0 honours the configured display mode;
/// the degraded modes are icon only. Text-only falls back to the icon while the name is unknown.
fn app_parts(dm: DisplayMode, layout_mode: u8, has_name: bool) -> (bool, bool) {
    if layout_mode != 0 {
        return (true, false);
    }
    match dm {
        DisplayMode::IconText => (true, has_name),
        DisplayMode::Icon => (true, false),
        DisplayMode::Text if has_name => (false, true),
        DisplayMode::Text => (true, false),
    }
}

/// Width (physical px) of the visible content of an app item, without the item padding.
fn content_width(has_icon: bool, has_text: bool, icon: i32, gap: i32, text_w: i32) -> i32 {
    match (has_icon, has_text) {
        (true, true) => icon + gap + text_w,
        (true, false) => icon,
        (false, _) => text_w,
    }
}

fn build_layout(
    scene: &Scene<'_>,
    mode: u8,
    dm: DisplayMode,
    name_w: &[i32],
    sw: &SysWidths,
    vis: usize,
    limit: bool,
) -> Layout {
    let m = scene.metrics;
    let pad_bar = m.px(PAD_BAR);
    let gap = m.px(ITEM_GAP);
    let height = bar_height(m, scene.sizes);
    // Items have full height (comfortable hit-testing); the plate narrows them (`plate_rect`).
    let top = 0;
    let bottom = height;
    let item_pad = if mode == 2 { m.px(ITEM_PAD_TIGHT) } else { m.px(ITEM_PAD) };
    let icon = scene.sizes.icon_px(m);
    let name_gap = m.px(NAME_GAP);
    let sep_w = m.px(1).max(1);
    let zone_gap = m.px(ZONE_GAP);
    let tray_px = scene.sizes.tray_px(m);
    let tray_w = 2 * m.px(TRAY_PAD) + tray_px;
    let chev_w = 2 * m.px(CHEV_PAD) + tray_px;
    let vis = vis.min(scene.tray.len());
    let has_chev = scene.overflow_enabled && (vis < scene.tray.len() || scene.force_chevron);
    let tray_count = vis + usize::from(has_chev);
    let tray_block =
        if tray_count > 0 { vis as i32 * tray_w + if has_chev { chev_w } else { 0 } + 2 * zone_gap + sep_w } else { 0 };
    let sys_total: i32 = scene.modules.iter().map(|id| module_width(sw, *id)).sum();
    let right_total = tray_block + sys_total;

    let fixed = usize::from(scene.start_button);
    let mut items = Vec::with_capacity(fixed + scene.apps.len() + tray_count + scene.modules.len());
    let mut x = pad_bar;
    // Start button: first item, the width of the other glyph buttons (module scale), same gap as the apps.
    if scene.start_button {
        let w = sw.start;
        items.push(ItemRect { id: ItemId::Start, rect: RECT { left: x, top, right: x + w, bottom } });
        x += w + gap;
    }
    let mut napps = 0usize;
    for i in 0..scene.apps.len() {
        let nw = name_w.get(i).copied().unwrap_or(0);
        let (has_icon, has_text) = app_parts(dm, mode, nw > 0);
        let w = 2 * item_pad + content_width(has_icon, has_text, icon, name_gap, nw);
        if limit && x + w + 2 * zone_gap + sep_w + right_total + pad_bar > scene.max_width {
            break;
        }
        items.push(ItemRect { id: ItemId::App(i), rect: RECT { left: x, top, right: x + w, bottom } });
        x += w + gap;
        napps += 1;
    }
    let separator_x;
    if napps > 0 || scene.start_button {
        // App side (with no apps the separator follows the Start button): app items have more inner air (ITEM_PAD)
        // than tray items (TRAY_PAD); the difference is subtracted to optically centre the separator.
        let app_side = (zone_gap - (item_pad - m.px(TRAY_PAD))).max(1);
        separator_x = x - gap + app_side;
        x = separator_x + sep_w + zone_gap;
    } else {
        separator_x = -1;
    }
    // Tray zone.
    let separator2_x;
    let tray_area;
    if tray_count > 0 {
        let first = x;
        for t in scene.tray.iter().take(vis) {
            items.push(ItemRect { id: ItemId::Tray(t.id), rect: RECT { left: x, top, right: x + tray_w, bottom } });
            x += tray_w;
        }
        if has_chev {
            items.push(ItemRect { id: ItemId::TrayOverflow, rect: RECT { left: x, top, right: x + chev_w, bottom } });
            x += chev_w;
        }
        tray_area = RECT { left: first, top, right: x, bottom };
        separator2_x = x + zone_gap;
        x = separator2_x + sep_w + zone_gap;
    } else {
        separator2_x = -1;
        tray_area = RECT { left: x, top, right: x, bottom };
    }
    for &id in scene.modules {
        let w = module_width(sw, id);
        items.push(ItemRect { id, rect: RECT { left: x, top, right: x + w, bottom } });
        x += w;
    }
    let tray_hidden = scene.tray.iter().skip(vis).map(|t| t.id).collect();
    Layout {
        width: x + pad_bar,
        height,
        items,
        separator_x,
        separator2_x,
        tray_area,
        tray_hidden,
        has_chevron: has_chev,
        mode,
    }
}

/// (Re)creates the `w` x `h` DIB (physical pixels, top-down, 32 bit). No-op if the size does not change.
pub fn resize(w: i32, h: i32) -> Result<(), String> {
    GFX.with(|c| {
        let mut b = c.borrow_mut();
        let Some(g) = b.as_mut() else { return Err("render not initialized".to_string()) };
        let txt_h = g.fonts.max_lh() + 4;
        g.gdi.resize(w, h, txt_h)
    })
}

/// Width of the background pill for the next `paint` calls (width animation): the pill is
/// centred in the DIB and everything outside it is clipped. 0 (or >= DIB width) = full.
pub fn set_pill_width(w: i32) {
    GFX.with(|c| {
        if let Some(g) = c.borrow_mut().as_mut() {
            g.pill_w = w.max(0);
        }
    });
}

/// Draws the scene into the DIB according to `layout` (which must be the one computed for the same
/// scene and with a size equal to that of `resize`).
pub fn paint(scene: &Scene<'_>, layout: &Layout) {
    GFX.with(|c| {
        let mut b = c.borrow_mut();
        let Some(g) = b.as_mut() else { return };
        paint_inner(g, scene, layout);
    });
}

/// Sends the DIB to the layered window (`UpdateLayeredWindow`, `ULW_ALPHA`, `AC_SRC_ALPHA`) with its
/// top-left corner at `(x, y)` (screen coords) and a constant opacity `alpha` (0..=255) on top of the
/// per-pixel alpha: the show/hide animation only re-presents the already painted DIB.
pub fn present_alpha(hwnd: HWND, x: i32, y: i32, alpha: u8) -> bool {
    let Some((dc_main, w, h)) = GFX.with(|c| {
        let b = c.borrow();
        let g = b.as_ref()?;
        let (w, h) = g.gdi.size()?;
        Some((g.gdi.dc_main(), w, h))
    }) else {
        return false;
    };
    // The `GFX` borrow is released: the call may re-enter (the DC stays valid until `destroy`).
    match crate::surface::present_layered(hwnd, dc_main, w, h, x, y, alpha) {
        Ok(()) => true,
        Err(e) => {
            log!("UpdateLayeredWindow failed: {e}");
            false
        }
    }
}

/// Truncates `name` to `NAME_MAX_CHARS` characters (19 + "…" if longer). Pure.
pub fn truncate_name(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() > NAME_MAX_CHARS {
        let mut s: String = chars[..NAME_MAX_CHARS - 1].iter().collect();
        s.push('\u{2026}');
        s
    } else {
        name.to_string()
    }
}

/// Releases the GDI resources. Idempotent.
pub fn destroy() {
    let Some(g) = GFX.with(|c| c.borrow_mut().take()) else { return };
    g.fonts.delete();
    // Dropping `g.gdi` frees the DIBs and the DCs.
}

// ------------------------------------------------------- background
// The pixel primitives (`div255`, `argb`, `over`, `rr_cov`, `Canvas`) are the ones of `surface`.

/// Bar background: `rgb` at alpha `bg_alpha` (premultiplied here; text and icons are composited over it
/// at their own full alpha) with a white RGBA(255,255,255,22) border of `bw` px, antialiased rounded
/// corners. Writes every pixel of `buf` (`w` x `h`): the pill is `pw` wide, starts at column `poff`,
/// and everything outside it is zeroed. Inside the pill the alpha never drops below 1: a layered
/// window lets the mouse through where alpha is 0, and the bar must keep its hover, clicks, drag
/// and drop and auto-hide even at 0 % opacity.
#[allow(clippy::too_many_arguments)]
fn draw_bg(buf: &mut [u32], w: i32, h: i32, pw: i32, poff: i32, radius: i32, bw: i32, rgb: (u32, u32, u32), bg_alpha: u32) {
    let px = |x: i32, y: i32| -> u32 {
        let outer = rr_cov(x, y, 0, 0, pw, h, radius);
        if outer <= 0.0 {
            return 0;
        }
        let inner = rr_cov(x, y, bw, bw, pw - bw, h - bw, radius - bw);
        let base = argb(((bg_alpha as f32 * outer + 0.5) as u32).max(1), rgb.0, rgb.1, rgb.2);
        let ring = (outer - inner).max(0.0);
        let ba = (BORDER_ALPHA as f32 * ring + 0.5) as u32;
        over(base, white(ba))
    };
    // Columns in [lo, hi) are outside the corner zones of both rects, so the pixel depends on y only.
    let r_out = radius.min(pw / 2).min(h / 2).max(0);
    let r_in = (radius - bw).min((pw - 2 * bw) / 2).min((h - 2 * bw) / 2).max(0);
    let lo = r_out.max(bw + r_in).clamp(0, pw);
    let hi = (pw - r_out).min(pw - bw - r_in).clamp(lo, pw);
    for y in 0..h {
        let row = &mut buf[(y * w) as usize..((y + 1) * w) as usize];
        row.fill(0);
        let pill = &mut row[poff as usize..(poff + pw) as usize];
        for x in (0..lo).chain(hi..pw) {
            pill[x as usize] = px(x, y);
        }
        if hi > lo {
            pill[lo as usize..hi as usize].fill(px(lo, y));
        }
    }
}

// ------------------------------------------------------- vector icons
// The shapes (bell, speaker, power, show desktop, chevron...) live in `glyphs`,
// shared with the popups.

// ------------------------------------------------------------------ text

/// Draws `s` in white with its top-left corner at `(x, y)` and alpha `alpha` into the canvas,
/// with font `t` (`Gdi::text`: grayscale GDI onto the scratch DIB, then manual compositing).
fn draw_text(g: &Gfx, cv: &mut Canvas<'_>, s: &str, x: i32, y: i32, t: Txt, alpha: u32) {
    g.gdi.text(cv, s, x, y, g.fonts.get(t), text_width(g, s, t), WHITE, alpha);
}

// ------------------------------------------------------------------ paint

/// Upward shift (physical px) of the labels to centre the cap height on the icon axis.
fn label_up(m: Metrics) -> i32 {
    if m.dpi >= 120 {
        1
    } else {
        0
    }
}

fn paint_inner(g: &Gfx, scene: &Scene<'_>, layout: &Layout) {
    let Some(mut cv) = g.gdi.canvas(None) else { return };
    let (w, h) = (cv.w, cv.h);
    if layout.width != w || layout.height != h {
        log!("render::paint: layout {}x{} differs from DIB {w}x{h}, skipping the frame", layout.width, layout.height);
        return;
    }
    let m = scene.metrics;
    let pw = if g.pill_w > 0 && g.pill_w < w { g.pill_w } else { w };
    let poff = (w - pw) / 2;
    // The pill background is painted straight into the DIB (no heap copy).
    let cfg = crate::config::get();
    draw_bg(cv.buf, w, h, pw, poff, m.px(BAR_RADIUS), m.px(1).max(1), cfg.background.rgb(), cfg.bar_alpha());

    // Hover/press plate.
    if let Some(p) = scene.plate {
        let pr = m.px(PLATE_RADIUS);
        cv.fill_rr(p.rect, pr, (255, 255, 255), p.alpha as u32);
        // "Glass" inner border: scales with the plate fade (capped at the hover value).
        let rim = PLATE_RIM_ALPHA * (p.alpha as u32).min(PLATE_REF_ALPHA) / PLATE_REF_ALPHA;
        cv.stroke_rr(p.rect, pr, m.px(1).max(1), rim);
    }
    // Separators (as tall, relative to the bar, as at the default height).
    let sh = m.px(SEP_HEIGHT) * h / m.px(BAR_HEIGHT).max(1);
    for sx in [layout.separator_x, layout.separator2_x] {
        if sx >= 0 {
            let sep = rect_wh(sx, (h - sh) / 2, m.px(1).max(1), sh);
            cv.fill_rr(sep, 0, (255, 255, 255), SEP_ALPHA);
        }
    }

    let sizes = scene.sizes;
    let icon_px = sizes.icon_px(m);
    let cy = axis(m, h, CONTENT_CY);
    let pad = if layout.mode == 2 { m.px(ITEM_PAD_TIGHT) } else { m.px(ITEM_PAD) };
    // Tray icons and chevron: own size, bar metrics. System modules: their own metrics `mm`.
    let tray_px = sizes.tray_px(m);
    let mm = sizes.module_metrics(m);
    let mcy = axis(mm, h, CONTENT_CY);
    let sys_pad = mm.px(TRAY_PAD);
    let sys_icon = mm.px(SYS_ICON);
    let lh_app = g.fonts.lh_app;
    let (lh, lh_small) = (g.fonts.lh, g.fonts.lh_small);
    let dm = crate::config::get().display_mode;
    let lift_id = scene.lift.map(|l| l.id);
    // Two passes: everything but the lifted item, then its plate and the item itself on top.
    for pass in 0..2u8 {
        if pass == 1 {
            if let Some(l) = scene.lift {
                if let Some(r) = layout.rect_of(l.id) {
                    let pr = m.px(PLATE_RADIUS);
                    let pl = plate_rect(r, m);
                    cv.fill_rr(pl, pr, (255, 255, 255), l.alpha as u32);
                    let rim = PLATE_RIM_ALPHA * (l.alpha as u32).min(PLATE_REF_ALPHA) / PLATE_REF_ALPHA;
                    cv.stroke_rr(pl, pr, m.px(1).max(1), rim);
                }
            }
        }
        for it in &layout.items {
            if (Some(it.id) == lift_id) != (pass == 1) {
                continue;
            }
            let r = it.rect;
            // Module glyph corner (tray items recompute it at the tray size).
            let gx = r.left + (r.right - r.left - sys_icon) / 2;
            let gy = mcy - sys_icon / 2;
            let hot = scene.hover == Some(it.id);
            let glyph_a = if hot { ICON_ALPHA_HOT } else { ICON_ALPHA };
            match it.id {
                ItemId::App(i) => {
                    let Some(app) = scene.apps.get(i) else { continue };
                    let ix = r.left + pad;
                    let iy = cy - icon_px / 2;
                    let opacity = match (app.dimmed, hot) {
                        (true, false) => DIM_OPACITY,
                        (true, true) => DIM_OPACITY_HOT,
                        _ => 255,
                    };
                    let text_alpha = match (app.dimmed, hot) {
                        (true, false) => DIM_TEXT_ALPHA,
                        (true, true) => DIM_TEXT_ALPHA_HOT,
                        (false, false) => TEXT_ALPHA,
                        (false, true) => TEXT_ALPHA_HOT,
                    };
                    let name = if layout.mode == 0 && dm != DisplayMode::Icon {
                        truncate_name(app.name)
                    } else {
                        String::new()
                    };
                    let (has_icon, has_text) = app_parts(dm, layout.mode, !name.is_empty());
                    let name_gap = m.px(NAME_GAP);
                    let text_w = if has_text { text_width(g, &name, Txt::App) } else { 0 };
                    if has_icon {
                        match app.icon {
                            Some(b) => cv.blit(b, ix, iy, icon_px, opacity),
                            None => {
                                let ph = rect_wh(ix, iy, icon_px, icon_px);
                                cv.fill_rr(ph, m.px(PLACEHOLDER_RADIUS), (150, 150, 150), div255(110 * opacity));
                            }
                        }
                    }
                    if has_text {
                        let nx = if has_icon { ix + icon_px + name_gap } else { ix };
                        draw_text(g, &mut cv, &name, nx, cy - lh_app / 2 - label_up(m), Txt::App, text_alpha);
                    }
                    // State bar: spans the whole visible content (icon, name or both). With the bar at the
                    // top it sits inside the pill under the icon (the bar floats EDGE_MARGIN from the screen).
                    let ih = m.px(INDICATOR_H).max(1);
                    let ty = if scene.bar_top {
                        // Below the lower of icon and name (both centred on `cy`).
                        let icon_half = if has_icon { icon_px - icon_px / 2 } else { 0 };
                        let tb = m.px(sizes.text_box());
                        let text_half = if has_text { tb - tb / 2 } else { 0 };
                        cy + icon_half.max(text_half) + m.px(INDICATOR_BELOW_ICON)
                    } else {
                        h - m.px(INDICATOR_EDGE) - ih
                    };
                    let cw = content_width(has_icon, has_text, icon_px, name_gap, text_w);
                    let t = app.active_t.clamp(0.0, 1.0);
                    let (alpha, rgb, frac) = match app.indicator {
                        Indicator::None => (0, (255, 255, 255), 0.0),
                        Indicator::Open | Indicator::Active => (
                            OPEN_ALPHA + ((255 - OPEN_ALPHA) as f32 * t + 0.5) as u32,
                            (255, 255, 255),
                            INDICATOR_OPEN_FRAC + (INDICATOR_ACTIVE_FRAC - INDICATOR_OPEN_FRAC) * t,
                        ),
                        Indicator::Attention => (255, ATTENTION_RGB, INDICATOR_ACTIVE_FRAC),
                    };
                    // Centred under the content; never narrower than a dot.
                    let iw = ((cw as f32 * frac + 0.5) as i32).max(ih).min(cw);
                    cv.fill_rr(rect_wh(ix + (cw - iw) / 2, ty, iw, ih), ih / 2, rgb, alpha);
                }
                ItemId::Tray(id) => {
                    if let Some(t) = scene.tray.iter().find(|t| t.id == id) {
                        let tx = r.left + (r.right - r.left - tray_px) / 2;
                        cv.blit(t.icon, tx, cy - tray_px / 2, tray_px, 255);
                    }
                }
                ItemId::TrayOverflow => {
                    let tx = r.left + (r.right - r.left - tray_px) / 2;
                    let shape: &dyn Fn(f32, f32) -> bool =
                        if scene.bar_top { &glyphs::chevron_down_shape } else { &glyphs::chevron_up_shape };
                    cv.glyph(tx, cy - tray_px / 2, tray_px, WHITE, glyph_a, shape);
                }
                ItemId::Notifications => {
                    let badge = scene.system.bell_badge;
                    if scene.system.bell_dnd {
                        cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &|u, v| glyphs::bell_off_shape(u, v, badge));
                    } else if badge {
                        cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &glyphs::bell_badge_shape);
                    } else {
                        cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &glyphs::bell_shape);
                    }
                    if badge {
                        // Badge always solid white, in the gap cut out around it in the bell.
                        cv.glyph(gx, gy, sys_icon, WHITE, 255, &glyphs::badge_shape);
                    }
                }
                ItemId::Volume => {
                    let vol = scene.system.volume;
                    let x = r.left + sys_pad;
                    let muted = vol.muted || !vol.available;
                    let waves = glyphs::waves_for(vol.level);
                    // Headphone glyph while the default device is headphones/headset (muted keeps the speaker).
                    let phones = !muted && crate::audio::default_is_headphone();
                    cv.glyph(x, gy, sys_icon, WHITE, glyph_a, &|u, v| {
                        if phones {
                            glyphs::device_shape(u, v, crate::audio::DeviceKind::Headphones)
                        } else {
                            glyphs::speaker_shape(u, v, waves, muted)
                        }
                    });
                    if scene.system.show_volume_percent && vol.available {
                        let s = format!("{}%", vol.level);
                        let a = if hot { TEXT2_ALPHA_HOT } else { TEXT2_ALPHA };
                        draw_text(
                            g,
                            &mut cv,
                            &s,
                            x + sys_icon + mm.px(SYS_TEXT_GAP),
                            mcy - lh / 2 - label_up(mm),
                            Txt::Module,
                            a,
                        );
                    }
                }
                ItemId::Clock => {
                    let sys = &scene.system;
                    let tw = text_width(g, sys.time, Txt::Module);
                    let cxm = (r.left + r.right) / 2;
                    match sys.date {
                        Some(d) if !d.is_empty() => {
                            let dw = text_width(g, d, Txt::Small);
                            let total = lh + lh_small - mm.px(2);
                            let y0 = axis(mm, h, CLOCK_CY) - total / 2 - mm.px(CLOCK_UP_HALVES) / 2;
                            let (ta, da) =
                                if hot { (TEXT_ALPHA_HOT, TEXT2_ALPHA_HOT) } else { (TEXT_ALPHA, TEXT2_ALPHA) };
                            draw_text(g, &mut cv, sys.time, cxm - tw / 2, y0, Txt::Module, ta);
                            draw_text(g, &mut cv, d, cxm - dw / 2, y0 + lh - mm.px(2), Txt::Small, da);
                        }
                        _ => {
                            let ta = if hot { TEXT_ALPHA_HOT } else { TEXT_ALPHA };
                            draw_text(g, &mut cv, sys.time, cxm - tw / 2, mcy - lh / 2, Txt::Module, ta)
                        }
                    }
                }
                ItemId::ShowDesktop => cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &glyphs::show_desktop_shape),
                ItemId::Start => cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &glyphs::windows_logo_shape),
                ItemId::Power => cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &glyphs::power_shape),
                ItemId::Network => {
                    let net = scene.system.net;
                    if net.kind == NetKind::Ethernet {
                        cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &glyphs::ethernet_shape);
                    } else {
                        // Wi-Fi: every arc dim, the lit ones on top (offline: all dim).
                        cv.glyph(gx, gy, sys_icon, WHITE, glyph_a * NET_DIM_ALPHA / 255, &|u, v| {
                            glyphs::wifi_shape(u, v, 4)
                        });
                        if net.kind == NetKind::Wifi {
                            cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &|u, v| glyphs::wifi_shape(u, v, net.bars));
                        }
                    }
                    // Corner badge: small globe = internet reachable, small x = no internet / offline.
                    let online = net.kind != NetKind::None && net.internet;
                    cv.glyph(gx, gy, sys_icon, WHITE, glyph_a, &|u, v| glyphs::net_badge_shape(u, v, online));
                }
                ItemId::Metrics => {
                    let u = scene.system.usage;
                    let step = lh_small - mm.px(2);
                    let y0 = axis(mm, h, CLOCK_CY) - (step + lh_small) / 2 - mm.px(CLOCK_UP_HALVES) / 2;
                    let pad = mm.px(CLOCK_PAD);
                    let (la, va) = if hot { (TEXT2_ALPHA, TEXT_ALPHA_HOT) } else { (METRIC_LABEL_ALPHA, TEXT_ALPHA) };
                    for (k, val) in [u.cpu, u.ram].into_iter().enumerate() {
                        let y = y0 + k as i32 * step;
                        draw_text(g, &mut cv, METRIC_LABELS[k], r.left + pad, y, Txt::Small, la);
                        // Values right-aligned on the item padding.
                        let s = format!("{val}%");
                        let vw = text_width(g, &s, Txt::Small);
                        draw_text(g, &mut cv, &s, r.right - pad - vw, y, Txt::Small, va);
                    }
                }
            }
        }
    }
    // Clip to the edges of the animated pill: items outside the background are not visible.
    if pw < w {
        for row in 0..h as usize {
            let base = row * w as usize;
            cv.buf[base..base + poff as usize].fill(0);
            cv.buf[base + (poff + pw) as usize..base + w as usize].fill(0);
        }
    }
}

// PREVIEW-HOOK (used by `preview.rs`): copy of the main DIB's pixels (premultiplied 0xAARRGGBB).
pub(crate) fn snapshot_pixels() -> Option<(i32, i32, Vec<u32>)> {
    GFX.with(|c| c.borrow().as_ref()?.gdi.snapshot_pixels())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_bar_keeps_the_mouse() {
        // 0 % opacity: every pixel of the pill keeps alpha >= 1 (a layered window is click-through
        // where alpha is 0), everything outside it stays fully transparent.
        let (w, h, pw, poff) = (60, 20, 50, 5);
        let mut buf = vec![0u32; (w * h) as usize];
        draw_bg(&mut buf, w, h, pw, poff, 6, 1, (0, 0, 0), 0);
        for y in 0..h {
            for x in 0..w {
                let a = buf[(y * w + x) as usize] >> 24;
                let inside = rr_cov(x - poff, y, 0, 0, pw, h, 6) > 0.0;
                assert_eq!(a > 0, inside, "pixel ({x}, {y})");
            }
        }
    }

    #[test]
    fn default_sizes_keep_the_original_bar() {
        let s = BarSizes::default();
        for dpi in [96, 120, 144, 168, 192] {
            let m = Metrics { dpi };
            assert_eq!(bar_height(m, s), m.px(BAR_HEIGHT));
            assert_eq!(s.module_metrics(m), m);
            assert_eq!(s.icon_px(m), m.px(ICON_SIZE));
            assert_eq!(s.tray_px(m), m.px(SYS_ICON));
            assert_eq!(axis(m, bar_height(m, s), CONTENT_CY), m.px(CONTENT_CY));
        }
    }

    #[test]
    fn start_button_is_the_first_item_only_when_on() {
        let m = Metrics { dpi: 96 };
        let sizes = BarSizes::default();
        render_init_for_test(m, sizes);
        let modules = [ItemId::Notifications, ItemId::Power];
        let scene = |start_button| Scene {
            metrics: m,
            sizes,
            apps: &[],
            tray: &[],
            overflow_enabled: false,
            force_chevron: false,
            bar_top: false,
            system: SystemView {
                volume: VolumeState { level: 0, muted: false, available: false },
                show_volume_percent: false,
                time: "00:00",
                date: None,
                bell_badge: false,
                bell_dnd: false,
                net: NetStatus { kind: NetKind::Ethernet, bars: 0, internet: false },
                usage: Usage { cpu: 0, ram: 0 },
            },
            plate: None,
            modules: &modules,
            start_button,
            lift: None,
            hover: None,
            max_width: 0,
        };
        let on = layout(&scene(true));
        let off = layout(&scene(false));
        assert_eq!(on.items[0].id, ItemId::Start);
        assert_eq!(on.items.len(), off.items.len() + 1);
        assert!(!off.items.iter().any(|i| i.id == ItemId::Start));
        // Same width as the other glyph buttons, and everything else moves right by one slot.
        let start = on.items[0].rect;
        let bell = on.rect_of(ItemId::Notifications).unwrap();
        assert_eq!(start.right - start.left, bell.right - bell.left);
        assert!(on.width > off.width);
        assert_eq!(on.hit_test(start.left + 1, 1), Some(ItemId::Start));
        assert_eq!(off.hit_test(start.left + 1, 1), Some(ItemId::Notifications));
        // With no apps the Start button still gets its separator; without it there is none.
        assert!(on.separator_x > start.right);
        assert_eq!(off.separator_x, -1);
    }

    /// Initialises the GDI state `layout` measures with (memory DCs only, no window).
    fn render_init_for_test(m: Metrics, sizes: BarSizes) {
        init(m, sizes).expect("render init");
    }

    #[test]
    fn bar_grows_with_the_tallest_content() {
        let m = Metrics { dpi: 96 };
        let big = BarSizes { icon: 32, ..BarSizes::default() };
        assert_eq!(bar_height(m, big), 52);
        let modules = BarSizes { module_pct: 150, ..BarSizes::default() };
        assert_eq!(bar_height(m, modules), 54);
        // Text only counts when names can be shown; the tray only when it is on.
        let text = BarSizes { text: 18, ..BarSizes::default() };
        assert!(bar_height(m, text) > 36);
        assert_eq!(bar_height(m, BarSizes { text_on: false, ..text }), 36);
        let tray = BarSizes { tray: 24, ..BarSizes::default() };
        assert_eq!(bar_height(m, tray), 44);
        assert_eq!(bar_height(m, BarSizes { tray_on: false, ..tray }), 36);
        // Smaller content gives a slimmer bar.
        let small = BarSizes { icon: 14, text: 10, tray: 12, module_pct: 75, ..BarSizes::default() };
        assert_eq!(bar_height(m, small), 34);
    }
}
