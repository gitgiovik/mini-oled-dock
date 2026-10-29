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
//! (`config::toggle_start_pin`). No power buttons on purpose.
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

use crate::icons::IconBitmap;
use crate::log;
use crate::popup::{center_y, lift, row_plate, Button, Content, DataEvent, HitMap, PopupKind, Reply};
use crate::render::Metrics;
use crate::shell_thread::{self, ShellRequest, StartApp};
use crate::surface::{rect_wh, Font, Surface, HOVER_ALPHA, ROW_RADIUS, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, WHITE};

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

// ------------------------------------------------------------------ hit ids

const ID_SEARCH: u32 = 1;
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
    Geo { w, h, search, pins_header, tiles, list_header, list, row_h, rows }
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
}

const EN: Labels = Labels {
    placeholder: "Type to search apps",
    pinned: "Pinned",
    all: "All apps",
    results: "Results",
    hint: "Right-click to pin",
    loading: "Loading apps\u{2026}",
    none: "No apps found",
};

const IT: Labels = Labels {
    placeholder: "Scrivi per cercare un'app",
    pinned: "Aggiunte",
    all: "Tutte le app",
    results: "Risultati",
    hint: "Clic destro per fissare",
    loading: "Caricamento app\u{2026}",
    none: "Nessuna app trovata",
};

// ------------------------------------------------------------------ content

pub(crate) struct StartMenu {
    labels: &'static Labels,
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
}

/// Creates the content; labels in the user's UI language (like the Start quick-links menu).
pub(crate) fn new() -> Box<dyn Content> {
    let italian = crate::popup::popup_start::ui_is_italian();
    Box::new(make(if italian { &IT } else { &EN }))
}

fn make(labels: &'static Labels) -> StartMenu {
    StartMenu {
        labels,
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
    }
}

/// PREVIEW-HOOK: the menu over a mock catalog (`apps`: id, name, icon; the icons all of one size),
/// English labels, `query` already typed. Replaces the store; asks the shell thread for nothing.
pub(crate) fn preview_new(apps: Vec<(String, String, IconBitmap)>, query: &str) -> Box<dyn Content> {
    let px = apps.first().map_or(32, |a| a.2.width);
    STORE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            *s = Store { icon_px: px, ..Store::default() };
        }
    });
    store_apps(apps.iter().map(|(id, name, _)| StartApp { id: id.clone(), name: name.clone() }).collect());
    store_icons(px, apps.into_iter().map(|(id, _, b)| (id, Some(b))).collect());
    let mut c = make(&EN);
    c.requested = true;
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
        self.geo = layout(self.m, self.pins.len());
        self.pins.truncate(self.geo.tiles.len());
        self.scroll = self.scroll.min(self.max_scroll());
        if let Some(s) = self.sel {
            self.sel = if self.results.is_empty() { None } else { Some(s.min(self.results.len() - 1)) };
        }
        self.rebuild_hits();
    }

    fn max_scroll(&self) -> usize {
        self.results.len().saturating_sub(self.geo.rows)
    }

    /// Rectangle of the `k`-th visible row.
    fn row_rect(&self, k: usize) -> RECT {
        let g = &self.geo;
        let top = g.list.top + k as i32 * g.row_h;
        let right = g.list.right - self.m.px(SCROLLBAR_W + SCROLLBAR_GAP);
        RECT { left: g.list.left, top, right, bottom: top + g.row_h }
    }

    fn rebuild_hits(&mut self) {
        let rows: Vec<(u32, RECT)> = (0..self.geo.rows)
            .map(|k| (k, self.scroll + k))
            .take_while(|&(_, i)| i < self.results.len())
            .map(|(k, i)| (ID_ROW + i as u32, self.row_rect(k)))
            .collect();
        self.hits.clear();
        self.hits.add(ID_SEARCH, self.geo.search);
        for (i, rc) in self.geo.tiles.iter().enumerate().take(self.pins.len()) {
            self.hits.add(ID_PIN + i as u32, *rc);
        }
        for (id, rc) in rows {
            self.hits.add(id, rc);
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
            self.pins.get((id - ID_PIN) as usize).copied()
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
            s.text(self.labels.placeholder, tx, ty, Font::Normal, WHITE, TEXT3_ALPHA);
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

    fn paint_body(&self, s: &mut Surface, st: &Store, hot: Option<u32>, pressed: Option<u32>) {
        let m = s.metrics();
        let g = &self.geo;
        // Pinned grid.
        if let Some(hh) = g.pins_header {
            Self::paint_header(s, hh, self.labels.pinned, None);
        }
        let ti = m.px(TILE_ICON);
        let lh_small = s.line_height(Font::Small);
        for (i, (rc, &idx)) in g.tiles.iter().zip(&self.pins).enumerate() {
            let Some(app) = st.apps.get(idx) else { continue };
            let plate = RECT { left: rc.left + m.px(2), top: rc.top, right: rc.right - m.px(2), bottom: rc.bottom };
            let lit = row_plate(s, plate, ID_PIN + i as u32, hot, pressed);
            let tw = rc.right - rc.left;
            let iy = rc.top + m.px(10);
            Self::paint_icon(s, &st.icons, &app.id, rc.left + (tw - ti) / 2, iy, ti);
            let name = s.ellipsize(&app.name, Font::Small, tw - m.px(10));
            let nx = rc.left + (tw - s.text_width(&name, Font::Small)) / 2;
            let ny = iy + ti + m.px(8);
            debug_assert!(ny + lh_small <= rc.bottom + m.px(4));
            s.text(&name, nx, ny, Font::Small, WHITE, lift(TEXT_ALPHA, lit));
        }
        // App list.
        let title = if self.searching() { self.labels.results } else { self.labels.all };
        Self::paint_header(s, g.list_header, title, Some(self.labels.hint));
        let lh = s.line_height(Font::Normal);
        if !st.loaded || self.results.is_empty() {
            let msg = if st.loaded { self.labels.none } else { self.labels.loading };
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
            Ok(st) => self.paint_body(s, &st, hot, pressed),
            Err(_) => log!("startmenu: store busy while painting"),
        });
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn clickable(&self, id: u32) -> bool {
        id != ID_SEARCH
    }

    fn click(&mut self, id: u32) -> Reply {
        match self.app_at(id) {
            Some(idx) => self.choose(idx),
            None => Reply::None,
        }
    }

    fn button(&mut self, id: u32, which: Button) -> Reply {
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
        let next = (self.scroll as i64 - notches as i64 * WHEEL_ROWS).clamp(0, self.max_scroll() as i64) as usize;
        if next == self.scroll {
            return Reply::None;
        }
        self.scroll = next;
        self.rebuild_hits();
        Reply::Redraw
    }

    fn key(&mut self, vk: u16) -> Option<Reply> {
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
            _ => return None,
        })
    }

    fn char(&mut self, c: char) -> Reply {
        if c.is_control() || (c.is_whitespace() && self.query.is_empty()) || self.query.chars().count() >= MAX_QUERY {
            return Reply::None;
        }
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

    fn after_close(&mut self) {
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
