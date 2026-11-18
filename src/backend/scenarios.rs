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
        h.cmd(Command::Search { query: "철수".into(), contacts: false, find: Default::default(), scope: None }).await;
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

/// What the message search finds for `query`: the snippets of the found messages, once the
/// scan (which waits for a pause in typing) is done.
async fn found(h: &mut Harness, query: &str, scope: Option<u32>) -> Vec<String> {
    let scope = scope.map(|who| thread_key(&Thread::Contact(ServiceId::Aci(super::fixture::contact(who).into()))));
    h.cmd(Command::Search { query: query.into(), contacts: false, find: Default::default(), scope }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.settle().await;
    let ev = h.take_events();
    let rows = ev.iter().rev().find_map(|e| match e {
        Event::Results(r) => Some(r.clone()),
        _ => None,
    });
    rows.unwrap_or_default()
        .into_iter()
        .filter_map(|r| match r {
            ResultDto::Message { thread, .. } => Some(thread.snippet),
            _ => None,
        })
        .collect()
}

#[test]
fn messages_are_found_as_they_now_read() {
    use presage::libsignal_service::content::ContentBody;
    use presage::proto::EditMessage;
    scenario(|| async {
        let mut h = Harness::new().await;
        // stored before the index existed: found once the backfill has run
        h.op("seed who=1 name=철수 n=2").await;
        h.op("recv who=1 n=2").await;
        h.op("recv who=2 n=1").await;
        assert_eq!(found(&mut h, "새 메시지", None).await, ["새 메시지 1", "새 메시지 2", "새 메시지 1"]);
        // the last syllable may still be in composition
        assert_eq!(found(&mut h, "새 멧", None).await.len(), 3);
        assert_eq!(found(&mut h, "새 메시지", Some(2)).await, ["새 메시지 1"], "only that chat");
        assert!(found(&mut h, "메시지 2", None).await.iter().all(|s| s.starts_with('새')), "seeded rows aren't indexed yet");

        h.app.backfill_pins();
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.settle().await;
        assert_eq!(found(&mut h, "시지 2", None).await, ["새 메시지 2", "메시지 2"]);

        let thread = Thread::Contact(ServiceId::Aci(super::fixture::contact(1).into()));
        let newest: Vec<u64> = h.app.index.page(&thread, i64::MAX, 10).await.unwrap().into_iter().map(|r| r.1).collect();
        let me = h.app.me;
        // an edit by its author changes what's found
        let (edit_ts, target) = (newest[0] + 10, newest[0]);
        let mut edit = super::fixture::incoming_dm(1, DataMessage { timestamp: Some(edit_ts), ..Default::default() }, me);
        edit.body = ContentBody::EditMessage(EditMessage {
            target_sent_timestamp: Some(target),
            data_message: Some(DataMessage { body: Some("고친 글".into()), timestamp: Some(edit_ts), ..Default::default() }),
        });
        h.app.store.save_message(&thread, edit.clone()).await.unwrap();
        h.app.content(edit).await;
        assert_eq!(found(&mut h, "고친", None).await, ["고친 글"]);
        assert!(found(&mut h, "새 메시지 2", None).await.is_empty());

        // deleted for everyone: gone
        let del_ts = newest[1] + 20;
        let del = super::fixture::incoming_dm(
            1,
            DataMessage { delete: Some(Delete { target_sent_timestamp: Some(newest[1]) }), timestamp: Some(del_ts), ..Default::default() },
            me,
        );
        h.app.content(del).await;
        assert_eq!(found(&mut h, "새 메시지 1", None).await.len(), 1, "only the other chat's");

        // a disappearing message isn't indexed at all
        let ts = newest[0] + 30;
        let gone = super::fixture::incoming_dm(1, DataMessage { body: Some("사라질 비밀".into()), expire_timer: Some(3600), timestamp: Some(ts), ..Default::default() }, me);
        h.app.store.save_message(&thread, gone.clone()).await.unwrap();
        h.app.content(gone).await;
        assert!(found(&mut h, "비밀", None).await.is_empty());

        // nor is anything left of a deleted chat (the index itself: the list would hide it anyway)
        let left = || h.app.index.search_text(Some(&thread), "글", &|_| true, 10, &|| false);
        assert_eq!(left().await.unwrap().len(), 1);
        h.app.store.clear_thread(&thread).await.unwrap();
        assert!(left().await.unwrap().is_empty());
    });
}

/// The snippets of the found messages in the last results sent.
/// The snippets of the found messages in the last results sent, if any were.
fn shown_hits(h: &Harness) -> Option<Vec<String>> {
    let ev = h.take_events();
    let rows = ev.iter().rev().find_map(|e| match e {
        Event::Results(r) => Some(r.clone()),
        _ => None,
    })?;
    Some(
        rows.into_iter()
            .filter_map(|r| match r {
                ResultDto::Message { thread, .. } => Some(thread.snippet),
                _ => None,
            })
            .collect(),
    )
}

#[test]
fn open_results_change_only_in_place() {
    use presage::libsignal_service::content::ContentBody;
    use presage::proto::EditMessage;
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("recv who=1 n=2").await;
        assert_eq!(found(&mut h, "새 메시지", None).await, ["새 메시지 2", "새 메시지 1"]);

        // a new match isn't added: rows would move under the pointer
        h.op("recv who=1 n=1").await;
        assert!(shown_hits(&h).is_none_or(|s| s.len() == 2));

        let thread = Thread::Contact(ServiceId::Aci(super::fixture::contact(1).into()));
        let tss: Vec<u64> = h.app.index.page(&thread, i64::MAX, 10).await.unwrap().into_iter().map(|r| r.1).collect();
        let me = h.app.me;
        // edited away from the query: the row stays, with the new text
        let edit_ts = tss[0] + 10;
        let mut edit = super::fixture::incoming_dm(1, DataMessage { timestamp: Some(edit_ts), ..Default::default() }, me);
        edit.body = ContentBody::EditMessage(EditMessage {
            target_sent_timestamp: Some(tss[1]),
            data_message: Some(DataMessage { body: Some("다른 말".into()), timestamp: Some(edit_ts), ..Default::default() }),
        });
        h.app.store.save_message(&thread, edit.clone()).await.unwrap();
        h.app.content(edit).await;
        h.settle().await;
        assert_eq!(shown_hits(&h).unwrap(), ["다른 말", "새 메시지 1"]);

        // deleted for everyone: the row stays, shown as deleted
        let del = super::fixture::incoming_dm(
            1,
            DataMessage { delete: Some(Delete { target_sent_timestamp: Some(tss[2]) }), timestamp: Some(edit_ts + 10), ..Default::default() },
            me,
        );
        h.app.content(del).await;
        h.settle().await;
        assert_eq!(shown_hits(&h).unwrap(), ["다른 말", "Deleted message"]);
    });
}

#[test]
fn no_messages_are_found_until_older_ones_are_indexed() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("recv who=1 n=1").await;
        h.app.search.indexing = Some(5);
        let ev = {
            h.cmd(Command::Search { query: "새 메시지".into(), contacts: false, find: Default::default(), scope: None }).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            h.settle().await;
            h.take_events()
        };
        let rows = ev.iter().rev().find_map(|e| match e {
            Event::Results(r) => Some(r.clone()),
            _ => None,
        });
        let rows = rows.expect("results");
        assert!(matches!(rows.first(), Some(ResultDto::Note(_))), "the note leads");
        assert!(!rows.iter().any(|r| matches!(r, ResultDto::Message { .. })), "a part could pass for all");

        // done: the open search looks now
        h.app.texts_indexed().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.settle().await;
        assert_eq!(shown_hits(&h).unwrap(), ["새 메시지 1"]);
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
        h.op("seed who=1 n=100").await;
        h.open(1).await;
        // scrolled up when news comes in
        h.cmd(Command::AtBottom(false)).await;
        h.op("recv who=1 n=10").await;
        h.take_events();

        // not been there yet: to the first unread message
        h.cmd(Command::UnreadButton).await;
        let ev = h.take_events();
        let first_id = jump_target(&ev).expect("a jump to the first unread").to_string();
        let len = h.app.current.as_ref().unwrap().window.len();
        assert_eq!(first_id, h.row(len - 10));

        // there now, even before reading reports it: to the bottom
        h.cmd(Command::UnreadButton).await;
        let ev = h.take_events();
        assert!(jump_target(&ev).is_none());
        assert_eq!(last_list(&ev).map(|l| l.1), Some(Scroll::Bottom));
    });
}

/// The thread opens at its first unread message, so the button goes on to the bottom at once,
/// though the list hasn't reported anything read yet (still landing, or the window inactive).
#[test]
fn the_button_after_opening_at_unread_goes_to_the_bottom() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=100 unread=30").await;
        h.open(1).await;
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
        assert!(ev.iter().any(|e| matches!(e, Event::Notice(s) if s == "Pin failed: offline")));
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

/// The media page's last answer: (files, items, more, reset).
fn last_gallery(events: &[Event]) -> Option<(bool, Vec<crate::types::GalleryItemDto>, bool, bool)> {
    events.iter().rev().find_map(|e| match e {
        Event::Gallery { files, items, more, reset, .. } => Some((*files, items.clone(), *more, *reset)),
        _ => None,
    })
}

#[test]
fn the_media_page_lists_what_arrived_and_drops_what_goes() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("seed who=1 n=3").await;
        h.op("gifs who=1 n=2").await;
        h.op("big who=1").await;
        h.open(1).await;
        h.take_events();
        h.cmd(Command::GalleryOpen { files: false }).await;
        let (files, items, more, reset) = last_gallery(&h.take_events()).expect("a page");
        assert!(!files && !more && reset);
        // newest message first, its pictures in order; what `ViewImage` takes for each
        let got: Vec<(usize, usize)> = items.iter().map(|i| (i.index, i.nth)).collect();
        assert_eq!(got, [(0, 0), (0, 0), (1, 1)]);
        let (big, gifs) = (items[0].id.clone(), items[1].id.clone());
        assert_ne!(big, gifs);

        // the cached GIFs get their pictures, under the page's keys
        h.cmd(Command::GalleryShown { from: 0, to: 3 }).await;
        let mut came = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if h.take_events().iter().any(|e| matches!(e, Event::Thumb(t) if t.key == items[1].key)) {
                came = true;
                break;
            }
        }
        assert!(came, "the GIF's picture never came");
        // out of range: let go of
        h.cmd(Command::GalleryShown { from: 0, to: 1 }).await;
        assert!(!h.app.gallery_requested.borrow().contains(&items[2].key));

        // deleted for everyone: gone from the page and the index
        let key = h.app.current.as_ref().unwrap().key.clone();
        h.app.forget_media(&key, gifs.parse().unwrap()).await;
        let (_, items, _, reset) = last_gallery(&h.take_events()).expect("the page again");
        assert!(reset);
        assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), [big.as_str()]);

        h.cmd(Command::GalleryOpen { files: true }).await;
        let (files, items, more, _) = last_gallery(&h.take_events()).expect("the files");
        assert!(files && items.is_empty() && !more);
        h.cmd(Command::GalleryClose).await;
        assert!(h.app.gallery.is_none() && h.app.gallery_requested.borrow().is_empty());
    });
}

#[test]
fn the_media_page_waits_for_the_backfill_of_its_chat() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("gifs who=1 n=2").await;
        h.open(1).await;
        let key = h.app.current.as_ref().unwrap().key.clone();
        let indexing = |ev: &[Event]| {
            ev.iter().rev().find_map(|e| match e {
                Event::Gallery { indexing, .. } => Some(*indexing),
                _ => None,
            })
        };
        // the backfill hasn't reached this chat: what's indexed may be only its oldest part
        h.app.media_backfill = Some(HashSet::from(["other".to_string()]));
        h.take_events();
        h.cmd(Command::GalleryOpen { files: false }).await;
        let ev = h.take_events();
        let (_, items, more, reset) = last_gallery(&ev).expect("a page");
        assert!(indexing(&ev) == Some(true) && items.is_empty() && !more && reset);

        Box::pin(h.app.internal(Internal::MediaIndexed("another".into()))).await;
        assert!(last_gallery(&h.take_events()).is_none(), "another chat's turn changes nothing");
        Box::pin(h.app.internal(Internal::MediaIndexed(key))).await;
        let ev = h.take_events();
        let (_, items, _, reset) = last_gallery(&ev).expect("the page, now read");
        assert!(indexing(&ev) == Some(false) && !items.is_empty() && reset);

        // the walk ends: the flag goes, and a page opened later reads right away
        h.app.persist.media_indexed = false;
        h.app.backfill_pins();
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.settle().await;
        assert!(h.app.media_backfill.is_none());
        h.cmd(Command::GalleryOpen { files: false }).await;
        let ev = h.take_events();
        assert!(indexing(&ev) == Some(false) && !last_gallery(&ev).unwrap().1.is_empty());
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

/// A message `from` sent to `to` at `ts`, with `pni_verified` as given.
fn dm_between(from: ServiceId, to: ServiceId, ts: u64, body: &str, pni_verified: Option<Uuid>) -> Content {
    let at = chrono::DateTime::from_timestamp_millis(ts as i64).unwrap();
    Content::from_body(
        DataMessage { body: Some(body.into()), timestamp: Some(ts), ..Default::default() },
        presage::libsignal_service::content::Metadata {
            sender: from,
            destination: to,
            sender_device: 1u32.try_into().unwrap(),
            pni_verified: pni_verified.map(Into::into),
            client_timestamp: at,
            server_timestamp: at,
            needs_receipt: false,
            unidentified_sender: false,
            was_plaintext: false,
            server_guid: None,
        },
    )
}

#[test]
fn a_pni_thread_joins_its_person_when_they_answer() {
    scenario(|| async {
        let mut h = Harness::new().await;
        let me = ServiceId::Aci(h.app.me.into());
        let pni = Uuid::from_u128(0xA0_0001);
        let aci = super::fixture::contact(1);
        let (pk, ck) = (thread_key(&super::pni::pni_thread(pni)), thread_key(&Thread::Contact(ServiceId::Aci(aci.into()))));
        assert!(pk.starts_with("p:") && parse_thread_key(&pk) == Some(super::pni::pni_thread(pni)));

        // found by number, written to twice before any answer
        h.app.index.pni_put(pni, Some("+821012345678")).await.unwrap();
        let t0 = 1_700_000_000_000u64;
        for (i, text) in ["안녕하세요", "보셨나요"].into_iter().enumerate() {
            let c = dm_between(me, ServiceId::Pni(pni.into()), t0 + i as u64 * 1_000, text, None);
            h.app.store.save_message(&super::pni::pni_thread(pni), c).await.unwrap();
        }
        h.app.persist.mute_override.insert(pk.clone(), 1);
        h.app.threads_dirty = true;
        h.cmd(Command::OpenThread(pk.clone())).await;
        let ev = h.take_events();
        let listed = ev.iter().rev().find_map(|e| match e {
            Event::Threads(list) => Some(list.clone()),
            _ => None,
        });
        assert!(listed.expect("a thread list").iter().any(|t| t.key == pk), "listed under its p: key");
        let header = ev.iter().rev().find_map(|e| match e {
            Event::Header { name, .. } => Some(name.clone()),
            _ => None,
        });
        assert!(header.expect("a header").contains("10-1234-5678"), "named by its number");

        // their answer: stored by presage under their ACI, signed with the PNI we wrote to
        let reply = dm_between(ServiceId::Aci(aci.into()), me, t0 + 5_000, "네 반가워요", Some(pni));
        let to = Thread::Contact(ServiceId::Aci(aci.into()));
        h.app.store.save_message(&to, reply.clone()).await.unwrap();
        h.app.content(reply).await;
        h.settle().await;
        let ev = h.take_events();
        assert!(ev.iter().any(|e| matches!(e, Event::ThreadMoved { from, to } if *from == pk && *to == ck)));
        assert_eq!(h.app.current.as_ref().map(|o| o.key.clone()), Some(ck.clone()), "the open thread followed");
        assert_eq!(h.app.index.pni_number(pni).await.unwrap(), None, "forgotten once moved");
        assert!(h.app.index.page(&super::pni::pni_thread(pni), i64::MAX, 10).await.unwrap().is_empty());
        assert_eq!(h.app.persist.mute_override.get(&ck), Some(&1));
        // what we wrote stays before their answer
        let mut rows = h.app.index.page(&to, i64::MAX, 10).await.unwrap();
        rows.reverse();
        assert_eq!(rows.iter().map(|r| r.1).collect::<Vec<_>>(), vec![t0, t0 + 1_000, t0 + 5_000]);
        let (items, _) = last_list(&ev).expect("the thread reopened");
        assert_eq!(items.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["안녕하세요", "보셨나요", "네 반가워요"]);
    });
}

/// The thread list's row of fixture contact `who`, as last sent.
fn listed(events: &[Event], who: u32) -> Option<ThreadDto> {
    let key = thread_key(&Thread::Contact(ServiceId::Aci(super::fixture::contact(who).into())));
    events.iter().rev().find_map(|e| match e {
        Event::Threads(list) => Some(list.iter().find(|t| t.key == key).cloned()),
        _ => None,
    })?
}

/// Whether the last header said the open thread is a message request.
fn header_request(events: &[Event]) -> Option<bool> {
    events.iter().rev().find_map(|e| match e {
        Event::Header { request, .. } => Some(*request),
        _ => None,
    })
}

fn key_of(who: u32) -> String {
    thread_key(&Thread::Contact(ServiceId::Aci(super::fixture::contact(who).into())))
}

#[test]
fn a_message_request_tells_its_sender_nothing_until_accepted() {
    scenario(|| async {
        let mut h = Harness::new().await;
        // the phone's storage has been read: 2 is a contact, 1 a stranger
        h.op("phone-storage who=2 accepted=1").await;
        h.op("recv who=1 n=2").await;
        h.op("recv who=2 n=1").await;
        assert_eq!(h.query("request", 1), "true");
        assert_eq!(h.query("request", 2), "false");
        assert_eq!(h.query("sent", 1), "delivery=1 read=0 sync=0", "only the contact hears it arrived");
        let ev = h.take_events();
        assert_eq!(listed(&ev, 1).expect("listed").snippet, "Message request", "the text doesn't show");
        assert_ne!(listed(&ev, 2).expect("listed").snippet, "Message request");

        // read on screen: our other devices hear it, the sender doesn't
        h.open(1).await;
        assert_eq!(header_request(&h.take_events()), Some(true), "the request bar instead of the composer");
        h.cmd(Command::ReadUpTo(h.row(1))).await;
        assert_eq!(h.query("sent", 1), "delivery=1 read=0 sync=2");
        // nothing goes to them before it's accepted: a reaction would say it was read
        h.cmd(Command::React { id: h.row(1), emoji: "👍".into(), remove: false }).await;
        h.cmd(Command::Composing(true)).await;
        assert!(h.app.ops.is_empty() && h.app.composing.is_none(), "refused");
        assert_eq!(h.query("request", 1), "true");

        h.cmd(Command::AnswerRequest { key: key_of(1), answer: crate::types::RequestAnswer::Accept }).await;
        assert_eq!(h.query("answers", 1), "Accept,ProfileKey");
        assert_eq!(h.query("request", 1), "false");
        assert_eq!(h.query("sent", 1), "delivery=1 read=2 sync=2", "what was on screen counts as read");
        assert_eq!(header_request(&h.take_events()), Some(false), "the composer comes back");
        h.op("recv who=1 n=1").await;
        assert_eq!(h.query("sent", 1), "delivery=2 read=2 sync=2", "an accepted chat answers as any other");
    });
}

#[test]
fn nothing_is_a_request_before_the_phone_storage_is_read() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("recv who=1 n=1").await;
        assert_eq!(h.query("request", 1), "false");
        assert_eq!(h.query("sent", 1), "delivery=1 read=0 sync=0");
    });
}

#[test]
fn a_blocked_persons_messages_are_dropped() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("phone-storage who=1 blocked=1").await;
        h.op("recv who=1 n=2").await;
        assert_eq!(h.op("query rows who=1").await, "0", "nothing kept");
        assert_eq!(h.query("sent", 1), "delivery=0 read=0 sync=0");
        assert!(listed(&h.take_events(), 1).is_none());
    });
}

#[test]
fn blocking_or_deleting_a_request_closes_it() {
    scenario(|| async {
        let mut h = Harness::new().await;
        h.op("phone-storage who=9 accepted=1").await;
        h.op("recv who=1 n=3").await;
        h.op("recv who=2 n=1").await;
        h.open(1).await;
        h.take_events();
        h.cmd(Command::AnswerRequest { key: key_of(1), answer: crate::types::RequestAnswer::Delete }).await;
        assert_eq!(h.op("query rows who=1").await, "0");
        assert_eq!(h.query("unread", 1), "0");
        assert_eq!(h.query("window", 1), "closed");
        let ev = h.take_events();
        assert!(ev.iter().any(|e| matches!(e, Event::Messages { key, .. } if key.is_empty())), "the UI shows nothing open");
        assert!(listed(&ev, 1).is_none(), "gone from the list");

        // blocked: the chat leaves the list but its messages stay, and new ones are dropped
        h.open(2).await;
        h.cmd(Command::AnswerRequest { key: key_of(2), answer: crate::types::RequestAnswer::Block }).await;
        assert_eq!(h.query("answers", 1), "Delete,Block");
        assert_eq!(h.query("window", 1), "closed");
        assert!(listed(&h.take_events(), 2).is_none());
        h.op("recv who=2 n=1").await;
        assert_eq!(h.op("query rows who=2").await, "1");
        // the phone's storage shows the block now; unblocking there brings the chat back
        h.op("phone-storage who=2 blocked=1").await;
        h.op("phone-storage who=2").await;
        assert_eq!(h.query("request", 2), "true");
        assert!(listed(&h.take_events(), 2).is_some());
    });
}

/// Everything we send gets its own timestamp, also with nothing pending: presage stores rows by
/// (thread, timestamp), so a receipt in the same ms as a message would replace it.
#[test]
fn send_timestamps_never_repeat() {
    scenario(|| async {
        let h = Harness::new().await;
        let tss: Vec<u64> = (0..100).map(|_| h.app.next_send_ts()).collect();
        assert!(tss.windows(2).all(|w| w[0] < w[1]));
    });
}
