#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alloc;
mod backend;
mod clip;
mod picker;
mod db;
mod emoji;
mod headless;
mod link;
mod mention;
mod secret;
mod types;
mod view;
mod winhook;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use slint::{Color, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use types::{Command, Event, MsgDto, Scroll, ThreadDto};

slint::include_modules!();

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

const PALETTE: [u32; 12] = [
    0xc73f00, 0xf6475d, 0xa23474, 0x8f2af4, 0x5b6ce6, 0x3a77c9, 0x0a7b83, 0x197a3d, 0x6b7b14,
    0x8a5c00, 0x9e3b8f, 0x4f6f8f,
];

fn tint(key: &str) -> Color {
    let h = key.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    let c = PALETTE[(h as usize) % PALETTE.len()];
    Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn initial(name: &str) -> String {
    name.chars().find(|c| !c.is_whitespace()).map(|c| c.to_uppercase().to_string()).unwrap_or_default()
}

/// UI-thread state that isn't a Slint property.
#[derive(Default)]
struct Ui {
    threads: Rc<VecModel<ThreadItem>>,
    messages: Rc<VecModel<MsgItem>>,
    /// thumbnails for the open thread only; cleared on thread switch to bound memory
    thumbs: HashMap<String, Image>,
    last: Vec<MsgDto>,
    files: Vec<PathBuf>,
    reply_to: Option<String>,
    /// id of our message being edited in the composer
    editing: Option<String>,
    /// where pasted clipboard images are written
    paste_dir: PathBuf,
    open_key: String,
    loading_older: bool,
    picking: bool,
    packs: Rc<VecModel<PackItem>>,
    pack_dtos: Vec<types::PackDto>,
    stickers: Rc<VecModel<StickerItem>>,
    grid: Vec<types::StickerDto>,
    /// picker thumbnails; dropped when the panel closes
    sticker_thumbs: HashMap<String, Image>,
    tx: Option<UnboundedSender<Command>>,
    /// who can be @mentioned in the open thread
    members: Vec<mention::Member>,
    /// rows of the open mention popup
    mention_rows: Vec<mention::Member>,
    /// members picked in the composer since it was last sent
    picked: Vec<mention::Member>,
}

fn send_cmd(c: Command) {
    UI.with_borrow(|ui| {
        if let Some(tx) = &ui.tx {
            let _ = tx.send(c);
        }
    });
}

fn pack_item(p: &types::PackDto, thumbs: &HashMap<String, Image>) -> PackItem {
    PackItem {
        id: p.id.as_str().into(),
        title: p.title.as_str().into(),
        installed: p.installed,
        cover: thumbs.get(&p.cover_key).cloned().unwrap_or_default(),
    }
}

fn close_stickers(app: &AppWindow) {
    app.set_sticker_open(false);
    UI.with_borrow_mut(|ui| {
        ui.sticker_thumbs.clear();
        ui.grid.clear();
        ui.stickers.set_vec(Vec::new());
        ui.packs.set_vec(Vec::new());
    });
}

/// Scroll the message list to the bottom and keep it there until its height settles.
/// The ListView estimates unseen rows from the average row height; rows near the bottom
/// only get their real (often taller: quotes, images) height once instantiated, which
/// grows `content-height` after the first scroll.
fn stick_to_bottom(app: &AppWindow) {
    thread_local! {
        static TIMER: slint::Timer = slint::Timer::default();
    }
    let weak = app.as_weak();
    let mut last_h = -1.0f32;
    let mut stable = 0u32;
    let mut ticks = 0u32;
    TIMER.with(|t| {
        t.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(16), move || {
            let Some(app) = weak.upgrade() else { return };
            app.invoke_scroll_to_bottom();
            let h = app.invoke_content_height();
            ticks += 1;
            stable = if h == last_h { stable + 1 } else { 0 };
            last_h = h;
            if stable >= 3 || ticks >= 60 {
                TIMER.with(|t| t.stop());
            }
        });
    });
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::default();
}

/// Message text with the @mentions colored and links underlined. The styled text is drawn
/// over a TextInput whose own glyphs are transparent, so selection keeps working; only
/// colors and underlines change, never the font, so both lay out identically.
fn rich_text(m: &MsgDto, links: &[(usize, usize)], dark: bool) -> slint::StyledText {
    use slint::private_unstable_api::re_exports::parse_markdown;
    // link color comes from the StyledText's `link-color`
    let mention = match (m.outgoing, dark) {
        (true, _) => "#ffe27a",
        (false, true) => "#7aa7ff",
        (false, false) => "#1f5fd6",
    };
    let mut spans: Vec<(usize, usize, bool)> = m.mentions.iter().map(|&(s, e, _)| (s, e, false)).collect();
    spans.extend(links.iter().map(|&(s, e)| (s, e, true)));
    spans.sort_unstable();
    // placeholders are filled with plain text verbatim, so user text is never parsed as markdown
    const P: char = '\u{e541}';
    let mut fmt = String::from(P);
    let mut args = Vec::with_capacity(spans.len() * 2 + 1);
    let mut at = 0;
    for (s, e, is_link) in spans {
        let (Some(before), Some(part)) = (m.text.get(at..s), m.text.get(s..e)) else { continue };
        args.push(slint::StyledText::from_plain_text(before));
        args.push(slint::StyledText::from_plain_text(part));
        if is_link {
            // a markdown link: the StyledText underlines it and reports clicks with the URL
            args.push(slint::StyledText::from_plain_text(&link::target(part)));
            fmt.push_str(&format!("[{P}]({P}){P}"));
        } else {
            fmt.push_str(&format!("<font color=\"{mention}\">{P}</font>{P}"));
        }
        at = e;
    }
    args.push(slint::StyledText::from_plain_text(&m.text[at..]));
    parse_markdown(&fmt, &args)
}

fn to_item(m: &MsgDto, thumbs: &HashMap<String, Image>, dark: bool) -> MsgItem {
    let image = thumbs.get(&m.image_key).cloned();
    let reactions: Vec<ReactionItem> = m
        .reactions
        .iter()
        .map(|(e, n)| ReactionItem { emoji: e.as_str().into(), count: *n as i32 })
        .collect();
    let links = if m.deleted { Vec::new() } else { link::find(&m.text) };
    let has_rich = !m.mentions.is_empty() || !links.is_empty();
    MsgItem {
        reactions: ModelRc::new(VecModel::from(reactions)),
        big_emoji: m.big_emoji,
        id: m.id.as_str().into(),
        sender: m.sender.as_str().into(),
        sender_color: tint(&m.sender_key),
        text: m.text.as_str().into(),
        has_rich,
        rich: if has_rich { rich_text(m, &links, dark) } else { Default::default() },
        time: m.time.as_str().into(),
        outgoing: m.outgoing,
        show_sender: m.show_sender,
        status: m.status.as_str().into(),
        quote: m.quote.as_str().into(),
        attachment: m.attachment.as_str().into(),
        has_image: !m.image_key.is_empty(),
        image: image.unwrap_or_default(),
        img_w: m.img_w as f32,
        img_h: m.img_h as f32,
        deleted: m.deleted,
        day_sep: m.day_sep.as_str().into(),
        system: m.system,
        sticker: m.sticker,
        receipt_info: m.receipt_info.as_str().into(),
        pending: m.pending,
    }
}

fn thread_item(t: &ThreadDto) -> ThreadItem {
    ThreadItem {
        key: t.key.as_str().into(),
        name: t.name.as_str().into(),
        snippet: t.snippet.as_str().into(),
        time: t.time.as_str().into(),
        unread: t.unread as i32,
        initial: if t.is_group && t.name.is_empty() { "#".into() } else { initial(&t.name).into() },
        color: tint(&t.key),
        is_group: t.is_group,
    }
}

fn qr_image(url: &str) -> Option<Image> {
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    let w = code.width();
    let quiet = 2;
    let size = (w + quiet * 2) as u32;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
    let px = buf.make_mut_slice();
    for p in px.iter_mut() {
        *p = Rgba8Pixel { r: 255, g: 255, b: 255, a: 255 };
    }
    for y in 0..w {
        for x in 0..w {
            if code[(x, y)] == qrcode::Color::Dark {
                px[(y + quiet) * size as usize + (x + quiet)] = Rgba8Pixel { r: 0, g: 0, b: 0, a: 255 };
            }
        }
    }
    Some(Image::from_rgba8(buf))
}

fn apply(app: &AppWindow, ev: Event) {
    match ev {
        Event::Loading(t) => {
            app.set_loading_text(t.into());
            app.set_page("loading".into());
        }
        Event::NeedLink => {
            app.set_has_qr(false);
            app.set_page("link".into());
        }
        Event::LinkQr(url) => {
            if let Some(img) = qr_image(&url) {
                app.set_qr(img);
                app.set_has_qr(true);
            }
        }
        Event::LinkStatus(s) => app.set_link_status(s.into()),
        Event::Ready => app.set_page("main".into()),
        Event::Threads(list) => UI.with_borrow(|ui| {
            app.set_total_unread(list.iter().map(|t| t.unread as i32).sum());
            ui.threads.set_vec(list.iter().map(thread_item).collect::<Vec<_>>());
        }),
        Event::Header { name, sub } => {
            app.set_current_name(name.into());
            app.set_current_sub(sub.into());
        }
        Event::Members(list) => UI.with_borrow_mut(|ui| ui.members = list),
        Event::Messages { key, items, can_older, scroll } => {
            let switched = UI.with_borrow_mut(|ui| {
                let switched = ui.open_key != key;
                if switched {
                    ui.thumbs.clear();
                    ui.open_key = key.clone();
                    ui.reply_to = None;
                    ui.picked.clear();
                    ui.mention_rows.clear();
                    if ui.editing.take().is_some() {
                        app.set_draft(SharedString::new());
                    }
                }
                ui.loading_older = false;
                switched
            });
            if switched {
                app.set_reply_preview(SharedString::new());
                app.set_edit_preview(SharedString::new());
                app.set_menu_id(SharedString::new());
                app.set_mention_items(ModelRc::default());
                close_stickers(app);
            }
            app.set_current_key(key.into());
            let near_bottom = app.get_near_bottom();
            let before_h = app.invoke_content_height();
            UI.with_borrow_mut(|ui| {
                if switched {
                    app.set_new_below(0);
                } else if !near_bottom {
                    // count incoming rows that weren't shown before
                    let seen: std::collections::HashSet<&str> = ui.last.iter().map(|m| m.id.as_str()).collect();
                    let fresh = items.iter().filter(|m| !m.outgoing && !m.system && !seen.contains(m.id.as_str())).count();
                    if matches!(scroll, Scroll::KeepIfNotAtBottom) && fresh > 0 {
                        app.set_new_below(app.get_new_below() + fresh as i32);
                    }
                }
                let dark = app.global::<Theme>().get_dark();
                ui.messages.set_vec(items.iter().map(|m| to_item(m, &ui.thumbs, dark)).collect::<Vec<_>>());
                ui.last = items;
            });
            app.set_can_load_older(can_older);
            match scroll {
                Scroll::Bottom => stick_to_bottom(app),
                Scroll::KeepIfNotAtBottom if near_bottom => stick_to_bottom(app),
                Scroll::KeepIfNotAtBottom => {}
                Scroll::Older => {
                    let weak = app.as_weak();
                    // let the ListView lay out the new rows before measuring
                    slint::Timer::single_shot(std::time::Duration::from_millis(16), move || {
                        let Some(app) = weak.upgrade() else { return };
                        let grown = app.invoke_content_height() - before_h;
                        app.invoke_shift_by(grown);
                    });
                }
            }
            if switched {
                app.invoke_focus_composer();
            }
        }
        Event::Thumb(t) => {
            let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height);
            let img = Image::from_rgba8(buf);
            if t.key.starts_with(types::STICKER_THUMB_PREFIX) {
                if !app.get_sticker_open() {
                    return;
                }
                UI.with_borrow_mut(|ui| {
                    ui.sticker_thumbs.insert(t.key.clone(), img.clone());
                    for (i, p) in ui.pack_dtos.iter().enumerate() {
                        if p.cover_key == t.key {
                            slint::Model::set_row_data(&*ui.packs, i, pack_item(p, &ui.sticker_thumbs));
                        }
                    }
                    for (i, s) in ui.grid.iter().enumerate() {
                        if s.key == t.key {
                            if let Some(mut row) = slint::Model::row_data(&*ui.stickers, i) {
                                row.img = img.clone();
                                slint::Model::set_row_data(&*ui.stickers, i, row);
                            }
                        }
                    }
                });
                return;
            }
            UI.with_borrow_mut(|ui| {
                ui.thumbs.insert(t.key.clone(), img.clone());
                for (i, m) in ui.last.iter().enumerate() {
                    if m.image_key == t.key {
                        if let Some(mut row) = slint::Model::row_data(&*ui.messages, i) {
                            row.image = img.clone();
                            slint::Model::set_row_data(&*ui.messages, i, row);
                        }
                    }
                }
            });
        }
        Event::Status(s) => app.set_status_text(s.into()),
        Event::Typing(t) => app.set_typing_text(t.into()),
        Event::StickerPacks(list) => {
            if !app.get_sticker_open() {
                return;
            }
            let first = UI.with_borrow_mut(|ui| {
                ui.packs.set_vec(list.iter().map(|p| pack_item(p, &ui.sticker_thumbs)).collect::<Vec<_>>());
                let first = list.iter().find(|p| p.installed).map(|p| p.id.clone());
                ui.pack_dtos = list;
                first
            });
            let cur = app.get_current_pack().to_string();
            let still_there = UI.with_borrow(|ui| ui.pack_dtos.iter().any(|p| p.installed && p.id == cur));
            // the grid is emptied when the panel closes, so always (re)load the selected pack
            let target = if still_there { Some(cur) } else { first };
            match target {
                Some(id) => {
                    app.set_current_pack(id.as_str().into());
                    send_cmd(Command::OpenPack(id));
                }
                None => app.set_current_pack("+".into()),
            }
        }
        Event::OpenStickerPanel(pack) => {
            app.set_current_pack(pack.into());
            app.set_sticker_open(true);
            // reloads the pack list, then opens the current pack
            send_cmd(Command::StickerPanel);
        }
        Event::AskInstallPack(msg_id) => {
            let ok = rfd::MessageDialog::new()
                .set_title("스티커 팩")
                .set_description("이 스티커가 들어 있는 팩을 추가할까요?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            if ok == rfd::MessageDialogResult::Yes {
                send_cmd(Command::InstallPackFromMessage(msg_id));
            }
        }
        Event::StickerGrid { pack, items } => {
            if !app.get_sticker_open() {
                return;
            }
            app.set_current_pack(pack.into());
            UI.with_borrow_mut(|ui| {
                let rows: Vec<StickerItem> = items
                    .iter()
                    .map(|s| StickerItem {
                        id: s.id as i32,
                        emoji: s.emoji.as_str().into(),
                        img: ui.sticker_thumbs.get(&s.key).cloned().unwrap_or_default(),
                    })
                    .collect();
                ui.stickers.set_vec(rows);
                ui.grid = items;
            });
        }
    }
}

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LITE_SIGNAL_DATA") {
        return PathBuf::from(d);
    }
    directories::ProjectDirs::from("dev", "lite-signal", "lite-signal")
        .map(|p| p.data_local_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("lite-signal-data"))
}

fn main() -> anyhow::Result<()> {
    if picker::run_if_child() {
        return Ok(());
    }
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env_lossy()
        .add_directive("libsignal=warn".parse()?)
        .add_directive("presage=warn".parse()?)
        .add_directive("sqlx=error".parse()?);
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    headless::install_if_requested()?;

    let app = AppWindow::new()?;
    if headless::active() {
        app.invoke_set_dark(headless::dark());
    }
    UI.with_borrow(|ui| {
        app.set_threads(ModelRc::from(ui.threads.clone()));
        app.set_messages(ModelRc::from(ui.messages.clone()));
        app.set_packs(ModelRc::from(ui.packs.clone()));
        app.set_stickers(ModelRc::from(ui.stickers.clone()));
    });

    let (cmd_tx, cmd_rx) = unbounded_channel::<Command>();
    let weak = app.as_weak();
    let emit: backend::Emit = Arc::new(move |ev: Event| {
        let _ = weak.upgrade_in_event_loop(move |app| apply(&app, ev));
    });
    if std::env::var_os("LITE_SIGNAL_DEMO").is_some() {
        demo(emit);
        if std::env::var_os("LITE_SIGNAL_DEMO_TICK").is_some() {
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
        if std::env::var_os("LITE_SIGNAL_DEMO_INFO").is_some() {
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
        if let Ok(what) = std::env::var("LITE_SIGNAL_DEMO_MENU") {
            // message menu on an outgoing row, typing indicator, edit banner
            let weak = app.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
                let Some(app) = weak.upgrade() else { return };
                app.set_typing_text("입력 중…".into());
                if what == "edit" {
                    app.invoke_edit_message("294".into());
                } else {
                    app.invoke_open_menu("297".into(), 520.0, 300.0);
                }
            });
        }
        if std::env::var_os("LITE_SIGNAL_DEMO_STICKERS").is_some() {
            let weak = app.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
                if let Some(app) = weak.upgrade() {
                    app.set_sticker_open(true);
                }
            });
        }
        if std::env::var_os("LITE_SIGNAL_SELFTEST").is_some() {
            selftest();
        }
    } else {
        let _backend = backend::spawn(data_dir(), cmd_rx, emit);
    }

    let paste_dir = data_dir().join("paste");
    clip::clean(&paste_dir);
    UI.with_borrow_mut(|ui| {
        ui.tx = Some(cmd_tx.clone());
        ui.paste_dir = paste_dir;
    });
    wire(&app, cmd_tx);
    app.run()?;
    Ok(())
}

fn wire(app: &AppWindow, tx: UnboundedSender<Command>) {
    let send = move |c: Command| {
        let _ = tx.send(c);
    };

    let s = send.clone();
    app.on_start_link(move |name| s(Command::StartLink(name.to_string())));

    let s = send.clone();
    app.on_open_thread(move |key| s(Command::OpenThread(key.to_string())));

    let weak = app.as_weak();
    app.on_jump_to_bottom(move || {
        if let Some(app) = weak.upgrade() {
            app.set_new_below(0);
            stick_to_bottom(&app);
        }
    });

    let s = send.clone();
    app.on_load_older(move || {
        let go = UI.with_borrow_mut(|ui| !std::mem::replace(&mut ui.loading_older, true));
        if go {
            s(Command::LoadOlder);
        }
    });

    let s = send.clone();
    let weak = app.as_weak();
    app.on_send(move |text| {
        let Some(app) = weak.upgrade() else { return };
        if let Some(id) = UI.with_borrow_mut(|ui| ui.editing.take()) {
            if text.trim().is_empty() {
                UI.with_borrow_mut(|ui| ui.editing = Some(id));
                return;
            }
            let mentions = UI.with_borrow_mut(|ui| std::mem::take(&mut ui.picked));
            s(Command::Edit { id, text: text.to_string(), mentions });
            app.set_mention_items(ModelRc::default());
            app.set_draft(SharedString::new());
            app.set_edit_preview(SharedString::new());
            return;
        }
        let (files, quote) = UI.with_borrow_mut(|ui| (std::mem::take(&mut ui.files), ui.reply_to.take()));
        if text.trim().is_empty() && files.is_empty() {
            return;
        }
        let mentions = UI.with_borrow_mut(|ui| std::mem::take(&mut ui.picked));
        s(Command::Send { text: text.to_string(), files, quote, mentions });
        app.set_mention_items(ModelRc::default());
        app.set_draft(SharedString::new());
        app.set_pending_files(ModelRc::default());
        app.set_reply_preview(SharedString::new());
    });

    let weak = app.as_weak();
    app.on_attach(move || {
        let Some(app) = weak.upgrade() else { return };
        if UI.with_borrow_mut(|ui| std::mem::replace(&mut ui.picking, true)) {
            return;
        }
        let weak = app.as_weak();
        picker::spawn(hwnd(&app), move |picked| {
            let _ = slint::invoke_from_event_loop(move || {
                UI.with_borrow_mut(|ui| ui.picking = false);
                if let Some(app) = weak.upgrade() {
                    add_files(&app, picked);
                }
            });
        });
    });

    let weak = app.as_weak();
    app.on_paste_files(move || {
        let Some(app) = weak.upgrade() else { return false };
        match clip::probe() {
            clip::Probe::Other => false,
            clip::Probe::Files => {
                add_files(&app, clip::files());
                true
            }
            clip::Probe::Image => {
                let dir = UI.with_borrow(|ui| ui.paste_dir.clone());
                let weak = app.as_weak();
                app.set_status_text("이미지 붙여넣는 중…".into());
                // encoding a large screenshot takes a moment: keep the UI responsive
                std::thread::spawn(move || {
                    let res = clip::save_image(&dir);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        match res {
                            Ok(p) => {
                                app.set_status_text(SharedString::new());
                                add_files(&app, vec![p]);
                            }
                            Err(e) => app.set_status_text(format!("붙여넣기 실패: {e:#}").into()),
                        }
                    });
                });
                true
            }
        }
    });

    // @mention popup: follow the "@query" before the cursor
    let weak = app.as_weak();
    app.on_mention_check(move |text, cursor, preedit| {
        let Some(app) = weak.upgrade() else { return };
        let rows: Vec<mention::Member> = UI.with_borrow(|ui| {
            if ui.members.is_empty() {
                return Vec::new();
            }
            let Some((_, query)) = mention::query_at(&text, cursor.max(0) as usize) else { return Vec::new() };
            let query = format!("{query}{preedit}");
            mention::filter(&ui.members, &query).into_iter().cloned().collect()
        });
        // Esc empties the popup in the UI only: show it again on the next keystroke
        let same = UI.with_borrow(|ui| ui.mention_rows == rows);
        if same && (rows.is_empty() || slint::Model::row_count(&app.get_mention_items()) > 0) {
            return;
        }
        let items: Vec<MentionItem> = rows
            .iter()
            .map(|(uuid, name)| MentionItem {
                name: name.as_str().into(),
                initial: name.chars().next().map(String::from).unwrap_or_default().into(),
                tint: tint(uuid),
            })
            .collect();
        app.set_mention_sel(0);
        app.set_mention_items(ModelRc::new(VecModel::from(items)));
        UI.with_borrow_mut(|ui| ui.mention_rows = rows);
    });

    let weak = app.as_weak();
    app.on_mention_pick(move |row, text, cursor| {
        let Some(app) = weak.upgrade() else { return };
        app.set_mention_items(ModelRc::default());
        let picked = UI.with_borrow_mut(|ui| {
            let m = ui.mention_rows.get(row.max(0) as usize).cloned();
            ui.mention_rows.clear();
            let m = m?;
            if !ui.picked.contains(&m) {
                ui.picked.push(m.clone());
            }
            Some(m)
        });
        let Some((_, name)) = picked else { return };
        let cursor = (cursor.max(0) as usize).min(text.len());
        let Some((at, _)) = mention::query_at(&text, cursor) else { return };
        let inserted = format!("@{name} ");
        let new = format!("{}{inserted}{}", &text[..at], &text[cursor..]);
        app.set_draft(new.into());
        app.invoke_focus_composer();
        app.invoke_composer_cursor((at + inserted.len()) as i32);
    });

    let s = send.clone();
    app.on_composing(move |active| {
        if UI.with_borrow(|ui| ui.editing.is_none()) {
            s(Command::Composing(active));
        }
    });

    let weak = app.as_weak();
    app.on_open_menu(move |id, x, y| {
        let Some(app) = weak.upgrade() else { return };
        let Some((mine, can_edit, can_delete)) = UI.with_borrow(|ui| {
            let m = ui.last.iter().find(|m| m.id == id.as_str())?;
            (!m.system && !m.deleted && !m.pending).then(|| (m.my_reaction.clone(), m.can_edit, m.can_delete))
        }) else {
            return;
        };
        app.set_menu_mine(mine.into());
        app.set_menu_can_edit(can_edit);
        app.set_menu_can_delete(can_delete);
        app.set_menu_x(x);
        app.set_menu_y(y);
        app.set_menu_id(id);
    });

    let s = send.clone();
    app.on_react(move |id, emoji| {
        let mine = UI.with_borrow(|ui| ui.last.iter().find(|m| m.id == id.as_str()).map(|m| m.my_reaction.clone()));
        let Some(mine) = mine else { return };
        // the same emoji again takes the reaction back
        s(Command::React { id: id.to_string(), remove: mine == emoji.as_str(), emoji: emoji.to_string() });
    });

    let weak = app.as_weak();
    app.on_edit_message(move |id| {
        let Some(app) = weak.upgrade() else { return };
        let Some(text) = UI.with_borrow_mut(|ui| {
            let m = ui.last.iter().find(|m| m.id == id.as_str() && m.can_edit)?;
            let text = m.text.clone();
            // keep the message's mentions: their "@name"s are sent as mentions again
            ui.picked = m
                .mentions
                .iter()
                .filter_map(|(s, e, uuid)| Some((uuid.clone(), text.get(s + 1..*e)?.to_string())))
                .collect();
            ui.editing = Some(id.to_string());
            ui.reply_to = None;
            Some(text)
        }) else {
            return;
        };
        app.set_reply_preview(SharedString::new());
        app.set_edit_preview(text.replace('\n', " ").into());
        app.set_draft(text.into());
        app.invoke_focus_composer();
        app.invoke_composer_to_end();
    });

    let weak = app.as_weak();
    app.on_cancel_edit(move || {
        UI.with_borrow_mut(|ui| ui.editing = None);
        if let Some(app) = weak.upgrade() {
            app.set_edit_preview(SharedString::new());
            app.set_draft(SharedString::new());
        }
    });

    let s = send.clone();
    app.on_delete_message(move |id| {
        let ok = rfd::MessageDialog::new()
            .set_title("메시지 삭제")
            .set_description("모든 사람에게서 이 메시지를 삭제할까요?")
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();
        if ok == rfd::MessageDialogResult::Yes {
            s(Command::Delete(id.to_string()));
        }
    });

    let weak = app.as_weak();
    app.on_clear_attachments(move || {
        UI.with_borrow_mut(|ui| ui.files.clear());
        if let Some(app) = weak.upgrade() {
            app.set_pending_files(ModelRc::default());
        }
    });

    let s = send.clone();
    app.on_attachment_clicked(move |id| {
        let is_sticker = UI.with_borrow(|ui| ui.last.iter().any(|m| m.id == id.as_str() && m.sticker));
        if is_sticker {
            s(Command::StickerClicked(id.to_string()));
        } else {
            s(Command::OpenAttachment(id.to_string()));
        }
    });

    let s = send.clone();
    let weak = app.as_weak();
    app.on_toggle_stickers(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.get_sticker_open() {
            close_stickers(&app);
        } else {
            app.set_sticker_open(true);
            s(Command::StickerPanel);
        }
    });

    let s = send.clone();
    let weak = app.as_weak();
    app.on_pick_pack(move |id| {
        let Some(app) = weak.upgrade() else { return };
        app.set_current_pack(id.clone());
        if id.as_str() != "+" {
            s(Command::OpenPack(id.to_string()));
        }
    });

    let s = send.clone();
    let weak = app.as_weak();
    app.on_send_sticker(move |pack, id| {
        s(Command::SendSticker { pack: pack.to_string(), id: id as u32 });
        if let Some(app) = weak.upgrade() {
            close_stickers(&app);
        }
    });

    let s = send.clone();
    app.on_install_pack(move |id| s(Command::InstallPack { id: id.to_string(), key: String::new() }));

    let s = send.clone();
    let weak = app.as_weak();
    app.on_add_pack_url(move |url| {
        if url.trim().is_empty() {
            return;
        }
        s(Command::InstallPackUrl(url.to_string()));
        if let Some(app) = weak.upgrade() {
            app.set_pack_url(SharedString::new());
        }
    });

    let weak = app.as_weak();
    app.on_reply(move |id| {
        let Some(app) = weak.upgrade() else { return };
        let preview = UI.with_borrow_mut(|ui| {
            let m = ui.last.iter().find(|m| m.id == id.as_str())?;
            if m.system || m.deleted || m.pending {
                return None;
            }
            if ui.editing.take().is_some() {
                app.set_edit_preview(SharedString::new());
                app.set_draft(SharedString::new());
            }
            ui.reply_to = Some(m.id.clone());
            let who = if m.outgoing { "나".to_string() } else { m.sender.clone() };
            let body = if m.text.is_empty() { m.attachment.clone() } else { m.text.replace('\n', " ") };
            Some(if who.is_empty() { body } else { format!("{who}: {body}") })
        });
        if let Some(p) = preview {
            app.set_reply_preview(p.into());
            app.invoke_focus_composer();
        }
    });

    let weak = app.as_weak();
    app.on_cancel_reply(move || {
        UI.with_borrow_mut(|ui| ui.reply_to = None);
        if let Some(app) = weak.upgrade() {
            app.set_reply_preview(SharedString::new());
        }
    });

    let s = send.clone();
    app.on_search_changed(move |q| s(Command::Search(q.to_string())));

    let s = send.clone();
    app.on_unlink(move || {
        let ok = rfd::MessageDialog::new()
            .set_title("연결 해제")
            .set_description("이 PC의 연결을 해제하고 로컬 데이터를 모두 삭제할까요?")
            .set_buttons(rfd::MessageButtons::YesNo)
            .set_level(rfd::MessageLevel::Warning)
            .show();
        if ok == rfd::MessageDialogResult::Yes {
            s(Command::Unlink);
        }
    });

    app.global::<Links>().on_open(|url| link::open(&url));

    if headless::active() {
        return; // no native window to hook
    }
    // Slint exposes neither focus nor minimize/restore: hook the window procedure once the
    // native window exists (it is created when the event loop starts).
    let weak = app.as_weak();
    let tries = std::cell::Cell::new(0u32);
    let timer = Rc::new(slint::Timer::default());
    let this = timer.clone();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(50), move || {
        let Some(app) = weak.upgrade() else { return };
        let h = hwnd(&app);
        tries.set(tries.get() + 1);
        if h == 0 && tries.get() < 100 {
            return; // native window not created yet
        }
        this.stop();
        // thumbnails are pre-scaled to physical pixels: the renderer samples images nearest-neighbor
        types::set_ui_scale(app.window().scale_factor());
        let last = std::rc::Rc::new(std::cell::Cell::new(true));
        let s2 = send.clone();
        let l2 = last.clone();
        let hooked = winhook::install(&app, h, move |f| {
            if f != l2.get() {
                l2.set(f);
                s2(Command::WindowFocus(f));
            }
        });
        tracing::info!(hwnd = h, hooked, "window hook");
        if !hooked {
            // fallback: poll the foreground window
            let weak = app.as_weak();
            let s = send.clone();
            let poll = Box::leak(Box::new(slint::Timer::default()));
            poll.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(700), move || {
                let Some(app) = weak.upgrade() else { return };
                let f = is_foreground(&app);
                if f != last.get() {
                    last.set(f);
                    s(Command::WindowFocus(f));
                }
            });
        }
    });
    // the timer holds an Rc to itself (to stop itself); it lives for the app's lifetime
    std::mem::forget(timer);
}

/// Adds files to the composer's pending attachments.
fn add_files(app: &AppWindow, files: Vec<PathBuf>) {
    if files.is_empty() {
        return;
    }
    let names: Vec<SharedString> = UI.with_borrow_mut(|ui| {
        ui.files.extend(files);
        ui.files
            .iter()
            .map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default().into())
            .collect()
    });
    app.set_pending_files(ModelRc::new(VecModel::from(names)));
    app.invoke_focus_composer();
}

fn hwnd(app: &AppWindow) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let wh = app.window().window_handle();
    match wh.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

#[cfg(windows)]
fn is_foreground(app: &AppWindow) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsIconic};
    let hwnd = hwnd(app) as *mut std::ffi::c_void;
    if hwnd.is_null() {
        return true;
    }
    // SAFETY: plain Win32 queries on our own window handle
    unsafe { GetForegroundWindow() == hwnd && IsIconic(hwnd) == 0 }
}

#[cfg(not(windows))]
fn is_foreground(_: &AppWindow) -> bool {
    true
}

/// Synthetic data for checking layout and memory without a Signal account.
fn demo(emit: backend::Emit) {
    let threads: Vec<ThreadDto> = (0..40)
        .map(|i| ThreadDto {
            key: format!("c:demo-{i}"),
            name: ["김민준", "이서연", "박지훈", "가족 단톡방", "Alice", "프로젝트 팀"][i % 6].to_string(),
            snippet: "내일 몇 시에 만날까? 저녁 7시쯤 어때요".into(),
            time: if i < 3 { "오후 3:12".into() } else { format!("10월 {}일", 30 - (i % 28)) },
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
            sender: if outgoing { String::new() } else { "이서연".into() },
            sender_key: "demo".into(),
            text: if i % 7 == 0 {
                "긴 메시지 테스트입니다. 한글 줄바꿈이 제대로 되는지, 말풍선 최대 폭이 적당한지 확인하기 위한 문장입니다. 🙂".into()
            } else {
                if i == 295 { "기호·이모지 테스트: ✓ ✕ ➤ ★ ♥ → 😀🎉👍❤️🇰🇷".to_string() } else if i == 296 { "🎉🥳👍🏽".to_string() } else { format!("메시지 {i}") }
            },
            time: "오후 3:12".into(),
            outgoing,
            show_sender: !outgoing && i % 4 == 1,
            status: if outgoing { ["✓", "✓✓", "읽음 2"][(i as usize / 3) % 3].into() } else { String::new() },
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
    items[292] = MsgDto { id: "292".into(), time: "오후 3:12".into(), deleted: true, text: "삭제된 메시지입니다".into(), ..Default::default() };
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
    emit(Event::Ready);
    emit(Event::Threads(threads));
    emit(Event::Header { name: "이서연".into(), sub: "+82 10-0000-0000".into() });
    let names = ["김민준", "이서연", "박지훈", "Alice Kim", "Bob", "김지원", "최유나"];
    emit(Event::Members(names.iter().enumerate().map(|(i, n)| (format!("00000000-0000-0000-0000-00000000000{i}"), n.to_string())).collect()));
    for m in items.iter_mut().filter(|m| m.attachment.is_empty() && m.quote.is_empty() && !m.sticker) {
        m.big_emoji = emoji::only_emoji(&m.text, 5).is_some();
    }
    emit(Event::Messages { key: "c:demo-1".into(), items, can_older: false, scroll: Scroll::Bottom });
    emit(Event::Status("데모 모드".into()));
    if std::env::var_os("LITE_SIGNAL_DEMO_STICKERS").is_some() {
        demo_stickers(emit);
    }
}

/// LITE_SIGNAL_SELFTEST=1 (with demo): measure what the file dialog and toasts cost.
fn selftest() {
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
                let _ = notify_rust::Notification::new().summary("selftest").body("toast").show();
            }
            _ => {}
        }
    });
}

fn demo_blob(key: &str, size: u32, hue: u8) -> types::Thumb {
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

fn demo_stickers(emit: backend::Emit) {
    for i in 0..2u8 {
        emit(Event::Thumb(Arc::new(demo_blob(&format!("demo-st-{i}"), 200, 60 + i * 120))));
    }
    let packs: Vec<types::PackDto> = (0..3)
        .map(|i| types::PackDto {
            id: format!("p{i}"),
            title: format!("추천 팩 {i}"),
            installed: i < 2 && std::env::var("LITE_SIGNAL_DEMO_STICKERS").as_deref() != Ok("add"),
            cover_key: format!("sp_p{i}_0"),
        })
        .collect();
    let items: Vec<types::StickerDto> =
        (0..20).map(|i| types::StickerDto { key: format!("sp_p0_{i}"), id: i, emoji: String::new() }).collect();
    // the panel is opened by main(); give it a moment before filling it
    slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
        emit(Event::StickerPacks(packs));
        if std::env::var("LITE_SIGNAL_DEMO_STICKERS").as_deref() != Ok("add") {
            emit(Event::StickerGrid { pack: "p0".into(), items });
        }
        for i in 0..20u32 {
            emit(Event::Thumb(Arc::new(demo_blob(&format!("sp_p0_{i}"), 96, (i * 12) as u8))));
        }
        emit(Event::Thumb(Arc::new(demo_blob("sp_p1_0", 96, 200))));
    });
}
