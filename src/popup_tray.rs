//! Tray chevron popup with two modes.
//!
//!  * NORMAL: grid (max 6 columns) of the icons that are outside the bar: those behind the chevron in the last
//!    layout (`ui::tray_hidden_ids`, which also accounts for the width fallback) plus those that
//!    the user hid (`tray_host::snapshot_all`, `user_hidden`). Left/right/double/
//!    middle clicks are forwarded with `tray_host::click`; the popup closes before a left or right click
//!    (the app menu must be able to take the foreground).
//!  * CUSTOMIZE: list of ALL tray apps (16 px icon, short name, pill toggle
//!    "visible in the bar"). A click flips the choice (`tray_host::set_user_hidden`: atomic config
//!    write and immediate bar update) and the popup stays open. "Done" returns to the grid, or closes the popup when
//!    it was opened directly in this mode (`popup::next_tray_chooser`, from the Settings popup).

use std::sync::Arc;

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::icons::IconBitmap;
use crate::log;
use crate::popup::{
    center_y, hline, lift, row_plate, toggle_switch, Button, Content, DataEvent, HitMap, PopupKind, Reply,
};
use crate::surface::{rect_wh, Font, Surface, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, TEXT_COL, WHITE};
use crate::tray_host::{self, TrayClick, TrayView};

/// Maximum columns of the grid.
const MAX_COLS: usize = 6;
/// Visible rows of the chooser list (scroll with the wheel beyond that).
const MAX_ROWS: usize = 14;

const ID_DONE: u32 = 100_001;
const ROW_BASE: u32 = 200_000;

struct Cell {
    id: u64,
    bitmap: Option<Arc<IconBitmap>>,
}

struct Row {
    key: String,
    name: String,
    bitmap: Option<Arc<IconBitmap>>,
    /// true = visible in the bar (toggle on).
    visible: bool,
    /// true = visible and really in the bar (false if it did not fit the bar width).
    in_bar: bool,
}

pub(crate) struct TrayPopup {
    cells: Vec<Cell>,
    rows: Vec<Row>,
    /// First row shown of "Customize" (scrolling).
    scroll: usize,
    customize: bool,
    /// true if opened directly in CUSTOMIZE mode: "Done" closes the popup instead of showing the grid.
    direct: bool,
    hits: HitMap,
    /// Click to forward after closing.
    pending: Option<(u64, TrayClick)>,
    icon: i32,
    /// Cached geometry (physical px): separator, width, rectangle of the "empty" message.
    sep_y: i32,
    width: i32,
    empty_rc: Option<RECT>,
}

/// Icons outside the bar: those that the last layout put behind the chevron (`Layout::tray_hidden`)
/// and those hidden by the user, in snapshot order.
fn overflow_cells() -> Vec<Cell> {
    let hidden = crate::ui::tray_hidden_ids();
    tray_host::snapshot_all()
        .into_iter()
        .filter(|v| v.user_hidden || hidden.contains(&v.id))
        .map(|v| Cell { id: v.id, bitmap: v.bitmap })
        .collect()
}

/// Rows of the "Customize" list: all tray apps, sorted by name.
fn customize_rows() -> Vec<Row> {
    let behind = crate::ui::tray_hidden_ids();
    let mut rows: Vec<Row> = tray_host::snapshot_all()
        .into_iter()
        .map(|v: TrayView| Row {
            key: v.key,
            name: v.name,
            bitmap: v.bitmap,
            visible: !v.user_hidden,
            in_bar: !v.user_hidden && !behind.contains(&v.id),
        })
        .collect();
    rows.sort_by_key(|r| r.name.to_lowercase());
    rows
}

/// Creates the content (initial state).
pub(crate) fn new() -> Box<dyn Content> {
    Box::new(TrayPopup {
        cells: overflow_cells(),
        rows: Vec::new(),
        scroll: 0,
        customize: false,
        direct: false,
        hits: HitMap::default(),
        pending: None,
        icon: 16,
        sep_y: 0,
        width: 0,
        empty_rc: None,
    })
}

/// Creates the content directly in the "choose visible icons" mode (no overflow grid).
pub(crate) fn new_chooser() -> Box<dyn Content> {
    Box::new(TrayPopup {
        cells: Vec::new(),
        rows: customize_rows(),
        scroll: 0,
        customize: true,
        direct: true,
        hits: HitMap::default(),
        pending: None,
        icon: 16,
        sep_y: 0,
        width: 0,
        empty_rc: None,
    })
}

impl Content for TrayPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::TrayOverflow
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let (pad, cell) = (m.px(8), m.px(30));
        self.icon = m.px(16);
        self.hits.clear();
        self.empty_rc = None;
        if self.customize {
            let (w, row) = (m.px(264), m.px(34));
            self.width = w;
            let mut y = pad;
            self.scroll = self.scroll.min(self.rows.len().saturating_sub(MAX_ROWS));
            let end = self.rows.len().min(self.scroll + MAX_ROWS);
            for i in self.scroll..end {
                self.hits.add(ROW_BASE + i as u32, rect_wh(pad, y, w - 2 * pad, row));
                y += row;
            }
            if self.rows.is_empty() {
                self.empty_rc = Some(rect_wh(pad, y, w - 2 * pad, row));
                y += row;
            }
            self.sep_y = y + m.px(4);
            y += m.px(4) * 2 + m.px(1);
            self.hits.add(ID_DONE, rect_wh(pad, y, w - 2 * pad, m.px(32)));
            y += m.px(32);
            return (w, y + pad);
        }
        let n = self.cells.len();
        let slots = n.max(1);
        let cols = slots.clamp(1, MAX_COLS) as i32;
        let rows = slots.div_ceil(MAX_COLS) as i32;
        // Grid aligned to the left: the first icon's edge falls on the text column (20).
        let gx = m.px(TEXT_COL) - (cell - self.icon) / 2;
        let w = 2 * gx + cols * cell;
        self.width = w;
        for i in 0..n {
            let (c, r) = ((i % MAX_COLS) as i32, (i / MAX_COLS) as i32);
            self.hits.add(i as u32, rect_wh(gx + c * cell, pad + r * cell, cell, cell));
        }
        self.sep_y = -1;
        (w, pad * 2 + rows * cell)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let inset = m.px(8);
        let pad_text = m.px(12);
        if self.sep_y >= 0 {
            hline(s, inset + pad_text, self.width - inset - pad_text, self.sep_y);
        }
        if let Some(rc) = self.empty_rc {
            let txt = crate::i18n::tr("No tray apps", "Nessuna app nell'area di notifica");
            let y = center_y(&rc, s.line_height(Font::Normal));
            s.text(txt, rc.left + pad_text, y, Font::Normal, WHITE, TEXT3_ALPHA);
        }
        let ic = self.icon;
        if self.customize {
            let (pw, ph) = (m.px(32), m.px(18));
            for (i, row) in self.rows.iter().enumerate() {
                let id = ROW_BASE + i as u32;
                let Some(rc) = self.hits.rect(id) else { continue };
                let h = row_plate(s, rc, id, hot, pressed);
                let ix = rc.left + m.px(12);
                if let Some(b) = row.bitmap.as_ref() {
                    s.blit(b, ix, rc.top + ((rc.bottom - rc.top) - ic) / 2, ic, if row.visible { 255 } else { 150 });
                }
                let tx = ix + ic + m.px(8);
                let px = rc.right - m.px(12) - pw;
                // On but ended up behind the chevron (bar full): the suffix says so.
                let label =
                    if row.visible && !row.in_bar { format!("{} · {}", row.name, crate::i18n::tr("no room", "spazio insufficiente")) } else { row.name.clone() };
                let name = s.ellipsize(&label, Font::Normal, (px - m.px(10) - tx).max(0));
                let a = lift(if row.visible { TEXT_ALPHA } else { TEXT2_ALPHA }, h);
                let ty = center_y(&rc, s.line_height(Font::Normal));
                s.text(&name, tx, ty, Font::Normal, WHITE, a);
                toggle_switch(s, px, rc.top + ((rc.bottom - rc.top) - ph) / 2, row.visible);
            }
            if let Some(rc) = self.hits.rect(ID_DONE) {
                let h = row_plate(s, rc, ID_DONE, hot, pressed);
                let y = center_y(&rc, s.line_height(Font::Normal));
                s.text(crate::i18n::tr("Done", "Fine"), rc.left + pad_text, y, Font::Normal, WHITE, lift(TEXT_ALPHA, h));
            }
            return;
        }
        for (i, cell) in self.cells.iter().enumerate() {
            let Some(rc) = self.hits.rect(i as u32) else { continue };
            row_plate(s, rc, i as u32, hot, pressed);
            if let Some(b) = cell.bitmap.as_ref() {
                let x = rc.left + ((rc.right - rc.left) - ic) / 2;
                let y = rc.top + ((rc.bottom - rc.top) - ic) / 2;
                s.blit(b, x, y, ic, 255);
            }
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn click(&mut self, id: u32) -> Reply {
        match id {
            ID_DONE if self.direct => Reply::Close,
            ID_DONE => {
                self.customize = false;
                self.cells = overflow_cells();
                Reply::Relayout
            }
            i if i >= ROW_BASE => {
                let idx = (i - ROW_BASE) as usize;
                let Some(row) = self.rows.get_mut(idx) else { return Reply::None };
                // `set_user_hidden` writes the config and notifies the UI (the bar updates right away); here
                // the row is updated too, then `data(TrayChanged)` realigns the list.
                tray_host::set_user_hidden(&row.key, row.visible);
                row.visible = !row.visible;
                Reply::Redraw
            }
            _ => {
                let Some(cell) = self.cells.get(id as usize) else { return Reply::None };
                self.pending = Some((cell.id, TrayClick::Left));
                Reply::Close
            }
        }
    }

    fn button(&mut self, id: u32, which: Button) -> Reply {
        if self.customize || id >= ID_DONE {
            return Reply::None;
        }
        let Some(cell) = self.cells.get(id as usize) else { return Reply::None };
        match which {
            Button::Right => {
                self.pending = Some((cell.id, TrayClick::Right));
                Reply::Close
            }
            Button::Middle => {
                // The middle click does not open a menu: it is forwarded right away, the popup stays open.
                tray_host::click(cell.id, TrayClick::Middle, cursor());
                Reply::None
            }
        }
    }

    fn wheel(&mut self, notches: i32, _x: i32, _y: i32) -> Reply {
        let max = self.rows.len().saturating_sub(MAX_ROWS);
        if !self.customize || max == 0 {
            return Reply::None;
        }
        let old = self.scroll;
        self.scroll = (self.scroll as i32 - notches).clamp(0, max as i32) as usize;
        if self.scroll == old {
            Reply::None
        } else {
            Reply::Relayout
        }
    }

    fn data(&mut self, ev: &DataEvent) -> Reply {
        if !matches!(ev, DataEvent::TrayChanged) {
            return Reply::None;
        }
        if self.customize {
            self.rows = customize_rows();
        } else {
            self.cells = overflow_cells();
        }
        Reply::Relayout
    }

    fn after_close(&mut self) {
        if let Some((id, kind)) = self.pending.take() {
            tray_host::click(id, kind, cursor());
        }
    }
}

/// Current cursor position (screen coordinates).
fn cursor() -> POINT {
    let mut p = POINT::default();
    // SAFETY: `p` is writable.
    if let Err(e) = unsafe { GetCursorPos(&mut p) } {
        log!("popup_tray: GetCursorPos failed: {e}");
    }
    p
}

// PREVIEW-HOOK (used by `preview.rs`): content with fake data instead of `tray_host`.
// `cells` = (id, bitmap) of the grid; `rows` = (key, name, bitmap, visible) of "Customize".
pub(crate) fn preview_new(
    cells: Vec<(u64, Option<Arc<IconBitmap>>)>,
    rows: Vec<(String, String, Option<Arc<IconBitmap>>, bool)>,
    customize: bool,
) -> Box<dyn Content> {
    Box::new(TrayPopup {
        cells: cells.into_iter().map(|(id, bitmap)| Cell { id, bitmap }).collect(),
        rows: rows
            .into_iter()
            .map(|(key, name, bitmap, visible)| Row { key, name, bitmap, visible, in_bar: true })
            .collect(),
        scroll: 0,
        customize,
        direct: false,
        hits: HitMap::default(),
        pending: None,
        icon: 16,
        sep_y: 0,
        width: 0,
        empty_rc: None,
    })
}
