//! Times and durations as the UI says them.

use chrono::{DateTime, Datelike, Local, TimeZone};

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// "1주", "8시간", "30초" for an expire timer in seconds.
pub fn timer_label(secs: u32) -> String {
    const UNITS: [(u32, &str); 4] = [(604_800, "주"), (86_400, "일"), (3_600, "시간"), (60, "분")];
    for (n, u) in UNITS {
        if secs >= n && secs % n == 0 {
            return format!("{}{u}", secs / n);
        }
    }
    format!("{secs}초")
}

pub(super) fn local(ts: u64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ts as i64).single().unwrap_or_else(Local::now)
}

pub(super) fn ampm(dt: &DateTime<Local>) -> String {
    let h = dt.format("%H").to_string().parse::<u32>().unwrap_or(0);
    let (p, h12) = match h {
        0 => ("오전", 12),
        1..=11 => ("오전", h),
        12 => ("오후", 12),
        _ => ("오후", h - 12),
    };
    format!("{p} {h12}:{}", dt.format("%M"))
}

pub fn time_short(ts: u64) -> String {
    if ts == 0 {
        return String::new();
    }
    let dt = local(ts);
    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        match (now - dt).num_minutes() {
            m if m < 1 => "방금".into(),
            m if m < 60 => format!("{m}분"),
            _ => ampm(&dt),
        }
    } else if (now.date_naive() - dt.date_naive()).num_days() == 1 {
        "어제".into()
    } else if dt.year() == now.year() {
        format!("{}월 {}일", dt.month(), dt.day())
    } else {
        format!("{}. {}. {}.", dt.year(), dt.month(), dt.day())
    }
}

/// A future moment: "오후 3:00" today, "내일 오후 3:00", "11월 6일 오후 3:00", with the year
/// when it differs.
pub fn time_until(ts: u64) -> String {
    let dt = local(ts);
    let now = Local::now();
    let days = (dt.date_naive() - now.date_naive()).num_days();
    match days {
        0 => ampm(&dt),
        1 => format!("내일 {}", ampm(&dt)),
        _ if dt.year() == now.year() => format!("{}월 {}일 {}", dt.month(), dt.day(), ampm(&dt)),
        _ => format!("{}년 {}월 {}일 {}", dt.year(), dt.month(), dt.day(), ampm(&dt)),
    }
}

/// Bubble time: "방금" / "3분" for the last hour, else the absolute `abs` ("오후 3:12 · 수정됨").
pub fn bubble_time(ts: u64, abs: &str) -> String {
    let mins = (Local::now() - local(ts)).num_minutes();
    if !(0..60).contains(&mins) && mins >= 0 {
        return abs.to_string();
    }
    let rel = if mins < 1 { "방금".to_string() } else { format!("{mins}분") };
    match abs.split_once(" · ") {
        Some((_, tail)) => format!("{rel} · {tail}"),
        None => rel,
    }
}

pub(super) fn day_label(dt: &DateTime<Local>) -> String {
    const WD: [&str; 7] = ["월", "화", "수", "목", "금", "토", "일"];
    format!(
        "{}년 {}월 {}일 {}요일",
        dt.year(),
        dt.month(),
        dt.day(),
        WD[dt.weekday().num_days_from_monday() as usize]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_labels() {
        assert_eq!(timer_label(30), "30초");
        assert_eq!(timer_label(300), "5분");
        assert_eq!(timer_label(8 * 3600), "8시간");
        assert_eq!(timer_label(86_400), "1일");
        assert_eq!(timer_label(4 * 604_800), "4주");
        assert_eq!(timer_label(90), "90초");
    }
}
