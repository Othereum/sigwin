//! Demo modes: a fake backend with synthetic data (`SIGWIN_DEMO`), and the memory self-test.

use super::*;
use crate::gui::{initial, tint_rgb};
use crate::types::{MsgDto, Mute, MuteSet, Paging, Scroll, ThreadDto, ThreadPrefs, MUTE_ALWAYS};

/// Demo mode: the fake backend plus the `SIGWIN_DEMO_*` / `SIGWIN_SELFTEST` extras.
pub(crate) fn start(app: &AppWindow, emit: backend::Emit, cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>) {
    demo(emit, cmd_rx);
    if std::env::var_os("SIGWIN_DEMO_TICK").is_some() {
        // UI updates arriving continuously, like a live backend (also while minimized)
        let weak = app.as_weak();
        let n = std::cell::Cell::new(0u32);
        let timer = Box::leak(Box::new(slint::Timer::default()));
        timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(300), move || {
            n.set(n.get() + 1);
            if let Some(app) = weak.upgrade() {
                app.set_connection_text(format!("tick {}", n.get()).into());
            }
        });
    }
    if let Ok(v) = std::env::var("SIGWIN_DEMO_BADGE") {
        // the real tray and taskbar badges, stepping through none and 1 to 10
        let weak = app.as_weak();
        let n = std::cell::Cell::new(0u32);
        let timer = Box::leak(Box::new(slint::Timer::default()));
        timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(v.parse().unwrap_or(2000)), move || {
            n.set((n.get() + 1) % 11);
            if let Some(app) = weak.upgrade() {
                app.set_total_unread(n.get() as i32);
            }
            badge::set_count(n.get(), "메시지");
        });
    }
    if std::env::var_os("SIGWIN_DEMO_INFO").is_some() {
        // the jump button's unread badge, set late so a test can scroll up first
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(4000), move || {
            if let Some(app) = weak.upgrade() {
                app.set_open_unread(3);
            }
        });
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
            if let Some(app) = weak.upgrade() {
                app.set_reply_author("이서연".into());
                app.set_reply_preview("답장 미리보기".into());
                app.set_info_text("읽음 (2)
김민준, 이서연

전달됨 (1)
박지훈".into());
            }
        });
    }
    if let Ok(what) = std::env::var("SIGWIN_DEMO_MENU") {
        // message menu on an outgoing row, typing indicator, edit banner
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
            let Some(app) = weak.upgrade() else { return };
            apply(&app, Event::Typing { group: true, who: vec![("demo-a".into(), "김민준".into())] });
            apply(&app, Event::RecentReactions(["🎉", "🙏", "👍", "🔥"].map(String::from).to_vec()));
            if what == "edit" {
                app.invoke_edit_message("294".into());
            } else if what == "pin" {
                app.invoke_open_menu("297".into(), 520.0, 300.0);
                app.set_menu_pin_step(true);
            } else if let Some(tab) = what.strip_prefix("picker").map(|t| t.parse::<i32>().unwrap_or(0)) {
                // "picker" or "picker<tab>": the emoji picker on that tab
                app.invoke_open_menu("297".into(), 520.0, 200.0);
                app.set_menu_picker(true);
                app.set_picker_tab(tab);
                app.invoke_picker_tab_changed(tab);
            } else {
                app.invoke_open_menu("297".into(), 520.0, 300.0);
            }
        });
    }
    if let Ok(v) = std::env::var("SIGWIN_DEMO_UPDATE") {
        // the "new version ready" bar; clicking it restarts the demo for real
        app.global::<SettingsData>().set_update_version(v.into());
    }
    if let Ok(v) = std::env::var("SIGWIN_DEMO_UPDATE_AVAILABLE") {
        // the "new version available" bar (automatic download off)
        app.global::<SettingsData>().set_update_available(v.into());
    }
    if std::env::var_os("SIGWIN_DEMO_WHATSNEW").is_some() {
        // the "updated, what's new" offer bar
        app.set_whatsnew_offer(true);
    }
    if std::env::var_os("SIGWIN_DEMO_SETTINGS").is_some() {
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
            if let Some(app) = weak.upgrade() {
                app.invoke_open_settings();
            }
        });
    }
    if std::env::var_os("SIGWIN_DEMO_STICKERS").is_some() {
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
            if let Some(app) = weak.upgrade() {
                app.set_sticker_open(true);
            }
        });
    }
    if std::env::var_os("SIGWIN_SELFTEST").is_some() {
        selftest();
    }
}


/// Synthetic data for checking layout and memory without a Signal account.
pub(crate) fn demo(emit: backend::Emit, mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>) {
    if std::env::var_os("SIGWIN_DEMO_REAL").is_some() {
        return demo_real(emit, cmd_rx);
    }
    let threads: Vec<ThreadDto> = (0..40)
        .map(|i| ThreadDto {
            key: format!("c:demo-{i}"),
            name: ["김민준", "이서연", "박지훈", "가족 단톡방", "Alice", "프로젝트 팀"][i % 6].to_string(),
            snippet: "내일 몇 시에 만날까? 저녁 7시쯤 어때요".into(),
            time: if i < 3 { "오후 3:12".into() } else { format!("10월 {}일", 30 - (i % 28)) },
            ts: 0,
            unread: if i % 5 == 0 { (i % 7) as u32 + 1 } else { 0 },
            is_group: i % 6 == 3 || i % 6 == 5,
            // off on the phone (forever / for a while), and off in Sigwin only
            mute: match i {
                3 => Mute { until: i64::MAX as u64, overridden: false, phone: i64::MAX as u64 },
                5 => Mute { until: view::time::now_ms() + 2 * 3_600_000, overridden: false, phone: view::time::now_ms() + 2 * 3_600_000 },
                10 => Mute { until: MUTE_ALWAYS, overridden: true, phone: 0 },
                _ => Mute::default(),
            },
            // own settings in one chat, the phone's mention setting in a group
            prefs: if i == 5 { ThreadPrefs { preview: Some(1), sound: Some(false), badge: Some(false), ..Default::default() } } else { ThreadPrefs::default() },
            phone_mentions: (i == 3).then_some(false),
            // our last message, read by one with a photo, one without, and by a crowd
            readers: match i {
                0 | 4 => vec![("demo-a".into(), "이서연".into())],
                1 => vec![("demo-b".into(), "박지훈".into())],
                3 => vec![("demo-a".into(), "이서연".into()), ("demo-b".into(), "박지훈".into()), ("demo-c".into(), "최유나".into())],
                _ => Vec::new(),
            },
            readers_more: if i == 3 { 2 } else { 0 },
        })
        .collect();
    let mut items = Vec::new();
    for i in 0..300u32 {
        // the last row is a message still being sent
        let outgoing = i % 3 == 0 || i == 299;
        items.push(MsgDto {
            id: i.to_string(),
            // 40s apart, a 10 minute pause every 6 rows (runs break there)
            ts: 1_700_000_000_000 + i as u64 * 40_000 + (i as u64 / 6) * 600_000,
            // a group: runs from two senders
            sender: if outgoing { String::new() } else if i % 5 < 3 { "이서연".into() } else { "박지훈".into() },
            sender_key: if outgoing { String::new() } else if i % 5 < 3 { "demo-a".into() } else { "demo-b".into() },
            text: if i % 7 == 0 {
                "긴 메시지 테스트입니다. 한글 줄바꿈이 제대로 되는지, 말풍선 최대 폭이 적당한지 확인하기 위한 문장입니다. 🙂".into()
            } else {
                if i == 295 { "기호·이모지 테스트: ✓ ✕ ➤ ★ ♥ → 😀🎉👍❤️🇰🇷".to_string() } else if i == 296 { "🎉🥳👍🏽".to_string() } else { format!("메시지 {i}") }
            },
            time: "오후 3:12".into(),
            outgoing,
            status: if outgoing { ["✓", "✓✓", "✓✓"][(i as usize / 3) % 3].into() } else { String::new() },
            readers: if outgoing && (i as usize / 3) % 3 == 2 {
                let all = [("demo-a", "이서연"), ("demo-b", "김민준"), ("demo-c", "박지훈"), ("demo-d", "최승민"), ("demo-e", "권준영")];
                all.iter().take(if i % 2 == 0 { 2 } else { 5 }).map(|(u, n)| (u.to_string(), n.to_string())).collect()
            } else {
                Vec::new()
            },
            receipt_info: if outgoing { "읽음 (2)
김민준, 이서연

전달됨 (1)
박지훈

※ 읽음 확인을 끈 사람은 '읽음'으로 표시되지 않습니다.".into() } else { String::new() },
            // tall rows at the end check that scrolling to the bottom reaches it
            quote: if i % 11 == 0 || i >= 298 { "이전 메시지 인용".into() } else { String::new() },
            quote_author: if i % 11 == 0 || i >= 298 { if i % 2 == 0 { "나".into() } else { "이서연".into() } } else { String::new() },
            // demo ids are row ids: quote a row some way up (the last rows: near the top of the
            // page that SIGWIN_DEMO_OLDER pages in for it)
            quote_id: if i >= 298 { (i - 175).to_string() } else if i % 11 == 0 { i.saturating_sub(40).to_string() } else { String::new() },
            reactions: if i % 9 == 0 { vec![("❤️".into(), 2, "이서연, 나".into()), ("👍".into(), 1, "박지훈".into()), ("😂".into(), 3, "이서연, 박지훈, 최민수".into())] } else { Vec::new() },
            // some with reactions too (every 9th), some alone
            replies: if i % 7 == 3 { (i % 4 + 1) as u32 } else { 0 },
            attachment: if i % 13 == 0 { "보고서.pdf · 2.4MB".into() } else { String::new() },
            day_sep: i % 50 == 0,
            // 50 rows a day, the newest today
            day: crate::view::time::day_num(&chrono::Local::now()) - (299 - i as i32) / 50,
            sticker: i == 298 || i == 297,
            image_key: if i == 298 || i == 297 { format!("demo-st-{}", i % 2) } else { String::new() },
            img_w: if i == 298 || i == 297 { 140 } else { 0 },
            img_h: if i == 298 || i == 297 { 140 } else { 0 },
            my_reaction: if i % 9 == 0 { "❤️".into() } else { String::new() },
            can_edit: outgoing && i != 297 && i != 298,
            can_delete: outgoing,
            ..Default::default()
        });
        if i == 299 {
            let m = items.last_mut().unwrap();
            m.pending = true;
            m.status = "전송 중".into();
            m.receipt_info = "서버로 보내는 중입니다.".into();
        }
    }
    // failed to send
    {
        let m = &mut items[296];
        m.outgoing = true;
        m.pending = true;
        m.failed = true;
        m.status = "실패".into();
        m.receipt_info = "보내지 못했습니다.".into();
    }
    // deleted for everyone: by us (right) and by someone else (left)
    view::set_deleted(&mut items[293]);
    items[292] = MsgDto { id: "292".into(), ts: items[292].ts, time: "오후 3:12".into(), deleted: true, text: "삭제된 메시지입니다".into(), ..Default::default() };
    // mentions, incoming and outgoing, with a line break and wrapping
    for (i, text) in [
        (290usize, "@김민준 이거 확인 부탁해요\n그리고 @박지훈 님도 내일 회의 전에 문서 한 번 읽어 주시면 좋겠습니다 <b>*그대로*</b>"),
        (291, "@이서연 넵 알겠습니다 🙂 자료는 https://signal.org/ko/ 그리고 www.naver.com."),
        (289, "https://example.com/a_(b) 링크로 시작 가나다라마바사 아자차카타파하 가나다라마바사 아자차카타파하 가나다 보세요 https://x.com/abc 끝 그리고 줄 끝까지 채우는 문장 www.naver.com"),
    ] {
        let m = &mut items[i];
        m.text = text.into();
        m.mentions = text.match_indices('@').map(|(s, _)| (s, s + text[s..].find(' ').unwrap(), String::new())).collect();
    }
    // long messages cut short with "더 읽기", incoming and outgoing
    for i in [286usize, 285] {
        let m = &mut items[i];
        m.text = format!("{}…", "긴 메시지의 앞부분입니다. ".repeat(25).trim_end());
        m.more = true;
        m.can_edit = false;
    }
    // text styles, incoming and outgoing: bold, italic, strikethrough, plain, spoiler, monospace
    for i in [288usize, 287] {
        let text = "굵게 기울임 취소선 보통 스포일러 내용 mono(iiWW) 끝 www.naver.com";
        let m = &mut items[i];
        m.text = text.into();
        let at = |w: &str| {
            let s = text.find(w).unwrap();
            (s, s + w.len())
        };
        let mut st = |w, k| {
            let (s, e) = at(w);
            m.styles.push((s, e, k));
        };
        st("굵게", view::text::STYLE_BOLD);
        st("기울임", view::text::STYLE_ITALIC);
        st("취소선", view::text::STYLE_STRIKE);
        st("스포일러 내용", view::text::STYLE_SPOILER);
        st("mono(iiWW)", view::text::STYLE_MONO);
    }
    emit(Event::Ready);
    emit(Event::Threads(threads.clone()));
    // photos on some threads, through the same crop/resample/circle path as real ones
    let photo = |key: String, i: u32, size: u32| {
        let src = image::RgbaImage::from_fn(320, 240, |x, y| {
            let (cx, cy) = (x as f32 - 160.0, y as f32 - 110.0);
            let face = cx * cx + cy * cy < 60.0f32.powi(2);
            if face { image::Rgba([250, 210, 170, 255]) } else { image::Rgba([(x / 2) as u8, (40 * i) as u8, (y / 2 + 60) as u8, 255]) }
        });
        let mut png = Vec::new();
        if src.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).is_ok() {
            if let Ok(r) = avatar::round(&png, types::physical_px(size)) {
                let t = types::Thumb { key, width: r.width(), height: r.height(), rgba: r.into_raw() };
                emit(Event::Avatar(Arc::new(t)));
            }
        }
    };
    for i in [0u32, 1, 2, 3, 7] {
        photo(format!("c:demo-{i}"), i, avatar::SIZE);
    }
    // SIGWIN_DEMO_NOPHOTO leaves our own photo out, to see the initial placeholder
    if std::env::var_os("SIGWIN_DEMO_NOPHOTO").is_none() {
        photo(avatar::ME_SIDEBAR.into(), 4, avatar::ME_SIDEBAR_SIZE);
        photo(avatar::ME_CARD.into(), 4, avatar::ME_CARD_SIZE);
    }
    emit(Event::Header { name: "이서연".into(), sub: "+82 10-0000-0000".into() });
    let names = ["김민준", "이서연", "박지훈", "Alice Kim", "Bob", "김지원", "최유나"];
    emit(Event::Members(names.iter().enumerate().map(|(i, n)| (format!("00000000-0000-0000-0000-00000000000{i}"), n.to_string())).collect()));
    for m in items.iter_mut().filter(|m| m.attachment.is_empty() && m.quote.is_empty() && !m.sticker) {
        m.big_emoji = emoji::only_emoji(&m.text, 5).is_some();
    }
    for i in [150usize, 290] {
        items[i].pinned = true;
    }
    // some quotes show the quoted photo
    for (n, m) in items.iter_mut().filter(|m| !m.quote.is_empty()).enumerate() {
        if n % 2 == 0 {
            m.quote_image_key = "demo-quote-photo".into();
        }
    }
    {
        // after the rows, as downloads do
        let emit = emit.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            emit(Event::Thumb(Arc::new(demo_link_image("demo-quote-photo", 52, 52))));
        });
    }
    view::mark_runs(&mut items, true);
    let pins: Vec<types::PinDto> = [(290usize, "이서연"), (150, "나")]
        .iter()
        .map(|&(i, who)| types::PinDto { id: i.to_string(), sender: who.into(), text: items[i].text.replace('\n', " ") })
        .collect();
    let reload = std::env::var_os("SIGWIN_DEMO_RELOAD").is_some();
    if std::env::var_os("SIGWIN_DEMO_VIEWER").is_some() {
        // the newest row, so a click reaches it without scrolling
        if let Some(m) = items.last_mut() {
            view::media::set_single_photo(m, "demo-photo".into(), (400, 300));
        }
    }
    if std::env::var_os("SIGWIN_DEMO_LINK").is_some() {
        demo_link_rows(&mut items);
    }
    if std::env::var_os("SIGWIN_DEMO_VIDEO").is_some() {
        demo_video_rows(&mut items);
    }
    if std::env::var_os("SIGWIN_DEMO_GRID").is_some() {
        // the newest rows, bare, with a caption and with a file
        let last = items.len() - DEMO_GRIDS.len();
        for (&count, m) in DEMO_GRIDS.iter().zip(&mut items[last..]) {
            m.big_emoji = false;
            m.sticker = false;
            m.quote.clear();
            m.quote_author.clear();
            m.attachment = if count == 5 { "setup.exe · 2.4MB".into() } else { String::new() }; m.file_danger = count == 5;
            m.text = if count == 3 { "사진 설명이 함께 있는 메시지".into() } else { String::new() };
            view::media::set_photo_grid(m, count, |n, _, _| format!("demo-grid-{count}-{n}"));
        }
    }
    if reload {
        // like a real account: photos make rows very uneven in height
        for (i, m) in items.iter_mut().enumerate().filter(|(i, _)| i % 9 == 4 && *i < 290) {
            view::media::set_single_photo(m, format!("demo-photo-{i}"), if i % 2 == 0 { (400, 300) } else { (300, 400) });
        }
    }
    // very long messages: `SIGWIN_DEMO_TALL=<row>:<chars>[,...]`
    if let Ok(v) = std::env::var("SIGWIN_DEMO_TALL") {
        for (row, chars) in v.split(',').filter_map(|p| p.split_once(':')) {
            let (Ok(row), Ok(chars)) = (row.parse::<usize>(), chars.parse::<usize>()) else { continue };
            // a line per 20 characters, so the row gets as tall as text can make it
            let text: String = "긴 메시지의 내용입니다. 줄이 아주 많습니다
".chars().cycle().take(chars).collect();
            items[row].text = text;
        }
    }
    if let Ok(v) = std::env::var("SIGWIN_DEMO_OLDER") {
        // history paged in like a real thread: the newest 60 rows, then 60 more per LoadOlder or
        // LoadNewer, each after `v` ms (default 150). `SIGWIN_DEMO_CAP=<rows>` bounds the window
        // like `MAX_WINDOW`, so it moves through the rows
        let delay = std::time::Duration::from_millis(v.parse().unwrap_or(150));
        let cap: usize = std::env::var("SIGWIN_DEMO_CAP").ok().and_then(|c| c.parse().ok()).unwrap_or(usize::MAX);
        let total = items.len();
        let (mut start, mut end) = (total - 60, total);
        // like view::build: the first row of the window always starts a day
        let all = items.clone();
        let rows = move |start: usize, end: usize| {
            let mut rows = all[start..end].to_vec();
            rows[0].day_sep = true;
            view::mark_runs(&mut rows, true);
            rows
        };
        let paging = move |start: usize, end: usize| Paging { older: start > 0, newer: end < total };
        emit(Event::Messages { key: "c:demo-1".into(), items: rows(start, end), paging: paging(start, end), scroll: Scroll::Bottom });
        let emit = emit.clone();
        std::thread::spawn(move || {
            while let Some(c) = cmd_rx.blocking_recv() {
                demo_frames(&emit, &c);
                let show = |start: usize, end: usize, scroll| {
                    emit(Event::Messages { key: "c:demo-1".into(), items: rows(start, end), paging: paging(start, end), scroll });
                };
                match &c {
                    Command::LoadOlder if start > 0 => {
                        std::thread::sleep(delay);
                        start = start.saturating_sub(60);
                        end = end.min(start.saturating_add(cap));
                        show(start, end, Scroll::Older);
                    }
                    Command::LoadNewer if end < total => {
                        std::thread::sleep(delay);
                        end = (end + 60).min(total);
                        start = start.max(end.saturating_sub(cap));
                        show(start, end, Scroll::Keep);
                    }
                    Command::ToLatest => {
                        (start, end) = (total - 60, total);
                        show(start, end, Scroll::Bottom);
                    }
                    // like App::settle_window, without its wait
                    Command::AtBottom(true) if end == total && end - start > 60 => {
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                        start = total - 60;
                        show(start, end, Scroll::KeepIfNotAtBottom);
                    }
                    // like App::jump_to: move the window to the target, margin either side
                    Command::JumpTo(id) => {
                        let Ok(target) = id.parse::<usize>() else { continue };
                        if target < start + 30 || target + 30 >= end.max(1) && end < total {
                            std::thread::sleep(delay);
                            start = target.saturating_sub(60);
                            end = (target + 60).min(total);
                            show(start, end, Scroll::Keep);
                        }
                        emit(Event::JumpTo(id.clone()));
                    }
                    _ => {}
                }
            }
        });
    } else {
        // `SIGWIN_DEMO_UNREAD=<n>`: the thread opens with its last n rows unread (the line above
        // the first, the list scrolled to it, the badge on the jump button)
        let unread: usize = std::env::var("SIGWIN_DEMO_UNREAD").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut first = items.clone();
        let mut scroll = Scroll::Bottom;
        if unread > 0 && unread < first.len() {
            let at = first.len() - unread;
            first[at].unread_divider = true;
            view::mark_runs(&mut first, true);
            scroll = Scroll::Unread;
            let mut list = threads.clone();
            if let Some(t) = list.iter_mut().find(|t| t.key == "c:demo-1") {
                t.unread = unread as u32;
            }
            emit(Event::Threads(list));
        }
        emit(Event::Messages { key: "c:demo-1".into(), items: first, paging: Paging::default(), scroll });
        let notes = std::env::var("SIGWIN_DEMO_NOTIFY").ok().map(|v| demo_notify(emit.clone(), v.parse().unwrap_or(10)));
        // clicking another thread shows the same rows under its key (drafts follow the key)
        let (emit, items, all_pins) = (emit.clone(), items.clone(), pins.clone());
        let mut threads = threads;
        std::thread::spawn(move || {
            while let Some(c) = cmd_rx.blocking_recv() {
                demo_frames(&emit, &c);
                if let Command::Replies(id) = &c {
                    // the rows after it stand in for its replies (demo ids are row ids)
                    let at = id.parse::<usize>().unwrap_or(0);
                    let list = items
                        .iter()
                        .skip(at + 1)
                        .take(items.get(at).map_or(0, |m| m.replies as usize))
                        .map(|m| types::ReplyDto { id: m.id.clone(), sender: if m.outgoing { "나".into() } else { m.sender.clone() }, text: m.text.replace('\n', " "), time: m.time.clone() })
                        .collect();
                    emit(Event::Replies(list));
                }
                if let Command::SetMute { key, set } = &c {
                    demo_mute(&mut threads, key, *set);
                    emit(Event::Threads(threads.clone()));
                }
                if let Command::SetPref { key, pref } = &c {
                    if let Some(t) = threads.iter_mut().find(|t| &t.key == key) {
                        t.prefs.set(*pref);
                    }
                    emit(Event::Threads(threads.clone()));
                }
                let open = match &c {
                    Command::OpenThread(key) => Some((key.clone(), None)),
                    Command::OpenFromToast { key, msg } => Some((key.clone(), *msg)),
                    _ => None,
                };
                if let Some((key, msg)) = open {
                    // threads differ in length, like real ones (scroll positions don't carry over)
                    let n: usize = key.trim_start_matches("c:demo-").parse().unwrap_or(1);
                    let items = if n == 1 { items.clone() } else { items[items.len() - (10 + n * 7).min(items.len())..].to_vec() };
                    // pins only in the first thread, sent after the rows like App::emit_messages
                    let pins = if key == "c:demo-1" { all_pins.clone() } else { Vec::new() };
                    emit(Event::Messages { key, items, paging: Paging::default(), scroll: Scroll::Bottom });
                    emit(Event::Pins(pins));
                    if let Some(m) = msg {
                        emit(Event::JumpTo(m.to_string()));
                    }
                }
                if let Some(tx) = &notes {
                    let _ = tx.send(c);
                }
            }
        });
    }
    emit(Event::Pins(pins));
    if reload && std::env::var_os("SIGWIN_DEMO_OLDER").is_none() {
        // ... and the list is re-sent all the time (receipts, typing, reactions)
        let emit = emit.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            emit(Event::Messages {
                key: "c:demo-1".into(),
                items: items.clone(),
                paging: Paging::default(),
                scroll: Scroll::KeepIfNotAtBottom,
            });
        });
    }
    // one sender with a photo, the other falls back to the initial
    photo(format!("{}demo-a", avatar::MSG_PREFIX), 1, avatar::MSG_SIZE);
    photo(format!("{}demo-a", avatar::READ_PREFIX), 1, avatar::READ_SIZE);
    photo(format!("{}demo-a", avatar::LIST_READ_PREFIX), 1, avatar::LIST_READ_SIZE);
    if let Ok(text) = std::env::var("SIGWIN_DEMO_NOTICE") {
        emit(Event::Notice(text));
    }
    if let Ok(text) = std::env::var("SIGWIN_DEMO_CONNECTION") {
        emit(Event::Connection { text, at_once: false });
    }
    if std::env::var_os("SIGWIN_DEMO_STATUS").is_some() {
        demo_status(emit.clone());
    }
    emit(Event::Account {
        name: "홍길동".into(),
        number: "+821012345678".into(),
        device: "Sigwin".into(),
        receipts: Some(true),
        typing: None,
    });
    emit(Event::Storage { db: 23 * 1024 * 1024, attachments: 148 * 1024 * 1024, thumbs: 3_400_000 });
    if std::env::var_os("SIGWIN_DEMO_VIDEO").is_some() {
        let emit = emit.clone();
        std::thread::spawn(move || demo_videos(&emit));
    }
    if std::env::var_os("SIGWIN_DEMO_GRID").is_some() {
        // after the rows, as downloads do
        let emit = emit.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            demo_grids(&emit);
        });
    }
    if std::env::var_os("SIGWIN_DEMO_LINK").is_some() {
        // after the rows, as downloads do; one image never arrives
        let emit = emit.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            emit(Event::Thumb(Arc::new(demo_link_image("demo-link-wide", 300, 158))));
            emit(Event::Thumb(Arc::new(demo_link_image("demo-link-small", 64, 64))));
        });
    }
    if std::env::var_os("SIGWIN_DEMO_STICKERS").is_some() {
        demo_stickers(emit);
    }
}

fn demo_link_image(key: &str, w: u32, h: u32) -> types::Thumb {
    let (pw, ph) = (types::physical_px(w), types::physical_px(h));
    // thin diagonal lines: any nearest-neighbor stretch of the card shows as jagged steps
    let img = image::RgbaImage::from_fn(pw, ph, |x, y| {
        if (x + y) % 24 == 0 { image::Rgba([255, 255, 255, 255]) } else { image::Rgba([(x * 255 / pw) as u8, 90, (y * 255 / ph) as u8, 255]) }
    });
    types::Thumb { key: key.into(), width: pw, height: ph, rgba: img.into_raw() }
}

/// SIGWIN_DEMO_LINK: the newest rows carry previews of every shape.
fn demo_link_rows(items: &mut [types::MsgDto]) {
    let card = |title: &str, description: &str, domain: &str, key: &str, size: (u32, u32)| {
        Some(types::LinkCard {
            url: format!("https://{domain}/demo"),
            title: title.into(),
            description: description.into(),
            domain: domain.into(),
            image_key: key.into(),
            img_w: size.0,
            img_h: size.1,
        })
    };
    let rows = [
        ("https://youtube.com/demo 이거 봐봐 창이 넓을 때 글이 길어서 말풍선이 카드보다 넓어지는 경우를 보려고 일부러 길게 쓴 메시지입니다 정말로 길게 씁니다 더 길게 씁니다 더 더 길게", false, card("합성 데이터로 만든 영상 제목인데 창이 넓을 때 카드가 늘어나는지 보려고 일부러 길게 쓴 제목입니다", "영상 설명이 이 자리에 나옵니다.", "youtube.com", "demo-link-wide", (300, 158))),
        ("https://www.example.kr/demo", true, card("내가 보낸 링크의 제목", "", "example.kr", "demo-link-wide", (300, 158))),
        ("https://news.example.kr/demo", false, card("아주 긴 뉴스 제목이 두 줄을 넘어가면 어떻게 잘리는지 확인하기 위해 일부러 길게 쓴 제목입니다 정말로 길게 썼습니다", "설명도 길게 씁니다. 두 줄을 넘는 설명은 끝에서 잘리고 말줄임표가 나와야 합니다. 그래서 충분히 길게 씁니다. 더 길게 씁니다.", "news.example.kr", "demo-link-small", (64, 64))),
        ("https://blog.example.kr/demo", false, card("이미지 없는 미리보기", "설명만 있습니다.", "blog.example.kr", "", (0, 0))),
        ("https://photo.example.kr/demo", false, card("이미지가 아직 도착하지 않은 카드", "", "photo.example.kr", "demo-link-missing", (300, 158))),
    ];
    let last = items.len() - rows.len();
    for ((text, outgoing, link), m) in rows.into_iter().zip(&mut items[last..]) {
        m.text = text.into();
        m.outgoing = outgoing;
        m.link = link;
        m.big_emoji = false;
        m.sticker = false;
        m.image_key.clear();
        m.attachment.clear();
        m.quote.clear();
        m.quote_author.clear();
    }
}

/// `App::link_preview`: any link gets a card shortly, except on a host starting with "fail".
fn demo_link_fetch(emit: &backend::Emit, url: &str) {
    std::thread::sleep(std::time::Duration::from_millis(300));
    let domain = crate::link::domain(url);
    let card = (!domain.starts_with("fail")).then(|| types::LinkCard {
        url: url.into(),
        title: "입력창 미리보기 제목".into(),
        description: "입력창에 나오는 설명입니다.".into(),
        domain,
        image_key: "demo-compose".into(),
        img_w: 300,
        img_h: 158,
    });
    emit(Event::Thumb(Arc::new(demo_link_image("demo-compose-c", 44, 44))));
    emit(Event::LinkPreview { url: url.into(), card });
}

/// `App::set_mute` on the demo list.
fn demo_mute(threads: &mut [ThreadDto], key: &str, set: MuteSet) {
    let Some(t) = threads.iter_mut().find(|t| t.key == key) else { return };
    let now = view::time::now_ms();
    let phone = t.mute.phone;
    let own = match set {
        MuteSet::Phone => None,
        MuteSet::On if phone <= now => None,
        MuteSet::On => Some(0),
        MuteSet::For(s) => Some(now + s * 1000),
        MuteSet::Always => Some(MUTE_ALWAYS),
    };
    t.mute = match own {
        Some(until) => Mute { until, overridden: true, phone },
        None => Mute { until: phone, overridden: false, phone },
    };
}

/// SIGWIN_SELFTEST=1 (with demo): measure what the file dialog and toasts cost.
pub(crate) fn selftest() {
    let step = std::rc::Rc::new(std::cell::Cell::new(0));
    let timer = Box::leak(Box::new(slint::Timer::default()));
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(4), move || {
        let s = step.get();
        step.set(s + 1);
        eprintln!("selftest step {s}: {}", alloc::report());
        match s {
            1 => {
                picker::spawn(0, |f| eprintln!("selftest picked {} files", f.len()));
            }
            3 => {
                toast::show(toast::Toast {
                    title: "selftest".into(),
                    body: "toast".into(),
                    picture: Some(toast::Picture::Initial { text: "S".into(), rgb: 0x3a77c9 }),
                    key: String::new(),
                    msg: None,
                    sound: true,
                });
            }
            _ => {}
        }
    });
}

pub(crate) fn demo_blob(key: &str, size: u32, hue: u8) -> types::Thumb {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    let c = size as f32 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let a = if d < c * 0.9 { 255 } else { 0 };
            rgba.extend_from_slice(&[hue, 180u8.wrapping_sub(hue / 2), 255 - hue, a]);
        }
    }
    types::Thumb { key: key.into(), width: size, height: size, rgba }
}

/// `SIGWIN_DEMO_NOTIFY=<secs>`: a fake incoming message every `secs` seconds, run through the
/// real notification logic (`backend::notify::Notifier`) and real toasts, so notifications can
/// be tried without anyone sending messages. Cycles through a 1:1 message with a photo, a group
/// message (initial picture), a reaction to one of our messages and a disappearing message.
/// Focus, opening a chat and clicking a toast act like in the real backend.
fn demo_notify(emit: backend::Emit, secs: u64) -> std::sync::mpsc::Sender<Command> {
    use backend::notify::{By, Notifier, Queued, ReactionRef};
    use toast::Picture;
    let (tx, rx) = std::sync::mpsc::channel::<Command>();
    let every = std::time::Duration::from_secs(secs.max(2));
    std::thread::spawn(move || {
        emit(Event::Connection { text: format!("알림 테스트: {}초마다 메시지", every.as_secs()), at_once: true });
        // a demo photo as stored bytes, like a profile avatar
        let src = image::RgbaImage::from_fn(320, 240, |x, y| {
            let (cx, cy) = (x as f32 - 160.0, y as f32 - 110.0);
            if cx * cx + cy * cy < 60.0f32.powi(2) { image::Rgba([250, 210, 170, 255]) } else { image::Rgba([(x / 2) as u8, 40, (y / 2 + 60) as u8, 255]) }
        });
        let photo = sigwin_thumb::png_rgba(&src).unwrap_or_default();
        let letter = |key: &str, name: &str| Picture::Initial { text: initial(name), rgb: tint_rgb(key) };
        let mut notes = Notifier::default();
        let mut focused = true;
        let mut next = std::time::Instant::now() + every;
        let mut n = 0u32;
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(Command::WindowFocus(f)) => {
                    focused = f;
                    if f {
                        notes.clear();
                    }
                }
                Ok(Command::OpenThread(key)) | Ok(Command::OpenFromToast { key, .. }) => notes.remove(By::Thread(&key)),
                Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if std::time::Instant::now() >= next {
                next += every;
                n += 1;
                let q = match n % 4 {
                    1 => Queued {
                        key: "c:demo-0".into(),
                        msg: 290,
                        sender_title: "김민준".into(),
                        message: format!("알림 테스트 {n}: 내일 몇 시에 만날까?"),
                        expiring: false,
                        reaction: None,
                        picture: Some(Picture::Photo(photo.clone())),
                        preview: crate::settings::preview().unwrap_or(0),
                        sound: crate::settings::sound(),
                    },
                    2 => Queued {
                        key: "c:demo-3".into(),
                        msg: 280,
                        sender_title: "가족 단톡방의 이서연 님".into(),
                        message: format!("알림 테스트 {n}: 저녁 같이 먹어요"),
                        expiring: false,
                        reaction: None,
                        picture: Some(letter("c:demo-3", "가족 단톡방")),
                        preview: crate::settings::preview().unwrap_or(0),
                        sound: crate::settings::sound(),
                    },
                    3 => Queued {
                        key: "c:demo-2".into(),
                        msg: 297,
                        sender_title: "박지훈".into(),
                        message: format!("알림 테스트 {n}: 보고서 보냈어요"),
                        expiring: false,
                        reaction: Some(ReactionRef { from: uuid::Uuid::from_u128(2), from_name: "박지훈".into(), emoji: "👍".into() }),
                        picture: Some(letter("c:demo-2", "박지훈")),
                        preview: crate::settings::preview().unwrap_or(0),
                        sound: crate::settings::sound(),
                    },
                    _ => Queued {
                        key: "c:demo-4".into(),
                        msg: 270,
                        sender_title: "Alice".into(),
                        message: "사라지는 메시지 (본문은 표시되지 않아야 함)".into(),
                        expiring: true,
                        reaction: None,
                        picture: Some(letter("c:demo-4", "Alice")),
                        preview: crate::settings::preview().unwrap_or(0),
                        sound: crate::settings::sound(),
                    },
                };
                tracing::info!(n, focused, "demo notify: message");
                notes.add(q);
            }
            notes.tick(focused);
        }
    });
    tx
}

/// `ViewImage`: a large landscape photo, a tiny one and a portrait one with a 1px checker in a
/// corner (blurry or uneven when not drawn 1:1), decoded like real images.
fn demo_viewer(emit: &backend::Emit, id: &str, index: usize, fit: (u32, u32)) {
    // SIGWIN_DEMO_VIEWER=<width>x<height>: size of the first image
    let first = std::env::var("SIGWIN_DEMO_VIEWER").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    let (w, h) = [first.unwrap_or((3000, 2000)), (200, 120), (900, 1400)][index.min(2)];
    let src = image::RgbaImage::from_fn(w, h, |x, y| {
        if x < 64 && y < 64 {
            let c = if (x + y) % 2 == 0 { 255 } else { 0 };
            return image::Rgba([c, c, c, 255]);
        }
        image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 120, 255])
    });
    let Ok(png) = sigwin_thumb::png_rgba(&src) else { return };
    drop(src);
    let Ok(Some(view)) = sigwin_thumb::decode_view(&png, fit.0, fit.1, u64::MAX) else { return };
    let pixels = types::ViewerPixels::from(view);
    let count = demo_viewer_count();
    emit(Event::ViewerImage(types::ViewerImage { id: id.to_string(), index: index.min(count - 1), count, pixels, animated: false }));
}

/// SIGWIN_DEMO_VIEWER_COUNT: images in the album, 3 by default; past the third they repeat it.
fn demo_viewer_count() -> usize {
    std::env::var("SIGWIN_DEMO_VIEWER_COUNT").ok().and_then(|v| v.parse().ok()).unwrap_or(3)
}

/// Goes through the notices and connection states over and over, as a live backend would send them.
fn demo_status(emit: backend::Emit) {
    let wait = |secs: f32| std::thread::sleep(std::time::Duration::from_secs_f32(secs));
    std::thread::spawn(move || {
        let connection = |text: &str, at_once: bool| emit(Event::Connection { text: text.into(), at_once });
        loop {
            // start
            connection("동기화 중…", true);
            wait(3.0);
            connection("", true);
            wait(2.0);
            // a notice goes away by itself
            emit(Event::Notice("보낸 지 24시간이 지난 메시지는 수정할 수 없습니다".into()));
            wait(8.0);
            // progress stays until the work ends
            emit(Event::Progress("첨부파일 다운로드 중…".into()));
            wait(9.0);
            emit(Event::ProgressDone);
            wait(2.0);
            // a long failure, cut at two lines
            emit(Event::Notice(
                "다운로드 실패: error sending request for url (https://cdn3.signal.org/attachments/abcdefghijklmnopqrstuvwxyz0123456789): connection timed out while waiting for a response from the server".into(),
            ));
            wait(8.0);
            // a short drop: nothing shows
            connection("연결이 끊어졌습니다. 다시 연결 중…", false);
            wait(2.0);
            connection("동기화 중…", false);
            wait(1.0);
            connection("", true);
            wait(4.0);
            // a long drop: the banner comes after the delay, then follows the retries
            connection("연결이 끊어졌습니다. 다시 연결 중…", false);
            wait(9.0);
            connection("연결 실패. 2초 후 재시도", false);
            wait(4.0);
            connection("연결 실패. 4초 후 재시도", false);
            wait(4.0);
            connection("동기화 중…", false);
            wait(3.0);
            connection("", true);
            wait(4.0);
        }
    });
}

/// `StickerFrames` / `PickerFrames`: `demo-st-1` and every 4th picker sticker are animated GIFs
/// (a pulsing blob), decoded like real stickers.
fn demo_frames(emit: &backend::Emit, c: &Command) {
    if let Command::LinkPreview(Some(url)) = c {
        return demo_link_fetch(emit, url);
    }
    if let Command::ViewImage { id, index, fit, .. } = c {
        return demo_viewer(emit, id, *index, *fit);
    }
    if let Command::SaveViewed { .. } | Command::SaveFiles { .. } = c {
        return emit(Event::Saved("첨부 파일 1개를 저장했습니다.".into(), std::env::temp_dir().join("demo.pdf")));
    }
    if let Command::ViewerThumbs { id, px } = c {
        for index in 0..demo_viewer_count() {
            let t = image::RgbaImage::from_fn(*px, *px, |x, y| image::Rgba([(x * 255 / *px) as u8, (y * 255 / *px) as u8, (index * 60 % 256) as u8, 255]));
            let (width, height) = t.dimensions();
            emit(Event::ViewerThumb(id.clone(), index, types::Thumb { key: id.clone(), width, height, rgba: t.into_raw() }));
        }
        return;
    }
    let (key, fit) = match c {
        Command::StickerFrames(k) => (k.clone(), view::media::STICKER_SIZE),
        Command::PickerFrames { pack, id } => (format!("{}{pack}_{id}", types::STICKER_THUMB_PREFIX), 72),
        _ => return,
    };
    let animated = key == "demo-st-1" || key.rsplit('_').next().and_then(|i| i.parse::<u32>().ok()).is_some_and(|i| i % 4 == 0);
    if !animated {
        emit(Event::Frames(types::Frames::still(key)));
        return;
    }
    let frames = (0..8u32)
        .map(|f| {
            let r = 40.0 + 50.0 * (f as f32 / 8.0 * std::f32::consts::TAU).sin().abs();
            image::RgbaImage::from_fn(200, 200, |x, y| {
                let d = ((x as f32 - 100.0).powi(2) + (y as f32 - 100.0).powi(2)).sqrt();
                if d < r { image::Rgba([240, 120 + (f * 15) as u8, 40, 255]) } else { image::Rgba([0, 0, 0, 0]) }
            })
        })
        .collect();
    let emit = emit.clone();
    std::thread::spawn(move || {
        if let Ok(gif) = sigwin_thumb::gif(frames, 120) {
            emit(Event::Frames(types::Frames::decode(key, &gif, fit, types::ANIM_BUDGET / 2)));
        }
    });
}

/// SIGWIN_DEMO_VIDEO: the newest rows show videos: with a frame on the way, with a blurhash only,
/// with nothing, and in a grid with an image.
/// SIGWIN_DEMO_VIDEO's row with a file being saved
static DEMO_FILE_ROW: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn demo_video_rows(items: &mut [types::MsgDto]) {
    // above them, a file on its way to be saved
    let file = items.len() - 5;
    let f = &mut items[file];
    f.big_emoji = false;
    f.sticker = false;
    f.text.clear();
    f.cells.clear();
    f.image_key.clear();
    f.attachment = view::media::file_line("보고서_최종.pdf", 0, Some(12 * 1024 * 1024));
    let _ = DEMO_FILE_ROW.set(f.id.clone());
    let last = items.len() - 4;
    for (n, m) in items[last..].iter_mut().enumerate() {
        m.big_emoji = false;
        m.sticker = false;
        m.quote.clear();
        m.quote_author.clear();
        m.attachment.clear();
        m.text = if n == 1 { "동영상 설명이 함께 있는 메시지".into() } else { String::new() };
        if n == 3 {
            // a video, a picture, a video and a GIF, none of them here yet but one
            view::media::set_photo_grid(m, 4, |i, _, _| format!("demo-video-grid-{i}"));
            m.cells[0].video = true;
            m.cells[2].video = true;
            m.cells[3].gif = true;
        } else {
            view::media::set_single_photo(m, format!("demo-video-{n}"), if n == 1 { (300, 450) } else { (400, 225) });
            m.cells[0].video = true;
        }
    }
}

fn demo_videos(emit: &backend::Emit) {
    const HASH: &str = "LEHV6nWB2yk8pyo0adR*.7kCMdnj";
    let thumb = |key: String, img: image::RgbaImage| {
        emit(Event::Thumb(Arc::new(types::Thumb { key, width: img.width(), height: img.height(), rgba: img.into_raw() })));
    };
    let file = std::env::var_os("SIGWIN_DEMO_VIDEO").filter(|v| v != "1");
    let hash = file.as_ref().and_then(|path| demo_video_hash(std::path::Path::new(path))).unwrap_or_else(|| HASH.into());
    let blur = |w, h| sigwin_thumb::cover(&sigwin_thumb::blurhash(&hash, 32, 32).expect("valid"), types::physical_px(w), types::physical_px(h));
    std::thread::sleep(std::time::Duration::from_millis(300));
    thumb("demo-video-0".into(), blur(400, 225));
    thumb("demo-video-1".into(), blur(300, 450));
    for (i, (w, h)) in [(150, 150); 4].into_iter().enumerate() {
        thumb(format!("demo-video-grid-{i}"), blur(w, h));
    }
    let label = |key: &str, text: &str, undownloaded| {
        emit(Event::MediaLabel { key: key.into(), label: types::MediaLabel { text: text.into(), undownloaded, progress: None } });
    };
    let downloading = |key: &str, text: &str, percent| {
        emit(Event::MediaLabel { key: key.into(), label: types::MediaLabel { text: text.into(), undownloaded: true, progress: Some(percent) } });
    };
    label("demo-video-0", "12MB", true);
    downloading("demo-video-1", "90MB / 245MB", 37);
    label("demo-video-2", "1.2GB", true);
    downloading("demo-video-grid-0", "38MB / 38MB", 100);
    label("demo-video-grid-1", "2.4MB", true);
    downloading("demo-video-grid-2", "123MB / 1.2GB", 0);
    label("demo-video-grid-3", "3.1MB", true);
    if let Some(id) = DEMO_FILE_ROW.get() {
        downloading(&types::file_line_key(id), "4.2MB / 12MB", 35);
    }
    // the first frame replaces the blurhash
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let (w, h) = (types::physical_px(400), types::physical_px(225));
    let (frame, seconds) = match file.and_then(|path| demo_video_frame(std::path::Path::new(&path))) {
        Some((f, seconds)) => (sigwin_thumb::cover(&f, w, h), seconds),
        None => (image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 120, 255])), 75),
    };
    thumb("demo-video-0".into(), frame);
    label("demo-video-0", &view::media::length_text(seconds), false);
}

/// A real video's frame, by the way real ones go: encrypted on disk, through the decoding child.
fn demo_video_frame(path: &std::path::Path) -> Option<(image::RgbaImage, u32)> {
    sigwin_vault::init([9; 32]);
    let sealed = std::env::temp_dir().join(format!("sigwin-demo-video-{}.enc", std::process::id()));
    sigwin_vault::write(&sealed, &std::fs::read(path).ok()?).ok()?;
    let started = std::time::Instant::now();
    let decoded = crate::videoframe::first_frame(&sealed);
    let shown = decoded.as_ref().map(|d| d.as_ref().map(|d| (d.frame.dimensions(), d.seconds))).map_err(|e| e.to_string());
    tracing::info!("demo video frame and seconds: {shown:?} in {:?}", started.elapsed());
    let _ = std::fs::remove_file(&sealed);
    let d = decoded.ok().flatten()?;
    Some((d.frame, d.seconds))
}

/// The blurhash a real video would be sent with, by the way a file being sent goes.
fn demo_video_hash(path: &std::path::Path) -> Option<String> {
    let v = crate::videoframe::first_frame_plain(path).ok().flatten()?;
    tracing::info!("demo video to send: size {:?}", v.size);
    v.blurhash()
}

/// SIGWIN_DEMO_GRID: images of the newest rows.
const DEMO_GRIDS: [usize; 5] = [2, 3, 4, 5, 8];

fn demo_grids(emit: &backend::Emit) {
    for count in DEMO_GRIDS {
        let mut row = types::MsgDto::default();
        let mut cells = Vec::new();
        view::media::set_photo_grid(&mut row, count, |n, w, h| {
            cells.push((n, w, h));
            format!("demo-grid-{count}-{n}")
        });
        // the last one of 4 never arrives: how a cell looks until then
        for (n, w, h) in cells.into_iter().filter(|c| count != 4 || c.0 != 3) {
            let (pw, ph) = (types::physical_px(w), types::physical_px(h));
            let img = image::RgbaImage::from_fn(pw, ph, |x, y| image::Rgba([(x * 255 / pw) as u8, (y * 255 / ph) as u8, (n * 50) as u8, 255]));
            let key = format!("demo-grid-{count}-{n}");
            emit(Event::Thumb(Arc::new(types::Thumb { key, width: pw, height: ph, rgba: img.into_raw() })));
        }
    }
}

pub(crate) fn demo_stickers(emit: backend::Emit) {
    for i in 0..2u8 {
        emit(Event::Thumb(Arc::new(demo_blob(&format!("demo-st-{i}"), 200, 60 + i * 120))));
    }
    let packs: Vec<types::PackDto> = (0..3)
        .map(|i| types::PackDto {
            id: format!("p{i}"),
            title: format!("추천 팩 {i}"),
            installed: i < 2 && std::env::var("SIGWIN_DEMO_STICKERS").as_deref() != Ok("add"),
            cover_key: format!("sp_p{i}_0"),
        })
        .collect();
    let items: Vec<types::StickerDto> =
        (0..20).map(|i| types::StickerDto { key: format!("sp_p0_{i}"), id: i, emoji: String::new(), animated: i % 4 == 0 }).collect();
    // the panel is opened by main(); give it a moment before filling it
    slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
        emit(Event::StickerPacks(packs));
        if std::env::var("SIGWIN_DEMO_STICKERS").as_deref() != Ok("add") {
            emit(Event::StickerGrid { pack: "p0".into(), items });
        }
        let mut thumbs: Vec<_> = (0..20u32).map(|i| demo_blob(&format!("sp_p0_{i}"), 96, (i * 12) as u8)).collect();
        thumbs.push(demo_blob("sp_p1_0", 96, 200));
        emit(Event::StickerThumbs(thumbs));
    });
}

/// `SIGWIN_DEMO_REAL=1`: a believable chat list and conversation (for README screenshots).
fn demo_real(emit: backend::Emit, mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>) {
    let th = |i: usize, name: &str, snippet: &str, time: &str, unread: u32, is_group: bool| ThreadDto {
        key: format!("c:demo-{i}"),
        name: name.into(),
        snippet: snippet.into(),
        time: time.into(),
        ts: 0,
        unread,
        is_group,
        mute: Mute::default(),
        prefs: ThreadPrefs::default(),
        phone_mentions: None,
        readers: Vec::new(),
        readers_more: 0,
    };
    let threads = vec![
        th(1, "이서연", "그럼 토요일 11시에 합정역에서 봐요!", "오후 6:42", 0, false),
        th(2, "가족 단톡방", "엄마: 이번 주말에 다들 올 수 있지?", "오후 5:58", 2, true),
        th(3, "김민준", "📷 IMG_2041.jpg", "오후 2:10", 0, false),
        th(4, "프로젝트 팀", "박지훈: 리뷰 올렸습니다", "오전 11:30", 0, true),
        th(5, "박지훈", "📄 unlockfps_nc_signed.exe", "어제", 0, false),
        th(6, "최유나", "고마워요 :)", "어제", 0, false),
        th(7, "독서 모임", "다음 책은 뭐로 할까요?", "11월 1일", 0, true),
        th(8, "Alice Kim", "See you next week!", "10월 31일", 0, false),
        th(9, "권준영", "ㅋㅋㅋㅋ 인정", "10월 29일", 0, false),
    ];
    let chat: [(bool, &str, &str); 14] = [
        (false, "주말에 시간 돼요?", "오후 6:20"),
        (true, "네 토요일 괜찮아요! 뭐 할까요?", "오후 6:21"),
        (false, "전에 말했던 전시 보러 가요. 합정에 새로 생긴 갤러리인데 평이 좋더라고요", "오후 6:23"),
        (true, "좋아요 👍", "오후 6:25"),
        (true, "점심 먹고 갈까요, 아니면 보고 나서 먹을까요?", "오후 6:25"),
        (false, "보고 나서 먹어요. 근처에 맛있는 파스타집 봐 뒀어요 🍝", "오후 6:31"),
        (true, "완벽하네요", "오후 6:32"),
        (false, "그럼 토요일 11시에 합정역에서 봐요!", "오후 6:42"),
        (true, "네 그때 봬요 😊", "오후 6:42"),
        (false, "참, 우산 챙겨요. 토요일에 비 온대요", "오후 6:44"),
        (true, "헉 알려줘서 고마워요 ☔", "오후 6:45"),
        (false, "ㅋㅋ 별말씀을", "오후 6:45"),
        (true, "혹시 늦으면 연락할게요", "오후 6:46"),
        (false, "넵 천천히 와요~", "오후 6:46"),
    ];
    let base = 1_762_000_000_000u64;
    let mut items: Vec<MsgDto> = chat
        .iter()
        .enumerate()
        .map(|(i, &(outgoing, text, time))| MsgDto {
            id: i.to_string(),
            ts: base + i as u64 * 60_000,
            sender: if outgoing { String::new() } else { "이서연".into() },
            sender_key: if outgoing { String::new() } else { "demo-a".into() },
            text: text.into(),
            time: time.into(),
            outgoing,
            status: if outgoing { "✓✓".into() } else { String::new() },
            can_edit: outgoing,
            can_delete: outgoing,
            day_sep: i == 0,
            day: crate::view::time::day_num(&chrono::Local::now()) - 2,
            ..Default::default()
        })
        .collect();
    items[3].reactions = vec![("❤️".into(), 1, "이서연".into())];
    items[10].reactions = vec![("😂".into(), 1, "박지훈".into())];
    for m in items.iter_mut() {
        m.big_emoji = emoji::only_emoji(&m.text, 5).is_some();
    }
    view::mark_runs(&mut items, false);
    emit(Event::Ready);
    emit(Event::Threads(threads));
    emit(Event::Header { name: "이서연".into(), sub: "+82 10-0000-0000".into() });
    emit(Event::Messages { key: "c:demo-1".into(), items: items.clone(), paging: Paging::default(), scroll: Scroll::Bottom });
    emit(Event::Pins(Vec::new()));
    emit(Event::Account { name: "홍길동".into(), number: "+821012345678".into(), device: "Sigwin".into(), receipts: Some(true), typing: None });
    std::thread::spawn(move || {
        while let Some(c) = cmd_rx.blocking_recv() {
            if let Command::OpenThread(key) = c {
                emit(Event::Messages { key, items: items.clone(), paging: Paging::default(), scroll: Scroll::Bottom });
                emit(Event::Pins(Vec::new()));
            }
        }
    });
}
