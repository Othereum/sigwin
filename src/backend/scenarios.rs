//! Scenarios on the real backend without an account (`fixture.rs`): messages are seeded or
//! received through the real receive path, commands are what the UI sends, and the checks look
//! at the events that come out, the counts and the receipts that would have gone out.

use super::fixture::harness::Harness;
use super::*;

/// Runs a scenario the way the backend runs: a thread with its big stack (the presage futures
/// are deep), a current-thread runtime and a `LocalSet` (the backend spawns local tasks).
fn scenario<Fut: std::future::Future<Output = ()>>(f: impl FnOnce() -> Fut + Send + 'static) {
    std::thread::Builder::new()
        .name("scenario".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            tokio::task::LocalSet::new().block_on(&rt, f());
        })
        .unwrap()
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e));
}

/// The last message list the backend sent.
fn last_list(events: &[Event]) -> Option<(&[MsgDto], Scroll)> {
    events.iter().rev().find_map(|e| match e {
        Event::Messages { items, scroll, .. } => Some((items.as_slice(), *scroll)),
        _ => None,
    })
}

/// Position of the "unread" line in a list, with how many rows are below it (it included).
fn divider(items: &[MsgDto]) -> Option<(usize, usize)> {
    let dividers: Vec<usize> = items.iter().enumerate().filter(|(_, m)| m.unread_divider).map(|(i, _)| i).collect();
    assert!(dividers.len() <= 1, "more than one divider: {dividers:?}");
    dividers.first().map(|&i| (i, items.len() - i))
}

/// The unread count the thread list last showed for fixture contact `who`.
fn listed_unread(events: &[Event], who: u32) -> Option<u32> {
    let key = thread_key(&Thread::Contact(ServiceId::Aci(super::fixture::contact(who).into())));
    events.iter().rev().find_map(|e| match e {
        Event::Threads(list) => list.iter().find(|t| t.key == key).map(|t| t.unread),
        _ => None,
    })
}

fn jump_target(events: &[Event]) -> Option<&str> {
    events.iter().rev().find_map(|e| match e {
        Event::JumpToUnread(id) => Some(id.as_str()),
        _ => None,
    })
}

#[test]
fn opening_with_unread_lands_on_the_first_one() {
    scenario(|| async {
        let mut h = Harness::new().await;
        // the thread list (with its count) went out while seeding, before the thread is opened
        h.op("seed who=1 name=철수 n=100 unread=30").await;
        h.open(1).await;
        let ev = h.take_events();
        let (items, scroll) = last_list(&ev).expect("a list");
        assert_eq!(scroll, Scroll::Unread);
        assert_eq!(divider(items).map(|d| d.1), Some(30), "the line sits above the 30 unread messages");
        assert_eq!(listed_unread(&ev, 1), Some(30));
        // nothing is read just by opening
        assert_eq!(h.query("unread", 1), "30");
        assert_eq!(h.query("sent", 1), "delivery=0 read=0 sync=0");
    });
}

#[test]
fn opening_without_unread_goes_to_the_bottom() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=20").await;
        h.take_events();
        h.open(1).await;
        let ev = h.take_events();
        let (items, scroll) = last_list(&ev).expect("a list");
        assert_eq!(scroll, Scroll::Bottom);
        assert!(divider(items).is_none());
        assert_eq!(h.query("sent", 1), "delivery=0 read=0 sync=0");
    });
}

#[test]
fn reading_goes_row_by_row() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=100 unread=30").await;
        h.open(1).await;
        let at = h.query("window", 1);
        let first: usize = at.split("divider=").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        h.take_events();

        // the first five unread rows came on screen
        h.cmd(Command::ReadUpTo(h.row(first + 4))).await;
        let ev = h.take_events();
        assert_eq!(h.query("unread", 1), "25");
        assert_eq!(listed_unread(&ev, 1), Some(25));
        assert_eq!(h.query("sent", 1), "delivery=0 read=5 sync=5");

        // the same rows again, or fewer: nothing new
        h.cmd(Command::ReadUpTo(h.row(first + 2))).await;
        h.cmd(Command::ReadUpTo(h.row(first + 4))).await;
        assert_eq!(h.query("unread", 1), "25");
        assert_eq!(h.query("sent", 1), "delivery=0 read=5 sync=5");

        // the last row: all of it
        h.cmd(Command::ReadUpTo(h.row(59))).await;
        assert_eq!(h.query("unread", 1), "0");
        assert_eq!(h.query("sent", 1), "delivery=0 read=30 sync=30");
    });
}

#[test]
fn nothing_is_read_while_not_in_front_of_the_window() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=100 unread=30").await;
        h.open(1).await;
        h.focus(false).await;
        h.cmd(Command::ReadUpTo(h.row(59))).await;
        h.cmd(Command::AtBottom(true)).await;
        assert_eq!(h.query("unread", 1), "30");
        assert_eq!(h.query("sent", 1), "delivery=0 read=0 sync=0");
        // back in front of it: the visible rows count again
        h.focus(true).await;
        assert_eq!(h.query("unread", 1), "0", "the list sits at its bottom, so coming back reads it");
        assert_eq!(h.query("sent", 1), "delivery=0 read=30 sync=30");
    });
}

#[test]
fn news_while_reading_at_the_bottom_stays_read() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=20").await;
        h.open(1).await;
        h.take_events();
        h.op("recv who=1 n=2").await;
        assert_eq!(h.query("unread", 1), "0");
        let ev = h.take_events();
        let (items, scroll) = last_list(&ev).expect("a list");
        assert_ne!(scroll, Scroll::News);
        assert!(divider(items).is_none());
    });
}

#[test]
fn news_in_the_background_is_shown_from_the_line() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=20").await;
        h.open(1).await;
        h.focus(false).await;
        h.take_events();
        h.op("recv who=1 n=2").await;
        let ev = h.take_events();
        let (items, scroll) = last_list(&ev).expect("a list");
        assert_eq!(scroll, Scroll::News);
        assert_eq!(divider(items).map(|d| d.1), Some(2));
        assert_eq!(h.query("unread", 1), "2");
        // coming back reads them, and the line goes
        h.focus(true).await;
        assert_eq!(h.query("unread", 1), "0");
    });
}

#[test]
fn news_in_another_thread_is_counted() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=20").await;
        h.op("seed who=2 n=5").await;
        h.open(1).await;
        h.op("recv who=2 n=3").await;
        assert_eq!(h.query("unread", 2), "3");
        assert_eq!(h.query("unread", 1), "0");
    });
}

#[test]
fn the_button_goes_to_the_first_unread_then_to_the_bottom() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=100 unread=30").await;
        h.open(1).await;
        let at = h.query("window", 1);
        let first: usize = at.split("divider=").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        let first_id = h.row(first);
        h.take_events();

        // not been there yet: to the first unread message
        h.cmd(Command::UnreadButton).await;
        let ev = h.take_events();
        assert_eq!(jump_target(&ev), Some(first_id.as_str()));

        // reading on from it
        h.cmd(Command::ReadUpTo(first_id.clone())).await;
        h.take_events();
        h.cmd(Command::UnreadButton).await;
        let ev = h.take_events();
        assert!(jump_target(&ev).is_none());
        assert_eq!(last_list(&ev).map(|l| l.1), Some(Scroll::Bottom));
    });
}

#[test]
fn the_phone_reading_clears_the_count() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=20 unread=10").await;
        assert_eq!(h.query("unread", 1), "10");
        h.op("phone-read who=1").await;
        assert_eq!(h.query("unread", 1), "0");
        assert_ne!(h.query("read_upto", 1), "none");
    });
}

#[test]
fn more_unread_than_the_window_holds_starts_at_its_oldest_message() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=700 unread=650").await;
        h.take_events();
        h.open(1).await;
        let ev = h.take_events();
        let (items, scroll) = last_list(&ev).expect("a list");
        assert_eq!(scroll, Scroll::Unread);
        assert_eq!(divider(items).map(|d| d.0), Some(0));
        assert!(items.len() <= MAX_WINDOW);
    });
}
