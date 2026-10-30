//! WINDOW PICKER: when the pointer rests on an app item that has two or more windows, a panel opens
//! above the item (below it with the bar at the top) with one card per window: the window title
//! (one line, ellipsized) and, under it, a LIVE DWM thumbnail of the window. Clicking a card
//! switches to that window (`apps::activate_window`) and closes the picker.
//!
//! WINDOW: class `DockPicker`, `WS_POPUP`, `WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`,
//! owner = dock. NOT layered: the DWM does not compose thumbnails onto a layered window drawn with
//! `UpdateLayeredWindow`, so background, titles and the hover plate are drawn into a
//! `surface::Surface` (same fonts, colours and plates as the popups) and copied to the window DC with
//! `BitBlt` in `WM_PAINT`. The background is the config `background` colour, OPAQUE: `bar_opacity`
//! does not apply (a non-layered window has no per-pixel alpha). Corners rounded by the DWM
//! (`DWMWCP_ROUND`, Windows 11; square elsewhere). ONE window, created on the first open (class
//! registered then), hidden and reused; with `window_previews` off it is never created.
//!
//! THUMBNAILS: one `DwmRegisterThumbnail(picker, source)` per card at open, sized from
//! `DwmQueryThumbnailSourceSize` (aspect ratio) and placed with `DwmUpdateThumbnailProperties`.
//! EVERY close unregisters all of them: no thumbnail handle lives while the picker is closed. The
//! pixels never enter this process (the DWM composes them). A window whose registration fails gets a
//! title-only card: the narrowest width, an empty thumbnail box.
//!
//! TITLES: `InternalGetWindowText` (sends no message: a hung app cannot block the UI), read at open
//! and kept only while the picker is open. The bar itself never shows window titles.
//!
//! LIFETIME: `ui` arms `TIMER_HOVER_PICKER` (one-shot) when the hover lands on an app item with 2+
//! windows and calls `open` when it fires. Closing: the pointer away from both the item and the
//! picker for `LEAVE_MS` (`TIMER_PICKER_LEAVE`, one-shot on this window, re-checks the pointer before
//! closing), a card clicked, a click on the bar, a popup opening (`popup::open`), the bar hiding, the
//! hover moving to another item, the app's window list changing (`sync_windows`), previews switched
//! off. No timer runs while the picker is closed and the pointer is not resting on such an item.
//!
//! THREADING: UI thread only. State is thread-local and taken with `try_borrow_mut`; no borrow is
//! held across calls that pump messages (`SetWindowPos`, `ShowWindow`, `activate_window`).

use std::cell::RefCell;

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmSetWindowAttribute, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWM_THUMBNAIL_PROPERTIES,
    DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, InternalGetWindowText, IsWindow, KillTimer,
    LoadCursorW, RegisterClassExW, SetCursor, SetTimer, SetWindowPos, ShowWindow, UnregisterClassW, HTCLIENT,
    HWND_TOPMOST, IDC_ARROW, IDC_HAND, MA_NOACTIVATE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WM_ERASEBKGND,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR, WM_TIMER, WNDCLASSEXW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::log;
use crate::msg::TIMER_PICKER_LEAVE;
use crate::popup::{center_y, lift, row_plate, Anchor};
use crate::render::Metrics;
use crate::surface::{rect_wh, Font, Surface, TEXT_ALPHA, WHITE};

/// Pointer away from item and picker for this long = close (ms).
pub const LEAVE_MS: u32 = 300;

/// Outer inset of the panel (logical px).
const PAD: i32 = 8;
/// Inner padding of a card (logical px).
const CARD_PAD: i32 = 8;
/// Gap between two cards (logical px).
const GAP: i32 = 4;
/// Height of the title line of a card (logical px).
const TITLE_H: i32 = 22;
/// Thumbnail height (logical px) when the cards fit; it shrinks down to `THUMB_MIN_H` otherwise.
const THUMB_H: i32 = 120;
/// Thumbnail width limits at `THUMB_H` (logical px), scaled with the height.
const THUMB_MIN_W: i32 = 120;
const THUMB_MAX_W: i32 = 260;
/// Below this thumbnail height (logical px) the picker becomes a title-only column.
const THUMB_MIN_H: i32 = 60;
/// Row height and text padding of the title-only column (logical px, like the popup rows).
const ROW_H: i32 = 34;
const ROW_TEXT_PAD: i32 = 12;
/// Width limits of the title-only column (logical px).
const COLUMN_MIN_W: i32 = 200;
const COLUMN_MAX_W: i32 = 420;
/// Aspect ratio assumed when the source size is unknown (16:10).
const DEFAULT_ASPECT: (i32, i32) = (16, 10);

/// `WM_MOUSELEAVE` (defined here to avoid depending on the feature that exports it).
const WM_MOUSELEAVE: u32 = 0x02A3;

const CLASS_NAME: PCWSTR = w!("DockPicker");

// ------------------------------------------------------------------ layout (pure)

/// Rectangles of one card (client coordinates of the picker, physical px).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CardRects {
    /// Whole card (hit area and hover plate).
    pub card: RECT,
    /// Title line.
    pub title: RECT,
    /// Thumbnail box; `None` in the title-only column.
    pub thumb: Option<RECT>,
}

/// Picker layout: size of the window and one `CardRects` per window.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Geometry {
    pub w: i32,
    pub h: i32,
    /// true = title-only column fallback (the cards did not fit even at `THUMB_MIN_H`).
    pub column: bool,
    pub cards: Vec<CardRects>,
}

/// Thumbnail width (physical px) for height `th`: from the source aspect ratio (`DEFAULT_ASPECT` if
/// unknown, i.e. `Some` of a non-positive size; `None` = no thumbnail: the narrowest width),
/// clamped to `THUMB_MIN_W..=THUMB_MAX_W` scaled by `th / THUMB_H`.
fn thumb_width(m: Metrics, th: i32, src: Option<(i32, i32)>) -> i32 {
    let full = m.px(THUMB_H).max(1);
    let lo = m.px(THUMB_MIN_W) * th / full;
    let Some(src) = src else { return lo };
    let (sw, sh) = Some(src).filter(|&(w, h)| w > 0 && h > 0).unwrap_or(DEFAULT_ASPECT);
    let hi = m.px(THUMB_MAX_W) * th / full;
    let w = ((th as i64 * sw as i64 + sh as i64 / 2) / sh as i64) as i32;
    w.clamp(lo, hi.max(lo))
}

/// Width of a row of cards with thumbnails of height `th`.
fn row_width(m: Metrics, th: i32, sources: &[Option<(i32, i32)>]) -> i32 {
    let cards: i32 = sources.iter().map(|&s| thumb_width(m, th, s) + 2 * m.px(CARD_PAD)).sum();
    2 * m.px(PAD) + cards + m.px(GAP) * (sources.len() as i32 - 1).max(0)
}

/// Lays out the cards for the source sizes `sources` (physical px, `None` = unknown) within `max_w`
/// physical px: one row with thumbnails `THUMB_H` high, shrunk (1 px at a time) until the row fits;
/// below `THUMB_MIN_H` a title-only column `column_w` wide (clamped to `COLUMN_MIN_W..=COLUMN_MAX_W`
/// and to `max_w`). The column holds at most as many rows as fit in `max_h` (physical px): the
/// remaining windows get no row.
pub(crate) fn layout(
    m: Metrics,
    sources: &[Option<(i32, i32)>],
    max_w: i32,
    max_h: i32,
    column_w: i32,
) -> Geometry {
    let (pad, cpad, gap, title_h) = (m.px(PAD), m.px(CARD_PAD), m.px(GAP), m.px(TITLE_H));
    let min_h = m.px(THUMB_MIN_H);
    let mut th = m.px(THUMB_H);
    while th >= min_h && row_width(m, th, sources) > max_w {
        th -= 1;
    }
    if th >= min_h {
        let mut cards = Vec::with_capacity(sources.len());
        let mut x = pad;
        for &s in sources {
            let tw = thumb_width(m, th, s);
            let card = rect_wh(x, pad, tw + 2 * cpad, cpad + title_h + th + cpad);
            let title = rect_wh(x + cpad, pad + cpad, tw, title_h);
            let thumb = rect_wh(x + cpad, pad + cpad + title_h, tw, th);
            cards.push(CardRects { card, title, thumb: Some(thumb) });
            x = card.right + gap;
        }
        let w = (x - gap + pad).max(2 * pad);
        let h = 2 * pad + cpad + title_h + th + cpad;
        return Geometry { w, h, column: false, cards };
    }
    let row = m.px(ROW_H);
    let w = column_w.clamp(m.px(COLUMN_MIN_W), m.px(COLUMN_MAX_W)).min(max_w.max(1));
    let tpad = m.px(ROW_TEXT_PAD);
    let n = sources.len().min(((max_h - 2 * pad) / row.max(1)).max(1) as usize);
    let cards = (0..n as i32)
        .map(|i| {
            let card = rect_wh(pad, pad + i * row, w - 2 * pad, row);
            let title = RECT { left: card.left + tpad, right: card.right - tpad, ..card };
            CardRects { card, title, thumb: None }
        })
        .collect();
    Geometry { w, h: 2 * pad + row * n as i32, column: true, cards }
}

/// Largest rectangle with the aspect ratio of `src` centered inside `bx` (the DWM stretches the
/// thumbnail to its destination, so the destination must keep the source's proportions).
pub(crate) fn fit(bx: RECT, src: (i32, i32)) -> RECT {
    let (bw, bh) = (bx.right - bx.left, bx.bottom - bx.top);
    let (sw, sh) = src;
    if sw <= 0 || sh <= 0 || bw <= 0 || bh <= 0 {
        return bx;
    }
    // Width-limited if the source is relatively wider than the box.
    let (w, h) = if sw as i64 * bh as i64 >= sh as i64 * bw as i64 {
        (bw, ((bw as i64 * sh as i64 + sw as i64 / 2) / sw as i64) as i32)
    } else {
        (((bh as i64 * sw as i64 + sh as i64 / 2) / sh as i64) as i32, bh)
    };
    let (x, y) = (bx.left + (bw - w) / 2, bx.top + (bh - h) / 2);
    rect_wh(x, y, w.max(1), h.max(1))
}

/// Window title made printable on one line: control characters (new lines, tabs) become spaces,
/// runs of spaces collapse, the ends are trimmed. Empty = `fallback` (the app name).
pub(crate) fn clean_title(raw: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if c == ' ' && (out.is_empty() || out.ends_with(' ')) {
            continue;
        }
        out.push(c);
    }
    let t = out.trim_end();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

/// Same set of windows (order ignored): the MRU order changes on every activation, not the set.
pub(crate) fn same_windows(a: &[isize], b: &[isize]) -> bool {
    let (mut a, mut b) = (a.to_vec(), b.to_vec());
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

// ------------------------------------------------------------------ painting

/// Draws the picker: opaque background (config `background`), the hover/press plate of card `hot`,
/// the titles (already ellipsized). The thumbnail boxes stay bare: the DWM composes over them.
pub(crate) fn paint(s: &mut Surface, g: &Geometry, titles: &[String], hot: Option<usize>, pressed: Option<usize>) {
    paint_inner(s, g, titles, hot, pressed, false);
}

/// `paint`; `preview` = also what the DWM adds on screen, for the offscreen preview: rounded corners
/// with a thin border, and dark framed boxes in place of the thumbnails.
fn paint_inner(
    s: &mut Surface,
    g: &Geometry,
    titles: &[String],
    hot: Option<usize>,
    pressed: Option<usize>,
    preview: bool,
) {
    let bg = crate::config::get().background.rgb();
    let m = s.metrics();
    let whole = rect_wh(0, 0, g.w, g.h);
    if preview {
        s.fill_rr(whole, m.px(8), bg, 255);
        s.stroke_rr(whole, m.px(8), m.px(1).max(1), crate::surface::PANEL_BORDER_ALPHA);
    } else {
        s.fill_rr(whole, 0, bg, 255);
    }
    let (hot, pressed) = (hot.map(|i| i as u32), pressed.map(|i| i as u32));
    let lh = s.line_height(Font::Normal);
    for (i, (c, t)) in g.cards.iter().zip(titles).enumerate() {
        let h = row_plate(s, c.card, i as u32, hot, pressed);
        s.text(t, c.title.left, center_y(&c.title, lh), Font::Normal, WHITE, lift(TEXT_ALPHA, h));
        if let (true, Some(tb)) = (preview, c.thumb) {
            let r = m.px(4);
            s.fill_rr(tb, r, (0x2a, 0x2d, 0x33), 255);
            s.stroke_rr(tb, r, m.px(1).max(1), 40);
        }
    }
}

/// Titles ellipsized to the title line of their card.
fn ellipsize_titles(s: &Surface, g: &Geometry, titles: &[String]) -> Vec<String> {
    g.cards.iter().zip(titles).map(|(c, t)| s.ellipsize(t, Font::Normal, c.title.right - c.title.left)).collect()
}

/// Width of the title-only column for `titles` (widest title + padding).
fn column_width(s: &Surface, titles: &[String]) -> i32 {
    let m = s.metrics();
    let tw = titles.iter().map(|t| s.text_width(t, Font::Normal)).max().unwrap_or(0);
    tw + 2 * m.px(ROW_TEXT_PAD) + 2 * m.px(PAD)
}

/// PREVIEW-HOOK: the picker for `titles` with placeholder thumbnails of `sources` and card `hot`
/// hovered, as premultiplied pixels `(w, h, pixels)`. No window, no DWM.
pub(crate) fn preview_pixels(
    m: Metrics,
    titles: &[&str],
    sources: &[Option<(i32, i32)>],
    max_w: i32,
    hot: Option<usize>,
) -> Option<(i32, i32, Vec<u32>)> {
    let mut s = Surface::new(m).ok()?;
    let titles: Vec<String> = titles.iter().map(|t| clean_title(t, crate::i18n::tr("Window", "Finestra"))).collect();
    let g = layout(m, sources, max_w, i32::MAX, column_width(&s, &titles));
    let titles = ellipsize_titles(&s, &g, &titles);
    s.resize(g.w, g.h).ok()?;
    paint_inner(&mut s, &g, &titles, hot, None, true);
    s.snapshot_pixels()
}

// ------------------------------------------------------------------ state

/// One window of the open picker.
struct Card {
    hwnd: isize,
    /// Ellipsized title.
    title: String,
    /// Registered DWM thumbnail (0 = none).
    thumb: isize,
    /// Source size from `DwmQueryThumbnailSourceSize` (physical px), if known.
    src: Option<(i32, i32)>,
}

/// The open picker.
struct Open {
    /// Group key of the app (`apps::*`).
    key: String,
    /// Anchor item rectangle (screen px): the pointer over it keeps the picker open.
    item: RECT,
    /// Window rectangle (screen px).
    rect: RECT,
    cards: Vec<Card>,
    geo: Geometry,
    surf: Surface,
    hot: Option<usize>,
    pressed: Option<usize>,
    tracking: bool,
}

#[derive(Default)]
struct State {
    hwnd: HWND,
    registered: bool,
    open: Option<Open>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Runs `f` on the state; `None` on an unexpected re-entry (logged, never a panic).
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| match s.try_borrow_mut() {
        Ok(mut g) => Some(f(&mut g)),
        Err(_) => {
            log!("picker: state re-entry (ignored)");
            None
        }
    })
}

/// true while the picker is open (the UI suspends auto-hide).
pub fn is_open() -> bool {
    STATE.with(|s| s.try_borrow().map(|g| g.open.is_some()).unwrap_or(false))
}

/// Group key of the app the open picker belongs to.
pub fn open_key() -> Option<String> {
    STATE.with(|s| s.try_borrow().ok().and_then(|g| g.open.as_ref().map(|o| o.key.clone())))
}

// ------------------------------------------------------------------ window

fn module() -> Option<HINSTANCE> {
    // SAFETY: no preconditions.
    match unsafe { GetModuleHandleW(PCWSTR::null()) } {
        Ok(m) => Some(HINSTANCE(m.0)),
        Err(e) => {
            log!("picker: GetModuleHandleW failed: {e}");
            None
        }
    }
}

/// The picker window: created (and its class registered) on the first call, then reused.
fn ensure_window() -> Option<HWND> {
    let (existing, registered) = with(|st| (st.hwnd, st.registered))?;
    // SAFETY: IsWindow accepts any handle, even null.
    if !existing.0.is_null() && unsafe { IsWindow(Some(existing)) }.as_bool() {
        return Some(existing);
    }
    let inst = module()?;
    if !registered {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: inst,
            // SAFETY: loads a system cursor (null hInstance), no pointers to manage.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        // SAFETY: `wc` is fully initialized; the class name is a static literal.
        if unsafe { RegisterClassExW(&wc) } == 0 {
            log!("picker: RegisterClassExW(DockPicker) failed");
            return None;
        }
        with(|st| st.registered = true);
    }
    let owner = crate::hwnd();
    let owner = if owner.0.is_null() { None } else { Some(owner) };
    // SAFETY: class registered above; static literals; the owner is the dock window (same thread).
    let r = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            CLASS_NAME,
            w!("Dock window picker"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            owner,
            None,
            Some(inst),
            None,
        )
    };
    let h = match r {
        Ok(h) => h,
        Err(e) => {
            log!("picker: CreateWindowExW failed: {e}");
            return None;
        }
    };
    let pref: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
    // SAFETY: `pref` lives for the call and has the size passed; fails harmlessly before Windows 11.
    let _ = unsafe {
        DwmSetWindowAttribute(
            h,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const DWM_WINDOW_CORNER_PREFERENCE as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    };
    with(|st| st.hwnd = h);
    Some(h)
}

/// Title of `h` without sending it any message.
fn window_title(h: isize) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is writable; InternalGetWindowText does not send messages to the window.
    let n = unsafe { InternalGetWindowText(HWND(h as *mut core::ffi::c_void), &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

impl Drop for Card {
    /// A card dropped with a live registration (failed install, replaced `Open`) releases it.
    fn drop(&mut self) {
        if self.thumb != 0 {
            // SAFETY: handle returned by DwmRegisterThumbnail and not yet unregistered (`unregister`
            // zeroes the field).
            let _ = unsafe { DwmUnregisterThumbnail(self.thumb) };
        }
    }
}

fn unregister(cards: &mut [Card]) {
    for c in cards.iter_mut().filter(|c| c.thumb != 0) {
        // SAFETY: handle returned by DwmRegisterThumbnail and not yet unregistered.
        if let Err(e) = unsafe { DwmUnregisterThumbnail(c.thumb) } {
            log!("picker: DwmUnregisterThumbnail failed: {e}");
        }
        c.thumb = 0;
    }
}

/// Opens the picker for the windows `windows` (stable order) of app `key`, anchored to `anchor`.
/// `name` replaces empty titles. No-op with fewer than 2 live windows or with previews off.
pub fn open(anchor: Anchor, key: &str, name: &str, windows: &[isize]) {
    close_inner("reopen");
    if !crate::config::get().window_previews {
        return;
    }
    // SAFETY: IsWindow accepts arbitrary HWNDs.
    let live: Vec<isize> = windows
        .iter()
        .copied()
        .filter(|&h| unsafe { IsWindow(Some(HWND(h as *mut core::ffi::c_void))) }.as_bool())
        .collect();
    if live.len() < 2 {
        return;
    }
    let Some(hwnd) = ensure_window() else { return };
    let m = Metrics { dpi: anchor.dpi.max(96) };
    let mut surf = match Surface::new(m) {
        Ok(s) => s,
        Err(e) => {
            log!("picker: Surface::new failed: {e}");
            return;
        }
    };
    let mut cards: Vec<Card> = live
        .iter()
        .map(|&h| {
            let src_hwnd = HWND(h as *mut core::ffi::c_void);
            // SAFETY: destination = our top-level window, source = a live top-level window.
            let thumb = match unsafe { DwmRegisterThumbnail(hwnd, src_hwnd) } {
                Ok(t) => t,
                Err(e) => {
                    log!("picker: DwmRegisterThumbnail({h:#x}) failed: {e}");
                    0
                }
            };
            // SAFETY: `thumb` is a handle registered just above.
            let src = (thumb != 0)
                .then(|| unsafe { DwmQueryThumbnailSourceSize(thumb) }.ok())
                .flatten()
                .map(|s| (s.cx, s.cy))
                .filter(|&(w, h)| w > 0 && h > 0);
            Card { hwnd: h, title: clean_title(&window_title(h), name), thumb, src }
        })
        .collect();
    let work = crate::popup::work_area(&anchor.bar);
    let max_w = (work.right - work.left) - 2 * m.px(crate::popup::POPUP_SCREEN_MARGIN);
    let raw: Vec<String> = cards.iter().map(|c| c.title.clone()).collect();
    // Registered but of unknown size = `(0, 0)` (default aspect); not registered = `None` (narrowest).
    let sources: Vec<Option<(i32, i32)>> =
        cards.iter().map(|c| (c.thumb != 0).then(|| c.src.unwrap_or((0, 0)))).collect();
    // Column height cap: the work area less the screen margins, the bar and its gap to the picker.
    let bar_h = anchor.bar.bottom - anchor.bar.top;
    let max_h = (work.bottom - work.top) - 2 * m.px(crate::popup::POPUP_SCREEN_MARGIN)
        - bar_h
        - m.px(crate::popup::POPUP_GAP);
    let geo = layout(m, &sources, max_w, max_h, column_width(&surf, &raw));
    if geo.column {
        // Title-only column: the thumbnails are not shown, so they are not kept either; windows
        // beyond the rows that fit are dropped from the picker.
        unregister(&mut cards);
        cards.truncate(geo.cards.len());
    }
    for (c, t) in cards.iter_mut().zip(ellipsize_titles(&surf, &geo, &raw)) {
        c.title = t;
    }
    if let Err(e) = surf.resize(geo.w, geo.h) {
        log!("picker: Surface::resize failed: {e}");
        unregister(&mut cards);
        return;
    }
    let titles: Vec<String> = cards.iter().map(|c| c.title.clone()).collect();
    paint(&mut surf, &geo, &titles, None, None);
    let (x, y) = crate::popup::compute_pos(&anchor, geo.w, geo.h);
    for (c, r) in cards.iter().zip(&geo.cards) {
        let (Some(bx), true) = (r.thumb, c.thumb != 0) else { continue };
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY | DWM_TNP_OPACITY,
            rcDestination: c.src.map(|s| fit(bx, s)).unwrap_or(bx),
            opacity: 255,
            fVisible: BOOL::from(true),
            fSourceClientAreaOnly: BOOL::from(false),
            ..Default::default()
        };
        // SAFETY: `props` lives for the call; `c.thumb` is registered.
        if let Err(e) = unsafe { DwmUpdateThumbnailProperties(c.thumb, &props) } {
            log!("picker: DwmUpdateThumbnailProperties failed: {e}");
        }
    }
    let n = cards.len();
    let installed = with(|st| {
        // Unregisters the thumbnails of a stale `Open` (`Card::drop`) rather than leaking them.
        drop(st.open.take());
        st.open = Some(Open {
            key: key.to_string(),
            item: anchor.item,
            rect: rect_wh(x, y, geo.w, geo.h),
            cards,
            geo: geo.clone(),
            surf,
            hot: None,
            pressed: None,
            tracking: false,
        })
    });
    if installed.is_none() {
        return;
    }
    log!("picker: open for '{key}' ({n} windows{})", if geo.column { ", titles only" } else { "" });
    // No borrow held: SetWindowPos delivers messages to the picker.
    // SAFETY: `hwnd` is our window; no pointers passed.
    if let Err(e) =
        unsafe { SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, geo.w, geo.h, SWP_NOACTIVATE | SWP_SHOWWINDOW) }
    {
        log!("picker: SetWindowPos failed: {e}");
    }
    invalidate(hwnd);
}

/// Closes the picker (no-op if closed). `reason` is for the log only.
pub fn close(reason: &str) {
    close_inner(reason);
}

fn close_inner(reason: &str) {
    let Some((hwnd, open)) = with(|st| (st.hwnd, st.open.take())) else { return };
    let Some(mut o) = open else { return };
    unregister(&mut o.cards);
    // SAFETY: `hwnd` is our window (the picker exists while `open` was Some); no pointers passed.
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_PICKER_LEAVE);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    log!("picker: closed ({reason})");
    // `o` (titles, surface) is dropped here.
}

/// The app's window list changed (`ui::rebuild_entries`): closes the picker if the open one no longer
/// matches `windows` (`None` = the app is gone). Simple and safe: no live re-layout.
pub fn sync_windows(key: &str, windows: Option<&[isize]>) {
    let stale = STATE.with(|s| {
        s.try_borrow().ok().and_then(|g| {
            g.open.as_ref().filter(|o| o.key == key).map(|o| {
                let cur: Vec<isize> = o.cards.iter().map(|c| c.hwnd).collect();
                windows.is_none_or(|w| !same_windows(&cur, w))
            })
        })
    });
    if stale == Some(true) {
        close_inner("window list changed");
    }
}

/// The pointer left the item or the picker: close in `LEAVE_MS` unless it comes back.
pub fn arm_leave() {
    let Some(Some(h)) = with(|st| st.open.as_ref().map(|_| st.hwnd)) else { return };
    // SAFETY: our window; without a callback WM_TIMER reaches `wnd_proc`.
    if unsafe { SetTimer(Some(h), TIMER_PICKER_LEAVE, LEAVE_MS, None) } == 0 {
        log!("picker: SetTimer(leave) failed");
    }
}

/// The pointer is back on the item or on the picker.
pub fn cancel_leave() {
    let Some(h) = with(|st| st.hwnd) else { return };
    if !h.0.is_null() {
        // SAFETY: our id/window; fails harmlessly if the timer does not exist.
        let _ = unsafe { KillTimer(Some(h), TIMER_PICKER_LEAVE) };
    }
}

fn invalidate(h: HWND) {
    // SAFETY: our window; whole client area, no erase.
    let _ = unsafe { InvalidateRect(Some(h), None, false) };
}

/// Destroys the picker window and unregisters its class. In `cleanup`, next to `popup::shutdown`.
/// Idempotent.
pub fn shutdown() {
    close_inner("shutdown");
    let Some((hwnd, registered)) = with(|st| {
        let r = (st.hwnd, st.registered);
        st.hwnd = HWND::default();
        st.registered = false;
        r
    }) else {
        return;
    };
    if !hwnd.0.is_null() {
        // SAFETY: window created by this module on the UI thread.
        if let Err(e) = unsafe { DestroyWindow(hwnd) } {
            log!("picker: DestroyWindow failed: {e}");
        }
    }
    if registered {
        // SAFETY: class registered by `ensure_window` with this module; no window of it survives.
        let _ = unsafe { UnregisterClassW(CLASS_NAME, module()) };
    }
}

// ------------------------------------------------------------------ messages

fn lo_i16(lp: LPARAM) -> i32 {
    (lp.0 & 0xFFFF) as u16 as i16 as i32
}

fn hi_i16(lp: LPARAM) -> i32 {
    ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32
}

fn card_at(o: &Open, x: i32, y: i32) -> Option<usize> {
    o.geo.cards.iter().position(|c| x >= c.card.left && x < c.card.right && y >= c.card.top && y < c.card.bottom)
}

/// Repaints the surface with the current hover/press state and invalidates the window.
fn repaint(h: HWND) {
    with(|st| {
        if let Some(o) = st.open.as_mut() {
            let titles: Vec<String> = o.cards.iter().map(|c| c.title.clone()).collect();
            paint(&mut o.surf, &o.geo, &titles, o.hot, o.pressed);
        }
    });
    invalidate(h);
}

fn on_mouse_move(h: HWND, x: i32, y: i32) {
    cancel_leave();
    let (track, changed) = with(|st| match st.open.as_mut() {
        Some(o) => {
            let hot = card_at(o, x, y);
            let changed = hot != o.hot;
            o.hot = hot;
            (!std::mem::replace(&mut o.tracking, true), changed)
        }
        None => (false, false),
    })
    .unwrap_or((false, false));
    if track {
        let mut t = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: h,
            dwHoverTime: 0,
        };
        // SAFETY: `t` is initialized with the correct cbSize; `h` is our window.
        if let Err(e) = unsafe { TrackMouseEvent(&mut t) } {
            log!("picker: TrackMouseEvent failed: {e}");
            with(|st| st.open.as_mut().map(|o| o.tracking = false));
        }
    }
    if changed {
        repaint(h);
    }
}

fn on_mouse_leave(h: HWND) {
    let was_open = with(|st| {
        st.open.as_mut().map(|o| {
            o.tracking = false;
            o.hot = None;
            o.pressed = None;
        })
    })
    .flatten()
    .is_some();
    if was_open {
        repaint(h);
        arm_leave();
    }
}

fn on_lbutton(h: HWND, x: i32, y: i32, down: bool) {
    let target = with(|st| {
        let o = st.open.as_mut()?;
        let hit = card_at(o, x, y);
        if down {
            o.pressed = hit;
            return None;
        }
        let was = o.pressed.take();
        hit.filter(|&i| was == Some(i)).map(|i| o.cards[i].hwnd)
    })
    .flatten();
    match target {
        Some(win) => {
            // Hide first (no flash of the picker over the activated window), then switch.
            close_inner("window chosen");
            crate::apps::activate_window(win);
        }
        None => repaint(h),
    }
}

/// `TIMER_PICKER_LEAVE`: closes unless the pointer is over the picker or the anchor item.
fn on_leave_timer(h: HWND) {
    // SAFETY: our id/window.
    let _ = unsafe { KillTimer(Some(h), TIMER_PICKER_LEAVE) };
    let mut p = POINT::default();
    // SAFETY: `p` is a valid, writable POINT.
    let have = unsafe { GetCursorPos(&mut p) }.is_ok();
    let inside = |r: &RECT| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
    let keep = with(|st| st.open.as_ref().is_some_and(|o| have && (inside(&o.rect) || inside(&o.item))));
    if keep == Some(false) {
        close_inner("pointer left");
    }
}

fn on_set_cursor(h: HWND, lp: LPARAM) -> Option<LRESULT> {
    if (lp.0 & 0xFFFF) as u32 != HTCLIENT {
        return None;
    }
    let over_card = STATE.with(|s| s.try_borrow().map(|g| g.open.as_ref().is_some_and(|o| o.hot.is_some())));
    if !over_card.unwrap_or(false) || h.0.is_null() {
        return None;
    }
    // SAFETY: loads a shared system cursor (null hInstance); no pointers to manage.
    let cur = unsafe { LoadCursorW(None, IDC_HAND) }.ok()?;
    // SAFETY: `cur` is a valid shared cursor.
    unsafe { SetCursor(Some(cur)) };
    Some(LRESULT(1))
}

fn on_paint(h: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: `h` is our window, inside WM_PAINT; `ps` is writable.
    let hdc = unsafe { BeginPaint(h, &mut ps) };
    if !hdc.is_invalid() {
        STATE.with(|s| {
            if let Ok(g) = s.try_borrow() {
                if let Some(o) = g.open.as_ref() {
                    o.surf.copy_to(hdc, 0, 0);
                }
            }
        });
    }
    // SAFETY: pairs the BeginPaint above.
    let _ = unsafe { EndPaint(h, &ps) };
}

unsafe extern "system" fn wnd_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        // The whole client area is painted from the surface.
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            on_paint(h);
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            on_mouse_move(h, lo_i16(lp), hi_i16(lp));
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            on_mouse_leave(h);
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            on_lbutton(h, lo_i16(lp), hi_i16(lp), msg == WM_LBUTTONDOWN);
            return LRESULT(0);
        }
        WM_SETCURSOR => {
            if let Some(r) = on_set_cursor(h, lp) {
                return r;
            }
        }
        WM_TIMER if wp.0 == TIMER_PICKER_LEAVE => {
            on_leave_timer(h);
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: standard forwarding of unhandled messages.
    unsafe { DefWindowProcW(h, msg, wp, lp) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics { dpi: 96 };

    #[test]
    fn widths_follow_the_aspect_ratio_within_limits() {
        // 16:9 at 120 px high = 213 px; a tall window clamps to 120, a very wide one to 260.
        assert_eq!(thumb_width(M, 120, Some((1920, 1080))), 213);
        assert_eq!(thumb_width(M, 120, Some((400, 1000))), 120);
        assert_eq!(thumb_width(M, 120, Some((4000, 500))), 260);
        // Unknown or empty source = 16:10.
        assert_eq!(thumb_width(M, 120, Some((0, 0))), 192);
        // No thumbnail (registration failed): the narrowest card.
        assert_eq!(thumb_width(M, 120, None), 120);
        // The limits scale with the height.
        assert_eq!(thumb_width(M, 60, Some((400, 1000))), 60);
    }

    #[test]
    fn row_layout_fits_at_full_height() {
        let g = layout(M, &[Some((1920, 1080)), Some((1280, 800))], 1900, 10_000, 0);
        assert!(!g.column);
        assert_eq!(g.cards.len(), 2);
        let t0 = g.cards[0].thumb.unwrap();
        assert_eq!((t0.right - t0.left, t0.bottom - t0.top), (213, 120));
        // Cards side by side with the gap, panel inset on both sides.
        assert_eq!(g.cards[1].card.left - g.cards[0].card.right, GAP);
        assert_eq!(g.w, g.cards[1].card.right + PAD);
        assert_eq!(g.h, 2 * PAD + 2 * CARD_PAD + TITLE_H + THUMB_H);
        // The title sits above the thumbnail, as wide as it.
        assert_eq!(g.cards[0].title.bottom, t0.top);
        assert_eq!(g.cards[0].title.right - g.cards[0].title.left, 213);
    }

    #[test]
    fn row_shrinks_then_falls_back_to_titles() {
        let src = vec![Some((1920, 1080)); 6];
        let full = row_width(M, THUMB_H, &src);
        // A bit narrower than the full row: smaller thumbnails, still a row, within the width.
        let g = layout(M, &src, full - 100, 10_000, 0);
        assert!(!g.column && g.w <= full - 100);
        let th = g.cards[0].thumb.map(|t| t.bottom - t.top).unwrap();
        assert!(th < THUMB_H && th >= THUMB_MIN_H);
        // Not even at the minimum height: title-only column, clamped to its limits.
        let narrow = row_width(M, THUMB_MIN_H, &src) - 1;
        let g = layout(M, &src, narrow, 10_000, 10_000);
        assert!(g.column && g.cards.iter().all(|c| c.thumb.is_none()));
        assert_eq!(g.w, COLUMN_MAX_W.min(narrow));
        assert_eq!(g.h, 2 * PAD + 6 * ROW_H);
        assert_eq!(layout(M, &src, narrow, 10_000, 50).w, COLUMN_MIN_W);
    }

    #[test]
    fn column_is_capped_by_the_available_height() {
        let src = vec![Some((1920, 1080)); 30];
        let narrow = row_width(M, THUMB_MIN_H, &src) - 1;
        // Room for exactly 5 rows.
        let g = layout(M, &src, narrow, 2 * PAD + 5 * ROW_H, 0);
        assert!(g.column);
        assert_eq!((g.cards.len(), g.h), (5, 2 * PAD + 5 * ROW_H));
        // One pixel short: 4 rows. Never fewer than one.
        assert_eq!(layout(M, &src, narrow, 2 * PAD + 5 * ROW_H - 1, 0).cards.len(), 4);
        assert_eq!(layout(M, &src, narrow, 0, 0).cards.len(), 1);
    }

    #[test]
    fn fit_keeps_the_source_proportions() {
        let bx = rect_wh(10, 20, 200, 100);
        // Wider source: full width, centered vertically.
        assert_eq!(fit(bx, (400, 100)), rect_wh(10, 45, 200, 50));
        // Taller source: full height, centered horizontally.
        assert_eq!(fit(bx, (100, 100)), rect_wh(60, 20, 100, 100));
        assert_eq!(fit(bx, (0, 5)), bx);
    }

    #[test]
    fn titles_are_cleaned_and_ellipsized() {
        assert_eq!(clean_title("  Inbox\r\n-\tMail  ", "App"), "Inbox - Mail");
        assert_eq!(clean_title("\n \t", "Firefox"), "Firefox");
        let s = Surface::new(M).expect("surface");
        let long = "A very long window title that cannot possibly fit in a small card";
        let short = s.ellipsize(long, Font::Normal, 120);
        assert!(short.ends_with('\u{2026}') && s.text_width(&short, Font::Normal) <= 120);
        assert_eq!(s.ellipsize("Short", Font::Normal, 120), "Short");
    }

    #[test]
    fn window_sets_ignore_order() {
        assert!(same_windows(&[1, 2, 3], &[3, 1, 2]));
        assert!(!same_windows(&[1, 2], &[1, 2, 3]));
        assert!(!same_windows(&[1, 2], &[1, 4]));
    }
}
