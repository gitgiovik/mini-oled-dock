//! NOTIFICATIONS popup. Data from `notif::snapshot()`; actions with `notif::remove`,
//! a click on a card = `ui::open_notification_app` (the app that sent it) + `notif::remove`,
//! `notif::clear_all`, `notif::toggle_dnd`; "Open notification center" = Win+N (SendInput with
//! `hook::INJECT_TAG`), "Notification settings" = `ms-settings:notifications` via
//! `shell_thread::request(Launch)`. On open `notif::set_popup_open(true)`, in `closing` false.

use windows::Win32::Foundation::RECT;

use crate::glyphs;
use crate::icons::{self, IconBitmap};
use crate::notif::{self, NotifAccess, NotifState};
use crate::popup::{
    center_y, hline, launch, lift, row_plate, toggle_switch, Content, DataEvent, HitMap, PopupKind, Reply,
};
use crate::surface::{
    rect_wh, Font, Surface, HOVER_ALPHA, PANEL_PAD, PRESS_ALPHA, ROW_RADIUS, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA,
    TEXT_COL, WHITE,
};

const ID_CLEAR: u32 = 1;
const ID_DND: u32 = 2;
const ID_OPEN_CENTER: u32 = 3;
const ID_SETTINGS: u32 = 4;
/// Card ids: `ID_CARD + index`; their X buttons: `ID_X + index`.
const ID_CARD: u32 = 1000;
const ID_X: u32 = 100_000;

/// Maximum height of the list area (logical px).
const LIST_MAX_H: i32 = 360;
/// Scroll per wheel notch (logical px).
const SCROLL_STEP: i32 = 48;
/// Side (logical px) of the app icon and gap between icon and name.
const ICON_SIZE: i32 = 16;
const ICON_GAP: i32 = 6;
/// Inner padding of the cards: 8 (plate inset) + 12 = text column at 20.
const CARD_PAD: i32 = 12;

/// Action to run once the popup is closed.
#[derive(Clone, PartialEq, Eq)]
enum After {
    WinN,
    Settings,
    /// A card was clicked: bring up the app that sent it (AppUserModelID, display name).
    OpenApp {
        aumid: String,
        app: String,
    },
}

/// A card already laid out (texts truncated/wrapped according to the width).
struct Card {
    id: u32,
    app: String,
    /// App icon already scaled to `p(ICON_SIZE)` px; `None` = placeholder.
    icon: Option<IconBitmap>,
    time: String,
    title: String,
    body: Vec<String>,
    /// Ordinate relative to the start of the list (without scrolling) and height, physical px.
    y: i32,
    h: i32,
}

pub(crate) struct NotifPopup {
    state: NotifState,
    scroll: i32,
    after: Option<After>,
    // Cached geometry (physical px).
    hits: HitMap,
    cards: Vec<Card>,
    width: i32,
    pad: i32,
    y_header: i32,
    header_h: i32,
    sep1: i32,
    sep2: i32,
    list: RECT,
    total_h: i32,
    step: i32,
    margin: i32,
}

/// Creates the content (initial state).
pub(crate) fn new() -> Box<dyn Content> {
    notif::set_popup_open(true);
    notif::refresh_now();
    Box::new(NotifPopup {
        state: notif::snapshot(),
        scroll: 0,
        after: None,
        hits: HitMap::default(),
        cards: Vec::new(),
        width: 0,
        pad: 0,
        y_header: 0,
        header_h: 0,
        sep1: 0,
        sep2: 0,
        list: RECT::default(),
        total_h: 0,
        step: 0,
        margin: 0,
    })
}

/// Splits `text` into at most `max_lines` lines of width at most `max_w`, wrapping by word; the last
/// allowed line holds the whole remainder truncated with "…".
fn wrap_lines(s: &Surface, text: &str, font: Font, max_w: i32, max_lines: usize) -> Vec<String> {
    let mut words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < words.len() {
        if lines.len() + 1 >= max_lines {
            let rest = words[i..].join(" ");
            let all = if cur.is_empty() { rest } else { format!("{cur} {rest}") };
            lines.push(s.ellipsize(&all, font, max_w));
            return lines;
        }
        let cand = if cur.is_empty() { words[i].clone() } else { format!("{cur} {}", words[i]) };
        if s.text_width(&cand, font) <= max_w {
            cur = cand;
            i += 1;
        } else if cur.is_empty() {
            // Word wider than the line: cut by characters.
            let chars: Vec<char> = words[i].chars().collect();
            let mut k = chars.len();
            while k > 1 && s.text_width(&chars[..k].iter().collect::<String>(), font) > max_w {
                k -= 1;
            }
            lines.push(chars[..k].iter().collect());
            if k >= chars.len() {
                i += 1;
            } else {
                words[i] = chars[k..].iter().collect();
            }
        } else {
            lines.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

impl NotifPopup {
    fn max_scroll(&self) -> i32 {
        (self.total_h - (self.list.bottom - self.list.top)).max(0)
    }
}

impl Content for NotifPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::Notifications
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let p = |v: i32| m.px(v);
        self.width = p(360);
        self.pad = p(PANEL_PAD);
        self.step = p(SCROLL_STEP);
        self.margin = p(8);
        self.hits.clear();
        self.cards.clear();

        let mut y = self.pad;
        self.y_header = y;
        self.header_h = p(24);
        if !self.state.items.is_empty() && self.state.access == NotifAccess::Allowed {
            let tw = s.text_width(crate::i18n::tr("Clear all", "Cancella tutto"), Font::Small);
            // Text right-aligned on the column (width - TEXT_COL); the plate sticks out 12 beyond the text.
            let rc = rect_wh(self.width - self.margin - tw - p(24), y, tw + p(24), self.header_h);
            self.hits.add(ID_CLEAR, rc);
        }
        y += self.header_h + p(6);
        self.sep1 = y;
        y += p(1) + p(6);

        // List.
        let list_top = y;
        let card_x = self.margin;
        let card_w = self.width - 2 * self.margin;
        let inner_w = card_w - 2 * p(CARD_PAD);
        let x_btn = p(24);
        let (lh_s, lh_b, lh_n) = (s.line_height(Font::Small), s.line_height(Font::Bold), s.line_height(Font::Normal));
        let icon_px = p(ICON_SIZE);
        let icon_gap = p(ICON_GAP);
        let line1 = lh_s.max(icon_px);
        let mut cy = 0;
        if self.state.access == NotifAccess::Allowed {
            for it in &self.state.items {
                let tw = s.text_width(&it.time_text, Font::Small);
                let app_max = (inner_w - icon_px - icon_gap - x_btn - tw - p(10)).max(p(20));
                let icon = notif::app_icon(&it.icon_key)
                    .map(|b| icons::resize_area(&b, icon_px as u32))
                    .filter(|b| !b.is_empty());
                let body = if it.body.trim().is_empty() {
                    Vec::new()
                } else {
                    wrap_lines(s, &it.body, Font::Normal, inner_w, 2)
                };
                let mut h = p(10) + line1 + p(2) + lh_b + p(10);
                if !body.is_empty() {
                    h += p(2) + body.len() as i32 * lh_n;
                }
                self.cards.push(Card {
                    id: it.id,
                    app: s.ellipsize(&it.app, Font::Small, app_max),
                    icon,
                    time: it.time_text.clone(),
                    title: s.ellipsize(&it.title, Font::Bold, inner_w),
                    body,
                    y: cy,
                    h,
                });
                cy += h + p(6);
            }
            self.total_h = (cy - p(6)).max(0);
        } else {
            self.total_h = 0;
        }
        let view_h = if self.cards.is_empty() { p(48) } else { self.total_h.min(p(LIST_MAX_H)) };
        self.list = RECT { left: 0, top: list_top, right: self.width, bottom: list_top + view_h };
        self.scroll = self.scroll.clamp(0, self.max_scroll());

        // Clickable zones of the cards (clipped to the visible area).
        let (vt, vb) = (self.list.top, self.list.bottom);
        for (i, c) in self.cards.iter().enumerate() {
            let top = list_top + c.y - self.scroll;
            let bottom = top + c.h;
            if bottom <= vt || top >= vb {
                continue;
            }
            self.hits.add(
                ID_CARD + i as u32,
                RECT { left: card_x, top: top.max(vt), right: card_x + card_w, bottom: bottom.min(vb) },
            );
            let xr = rect_wh(card_x + card_w - p(4) - x_btn, top + p(4), x_btn, x_btn);
            if xr.top >= vt && xr.bottom <= vb {
                self.hits.add(ID_X + i as u32, xr);
            }
        }
        y += view_h + p(6);
        self.sep2 = y;
        y += p(1) + p(6);

        let inset = self.margin;
        let row = p(34);
        if self.state.dnd.is_some() {
            self.hits.add(ID_DND, rect_wh(inset, y, self.width - 2 * inset, row));
            y += row;
        }
        self.hits.add(ID_OPEN_CENTER, rect_wh(inset, y, self.width - 2 * inset, row));
        y += row;
        self.hits.add(ID_SETTINGS, rect_wh(inset, y, self.width - 2 * inset, row));
        y += row + inset;
        (self.width, y)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let p = |v: i32| m.px(v);
        let (lh_s, lh_b, lh_n) = (s.line_height(Font::Small), s.line_height(Font::Bold), s.line_height(Font::Normal));
        let radius = p(ROW_RADIUS);

        // Header.
        let head = rect_wh(0, self.y_header, self.width, self.header_h);
        s.text(crate::i18n::tr("Notifications", "Notifiche"), p(TEXT_COL), center_y(&head, lh_b), Font::Bold, WHITE, TEXT_ALPHA);
        if let Some(rc) = self.hits.rect(ID_CLEAR) {
            row_plate(s, rc, ID_CLEAR, hot, pressed);
            let a = if hot == Some(ID_CLEAR) { TEXT_ALPHA } else { TEXT2_ALPHA };
            let tw = s.text_width(crate::i18n::tr("Clear all", "Cancella tutto"), Font::Small);
            s.text(crate::i18n::tr("Clear all", "Cancella tutto"), rc.right - p(12) - tw, center_y(&rc, lh_s), Font::Small, WHITE, a);
        }
        hline(s, p(TEXT_COL), self.width - p(TEXT_COL), self.sep1);

        // List.
        let msg_rc = rect_wh(0, self.list.top, self.width, self.list.bottom - self.list.top);
        if self.state.access != NotifAccess::Allowed {
            let (txt, font) = if self.state.access == NotifAccess::Unknown {
                (crate::i18n::tr("Loading\u{2026}", "Caricamento\u{2026}"), Font::Small)
            } else {
                (crate::i18n::tr("List unavailable", "Elenco non disponibile"), Font::Small)
            };
            let tw = s.text_width(txt, font);
            s.text(txt, (self.width - tw) / 2, center_y(&msg_rc, s.line_height(font)), font, WHITE, TEXT3_ALPHA);
        } else if self.cards.is_empty() {
            let txt = crate::i18n::tr("No notifications", "Nessuna notifica");
            let tw = s.text_width(txt, Font::Normal);
            s.text(txt, (self.width - tw) / 2, center_y(&msg_rc, lh_n), Font::Normal, WHITE, TEXT2_ALPHA);
        } else {
            s.set_clip(Some(self.list));
            let card_x = self.margin;
            let card_w = self.width - 2 * self.margin;
            let x_btn = p(24);
            for (i, c) in self.cards.iter().enumerate() {
                let top = self.list.top + c.y - self.scroll;
                if top + c.h <= self.list.top || top >= self.list.bottom {
                    continue;
                }
                let idc = ID_CARD + i as u32;
                let idx = ID_X + i as u32;
                let card_hot = hot == Some(idc) || hot == Some(idx);
                if card_hot {
                    s.plate(rect_wh(card_x, top, card_w, c.h), radius, HOVER_ALPHA);
                }
                let x0 = card_x + p(CARD_PAD);
                let mut y = top + p(10);
                let xr = rect_wh(card_x + card_w - p(4) - x_btn, top + p(4), x_btn, x_btn);
                let icon_px = p(ICON_SIZE);
                let line1 = lh_s.max(icon_px);
                match &c.icon {
                    Some(b) => s.blit(b, x0, y + (line1 - icon_px) / 2, icon_px, 255),
                    // Neutral placeholder until the icon arrives.
                    None => s.fill_rr(rect_wh(x0, y + (line1 - icon_px) / 2, icon_px, icon_px), p(4), WHITE, 40),
                }
                let ty = y + (line1 - lh_s) / 2;
                s.text(&c.app, x0 + icon_px + p(ICON_GAP), ty, Font::Small, WHITE, TEXT2_ALPHA);
                let tw = s.text_width(&c.time, Font::Small);
                s.text(&c.time, xr.left - p(2) - tw, ty, Font::Small, WHITE, TEXT3_ALPHA);
                y += line1 + p(2);
                s.text(&c.title, x0, y, Font::Bold, WHITE, TEXT_ALPHA);
                y += lh_b + p(2);
                for line in &c.body {
                    s.text(line, x0, y, Font::Normal, WHITE, TEXT2_ALPHA);
                    y += lh_n;
                }
                if card_hot {
                    if hot == Some(idx) {
                        let a = if pressed == Some(idx) { PRESS_ALPHA } else { HOVER_ALPHA };
                        s.plate(xr, radius, a);
                    }
                    let icon = p(16);
                    s.glyph(
                        xr.left + (x_btn - icon) / 2,
                        xr.top + (x_btn - icon) / 2,
                        icon,
                        TEXT_ALPHA,
                        &glyphs::close_shape,
                    );
                }
            }
            s.set_clip(None);
            // Scroll indicator.
            if self.max_scroll() > 0 {
                let vh = self.list.bottom - self.list.top;
                let bar_h = (vh * vh / self.total_h).max(p(20));
                let bar_y = self.list.top + (vh - bar_h) * self.scroll / self.max_scroll();
                let bw = p(3);
                s.fill_rr(rect_wh(self.width - p(7), bar_y, bw, bar_h), bw / 2, WHITE, 70);
            }
        }
        hline(s, p(TEXT_COL), self.width - p(TEXT_COL), self.sep2);

        // Do not disturb.
        if let (Some(rc), Some(dnd)) = (self.hits.rect(ID_DND), self.state.dnd) {
            let h = row_plate(s, rc, ID_DND, hot, pressed);
            s.text(crate::i18n::tr("Do not disturb", "Non disturbare"), rc.left + p(12), center_y(&rc, lh_n), Font::Normal, WHITE, lift(TEXT_ALPHA, h));
            let (sw_w, sw_h) = (p(32), p(18));
            toggle_switch(s, rc.right - p(12) - sw_w, rc.top + (rc.bottom - rc.top - sw_h) / 2, dnd);
        }
        for (id, label) in [
            (ID_OPEN_CENTER, crate::i18n::tr("Open notification center", "Apri centro notifiche")),
            (ID_SETTINGS, crate::i18n::tr("Notification settings", "Impostazioni notifiche")),
        ] {
            let Some(rc) = self.hits.rect(id) else { continue };
            let h = row_plate(s, rc, id, hot, pressed);
            s.text(label, rc.left + p(12), center_y(&rc, lh_n), Font::Normal, WHITE, lift(TEXT_ALPHA, h));
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn clickable(&self, _id: u32) -> bool {
        // Cards too: a click opens the app that sent the notification.
        true
    }

    fn click(&mut self, id: u32) -> Reply {
        match id {
            ID_CLEAR => {
                notif::clear_all();
                self.state.items.clear();
                self.scroll = 0;
                Reply::Relayout
            }
            ID_DND => {
                notif::toggle_dnd();
                // Optimistic flip: the confirmation arrives with WM_DOCK_NOTIF.
                self.state.dnd = self.state.dnd.map(|d| !d);
                Reply::Redraw
            }
            ID_OPEN_CENTER => {
                self.after = Some(After::WinN);
                Reply::Close
            }
            ID_SETTINGS => {
                self.after = Some(After::Settings);
                Reply::Close
            }
            i if i >= ID_X => {
                let idx = (i - ID_X) as usize;
                let Some(card) = self.cards.get(idx) else { return Reply::None };
                let nid = card.id;
                notif::remove(nid);
                self.state.items.retain(|it| it.id != nid);
                Reply::Relayout
            }
            i if i >= ID_CARD => {
                let idx = (i - ID_CARD) as usize;
                let Some(card) = self.cards.get(idx) else { return Reply::None };
                let nid = card.id;
                let Some(it) = self.state.items.iter().find(|it| it.id == nid) else { return Reply::None };
                // Like Windows: a notification that was acted upon leaves the list.
                self.after = Some(After::OpenApp { aumid: it.aumid.clone(), app: it.app.clone() });
                notif::remove(nid);
                Reply::Close
            }
            _ => Reply::None,
        }
    }

    fn wheel(&mut self, notches: i32, _x: i32, y: i32) -> Reply {
        if self.max_scroll() == 0 || y < self.list.top || y >= self.list.bottom {
            return Reply::None;
        }
        let old = self.scroll;
        self.scroll = (self.scroll - notches * self.step).clamp(0, self.max_scroll());
        if self.scroll == old {
            Reply::None
        } else {
            // The clickable zones of the cards depend on the scrolling.
            Reply::Relayout
        }
    }

    fn data(&mut self, ev: &DataEvent) -> Reply {
        if !matches!(ev, DataEvent::NotifChanged) {
            return Reply::None;
        }
        self.state = notif::snapshot();
        Reply::Relayout
    }

    fn closing(&mut self) {
        notif::set_popup_open(false);
    }

    fn after_close(&mut self) {
        match self.after.take() {
            Some(After::WinN) => crate::ui::send_win_n(),
            Some(After::Settings) => launch("ms-settings:notifications"),
            Some(After::OpenApp { aumid, app }) => crate::ui::open_notification_app(&aumid, &app),
            None => {}
        }
    }
}
