//! `SIGWIN_DEMO_REAL=1`: a believable chat list and conversations, for the README's pictures
//! (`scripts/readme-media.ps1`). The people and the chats follow the UI's language. A 1:1 chat
//! shows a photo, a reply and a link preview; a group shows senders, a photo grid and who read
//! our message, and another chat has view-once photos both ways. Sending works, with a link getting its preview after the message is on screen,
//! and photos open in the viewer.

use super::*;
use crate::types::ViewOnce;
use std::collections::HashMap;

/// The chat open at start (the README's main picture) and the group.
const CHAT: &str = "c:demo-1";
const GROUP: &str = "c:demo-2";
/// A team whose members have Korean names in either language, for @mentions typed with an IME.
const TEAM: &str = "c:demo-4";
/// View-once photos both ways: one still to open, one viewed, ours, a video left to the phone.
const ONCE: &str = "c:demo-6";
/// The view-once photo there that can still be opened, and its scene variant.
const ONCE_PHOTO: (&str, usize) = ("3", 1);

/// A photo of the gallery in the 1:1 chat, and a trip's photos in the group (scene variants).
const CHAT_PHOTO: (&str, usize) = ("real-photo", 0);
const GROUP_PHOTOS: [usize; 3] = [1, 3, 2];
const LINK_PHOTO: usize = 2;

/// A local time `days_ago` days back, as the backend's timestamps are.
fn at(days_ago: u64, h: u32, m: u32) -> u64 {
    let d = chrono::Local::now().date_naive() - chrono::Days::new(days_ago);
    let t = chrono::TimeZone::from_local_datetime(&chrono::Local, &d.and_hms_opt(h, m, 0).unwrap()).earliest().unwrap();
    t.timestamp_millis() as u64
}

fn list_time(days_ago: u64, h: u32, m: u32) -> String {
    let ts = at(days_ago, h, m);
    // not `time_short` for today: it says "5m" when the shot is taken right after
    if days_ago == 0 { view::time::ampm(&view::time::local(ts)) } else { view::time::time_short(ts) }
}

/// A message sent today at `h:m`; `from` is (name, key), `None` for ours.
fn msg(id: usize, from: Option<(&str, &str)>, text: &str, h: u32, m: u32) -> MsgDto {
    let ts = at(0, h, m);
    let outgoing = from.is_none();
    let (sender, sender_key) = from.unwrap_or_default();
    MsgDto {
        id: id.to_string(),
        ts,
        sender: sender.into(),
        sender_key: sender_key.into(),
        text: text.into(),
        time: view::time::ampm(&view::time::local(ts)),
        outgoing,
        status: if outgoing { "✓✓".into() } else { String::new() },
        can_edit: outgoing,
        can_delete: outgoing,
        day_sep: id == 0,
        day: view::time::day_num(&view::time::local(ts)),
        big_emoji: emoji::only_emoji(text, 5).is_some(),
        ..Default::default()
    }
}

fn card(url: &str, title: &str, description: &str, image_key: &str) -> types::LinkCard {
    types::LinkCard {
        url: url.into(),
        title: title.into(),
        description: description.into(),
        domain: crate::link::domain(url),
        image_key: image_key.into(),
        img_w: 300,
        img_h: 158,
    }
}

struct Data {
    threads: Vec<ThreadDto>,
    chats: HashMap<String, Vec<MsgDto>>,
    /// what a link sent from the composer previews as
    sent_link: (String, String),
}

fn data(ko: bool) -> Data {
    let th = |i: usize, name: &str, snippet: &str, time: String, unread: u32, is_group: bool| ThreadDto {
        key: format!("c:demo-{i}"),
        name: name.into(),
        snippet: snippet.into(),
        time,
        ts: 0,
        unread,
        loud: false,
        is_group,
        mute: Mute::default(),
        prefs: ThreadPrefs::default(),
        phone_mentions: None,
        readers: Vec::new(),
        readers_more: 0,
    };
    type Row = (&'static str, &'static str, u64, u32, u32, u32, bool);
    let rows: [Row; 9] = if ko {
        [
            ("이서연", "좋아요. 그럼 그때 봬요 😊", 0, 18, 46, 0, false),
            ("가족 단톡방", "엄마: 다들 표정 좋다 🥹", 0, 17, 58, 2, true),
            ("김민준", "📷 IMG_2041.jpg", 0, 14, 10, 0, false),
            ("프로젝트 팀", "박지훈: 리뷰 올렸습니다", 0, 11, 30, 0, true),
            ("박지훈", "📄 회의록_11월.pdf", 1, 20, 5, 0, false),
            ("최유나", "고마워요 :)", 1, 9, 12, 0, false),
            ("독서 모임", "다음 책은 뭐로 할까요?", 14, 21, 0, 0, true),
            ("Alice Kim", "See you next week!", 15, 13, 40, 0, false),
            ("권준영", "ㅋㅋㅋㅋ 인정", 17, 22, 15, 0, false),
        ]
    } else {
        [
            ("Emma Lee", "Perfect. See you then 😊", 0, 18, 46, 0, false),
            ("Family", "Mom: Look how happy everyone was 🥹", 0, 17, 58, 2, true),
            ("Noah Kim", "📷 IMG_2041.jpg", 0, 14, 10, 0, false),
            ("Project team", "Liam: Pushed the review", 0, 11, 30, 0, true),
            ("Liam Park", "📄 meeting-notes.pdf", 1, 20, 5, 0, false),
            ("Olivia Choi", "Thanks :)", 1, 9, 12, 0, false),
            ("Book club", "What should we read next?", 14, 21, 0, 0, true),
            ("Jonas Weber", "Bis nächste Woche!", 15, 13, 40, 0, false),
            ("Ethan Kwon", "haha true", 17, 22, 15, 0, false),
        ]
    };
    let mut threads: Vec<ThreadDto> = rows
        .iter()
        .enumerate()
        .map(|(i, &(name, snippet, days, h, m, unread, group))| th(i + 1, name, snippet, list_time(days, h, m), unread, group))
        .collect();
    let her = (rows[0].0, "demo-a");
    // our last message there was read: her initial shows on the list too
    threads[0].readers = vec![(her.1.into(), her.0.into())];
    // the family chat, where the notification picture is taken, has settings of its own
    threads[1].prefs = ThreadPrefs { preview: Some(1), sound: Some(false), mentions: Some(true), ..Default::default() };

    // the 1:1 chat: a photo, a reply to it, a read receipt (a link preview is the animation's)
    let t = |en: &'static str, k: &'static str| if ko { k } else { en };
    let mut chat = vec![
        msg(0, Some(her), t("Are you free this weekend?", "주말에 시간 돼요?"), 18, 20),
        msg(1, None, t("Saturday works! What do you have in mind?", "토요일 괜찮아요! 뭐 할까요?"), 18, 21),
        msg(2, Some(her), t("Saw this at a gallery downtown. The rest of the show is just as good 🌅", "시내 갤러리에서 본 건데, 다른 작품들도 다 이렇게 좋아요 🌅"), 18, 23),
        msg(3, None, t("Wow, we have to go see it", "와 이건 꼭 보러 가야겠네요"), 18, 25),
        msg(4, Some(her), t("Saturday at 11, then? There's a nice pasta place nearby 🍝", "그럼 토요일 11시 어때요? 근처에 괜찮은 파스타집도 있어요 🍝"), 18, 31),
        msg(5, None, t("Perfect. See you then 😊", "좋아요. 그럼 그때 봬요 😊"), 18, 46),
    ];
    view::media::set_single_photo(&mut chat[2], CHAT_PHOTO.0.into(), (400, 300));
    chat[3].quote = chat[2].text.clone();
    chat[3].quote_author = her.0.into();
    chat[3].quote_id = "2".into();
    chat[3].quote_image_key = format!("{}-q", CHAT_PHOTO.0);
    chat[3].reactions = vec![("❤️".into(), 1, her.0.into())];
    chat[5].readers = vec![(her.1.into(), her.0.into())];
    view::mark_runs(&mut chat, false);

    // the group: senders, a photo grid, everyone's read our message
    let (mom, dad, sis) = if ko { (("엄마", "demo-m"), ("아빠", "demo-d"), ("지은", "demo-s")) } else { (("Mom", "demo-m"), ("Dad", "demo-d"), ("Sophie", "demo-s")) };
    let mut group = vec![
        msg(0, Some(mom), t("Grandma turns 80 on Sunday 🎂 Dinner at our place?", "일요일에 할머니 팔순이야 🎂 우리 집에서 저녁 먹을까?"), 17, 30),
        msg(1, Some(sis), t("Yes!! I'm in", "좋아!! 나 갈게"), 17, 32),
        msg(2, Some(dad), t("I'll get the table ready", "상은 내가 차릴게"), 17, 33),
        msg(3, None, t("I'll bring the cake 🍰", "케이크는 제가 사 갈게요 🍰"), 17, 40),
        msg(4, Some(dad), t("Found the photos from last summer's trip", "작년 여름 여행 사진 찾았다"), 17, 55),
        msg(5, Some(mom), t("Look how happy everyone was 🥹", "다들 표정 좋다 🥹"), 17, 58),
    ];
    group[3].reactions = vec![("❤️".into(), 2, format!("{}, {}", mom.0, sis.0))];
    group[3].readers = [dad, mom, sis].iter().map(|(n, k)| (k.to_string(), n.to_string())).collect();
    view::media::set_photo_grid(&mut group[4], GROUP_PHOTOS.len(), |n, _, _| format!("real-grid-{n}"));
    view::mark_runs(&mut group, true);

    // the other threads open on their last message
    let mut chats: HashMap<String, Vec<MsgDto>> = threads
        .iter()
        .map(|t| {
            let (who, text) = match t.snippet.split_once(": ") {
                Some((who, text)) if t.is_group => (who, text),
                _ => (t.name.as_str(), t.snippet.as_str()),
            };
            let mut m = msg(0, Some((who, "demo-x")), text, 9, 0);
            m.time = t.time.clone();
            (t.key.clone(), vec![m])
        })
        .collect();
    chats.insert(CHAT.into(), chat);
    chats.insert(GROUP.into(), group);

    // view-once photos, sent and received (not in the README's pictures)
    let yuna = (rows[5].0, "demo-y");
    let mut once = vec![
        msg(0, Some(yuna), t("Want to see what I made?", "제가 만든 거 볼래요?"), 8, 50),
        msg(1, Some(yuna), "", 8, 51),
        msg(2, None, t("Whoa, that's amazing. Here's mine", "와 대박이다. 제 것도 보여 줄게요"), 8, 55),
        msg(3, None, "", 8, 56),
        msg(4, Some(yuna), "", 9, 5),
        msg(5, Some(yuna), "", 9, 8),
        msg(6, Some(yuna), t("Thanks :)", "고마워요 :)"), 9, 12),
    ];
    once[1].view_once = ViewOnce::Viewed;
    once[3].view_once = ViewOnce::Sent;
    once[4].view_once = ViewOnce::Photo;
    once[5].view_once = ViewOnce::Phone;
    // the one still to open is the one the viewer shows
    once[4].id = ONCE_PHOTO.0.into();
    once[3].id = "4".into();
    // yesterday, as the chat list says
    for m in &mut once {
        m.ts -= 24 * 3600 * 1000;
        m.day = view::time::day_num(&view::time::local(m.ts));
    }
    view::mark_runs(&mut once, false);
    chats.insert(ONCE.into(), once);
    let sent_link = (
        t("Tickets for Saturday", "토요일 관람권").to_string(),
        t("Light & Land: Landscapes at Dusk. Saturday, 11:00. 2 tickets.", "빛과 땅: 해 질 녘의 풍경. 토요일 11:00, 2매.").to_string(),
    );
    Data { threads, chats, sent_link }
}

pub(super) fn demo_real(emit: backend::Emit, thumbs: Thumbs, mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>) {
    let ko = crate::i18n::lang() == "ko";
    let Data { threads, mut chats, sent_link } = data(ko);
    emit(Event::Ready);
    emit(Event::Threads(threads.clone()));
    header(&emit, &threads, CHAT);
    emit(Event::Messages { key: CHAT.into(), items: chats[CHAT].clone(), paging: Paging::default(), scroll: Scroll::Bottom });
    emit(Event::Pins(Vec::new()));
    let (me, number) = if ko { ("홍길동", "+821012345678") } else { ("Alex Morgan", "+14155550123") };
    emit(Event::Account { name: me.into(), number: number.into(), device: "Sigwin".into(), receipts: Some(true), typing: None });
    std::thread::spawn(move || {
        // after the rows and the window's scale, as downloads do
        std::thread::sleep(std::time::Duration::from_millis(300));
        send_thumbs(&emit);
        let mut current = CHAT.to_string();
        while let Some(c) = cmd_rx.blocking_recv() {
            match c {
                Command::OpenThread(key) => {
                    header(&emit, &threads, &key);
                    let items = chats.get(&key).cloned().unwrap_or_default();
                    emit(Event::Messages { key: key.clone(), items: items.clone(), paging: Paging::default(), scroll: Scroll::Bottom });
                    emit(Event::Pins(Vec::new()));
                    resend_thumbs(&emit, &thumbs, &items);
                    // who @mentions can name in the team (the IME picture composes "잇", then
                    // types the initials "ㅇㅁㅈ")
                    if key == TEAM {
                        let names = ["이서연", "이민준", "박지훈", "최유나"];
                        emit(Event::Members(names.iter().enumerate().map(|(i, n)| (format!("00000000-0000-0000-0000-00000000010{i}"), n.to_string())).collect()));
                    }
                    current = key;
                }
                Command::LinkPreview(Some(url)) => {
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    emit(Event::Thumb(Arc::new(scene_thumb(&format!("real-sent{}", crate::backend::linkpreview::COMPOSER_KEY), (44, 44), LINK_PHOTO))));
                    let c = card(&url, &sent_link.0, &sent_link.1, "real-sent");
                    emit(Event::LinkPreview { url, card: Some(c) });
                }
                Command::Send { text, link, view_once, .. } => {
                    let Some(items) = chats.get_mut(&current) else { continue };
                    let ts = view::time::now_ms();
                    let mut m = view::outgoing_stub(ts, items.last().map(|m| m.ts));
                    m.text = text.trim_end().into();
                    if view_once {
                        m.view_once = ViewOnce::Sent;
                    }
                    items.push(m);
                    let show = |items: &mut Vec<MsgDto>| {
                        view::mark_runs(items, current == GROUP);
                        emit(Event::Messages { key: current.clone(), items: items.clone(), paging: Paging::default(), scroll: Scroll::Bottom });
                    };
                    show(items);
                    // the preview is fetched while the message waits, then both go
                    std::thread::sleep(std::time::Duration::from_millis(900));
                    if let Some(url) = link {
                        emit(Event::Thumb(Arc::new(scene_thumb("real-sent", (300, 158), LINK_PHOTO))));
                        items.last_mut().unwrap().link = Some(card(&url, &sent_link.0, &sent_link.1, "real-sent"));
                    }
                    let m = items.last_mut().unwrap();
                    m.pending = false;
                    m.status = "✓".into();
                    m.can_edit = true;
                    m.can_delete = true;
                    show(items);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    items.last_mut().unwrap().status = "✓✓".into();
                    show(items);
                    // the group reads it one by one: each photo moves down from our last message,
                    // as a reader shows on the newest message they read only
                    let readers: Vec<_> = items.iter().rev().skip(1).find(|m| !m.readers.is_empty()).map(|m| m.readers.clone()).unwrap_or_default();
                    for (i, r) in readers.into_iter().enumerate() {
                        std::thread::sleep(std::time::Duration::from_millis(if i == 0 { 900 } else { 600 }));
                        for m in items.iter_mut() {
                            m.readers.retain(|x| x.0 != r.0);
                        }
                        items.last_mut().unwrap().readers.push(r);
                        show(items);
                    }
                }
                Command::ViewImage { id, index, fit } => {
                    let once = current == ONCE;
                    if once && chats[ONCE].iter().any(|m| m.id == id && m.view_once != ViewOnce::Photo) {
                        emit(Event::Notice(tr!("This view-once media can't be opened anymore.").into()));
                        continue;
                    }
                    let Some((variants, size)) = viewed(&current, &id) else { continue };
                    let index = index.min(variants.len() - 1);
                    let Ok(png) = sigwin_thumb::png_rgba(&scene(size.0, size.1, variants[index])) else { continue };
                    let Ok(Some(view)) = sigwin_thumb::decode_view(&png, fit.0, fit.1, u64::MAX) else { continue };
                    let pixels = types::ViewerPixels::from(view);
                    emit(Event::ViewerImage(types::ViewerImage { id, index, count: variants.len(), pixels, animated: false, pending: String::new(), once }));
                }
                // shown once: it reads as viewed from now on
                Command::ViewOnceSeen(id) => {
                    let Some(items) = chats.get_mut(&current) else { continue };
                    let Some(m) = items.iter_mut().find(|m| m.id == id && m.view_once == ViewOnce::Photo) else { continue };
                    m.view_once = ViewOnce::Viewed;
                    emit(Event::Messages { key: current.clone(), items: items.clone(), paging: Paging::default(), scroll: Scroll::KeepIfNotAtBottom });
                }
                Command::ViewerThumbs { id, px } => {
                    let Some((variants, _)) = viewed(&current, &id) else { continue };
                    for (index, &v) in variants.iter().enumerate() {
                        let t = scene(px, px, v);
                        let (width, height) = t.dimensions();
                        emit(Event::ViewerThumb(id.clone(), index, types::Thumb { key: id.clone(), width, height, rgba: t.into_raw() }));
                    }
                }
                _ => {}
            }
        }
    });
}

/// The conversation header; the family is us and three others.
fn header(emit: &backend::Emit, threads: &[ThreadDto], key: &str) {
    if key != GROUP {
        return super::header(emit, threads, key, false);
    }
    if let Some(t) = threads.iter().find(|t| t.key == key) {
        emit(Event::Header { name: t.name.clone(), sub: tr!("{n} member" | "{n} members" % 4), request: false, common_groups: 0 });
    }
}

/// The photos of a message the viewer opens: scene variants and the original's size.
fn viewed(thread: &str, id: &str) -> Option<(&'static [usize], (u32, u32))> {
    match (thread, id) {
        (CHAT, "2") => Some((std::slice::from_ref(&CHAT_PHOTO.1), (2400, 1800))),
        (GROUP, "4") => Some((&GROUP_PHOTOS, (1800, 1800))),
        (ONCE, id) if id == ONCE_PHOTO.0 => Some((std::slice::from_ref(&ONCE_PHOTO.1), (1600, 2000))),
        _ => None,
    }
}

fn send_thumbs(emit: &backend::Emit) {
    let send = |key: &str, size: (u32, u32), v: usize| emit(Event::Thumb(Arc::new(scene_thumb(key, size, v))));
    send(CHAT_PHOTO.0, (400, 300), CHAT_PHOTO.1);
    send(&format!("{}-q", CHAT_PHOTO.0), crate::view::QUOTE_IMAGE, CHAT_PHOTO.1);
    for (n, &v) in GROUP_PHOTOS.iter().enumerate() {
        send(&format!("real-grid-{n}"), view::media::grid_cell_size(GROUP_PHOTOS.len(), n), v);
    }
}

/// A thumbnail `size` logical px big, at the window's scale like real ones.
fn scene_thumb(key: &str, size: (u32, u32), v: usize) -> types::Thumb {
    let img = scene(types::physical_px(size.0), types::physical_px(size.1), v);
    types::Thumb { key: key.into(), width: img.width(), height: img.height(), rgba: img.into_raw() }
}

/// A painted landscape (sky, sun, hills, a lake reflecting them), so the pictures look like
/// photos without being anyone's.
fn scene(w: u32, h: u32, v: usize) -> image::RgbaImage {
    struct Pal {
        top: [f32; 3],
        bottom: [f32; 3],
        sun: [f32; 3],
        sun_at: (f32, f32),
        hills: [[f32; 3]; 3],
    }
    let pals = [
        // dusk
        Pal { top: [38., 46., 102.], bottom: [247., 164., 120.], sun: [255., 226., 170.], sun_at: (0.62, 0.50), hills: [[152., 112., 150.], [96., 74., 122.], [48., 42., 82.]] },
        // clear morning
        Pal { top: [74., 140., 214.], bottom: [206., 228., 242.], sun: [255., 251., 235.], sun_at: (0.25, 0.20), hills: [[140., 170., 198.], [86., 126., 160.], [44., 84., 104.]] },
        // evening
        Pal { top: [26., 30., 72.], bottom: [214., 112., 132.], sun: [255., 200., 170.], sun_at: (0.40, 0.55), hills: [[118., 82., 128.], [72., 54., 100.], [34., 30., 62.]] },
        // green valley
        Pal { top: [104., 168., 226.], bottom: [222., 238., 244.], sun: [255., 252., 236.], sun_at: (0.78, 0.18), hills: [[128., 170., 150.], [74., 132., 96.], [38., 92., 62.]] },
    ];
    let p = &pals[v % pals.len()];
    const HORIZON: f32 = 0.72;
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
    let aspect = w as f32 / h as f32;
    let px = 1.0 / h as f32;
    let phase = v as f32 * 1.9;
    let land = |fx: f32, fy: f32| {
        let mut c = mix(p.top, p.bottom, (fy / HORIZON).clamp(0.0, 1.0).powf(1.3));
        let d = (((fx - p.sun_at.0) * aspect).powi(2) + (fy - p.sun_at.1).powi(2)).sqrt();
        c = mix(c, p.sun, (-d * 6.0).exp() * 0.5);
        c = mix(c, p.sun, ((0.06 - d) / px + 0.5).clamp(0.0, 1.0));
        for (i, &col) in p.hills.iter().enumerate() {
            let f = i as f32;
            let s = 0.5 * (fx * (2.3 + f) * aspect + phase + f).sin()
                + 0.3 * (fx * (5.7 + 2.0 * f) * aspect + phase * 1.3).sin()
                + 0.2 * (fx * (11.0 + 3.0 * f) * aspect + phase * 2.1 + f).sin();
            let ridge = [0.50, 0.58, 0.66][i] - [0.16, 0.12, 0.08][i] * (s + 1.0) / 2.0;
            c = mix(c, col, ((fy - ridge) / px + 0.5).clamp(0.0, 1.0));
        }
        c
    };
    image::RgbaImage::from_fn(w, h, |x, y| {
        let (fx, fy) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
        let c = if fy < HORIZON {
            land(fx, fy)
        } else {
            let depth = (fy - HORIZON) / (1.0 - HORIZON);
            let ripple = 0.004 * (fy * 900.0).sin() * (1.0 - depth);
            let mirrored = land(fx + ripple, 2.0 * HORIZON - fy);
            mix(mirrored, [p.hills[2][0] * 0.6, p.hills[2][1] * 0.6, p.hills[2][2] * 0.6], 0.3 + 0.4 * depth)
        };
        image::Rgba([c[0] as u8, c[1] as u8, c[2] as u8, 255])
    })
}
