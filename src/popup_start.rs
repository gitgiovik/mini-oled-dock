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

use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY;

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
/// `PRIMARYLANGID` of Italian (`LANG_ITALIAN`; its feature module is not enabled for one constant).
const LANG_ITALIAN: u16 = 0x10;

/// Rows in the order of the native Win+X menu (`None` = separator). The row id is its index + 1.
const ROWS: [Option<Act>; 18] = [
    Some(Act::Launch("ms-settings:appsfeatures")),
    Some(Act::Launch("ms-settings:powersleep")),
    Some(Act::Launch("eventvwr.msc")),
    Some(Act::Launch("ms-settings:about")),
    Some(Act::Launch("devmgmt.msc")),
    Some(Act::Launch("ms-settings:network")),
    Some(Act::Launch("diskmgmt.msc")),
    Some(Act::Launch("compmgmt.msc")),
    Some(Act::Admin(AdminTool::Terminal)),
    Some(Act::Admin(AdminTool::PowerShell)),
    None,
    Some(Act::Launch("taskmgr.exe")),
    Some(Act::Launch("ms-settings:")),
    Some(Act::Launch("explorer.exe")),
    Some(Act::WinKey(VK_S)),
    Some(Act::WinKey(VK_R)),
    None,
    Some(Act::SignOut),
];

// This menu is the only localized part of the dock (Italian UI language, English otherwise).
/// English labels by row index (`""` = separator).
const LABELS_EN: &[&str] = &[
    "Installed apps",
    "Power Options",
    "Event Viewer",
    "System",
    "Device Manager",
    "Network Connections",
    "Disk Management",
    "Computer Management",
    "Terminal (admin)",
    "PowerShell (admin)",
    "",
    "Task Manager",
    "Settings",
    "File Explorer",
    "Search",
    "Run",
    "",
    "Sign out",
];

/// Italian labels by row index (`""` = separator).
const LABELS_IT: &[&str] = &[
    "App installate",
    "Opzioni spegnimento",
    "Visualizzatore eventi",
    "Sistema",
    "Gestione dispositivi",
    "Connessioni di rete",
    "Gestione disco",
    "Gestione computer",
    "Terminale (admin)",
    "PowerShell (admin)",
    "",
    "Gestione attivit\u{e0}",
    "Impostazioni",
    "Esplora file",
    "Cerca",
    "Esegui",
    "",
    "Disconnetti",
];

/// Label of row `i` in the chosen language (`""` for separators).
fn label(i: usize, italian: bool) -> &'static str {
    let table = if italian { LABELS_IT } else { LABELS_EN };
    table.get(i).copied().unwrap_or("")
}

/// True when the user's Windows UI language is Italian (`PRIMARYLANGID == LANG_ITALIAN`).
pub(crate) fn ui_is_italian() -> bool {
    // SAFETY: no arguments, no preconditions; returns a LANGID.
    let langid = unsafe { GetUserDefaultUILanguage() };
    langid & 0x3ff == LANG_ITALIAN
}

pub(crate) struct StartPopup {
    hits: HitMap,
    /// Row chosen, run after closing.
    pending: Option<Act>,
    width: i32,
    /// Label language, read when the popup is built.
    italian: bool,
}

/// Creates the content (initial state), labels in the user's UI language.
pub(crate) fn new() -> Box<dyn Content> {
    with_language(ui_is_italian())
}

/// Preview: always English, so the README screenshot does not depend on the machine.
pub(crate) fn preview_new() -> Box<dyn Content> {
    with_language(false)
}

fn with_language(italian: bool) -> Box<dyn Content> {
    Box::new(StartPopup { hits: HitMap::default(), pending: None, width: 0, italian })
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
        let tw = (0..ROWS.len()).map(|i| s.text_width(label(i, self.italian), Font::Normal)).max().unwrap_or(0);
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
            s.text(label(i, self.italian), rc.left + pad_text, ty, Font::Normal, WHITE, lift(TEXT_ALPHA, h));
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<u32> {
        self.hits.at(x, y)
    }

    fn click(&mut self, id: u32) -> Reply {
        self.pending = ROWS.get((id as usize).wrapping_sub(1)).copied().flatten();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_tables_match_rows() {
        assert_eq!(LABELS_EN.len(), ROWS.len());
        assert_eq!(LABELS_IT.len(), ROWS.len());
        for (i, r) in ROWS.iter().enumerate() {
            // Separators have no label; every action has one in both languages.
            assert_eq!(r.is_none(), LABELS_EN[i].is_empty(), "row {i}");
            assert_eq!(r.is_none(), LABELS_IT[i].is_empty(), "row {i}");
        }
    }
}
