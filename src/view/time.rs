//! Times and durations as the UI says them. Words go through the catalogs; the order of a date's
//! parts and the names of months and weekdays are per language here (`i18n::lang`).

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};

use crate::i18n;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// "1 week", "8 hours", "30 seconds" for an expire timer in seconds.
pub fn timer_label(secs: u32) -> String {
    let whole = |unit: u32| (secs >= unit && secs % unit == 0).then_some(secs / unit);
    if let Some(n) = whole(604_800) {
        tr!("{n} week" | "{n} weeks" % n)
    } else if let Some(n) = whole(86_400) {
        tr!("{n} day" | "{n} days" % n)
    } else if let Some(n) = whole(3_600) {
        tr!("{n} hour" | "{n} hours" % n)
    } else if let Some(n) = whole(60) {
        tr!("{n} minute" | "{n} minutes" % n)
    } else {
        tr!("{n} second" | "{n} seconds" % secs)
    }
}

pub(super) fn local(ts: u64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ts as i64).single().unwrap_or_else(Local::now)
}

/// A clock time: "3:12 PM", "오후 3:12".
pub(super) fn ampm(dt: &DateTime<Local>) -> String {
    let (pm, h12) = dt.hour12();
    let m = dt.minute();
    match i18n::lang() {
        "ko" => format!("{} {h12}:{m:02}", if pm { "오후" } else { "오전" }),
        _ => format!("{h12}:{m:02} {}", if pm { "PM" } else { "AM" }),
    }
}

/// A day: "Nov 7" / "11월 7일", with the year and the weekday when asked.
fn date(d: NaiveDate, year: bool, weekday: bool) -> String {
    match i18n::lang() {
        "ko" => {
            const WD: [&str; 7] = ["월", "화", "수", "목", "금", "토", "일"];
            let y = if year { format!("{}년 ", d.year()) } else { String::new() };
            let wd = if weekday { format!(" ({})", WD[d.weekday().num_days_from_monday() as usize]) } else { String::new() };
            format!("{y}{}월 {}일{wd}", d.month(), d.day())
        }
        _ => {
            let wd = if weekday { d.format("%a, ").to_string() } else { String::new() };
            let y = if year { format!(", {}", d.year()) } else { String::new() };
            format!("{wd}{}{y}", d.format("%b %-d"))
        }
    }
}

pub fn time_short(ts: u64) -> String {
    if ts == 0 {
        return String::new();
    }
    let dt = local(ts);
    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        match (now - dt).num_minutes() {
            m if m < 1 => tr!("Now").into(),
            m if m < 60 => tr!("{}m", m),
            _ => ampm(&dt),
        }
    } else if (now.date_naive() - dt.date_naive()).num_days() == 1 {
        tr!("Yesterday").into()
    } else if dt.year() == now.year() {
        date(dt.date_naive(), false, false)
    } else if i18n::lang() == "ko" {
        format!("{}. {}. {}.", dt.year(), dt.month(), dt.day())
    } else {
        dt.format("%-m/%-d/%y").to_string()
    }
}

/// A future moment: "3:00 PM" today, "Tomorrow 3:00 PM", "Nov 6 3:00 PM", with the year when it
/// differs.
pub fn time_until(ts: u64) -> String {
    let dt = local(ts);
    let now = Local::now();
    let days = (dt.date_naive() - now.date_naive()).num_days();
    match days {
        0 => ampm(&dt),
        1 => tr!("Tomorrow {}", ampm(&dt)),
        _ => format!("{} {}", date(dt.date_naive(), dt.year() != now.year(), false), ampm(&dt)),
    }
}

/// Bubble time: "Now" / "3m" for the last hour, else the absolute `abs` ("3:12 PM · edited").
pub fn bubble_time(ts: u64, abs: &str) -> String {
    let mins = (Local::now() - local(ts)).num_minutes();
    if !(0..60).contains(&mins) && mins >= 0 {
        return abs.to_string();
    }
    let rel = if mins < 1 { tr!("Now").to_string() } else { tr!("{}m", mins) };
    match abs.split_once(" · ") {
        Some((_, tail)) => format!("{rel} · {tail}"),
        None => rel,
    }
}

/// A local day as a number (days since 0001-01-01), the form rows carry.
pub fn day_num(dt: &DateTime<Local>) -> i32 {
    dt.date_naive().num_days_from_ce()
}

/// Milliseconds at which local day `day` (`day_num`) starts.
pub fn day_start_ms(day: i32) -> Option<u64> {
    let d = NaiveDate::from_num_days_from_ce_opt(day).filter(|_| day > 0)?;
    let start = Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest()?;
    u64::try_from(start.timestamp_millis()).ok()
}

/// The date shown while scrolling: "Today", "Yesterday", "Sat, Nov 7", with the year when it
/// differs.
pub fn scroll_day_label(day: i32) -> String {
    // 0: no day
    let Some(d) = NaiveDate::from_num_days_from_ce_opt(day).filter(|_| day > 0) else { return String::new() };
    let now = Local::now().date_naive();
    match (now - d).num_days() {
        0 => tr!("Today").into(),
        1 => tr!("Yesterday").into(),
        _ => date(d, d.year() != now.year(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // tests run in English, the source language (`i18n::CURRENT` starts there)
    #[test]
    fn scroll_day_labels() {
        let today = day_num(&Local::now());
        assert_eq!(scroll_day_label(today), "Today");
        assert_eq!(scroll_day_label(today - 1), "Yesterday");
        let d = NaiveDate::from_ymd_opt(2020, 11, 7).unwrap().num_days_from_ce();
        assert_eq!(scroll_day_label(d), "Sat, Nov 7, 2020");
        let start = day_start_ms(d).unwrap();
        assert_eq!(day_num(&local(start)), d);
        assert_eq!(day_num(&local(start - 1)), d - 1);
    }

    #[test]
    fn timer_labels() {
        assert_eq!(timer_label(30), "30 seconds");
        assert_eq!(timer_label(300), "5 minutes");
        assert_eq!(timer_label(8 * 3600), "8 hours");
        assert_eq!(timer_label(86_400), "1 day");
        assert_eq!(timer_label(4 * 604_800), "4 weeks");
        assert_eq!(timer_label(90), "90 seconds");
    }
}
