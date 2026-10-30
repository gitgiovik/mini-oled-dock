//! START quick-links popup (right click on the Start button): the entries of the native Win+X menu
//! that make sense for the dock, drawn like the app menu (`popup_app`): same rows, hover plate and
//! separators. A click closes the popup and THEN runs the action (`after_close`).
//!
//! Every launch goes through `ShellRequest::Launch`, i.e. the desktop shell as the normal user (never
//! elevated). The two "(admin)" entries go through `ShellRequest::LaunchAdmin`: Explorer runs a fixed
//! tool with the `runas` verb and Windows shows the UAC prompt (the dock's own token is never used).
//! "Search" and "Run" are a synthetic Win+S / Win+R tagged with `INJECT_TAG`; "Sign out" is the
//! `power::PowerAction::SignOut` action. The entries are static strings: no state, timer or allocation
//! survives the popup.
//!
//! Declared as a child module of `popup` (`#[path]`).

use windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY;

use crate::i18n::tr;
use crate::popup::{center_y, hline, lift, row_plate, Content, HitMap, PopupKind, Reply};
use crate::power::{self, PowerAction};
use crate::shell_thread::{self, AdminTool, ShellRequest};
use crate::surface::{rect_wh, Font, Surface, TEXT_ALPHA, WHITE};

/// What a row does.
#[derive(Clone, Copy)]
enum Act {
    /// Launch a target through the desktop shell (de-elevated): `ms-settings:` URI, `.msc`, exe name.
    Launch(&'static str),
    /// Fixed tool started by Explorer with the `runas` verb (UAC prompt).
    Admin(AdminTool),
    /// Win + letter (`INJECT_TAG`).
    WinKey(VIRTUAL_KEY),
    /// Sign out (`power::run(PowerAction::SignOut)`).
    SignOut,
}

const VK_S: VIRTUAL_KEY = VIRTUAL_KEY(0x53);
const VK_R: VIRTUAL_KEY = VIRTUAL_KEY(0x52);

/// One row: action and label (English, Italian; picked by `i18n::tr`, like every other text).
type Row = (Act, &'static str, &'static str);

/// Rows in the order of the native Win+X menu (`None` = separator). The row id is its index + 1.
const ROWS: [Option<Row>; 18] = [
    Some((Act::Launch("ms-settings:appsfeatures"), "Installed apps", "App installate")),
    Some((Act::Launch("ms-settings:powersleep"), "Power Options", "Opzioni spegnimento")),
    Some((Act::Launch("eventvwr.msc"), "Event Viewer", "Visualizzatore eventi")),
    Some((Act::Launch("ms-settings:about"), "System", "Sistema")),
    Some((Act::Launch("devmgmt.msc"), "Device Manager", "Gestione dispositivi")),
    Some((Act::Launch("ms-settings:network"), "Network Connections", "Connessioni di rete")),
    Some((Act::Launch("diskmgmt.msc"), "Disk Management", "Gestione disco")),
    Some((Act::Launch("compmgmt.msc"), "Computer Management", "Gestione computer")),
    Some((Act::Admin(AdminTool::Terminal), "Terminal (admin)", "Terminale (admin)")),
    Some((Act::Admin(AdminTool::PowerShell), "PowerShell (admin)", "PowerShell (admin)")),
    None,
    Some((Act::Launch("taskmgr.exe"), "Task Manager", "Gestione attivit\u{e0}")),
    Some((Act::Launch("ms-settings:"), "Settings", "Impostazioni")),
    Some((Act::Launch("explorer.exe"), "File Explorer", "Esplora file")),
    Some((Act::WinKey(VK_S), "Search", "Cerca")),
    Some((Act::WinKey(VK_R), "Run", "Esegui")),
    None,
    Some((Act::SignOut, "Sign out", "Disconnetti")),
];

/// Label of row `i` in the dock language (`""` for separators).
fn label(i: usize) -> &'static str {
    ROWS.get(i).copied().flatten().map_or("", |(_, en, it)| tr(en, it))
}

pub(crate) struct StartPopup {
    hits: HitMap,
    /// Row chosen, run after closing.
    pending: Option<Act>,
    width: i32,
}

/// Creates the content (initial state). Labels follow the dock language (`i18n`; English in
/// preview mode, so the README screenshot does not depend on the machine).
pub(crate) fn new() -> Box<dyn Content> {
    Box::new(StartPopup { hits: HitMap::default(), pending: None, width: 0 })
}

/// Height taken by a separator row (physical px): space, 1 px line, space.
fn sep_h(s: &Surface) -> i32 {
    s.metrics().px(4) * 2 + s.metrics().px(1)
}

impl Content for StartPopup {
    fn kind(&self) -> PopupKind {
        PopupKind::Start
    }

    fn size(&mut self, s: &Surface) -> (i32, i32) {
        let m = s.metrics();
        let (inset, row, pad_text) = (m.px(8), m.px(34), m.px(12));
        let tw = (0..ROWS.len()).map(|i| s.text_width(label(i), Font::Normal)).max().unwrap_or(0);
        let w = (tw + 2 * pad_text + 2 * inset).clamp(m.px(190), m.px(300));
        self.width = w;
        self.hits.clear();
        let mut y = inset;
        for (i, r) in ROWS.iter().enumerate() {
            if r.is_some() {
                self.hits.add(i as u32 + 1, rect_wh(inset, y, w - 2 * inset, row));
                y += row;
            } else {
                y += sep_h(s);
            }
        }
        (w, y + inset)
    }

    fn paint(&self, s: &mut Surface, hot: Option<u32>, pressed: Option<u32>) {
        s.panel();
        let m = s.metrics();
        let (inset, pad_text) = (m.px(8), m.px(12));
        let mut y = inset;
        for (i, r) in ROWS.iter().enumerate() {
            if r.is_none() {
                hline(s, inset + pad_text, self.width - inset - pad_text, y + m.px(4));
                y += sep_h(s);
                continue;
            }
            let Some(rc) = self.hits.rect(i as u32 + 1) else { continue };
            y = rc.bottom;
            let h = row_plate(s, rc, i as u32 + 1, hot, pressed);
            let ty = center_y(&rc, s.line_height(Font::Normal));
            s.text(label(i), rc.left + pad_text, ty, Font::Normal, WHITE, lift(TEXT_ALPHA, h));
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn click(&mut self, id: u32) -> Reply {
        self.pending = ROWS.get((id as usize).wrapping_sub(1)).copied().flatten().map(|(act, _, _)| act);
        Reply::Close
    }

    fn after_close(&mut self) {
        match self.pending.take() {
            Some(Act::Launch(target)) => crate::ui::launch_target_str(target),
            Some(Act::Admin(tool)) => shell_thread::request(ShellRequest::LaunchAdmin(tool)),
            Some(Act::WinKey(vk)) => crate::ui::send_win_key(vk),
            Some(Act::SignOut) => power::run(PowerAction::SignOut),
            None => {}
        }
    }
}
