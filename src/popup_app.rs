//! App MENU popup (right click on an app item): header with the name, separator, then
//! "New window", "Pin to dock" / "Unpin from dock", "Close window(s)". Same style as the power
//! popup. The choice is communicated to the dock with `WM_DOCK_APP_MENU_CMD` AFTER it closes.
//!
//! Declared as a child module of `popup` (`#[path]`).

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::log;
use crate::msg::{ID_MENU_CLOSE_ALL, ID_MENU_NEW_WINDOW, ID_MENU_PIN, ID_MENU_UNPIN, WM_DOCK_APP_MENU_CMD};
use crate::popup::{center_y, hline, lift, row_plate, AppMenuInfo, Content, HitMap, PopupKind, Reply};
use crate::surface::{rect_wh, Font, Surface, TEXT2_ALPHA, TEXT_ALPHA, WHITE};

pub(crate) struct AppPopup {
    info: AppMenuInfo,
    hits: HitMap,
    /// Command to send after closing.
    pending: Option<u32>,
    /// Cached geometry (physical px).
    sep_y: i32,
    pad_text: i32,
    width: i32,
    /// Name already shortened.
    title: String,
}

/// Creates the content (initial state).
pub(crate) fn new(info: AppMenuInfo) -> Box<dyn Content> {
    Box::new(AppPopup {
        info,
        hits: HitMap::default(),
        pending: None,
        sep_y: 0,
        pad_text: 0,
        width: 0,
        title: String::new(),
    })
}

impl AppPopup {
    /// Rows in order: (id, text, enabled).
    fn rows(&self) -> Vec<(u32, String, bool)> {
        let mut v = Vec::new();
        if self.info.can_launch {
            v.push((ID_MENU_NEW_WINDOW, crate::i18n::tr("New window", "Nuova finestra").to_string(), true));
        }
        if self.info.pinned {
            v.push((ID_MENU_UNPIN, crate::i18n::tr("Unpin from dock", "Rimuovi dal dock").to_string(), true));
        } else {
            v.push((ID_MENU_PIN, crate::i18n::tr("Pin to dock", "Aggiungi al dock").to_string(), self.info.can_launch));
        }
        let close = match self.info.windows {
            0 | 1 => crate::i18n::tr("Close window", "Chiudi finestra").to_string(),
            n if crate::i18n::italian() => format!("Chiudi {n} finestre"),
            n => format!("Close {n} windows"),
        };
        v.push((ID_MENU_CLOSE_ALL, close, self.info.windows > 0));
        v
    }
}

impl Content for AppPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::AppMenu
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let (inset, row) = (m.px(8), m.px(34));
        self.pad_text = m.px(12);
        let rows = self.rows();
        // Width = widest text + padding, within min/max.
        let mut tw = s.text_width(&self.info.name, Font::Bold);
        for (_, t, _) in &rows {
            tw = tw.max(s.text_width(t, Font::Normal));
        }
        let w = (tw + 2 * self.pad_text + 2 * inset).clamp(m.px(190), m.px(300));
        self.width = w;
        self.title = s.ellipsize(&self.info.name, Font::Bold, w - 2 * inset - 2 * self.pad_text);
        self.hits.clear();
        let mut y = inset + row;
        self.sep_y = y + m.px(2);
        y += m.px(2) * 2 + m.px(1);
        for (id, _, enabled) in &rows {
            if *enabled {
                self.hits.add(*id, rect_wh(inset, y, w - 2 * inset, row));
            }
            y += row;
        }
        (w, y + inset)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let (inset, row) = (m.px(8), m.px(34));
        let head = rect_wh(inset, inset, self.width - 2 * inset, row);
        let hy = center_y(&head, s.line_height(Font::Bold));
        s.text(&self.title, inset + self.pad_text, hy, Font::Bold, WHITE, TEXT2_ALPHA);
        hline(s, inset + self.pad_text, self.width - inset - self.pad_text, self.sep_y);
        let mut y = self.sep_y + m.px(1) + m.px(2);
        for (id, label, enabled) in self.rows() {
            let rc = rect_wh(inset, y, self.width - 2 * inset, row);
            y += row;
            let h = enabled && row_plate(s, rc, id, hot, pressed);
            let alpha = if enabled { TEXT_ALPHA } else { TEXT2_ALPHA };
            let ty = center_y(&rc, s.line_height(Font::Normal));
            s.text(&label, rc.left + self.pad_text, ty, Font::Normal, WHITE, lift(alpha, h));
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn click(&mut self, id: u32) -> Reply {
        self.pending = Some(id);
        crate::popup::set_app_menu_key(self.info.key.clone());
        Reply::Close
    }

    fn after_close(&mut self) {
        if let Some(cmd) = self.pending.take() {
            // SAFETY: PostMessageW to the dock window; if it no longer exists it just fails.
            if let Err(e) =
                unsafe { PostMessageW(Some(crate::hwnd()), WM_DOCK_APP_MENU_CMD, WPARAM(cmd as usize), LPARAM(0)) }
            {
                log!("popup_app: PostMessageW(WM_DOCK_APP_MENU_CMD) failed: {e}");
            }
        }
    }
}
