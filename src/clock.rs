//! Date/time formats of the dock clock.
//!
//! Time "HH:mm" 24h with `GetTimeFormatEx` (user locale, `LOCALE_NAME_USER_DEFAULT`) and short date
//! like "Fri 23 Oct" with `GetDateFormatEx` (custom format "ddd d MMM").
//! THREADING: UI thread only (no function blocks). `WM_SETTINGCHANGE` with lParam
//! "intl" -> `invalidate_formats()`.

use std::sync::Mutex;

use windows::core::PCWSTR; // locale NULL = LOCALE_NAME_USER_DEFAULT
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{GetDateFormatEx, GetTimeFormatEx, ENUM_DATE_FORMATS_FLAGS, TIME_FORMAT_FLAGS};
use windows::Win32::System::SystemInformation::GetLocalTime;

/// Already formatted clock texts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClockText {
    /// "HH:mm"
    pub time: String,
    /// Short date, e.g. "Fri 23 Oct".
    pub date: String,
}

/// Last result per minute: avoids NLS calls if the minute/day have not changed.
/// Key = (year, month, day, hour, minute). Cleared by `invalidate_formats`.
type CacheEntry = ((u16, u16, u16, u16, u16, u16), String, String, ClockText);
static CACHE: Mutex<Option<CacheEntry>> = Mutex::new(None);

fn wide(buf: &[u16], n: i32) -> Option<String> {
    // n includes the null terminator; 0 = error.
    if n <= 1 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..(n as usize - 1).min(buf.len())]))
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Effective time picture: `time_format`, plus ":ss" when `show_seconds` and it has no "s".
fn time_picture(cfg: &crate::config::Config) -> String {
    let base = if cfg.time_format.trim().is_empty() { "HH:mm" } else { cfg.time_format.as_str() };
    if cfg.show_seconds && !base.contains('s') {
        format!("{base}:ss")
    } else {
        base.to_string()
    }
}

/// True when the displayed time includes seconds: `show_seconds`, or an `s` outside the
/// single-quoted literals of `time_format`.
fn seconds_on(cfg: &crate::config::Config) -> bool {
    if cfg.show_seconds {
        return true;
    }
    let mut quoted = false;
    cfg.time_format.chars().any(|c| {
        if c == '\'' {
            quoted = !quoted;
        }
        !quoted && c == 's'
    })
}

fn fmt_time(st: &SYSTEMTIME, pic: &str) -> Option<String> {
    let mut buf = [0u16; 64];
    let p = to_wide(pic);
    // SAFETY: `st`, `buf` and `p` (nul-terminated) are valid for the call; locale: `i18n::locale`
    // (NULL = user default, or a static nul-terminated name).
    let loc = crate::i18n::locale();
    let n = unsafe { GetTimeFormatEx(loc, TIME_FORMAT_FLAGS(0), Some(st), PCWSTR(p.as_ptr()), Some(&mut buf)) };
    wide(&buf, n)
}

fn fmt_date(st: &SYSTEMTIME, pic: &str) -> Option<String> {
    let mut buf = [0u16; 128];
    let p = to_wide(pic);
    // SAFETY: as in `fmt_time`; calendar NULL = locale default.
    let n = unsafe {
        GetDateFormatEx(
            crate::i18n::locale(),
            ENUM_DATE_FORMATS_FLAGS(0),
            Some(st),
            PCWSTR(p.as_ptr()),
            Some(&mut buf),
            PCWSTR::null(),
        )
    };
    wide(&buf, n)
}

/// Formats the current local instant using the configured pictures (user locale).
/// Falls back to fixed "HH:mm[:ss]" for time and empty for date if NLS fails.
pub fn format_now() -> ClockText {
    // SAFETY: GetLocalTime has no preconditions.
    let t = unsafe { GetLocalTime() };
    let cfg = crate::config::get();
    let secs_on = seconds_on(&cfg);
    // u16::MAX marks "no seconds" so toggling seconds always misses the cache.
    let secs = if secs_on { t.wSecond } else { u16::MAX };
    let key = (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, secs);
    if let Ok(g) = CACHE.lock() {
        if let Some((k, tf, df, txt)) = g.as_ref() {
            if *k == key && *tf == cfg.time_format && *df == cfg.date_format {
                return txt.clone();
            }
        }
    }
    let tp = time_picture(&cfg);
    let txt = ClockText {
        time: fmt_time(&t, &tp).unwrap_or_else(|| {
            if secs_on {
                format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
            } else {
                format!("{:02}:{:02}", t.wHour, t.wMinute)
            }
        }),
        date: if cfg.date_format.is_empty() {
            String::new()
        } else {
            fmt_date(&t, &cfg.date_format).unwrap_or_default()
        },
    };
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((key, cfg.time_format.clone(), cfg.date_format.clone(), txt.clone()));
    }
    txt
}

/// Timer period for `TIMER_CLOCK`: ms to the next second when seconds are shown, else to the
/// next minute (at least 50).
pub fn tick_ms() -> u32 {
    // SAFETY: GetLocalTime has no preconditions.
    let t = unsafe { GetLocalTime() };
    if seconds_on(&crate::config::get()) {
        (1000u32.saturating_sub(t.wMilliseconds as u32)).max(50)
    } else {
        let into = (t.wSecond as u32) * 1000 + t.wMilliseconds as u32;
        (60_000u32.saturating_sub(into)).max(50)
    }
}

/// Invalidates the text cache (called on `WM_SETTINGCHANGE` with "intl"): the next
/// `format_now` rereads the user locale.
pub fn invalidate_formats() {
    if let Ok(mut g) = CACHE.lock() {
        *g = None;
    }
}
