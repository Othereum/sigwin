//! Demo modes: a fake backend with synthetic data (`SIGWIN_DEMO`), and the memory self-test.

use super::*;
use crate::types::{MsgDto, Scroll, ThreadDto};

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
                app.set_status_text(format!("tick {}", n.get()).into());
            }
        });
    }
    if std::env::var_os("SIGWIN_DEMO_INFO").is_some() {
        // "new messages below" badge, set late so a test can scroll up first
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(4000), move || {
            if let Some(app) = weak.upgrade() {
                app.set_new_below(3);
            }
        });
        let weak = app.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
            if let Some(app) = weak.upgrade() {
                app.set_reply_preview("이서연: 답장 미리보기".into());
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
            if what == "edit" {
                app.invoke_edit_message("294".into());
            } else if what == "pin" {
                app.invoke_open_menu("297".into(), 520.0, 300.0);
                app.set_menu_pin_step(true);
            } else {
                app.invoke_open_menu("297".into(), 520.0, 300.0);
            }
        });
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
    let threads: Vec<ThreadDto> = (0..40)
        .map(|i| ThreadDto {
            key: format!("c:demo-{i}"),
            name: ["김민준", "이서연", "박지훈", "가족 단톡방", "Alice", "프로젝트 팀"][i % 6].to_string(),
            snippet: "내일 몇 시에 만날까? 저녁 7시쯤 어때요".into(),
            time: if i < 3 { "오후 3:12".into() } else { format!("10월 {}일", 30 - (i % 28)) },
            ts: 0,
            unread: if i % 5 == 0 { (i % 7) as u32 + 1 } else { 0 },
            is_group: i % 6 == 3 || i % 6 == 5,
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
            reactions: if i % 9 == 0 { vec![("❤️".into(), 2), ("👍".into(), 1), ("😂".into(), 3)] } else { Vec::new() },
            attachment: if i % 13 == 0 { "보고서.pdf".into() } else { String::new() },
            day_sep: if i % 50 == 0 { "2026년 11월 1일 일요일".into() } else { String::new() },
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
        st("굵게", view::STYLE_BOLD);
        st("기울임", view::STYLE_ITALIC);
        st("취소선", view::STYLE_STRIKE);
        st("스포일러 내용", view::STYLE_SPOILER);
        st("mono(iiWW)", view::STYLE_MONO);
    }
    emit(Event::Ready);
    emit(Event::Threads(threads));
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
    emit(Event::Header { name: "이서연".into(), sub: "+82 10-0000-0000".into() });
    let names = ["김민준", "이서연", "박지훈", "Alice Kim", "Bob", "김지원", "최유나"];
    emit(Event::Members(names.iter().enumerate().map(|(i, n)| (format!("00000000-0000-0000-0000-00000000000{i}"), n.to_string())).collect()));
    for m in items.iter_mut().filter(|m| m.attachment.is_empty() && m.quote.is_empty() && !m.sticker) {
        m.big_emoji = emoji::only_emoji(&m.text, 5).is_some();
    }
    for i in [150usize, 290] {
        items[i].pinned = true;
    }
    view::mark_runs(&mut items, true);
    let pins = [(290usize, "이서연"), (150, "나")]
        .iter()
        .map(|&(i, who)| types::PinDto { id: i.to_string(), sender: who.into(), text: items[i].text.replace('\n', " ") })
        .collect();
    let reload = std::env::var_os("SIGWIN_DEMO_RELOAD").is_some();
    if reload {
        // like a real account: photos make rows very uneven in height
        for (i, m) in items.iter_mut().enumerate().filter(|(i, _)| i % 9 == 4 && *i < 290) {
            m.image_key = format!("demo-photo-{i}");
            (m.img_w, m.img_h) = if i % 2 == 0 { (400, 300) } else { (300, 400) };
        }
    }
    if let Ok(v) = std::env::var("SIGWIN_DEMO_OLDER") {
        // history paged in from the top like a real thread: 60 rows, then 60 more per LoadOlder,
        // each after `v` ms (default 150)
        let delay = std::time::Duration::from_millis(v.parse().unwrap_or(150));
        let mut start = items.len() - 60;
        // like view::build: the first row of the window always starts a day
        let all = items.clone();
        let window = move |start: usize| {
            let mut rows = all[start..].to_vec();
            if rows[0].day_sep.is_empty() {
                rows[0].day_sep = "2026년 10월 31일 토요일".into();
            }
            view::mark_runs(&mut rows, true);
            rows
        };
        emit(Event::Messages { key: "c:demo-1".into(), items: window(start), can_older: true, scroll: Scroll::Bottom });
        let emit = emit.clone();
        std::thread::spawn(move || {
            while let Some(c) = cmd_rx.blocking_recv() {
                demo_frames(&emit, &c);
                if matches!(c, Command::LoadOlder) && start > 0 {
                    std::thread::sleep(delay);
                    start = start.saturating_sub(60);
                    emit(Event::Messages { key: "c:demo-1".into(), items: window(start), can_older: start > 0, scroll: Scroll::Older });
                }
            }
        });
    } else {
        emit(Event::Messages { key: "c:demo-1".into(), items: items.clone(), can_older: false, scroll: Scroll::Bottom });
        // clicking another thread shows the same rows under its key (drafts follow the key)
        let (emit, items) = (emit.clone(), items.clone());
        std::thread::spawn(move || {
            while let Some(c) = cmd_rx.blocking_recv() {
                demo_frames(&emit, &c);
                if let Command::OpenThread(key) = c {
                    emit(Event::Messages { key, items: items.clone(), can_older: false, scroll: Scroll::Bottom });
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
                can_older: false,
                scroll: Scroll::KeepIfNotAtBottom,
            });
        });
    }
    // one sender with a photo, the other falls back to the initial
    photo(format!("{}demo-a", avatar::MSG_PREFIX), 1, avatar::MSG_SIZE);
    photo(format!("{}demo-a", avatar::READ_PREFIX), 1, avatar::READ_SIZE);
    emit(Event::Status("데모 모드".into()));
    emit(Event::Account {
        name: "홍길동".into(),
        number: "+821012345678".into(),
        device: "Sigwin".into(),
        receipts: Some(true),
        typing: None,
    });
    emit(Event::Storage { db: 23 * 1024 * 1024, attachments: 148 * 1024 * 1024, thumbs: 3_400_000 });
    if std::env::var_os("SIGWIN_DEMO_STICKERS").is_some() {
        demo_stickers(emit);
    }
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

/// `StickerFrames` / `PickerFrames`: `demo-st-1` and every 4th picker sticker are animated GIFs
/// (a pulsing blob), decoded like real stickers.
fn demo_frames(emit: &backend::Emit, c: &Command) {
    let (key, fit) = match c {
        Command::StickerFrames(k) => (k.clone(), view::STICKER_SIZE),
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
