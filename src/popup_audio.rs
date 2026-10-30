//! AUDIO popup: four collapsible SECTIONS, in the order of `config::AudioLayout`:
//!  - Master: mute button (`VolumeToggleMute`) + volume slider (drag + wheel, `ShellRequest::VolumeSet`);
//!  - Output: output devices (`ListOutputs`, click = `SetDefaultOutput`);
//!  - Spatial: spatial sound formats of the default output (`ListSpatial`, click = `SetSpatialFormat`),
//!    dropped when the device does not support spatial sound;
//!  - Mixer: per-app volume mixer (`ListSessions`; mute button = `ToggleSessionMute`, slider =
//!    `SetSessionVolume`), dropped when no session exists;
//!
//! then the "More volume settings" link (`ms-settings:apps-volume`) and the pencil button (Customize).
//!
//! Each section has a header row (title + chevron): a click expands / collapses it. In Customize
//! mode every header also shows a show / hide button, hidden sections stay listed (dimmed), and a
//! header can be dragged vertically to reorder the sections: the press / move / release path of the
//! sliders (`Content::drag` / `release`, mouse captured by `popup`) drives it, an insertion line
//! marks the drop slot. A press without movement still expands / collapses. Every change is saved at
//! once (`config::modify`, the path the Settings popup uses), the hint of the hovered control is
//! shown in the link row.
//!
//! Loading (memory budget): a section reads its data from Windows ONLY while it is on screen with
//! its content (`AudioLayout::loadable`: visible and expanded). `sync_loading` sends the
//! request when a section becomes loadable, drops the data when it stops being so, and tells the
//! shell thread which data to refresh on a device change (`audio::set_popup_wants`). Collapsed
//! Output = no device enumeration, collapsed Spatial = no spatial-format probe (WinRT), collapsed
//! Mixer = no session enumeration. The wheel always adjusts the master volume.
//!
//! Initial state: the last known `VolumeState` passed by `popup::toggle` (via `DataEvent::Volume`).
//! Nothing outlives the popup.

use windows::Win32::Foundation::RECT;

use crate::audio::{self, AppSession, OutputDevice, SpatialState, VolumeState, WHEEL_STEP_PERCENT};
use crate::config::{self, AudioLayout, AUDIO_MASTER, AUDIO_MIXER, AUDIO_OUTPUT, AUDIO_SPATIAL};
use crate::glyphs;
use crate::i18n::tr;
use crate::popup::{
    center_y, hline, launch, lift, paint_slider, row_plate, slider_value_at, tick_ms, Content, DataEvent, HitMap, PopupKind, Reply,
};
use crate::shell_thread::{self, ShellRequest};
use crate::surface::{rect_wh, Font, Surface, PANEL_PAD, TEXT2_ALPHA, TEXT3_ALPHA, TEXT_ALPHA, TEXT_COL, WHITE};

const ID_MUTE: u32 = 1;
const ID_SLIDER: u32 = 2;
const ID_MIXER: u32 = 3;
/// Pencil button: Customize mode on / off.
/// PREVIEW-HOOK: `preview` clicks it to render the Customize mode.
pub(crate) const ID_EDIT: u32 = 4;
/// Header of section `ID_HEAD + section` (0..=3): expand / collapse; in Customize mode also the
/// handle that drags the section to a new place.
const ID_HEAD: u32 = 10;
/// Customize show / hide button of section `ID_EYE + section`.
const ID_EYE: u32 = 40;
/// Ids per section block (`base..base + ITEMS`).
const ITEMS: u32 = 4;
/// Device rows have id `ID_DEVICE + index`.
const ID_DEVICE: u32 = 100;
/// Spatial format rows have id `ID_SPATIAL + index`.
const ID_SPATIAL: u32 = 200;
/// Mixer rows: mute button `ID_APP_MUTE + index`, slider `ID_APP_SLIDER + index`.
const ID_APP_MUTE: u32 = 300;
const ID_APP_SLIDER: u32 = 400;
/// End (exclusive) of the mixer slider id range.
const ID_END: u32 = 500;

/// Minimum interval between two `VolumeSet`/`SetSessionVolume` calls while dragging (ms).
const SET_THROTTLE_MS: u64 = 30;

/// Row height (logical px) of every list row (devices, spatial formats, mixer, link).
const ROW: i32 = 34;
/// Height (logical px) of a section header row.
const HEAD: i32 = 28;
/// Height (logical px) of the master row (32 px button + 2 px above and below).
const MASTER_H: i32 = 36;
/// Side (logical px) of a Customize button.
const TOOL: i32 = 22;
/// Vertical movement (logical px) after which a press on a header becomes a drag (below it the
/// release is a plain click: expand / collapse).
const DRAG_MIN: i32 = 4;
/// Top inset (logical px) above the first header.
const TOP: i32 = 8;

/// `asked` bits: the data request of that section was sent (and its data is kept).
const ASK_OUTPUTS: u8 = 1;
const ASK_SPATIAL: u8 = 2;
const ASK_SESSIONS: u8 = 4;

/// A section header on screen, with what goes under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Block {
    /// Section (`AUDIO_*`).
    item: u8,
    /// The content is shown under the header (visible and expanded).
    content: bool,
    /// Hidden section listed in Customize mode: dimmed, no content.
    dim: bool,
}

/// A block with its geometry (physical px).
#[derive(Clone, Copy)]
struct Placed {
    b: Block,
    /// Separator ordinate above the header (none for the first block).
    sep: Option<i32>,
    head: RECT,
    /// Top of the content (rows below the header).
    content_top: i32,
    /// Bottom of the content (= `content_top` when collapsed).
    end: i32,
}

/// A header pressed in Customize mode (a drag in the making).
#[derive(Clone, Copy)]
struct Reorder {
    section: u8,
    /// Pointer ordinate at the press (physical px).
    y0: i32,
    /// Order index the section takes if released now; `None` until the pointer moved `DRAG_MIN`.
    to: Option<usize>,
}

pub(crate) struct AudioPopup {
    level: u8,
    muted: bool,
    available: bool,
    /// Layout (a copy of `config.audio_layout`, saved on every change).
    layout: AudioLayout,
    /// Customize mode (not saved).
    customize: bool,
    /// `ASK_*` bits of the requests sent for the loadable sections.
    asked: u8,
    /// `None` until the first reply to `ListOutputs` arrives (or while Output is not loadable).
    outputs: Option<Vec<OutputDevice>>,
    /// `None` = not supported (or not read yet, see `spatial_loaded`).
    spatial: Option<SpatialState>,
    /// true once the reply to `ListSpatial` arrived.
    spatial_loaded: bool,
    /// Mixer rows; `None` until the first reply to `ListSessions` arrives.
    sessions: Option<Vec<AppSession>>,
    dragging: bool,
    /// Mixer row whose slider is being dragged.
    drag_app: Option<usize>,
    /// Header pressed / dragged in Customize mode.
    reorder: Option<Reorder>,
    /// Header id whose click must be ignored: the release that ended a drag (`release` runs right
    /// before `click`).
    swallow: Option<u32>,
    /// `DRAG_MIN` in physical px.
    drag_min: i32,
    last_set: u64,
    open_mixer: bool,
    // Cached geometry (physical px).
    hits: HitMap,
    placed: Vec<Placed>,
    width: i32,
    pad: i32,
    slider: RECT,
    knob_r: i32,
    pct_right: i32,
    sep2: i32,
    /// Name column of the mixer rows: x and width.
    app_name_x: i32,
    app_name_w: i32,
}

/// Creates the content (initial state).
pub(crate) fn new(volume: VolumeState) -> Box<dyn Content> {
    audio::set_popup_open(true);
    // Fresh volume (also for the wheel when the master section is collapsed); the section data is
    // requested by `sync_loading`, only for what is on screen.
    shell_thread::request(ShellRequest::VolumeRefresh);
    let mut p = AudioPopup {
        level: volume.level.min(100),
        muted: volume.muted,
        available: volume.available,
        layout: config::get().audio_layout,
        customize: false,
        asked: 0,
        outputs: None,
        spatial: None,
        spatial_loaded: false,
        sessions: None,
        dragging: false,
        drag_app: None,
        reorder: None,
        swallow: None,
        drag_min: DRAG_MIN,
        last_set: 0,
        open_mixer: false,
        hits: HitMap::default(),
        placed: Vec::new(),
        width: 0,
        pad: 0,
        slider: RECT::default(),
        knob_r: 7,
        pct_right: 0,
        sep2: 0,
        app_name_x: 0,
        app_name_w: 0,
    };
    p.sync_loading();
    Box::new(p)
}

// ------------------------------------------------------------------ shared slider / speaker helpers

/// Level 0..=100 for pointer x on slider `rc` (`popup::slider_value_at`).
fn level_at(rc: &RECT, knob_r: i32, x: i32) -> u8 {
    slider_value_at(rc, knob_r, 0, 100, x) as u8
}

/// Track, filled part and knob of slider `rc` at `level` (`popup::paint_slider`).
fn paint_level(s: &mut Surface, rc: &RECT, knob_r: i32, level: u8) {
    paint_slider(s, rc, knob_r, 0, 100, i32::from(level.min(100)));
}

/// Speaker button (plate + glyph) for mute toggle `id` in `rc`.
fn paint_speaker(s: &mut Surface, rc: RECT, id: u32, hot: Option<u32>, pressed: Option<u32>, level: u8, muted: bool) {
    let icon = s.metrics().px(16);
    let h = row_plate(s, rc, id, hot, pressed);
    let waves = glyphs::waves_for(level);
    let x = rc.left + ((rc.right - rc.left) - icon) / 2;
    let y = rc.top + ((rc.bottom - rc.top) - icon) / 2;
    s.glyph(x, y, icon, lift(TEXT_ALPHA, h), &|u, v| glyphs::speaker_shape(u, v, waves, muted));
}

/// Percentage text right-aligned at `right`, vertically centered on `row`.
fn paint_pct(s: &mut Surface, level: u8, right: i32, row: &RECT, alpha: u32) {
    let pct = format!("{level}%");
    let tw = s.text_width(&pct, Font::Normal);
    let lh = s.line_height(Font::Normal);
    s.text(&pct, right - tw, center_y(row, lh), Font::Normal, WHITE, alpha);
}

/// Centered one-line message ("Loading…", "No output device") in a list row at `y`.
fn paint_note(s: &mut Surface, width: i32, y: i32, txt: &str, font: Font, alpha: u32) {
    let rc = rect_wh(0, y, width, s.metrics().px(ROW));
    let tw = s.text_width(txt, font);
    let lh = s.line_height(font);
    s.text(txt, (width - tw) / 2, center_y(&rc, lh), font, WHITE, alpha);
}

/// Header title of a section.
fn title(section: u8) -> &'static str {
    match section {
        AUDIO_MASTER => tr("Master volume", "Volume generale"),
        AUDIO_OUTPUT => tr("Output", "Uscita"),
        AUDIO_SPATIAL => tr("Spatial sound", "Audio spaziale"),
        _ => tr("Volume mixer", "Mixer volume"),
    }
}

/// `(base, section)` of a header or show / hide id.
fn split(id: u32) -> Option<(u32, u8)> {
    [ID_HEAD, ID_EYE]
        .into_iter()
        .find(|&b| (b..b + ITEMS).contains(&id))
        .map(|b| (b, (id - b) as u8))
}

// ------------------------------------------------------------------ layout (pure)

/// Logical-pixel layout inputs of the sections' content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListCounts {
    /// Device rows (0 = one "empty/loading" row).
    pub devices: usize,
    /// Spatial rows: `None` = not read yet (one "loading" row), `Some(0)` = not supported (the
    /// section is dropped, except in Customize mode where its header stays).
    pub spatial: Option<usize>,
    /// Mixer rows: `None` = not read yet (one "loading" row), `Some(0)` = no session (as above).
    pub apps: Option<usize>,
}

/// false for a section whose data says it has nothing to show (spatial not supported, no session).
fn present(section: u8, c: &ListCounts) -> bool {
    match section {
        AUDIO_SPATIAL => c.spatial != Some(0),
        AUDIO_MIXER => c.apps != Some(0),
        _ => true,
    }
}

/// The headers on screen, in order, into `out`. Outside Customize mode hidden (and empty) sections
/// are left out; in Customize mode every section is listed, in layout order (the drag relies on it).
fn collect_blocks(l: &AudioLayout, customize: bool, c: &ListCounts, out: &mut Vec<Block>) {
    out.clear();
    for &s in l.order() {
        if customize || (!l.is_hidden(s) && present(s, c)) {
            out.push(Block { item: s, content: l.loadable(s), dim: l.is_hidden(s) });
        }
    }
}

/// Order index a header dragged from index `from` takes when released at pointer ordinate `y`,
/// given the vertical midpoints of all the headers (top to bottom, the dragged one included): the
/// drop slot is the number of midpoints above `y`, and the slots past `from` shift by one once the
/// header leaves its place.
fn drop_index(mids: impl IntoIterator<Item = i32>, from: usize, y: i32) -> usize {
    let slot = mids.into_iter().filter(|&m| m < y).count();
    if slot > from {
        slot - 1
    } else {
        slot
    }
}

/// Drop slot (0..=n, a gap between headers) where a header dragged from `from` lands at index `to`.
fn drop_slot(from: usize, to: usize) -> usize {
    if to > from {
        to + 1
    } else {
        to
    }
}

/// Height (logical units through `px`) of the content of `item` when it is shown.
fn content_h(px: &dyn Fn(i32) -> i32, item: u8, c: &ListCounts) -> i32 {
    let row = px(ROW);
    match item {
        AUDIO_MASTER => px(MASTER_H),
        AUDIO_OUTPUT => row * c.devices.max(1) as i32,
        AUDIO_SPATIAL => row * c.spatial.unwrap_or(1) as i32,
        _ => row * c.apps.unwrap_or(1) as i32,
    }
}

/// Separator above every header but the first: gap, 1 px line, gap.
fn sep_h(px: &dyn Fn(i32) -> i32) -> i32 {
    px(4) + px(1) + px(3)
}

/// Total popup height for layout `l` and counts `c` (the same arithmetic `size()` walks through),
/// used by the tests and handy to reason about the screen fit.
pub(crate) fn popup_height(px: &dyn Fn(i32) -> i32, l: &AudioLayout, customize: bool, c: ListCounts) -> i32 {
    let mut blocks = Vec::with_capacity(5);
    collect_blocks(l, customize, &c, &mut blocks);
    let mut y = px(TOP);
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            y += sep_h(px);
        }
        y += px(HEAD);
        if b.content {
            y += content_h(px, b.item, &c);
        }
    }
    // Bottom separator + link row + bottom inset.
    y += px(6) + px(1) + px(6) + px(ROW) + px(8);
    y
}

impl AudioPopup {
    fn send_level(&mut self) {
        self.last_set = tick_ms();
        shell_thread::request(ShellRequest::VolumeSet { percent: self.level });
    }

    fn send_app_level(&mut self, idx: usize) {
        let Some(a) = self.sessions.as_ref().and_then(|v| v.get(idx)) else { return };
        let (pid, system, level) = (a.pid, a.system, a.level);
        self.last_set = tick_ms();
        shell_thread::request(ShellRequest::SetSessionVolume { pid, system, level });
    }

    fn spatial_len(&self) -> usize {
        self.spatial.as_ref().map_or(0, |s| s.options.len())
    }

    fn counts(&self) -> ListCounts {
        ListCounts {
            devices: self.outputs.as_ref().map_or(0, Vec::len),
            spatial: self.spatial_loaded.then(|| self.spatial_len()),
            apps: self.sessions.as_ref().map(Vec::len),
        }
    }

    /// Requests the data of every section that just became loadable (visible and expanded) and
    /// drops the data of those that stopped being so; tells the shell thread what to
    /// refresh on a device change. No timer, no thread: one request per section per opening.
    fn sync_loading(&mut self) {
        let want_o = self.layout.loadable(AUDIO_OUTPUT);
        let want_s = self.layout.loadable(AUDIO_SPATIAL);
        let want_m = self.layout.loadable(AUDIO_MIXER);
        audio::set_popup_wants(want_o, want_s, want_m);
        if want_o {
            if self.asked & ASK_OUTPUTS == 0 {
                self.asked |= ASK_OUTPUTS;
                shell_thread::request(ShellRequest::ListOutputs);
            }
        } else {
            self.asked &= !ASK_OUTPUTS;
            self.outputs = None;
        }
        if want_s {
            if self.asked & ASK_SPATIAL == 0 {
                self.asked |= ASK_SPATIAL;
                shell_thread::request(ShellRequest::ListSpatial);
            }
        } else {
            self.asked &= !ASK_SPATIAL;
            self.spatial = None;
            self.spatial_loaded = false;
        }
        if want_m {
            if self.asked & ASK_SESSIONS == 0 {
                self.asked |= ASK_SESSIONS;
                shell_thread::request(ShellRequest::ListSessions);
            }
        } else {
            self.asked &= !ASK_SESSIONS;
            self.sessions = None;
            self.drag_app = None;
        }
    }

    /// The layout changed: saved now (like the Settings popup; in memory only if the file cannot be
    /// written), the loading follows, relayout.
    fn commit(&mut self) -> Reply {
        let l = self.layout;
        if let Err(e) = config::modify(|c| c.audio_layout = l) {
            crate::log!("audio popup: saving the layout failed ({e}), applied in memory only");
            let mut c = (*config::get()).clone();
            c.audio_layout = l;
            config::set(c);
        }
        self.sync_loading();
        Reply::Relayout
    }

    /// Hint of a hovered pencil / Customize control, shown in the link row.
    fn hint(&self, id: u32) -> Option<&'static str> {
        if id == ID_EDIT {
            return Some(if self.customize {
                tr("Done customizing", "Fine personalizzazione")
            } else {
                tr("Customize panel", "Personalizza pannello")
            });
        }
        let (base, item) = split(id)?;
        Some(match base {
            ID_HEAD if self.customize => tr("Drag to reorder", "Trascina per riordinare"),
            ID_EYE if self.layout.is_hidden(item) => tr("Show", "Mostra"),
            ID_EYE => tr("Hide", "Nascondi"),
            _ => return None,
        })
    }

    /// Press (first call) or move with the button held on the header of `section`. Customize mode
    /// only: past `DRAG_MIN` of vertical movement the header is being dragged and the drop index
    /// follows the pointer. The mouse is already captured by `popup` for the whole press.
    fn drag_header(&mut self, section: u8, y: i32) -> Reply {
        if !self.customize {
            return Reply::None;
        }
        let r = match self.reorder.as_mut() {
            Some(r) if r.section == section => r,
            _ => {
                self.reorder = Some(Reorder { section, y0: y, to: None });
                return Reply::None;
            }
        };
        if r.to.is_none() && (y - r.y0).abs() < self.drag_min {
            return Reply::None;
        }
        let Some(from) = self.placed.iter().position(|p| p.b.item == section) else { return Reply::None };
        let to = drop_index(self.placed.iter().map(|p| (p.head.top + p.head.bottom) / 2), from, y);
        if r.to == Some(to) {
            return Reply::None;
        }
        r.to = Some(to);
        Reply::Redraw
    }

    /// Section being dragged (moved past `DRAG_MIN`) and the order index it would take.
    fn dragging_header(&self) -> Option<(u8, usize)> {
        self.reorder.and_then(|r| r.to.map(|to| (r.section, to)))
    }

    /// Header row: plate, title, chevron and (Customize mode) the show / hide button. The header
    /// being dragged keeps a steady plate and a dimmed title.
    fn paint_header(&self, s: &mut Surface, p: &Placed, hot: Option<u32>, pressed: Option<u32>, dragged: bool) {
        let m = s.metrics();
        let item = p.b.item;
        let rc = p.head;
        let h = if dragged {
            s.plate(rc, m.px(crate::surface::ROW_RADIUS), crate::surface::PRESS_ALPHA);
            false
        } else {
            row_plate(s, rc, ID_HEAD + item as u32, hot, pressed)
        };
        let base = if p.b.dim || dragged { TEXT3_ALPHA / 2 } else { TEXT3_ALPHA };
        let alpha = lift(base, h);
        // Chevron at the right end: down = expanded, right = collapsed.
        let chev = m.px(12);
        let cx = rc.right - m.px(8) - chev;
        let cy = rc.top + ((rc.bottom - rc.top) - chev) / 2;
        let open = self.layout.is_open(item);
        s.glyph(cx, cy, chev, alpha, &|u, v| {
            if open {
                glyphs::chevron_down_shape(u, v)
            } else {
                glyphs::chevron_right_shape(u, v)
            }
        });
        // Title up to the show / hide button (or the chevron).
        let tx = m.px(TEXT_COL);
        let eye = if self.customize { self.hits.rect(ID_EYE + item as u32) } else { None };
        let limit = eye.map_or(cx - m.px(6), |e| e.left - m.px(4));
        let lh = s.line_height(Font::Small);
        let name = s.ellipsize(title(item), Font::Small, limit - tx);
        s.text(&name, tx, center_y(&rc, lh), Font::Small, WHITE, alpha);
        let Some(brc) = eye else { return };
        let icon = m.px(14);
        let bh = row_plate(s, brc, ID_EYE + item as u32, hot, pressed);
        let gx = brc.left + ((brc.right - brc.left) - icon) / 2;
        let gy = brc.top + ((brc.bottom - brc.top) - icon) / 2;
        let hidden = self.layout.is_hidden(item);
        s.glyph(gx, gy, icon, lift(TEXT2_ALPHA, bh), &|u, v| glyphs::eye_shape(u, v, hidden));
    }

    /// Insertion line of the header being dragged (none while the drop would not move it).
    fn paint_drop_line(&self, s: &mut Surface) {
        let Some((section, to)) = self.dragging_header() else { return };
        let Some(from) = self.placed.iter().position(|p| p.b.item == section) else { return };
        if to == from {
            return;
        }
        let m = s.metrics();
        let slot = drop_slot(from, to);
        let y = match (slot, self.placed.get(slot)) {
            (0, Some(first)) => first.head.top - m.px(4),
            (_, Some(next)) => next.sep.unwrap_or(next.head.top - m.px(4)),
            (_, None) => self.placed.last().map_or(self.sep2, |last| last.end + m.px(3)),
        };
        let th = m.px(2);
        let x0 = m.px(TEXT_COL) - m.px(4);
        let rc = RECT { left: x0, top: y - th / 2, right: self.width - x0, bottom: y - th / 2 + th };
        s.fill_rr(rc, th / 2, WHITE, TEXT_ALPHA);
    }

    fn paint_master(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        let Some(rc) = self.hits.rect(ID_MUTE) else { return };
        paint_speaker(s, rc, ID_MUTE, hot, pressed, self.level, self.muted || !self.available);
        paint_level(s, &self.slider, self.knob_r, self.level);
        paint_pct(s, self.level, self.pct_right, &rc, TEXT2_ALPHA);
    }

    fn paint_outputs(&self, s: &mut Surface, top: i32, hot: Option<u32>, pressed: Option<u32>) {
        let m = s.metrics();
        let icon = m.px(16);
        let lh = s.line_height(Font::Normal);
        match self.outputs.as_ref() {
            Some(list) if !list.is_empty() => {
                for (i, d) in list.iter().enumerate() {
                    let id = ID_DEVICE + i as u32;
                    let Some(rc) = self.hits.rect(id) else { continue };
                    let h = row_plate(s, rc, id, hot, pressed);
                    // Check mark in a 16 px slot at the text column (20), name at 20 + 16 + 8.
                    let x0 = rc.left + m.px(12);
                    if d.is_default {
                        s.glyph(x0, rc.top + ((rc.bottom - rc.top) - icon) / 2, icon, TEXT_ALPHA, &glyphs::check_shape);
                    }
                    // Form-factor icon after the check slot, name after the icon.
                    let ix = x0 + icon + m.px(6);
                    let kind = d.kind;
                    let ia = if d.is_default { TEXT_ALPHA } else { TEXT2_ALPHA };
                    s.glyph(ix, rc.top + ((rc.bottom - rc.top) - icon) / 2, icon, lift(ia, h), &|u, v| {
                        glyphs::device_shape(u, v, kind)
                    });
                    let tx = ix + icon + m.px(8);
                    let name = s.ellipsize(&d.name, Font::Normal, rc.right - m.px(10) - tx);
                    let alpha = if d.is_default { TEXT_ALPHA } else { TEXT2_ALPHA };
                    s.text(&name, tx, center_y(&rc, lh), Font::Normal, WHITE, lift(alpha, h));
                }
            }
            Some(_) => paint_note(
                s,
                self.width,
                top,
                tr("No output device", "Nessun dispositivo di uscita"),
                Font::Normal,
                TEXT2_ALPHA,
            ),
            None => paint_note(s, self.width, top, tr("Loading\u{2026}", "Caricamento\u{2026}"), Font::Small, TEXT3_ALPHA),
        }
    }

    /// Spatial sound: same check-mark style as the device rows (no form-factor icon).
    fn paint_spatial(&self, s: &mut Surface, top: i32, hot: Option<u32>, pressed: Option<u32>) {
        if !self.spatial_loaded {
            paint_note(s, self.width, top, tr("Loading\u{2026}", "Caricamento\u{2026}"), Font::Small, TEXT3_ALPHA);
            return;
        }
        let Some(sp) = self.spatial.as_ref() else { return };
        let m = s.metrics();
        let icon = m.px(16);
        let lh = s.line_height(Font::Normal);
        for (i, o) in sp.options.iter().enumerate() {
            let id = ID_SPATIAL + i as u32;
            let Some(rc) = self.hits.rect(id) else { continue };
            let h = row_plate(s, rc, id, hot, pressed);
            let selected = sp.current == Some(i);
            let x0 = rc.left + m.px(12);
            if selected {
                s.glyph(x0, rc.top + ((rc.bottom - rc.top) - icon) / 2, icon, TEXT_ALPHA, &glyphs::check_shape);
            }
            let tx = x0 + icon + m.px(8);
            let name = s.ellipsize(o.kind.label(), Font::Normal, rc.right - m.px(10) - tx);
            let alpha = if selected { TEXT_ALPHA } else { TEXT2_ALPHA };
            s.text(&name, tx, center_y(&rc, lh), Font::Normal, WHITE, lift(alpha, h));
        }
    }

    fn paint_mixer(&self, s: &mut Surface, top: i32, hot: Option<u32>, pressed: Option<u32>) {
        let Some(list) = self.sessions.as_ref() else {
            paint_note(s, self.width, top, tr("Loading\u{2026}", "Caricamento\u{2026}"), Font::Small, TEXT3_ALPHA);
            return;
        };
        let lh = s.line_height(Font::Normal);
        for (i, a) in list.iter().enumerate() {
            let (Some(brc), Some(src)) =
                (self.hits.rect(ID_APP_MUTE + i as u32), self.hits.rect(ID_APP_SLIDER + i as u32))
            else {
                continue;
            };
            paint_speaker(s, brc, ID_APP_MUTE + i as u32, hot, pressed, a.level, a.muted);
            let name = s.ellipsize(&a.name, Font::Normal, self.app_name_w);
            let alpha = if a.muted { TEXT3_ALPHA } else { TEXT2_ALPHA };
            s.text(&name, self.app_name_x, center_y(&brc, lh), Font::Normal, WHITE, alpha);
            paint_level(s, &src, self.knob_r, a.level);
            paint_pct(s, a.level, self.pct_right, &brc, TEXT2_ALPHA);
        }
    }
}

impl Content for AudioPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::Audio
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let p = |v: i32| m.px(v);
        self.width = p(320);
        self.pad = p(PANEL_PAD);
        self.hits.clear();
        self.drag_min = p(DRAG_MIN);
        if !self.customize {
            self.reorder = None;
        }
        let counts = self.counts();
        let mut blocks = Vec::with_capacity(5);
        collect_blocks(&self.layout, self.customize, &counts, &mut blocks);
        self.placed.clear();
        let btn = p(32);
        let pct_w = s.text_width("100%", Font::Normal);
        self.pct_right = self.width - p(TEXT_COL);
        let sl_right = self.pct_right - pct_w - p(8);
        let sl_h = p(24);
        self.knob_r = p(7);
        let inset = p(8);
        let row = p(ROW);
        let head = p(HEAD);
        let tool = p(TOOL);
        let mut y = p(TOP);
        for (i, b) in blocks.iter().copied().enumerate() {
            let sep = if i > 0 {
                let line = y + p(4);
                y = line + p(1) + p(3);
                Some(line)
            } else {
                None
            };
            let hrc = rect_wh(inset, y, self.width - 2 * inset, head);
            self.hits.add(ID_HEAD + b.item as u32, hrc);
            if self.customize {
                // Show / hide button before the chevron slot (added after the header: it wins).
                let x = hrc.right - p(8) - p(12) - p(6) - tool;
                self.hits.add(ID_EYE + b.item as u32, rect_wh(x, y + (head - tool) / 2, tool, tool));
            }
            y += head;
            let content_top = y;
            if b.content {
                match b.item {
                    AUDIO_MASTER => {
                        let top = y + (p(MASTER_H) - btn) / 2;
                        self.hits.add(ID_MUTE, rect_wh(self.pad, top, btn, btn));
                        let sl_left = self.pad + btn + p(8);
                        self.slider = rect_wh(sl_left, top + (btn - sl_h) / 2, (sl_right - sl_left).max(p(40)), sl_h);
                        self.hits.add(ID_SLIDER, self.slider);
                        y += p(MASTER_H);
                    }
                    AUDIO_OUTPUT => match self.outputs.as_ref() {
                        Some(list) if !list.is_empty() => {
                            for i in 0..list.len() {
                                self.hits.add(ID_DEVICE + i as u32, rect_wh(inset, y, self.width - 2 * inset, row));
                                y += row;
                            }
                        }
                        _ => y += row,
                    },
                    AUDIO_SPATIAL => {
                        if self.spatial_loaded {
                            for i in 0..self.spatial_len() {
                                self.hits.add(ID_SPATIAL + i as u32, rect_wh(inset, y, self.width - 2 * inset, row));
                                y += row;
                            }
                        } else {
                            y += row;
                        }
                    }
                    AUDIO_MIXER => match self.sessions.as_ref() {
                        Some(list) => {
                            let abtn = p(30);
                            self.app_name_x = self.pad + abtn + p(8);
                            self.app_name_w = p(92);
                            let a_sl_left = self.app_name_x + self.app_name_w + p(8);
                            for i in 0..list.len() {
                                self.hits.add(ID_APP_MUTE + i as u32, rect_wh(self.pad, y + (row - abtn) / 2, abtn, abtn));
                                self.hits.add(
                                    ID_APP_SLIDER + i as u32,
                                    rect_wh(a_sl_left, y + (row - sl_h) / 2, (sl_right - a_sl_left).max(p(40)), sl_h),
                                );
                                y += row;
                            }
                        }
                        None => y += row,
                    },
                    _ => {}
                }
            }
            self.placed.push(Placed { b, sep, head: hrc, content_top, end: y });
        }
        y += p(6);
        self.sep2 = y;
        y += p(1) + p(6);
        // Link row, with the pencil button at its right end.
        self.hits.add(ID_MIXER, rect_wh(inset, y, self.width - 2 * inset - row - p(4), row));
        self.hits.add(ID_EDIT, rect_wh(self.width - inset - row, y, row, row));
        y += row + inset;
        debug_assert_eq!(y, popup_height(&p, &self.layout, self.customize, counts));
        (self.width, y)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let lh = s.line_height(Font::Normal);
        let text_x0 = m.px(TEXT_COL);
        let text_x1 = self.width - m.px(TEXT_COL);
        let dragged = self.dragging_header().map(|(section, _)| section);
        for pl in &self.placed {
            if let Some(y) = pl.sep {
                hline(s, text_x0, text_x1, y);
            }
            self.paint_header(s, pl, hot, pressed, dragged == Some(pl.b.item));
            if !pl.b.content {
                continue;
            }
            match pl.b.item {
                AUDIO_MASTER => self.paint_master(s, hot, pressed),
                AUDIO_OUTPUT => self.paint_outputs(s, pl.content_top, hot, pressed),
                AUDIO_SPATIAL => self.paint_spatial(s, pl.content_top, hot, pressed),
                AUDIO_MIXER => self.paint_mixer(s, pl.content_top, hot, pressed),
                _ => {}
            }
        }

        hline(s, text_x0, text_x1, self.sep2);
        self.paint_drop_line(s);
        if let Some(rc) = self.hits.rect(ID_MIXER) {
            let h = row_plate(s, rc, ID_MIXER, hot, pressed);
            // The hint of a hovered pencil / Customize control replaces the link label meanwhile
            // (the drag hint stays for the whole drag, the pointer may leave the header).
            let hint = match dragged {
                Some(_) => Some(tr("Drag to reorder", "Trascina per riordinare")),
                None => hot.and_then(|id| self.hint(id)),
            };
            match hint {
                Some(hint) => {
                    let txt = s.ellipsize(hint, Font::Small, rc.right - rc.left - m.px(16));
                    let slh = s.line_height(Font::Small);
                    s.text(&txt, rc.left + m.px(12), center_y(&rc, slh), Font::Small, WHITE, TEXT2_ALPHA);
                }
                None => s.text(
                    tr("More volume settings", "Altre impostazioni volume"),
                    rc.left + m.px(12),
                    center_y(&rc, lh),
                    Font::Normal,
                    WHITE,
                    lift(TEXT_ALPHA, h),
                ),
            }
        }
        if let Some(rc) = self.hits.rect(ID_EDIT) {
            // Customize on: a steady plate marks the pencil as active.
            if self.customize && hot != Some(ID_EDIT) {
                s.plate(rc, m.px(crate::surface::ROW_RADIUS), crate::surface::HOVER_ALPHA);
            }
            let h = row_plate(s, rc, ID_EDIT, hot, pressed);
            let icon = m.px(16);
            let base = if self.customize { TEXT_ALPHA } else { TEXT2_ALPHA };
            let gx = rc.left + ((rc.right - rc.left) - icon) / 2;
            let gy = rc.top + ((rc.bottom - rc.top) - icon) / 2;
            s.glyph(gx, gy, icon, lift(base, h), &glyphs::pencil_shape);
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn click(&mut self, id: u32) -> Reply {
        // The release that ended a header drag is not a click.
        if self.swallow.take() == Some(id) {
            return Reply::None;
        }
        if let Some((base, item)) = split(id) {
            match base {
                ID_HEAD => self.layout.toggle_open(item),
                _ => self.layout.toggle_hidden(item),
            }
            return self.commit();
        }
        match id {
            ID_MUTE => {
                shell_thread::request(ShellRequest::VolumeToggleMute);
                self.muted = !self.muted;
                Reply::Redraw
            }
            ID_MIXER => {
                self.open_mixer = true;
                Reply::Close
            }
            ID_EDIT => {
                self.customize = !self.customize;
                self.reorder = None;
                Reply::Relayout
            }
            i if (ID_DEVICE..ID_SPATIAL).contains(&i) => {
                let idx = (i - ID_DEVICE) as usize;
                let Some(list) = self.outputs.as_mut() else { return Reply::None };
                let Some(dev) = list.get(idx) else { return Reply::None };
                if dev.is_default {
                    return Reply::None;
                }
                let target = dev.id.clone();
                shell_thread::request(ShellRequest::SetDefaultOutput { id: target.clone() });
                for d in list.iter_mut() {
                    d.is_default = d.id == target;
                }
                Reply::Redraw
            }
            i if (ID_SPATIAL..ID_APP_MUTE).contains(&i) => {
                let idx = (i - ID_SPATIAL) as usize;
                let Some(sp) = self.spatial.as_mut() else { return Reply::None };
                let Some(opt) = sp.options.get(idx) else { return Reply::None };
                if sp.current == Some(idx) {
                    return Reply::None;
                }
                shell_thread::request(ShellRequest::SetSpatialFormat { subtype: opt.subtype.clone() });
                sp.current = Some(idx);
                Reply::Redraw
            }
            i if (ID_APP_MUTE..ID_APP_SLIDER).contains(&i) => {
                let idx = (i - ID_APP_MUTE) as usize;
                let Some(a) = self.sessions.as_mut().and_then(|v| v.get_mut(idx)) else { return Reply::None };
                shell_thread::request(ShellRequest::ToggleSessionMute { pid: a.pid, system: a.system });
                a.muted = !a.muted;
                Reply::Redraw
            }
            _ => Reply::None,
        }
    }

    fn drag(&mut self, id: u32, x: i32, y: i32) -> Reply {
        // A new press or move: any click to swallow belonged to the previous release.
        self.swallow = None;
        if let Some((ID_HEAD, section)) = split(id) {
            return self.drag_header(section, y);
        }
        if (ID_APP_SLIDER..ID_END).contains(&id) {
            let idx = (id - ID_APP_SLIDER) as usize;
            let Some(rc) = self.hits.rect(id) else { return Reply::None };
            let lvl = level_at(&rc, self.knob_r, x);
            let Some(a) = self.sessions.as_mut().and_then(|v| v.get_mut(idx)) else { return Reply::None };
            if lvl == a.level && !(a.muted && lvl > 0) {
                self.drag_app = Some(idx);
                return Reply::None;
            }
            a.level = lvl;
            if lvl > 0 {
                a.muted = false;
            }
            self.drag_app = Some(idx);
            if tick_ms().saturating_sub(self.last_set) >= SET_THROTTLE_MS {
                self.send_app_level(idx);
            }
            return Reply::Redraw;
        }
        if id != ID_SLIDER {
            return Reply::None;
        }
        let lvl = level_at(&self.slider, self.knob_r, x);
        self.dragging = true;
        if lvl != self.level || self.muted {
            self.level = lvl;
            if lvl > 0 {
                self.muted = false;
            }
            if tick_ms().saturating_sub(self.last_set) >= SET_THROTTLE_MS {
                self.send_level();
            }
            return Reply::Redraw;
        }
        Reply::None
    }

    fn release(&mut self, id: u32) -> Reply {
        if let Some(r) = self.reorder.take() {
            let Some(to) = r.to else { return Reply::None };
            // A drag: the click that follows the release is not an expand / collapse.
            self.swallow = Some(id);
            return if self.layout.move_to(r.section, to) { self.commit() } else { Reply::Redraw };
        }
        if id == ID_SLIDER {
            self.dragging = false;
            self.send_level();
        } else if (ID_APP_SLIDER..ID_END).contains(&id) {
            if let Some(idx) = self.drag_app.take() {
                self.send_app_level(idx);
                // Re-read the real state once the change is done.
                shell_thread::request(ShellRequest::ListSessions);
            }
        }
        Reply::None
    }

    fn wheel(&mut self, notches: i32, _x: i32, _y: i32) -> Reply {
        // Always the master volume, whatever the layout (collapsed or hidden master included).
        let delta = notches * WHEEL_STEP_PERCENT;
        shell_thread::request(ShellRequest::VolumeAdjust { delta_percent: delta });
        self.level = (self.level as i32 + delta).clamp(0, 100) as u8;
        if delta > 0 {
            self.muted = false;
        }
        Reply::Redraw
    }

    fn data(&mut self, ev: &DataEvent) -> Reply {
        match ev {
            DataEvent::Volume(v) => {
                if !self.dragging {
                    self.level = v.level.min(100);
                }
                self.muted = v.muted;
                self.available = v.available;
                Reply::Redraw
            }
            // A late reply for a section collapsed meanwhile is dropped (nothing kept).
            DataEvent::AudioOutputs(_) if !self.layout.loadable(AUDIO_OUTPUT) => Reply::None,
            DataEvent::Spatial(_) if !self.layout.loadable(AUDIO_SPATIAL) => Reply::None,
            DataEvent::AudioSessions(_) if !self.layout.loadable(AUDIO_MIXER) => Reply::None,
            DataEvent::AudioOutputs(list) => {
                self.outputs = Some(list.clone());
                Reply::Relayout
            }
            DataEvent::Spatial(st) => {
                self.spatial = st.clone();
                self.spatial_loaded = true;
                Reply::Relayout
            }
            DataEvent::AudioSessions(list) => {
                // Keep the local level of a row being dragged (the reply may predate the last set).
                let dragged = self
                    .drag_app
                    .and_then(|i| self.sessions.as_ref().and_then(|v| v.get(i)))
                    .map(|a| (a.pid, a.system, a.level));
                let mut rows = list.clone();
                if let Some((pid, system, level)) = dragged {
                    if let Some(a) = rows.iter_mut().find(|a| a.pid == pid && a.system == system) {
                        a.level = level;
                    }
                }
                self.sessions = Some(rows);
                Reply::Relayout
            }
            _ => Reply::None,
        }
    }

    fn closing(&mut self) {
        audio::set_popup_open(false);
    }

    fn after_close(&mut self) {
        if self.open_mixer {
            launch("ms-settings:apps-volume");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(l: i32, r: i32) -> RECT {
        RECT { left: l, top: 0, right: r, bottom: 24 }
    }

    #[test]
    fn slider_levels() {
        let r = rc(100, 214); // track 107..207
        assert_eq!(level_at(&r, 7, 0), 0);
        assert_eq!(level_at(&r, 7, 107), 0);
        assert_eq!(level_at(&r, 7, 157), 50);
        assert_eq!(level_at(&r, 7, 207), 100);
        assert_eq!(level_at(&r, 7, 999), 100);
    }

    const FOOT: i32 = 6 + 1 + 6 + ROW + 8;
    const SEP: i32 = 4 + 1 + 3;

    #[test]
    fn height_grows_by_sections() {
        let px = |v: i32| v; // 96 dpi
        let l = AudioLayout::default();
        let c = |devices, spatial, apps| ListCounts { devices, spatial, apps };
        let base = popup_height(&px, &l, false, c(2, Some(0), Some(0)));
        // Master and Output only (spatial unsupported, no session): the original look plus headers.
        assert_eq!(base, TOP + HEAD + MASTER_H + SEP + HEAD + 2 * ROW + FOOT);
        let loading = popup_height(&px, &l, false, c(0, Some(0), Some(0)));
        assert_eq!(base - loading, ROW);
        let with_spatial = popup_height(&px, &l, false, c(2, Some(3), Some(0)));
        assert_eq!(with_spatial - base, SEP + HEAD + 3 * ROW);
        let full = popup_height(&px, &l, false, c(2, Some(3), Some(4)));
        assert_eq!(full - with_spatial, SEP + HEAD + 4 * ROW);
        // Not read yet: one "loading" row each.
        assert_eq!(popup_height(&px, &l, false, c(2, None, None)) - base, 2 * (SEP + HEAD + ROW));
        // IDs do not collide at the maximum counts.
        assert!(ID_HEAD + ITEMS <= ID_EYE);
        assert!(ID_EYE + ITEMS <= ID_DEVICE);
        assert!(ID_DEVICE + 99 < ID_SPATIAL);
        assert!(ID_SPATIAL + 8 < ID_APP_MUTE);
        assert!(ID_APP_MUTE + audio::MAX_SESSIONS as u32 <= ID_APP_SLIDER);
        assert!(ID_APP_SLIDER + audio::MAX_SESSIONS as u32 <= ID_END);
        assert_eq!(split(ID_EYE + 3), Some((ID_EYE, 3)));
        assert_eq!(split(ID_EDIT), None);
    }

    #[test]
    fn height_with_collapsed_hidden_and_moved_sections() {
        let px = |v: i32| v;
        let c = ListCounts { devices: 2, spatial: Some(3), apps: Some(4) };
        let full = popup_height(&px, &AudioLayout::default(), false, c);
        // Collapsed: the header stays, the rows go.
        let mut l = AudioLayout::default();
        l.toggle_open(AUDIO_OUTPUT);
        assert_eq!(full - popup_height(&px, &l, false, c), 2 * ROW);
        l.toggle_open(AUDIO_MASTER);
        assert_eq!(full - popup_height(&px, &l, false, c), 2 * ROW + MASTER_H);
        // Hidden: header and rows go; in Customize mode the header comes back (dimmed, no rows).
        let mut l = AudioLayout::default();
        l.toggle_hidden(AUDIO_MIXER);
        assert_eq!(full - popup_height(&px, &l, false, c), SEP + HEAD + 4 * ROW);
        assert_eq!(full - popup_height(&px, &l, true, c), 4 * ROW);
        // Everything hidden: only the link row is left.
        for s in 0..4 {
            l.toggle_hidden(s);
        }
        l.toggle_hidden(AUDIO_MIXER);
        assert!((0..4).all(|s| l.is_hidden(s)));
        assert_eq!(popup_height(&px, &l, false, c), TOP + FOOT);
        assert_eq!(popup_height(&px, &l, true, c), TOP + 4 * HEAD + 3 * SEP + FOOT);
        // Reordering does not change the height.
        let mut l = AudioLayout::default();
        assert!(l.move_to(AUDIO_MASTER, 3));
        assert_eq!(popup_height(&px, &l, false, c), full);
    }

    #[test]
    fn blocks_follow_the_layout() {
        let c = ListCounts { devices: 1, spatial: None, apps: None };
        let mut out = Vec::new();
        // An old grouped string: flattened in place.
        let l = AudioLayout::parse("x-MG(o)S");
        collect_blocks(&l, false, &c, &mut out);
        let items: Vec<u8> = out.iter().map(|b| b.item).collect();
        assert_eq!(items, vec![AUDIO_MIXER, AUDIO_OUTPUT, AUDIO_SPATIAL]);
        assert!(!out[0].content && !out[1].content && out[2].content);
        // Customize mode lists every section in layout order (the drag indexes rely on it).
        collect_blocks(&l, true, &c, &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out[1], Block { item: AUDIO_MASTER, content: false, dim: true });
        let empty = ListCounts { devices: 1, spatial: Some(0), apps: Some(0) };
        collect_blocks(&AudioLayout::default(), true, &empty, &mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn drop_index_follows_the_pointer() {
        // Four headers with midpoints 10, 50, 90, 130.
        let mids = [10, 50, 90, 130];
        // Dragging the second header (index 1).
        assert_eq!(drop_index(mids, 1, 0), 0);
        assert_eq!(drop_index(mids, 1, 9), 0);
        assert_eq!(drop_index(mids, 1, 30), 1); // below the first midpoint: stays
        assert_eq!(drop_index(mids, 1, 49), 1);
        assert_eq!(drop_index(mids, 1, 60), 1); // over its own place
        assert_eq!(drop_index(mids, 1, 91), 2);
        assert_eq!(drop_index(mids, 1, 500), 3);
        // First and last header.
        assert_eq!(drop_index(mids, 0, 70), 1);
        assert_eq!(drop_index(mids, 0, -5), 0);
        assert_eq!(drop_index(mids, 3, 120), 3);
        assert_eq!(drop_index(mids, 3, 11), 1);
        assert_eq!(drop_index(mids, 3, 5), 0);
        // The slot (gap) where the insertion line goes.
        assert_eq!(drop_slot(1, 0), 0);
        assert_eq!(drop_slot(1, 2), 3);
        assert_eq!(drop_slot(0, 3), 4);
        assert_eq!(drop_slot(3, 1), 1);
        // Dropping applies the index to the layout.
        let mut l = AudioLayout::default();
        assert!(l.move_to(AUDIO_OUTPUT, drop_index(mids, 1, 500)));
        assert_eq!(l.order(), &[AUDIO_MASTER, AUDIO_SPATIAL, AUDIO_MIXER, AUDIO_OUTPUT]);
    }
}
