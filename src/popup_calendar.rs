//! CLOCK/CALENDAR popup: big "HH:mm" time, long date, 6x7 month grid
//! with previous/next month arrows, today highlighted (white plate, black text), first
//! day of the week from the locale, "Date & time settings" link (`ms-settings:dateandtime`).
//! Math and texts in `calendar.rs`.

use windows::Win32::Foundation::RECT;

use crate::calendar::{self, DayCell};
use crate::glyphs;
use crate::popup::{center_y, hline, launch, lift, row_plate, Content, HitMap, PopupKind, Reply};
use crate::surface::{
    in_rect, rect_wh, Font, Surface, PANEL_PAD, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, TEXT_COL, WHITE,
};

const ID_PREV: u32 = 1;
const ID_NEXT: u32 = 2;
const ID_TITLE: u32 = 3;
const ID_SETTINGS: u32 = 4;

pub(crate) struct CalendarPopup {
    /// Displayed month.
    view: (u16, u16),
    today: (u16, u16, u16),
    first_sun0: u32,
    headers: [String; 7],
    grid: [DayCell; 42],
    time_s: String,
    date_s: String,
    title_s: String,
    open_settings: bool,
    // Cached geometry (physical px).
    hits: HitMap,
    width: i32,
    pad: i32,
    y_time: i32,
    y_date: i32,
    sep1: i32,
    sep2: i32,
    y_head_row: i32,
    y_wd: i32,
    grid_x: i32,
    grid_y: i32,
    cell_w: i32,
    cell_h: i32,
    circle: i32,
    wheel_area: RECT,
}

/// Creates the content (initial state).
pub(crate) fn new() -> Box<dyn Content> {
    let today = calendar::today();
    let first = calendar::first_day_of_week_sun0();
    let mut c = CalendarPopup {
        view: (today.0, today.1),
        today,
        first_sun0: first,
        headers: calendar::weekday_headers(first),
        grid: [DayCell::default(); 42],
        time_s: calendar::time_now(),
        date_s: calendar::long_date_today(),
        title_s: String::new(),
        open_settings: false,
        hits: HitMap::default(),
        width: 0,
        pad: 0,
        y_time: 0,
        y_date: 0,
        sep1: 0,
        sep2: 0,
        y_head_row: 0,
        y_wd: 0,
        grid_x: 0,
        grid_y: 0,
        cell_w: 0,
        cell_h: 0,
        circle: 0,
        wheel_area: RECT::default(),
    };
    c.rebuild_grid();
    Box::new(c)
}

impl CalendarPopup {
    fn rebuild_grid(&mut self) {
        self.grid = calendar::month_grid(self.view.0, self.view.1, self.today, self.first_sun0);
        self.title_s = calendar::month_title(self.view.0, self.view.1);
    }

    fn set_view(&mut self, v: (u16, u16)) -> Reply {
        if v == self.view {
            return Reply::None;
        }
        self.view = v;
        self.rebuild_grid();
        // The title changes width: the clickable area must be recalculated.
        Reply::Relayout
    }
}

impl Content for CalendarPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::Calendar
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let p = |v: i32| m.px(v);
        self.width = p(300);
        self.pad = p(PANEL_PAD);
        self.hits.clear();
        let mut y = self.pad;
        self.y_time = y;
        y += s.line_height(Font::Big);
        self.y_date = y;
        y += s.line_height(Font::Normal);
        y += p(6);
        self.sep1 = y;
        y += p(1) + p(6);
        // Month row: clickable title on the left, arrows on the right.
        self.y_head_row = y;
        let row_h = p(28);
        let arrow = p(24);
        let next = rect_wh(self.width - self.pad - arrow, y + (row_h - arrow) / 2, arrow, arrow);
        let prev = rect_wh(next.left - p(4) - arrow, next.top, arrow, arrow);
        let tw = s.text_width(&self.title_s, Font::Bold);
        let title = rect_wh(p(TEXT_COL) - p(6), y, tw + p(12), row_h);
        self.hits.add(ID_TITLE, title);
        self.hits.add(ID_PREV, prev);
        self.hits.add(ID_NEXT, next);
        y += row_h + p(4);
        self.y_wd = y;
        y += p(20);
        self.cell_w = p(34);
        self.cell_h = p(32);
        self.circle = p(26);
        self.grid_x = (self.width - 7 * self.cell_w) / 2;
        self.grid_y = y;
        y += 6 * self.cell_h;
        self.wheel_area = RECT { left: 0, top: self.y_head_row, right: self.width, bottom: y };
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
        let col = s.metrics().px(TEXT_COL);
        s.text(&self.time_s, col, self.y_time, Font::Big, WHITE, TEXT_ALPHA);
        s.text(&self.date_s, col, self.y_date, Font::Normal, WHITE, TEXT2_ALPHA);
        let tc = s.metrics().px(TEXT_COL);
        hline(s, tc, self.width - tc, self.sep1);
        hline(s, tc, self.width - tc, self.sep2);

        let icon = s.metrics().px(16);
        // Month title.
        if let Some(rc) = self.hits.rect(ID_TITLE) {
            let h = row_plate(s, rc, ID_TITLE, hot, pressed);
            let y = center_y(&rc, s.line_height(Font::Bold));
            s.text(&self.title_s, col, y, Font::Bold, WHITE, lift(TEXT_ALPHA, h));
        }
        // Arrows.
        for (id, shape) in
            [(ID_PREV, glyphs::chevron_left_shape as fn(f32, f32) -> bool), (ID_NEXT, glyphs::chevron_right_shape)]
        {
            let Some(rc) = self.hits.rect(id) else { continue };
            let h = row_plate(s, rc, id, hot, pressed);
            let x = rc.left + ((rc.right - rc.left) - icon) / 2;
            let y = rc.top + ((rc.bottom - rc.top) - icon) / 2;
            s.glyph(x, y, icon, lift(TEXT_ALPHA, h), &shape);
        }
        // Day headers.
        let lh_small = s.line_height(Font::Small);
        for (i, h) in self.headers.iter().enumerate() {
            let cx = self.grid_x + i as i32 * self.cell_w + self.cell_w / 2;
            let tw = s.text_width(h, Font::Small);
            s.text(h, cx - tw / 2, self.y_wd + (s.metrics().px(20) - lh_small) / 2, Font::Small, WHITE, TEXT3_ALPHA);
        }
        // 6x7 grid.
        let lh = s.line_height(Font::Normal);
        for (i, cell) in self.grid.iter().enumerate() {
            let (col, row) = ((i % 7) as i32, (i / 7) as i32);
            let cx = self.grid_x + col * self.cell_w + self.cell_w / 2;
            let cy = self.grid_y + row * self.cell_h + self.cell_h / 2;
            let txt = cell.day.to_string();
            let tw = s.text_width(&txt, Font::Normal);
            if cell.today {
                let d = self.circle;
                s.fill_rr(rect_wh(cx - d / 2, cy - d / 2, d, d), d / 2, WHITE, 255);
                s.text(&txt, cx - tw / 2, cy - lh / 2, Font::Normal, (0, 0, 0), 255);
            } else {
                let a = if cell.in_month { TEXT_ALPHA } else { TEXT3_ALPHA };
                s.text(&txt, cx - tw / 2, cy - lh / 2, Font::Normal, WHITE, a);
            }
        }
        // Settings link.
        if let Some(rc) = self.hits.rect(ID_SETTINGS) {
            let h = row_plate(s, rc, ID_SETTINGS, hot, pressed);
            let y = center_y(&rc, s.line_height(Font::Normal));
            s.text(crate::i18n::tr("Date & time settings", "Impostazioni data e ora"), rc.left + s.metrics().px(12), y, Font::Normal, WHITE, lift(TEXT_ALPHA, h));
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn click(&mut self, id: u32) -> Reply {
        match id {
            ID_PREV => self.set_view(calendar::add_months(self.view.0, self.view.1, -1)),
            ID_NEXT => self.set_view(calendar::add_months(self.view.0, self.view.1, 1)),
            ID_TITLE => self.set_view((self.today.0, self.today.1)),
            ID_SETTINGS => {
                self.open_settings = true;
                Reply::Close
            }
            _ => Reply::None,
        }
    }

    fn wheel(&mut self, notches: i32, x: i32, y: i32) -> Reply {
        if !in_rect(&self.wheel_area, x, y) {
            return Reply::None;
        }
        // Wheel up = previous month.
        self.set_view(calendar::add_months(self.view.0, self.view.1, -notches))
    }

    fn tick(&mut self) -> Reply {
        let today = calendar::today();
        let time = calendar::time_now();
        let mut changed = false;
        if today != self.today {
            self.today = today;
            self.date_s = calendar::long_date_today();
            self.view = (today.0, today.1);
            self.rebuild_grid();
            changed = true;
        }
        if time != self.time_s {
            self.time_s = time;
            changed = true;
        }
        if changed {
            Reply::Relayout
        } else {
            Reply::None
        }
    }

    fn after_close(&mut self) {
        if self.open_settings {
            launch("ms-settings:dateandtime");
        }
    }
}
