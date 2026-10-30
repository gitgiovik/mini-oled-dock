//! Clock popup calendar: month math (pure, testable) and localized
//! texts (NLS: `GetLocaleInfoEx`, `GetDateFormatEx`, `GetCalendarInfoEx`).
//!
//!
//! Weekday convention: `sun0` = 0 Sunday .. 6 Saturday (like `SYSTEMTIME.wDayOfWeek`).
//! `LOCALE_IFIRSTDAYOFWEEK` instead is 0 = Monday .. 6 = Sunday: convert with `(v + 1) % 7`.
//!
//! THREADING: UI thread only; the NLS calls do not block.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{
    GetCalendarInfoEx, GetDateFormatEx, GetLocaleInfoEx, CAL_GREGORIAN, CAL_SSHORTESTDAYNAME1, DATE_LONGDATE,
    ENUM_DATE_FORMATS_FLAGS, LOCALE_IFIRSTDAYOFWEEK, LOCALE_RETURN_NUMBER,
};
use windows::Win32::System::SystemInformation::GetLocalTime;

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use crate::log;

/// Preview mode (`--preview`): fixed `en-US` locale, date and time, so the screenshots are
/// reproducible and do not depend on the machine they are rendered on.
static PREVIEW: AtomicBool = AtomicBool::new(false);

/// Enables the fixed locale / date / time used by `--preview` (and English labels, `i18n`).
pub fn set_preview(on: bool) {
    PREVIEW.store(on, Relaxed);
    crate::i18n::set_preview(on);
}

/// Locale name for the NLS calls: the dock language (`i18n::locale`; `en-US` in preview mode).
fn locale() -> PCWSTR {
    crate::i18n::locale()
}

/// One cell of the 6x7 grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DayCell {
    /// Day of the month 1..=31.
    pub day: u16,
    /// false for days of the previous/next month (drawn with `TEXT3_ALPHA`).
    pub in_month: bool,
    /// true for today's date (white plate, black text).
    pub today: bool,
}

/// true if `y` is a leap year (Gregorian).
pub fn is_leap(y: u16) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

/// Days of month `m` (1..=12) of year `y`.
pub fn days_in_month(y: u16, m: u16) -> u16 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 30,
    }
}

/// Weekday (0 = Sunday) of `y-m-d` (Sakamoto's algorithm).
pub fn weekday_sun0(y: u16, m: u16, d: u16) -> u32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let m = m.clamp(1, 12);
    let mut yy = y as i32;
    if m < 3 {
        yy -= 1;
    }
    (yy + yy / 4 - yy / 100 + yy / 400 + T[(m - 1) as usize] + d as i32).rem_euclid(7) as u32
}

/// `(y, m)` shifted by `delta` months (also negative). The year stays within 1601..=9999.
pub fn add_months(y: u16, m: u16, delta: i32) -> (u16, u16) {
    let idx = y as i32 * 12 + (m.clamp(1, 12) as i32 - 1) + delta;
    let idx = idx.clamp(1601 * 12, 9999 * 12 + 11);
    ((idx / 12) as u16, (idx % 12) as u16 + 1)
}

/// Fixed 6x7 grid of month `(y, m)`, first column = `first_sun0`; `today` = (year, month, day).
/// Pure.
pub fn month_grid(y: u16, m: u16, today: (u16, u16, u16), first_sun0: u32) -> [DayCell; 42] {
    let mut out = [DayCell::default(); 42];
    let first_wd = weekday_sun0(y, m, 1) as i32;
    let offset = (first_wd + 7 - (first_sun0 % 7) as i32) % 7;
    let dim = days_in_month(y, m) as i32;
    let (py, pm) = add_months(y, m, -1);
    let pdim = days_in_month(py, pm) as i32;
    for (i, cell) in out.iter_mut().enumerate() {
        let d = i as i32 - offset + 1;
        if d < 1 {
            *cell = DayCell { day: (pdim + d) as u16, in_month: false, today: false };
        } else if d > dim {
            *cell = DayCell { day: (d - dim) as u16, in_month: false, today: false };
        } else {
            *cell = DayCell { day: d as u16, in_month: true, today: (y, m, d as u16) == today };
        }
    }
    out
}

/// First day of the week of the user locale in `sun0` convention (e.g. it-IT: 1 = Monday).
/// Falls back to 1 if the NLS call fails.
pub fn first_day_of_week_sun0() -> u32 {
    let mut buf = [0u16; 4];
    // SAFETY: `buf` (4 u16 = 8 bytes) is large enough for the DWORD that LOCALE_RETURN_NUMBER
    // writes into the buffer; NULL locale name = user locale.
    let n = unsafe { GetLocaleInfoEx(locale(), LOCALE_IFIRSTDAYOFWEEK | LOCALE_RETURN_NUMBER, Some(&mut buf)) };
    if n <= 0 {
        log!("calendar: GetLocaleInfoEx(IFIRSTDAYOFWEEK) failed, using Monday");
        return 1;
    }
    let v = buf[0] as u32 | ((buf[1] as u32) << 16);
    if v > 6 {
        return 1;
    }
    (v + 1) % 7
}

/// Short day headers (English fallback: "Mo", "Tu", ...) starting from `first_sun0`
/// (`CAL_SSHORTESTDAYNAME1..7`, note: 1 is Monday).
pub fn weekday_headers(first_sun0: u32) -> [String; 7] {
    const FALLBACK: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
    const FALLBACK_IT: [&str; 7] = ["Lu", "Ma", "Me", "Gi", "Ve", "Sa", "Do"];
    let mut out: [String; 7] = Default::default();
    for (i, slot) in out.iter_mut().enumerate() {
        // column i -> sun0 day -> index with Monday = 0
        let sun0 = (first_sun0 as usize + i) % 7;
        let mon0 = (sun0 + 6) % 7;
        let mut buf = [0u16; 32];
        // SAFETY: `buf` valid for the duration of the call; no numeric value requested
        // (lpvalue absent); NULL locale = user, reserved parameter NULL.
        let n = unsafe {
            GetCalendarInfoEx(
                locale(),
                CAL_GREGORIAN,
                PCWSTR::null(),
                CAL_SSHORTESTDAYNAME1 + mon0 as u32,
                Some(&mut buf),
                None,
            )
        };
        *slot = if n > 1 {
            String::from_utf16_lossy(&buf[..(n as usize - 1).min(buf.len())])
        } else {
            if crate::i18n::italian() { FALLBACK_IT[mon0] } else { FALLBACK[mon0] }.to_string()
        };
    }
    out
}

/// First letter uppercase.
fn capitalize(s: &str) -> String {
    let mut it = s.chars();
    match it.next() {
        Some(c) => c.to_uppercase().collect::<String>() + it.as_str(),
        None => String::new(),
    }
}

/// Month title with initial capital, e.g. "October 2026" (`GetDateFormatEx` "MMMM yyyy").
pub fn month_title(y: u16, m: u16) -> String {
    let st = SYSTEMTIME { wYear: y, wMonth: m, wDay: 1, ..Default::default() };
    let mut buf = [0u16; 64];
    // SAFETY: `st` and `buf` valid for the duration of the call; static NUL-terminated format.
    let n = unsafe {
        GetDateFormatEx(
            locale(),
            ENUM_DATE_FORMATS_FLAGS(0),
            Some(&st),
            w!("MMMM yyyy"),
            Some(&mut buf),
            PCWSTR::null(),
        )
    };
    if n > 1 {
        capitalize(&String::from_utf16_lossy(&buf[..(n as usize - 1).min(buf.len())]))
    } else {
        format!("{m:02}/{y}")
    }
}

/// Today's long date with initial capital, e.g. "Friday 23 October 2026" (`DATE_LONGDATE`).
pub fn long_date_today() -> String {
    let mut buf = [0u16; 96];
    // SAFETY: NULL date = today; `buf` valid; NULL format = the locale's long format (DATE_LONGDATE flag).
    let n = unsafe {
        GetDateFormatEx(
            locale(),
            DATE_LONGDATE,
            Some(&today_systemtime()),
            PCWSTR::null(),
            Some(&mut buf),
            PCWSTR::null(),
        )
    };
    if n > 1 {
        capitalize(&String::from_utf16_lossy(&buf[..(n as usize - 1).min(buf.len())]))
    } else {
        String::new()
    }
}

/// Current time "HH:mm" (reuses `clock::format_now().time`).
pub fn time_now() -> String {
    if PREVIEW.load(Relaxed) {
        return "14:32".to_string();
    }
    crate::clock::format_now().time
}

/// Local today (year, month, day) from `GetLocalTime`.
pub fn today() -> (u16, u16, u16) {
    let t = today_systemtime();
    (t.wYear, t.wMonth, t.wDay)
}

/// Local date as a `SYSTEMTIME` (fixed 2026-10-23 in preview mode).
fn today_systemtime() -> SYSTEMTIME {
    if PREVIEW.load(Relaxed) {
        return SYSTEMTIME { wYear: 2026, wMonth: 10, wDay: 23, wDayOfWeek: 5, ..Default::default() };
    }
    // SAFETY: GetLocalTime has no preconditions.
    unsafe { GetLocalTime() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leap_years() {
        assert!(is_leap(2024));
        assert!(!is_leap(1900));
        assert!(is_leap(2000));
        assert!(!is_leap(2026));
    }

    #[test]
    fn days_in_month_values() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 10), 31);
    }

    #[test]
    fn weekday() {
        assert_eq!(weekday_sun0(2026, 10, 23), 5); // Friday
        assert_eq!(weekday_sun0(2000, 1, 1), 6); // Saturday
        assert_eq!(weekday_sun0(2024, 2, 29), 4); // Thursday
        assert_eq!(weekday_sun0(1970, 1, 1), 4); // Thursday
    }

    #[test]
    fn months() {
        assert_eq!(add_months(2026, 12, 1), (2027, 1));
        assert_eq!(add_months(2026, 1, -1), (2025, 12));
        assert_eq!(add_months(2026, 10, 14), (2027, 12));
        assert_eq!(add_months(2026, 10, -22), (2024, 12));
        assert_eq!(add_months(2026, 10, 0), (2026, 10));
    }

    #[test]
    fn grid_october_2026_monday() {
        // 1 October 2026 = Thursday: with the week starting on Monday it falls in column 3.
        let g = month_grid(2026, 10, (2026, 10, 23), 1);
        assert_eq!(g[0], DayCell { day: 28, in_month: false, today: false });
        assert_eq!(g[2], DayCell { day: 30, in_month: false, today: false });
        assert_eq!(g[3], DayCell { day: 1, in_month: true, today: false });
        let t = g.iter().find(|c| c.today).copied();
        assert_eq!(t, Some(DayCell { day: 23, in_month: true, today: true }));
        assert_eq!(g[3 + 30], DayCell { day: 31, in_month: true, today: false });
        assert_eq!(g[34], DayCell { day: 1, in_month: false, today: false });
        assert_eq!(g[41].day, 8);
    }

    #[test]
    fn grid_sunday_first() {
        // With Sunday first, 1 October 2026 (Thursday) falls in column 4.
        let g = month_grid(2026, 10, (2000, 1, 1), 0);
        assert_eq!(g[4].day, 1);
        assert!(g[4].in_month);
        assert!(g.iter().all(|c| !c.today));
    }

    #[test]
    fn grid_month_starting_on_monday() {
        // June 2026 starts on a Monday: no day of the previous month in the first row.
        let g = month_grid(2026, 6, (2026, 6, 1), 1);
        assert_eq!(g[0], DayCell { day: 1, in_month: true, today: true });
    }

    #[test]
    fn grid_january_uses_december() {
        let g = month_grid(2026, 1, (2026, 1, 1), 1);
        // 1 January 2026 = Thursday -> column 3, cell 0 = 29 December.
        assert_eq!(g[0].day, 29);
        assert!(!g[0].in_month);
    }
}
