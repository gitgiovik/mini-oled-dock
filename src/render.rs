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
//! THREADING: UI thread only (thread-local GDI state). DIBs are recreated by `resize`.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, DeleteDC, DeleteObject, DrawTextW, GdiFlush, GetDC,
    GetTextExtentPoint32W, GetTextFaceW, GetTextMetricsW, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_LEFT, DT_NOCLIP, DT_NOPREFIX, DT_SINGLELINE, DT_TOP, HBITMAP, HDC, HFONT,
    HGDIOBJ, LOGFONTW, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

use crate::audio::VolumeState;
use crate::config::DisplayMode;
use crate::glyphs;
use crate::icons::IconBitmap;
use crate::log;
use crate::metrics::Usage;
use crate::net::{NetKind, NetStatus};

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

/// 32-bit top-down DIB selected into a DC.
struct Dib {
    bmp: HBITMAP,
    /// Bitmap that was selected in the DC before ours (to restore before deleting the DIB).
    old: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
}

struct Gfx {
    metrics: Metrics,
    /// DC holding the main DIB (source of `UpdateLayeredWindow`).
    dc_main: HDC,
    /// DC holding the scratch DIB for text, with the font selected.
    dc_txt: HDC,
    sizes: BarSizes,
    fonts: Fonts,
    main: Option<Dib>,
    txt: Option<Dib>,
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
            // SAFETY: fonts created by `build_fonts`; they are only selected temporarily, never at deletion.
            let _ = unsafe { DeleteObject(HGDIOBJ(f.0)) };
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

/// Pixels of a DIB as a slice.
/// Invariant: the bitmap (w*h*4 bytes) is alive as long as the `Dib` exists; the slice must be used in short
/// scopes and must not coexist with GDI calls that write to the same DIB (here GDI only writes
/// to the text scratch DIB, never to the main one).
fn dib_pixels<'a>(d: &Dib) -> &'a mut [u32] {
    // SAFETY: see invariant above; `bits` points to w*h u32 allocated by CreateDIBSection.
    unsafe { std::slice::from_raw_parts_mut(d.bits, (d.w as usize) * (d.h as usize)) }
}

fn make_dib(dc: HDC, w: i32, h: i32) -> Result<Dib, String> {
    if w <= 0 || h <= 0 {
        return Err(format!("invalid DIB dimensions {w}x{h}"));
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `bmi` is valid; `bits` receives the pointer to the buffer owned by the bitmap.
    let bmp = unsafe { CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(|e| format!("CreateDIBSection {w}x{h}: {e}"))?;
    if bits.is_null() {
        // SAFETY: `bmp` was just created and is not selected anywhere.
        let _ = unsafe { DeleteObject(HGDIOBJ(bmp.0)) };
        return Err("CreateDIBSection: null bits".into());
    }
    // SAFETY: `dc` and `bmp` are valid; the previous bitmap is saved for restoring.
    let old = unsafe { SelectObject(dc, HGDIOBJ(bmp.0)) };
    Ok(Dib { bmp, old, bits: bits.cast::<u32>(), w, h })
}

fn free_dib(dc: HDC, d: Dib) {
    // SAFETY: `d` was created by `make_dib` with this same `dc`; the original bitmap is restored
    // before deleting ours (a selected bitmap cannot be deleted).
    unsafe {
        SelectObject(dc, d.old);
        let _ = DeleteObject(HGDIOBJ(d.bmp.0));
    }
}

fn make_font(face: &str, height_px: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -height_px,
        lfWeight: 400,
        lfCharSet: DEFAULT_CHARSET,
        lfQuality: ANTIALIASED_QUALITY, // grayscale, no ClearType
        ..Default::default()
    };
    for (d, s) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *d = s;
    }
    // SAFETY: `lf` is fully initialized.
    unsafe { CreateFontIndirectW(&lf) }
}

/// Creates the `height_px` font trying "Segoe UI Variable Text" and then "Segoe UI". GDI silently
/// substitutes a missing font, so the actual name is checked with `GetTextFaceW`.
fn create_font(dc: HDC, height_px: i32) -> Result<HFONT, String> {
    for face in ["Segoe UI Variable Text", "Segoe UI"] {
        let f = make_font(face, height_px);
        if f.is_invalid() {
            continue;
        }
        let mut buf = [0u16; 64];
        // SAFETY: `dc` is valid; the font is selected only for the check and then restored.
        let got = unsafe {
            let old = SelectObject(dc, HGDIOBJ(f.0));
            let n = GetTextFaceW(dc, Some(&mut buf));
            SelectObject(dc, old);
            n
        };
        let name = String::from_utf16_lossy(&buf[..(got.max(0) as usize).min(buf.len())]);
        if name.trim_end_matches('\0').eq_ignore_ascii_case(face) {
            return Ok(f);
        }
        // SAFETY: `f` was created above and is not selected.
        let _ = unsafe { DeleteObject(HGDIOBJ(f.0)) };
    }
    let f = make_font("Segoe UI", height_px);
    if f.is_invalid() {
        Err("CreateFontIndirectW failed".into())
    } else {
        Ok(f)
    }
}

fn line_height(dc: HDC, font: HFONT) -> i32 {
    let mut tm = TEXTMETRICW::default();
    // SAFETY: `dc` is valid, `tm` is writable; the font is restored immediately.
    let ok = unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let ok = GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
        ok
    };
    if ok.as_bool() && tm.tmHeight > 0 {
        tm.tmHeight
    } else {
        font_fallback_height()
    }
}

fn font_fallback_height() -> i32 {
    17
}

// ------------------------------------------------------------------ API

/// Creates the GDI resources (memory DCs, Segoe UI Variable Text fonts -> Segoe UI fallback, see
/// `Fonts`) for the given `metrics` and `sizes`. The DIB is born with the first `resize`.
pub fn init(metrics: Metrics, sizes: BarSizes) -> Result<(), String> {
    destroy();
    // SAFETY: the screen DC is taken and released here; the memory DCs are ours.
    let (dc_main, dc_txt) = unsafe {
        let screen = GetDC(None);
        let a = CreateCompatibleDC(Some(screen));
        let b = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        (a, b)
    };
    if dc_main.is_invalid() || dc_txt.is_invalid() {
        // SAFETY: any valid DCs created above, not yet used.
        unsafe {
            if !dc_main.is_invalid() {
                let _ = DeleteDC(dc_main);
            }
            if !dc_txt.is_invalid() {
                let _ = DeleteDC(dc_txt);
            }
        }
        return Err("CreateCompatibleDC failed".into());
    }
    // SAFETY: `dc_txt` is valid. White text, transparent background: the coverage ends up in the G channel.
    unsafe {
        SetBkMode(dc_txt, TRANSPARENT);
        SetTextColor(dc_txt, COLORREF(0x00FF_FFFF));
    }
    let fonts = match build_fonts(dc_txt, metrics, sizes) {
        Ok(v) => v,
        Err(e) => {
            // SAFETY: DCs created above, with no selected objects.
            unsafe {
                let _ = DeleteDC(dc_main);
                let _ = DeleteDC(dc_txt);
            }
            return Err(e);
        }
    };
    GFX.with(|c| {
        *c.borrow_mut() =
            Some(Gfx { metrics, dc_main, dc_txt, sizes, fonts, main: None, txt: None, pill_w: 0 });
    });
    log!("render::init: dpi={} {sizes:?}", metrics.dpi);
    Ok(())
}

fn build_fonts(dc: HDC, m: Metrics, s: BarSizes) -> Result<Fonts, String> {
    let mm = s.module_metrics(m);
    let mut made: Vec<HFONT> = Vec::with_capacity(3);
    for px in [m.px(s.text), mm.px(13), mm.px(11)] {
        match create_font(dc, px.max(1)) {
            Ok(f) => made.push(f),
            Err(e) => {
                for f in made {
                    // SAFETY: created above, never selected.
                    let _ = unsafe { DeleteObject(HGDIOBJ(f.0)) };
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
        let fonts = build_fonts(g.dc_txt, metrics, sizes)?;
        // The old fonts are not selected in the DCs (they are only selected temporarily).
        g.fonts.delete();
        g.fonts = fonts;
        g.metrics = metrics;
        g.sizes = sizes;
        // The DIBs depend on the DPI (height of the text DIB): they are recreated by the next `resize`.
        if let Some(d) = g.main.take() {
            free_dib(g.dc_main, d);
        }
        if let Some(d) = g.txt.take() {
            free_dib(g.dc_txt, d);
        }
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

fn text_width(g: &Gfx, s: &str, t: Txt) -> i32 {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let font = g.fonts.get(t).0;
    let mut sz = SIZE::default();
    // SAFETY: `dc_txt` is valid; `wide` and `sz` outlive the call; the font is restored.
    let ok = unsafe {
        let old = SelectObject(g.dc_txt, HGDIOBJ(font.0));
        let ok = GetTextExtentPoint32W(g.dc_txt, &wide, &mut sz);
        SelectObject(g.dc_txt, old);
        ok
    };
    if ok.as_bool() {
        sz.cx
    } else {
        wide.len() as i32 * 7
    }
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
        if let Some(d) = &g.main {
            if d.w == w && d.h == h {
                return Ok(());
            }
        }
        if let Some(d) = g.main.take() {
            free_dib(g.dc_main, d);
        }
        if let Some(d) = g.txt.take() {
            free_dib(g.dc_txt, d);
        }
        let main = make_dib(g.dc_main, w, h)?;
        let txt_h = g.fonts.max_lh() + 4;
        let txt = match make_dib(g.dc_txt, w, txt_h) {
            Ok(t) => t,
            Err(e) => {
                free_dib(g.dc_main, main);
                return Err(e);
            }
        };
        g.main = Some(main);
        g.txt = Some(txt);
        Ok(())
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
        let d = g.main.as_ref()?;
        Some((g.dc_main, d.w, d.h))
    }) else {
        return false;
    };
    {
        let pos = POINT { x, y };
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: all pointers point to local variables alive for the call; `dc_main` has
        // the DIB selected (a re-entry that recreates it still leaves a valid DC, which
        // lives until `destroy` frees it); the screen DC is taken and released here.
        let r = unsafe {
            let screen = GetDC(None);
            let r = UpdateLayeredWindow(
                hwnd,
                Some(screen),
                Some(&pos),
                Some(&size),
                Some(dc_main),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            ReleaseDC(None, screen);
            r
        };
        match r {
            Ok(()) => true,
            Err(e) => {
                log!("UpdateLayeredWindow failed: {e}");
                false
            }
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
    let Some(mut g) = GFX.with(|c| c.borrow_mut().take()) else { return };
    if let Some(d) = g.main.take() {
        free_dib(g.dc_main, d);
    }
    if let Some(d) = g.txt.take() {
        free_dib(g.dc_txt, d);
    }
    g.fonts.delete();
    // SAFETY: DCs created in `init`; no object of ours is selected in them any more.
    unsafe {
        let _ = DeleteDC(g.dc_main);
        let _ = DeleteDC(g.dc_txt);
    }
}

// ------------------------------------------------------- pixel primitives

#[inline]
fn div255(x: u32) -> u32 {
    (x + 128 + ((x + 128) >> 8)) >> 8
}

/// Premultiplied 0xAARRGGBB pixel from a non-premultiplied colour and alpha.
#[inline]
fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
    (a << 24) | (div255(r * a) << 16) | (div255(g * a) << 8) | div255(b * a)
}

/// White with alpha `a`, premultiplied.
#[inline]
fn white(a: u32) -> u32 {
    argb(a, 255, 255, 255)
}

/// "Source over" compositing of `src` onto `dst`, both premultiplied.
#[inline]
fn over(dst: u32, src: u32) -> u32 {
    let sa = src >> 24;
    if sa == 255 {
        return src;
    }
    if sa == 0 {
        return dst;
    }
    let inv = 255 - sa;
    let ch = |s: u32, d: u32| (s + div255(d * inv)).min(255);
    (ch(src >> 24, dst >> 24) << 24)
        | (ch((src >> 16) & 255, (dst >> 16) & 255) << 16)
        | (ch((src >> 8) & 255, (dst >> 8) & 255) << 8)
        | ch(src & 255, dst & 255)
}

/// Coverage (0..1) of pixel `(x, y)` with respect to the rounded rectangle `[l, r) x [t, b)` with
/// radius `rad`: manual antialiasing at the corners only (the sides are pixel-aligned).
fn rr_cov(x: i32, y: i32, l: i32, t: i32, r: i32, b: i32, rad: i32) -> f32 {
    if x < l || x >= r || y < t || y >= b {
        return 0.0;
    }
    let rad = rad.min((r - l) / 2).min((b - t) / 2).max(0);
    let cx = if x < l + rad {
        l + rad
    } else if x >= r - rad {
        r - rad
    } else {
        return 1.0;
    };
    let cy = if y < t + rad {
        t + rad
    } else if y >= b - rad {
        b - rad
    } else {
        return 1.0;
    };
    let dx = (x as f32 + 0.5) - cx as f32;
    let dy = (y as f32 + 0.5) - cy as f32;
    (rad as f32 + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
}

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

/// Drawing surface: slice of the main DIB with its size.
struct Canvas<'a> {
    buf: &'a mut [u32],
    w: i32,
    h: i32,
}

impl Canvas<'_> {
    #[inline]
    fn blend(&mut self, x: i32, y: i32, src: u32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = (y * self.w + x) as usize;
        self.buf[i] = over(self.buf[i], src);
    }

    /// Filled rounded rectangle with antialiasing, colour `(r,g,b)` and alpha `alpha`.
    fn fill_rr(&mut self, rc: RECT, radius: i32, rgb: (u32, u32, u32), alpha: u32) {
        if alpha == 0 {
            return;
        }
        for y in rc.top.max(0)..rc.bottom.min(self.h) {
            for x in rc.left.max(0)..rc.right.min(self.w) {
                let cov = rr_cov(x, y, rc.left, rc.top, rc.right, rc.bottom, radius);
                let a = (alpha as f32 * cov + 0.5) as u32;
                if a > 0 {
                    self.blend(x, y, argb(a, rgb.0, rgb.1, rgb.2));
                }
            }
        }
    }

    /// 1 px (`bw`) inner border of a rounded rectangle: white ring with alpha `alpha`.
    fn stroke_rr(&mut self, rc: RECT, radius: i32, bw: i32, alpha: u32) {
        if alpha == 0 {
            return;
        }
        for y in rc.top.max(0)..rc.bottom.min(self.h) {
            for x in rc.left.max(0)..rc.right.min(self.w) {
                let outer = rr_cov(x, y, rc.left, rc.top, rc.right, rc.bottom, radius);
                let inner = rr_cov(x, y, rc.left + bw, rc.top + bw, rc.right - bw, rc.bottom - bw, radius - bw);
                let a = (alpha as f32 * (outer - inner).max(0.0) + 0.5) as u32;
                if a > 0 {
                    self.blend(x, y, white(a));
                }
            }
        }
    }

    /// Blends a premultiplied bitmap into the `size` x `size` square with corner `(x, y)`.
    /// If the bitmap has a different size it is scaled (averaged when shrinking, else nearest-neighbour; transitional case: the icon at the
    /// right size is arriving from the shell thread).
    fn blit(&mut self, bmp: &IconBitmap, x: i32, y: i32, size: i32, opacity: u32) {
        let (sw, sh) = (bmp.width as i32, bmp.height as i32);
        if bmp.is_empty() || size <= 0 || bmp.pixels.len() < (sw as usize) * (sh as usize) {
            return;
        }
        let same = sw == size && sh == size;
        for dy in 0..size {
            let yy = y + dy;
            if yy < 0 || yy >= self.h {
                continue;
            }
            let sy = if same { dy } else { dy * sh / size };
            for dx in 0..size {
                let xx = x + dx;
                if xx < 0 || xx >= self.w {
                    continue;
                }
                let sx = if same { dx } else { dx * sw / size };
                let mut s = if same || (sw <= size && sh <= size) {
                    bmp.pixels[(sy * sw + sx) as usize]
                } else {
                    // Shrinking (e.g. tray icons kept at 16 px for the popup): average, don't skip.
                    crate::surface::area_sample(bmp, dx, dy, size)
                };
                if s >> 24 == 0 {
                    continue;
                }
                if opacity < 255 {
                    s = (div255((s >> 24) * opacity) << 24)
                        | (div255(((s >> 16) & 255) * opacity) << 16)
                        | (div255(((s >> 8) & 255) * opacity) << 8)
                        | div255((s & 255) * opacity);
                }
                self.blend(xx, yy, s);
            }
        }
    }

    /// Draws in white a shape defined in 16x16 space by the function `inside`, in a
    /// square of `size` pixels with corner `(x, y)`, with 4x4 supersampling.
    fn vector(&mut self, x: i32, y: i32, size: i32, alpha: u32, inside: &dyn Fn(f32, f32) -> bool) {
        if size <= 0 {
            return;
        }
        glyphs::set_scale(size);
        let k = 16.0 / size as f32;
        for py in 0..size {
            for px in 0..size {
                let mut hits = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let u = (px as f32 + (sx as f32 + 0.5) / 4.0) * k;
                        let v = (py as f32 + (sy as f32 + 0.5) / 4.0) * k;
                        if inside(u, v) {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    self.blend(x + px, y + py, white(alpha * hits / 16));
                }
            }
        }
    }
}

// ------------------------------------------------------- vector icons
// The shapes (bell, speaker, power, show desktop, chevron...) live in `glyphs`,
// shared with the popups.

// ------------------------------------------------------------------ text

/// Draws `s` with its top-left corner at `(x, y)`: white grayscale GDI onto the black
/// scratch DIB, then manual compositing with alpha `alpha` into the canvas.
fn draw_text(g: &Gfx, cv: &mut Canvas<'_>, s: &str, x: i32, y: i32, t: Txt, alpha: u32) {
    let Some(txt) = g.txt.as_ref() else { return };
    if s.is_empty() {
        return;
    }
    let (font, lh) = g.fonts.get(t);
    let w = text_width(g, s, t).min(txt.w);
    let h = lh.min(txt.h);
    if w <= 0 || h <= 0 {
        return;
    }
    let tbuf = dib_pixels(txt);
    for row in 0..h as usize {
        let start = row * txt.w as usize;
        tbuf[start..start + w as usize].fill(0);
    }
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    let mut rc = RECT { left: 0, top: 0, right: w, bottom: h };
    // SAFETY: `dc_txt` has the scratch DIB selected; `wide` and `rc` outlive the call;
    // the font is restored immediately. GdiFlush guarantees GDI has written before reading.
    unsafe {
        let old = SelectObject(g.dc_txt, HGDIOBJ(font.0));
        DrawTextW(g.dc_txt, &mut wide, &mut rc, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP);
        SelectObject(g.dc_txt, old);
        let _ = GdiFlush();
    }
    for row in 0..h {
        for col in 0..w {
            let cov = (tbuf[(row * txt.w + col) as usize] >> 8) & 0xFF; // coverage = G channel
            if cov == 0 {
                continue;
            }
            cv.blend(x + col, y + row, white(div255(cov * alpha)));
        }
    }
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

fn rect_wh(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

fn paint_inner(g: &mut Gfx, scene: &Scene<'_>, layout: &Layout) {
    let (w, h) = match &g.main {
        Some(d) => (d.w, d.h),
        None => return,
    };
    if layout.width != w || layout.height != h {
        log!("render::paint: layout {}x{} differs from DIB {w}x{h}, skipping the frame", layout.width, layout.height);
        return;
    }
    let m = scene.metrics;
    let pw = if g.pill_w > 0 && g.pill_w < w { g.pill_w } else { w };
    let poff = (w - pw) / 2;
    let Some(main) = g.main.as_ref() else { return };
    let buf = dib_pixels(main);
    // The pill background is painted straight into the DIB (no heap copy).
    let cfg = crate::config::get();
    draw_bg(buf, w, h, pw, poff, m.px(BAR_RADIUS), m.px(1).max(1), cfg.background.rgb(), cfg.bar_alpha());
    let g: &Gfx = g;
    let mut cv = Canvas { buf, w, h };

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
                    cv.vector(tx, cy - tray_px / 2, tray_px, glyph_a, shape);
                }
                ItemId::Notifications => {
                    let badge = scene.system.bell_badge;
                    if scene.system.bell_dnd {
                        cv.vector(gx, gy, sys_icon, glyph_a, &|u, v| glyphs::bell_off_shape(u, v, badge));
                    } else if badge {
                        cv.vector(gx, gy, sys_icon, glyph_a, &glyphs::bell_badge_shape);
                    } else {
                        cv.vector(gx, gy, sys_icon, glyph_a, &glyphs::bell_shape);
                    }
                    if badge {
                        // Badge always solid white, in the gap cut out around it in the bell.
                        cv.vector(gx, gy, sys_icon, 255, &glyphs::badge_shape);
                    }
                }
                ItemId::Volume => {
                    let vol = scene.system.volume;
                    let x = r.left + sys_pad;
                    let muted = vol.muted || !vol.available;
                    let waves = glyphs::waves_for(vol.level);
                    // Headphone glyph while the default device is headphones/headset (muted keeps the speaker).
                    let phones = !muted && crate::audio::default_is_headphone();
                    cv.vector(x, gy, sys_icon, glyph_a, &|u, v| {
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
                ItemId::ShowDesktop => cv.vector(gx, gy, sys_icon, glyph_a, &glyphs::show_desktop_shape),
                ItemId::Start => cv.vector(gx, gy, sys_icon, glyph_a, &glyphs::windows_logo_shape),
                ItemId::Power => cv.vector(gx, gy, sys_icon, glyph_a, &glyphs::power_shape),
                ItemId::Network => {
                    let net = scene.system.net;
                    if net.kind == NetKind::Ethernet {
                        cv.vector(gx, gy, sys_icon, glyph_a, &glyphs::ethernet_shape);
                    } else {
                        // Wi-Fi: every arc dim, the lit ones on top (offline: all dim).
                        cv.vector(gx, gy, sys_icon, glyph_a * NET_DIM_ALPHA / 255, &|u, v| glyphs::wifi_shape(u, v, 4));
                        if net.kind == NetKind::Wifi {
                            cv.vector(gx, gy, sys_icon, glyph_a, &|u, v| glyphs::wifi_shape(u, v, net.bars));
                        }
                    }
                    // Corner badge: small globe = internet reachable, small x = no internet / offline.
                    let online = net.kind != NetKind::None && net.internet;
                    cv.vector(gx, gy, sys_icon, glyph_a, &|u, v| glyphs::net_badge_shape(u, v, online));
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
    GFX.with(|c| {
        let b = c.borrow();
        let d = b.as_ref()?.main.as_ref()?;
        Some((d.w, d.h, dib_pixels(d).to_vec()))
    })
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
