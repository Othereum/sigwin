//! Scenarios on the real backend without an account (`fixture.rs`): messages are seeded or
//! received through the real receive path, commands are what the UI sends, and the checks look
//! at the events that come out, the counts and the receipts that would have gone out.

use super::fixture::harness::Harness;
use crate::types::ResultDto;
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
fn news_while_watching_stays_at_the_bottom_and_is_read_by_the_ui() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=20").await;
        h.open(1).await;
        h.focus(false).await;
        h.watch(true).await;
        h.take_events();
        h.op("recv who=1 n=2").await;
        let ev = h.take_events();
        let (items, scroll) = last_list(&ev).expect("a list");
        assert_ne!(scroll, Scroll::News, "the list stays at the bottom");
        assert!(divider(items).is_none(), "no line: the user is looking");
        assert_eq!(h.query("unread", 1), "2", "read once the UI says the rows stayed on screen");
        assert_eq!(h.query("toasts", 1), "", "the open thread doesn't toast");
        // the last row stayed on screen: read, with receipts
        h.cmd(Command::ReadUpTo(h.row(21))).await;
        assert_eq!(h.query("unread", 1), "0");
        assert_eq!(h.query("sent", 1), "delivery=2 read=2 sync=2");
    });
}

#[test]
fn watching_stops_when_the_window_is_covered() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=20").await;
        h.open(1).await;
        h.focus(false).await;
        h.watch(true).await;
        h.watch(false).await;
        h.take_events();
        h.op("recv who=1 n=2").await;
        let ev = h.take_events();
        let (_, scroll) = last_list(&ev).expect("a list");
        assert_eq!(scroll, Scroll::News);
        h.cmd(Command::ReadUpTo(h.row(21))).await;
        assert_eq!(h.query("unread", 1), "2", "nothing is read from a covered window");
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
fn searching_leaves_the_thread_list_whole() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 name=철수 n=2").await;
        h.op("seed who=2 name=영희 n=2").await;
        h.settle().await;
        h.take_events();
        h.cmd(Command::Search { query: "철수".into(), contacts: false, find: Default::default() }).await;
        let ev = h.take_events();
        let results = ev.iter().rev().find_map(|e| match e {
            Event::Results(r) => Some(r),
            _ => None,
        });
        let names: Vec<&str> = results
            .expect("results")
            .iter()
            .filter_map(|r| match r {
                ResultDto::Thread(t) => Some(t.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["철수"]);
        // the unread badge and the rest count from the whole list
        assert!(!ev.iter().any(|e| matches!(e, Event::Threads(l) if l.len() < 2)), "the thread list was cut to the search");
    });
}

#[test]
fn receipts_leave_the_message_store() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=2").await;
        h.op("receipts who=2 n=3").await;
        assert_eq!(h.op("query rows who=2").await, "0", "receipts arriving now are dropped");
        // stored before that: moved out once, nothing of them lost
        h.op("receipts who=3 n=4 old=1").await;
        let thread = Thread::Contact(ServiceId::Aci(super::fixture::contact(3).into()));
        let targets: Vec<u64> = h.app.index.page(&thread, i64::MAX, 100).await.unwrap().iter().map(|r| r.1).collect();
        // and the ones we sent, which presage stores too
        h.op("receipts who=3 n=2 mine=1").await;
        assert_eq!(h.op("query rows who=3").await, "6");
        let (store, me) = (h.app.store.clone(), h.app.me);
        let moved = super::receive::move_receipts_out(store, &h.app.index, me).await.unwrap();
        assert_eq!(moved, 6);
        assert_eq!(h.op("query rows who=3").await, "0");
        assert_eq!(h.op("query rows who=1").await, "2", "messages stay");
        assert_eq!(h.app.index.receipts_for(&targets).await.unwrap().len(), 4);
    });
}

#[test]
fn someone_who_only_sent_receipts_is_not_listed() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=3").await;
        h.op("receipts who=2 n=3").await;
        h.settle().await;
        let ev = h.take_events();
        let keys = |who: u32| thread_key(&Thread::Contact(ServiceId::Aci(super::fixture::contact(who).into())));
        let list = ev.iter().rev().find_map(|e| match e {
            Event::Threads(l) => Some(l),
            _ => None,
        });
        let list = list.expect("a thread list");
        assert!(list.iter().any(|t| t.key == keys(1)));
        assert!(!list.iter().any(|t| t.key == keys(2)), "a thread of receipts only is listed");
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

/// What one of our ops changed is indexed once presage stores it, and taken back when it fails.
#[test]
fn ops_are_indexed_when_stored_and_undone_when_they_fail() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=5").await;
        h.open(1).await;
        let key = h.app.current.as_ref().unwrap().key.clone();
        let (row, target, ..) = h.app.target(&h.row(4)).unwrap();
        let me = h.app.me;

        // a reaction we sent was stored: it's in the index and no longer pending
        let op = Op::React { emoji: Some("👍".into()), target };
        h.app.ops.push(PendingOp { ts: 9_001, thread_key: key.clone(), row, op });
        Box::pin(h.app.internal(Internal::Sent(9_001, Ok(())))).await;
        h.settle().await;
        assert!(h.app.ops.is_empty());
        let reactions = h.app.index.reactions_for(&key, &[target]).await.unwrap();
        assert_eq!(reactions.get(&target), Some(&vec![(me, "👍".to_string())]));

        // a pin is written before sending: a failed send takes it out again and says so
        h.app.index.pin(&key, target, me, 9_002, 0).await.unwrap();
        h.app.ops.push(PendingOp { ts: 9_002, thread_key: key.clone(), row, op: Op::Pin { target } });
        h.take_events();
        Box::pin(h.app.internal(Internal::Sent(9_002, Err("offline".into())))).await;
        h.settle().await;
        assert!(h.app.ops.is_empty());
        assert!(h.app.index.pins(&key).await.unwrap().is_empty());
        let ev = h.take_events();
        assert!(ev.iter().any(|e| matches!(e, Event::Notice(s) if s == "고정 실패: offline")));
        assert!(last_list(&ev).is_some(), "the row is shown again without the change");
    });
}

/// A message shows how many replies it has, the list names them, and a reply that is gone is
/// dropped from both.
#[test]
fn replies_are_counted_and_listed() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=5").await;
        h.open(1).await;
        let key = h.app.current.as_ref().unwrap().key.clone();
        let (target_id, reply_id) = (h.row(0), h.row(3));
        let target = h.app.target(&target_id).unwrap().1;
        let (reply, by) = {
            let t = h.app.target(&reply_id).unwrap();
            (t.1, t.2)
        };
        h.app.index.record_reply(&key, target, reply, by).await.unwrap();
        // a reply whose message is not stored (deleted, expired)
        h.app.index.record_reply(&key, target, 1, by).await.unwrap();
        h.app.emit_messages(Scroll::Keep).await;
        let ev = h.take_events();
        let (items, _) = last_list(&ev).expect("a list");
        assert_eq!(items.iter().find(|m| m.id == target_id).map(|m| m.replies), Some(2));

        h.cmd(Command::Replies(target_id.clone())).await;
        let ev = h.take_events();
        let list = ev.iter().find_map(|e| if let Event::Replies(l) = e { Some(l) } else { None }).expect("the list");
        assert_eq!(list.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), [reply.to_string()]);
        // the missing one was forgotten
        assert_eq!(h.app.index.reply_counts(&key, &[target]).await.unwrap()[&target], 1);
    });
}

#[test]
fn jumping_to_a_day_lands_on_its_first_message() {
    scenario(|| async {
        let mut h = Harness::new().await;
        // the fixture's messages start two days ago, a second apart: all on one day, far more
        // than the window the thread opens with
        h.op("seed who=1 n=300").await;
        h.open(1).await;
        let day = view::time::day_num(&(chrono::Local::now() - chrono::Duration::days(2)));
        h.take_events();
        h.cmd(Command::JumpToDay(day)).await;
        let ev = h.take_events();
        let id = ev
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::JumpToDay(id) => Some(id.clone()),
                _ => None,
            })
            .expect("a jump");
        let (items, _) = last_list(&ev).expect("the window moved to it");
        let first = items.iter().find(|m| m.id == id).expect("the target is in the window");
        assert_eq!(first.text, "메시지 1");
        assert!(first.day_sep && first.day == day);
    });
}
