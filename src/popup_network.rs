//! NETWORK popup: header with the connection glyph, name (SSID or adapter) and kind/signal,
//! details (internet yes/no, local IPv4, Wi-Fi signal), "Network settings" link (`ms-settings:network`,
//! de-elevated through the shell thread). Data: `net::NetInfo` (last known on open, then
//! `DataEvent::Network` from the polls of the visible bar; a fresh `NetworkQuery` is sent on open).

use crate::glyphs;
use crate::net::{NetInfo, NetKind};
use crate::popup::{center_y, hline, launch, lift, row_plate, Content, DataEvent, HitMap, PopupKind, Reply};
use crate::shell_thread::{self, ShellRequest};
use crate::surface::{rect_wh, Font, Surface, PANEL_PAD, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, TEXT_COL, WHITE};

const ID_SETTINGS: u32 = 1;

/// Unlit Wi-Fi arcs in the header glyph (alpha).
const DIM_ALPHA: u32 = 70;

pub(crate) struct NetworkPopup {
    info: NetInfo,
    open_settings: bool,
    // Cached geometry (physical px).
    hits: HitMap,
    width: i32,
    pad: i32,
    y_head: i32,
    head_h: i32,
    sep1: i32,
    y_rows: i32,
    row_h: i32,
    sep2: i32,
}

/// Creates the content with the last known status; asks the shell thread for a fresh one.
pub(crate) fn new(info: NetInfo) -> Box<dyn Content> {
    shell_thread::request(ShellRequest::NetworkQuery);
    Box::new(NetworkPopup {
        info,
        open_settings: false,
        hits: HitMap::default(),
        width: 0,
        pad: 0,
        y_head: 0,
        head_h: 0,
        sep1: 0,
        y_rows: 0,
        row_h: 0,
        sep2: 0,
    })
}

impl NetworkPopup {
    /// Detail rows (label, value).
    fn rows(&self) -> Vec<(&'static str, String)> {
        let i = &self.info;
        let tr = crate::i18n::tr;
        let on = i.kind != NetKind::None && i.internet;
        let mut v = vec![("Internet", if on { tr("Connected", "Connesso") } else { tr("No access", "Nessun accesso") }.to_string())];
        if i.kind != NetKind::None {
            v.push((tr("IPv4 address", "Indirizzo IPv4"), if i.ipv4.is_empty() { "\u{2014}".to_string() } else { i.ipv4.clone() }));
        }
        if i.kind == NetKind::Wifi {
            v.push((tr("Signal", "Segnale"), format!("{}%", i.signal)));
        }
        v
    }

    fn title(&self) -> (String, String) {
        let i = &self.info;
        let tr = crate::i18n::tr;
        match i.kind {
            NetKind::None => (tr("Not connected", "Non connesso").to_string(), tr("No network", "Nessuna rete").to_string()),
            NetKind::Wifi => (if i.name.is_empty() { "Wi-Fi".to_string() } else { i.name.clone() }, "Wi-Fi".to_string()),
            NetKind::Ethernet => {
                (if i.name.is_empty() { "Ethernet".to_string() } else { i.name.clone() }, tr("Wired", "Cablata").to_string())
            }
        }
    }
}

/// Draws the network glyph (same look as the bar) in a `size` square at `(x, y)`.
fn net_glyph(s: &mut Surface, x: i32, y: i32, size: i32, info: &NetInfo) {
    match info.kind {
        NetKind::Ethernet => s.glyph(x, y, size, TEXT_ALPHA, &glyphs::ethernet_shape),
        kind => {
            s.glyph(x, y, size, DIM_ALPHA, &|u, v| glyphs::wifi_shape(u, v, 4));
            if kind == NetKind::Wifi {
                let bars = info.status().bars;
                s.glyph(x, y, size, TEXT_ALPHA, &|u, v| glyphs::wifi_shape(u, v, bars));
            }
        }
    }
    let online = info.kind != NetKind::None && info.internet;
    s.glyph(x, y, size, TEXT_ALPHA, &|u, v| glyphs::net_badge_shape(u, v, online));
}

impl Content for NetworkPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::Network
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let p = |v: i32| m.px(v);
        self.width = p(280);
        self.pad = p(PANEL_PAD);
        self.hits.clear();
        let mut y = self.pad;
        self.y_head = y;
        self.head_h = s.line_height(Font::Bold) + s.line_height(Font::Small) + p(8);
        y += self.head_h;
        y += p(6);
        self.sep1 = y;
        y += p(1) + p(6);
        self.y_rows = y;
        self.row_h = p(26);
        y += self.row_h * self.rows().len() as i32;
        y += p(6);
        self.sep2 = y;
        y += p(1) + p(6);
        let inset = p(8);
        self.hits.add(ID_SETTINGS, rect_wh(inset, y, self.width - 2 * inset, p(34)));
        y += p(34) + inset;
        (self.width, y)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let col = m.px(TEXT_COL);
        // Header: 24 px glyph, name (bold) and kind (small) on two lines.
        let gsz = m.px(24);
        let gy = self.y_head + (self.head_h - gsz) / 2;
        net_glyph(s, col - m.px(4), gy, gsz, &self.info);
        let tx = col - m.px(4) + gsz + m.px(12);
        let (name, sub) = self.title();
        let lb = s.line_height(Font::Bold);
        let ls = s.line_height(Font::Small);
        let ty = self.y_head + (self.head_h - lb - ls) / 2;
        let name = s.ellipsize(&name, Font::Bold, self.width - col - tx);
        s.text(&name, tx, ty, Font::Bold, WHITE, TEXT_ALPHA);
        s.text(&sub, tx, ty + lb, Font::Small, WHITE, TEXT3_ALPHA);

        hline(s, col, self.width - col, self.sep1);
        // Details: label on the text column, value right-aligned.
        let ln = s.line_height(Font::Normal);
        for (k, (label, value)) in self.rows().iter().enumerate() {
            let rc = rect_wh(0, self.y_rows + k as i32 * self.row_h, self.width, self.row_h);
            let y = center_y(&rc, ln);
            s.text(label, col, y, Font::Normal, WHITE, TEXT2_ALPHA);
            let vw = s.text_width(value, Font::Normal);
            s.text(value, self.width - col - vw, y, Font::Normal, WHITE, TEXT_ALPHA);
        }

        hline(s, col, self.width - col, self.sep2);
        if let Some(rc) = self.hits.rect(ID_SETTINGS) {
            let h = row_plate(s, rc, ID_SETTINGS, hot, pressed);
            s.text(crate::i18n::tr("Network settings", "Impostazioni di rete"), rc.left + m.px(12), center_y(&rc, ln), Font::Normal, WHITE, lift(TEXT_ALPHA, h));
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn click(&mut self, id: u32) -> Reply {
        if id == ID_SETTINGS {
            self.open_settings = true;
            return Reply::Close;
        }
        Reply::None
    }

    fn data(&mut self, ev: &DataEvent) -> Reply {
        match ev {
            DataEvent::Network(info) if *info != self.info => {
                let rows = self.rows().len();
                self.info = info.clone();
                if self.rows().len() == rows {
                    Reply::Redraw
                } else {
                    Reply::Relayout
                }
            }
            _ => Reply::None,
        }
    }

    fn after_close(&mut self) {
        if self.open_settings {
            launch("ms-settings:network");
        }
    }
}
