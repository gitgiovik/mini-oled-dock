//! START MENU (Win double tap): the dock's own Start menu, drawn like the other popups (OLED panel,
//! same rows and plates), so that it opens on ANY display: the native Start menu only opens on the
//! display of the primary taskbar and offers no API to choose. `ui::open_start_menu` opens it anchored
//! to the bar's Start button on the display under the pointer.
//!
//! CONTENT, top to bottom:
//!  * search box: typing filters the list (the popup has the focus: `WM_CHAR` / `WM_KEYDOWN` reach
//!    `Content::char` / `Content::key`); Enter launches the selected result (the first one by default),
//!    Up / Down / PgUp / PgDn / Home / End move the selection, Backspace deletes, Esc clears the text
//!    (and closes the menu when it is already empty);
//!  * pinned apps (only while the search is empty): a grid of up to `COLS * MAX_PIN_ROWS` tiles from
//!    `config.start_pins`, a list SEPARATE from the bar pins;
//!  * all apps: every entry of `shell:AppsFolder` (classic programs AND Store apps, the catalog of the
//!    native Start menu), alphabetical, scrolled with the wheel or the keyboard.
//!
//! Left click launches the app (`shell:AppsFolder\<id>` through `ShellRequest::Launch`, so de-elevated
//! like every launch of the dock) AFTER the popup closes. Right click on an app pins / unpins it
//! (`config::toggle_start_pin`). No power buttons in this (vertical) style.
//!
//! HORIZONTAL STYLE (`config.start_style`, read when the menu opens): the same content with a second
//! layout (`layout_h`) in the Windows 11 organization, sharing the store, the search, the pins and the
//! launch: a wider fixed panel; the search box; "Pinned" with an "All apps" button; pages of
//! `H_COLS * H_ROWS` tiles (wheel or page dots); "All apps" swaps the body for the alphabetical
//! list ("Back" or Esc returns); a bottom bar with the user's initial and name (no picture) and a
//! power button.
//!
//! POWER FLYOUT (horizontal style): the power button toggles a small rounded panel drawn by this
//! popup itself over its own surface (no second window), right-aligned to the button and just above
//! it, like the native Windows 11 one. It offers the entries of the power popup (`popup_power`:
//! labels, glyphs and actions shared from there, without the autostart toggle): a system action runs
//! after the Start closes, "Settings" switches to the Settings popup. While it is open a backdrop
//! hit covers the rest of the menu: a click anywhere else, a right click or Esc closes only the
//! flyout (Esc does not close the Start); typing closes it and goes on searching. It lives in the
//! content, so it is gone when the Start closes.
//!
//! DATA: the catalog and the icons live in a UI-thread store (`STORE`) that outlives the popup, so the
//! menu reopens instantly. Every open asks the shell thread for a fresh catalog
//! (`ShellRequest::ListStartApps`); only the icons not received yet are extracted (in small batches,
//! behind every other shell request) and arrive as `ShellEvent::StartIcons`. Nothing is loaded until
//! the menu is opened the first time.
//!
//! Declared as a child module of `popup` (`#[path]`). UI thread only.

use std::cell::RefCell;
use std::collections::HashMap;

use windows::Win32::Foundation::RECT;

use crate::config::StartStyle;
use crate::i18n::tr;
use crate::icons::IconBitmap;
use crate::log;
use crate::popup::{center_y, hline, lift, row_plate, Button, Content, DataEvent, HitMap, PopupKind, Reply};
use crate::popup_power::{self, Choice};
use crate::power::PowerAction;
use crate::render::Metrics;
use crate::shell_thread::{self, ShellRequest, StartApp};
use crate::surface::{
    rect_wh, Font, Surface, HOVER_ALPHA, PANEL_BORDER_ALPHA, ROW_RADIUS, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, WHITE,
};

// ------------------------------------------------------------------ layout constants (logical px)

/// Panel size: fixed, so the menu does not jump while the search filters the list.
const W: i32 = 520;
const H: i32 = 600;
/// Outer inset of the content.
const INSET: i32 = 14;
const SEARCH_H: i32 = 38;
/// Space between the sections.
const GAP: i32 = 10;
/// Section title line ("Pinned", "All apps").
const HEADER_H: i32 = 28;
/// Pinned grid: columns, rows at most, tile height and icon side.
const COLS: usize = 6;
const MAX_PIN_ROWS: usize = 2;
const TILE_H: i32 = 80;
const TILE_ICON: i32 = 32;
/// App list: row height, icon side, horizontal padding inside a row.
const ROW_H: i32 = 38;
const ROW_ICON: i32 = 24;
const ROW_PAD: i32 = 10;
/// Scroll bar width and the gap between it and the rows.
const SCROLLBAR_W: i32 = 4;
const SCROLLBAR_GAP: i32 = 6;
/// Rows scrolled per wheel notch.
const WHEEL_ROWS: i64 = 3;
/// Side of the icons kept in the store (the tile size; list rows shrink them).
const STORE_ICON: i32 = TILE_ICON;
/// Longest search text kept (chars).
const MAX_QUERY: usize = 64;

// Horizontal style (`layout_h`).
/// Panel size: fixed.
const HW: i32 = 640;
const HH: i32 = 620;
const H_INSET: i32 = 20;
/// Pinned page: columns, rows, tiles per page, tile height, icon top inside a tile.
const H_COLS: usize = 6;
const H_ROWS: usize = 3;
const H_PAGE: usize = H_COLS * H_ROWS;
const H_TILE_H: i32 = 96;
const H_TILE_ICON_TOP: i32 = 16;
/// Header button ("All apps" / "Back"): height and horizontal text padding.
const H_BTN_H: i32 = 26;
const H_BTN_PAD: i32 = 12;
/// Idle fill of the header button (a hover plate goes on top).
const H_BTN_ALPHA: u32 = 16;
/// Bottom bar: height, user circle diameter, power button side.
const H_BAR_H: i32 = 60;
const H_AVATAR: i32 = 32;
const H_POWER: i32 = 36;
/// Page dots: dot diameter and the side of each dot's hit square (also the spacing).
const H_DOT: i32 = 6;
const H_DOT_HIT: i32 = 16;
/// Power flyout: width, row height, inner inset, gap above the power button, separator band,
/// corner radius, margin kept from the panel edges.
const FLY_W: i32 = 224;
const FLY_ROW: i32 = 34;
const FLY_INSET: i32 = 6;
const FLY_GAP: i32 = 6;
const FLY_SEP: i32 = 9;
const FLY_RADIUS: i32 = 10;
const FLY_MARGIN: i32 = 4;
/// Power flyout fill over the panel colour (a lift that sets it apart) and its outline.
const FLY_LIFT_ALPHA: u32 = 12;
const FLY_RIM_ALPHA: u32 = PANEL_BORDER_ALPHA * 2;

// ------------------------------------------------------------------ hit ids

const ID_SEARCH: u32 = 1;
/// Horizontal style: header button ("All apps" / "Back"), power button, page dot + page.
const ID_ALL: u32 = 2;
const ID_POWER: u32 = 3;
const ID_DOT: u32 = 10;
/// Most page dots drawn (ids stay below `ID_FLY_BACKDROP`).
const MAX_DOTS: usize = 64;
/// Power flyout open: the backdrop over the rest of the menu (closes the flyout) and, + a
/// `popup_power` item id, the flyout rows.
const ID_FLY_BACKDROP: u32 = 900;
const ID_FLY: u32 = 910;
/// + index of the pinned tile.
const ID_PIN: u32 = 1_000;
/// + index in the result list.
const ID_ROW: u32 = 100_000;

// ------------------------------------------------------------------ virtual keys

const VK_BACK: u16 = 0x08;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_UP: u16 = 0x26;
const VK_DOWN: u16 = 0x28;

// ------------------------------------------------------------------ store (outlives the popup)

struct App {
    id: String,
    name: String,
    /// Lowercase name (search and sort key).
    lower: String,
}

#[derive(Default)]
struct Store {
    /// Alphabetical.
    apps: Vec<App>,
    /// true once the first catalog arrived ("Loading" until then).
    loaded: bool,
    /// Icons by lowercase id, `icon_px` square.
    icons: HashMap<String, IconBitmap>,
    icon_px: u32,
}

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
}

/// `ShellEvent::StartApps`: replaces the catalog (sorted by name).
pub(crate) fn store_apps(list: Vec<StartApp>) {
    let mut apps: Vec<App> =
        list.into_iter().map(|a| App { lower: a.name.to_lowercase(), id: a.id, name: a.name }).collect();
    apps.sort_by(|a, b| a.lower.cmp(&b.lower).then_with(|| a.name.cmp(&b.name)));
    STORE.with(|s| match s.try_borrow_mut() {
        Ok(mut s) => {
            s.apps = apps;
            s.loaded = true;
        }
        Err(_) => log!("startmenu: store busy, catalog dropped"),
    });
}

/// `ShellEvent::StartIcons`: keeps the icons made at the current size. true if any was stored.
pub(crate) fn store_icons(icon_px: u32, icons: Vec<(String, Option<IconBitmap>)>) -> bool {
    STORE.with(|s| {
        let Ok(mut s) = s.try_borrow_mut() else { return false };
        if icon_px != s.icon_px {
            return false;
        }
        let mut any = false;
        for (id, b) in icons {
            if let Some(b) = b {
                s.icons.insert(id.to_lowercase(), b);
                any = true;
            }
        }
        any
    })
}

/// Asks the shell thread for a fresh catalog (and the icons still missing) at `px`. A larger size
/// than the stored one (a display with a higher DPI) starts the icons over; a smaller one reuses
/// them (they are shrunk when drawn).
fn request_catalog(px: u32) {
    let px = STORE.with(|s| match s.try_borrow_mut() {
        Ok(mut s) => {
            if px > s.icon_px {
                s.icon_px = px;
                s.icons.clear();
            }
            s.icon_px
        }
        Err(_) => px,
    });
    let first = crate::config::get().start_pins.clone();
    shell_thread::request(ShellRequest::ListStartApps { icon_px: px, first });
}

// ------------------------------------------------------------------ search (pure)

/// Match rank of the lowercase name `lower` for the lowercase query `q`: every word of the query
/// must appear in the name; 0 = the name starts with the query, 1 = a word of the name starts with
/// the first word of the query, 2 = found elsewhere. `None` = no match. An empty query matches all.
pub(crate) fn rank(lower: &str, q: &str) -> Option<u8> {
    let q = q.trim();
    if q.is_empty() {
        return Some(0);
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    if !words.iter().all(|w| lower.contains(w)) {
        return None;
    }
    if lower.starts_with(q) {
        return Some(0);
    }
    let first = words[0];
    if lower.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(first)) {
        Some(1)
    } else {
        Some(2)
    }
}

/// Indices of `names` (lowercase, already alphabetical) matching `query`, best rank first, the
/// alphabetical order kept inside each rank.
pub(crate) fn filter(names: &[&str], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    let mut v: Vec<(u8, usize)> = names.iter().enumerate().filter_map(|(i, n)| rank(n, &q).map(|r| (r, i))).collect();
    v.sort_by_key(|&(r, i)| (r, i));
    v.into_iter().map(|(_, i)| i).collect()
}

// ------------------------------------------------------------------ geometry (pure)

/// Layout of the panel, client coordinates, physical px.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Geo {
    pub w: i32,
    pub h: i32,
    pub search: RECT,
    /// "Pinned" title; `None` without pins (or while searching).
    pub pins_header: Option<RECT>,
    /// One tile per shown pin.
    pub tiles: Vec<RECT>,
    pub list_header: RECT,
    /// App list viewport (rows + scroll bar).
    pub list: RECT,
    pub row_h: i32,
    /// Whole rows that fit the viewport (at least 1).
    pub rows: usize,
    /// The list is on screen (always in the vertical style; the horizontal one shows it while
    /// searching or on the "All apps" page).
    pub list_shown: bool,
    /// Horizontal style only: the header button, the pinned page area, the page dots, the bottom
    /// bar and its power button.
    pub button: Option<RECT>,
    pub grid: RECT,
    pub dots: Vec<RECT>,
    pub bar: Option<RECT>,
    pub power: Option<RECT>,
}

/// Layout for `pins` pinned apps to show (0 while searching).
pub(crate) fn layout(m: Metrics, pins: usize) -> Geo {
    let (w, h, inset, gap) = (m.px(W), m.px(H), m.px(INSET), m.px(GAP));
    let inner = w - 2 * inset;
    let search = rect_wh(inset, inset, inner, m.px(SEARCH_H));
    let mut y = search.bottom + gap;
    let mut pins_header = None;
    let mut tiles = Vec::new();
    let shown = pins.min(COLS * MAX_PIN_ROWS);
    if shown > 0 {
        let hh = rect_wh(inset, y, inner, m.px(HEADER_H));
        y = hh.bottom;
        pins_header = Some(hh);
        let (tw, th) = (inner / COLS as i32, m.px(TILE_H));
        for i in 0..shown {
            let (r, c) = ((i / COLS) as i32, (i % COLS) as i32);
            tiles.push(rect_wh(inset + c * tw, y + r * th, tw, th));
        }
        y += shown.div_ceil(COLS) as i32 * th + gap;
    }
    let list_header = rect_wh(inset, y, inner, m.px(HEADER_H));
    let row_h = m.px(ROW_H).max(1);
    let list = RECT { left: inset, top: list_header.bottom, right: w - inset, bottom: h - inset };
    let rows = ((list.bottom - list.top) / row_h).max(1) as usize;
    Geo { w, h, search, pins_header, tiles, list_header, list, row_h, rows, list_shown: true, ..Geo::default() }
}

/// Horizontal layout. `pins`: pinned apps to page through (0 on the list); `page`: the page shown
/// (clamped by the caller); `list`: the list replaces the pinned page (search or "All apps");
/// `button_w`: width of the header button (physical px), `None` = no button (searching).
pub(crate) fn layout_h(m: Metrics, pins: usize, page: usize, list: bool, button_w: Option<i32>) -> Geo {
    let (w, h, inset, gap) = (m.px(HW), m.px(HH), m.px(H_INSET), m.px(GAP));
    let inner = w - 2 * inset;
    let search = rect_wh(inset, inset, inner, m.px(SEARCH_H));
    let list_header = rect_wh(inset, search.bottom + gap + m.px(6), inner, m.px(HEADER_H));
    let button = button_w.map(|bw| {
        let bh = m.px(H_BTN_H);
        rect_wh(list_header.right - bw, list_header.top + (list_header.bottom - list_header.top - bh) / 2, bw, bh)
    });
    let bar = RECT { left: 0, top: h - m.px(H_BAR_H), right: w, bottom: h };
    let body = RECT { left: inset, top: list_header.bottom + m.px(6), right: w - inset, bottom: bar.top - gap };
    let row_h = m.px(ROW_H).max(1);
    let rows = ((body.bottom - body.top) / row_h).max(1) as usize;
    let (tw, th) = (inner / H_COLS as i32, m.px(H_TILE_H));
    let grid = RECT { bottom: body.top + H_ROWS as i32 * th, ..body };
    let mut tiles = Vec::new();
    let mut dots = Vec::new();
    if !list {
        let first = page * H_PAGE;
        for i in 0..pins.saturating_sub(first).min(H_PAGE) {
            let (r, c) = ((i / H_COLS) as i32, (i % H_COLS) as i32);
            tiles.push(rect_wh(inset + c * tw, grid.top + r * th, tw, th));
        }
        let pages = pins.div_ceil(H_PAGE).min(MAX_DOTS);
        if pages > 1 {
            // A column in the right margin, centred on the grid.
            let d = m.px(H_DOT_HIT);
            let x = w - inset + (inset - d) / 2;
            let y0 = grid.top + (grid.bottom - grid.top - pages as i32 * d) / 2;
            dots = (0..pages as i32).map(|p| rect_wh(x, y0 + p * d, d, d)).collect();
        }
    }
    let pw = m.px(H_POWER);
    let power = rect_wh(w - inset - pw, bar.top + (bar.bottom - bar.top - pw) / 2, pw, pw);
    Geo {
        w,
        h,
        search,
        pins_header: None,
        tiles,
        list_header,
        list: body,
        row_h,
        rows,
        list_shown: list,
        button,
        grid,
        dots,
        bar: Some(bar),
        power: Some(power),
    }
}

/// Power flyout layout, client coordinates, physical px.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Flyout {
    pub panel: RECT,
    /// `popup_power` item id and its row, in menu order.
    pub rows: Vec<(u32, RECT)>,
    /// Ordinate of the separator between the system actions and the dock entries.
    pub sep_y: i32,
}

/// Flyout of a `w` x `h` panel for the power button `power`: right-aligned to the button and just
/// above it, kept inside the panel. Rows: `popup_power::SYSTEM_ITEMS`, a separator,
/// `popup_power::DOCK_ITEMS`.
pub(crate) fn layout_flyout(m: Metrics, w: i32, h: i32, power: RECT) -> Flyout {
    let (fw, row, inset, sep) = (m.px(FLY_W), m.px(FLY_ROW), m.px(FLY_INSET), m.px(FLY_SEP));
    let n = (popup_power::SYSTEM_ITEMS.len() + popup_power::DOCK_ITEMS.len()) as i32;
    let fh = 2 * inset + n * row + sep;
    let margin = m.px(FLY_MARGIN);
    let right = power.right.min(w - margin);
    let left = (right - fw).max(margin);
    let bottom = (power.top - m.px(FLY_GAP)).min(h - margin);
    let top = (bottom - fh).max(margin);
    let panel = RECT { left, top, right, bottom: top + fh };
    let mut rows = Vec::with_capacity(n as usize);
    let mut y = top + inset;
    for id in popup_power::SYSTEM_ITEMS {
        rows.push((id, rect_wh(left + inset, y, right - left - 2 * inset, row)));
        y += row;
    }
    let sep_y = y + sep / 2;
    y += sep;
    for id in popup_power::DOCK_ITEMS {
        rows.push((id, rect_wh(left + inset, y, right - left - 2 * inset, row)));
        y += row;
    }
    Flyout { panel, rows, sep_y }
}

/// The end of `text` that fits in `max_w` (the start is dropped): the search box shows what is
/// being typed.
fn tail_fit(s: &Surface, text: &str, max_w: i32) -> String {
    let mut t = text;
    while !t.is_empty() && s.text_width(t, Font::Normal) > max_w {
        let mut it = t.char_indices();
        it.next();
        t = it.next().map_or("", |(i, _)| &t[i..]);
    }
    t.to_string()
}

/// Magnifier glyph (16x16 space): a ring and a handle towards the bottom right.
fn magnifier(u: f32, v: f32) -> bool {
    let (dx, dy) = (u - 6.5, v - 6.5);
    if ((dx * dx + dy * dy).sqrt() - 4.6).abs() <= 0.85 {
        return true;
    }
    let (du, dv) = (u - 10.0, v - 10.0);
    let t = (du + dv) / 2.0;
    (0.0..=4.0).contains(&t) && (du - dv).abs() / std::f32::consts::SQRT_2 <= 1.0
}

// ------------------------------------------------------------------ labels

struct Labels {
    placeholder: &'static str,
    pinned: &'static str,
    all: &'static str,
    results: &'static str,
    hint: &'static str,
    loading: &'static str,
    none: &'static str,
    /// Horizontal style: header buttons and the hint of an empty pinned page.
    all_button: &'static str,
    back: &'static str,
    no_pins: &'static str,
}

/// The labels in the dock language (`i18n::tr`, like every other popup; English in preview mode).
fn labels() -> Labels {
    Labels {
        placeholder: tr("Type to search apps", "Scrivi per cercare un'app"),
        pinned: tr("Pinned", "Aggiunte"),
        all: tr("All apps", "Tutte le app"),
        results: tr("Results", "Risultati"),
        hint: tr("Right-click to pin", "Clic destro per fissare"),
        loading: tr("Loading apps\u{2026}", "Caricamento app\u{2026}"),
        none: tr("No apps found", "Nessuna app trovata"),
        all_button: tr("All apps \u{203A}", "Tutte le app \u{203A}"),
        back: tr("\u{2039} Back", "\u{2039} Indietro"),
        no_pins: tr("Right-click an app in All apps to pin it", "Clic destro su un'app in Tutte le app per fissarla"),
    }
}

// ------------------------------------------------------------------ content

pub(crate) struct StartMenu {
    m: Metrics,
    geo: Geo,
    query: String,
    /// Indices into `STORE.apps` matching the query (all of them when empty).
    results: Vec<usize>,
    /// Indices into `STORE.apps` of the pins shown (none while searching).
    pins: Vec<usize>,
    /// First result shown.
    scroll: usize,
    /// Keyboard selection (index into `results`).
    sel: Option<usize>,
    hits: HitMap,
    requested: bool,
    /// App id chosen, launched after the popup closes.
    pending: Option<String>,
    /// Horizontal style (`config.start_style` at open time).
    horiz: bool,
    /// Horizontal: the "All apps" page replaces the pinned page.
    all: bool,
    /// Horizontal: pinned page shown.
    page: usize,
    /// Horizontal: Windows user name for the bottom bar (empty in the vertical style).
    user: String,
    /// Horizontal: widths of the "All apps" and "Back" buttons (physical px, set by `size`).
    button_w: (i32, i32),
    /// Horizontal: the power flyout is open, and its layout (set by `refresh`).
    flyout: bool,
    fly: Flyout,
    /// Power action chosen in the flyout, run after the popup closes.
    power_pending: Option<PowerAction>,
}

/// Creates the content in the configured style; labels in the dock language (`i18n`).
pub(crate) fn new() -> Box<dyn Content> {
    let mut c = make();
    if crate::config::get().start_style == StartStyle::Horizontal {
        c.horiz = true;
        // The account name of the process (as GetUserNameW): no picture, nothing else loaded.
        c.user = std::env::var("USERNAME").unwrap_or_default();
    }
    Box::new(c)
}

fn make() -> StartMenu {
    StartMenu {
        m: Metrics { dpi: 96 },
        geo: Geo::default(),
        query: String::new(),
        results: Vec::new(),
        pins: Vec::new(),
        scroll: 0,
        sel: None,
        hits: HitMap::default(),
        requested: false,
        pending: None,
        horiz: false,
        all: false,
        page: 0,
        user: String::new(),
        button_w: (0, 0),
        flyout: false,
        fly: Flyout::default(),
        power_pending: None,
    }
}

/// PREVIEW-HOOK: the menu over a mock catalog (`apps`: id, name, icon; the icons all of one size),
/// English labels (`--preview` puts `i18n` in preview mode), `query` already typed, in `style`
/// (horizontal: `all` = on the "All apps" page, a fixed user name, `power` = the power flyout
/// open). Replaces the store; asks the shell thread for nothing.
pub(crate) fn preview_new(
    apps: Vec<(String, String, IconBitmap)>,
    query: &str,
    style: StartStyle,
    all: bool,
    power: bool,
) -> Box<dyn Content> {
    let px = apps.first().map_or(32, |a| a.2.width);
    STORE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            *s = Store { icon_px: px, ..Store::default() };
        }
    });
    store_apps(apps.iter().map(|(id, name, _)| StartApp { id: id.clone(), name: name.clone() }).collect());
    store_icons(px, apps.into_iter().map(|(id, _, b)| (id, Some(b))).collect());
    let mut c = make();
    c.requested = true;
    if style == StartStyle::Horizontal {
        c.horiz = true;
        c.all = all;
        c.user = "User".to_string();
        c.flyout = power;
    }
    c.query = query.to_string();
    if c.searching() {
        c.sel = Some(0);
    }
    Box::new(c)
}

impl StartMenu {
    fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// Recomputes results, pins, layout, scroll / selection bounds and the hit map from the store.
    fn refresh(&mut self) {
        let cfg = crate::config::get();
        let searching = self.searching();
        let (results, pins) = STORE.with(|s| {
            let Ok(s) = s.try_borrow() else { return (Vec::new(), Vec::new()) };
            let names: Vec<&str> = s.apps.iter().map(|a| a.lower.as_str()).collect();
            let results = filter(&names, &self.query);
            let pins = if searching {
                Vec::new()
            } else {
                cfg.start_pins.iter().filter_map(|p| s.apps.iter().position(|a| a.id.eq_ignore_ascii_case(p))).collect()
            };
            (results, pins)
        });
        self.results = results;
        self.pins = pins;
        if self.horiz {
            let list = self.list_mode();
            self.page = self.page.min(self.pages() - 1);
            let button = (!searching).then_some(if self.all { self.button_w.1 } else { self.button_w.0 });
            self.geo = layout_h(self.m, if list { 0 } else { self.pins.len() }, self.page, list, button);
            self.fly = match self.geo.power {
                Some(p) => layout_flyout(self.m, self.geo.w, self.geo.h, p),
                None => Flyout::default(),
            };
        } else {
            self.geo = layout(self.m, self.pins.len());
            self.pins.truncate(self.geo.tiles.len());
        }
        self.scroll = self.scroll.min(self.max_scroll());
        if let Some(s) = self.sel {
            self.sel = if self.results.is_empty() { None } else { Some(s.min(self.results.len() - 1)) };
        }
        self.rebuild_hits();
    }

    fn max_scroll(&self) -> usize {
        self.results.len().saturating_sub(self.geo.rows)
    }

    /// Horizontal: the list replaces the pinned page (searching, or the "All apps" page).
    fn list_mode(&self) -> bool {
        self.searching() || self.all
    }

    /// Horizontal: pinned pages (at least 1, at most `MAX_DOTS`).
    fn pages(&self) -> usize {
        self.pins.len().div_ceil(H_PAGE).clamp(1, MAX_DOTS)
    }

    /// Index into `pins` of the first tile shown.
    fn pin_base(&self) -> usize {
        if self.horiz {
            self.page * H_PAGE
        } else {
            0
        }
    }

    /// Horizontal: shows the "All apps" page (`true`) or back to the pinned page.
    fn show_all(&mut self, on: bool) -> Reply {
        self.all = on;
        self.scroll = 0;
        self.sel = None;
        self.refresh();
        Reply::Redraw
    }

    /// Rectangle of the `k`-th visible row.
    fn row_rect(&self, k: usize) -> RECT {
        let g = &self.geo;
        let top = g.list.top + k as i32 * g.row_h;
        let right = g.list.right - self.m.px(SCROLLBAR_W + SCROLLBAR_GAP);
        RECT { left: g.list.left, top, right, bottom: top + g.row_h }
    }

    fn rebuild_hits(&mut self) {
        let shown = if self.geo.list_shown { self.geo.rows } else { 0 };
        let rows: Vec<(u32, RECT)> = (0..shown)
            .map(|k| (k, self.scroll + k))
            .take_while(|&(_, i)| i < self.results.len())
            .map(|(k, i)| (ID_ROW + i as u32, self.row_rect(k)))
            .collect();
        self.hits.clear();
        self.hits.add(ID_SEARCH, self.geo.search);
        let n = self.pins.len().saturating_sub(self.pin_base());
        for (i, rc) in self.geo.tiles.iter().enumerate().take(n) {
            self.hits.add(ID_PIN + i as u32, *rc);
        }
        for (id, rc) in rows {
            self.hits.add(id, rc);
        }
        if let Some(b) = self.geo.button {
            self.hits.add(ID_ALL, b);
        }
        for (p, rc) in self.geo.dots.iter().enumerate() {
            self.hits.add(ID_DOT + p as u32, *rc);
        }
        if let Some(p) = self.geo.power {
            self.hits.add(ID_POWER, p);
            if self.flyout {
                // On top of everything (the last added wins): the backdrop, the power button again
                // (it toggles the flyout), then the flyout rows.
                self.hits.add(ID_FLY_BACKDROP, RECT { left: 0, top: 0, right: self.geo.w, bottom: self.geo.h });
                self.hits.add(ID_POWER, p);
                for &(id, rc) in &self.fly.rows {
                    self.hits.add(ID_FLY + id, rc);
                }
            }
        }
    }

    /// Opens or closes the power flyout (horizontal style only).
    fn set_flyout(&mut self, on: bool) -> Reply {
        let on = on && self.geo.power.is_some();
        if on == self.flyout {
            return Reply::None;
        }
        self.flyout = on;
        self.rebuild_hits();
        Reply::Redraw
    }

    /// A row of the flyout was chosen: an action runs after the popup closes, "Settings" switches.
    fn choose_power(&mut self, item: u32) -> Reply {
        match popup_power::choice(item) {
            Some(Choice::Settings) => Reply::Switch(PopupKind::Settings),
            Some(Choice::Run(a)) => {
                self.power_pending = Some(a);
                Reply::Close
            }
            None => Reply::None,
        }
    }

    /// Keeps the selection inside the viewport.
    fn ensure_visible(&mut self) {
        if let Some(s) = self.sel {
            if s < self.scroll {
                self.scroll = s;
            } else if s >= self.scroll + self.geo.rows {
                self.scroll = s + 1 - self.geo.rows;
            }
        }
    }

    /// Id of the app at `idx` in the store.
    fn app_id(idx: usize) -> Option<String> {
        STORE.with(|s| s.try_borrow().ok().and_then(|s| s.apps.get(idx).map(|a| a.id.clone())))
    }

    /// Store index of the app behind hit `id` (pin tile or list row).
    fn app_at(&self, id: u32) -> Option<usize> {
        if (ID_PIN..ID_ROW).contains(&id) {
            self.pins.get(self.pin_base() + (id - ID_PIN) as usize).copied()
        } else if id >= ID_ROW {
            self.results.get((id - ID_ROW) as usize).copied()
        } else {
            None
        }
    }

    /// The text changed: back to the top, the first result selected while searching.
    fn query_changed(&mut self) -> Reply {
        self.scroll = 0;
        self.refresh();
        self.sel = (self.searching() && !self.results.is_empty()).then_some(0);
        Reply::Redraw
    }

    /// Moves the selection by `delta` rows (clamped); the first move from no selection lands on the top row shown.
    fn move_sel(&mut self, delta: i64) -> Reply {
        if self.results.is_empty() {
            return Reply::None;
        }
        let last = self.results.len() as i64 - 1;
        let next = match self.sel {
            Some(s) => (s as i64 + delta).clamp(0, last),
            None => (self.scroll as i64).clamp(0, last),
        };
        self.sel = Some(next as usize);
        self.ensure_visible();
        self.rebuild_hits();
        Reply::Redraw
    }

    /// Chooses the app at store index `idx`: launched after the popup closes.
    fn choose(&mut self, idx: usize) -> Reply {
        match Self::app_id(idx) {
            Some(id) => {
                self.pending = Some(id);
                Reply::Close
            }
            None => Reply::None,
        }
    }

    // -------------------------------------------------------------- painting

    fn paint_search(&self, s: &mut Surface) {
        let m = s.metrics();
        let g = &self.geo;
        let r = m.px(ROW_RADIUS);
        s.fill_rr(g.search, r, WHITE, 16);
        s.stroke_rr(g.search, r, m.px(1).max(1), 34);
        let gs = m.px(16);
        let gx = g.search.left + m.px(12);
        s.glyph(gx, g.search.top + (g.search.bottom - g.search.top - gs) / 2, gs, TEXT2_ALPHA, &magnifier);
        let tx = gx + gs + m.px(10);
        let lh = s.line_height(Font::Normal);
        let ty = center_y(&g.search, lh);
        let max_w = g.search.right - m.px(12) - tx;
        let caret_x = if self.query.is_empty() {
            s.text(labels().placeholder, tx, ty, Font::Normal, WHITE, TEXT3_ALPHA);
            tx
        } else {
            let shown = tail_fit(s, &self.query, max_w);
            s.text(&shown, tx, ty, Font::Normal, WHITE, TEXT_ALPHA);
            tx + s.text_width(&shown, Font::Normal) + m.px(1)
        };
        s.fill_rr(rect_wh(caret_x, ty, m.px(1).max(1), lh), 0, WHITE, 200);
    }

    fn paint_header(s: &mut Surface, rc: RECT, title: &str, hint: Option<&str>) {
        let m = s.metrics();
        s.text(title, rc.left + m.px(4), center_y(&rc, s.line_height(Font::Bold)), Font::Bold, WHITE, TEXT2_ALPHA);
        if let Some(h) = hint {
            let w = s.text_width(h, Font::Small);
            let y = center_y(&rc, s.line_height(Font::Small));
            s.text(h, rc.right - m.px(4) - w, y, Font::Small, WHITE, TEXT3_ALPHA);
        }
    }

    /// The app icon (shrunk to `size`), or a dim placeholder square while it has not arrived.
    fn paint_icon(s: &mut Surface, icons: &HashMap<String, IconBitmap>, id: &str, x: i32, y: i32, size: i32) {
        match icons.get(&id.to_lowercase()) {
            Some(b) => s.blit(b, x, y, size, 255),
            None => s.fill_rr(rect_wh(x, y, size, size), size / 4, WHITE, 26),
        }
    }

    /// A pinned tile: hover plate, icon `icon_top` below the tile top, the name under it.
    #[allow(clippy::too_many_arguments)]
    fn paint_tile(s: &mut Surface, st: &Store, rc: &RECT, app: &App, id: u32, icon_top: i32, hot: Option<u32>, pressed: Option<u32>) {
        let m = s.metrics();
        let ti = m.px(TILE_ICON);
        let lh_small = s.line_height(Font::Small);
        let plate = RECT { left: rc.left + m.px(2), top: rc.top, right: rc.right - m.px(2), bottom: rc.bottom };
        let lit = row_plate(s, plate, id, hot, pressed);
        let tw = rc.right - rc.left;
        let iy = rc.top + icon_top;
        Self::paint_icon(s, &st.icons, &app.id, rc.left + (tw - ti) / 2, iy, ti);
        let name = s.ellipsize(&app.name, Font::Small, tw - m.px(10));
        let nx = rc.left + (tw - s.text_width(&name, Font::Small)) / 2;
        let ny = iy + ti + m.px(8);
        debug_assert!(ny + lh_small <= rc.bottom + m.px(4));
        s.text(&name, nx, ny, Font::Small, WHITE, lift(TEXT_ALPHA, lit));
    }

    fn paint_body(&self, s: &mut Surface, st: &Store, hot: Option<u32>, pressed: Option<u32>) {
        let m = s.metrics();
        let g = &self.geo;
        // Pinned grid.
        if let Some(hh) = g.pins_header {
            Self::paint_header(s, hh, labels().pinned, None);
        }
        for (i, (rc, &idx)) in g.tiles.iter().zip(&self.pins).enumerate() {
            let Some(app) = st.apps.get(idx) else { continue };
            Self::paint_tile(s, st, rc, app, ID_PIN + i as u32, m.px(10), hot, pressed);
        }
        // App list.
        let title = if self.searching() { labels().results } else { labels().all };
        Self::paint_header(s, g.list_header, title, Some(labels().hint));
        self.paint_list(s, st, hot, pressed);
    }

    /// Horizontal style: header with its button, pinned page or list, bottom bar.
    fn paint_body_h(&self, s: &mut Surface, st: &Store, hot: Option<u32>, pressed: Option<u32>) {
        let m = s.metrics();
        let g = &self.geo;
        let list = self.list_mode();
        // Header: title on the left, the button on the right, the pin hint before it on the list.
        let title = if self.searching() {
            labels().results
        } else if self.all {
            labels().all
        } else {
            labels().pinned
        };
        let head = RECT { right: g.button.map_or(g.list_header.right, |b| b.left - m.px(8)), ..g.list_header };
        Self::paint_header(s, head, title, list.then_some(labels().hint));
        if let Some(b) = g.button {
            let r = m.px(ROW_RADIUS);
            s.fill_rr(b, r, WHITE, H_BTN_ALPHA);
            let lit = row_plate(s, b, ID_ALL, hot, pressed);
            let text = if self.all { labels().back } else { labels().all_button };
            let tx = b.left + (b.right - b.left - s.text_width(text, Font::Small)) / 2;
            s.text(text, tx, center_y(&b, s.line_height(Font::Small)), Font::Small, WHITE, lift(TEXT_ALPHA, lit));
        }
        if list {
            self.paint_list(s, st, hot, pressed);
        } else if self.pins.is_empty() {
            // Nothing pinned (or the catalog still loading while pins are configured).
            let waiting = !st.loaded && !crate::config::get().start_pins.is_empty();
            let msg = if waiting { labels().loading } else { labels().no_pins };
            let msg = s.ellipsize(msg, Font::Normal, g.grid.right - g.grid.left);
            let w = s.text_width(&msg, Font::Normal);
            let x = g.grid.left + (g.grid.right - g.grid.left - w) / 2;
            let y = center_y(&g.grid, s.line_height(Font::Normal));
            s.text(&msg, x, y, Font::Normal, WHITE, TEXT3_ALPHA);
        } else {
            let base = self.pin_base();
            for (i, rc) in g.tiles.iter().enumerate() {
                let Some(app) = self.pins.get(base + i).and_then(|&idx| st.apps.get(idx)) else { continue };
                Self::paint_tile(s, st, rc, app, ID_PIN + i as u32, m.px(H_TILE_ICON_TOP), hot, pressed);
            }
            let d = m.px(H_DOT).max(2);
            for (p, rc) in g.dots.iter().enumerate() {
                let id = ID_DOT + p as u32;
                let a = if p == self.page {
                    220
                } else if hot == Some(id) {
                    140
                } else {
                    70
                };
                let (cx, cy) = ((rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2);
                s.fill_rr(rect_wh(cx - d / 2, cy - d / 2, d, d), d / 2, WHITE, a);
            }
        }
        self.paint_bar(s, hot, pressed);
    }

    /// Horizontal style: separator, user circle with the initial, user name, power button.
    fn paint_bar(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        let (Some(bar), Some(power)) = (self.geo.bar, self.geo.power) else { return };
        let m = s.metrics();
        hline(s, bar.left, bar.right, bar.top);
        let inset = m.px(H_INSET);
        let d = m.px(H_AVATAR);
        let circle = rect_wh(inset, bar.top + (bar.bottom - bar.top - d) / 2, d, d);
        s.fill_rr(circle, d / 2, WHITE, 40);
        let initial: String = self.user.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
        if !initial.is_empty() {
            let x = circle.left + (d - s.text_width(&initial, Font::Bold)) / 2;
            s.text(&initial, x, center_y(&circle, s.line_height(Font::Bold)), Font::Bold, WHITE, TEXT_ALPHA);
        }
        let nx = circle.right + m.px(12);
        let name = s.ellipsize(&self.user, Font::Normal, power.left - m.px(16) - nx);
        s.text(&name, nx, center_y(&bar, s.line_height(Font::Normal)), Font::Normal, WHITE, TEXT_ALPHA);
        let mut lit = row_plate(s, power, ID_POWER, hot, pressed);
        if !lit && self.flyout {
            // The button stays lit while its flyout is open.
            s.plate(power, m.px(ROW_RADIUS), HOVER_ALPHA);
            lit = true;
        }
        let gs = m.px(16);
        let gx = power.left + (power.right - power.left - gs) / 2;
        let gy = power.top + (power.bottom - power.top - gs) / 2;
        s.glyph(gx, gy, gs, lift(TEXT2_ALPHA, lit), &crate::glyphs::power_shape);
    }

    /// Horizontal style, flyout open: a raised rounded panel over the menu with the power rows
    /// (icon and label as in the power popup, the same hover / pressed plates).
    fn paint_flyout(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        if !self.flyout || self.fly.rows.is_empty() {
            return;
        }
        let m = s.metrics();
        let f = &self.fly;
        let r = m.px(FLY_RADIUS);
        // Soft shadow, opaque base in the panel colour, a lift and an outline.
        let g = m.px(2).max(1);
        let shadow = RECT { left: f.panel.left - g, top: f.panel.top - g, right: f.panel.right + g, bottom: f.panel.bottom + 2 * g };
        s.fill_rr(shadow, r + g, (0, 0, 0), 120);
        s.fill_rr(f.panel, r, crate::config::get().background.rgb(), 255);
        s.fill_rr(f.panel, r, WHITE, FLY_LIFT_ALPHA);
        s.stroke_rr(f.panel, r, m.px(1).max(1), FLY_RIM_ALPHA);
        let pad = m.px(12);
        hline(s, f.panel.left + pad, f.panel.right - pad, f.sep_y);
        let icon = m.px(16);
        let lh = s.line_height(Font::Normal);
        for &(item, rc) in &f.rows {
            let lit = row_plate(s, rc, ID_FLY + item, hot, pressed);
            let a = lift(popup_power::alpha(item), lit);
            let shape = popup_power::shape(item);
            s.glyph(rc.left + pad, rc.top + (rc.bottom - rc.top - icon) / 2, icon, a, &shape);
            let tx = rc.left + pad + icon + m.px(8);
            let label = s.ellipsize(popup_power::label(item), Font::Normal, rc.right - pad - tx);
            s.text(&label, tx, center_y(&rc, lh), Font::Normal, WHITE, a);
        }
    }

    /// The app list (results, or all apps): rows, keyboard selection, scroll bar; a message when
    /// empty or still loading.
    fn paint_list(&self, s: &mut Surface, st: &Store, hot: Option<u32>, pressed: Option<u32>) {
        let m = s.metrics();
        let g = &self.geo;
        let lh = s.line_height(Font::Normal);
        if !st.loaded || self.results.is_empty() {
            let msg = if st.loaded { labels().none } else { labels().loading };
            let w = s.text_width(msg, Font::Normal);
            let x = g.list.left + (g.list.right - g.list.left - w) / 2;
            s.text(msg, x, g.list.top + m.px(24), Font::Normal, WHITE, TEXT3_ALPHA);
            return;
        }
        let (ri, rpad) = (m.px(ROW_ICON), m.px(ROW_PAD));
        let radius = m.px(ROW_RADIUS);
        for k in 0..g.rows {
            let i = self.scroll + k;
            let Some(app) = self.results.get(i).and_then(|&idx| st.apps.get(idx)) else { break };
            let rc = self.row_rect(k);
            let id = ID_ROW + i as u32;
            let mut lit = row_plate(s, rc, id, hot, pressed);
            if !lit && self.sel == Some(i) {
                s.plate(rc, radius, HOVER_ALPHA);
                lit = true;
            }
            Self::paint_icon(s, &st.icons, &app.id, rc.left + rpad, rc.top + (g.row_h - ri) / 2, ri);
            let tx = rc.left + rpad + ri + m.px(12);
            let name = s.ellipsize(&app.name, Font::Normal, rc.right - rpad - tx);
            s.text(&name, tx, center_y(&rc, lh), Font::Normal, WHITE, lift(TEXT_ALPHA, lit));
        }
        // Scroll bar.
        let n = self.results.len();
        if n > g.rows {
            let sbw = m.px(SCROLLBAR_W).max(2);
            let track = RECT { left: g.list.right - sbw, top: g.list.top, right: g.list.right, bottom: g.list.bottom };
            let th_total = track.bottom - track.top;
            s.fill_rr(track, sbw / 2, WHITE, 14);
            let thumb_h = (th_total as i64 * g.rows as i64 / n as i64).max(m.px(24) as i64) as i32;
            let max_scroll = (n - g.rows) as i64;
            let ty = track.top + ((th_total - thumb_h) as i64 * self.scroll as i64 / max_scroll.max(1)) as i32;
            s.fill_rr(rect_wh(track.left, ty, sbw, thumb_h), sbw / 2, WHITE, 90);
        }
    }
}

impl Content for StartMenu {
    fn kind(&self) -> PopupKind {
        PopupKind::StartMenu
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        self.m = s.metrics();
        if self.horiz {
            let bw = |t: &str| s.text_width(t, Font::Small) + 2 * self.m.px(H_BTN_PAD);
            self.button_w = (bw(labels().all_button), bw(labels().back));
        }
        if !self.requested {
            self.requested = true;
            request_catalog(self.m.px(STORE_ICON).max(8) as u32);
        }
        self.refresh();
        (self.geo.w, self.geo.h)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        self.paint_search(s);
        STORE.with(|st| match st.try_borrow() {
            Ok(st) if self.horiz => self.paint_body_h(s, &st, hot, pressed),
            Ok(st) => self.paint_body(s, &st, hot, pressed),
            Err(_) => log!("startmenu: store busy while painting"),
        });
        self.paint_flyout(s, hot, pressed);
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn clickable(&self, id: u32) -> bool {
        id != ID_SEARCH && id != ID_FLY_BACKDROP
    }

    fn click(&mut self, id: u32) -> Reply {
        match id {
            ID_ALL => return self.show_all(!self.all),
            // The power flyout opens over the menu (the Start stays open).
            ID_POWER => return self.set_flyout(!self.flyout),
            // A click elsewhere while the flyout is open closes only the flyout.
            ID_FLY_BACKDROP => return self.set_flyout(false),
            _ if self.flyout && (ID_FLY..ID_PIN).contains(&id) => return self.choose_power(id - ID_FLY),
            _ if (ID_DOT..ID_DOT + MAX_DOTS as u32).contains(&id) => {
                self.page = (id - ID_DOT) as usize;
                self.refresh();
                return Reply::Redraw;
            }
            _ => {}
        }
        match self.app_at(id) {
            Some(idx) => self.choose(idx),
            None => Reply::None,
        }
    }

    fn button(&mut self, id: u32, which: Button) -> Reply {
        if self.flyout {
            // Any other button over the menu closes the flyout, nothing else.
            return self.set_flyout(false);
        }
        if which != Button::Right {
            return Reply::None;
        }
        let Some(app) = self.app_at(id).and_then(Self::app_id) else { return Reply::None };
        if let Err(e) = crate::config::toggle_start_pin(&app) {
            log!("startmenu: cannot save the pin of '{app}': {e}");
            return Reply::None;
        }
        log!("startmenu: pin toggled for '{app}'");
        self.refresh();
        Reply::Redraw
    }

    fn wheel(&mut self, notches: i32, _x: i32, _y: i32) -> Reply {
        if self.flyout {
            // The menu behind the flyout stays still.
            return Reply::None;
        }
        if self.horiz && !self.list_mode() {
            // Pinned page: one page per notch.
            let next = (self.page as i64 - notches as i64).clamp(0, self.pages() as i64 - 1) as usize;
            if next == self.page {
                return Reply::None;
            }
            self.page = next;
            self.refresh();
            return Reply::Redraw;
        }
        let next = (self.scroll as i64 - notches as i64 * WHEEL_ROWS).clamp(0, self.max_scroll() as i64) as usize;
        if next == self.scroll {
            return Reply::None;
        }
        self.scroll = next;
        self.rebuild_hits();
        Reply::Redraw
    }

    fn key(&mut self, vk: u16) -> Option<Reply> {
        if self.flyout {
            // Esc closes only the flyout (handled: the Start stays open); other keys wait for it.
            return Some(if vk == VK_ESCAPE { self.set_flyout(false) } else { Reply::None });
        }
        if self.horiz && !self.list_mode() && matches!(vk, VK_DOWN | VK_UP | VK_NEXT | VK_PRIOR | VK_HOME | VK_END) {
            // Horizontal pinned page: no list to move in.
            return Some(Reply::None);
        }
        let page = self.geo.rows.max(1) as i64;
        Some(match vk {
            VK_DOWN => self.move_sel(1),
            VK_UP => self.move_sel(-1),
            VK_NEXT => self.move_sel(page),
            VK_PRIOR => self.move_sel(-page),
            VK_HOME => self.move_sel(i64::MIN / 2),
            VK_END => self.move_sel(i64::MAX / 2),
            VK_RETURN => {
                let target = self.sel.or((self.searching() && !self.results.is_empty()).then_some(0));
                match target.and_then(|i| self.results.get(i).copied()) {
                    Some(idx) => self.choose(idx),
                    None => Reply::None,
                }
            }
            VK_BACK => {
                if self.query.pop().is_none() {
                    return Some(Reply::None);
                }
                self.query_changed()
            }
            // Esc first clears the search; with an empty search it closes (not handled here).
            VK_ESCAPE if !self.query.is_empty() => {
                self.query.clear();
                self.query_changed()
            }
            // Horizontal: Esc on the "All apps" page goes back to the pinned page.
            VK_ESCAPE if self.horiz && self.all => self.show_all(false),
            _ => return None,
        })
    }

    fn char(&mut self, c: char) -> Reply {
        if c.is_control() || (c.is_whitespace() && self.query.is_empty()) || self.query.chars().count() >= MAX_QUERY {
            return Reply::None;
        }
        // Typing closes the flyout and goes on searching.
        self.flyout = false;
        self.query.push(c);
        self.query_changed()
    }

    fn data(&mut self, ev: &DataEvent) -> Reply {
        if !matches!(ev, DataEvent::StartMenuChanged) {
            return Reply::None;
        }
        self.refresh();
        Reply::Redraw
    }

    fn closing(&mut self) {
        self.flyout = false;
    }

    fn after_close(&mut self) {
        if let Some(a) = self.power_pending.take() {
            popup_power::run(a);
            return;
        }
        if let Some(id) = self.pending.take() {
            log!("startmenu: launching '{id}'");
            crate::ui::launch_target_str(&format!("shell:AppsFolder\\{id}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics { dpi: 96 };

    #[test]
    fn search_ranks_prefix_then_word_then_substring() {
        let names = ["adobe reader", "calculator", "git bash", "notepad", "visual studio code", "windows terminal"];
        // "ca": prefix of calculator only.
        assert_eq!(filter(&names, "Ca"), vec![1]);
        // "te": word start of "terminal" (rank 1) before substrings ("notepad", rank 2).
        assert_eq!(filter(&names, "te"), vec![5, 3]);
        // Every word must match, in any order.
        assert_eq!(filter(&names, "code vis"), vec![4]);
        assert!(filter(&names, "zzz").is_empty());
        // Empty (or blank) query: everything, in the original order.
        assert_eq!(filter(&names, "  "), (0..names.len()).collect::<Vec<_>>());
        assert_eq!(rank("visual studio code", "visual"), Some(0));
        assert_eq!(rank("visual studio code", "studio"), Some(1));
        assert_eq!(rank("visual studio code", "udio"), Some(2));
    }

    #[test]
    fn layout_without_pins_gives_the_list_more_rows() {
        let none = layout(M, 0);
        let some = layout(M, 3);
        let full = layout(M, 40);
        assert!(none.pins_header.is_none() && none.tiles.is_empty());
        assert_eq!(some.tiles.len(), 3);
        assert_eq!(full.tiles.len(), COLS * MAX_PIN_ROWS);
        assert!(none.rows > some.rows && some.rows > full.rows && full.rows >= 1);
        // Fixed panel size; everything stays inside it.
        for g in [&none, &some, &full] {
            assert_eq!((g.w, g.h), (W, H));
            assert!(g.list.bottom <= g.h && g.list.top + g.rows as i32 * g.row_h <= g.list.bottom);
            assert!(g.tiles.iter().all(|t| t.right <= g.w - INSET && t.bottom <= g.list_header.top));
        }
        // Tiles: one row of 6, then the 7th starts the second row.
        let seven = layout(M, 7);
        assert_eq!(seven.tiles[6].left, seven.tiles[0].left);
        assert_eq!(seven.tiles[6].top, seven.tiles[0].bottom);
    }

    #[test]
    fn horizontal_layout_pages_and_stays_inside() {
        // One full page, no dots.
        let one = layout_h(M, 18, 0, false, Some(90));
        assert_eq!((one.w, one.h), (HW, HH));
        assert_eq!(one.tiles.len(), H_PAGE);
        assert!(one.dots.is_empty() && !one.list_shown);
        // 6 columns x 3 rows: the 7th tile starts the second row, the last is bottom right.
        assert_eq!(one.tiles[6].left, one.tiles[0].left);
        assert_eq!(one.tiles[6].top, one.tiles[0].bottom);
        assert_eq!(one.tiles[17].bottom, one.grid.bottom);
        // 20 pins: two pages, the second one with 2 tiles, one dot per page.
        let p0 = layout_h(M, 20, 0, false, Some(90));
        let p1 = layout_h(M, 20, 1, false, Some(90));
        assert_eq!((p0.tiles.len(), p1.tiles.len()), (H_PAGE, 2));
        assert_eq!((p0.dots.len(), p1.dots.len()), (2, 2));
        // List page: no tiles, rows fill the body above the bottom bar.
        let list = layout_h(M, 0, 0, true, None);
        assert!(list.list_shown && list.tiles.is_empty() && list.button.is_none() && list.rows > 8);
        for g in [&one, &p0, &p1, &list] {
            let bar = g.bar.unwrap();
            let power = g.power.unwrap();
            assert!(g.grid.bottom <= bar.top && g.list.bottom <= bar.top);
            assert!(g.list.top + g.rows as i32 * g.row_h <= g.list.bottom);
            assert!(power.right <= g.w && power.top >= bar.top && power.bottom <= g.h);
            assert!(g.tiles.iter().all(|t| t.right <= g.w - H_INSET && t.top >= g.list_header.bottom));
            assert!(g.dots.iter().all(|d| d.left >= g.w - H_INSET && d.right <= g.w));
            if let Some(b) = g.button {
                assert!(b.right == g.list_header.right && b.top >= g.list_header.top && b.bottom <= g.list_header.bottom);
            }
        }
        let big = layout_h(Metrics { dpi: 144 }, 3, 0, false, None);
        assert_eq!((big.w, big.h), (HW * 3 / 2, HH * 3 / 2));
    }

    #[test]
    fn horizontal_menu_pages_by_wheel_and_toggles_all_apps() {
        let mut c = make();
        c.horiz = true;
        c.requested = true;
        c.button_w = (90, 70);
        c.pins = (0..40).collect();
        assert_eq!(c.pages(), 3);
        c.geo = layout_h(M, c.pins.len(), 0, false, Some(90));
        assert!(!c.list_mode());
        // Pin ids map through the page.
        c.page = 1;
        assert_eq!(c.app_at(ID_PIN), Some(H_PAGE));
        c.all = true;
        assert!(c.list_mode());
        // The vertical style never offsets the pins.
        let mut v = make();
        v.pins = vec![4, 5];
        v.page = 3;
        assert_eq!(v.app_at(ID_PIN + 1), Some(5));
    }

    #[test]
    fn power_flyout_sits_above_the_power_button_inside_the_panel() {
        for dpi in [96, 120, 144, 192] {
            let m = Metrics { dpi };
            let g = layout_h(m, 8, 0, false, Some(90));
            let power = g.power.unwrap();
            let f = layout_flyout(m, g.w, g.h, power);
            let p = f.panel;
            // Inside the popup.
            assert!(p.left >= 0 && p.top >= 0 && p.right <= g.w && p.bottom <= g.h, "dpi {dpi}: {p:?}");
            // Above the power button, right-aligned to it, over the body (not the bottom bar).
            assert!(p.bottom <= power.top && p.bottom >= power.top - m.px(FLY_GAP) - 1);
            assert_eq!(p.right, power.right);
            assert!(p.left < power.left && p.top > g.search.bottom);
            // One row per shared power item, in order, inside the flyout, the separator between
            // the system actions and the dock entries.
            let ids: Vec<u32> = f.rows.iter().map(|r| r.0).collect();
            let want: Vec<u32> = popup_power::SYSTEM_ITEMS.iter().chain(&popup_power::DOCK_ITEMS).copied().collect();
            assert_eq!(ids, want);
            for (_, rc) in &f.rows {
                assert!(rc.left >= p.left && rc.right <= p.right && rc.top >= p.top && rc.bottom <= p.bottom);
            }
            let k = popup_power::SYSTEM_ITEMS.len();
            assert!(f.rows[k - 1].1.bottom <= f.sep_y && f.sep_y < f.rows[k].1.top);
            // Every item maps to a choice (an action or Settings).
            assert!(want.iter().all(|&id| popup_power::choice(id).is_some()));
        }
        // A panel too small: the flyout is kept inside it anyway.
        let f = layout_flyout(M, 100, 120, rect_wh(60, 80, 36, 36));
        assert!(f.panel.left >= 0 && f.panel.top >= 0 && f.panel.right <= 100);
    }

    #[test]
    fn power_flyout_toggles_and_esc_closes_only_it() {
        let mut c = make();
        c.horiz = true;
        c.requested = true;
        c.button_w = (90, 70);
        c.refresh();
        assert!(!c.flyout);
        assert_eq!(c.click(ID_POWER), Reply::Redraw);
        assert!(c.flyout);
        // The backdrop covers the menu, the flyout rows sit on top of it.
        let (_, rc) = c.fly.rows[0];
        assert_eq!(c.hit(rc.left + 2, rc.top + 2), Some(ID_FLY + popup_power::SYSTEM_ITEMS[0]));
        assert_eq!(c.hit(c.geo.search.left + 2, c.geo.search.top + 2), Some(ID_FLY_BACKDROP));
        let pw = c.geo.power.unwrap();
        assert_eq!(c.hit(pw.left + 2, pw.top + 2), Some(ID_POWER));
        // Esc is handled (the Start does not close) and closes the flyout.
        assert_eq!(c.key(VK_ESCAPE), Some(Reply::Redraw));
        assert!(!c.flyout);
        assert_eq!(c.hit(c.geo.search.left + 2, c.geo.search.top + 2), Some(ID_SEARCH));
        // A click on the backdrop closes it; the power button toggles it.
        c.click(ID_POWER);
        assert_eq!(c.click(ID_FLY_BACKDROP), Reply::Redraw);
        assert!(!c.flyout);
        c.click(ID_POWER);
        c.click(ID_POWER);
        assert!(!c.flyout);
        // Settings switches; an action closes the Start and waits for `after_close`.
        c.click(ID_POWER);
        assert_eq!(c.click(ID_FLY + popup_power::ID_SETTINGS), Reply::Switch(PopupKind::Settings));
        assert_eq!(c.click(ID_FLY + popup_power::ID_LOCK), Reply::Close);
        assert_eq!(c.power_pending, Some(PowerAction::Lock));
        c.power_pending = None;
        // The vertical style has no power button: no flyout.
        let mut v = make();
        v.refresh();
        assert_eq!(v.click(ID_POWER), Reply::None);
        assert!(!v.flyout);
    }

    #[test]
    fn layout_scales_with_dpi() {
        let g = layout(Metrics { dpi: 144 }, 2);
        assert_eq!((g.w, g.h), (W * 3 / 2, H * 3 / 2));
    }

    #[test]
    fn magnifier_has_ring_and_handle() {
        assert!(magnifier(6.5, 1.9)); // top of the ring
        assert!(!magnifier(6.5, 6.5)); // hollow centre
        assert!(magnifier(12.0, 12.0)); // handle
        assert!(!magnifier(1.0, 15.0));
    }

    #[test]
    fn query_tail_is_kept_when_too_long() {
        let s = Surface::new(M).expect("surface");
        let long = "a very long search text that does not fit";
        let t = tail_fit(&s, long, 60);
        assert!(long.ends_with(&t) && !t.is_empty() && s.text_width(&t, Font::Normal) <= 60);
        assert_eq!(tail_fit(&s, "abc", 500), "abc");
    }
}
