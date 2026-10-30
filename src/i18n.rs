//! UI language of the whole dock: English or Italian, chosen in Settings (`config.language`).
//!
//! `"auto"` (the default) follows the Windows display language: Italian when it is Italian, English
//! otherwise. Every visible string is a pair of `&'static str` compiled into the binary and picked by
//! `tr(en, it)`: no table, no allocation, no file; the only state is one byte. `apply` is called by
//! `config::set`, so every way the config changes (start, Settings, hand edit) switches the language;
//! the bar is rebuilt and every popup reads the labels again when it is (re)built.
//!
//! The NLS calls (clock, date, calendar day and month names) use `locale()`: the user locale with
//! `"auto"`, `en-US` / `it-IT` when a language is chosen, so names follow the dock language too.
//! Preview mode (`--preview`) is always English.

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

use windows::core::{w, PCWSTR};
use windows::Win32::Globalization::GetUserDefaultUILanguage;

use crate::config::Language;

/// `PRIMARYLANGID` of Italian (`LANG_ITALIAN`; its feature module is not enabled for one constant).
const LANG_ITALIAN: u16 = 0x10;

/// Bit 0: Italian. Bit 1: chosen explicitly (not `auto`). Bit 2: preview mode (English, `en-US`).
static STATE: AtomicU8 = AtomicU8::new(0);
const IT: u8 = 1;
const EXPLICIT: u8 = 2;
const PREVIEW: u8 = 4;

/// True when the user's Windows UI language is Italian (`PRIMARYLANGID == LANG_ITALIAN`).
fn windows_is_italian() -> bool {
    // SAFETY: no arguments, no preconditions; returns a LANGID.
    let langid = unsafe { GetUserDefaultUILanguage() };
    langid & 0x3ff == LANG_ITALIAN
}

/// Sets the language from the config value (called by `config::set`).
pub fn apply(lang: Language) {
    let bits = match lang {
        Language::Auto => u8::from(windows_is_italian()) * IT,
        Language::En => EXPLICIT,
        Language::It => EXPLICIT | IT,
    };
    STATE.store(bits | (STATE.load(Relaxed) & PREVIEW), Relaxed);
}

/// Preview mode (`--preview`): English labels and `en-US` names whatever the config says.
pub fn set_preview(on: bool) {
    if on {
        STATE.fetch_or(PREVIEW, Relaxed);
    } else {
        STATE.fetch_and(!PREVIEW, Relaxed);
    }
}

/// True when the dock speaks Italian.
pub fn italian() -> bool {
    italian_in(STATE.load(Relaxed))
}

fn italian_in(s: u8) -> bool {
    s & PREVIEW == 0 && s & IT != 0
}

/// The string in the dock language.
#[inline]
pub fn tr(en: &'static str, it: &'static str) -> &'static str {
    if italian() {
        it
    } else {
        en
    }
}

/// Locale name for the NLS calls (time, date, day and month names): NULL = the user locale
/// (language `auto`), `en-US` / `it-IT` for a chosen language, `en-US` in preview mode.
pub fn locale() -> PCWSTR {
    locale_in(STATE.load(Relaxed))
}

fn locale_in(s: u8) -> PCWSTR {
    if s & PREVIEW != 0 {
        w!("en-US")
    } else if s & EXPLICIT == 0 {
        PCWSTR::null()
    } else if s & IT != 0 {
        w!("it-IT")
    } else {
        w!("en-US")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure functions on a state value: the global byte is never touched, so tests running in
    // parallel keep seeing English.
    fn name(p: PCWSTR) -> String {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: the non-null values are the nul-terminated `w!` literals above.
            unsafe { p.to_string() }.unwrap_or_default()
        }
    }

    #[test]
    fn states() {
        assert!(!italian_in(0));
        assert!(italian_in(IT));
        assert!(italian_in(EXPLICIT | IT));
        assert!(!italian_in(EXPLICIT | IT | PREVIEW));
        assert_eq!(name(locale_in(0)), "");
        assert_eq!(name(locale_in(IT)), "");
        assert_eq!(name(locale_in(EXPLICIT)), "en-US");
        assert_eq!(name(locale_in(EXPLICIT | IT)), "it-IT");
        assert_eq!(name(locale_in(EXPLICIT | IT | PREVIEW)), "en-US");
        assert_eq!(name(locale_in(PREVIEW)), "en-US");
    }
}
