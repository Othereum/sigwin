//! The date picker (`ui/calendar.slint`): a month's days or a year's months, the ones holding
//! no messages dimmed. Which do comes from the backend, one view at a time
//! (`Command::CalendarMarks`); a pick goes through `show-day`, like a click on the date pill. It also
//! picks the search's "until" date, then with every day up to today open.

use chrono::{Datelike, Local, NaiveDate};
use sigwin_ui::{CalendarCell, CalendarData, SearchData};

use super::*;
use crate::view::time;

#[derive(Clone, Copy, PartialEq)]
enum View {
    Days { year: i32, month: u32 },
    Months { year: i32 },
}

struct Cal {
    /// the thread it was opened for
    key: String,
    /// picking the search's "until" date: every day up to today, no marks to ask for
    until: bool,
    view: View,
    /// the day highlighted: the one on screen when it opened (0: none)
    selected: i32,
    /// per span of `bounds()`, whether it holds messages; None until the backend answers
    marks: Option<Vec<bool>>,
    /// the day of the thread's oldest message; None while unknown
    first: Option<i32>,
}

thread_local! {
    static CAL: RefCell<Option<Cal>> = const { RefCell::new(None) };
}

pub(crate) fn wire(app: &AppWindow) {
    let d = app.global::<CalendarData>();
    let weak = app.as_weak();
    d.on_show(move |day, x, y| {
        if let Some(app) = weak.upgrade() {
            open(&app, day, x, y);
        }
    });
    let weak = app.as_weak();
    d.on_show_until(move |x, y| {
        if let Some(app) = weak.upgrade() {
            open_until(&app, x, y);
        }
    });
    let weak = app.as_weak();
    d.on_step(move |by| {
        let Some(app) = weak.upgrade() else { return };
        change(&app, |v| match v {
            View::Days { year, month } => {
                let m = year * 12 + month as i32 - 1 + by.signum();
                View::Days { year: m.div_euclid(12), month: m.rem_euclid(12) as u32 + 1 }
            }
            View::Months { year } => View::Months { year: year + by.signum() },
        });
    });
    let weak = app.as_weak();
    d.on_zoom_out(move || {
        let Some(app) = weak.upgrade() else { return };
        change(&app, |v| match v {
            View::Days { year, .. } => View::Months { year },
            v => v,
        });
    });
    let weak = app.as_weak();
    d.on_pick(move |value| {
        let Some(app) = weak.upgrade() else { return };
        let Some((view, until)) = CAL.with_borrow(|c| c.as_ref().map(|c| (c.view, c.until))) else { return };
        match view {
            View::Months { year } => change(&app, |_| View::Days { year, month: value.clamp(0, 11) as u32 + 1 }),
            View::Days { .. } if until => {
                close(&app);
                let s = app.global::<SearchData>();
                let label = time::day_start_ms(value).map(time::day_short).unwrap_or_default();
                s.set_until_day(value);
                s.set_until_label(tr!("Until {}", label).into());
                s.invoke_changed();
                app.invoke_focus_search();
            }
            View::Days { .. } => {
                close(&app);
                app.invoke_focus_composer();
                app.invoke_show_day(value);
            }
        }
    });
    let weak = app.as_weak();
    d.on_close(move || {
        if let Some(app) = weak.upgrade() {
            close(&app);
        }
    });
}

/// Opens on `day`, or on the day at the top of the message list (0), else today.
fn open(app: &AppWindow, day: i32, x: f32, y: f32) {
    let key = app.get_current_key().to_string();
    if key.is_empty() {
        return;
    }
    let day = if day > 0 { day } else { top_day(app) };
    let today = today();
    let d = NaiveDate::from_num_days_from_ce_opt(if day > 0 { day } else { today }).unwrap_or_default();
    CAL.set(Some(Cal { key, until: false, view: View::Days { year: d.year(), month: d.month() }, selected: day.max(0), marks: None, first: None }));
    show(app, x, y, false);
    ask();
}

/// Opens for the search's "until" date, on the one picked before, else today.
fn open_until(app: &AppWindow, x: f32, y: f32) {
    let picked = app.global::<SearchData>().get_until_day();
    let d = NaiveDate::from_num_days_from_ce_opt(if picked > 0 { picked } else { today() }).unwrap_or_default();
    CAL.set(Some(Cal {
        key: String::new(),
        until: true,
        view: View::Days { year: d.year(), month: d.month() },
        selected: picked.max(0),
        marks: None,
        first: None,
    }));
    show(app, x, y, true);
}

fn show(app: &AppWindow, x: f32, y: f32, until: bool) {
    let data = app.global::<CalendarData>();
    data.set_anchor_x(x);
    data.set_anchor_y(y);
    data.set_until(until);
    let heads: Vec<SharedString> = time::weekday_heads().iter().map(|&h| h.into()).collect();
    data.set_heads(ModelRc::new(VecModel::from(heads)));
    data.set_open(true);
    render(app);
}

pub(crate) fn close(app: &AppWindow) {
    CAL.set(None);
    app.global::<CalendarData>().set_open(false);
}

/// The day of the row across the message list's top edge; 0 if there is none.
fn top_day(app: &AppWindow) -> i32 {
    let id = app.get_view_top_id();
    let msgs = app.get_messages();
    slint::Model::iter(&msgs).find(|m| m.id == id).map_or(0, |m| m.day)
}

fn today() -> i32 {
    time::day_num(&Local::now())
}

fn change(app: &AppWindow, f: impl FnOnce(View) -> View) {
    let changed = CAL.with_borrow_mut(|c| {
        let Some(c) = c.as_mut() else { return false };
        let view = f(c.view);
        if view == c.view {
            return false;
        }
        c.view = view;
        c.marks = None;
        true
    });
    if changed {
        render(app);
        if !CAL.with_borrow(|c| c.as_ref().is_some_and(|c| c.until)) {
            ask();
        }
    }
}

fn day(y: i32, m: u32, d: u32) -> i32 {
    NaiveDate::from_ymd_opt(y, m, d).map_or(0, |d| d.num_days_from_ce())
}

/// The local days the view's spans start at, and the end of the last.
fn bounds(view: View) -> Vec<i32> {
    match view {
        View::Days { year, month } => {
            let next = if month == 12 { day(year + 1, 1, 1) } else { day(year, month + 1, 1) };
            (day(year, month, 1)..=next).collect()
        }
        View::Months { year } => (0..13).map(|i| day(year + i / 12, (i % 12) as u32 + 1, 1)).collect(),
    }
}

fn ask() {
    if let Some(view) = CAL.with_borrow(|c| c.as_ref().map(|c| c.view)) {
        send_cmd(Command::CalendarMarks(bounds(view)));
    }
}

/// The backend's answer, if it is for the view showing.
pub(crate) fn marks_arrived(app: &AppWindow, key: String, got: Vec<i32>, marks: Vec<bool>, first: Option<i32>) {
    let fits = CAL.with_borrow_mut(|c| {
        let Some(c) = c.as_mut().filter(|c| c.key == key && bounds(c.view) == got) else { return false };
        c.marks = Some(marks);
        c.first = first;
        true
    });
    if fits {
        render(app);
    }
}

fn render(app: &AppWindow) {
    let data = app.global::<CalendarData>();
    CAL.with_borrow(|c| {
        let Some(c) = c.as_ref() else { return };
        let today = today();
        let b = bounds(c.view);
        let (start, end) = (b[0], b[b.len() - 1]);
        let mark = |i: usize| c.marks.as_ref().map(|m| m.get(i).copied().unwrap_or(false));
        let cell = |i: usize, label: String, value: i32, selected: bool, is_today: bool| {
            // a day after today can't be picked; a month view's cells are all months
            let mark = if c.until { Some(matches!(c.view, View::Months { .. }) || value <= today) } else { mark(i) };
            CalendarCell {
                label: label.into(),
                value,
                blank: false,
                enabled: mark == Some(true),
                dim: mark == Some(false),
                selected,
                today: is_today,
            }
        };
        let cells: Vec<CalendarCell> = match c.view {
            View::Days { year, month } => {
                let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_default();
                let lead = if time::week_starts_monday() {
                    first.weekday().num_days_from_monday()
                } else {
                    first.weekday().num_days_from_sunday()
                } as usize;
                let blank = || CalendarCell { blank: true, ..Default::default() };
                let mut cells: Vec<CalendarCell> = (0..lead).map(|_| blank()).collect();
                for (i, d) in (start..end).enumerate() {
                    cells.push(cell(i, (i + 1).to_string(), d, d == c.selected, d == today));
                }
                // six weeks always: the popup keeps its height from month to month
                cells.resize_with(42, blank);
                data.set_title(time::month_title(first).into());
                cells
            }
            View::Months { year } => {
                let sel = NaiveDate::from_num_days_from_ce_opt(c.selected).filter(|_| c.selected > 0);
                let now = Local::now().date_naive();
                data.set_title(year.to_string().into());
                (0..12u32)
                    .map(|m| {
                        let selected = sel.is_some_and(|s| s.year() == year && s.month0() == m);
                        let is_today = now.year() == year && now.month0() == m;
                        cell(m as usize, time::month_short(m), m as i32, selected, is_today)
                    })
                    .collect()
            }
        };
        data.set_months(matches!(c.view, View::Months { .. }));
        data.set_cells(ModelRc::new(VecModel::from(cells)));
        // nothing before the oldest message (while unknown, the way back stays open) or after today
        data.set_can_prev(c.until || c.first.is_none_or(|f| f < start));
        data.set_can_next(end <= today);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_span_their_days_and_months() {
        let feb = bounds(View::Days { year: 2028, month: 2 });
        assert_eq!(feb.len(), 30);
        assert_eq!(feb[0], day(2028, 2, 1));
        assert_eq!(*feb.last().unwrap(), day(2028, 3, 1));
        let dec = bounds(View::Days { year: 2026, month: 12 });
        assert_eq!(*dec.last().unwrap(), day(2027, 1, 1));
        let year = bounds(View::Months { year: 2026 });
        assert_eq!(year.len(), 13);
        assert_eq!(year[11], day(2026, 12, 1));
        assert_eq!(year[12], day(2027, 1, 1));
    }
}
