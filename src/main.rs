#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alloc;
mod avatar;
mod badge;
mod backend;
mod clip;
mod picker;
mod db;
mod emoji;
mod headless;
mod format;
mod link;
mod mention;
mod secret;
mod settings;
mod toast;
mod types;
mod view;
mod winhook;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use i_slint_core::styled_text::{sigwin_clear_plain_styles, sigwin_set_plain_style, sigwin_styled, SigwinStyle};
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
    /// round photos by thread key, for the whole session (~6KB each at 40px)
    avatars: HashMap<String, Image>,
    /// photos next to group messages by sender uuid, open thread only
    sender_avatars: HashMap<String, Image>,
    last: Vec<MsgDto>,
    files: Vec<PathBuf>,
    reply_to: Option<String>,
    /// id of our message being edited in the composer
    editing: Option<String>,
    /// where pasted clipboard images are written
    paste_dir: PathBuf,
    open_key: String,
    loading_older: bool,
    /// an older page waiting for its anchor to be measured (`hold_older`), with can_older
    older: Option<(Vec<MsgDto>, bool)>,
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
    /// who is typing in the open thread: (uuid, name), and whether it's a group
    typing: Vec<(String, String)>,
    typing_group: bool,
    /// the last row of `messages` is the typing bubble (not in `last`)
    typing_row: bool,
    settings: settings::Settings,
    settings_path: PathBuf,
    /// messages whose spoilers were clicked open, open thread only
    revealed: std::collections::HashSet<String>,
}

/// Brings the message model from `ui.last` to `items` touching only the rows that differ.
/// Replacing the whole model makes the ListView drop every row and re-seek, which snaps the
/// scroll position to a row boundary on every reload (receipts, reactions, typing).
fn update_rows(ui: &mut Ui, items: &[MsgDto], dark: bool) {
    use slint::Model;
    if ui.typing_row {
        let n = ui.messages.row_count();
        if n > 0 {
            ui.messages.remove(n - 1);
        }
        ui.typing_row = false;
    }
    let old = &ui.last;
    let max = old.len().min(items.len());
    let pre = old.iter().zip(items).take_while(|(a, b)| a.id == b.id).count();
    let suf = old.iter().rev().zip(items.iter().rev()).take(max - pre).take_while(|(a, b)| a.id == b.id).count();
    let item = |m: &MsgDto| to_item(m, &ui.thumbs, &ui.sender_avatars, &ui.revealed, dark);
    for i in (0..pre).chain(old.len() - suf..old.len()) {
        let j = if i < pre { i } else { i + items.len() - old.len() };
        if old[i] != items[j] {
            ui.messages.set_row_data(i, item(&items[j]));
        }
    }
    for _ in pre..old.len() - suf {
        ui.messages.remove(pre);
    }
    for (k, m) in items[pre..items.len() - suf].iter().enumerate() {
        ui.messages.insert(pre + k, item(m));
    }
}

/// Re-derives the relative times of recent bubbles ("3분") as the minutes pass.
fn refresh_times(ui: &Ui) {
    use slint::Model;
    for (i, m) in ui.last.iter().enumerate().rev().take(100) {
        let Ok(ts) = m.id.parse::<u64>() else { continue };
        let t = view::bubble_time(ts, &m.time);
        let t = if m.pinned { format!("📌 {t}") } else { t };
        if let Some(mut row) = ui.messages.row_data(i) {
            if row.time.as_str() != t {
                row.time = t.into();
                ui.messages.set_row_data(i, row);
            }
        }
    }
}

/// Put the typing bubble at the end of the message list, or take it away.
fn sync_typing(ui: &mut Ui) {
    if ui.typing_row {
        let n = slint::Model::row_count(&*ui.messages);
        if n > 0 {
            ui.messages.remove(n - 1);
        }
        ui.typing_row = false;
    }
    let Some((uuid, name)) = ui.typing.first() else { return };
    let avatar = if ui.typing_group { ui.sender_avatars.get(uuid).cloned() } else { None };
    ui.messages.push(MsgItem {
        id: "typing".into(),
        typing: true,
        avatar_col: ui.typing_group,
        show_avatar: ui.typing_group,
        has_avatar: avatar.is_some(),
        avatar: avatar.unwrap_or_default(),
        initial: initial(name).into(),
        sender_color: tint(uuid),
        ..Default::default()
    });
    ui.typing_row = true;
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
    // a jump to a pinned message owns the scroll position until it lands
    if !app.get_jump_id().is_empty() {
        return;
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
    /// stick_to_bottom
    static TIMER: slint::Timer = slint::Timer::default();
    /// jump_to_row
    static JUMP: slint::Timer = slint::Timer::default();
}

/// Prefix of the link that opens a message's spoilers; the message id follows.
const SPOILER_URL: &str = "sigwin-spoiler:";

/// Font styles of a message (bold, italic, strikethrough, monospace) as styled text spans.
fn font_spans(m: &MsgDto) -> Vec<(std::ops::Range<usize>, SigwinStyle)> {
    m.styles
        .iter()
        .filter_map(|&(s, e, k)| {
            let style = match k {
                view::STYLE_BOLD => SigwinStyle::Bold,
                view::STYLE_ITALIC => SigwinStyle::Italic,
                view::STYLE_STRIKE => SigwinStyle::Strike,
                view::STYLE_MONO => SigwinStyle::Mono,
                _ => return None,
            };
            Some((s..e, style))
        })
        .collect()
}

/// Spoiler ranges still hidden.
fn hidden_spoilers(m: &MsgDto, revealed: &std::collections::HashSet<String>) -> Vec<(usize, usize)> {
    if m.deleted || revealed.contains(&m.id) {
        return Vec::new();
    }
    m.styles.iter().filter(|s| s.2 == view::STYLE_SPOILER).map(|&(s, e, _)| (s, e)).collect()
}

/// Message text with the @mentions colored, links underlined and spoilers covered. The styled
/// text is drawn over a TextInput whose own glyphs are transparent, so selection keeps working.
/// Both get the same font styles (the TextInput through `sigwin_set_plain_style`), so they lay
/// out identically; the rest only changes colors, underlines and boxes.
fn rich_text(m: &MsgDto, links: &[(usize, usize)], spoilers: &[(usize, usize)], dark: bool) -> slint::StyledText {
    // link color comes from the StyledText's `link-color`
    let mention = match (m.outgoing, dark) {
        (true, _) => Color::from_rgb_u8(0xff, 0xe2, 0x7a),
        (false, true) => Color::from_rgb_u8(0x7a, 0xa7, 0xff),
        (false, false) => Color::from_rgb_u8(0x1f, 0x5f, 0xd6),
    };
    // alpha 1 = spoiler box of this color (patched i-slint-core)
    let (r, g, b) = match (m.outgoing, dark) {
        (true, _) => (0xa9, 0xbf, 0xf8),
        (false, true) => (0x6a, 0x6a, 0x72),
        (false, false) => (0xb4, 0xb4, 0xbc),
    };
    let spoiler = Color::from_argb_u8(1, r, g, b);
    let covered = |s: usize, e: usize| spoilers.iter().any(|&(a, b)| s < b && a < e);
    let mut spans = font_spans(m);
    spans.extend(m.mentions.iter().map(|&(s, e, _)| (s..e, SigwinStyle::Color(mention))));
    for &(s, e) in links.iter().filter(|&&(s, e)| !covered(s, e)) {
        if let Some(part) = m.text.get(s..e) {
            // the StyledText underlines it and reports clicks with the URL
            spans.push((s..e, SigwinStyle::Link(link::target(part))));
        }
    }
    let url = format!("{SPOILER_URL}{}", m.id);
    for &(s, e) in spoilers {
        spans.push((s..e, SigwinStyle::Color(spoiler)));
        spans.push((s..e, SigwinStyle::Hit(url.clone())));
    }
    sigwin_styled(&m.text, &spans)
}

/// The photos (first `MAX_READER_PHOTOS`) of those who read a message; keys `r:<uuid>` in `senders`.
fn reader_items(m: &MsgDto, senders: &HashMap<String, Image>) -> ModelRc<ReaderItem> {
    let rows: Vec<ReaderItem> = m
        .readers
        .iter()
        .take(types::MAX_READER_PHOTOS)
        .map(|(uuid, name)| {
            let photo = senders.get(&format!("{}{uuid}", avatar::READ_PREFIX)).cloned();
            ReaderItem {
                has_photo: photo.is_some(),
                photo: photo.unwrap_or_default(),
                initial: initial(name).into(),
                tint: tint(uuid),
            }
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

fn to_item(
    m: &MsgDto,
    thumbs: &HashMap<String, Image>,
    senders: &HashMap<String, Image>,
    revealed: &std::collections::HashSet<String>,
    dark: bool,
) -> MsgItem {
    let image = thumbs.get(&m.image_key).cloned();
    let avatar = if m.show_avatar { senders.get(&m.sender_key).cloned() } else { None };
    let reactions: Vec<ReactionItem> = m
        .reactions
        .iter()
        .map(|(e, n)| ReactionItem { emoji: e.as_str().into(), count: *n as i32 })
        .collect();
    let links = if m.deleted { Vec::new() } else { link::find(&m.text) };
    let fonts = if m.deleted { Vec::new() } else { font_spans(m) };
    if !fonts.is_empty() {
        // the TextInput lays out with the same font styles as the StyledText over it
        sigwin_set_plain_style(&m.text, Some(sigwin_styled(&m.text, &fonts)));
    }
    let spoilers = hidden_spoilers(m, revealed);
    let has_rich = !m.mentions.is_empty() || !links.is_empty() || !spoilers.is_empty();
    MsgItem {
        reactions: ModelRc::new(VecModel::from(reactions)),
        big_emoji: m.big_emoji,
        id: m.id.as_str().into(),
        sender: m.sender.as_str().into(),
        sender_color: tint(&m.sender_key),
        text: m.text.as_str().into(),
        has_rich,
        rich: if has_rich { rich_text(m, &links, &spoilers, dark) } else { Default::default() },
        time: {
            let t = m.id.parse::<u64>().map_or(m.time.clone(), |ts| view::bubble_time(ts, &m.time));
            if m.pinned { format!("📌 {t}").into() } else { t.into() }
        },
        outgoing: m.outgoing,
        show_sender: m.show_sender,
        avatar_col: m.avatar_col,
        show_avatar: m.show_avatar,
        has_avatar: avatar.is_some(),
        avatar: avatar.unwrap_or_default(),
        initial: initial(&m.sender).into(),
        status: m.status.as_str().into(),
        readers: reader_items(m, senders),
        readers_more: m.readers.len().saturating_sub(types::MAX_READER_PHOTOS) as i32,
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
        hide_time: m.hide_time,
        typing: false,
    }
}

fn thread_item(t: &ThreadDto, avatars: &HashMap<String, Image>) -> ThreadItem {
    let photo = avatars.get(&t.key);
    ThreadItem {
        has_photo: photo.is_some(),
        photo: photo.cloned().unwrap_or_default(),
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

/// The open thread's photo (or initial) in the conversation header, from its thread list row.
fn header_avatar(app: &AppWindow, ui: &Ui) {
    let row = (0..slint::Model::row_count(&*ui.threads))
        .filter_map(|i| slint::Model::row_data(&*ui.threads, i))
        .find(|t| t.key.as_str() == ui.open_key);
    let Some(row) = row else {
        // not in the list (a contact found by search)
        app.set_current_initial(initial(&app.get_current_name()).into());
        app.set_current_tint(tint(&ui.open_key));
        app.set_current_has_photo(false);
        app.set_current_photo(Image::default());
        return;
    };
    app.set_current_initial(row.initial);
    app.set_current_tint(row.color);
    app.set_current_has_photo(row.has_photo);
    app.set_current_photo(row.photo);
}

/// Scrolls the message list until row `id` is centered in the view, then flashes it.
///
/// Only rows on screen exist, and after every layout the ListView rescales all positions by the
/// average height of those rows, so positions from different frames don't match. So this
/// alternates: one frame the target row reports its on-screen position (y and content-y read
/// together), the next frame acts on it.
/// - Row on screen: shift by the exact distance (a short scroll lays out real row heights).
/// - Row not instantiated: content-y = -(idx + 0.5) * average height. That is the ListView's own
///   "random seek" formula (offset = floor(-content-y / average)) inverted, so for a far target
///   the row lands at the top of the view; a near one just scrolls toward it.
fn jump_to_row(app: &AppWindow, id: String) {
    seek_row(app, id, None);
}

/// Moves row `id` so its bottom edge is `want` px from the top of the list (None: centered,
/// then flashed).
fn seek_row(app: &AppWindow, id: String, want: Option<f32>) {
    let Some(idx) = UI.with_borrow(|ui| ui.last.iter().position(|m| m.id == id)) else { return };
    TIMER.with(|t| t.stop());
    app.set_jump_id(id.as_str().into());
    app.set_jump_found(false);
    app.set_jump_probe(app.get_jump_probe() + 1);
    let weak = app.as_weak();
    let mut ticks = 0u32;
    let mut last_pos: Option<f32> = None;
    JUMP.with(|t| {
        t.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(20), move || {
            let Some(app) = weak.upgrade() else { return };
            ticks += 1;
            if ticks % 2 == 0 {
                // measure (the layout has caught up with the last move)
                app.set_jump_found(false);
                app.set_jump_probe(app.get_jump_probe() + 1);
                return;
            }
            let vh = app.invoke_list_visible_height();
            let mut done = ticks >= 60;
            if app.get_jump_found() {
                let pos = app.get_jump_pos();
                // centered; a row taller than the view starts at the top
                // a `want` is for the row's bottom edge, which doesn't move when its day
                // separator comes or goes
                let (want, near) = match want {
                    Some(w) => (w - app.get_jump_h(), 0.5),
                    None => (((vh - app.get_jump_h()) / 2.0).max(0.0), 4.0),
                };
                // close enough, or the list can't scroll further (row near either end)
                if (pos - want).abs() <= near || last_pos.is_some_and(|p: f32| (p - pos).abs() < near.min(1.0)) {
                    done = true;
                } else {
                    app.invoke_shift_by(pos - want);
                }
                last_pos = Some(pos);
            } else {
                last_pos = None;
                // rows above may have been added since (older history, a reload)
                let jid = app.get_jump_id();
                let idx = UI.with_borrow(|ui| ui.last.iter().position(|m| m.id == jid.as_str())).unwrap_or(idx);
                let n = slint::Model::row_count(&app.get_messages()).max(1) as f32;
                let avg = app.invoke_content_height() / n;
                app.invoke_set_content_y(-(idx as f32 + 0.5) * avg);
            }
            if done {
                JUMP.with(|t| t.stop());
                let id = app.get_jump_id();
                app.set_jump_id(SharedString::new());
                if want.is_some() {
                    tracing::debug!("older page anchored after {ticks} ticks");
                    // still at the top (short page): go on loading
                    app.invoke_check_older();
                    return;
                }
                app.set_flash_id(id.clone());
                let weak = app.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
                    if let Some(app) = weak.upgrade().filter(|a| a.get_flash_id() == id) {
                        app.set_flash_id(SharedString::new());
                    }
                });
            }
        });
    });
}

/// Puts a message list on screen. `rebuild` replaces the whole model (thread switch, anchored
/// older page); otherwise only changed rows are touched, and rows inserted above the ones on
/// screen keep the view where it is.
fn show_rows(app: &AppWindow, items: Vec<MsgDto>, can_older: bool, scroll: Scroll, rebuild: bool) {
    let near_bottom = app.get_near_bottom();
    UI.with_borrow_mut(|ui| {
        ui.loading_older = false;
        if rebuild {
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
        if rebuild {
            ui.messages.set_vec(items.iter().map(|m| to_item(m, &ui.thumbs, &ui.sender_avatars, &ui.revealed, dark)).collect::<Vec<_>>());
            ui.typing_row = false;
        } else {
            update_rows(ui, &items, dark);
        }
        sync_typing(ui);
        ui.last = items;
    });
    app.set_can_load_older(can_older);
    match scroll {
        Scroll::Bottom => stick_to_bottom(app),
        Scroll::KeepIfNotAtBottom if near_bottom => stick_to_bottom(app),
        Scroll::KeepIfNotAtBottom | Scroll::Keep | Scroll::Older => {}
    }
    if rebuild && scroll != Scroll::Older {
        app.invoke_focus_composer();
    }
}

/// An older page arrived: keep the row that was on top (`anchor`, the oldest row so far) at
/// the same place on screen.
///
/// If the anchor isn't instantiated, the view is further down and inserting rows above it
/// keeps the position by itself. If it is, the ListView is at its first row and would show
/// the inserted rows instead, and its content height is only (average row height x rows), so
/// shifting by the height difference lands rows off. So: measure the anchor (one frame),
/// replace the model, seek straight to the anchor's index with the ListView's own estimate
/// (it then lays out from that row), and settle it at the measured spot like a pin jump.
fn hold_older(app: &AppWindow, anchor: String, items: Vec<MsgDto>, can_older: bool) {
    UI.with_borrow_mut(|ui| ui.older = Some((items, can_older)));
    TIMER.with(|t| t.stop());
    app.set_jump_id(anchor.as_str().into());
    app.set_jump_found(false);
    app.set_jump_probe(app.get_jump_probe() + 1);
    let weak = app.as_weak();
    JUMP.with(|t| {
        t.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(20), move || {
            let Some(app) = weak.upgrade() else { return };
            let ours = app.get_jump_id() == anchor.as_str();
            let Some((items, can_older)) = UI.with_borrow_mut(|ui| ui.older.take()) else {
                if ours {
                    app.set_jump_id(SharedString::new());
                }
                return;
            };
            let found = ours && app.get_jump_found();
            if ours {
                app.set_jump_id(SharedString::new());
            }
            let idx = items.iter().position(|m| m.id == anchor);
            let (Some(idx), true) = (idx, found) else {
                tracing::debug!("older page: anchor not on screen, inserting above");
                show_rows(&app, items, can_older, Scroll::Older, false);
                // a short page may leave the top still near (content-y is current after a layout)
                let weak = app.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
                    if let Some(app) = weak.upgrade() {
                        app.invoke_check_older();
                    }
                });
                return;
            };
            let want = app.get_jump_pos() + app.get_jump_h();
            let n = slint::Model::row_count(&app.get_messages()).max(1) as f32;
            let avg = app.invoke_content_height() / n;
            tracing::debug!("older page: {idx} rows above the anchor, its bottom at {want:.1}px");
            show_rows(&app, items, can_older, Scroll::Older, true);
            app.invoke_set_content_y_raw(-(idx as f32 + 0.5) * avg);
            seek_row(&app, anchor.clone(), Some(want));
        });
    });
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
            let total: u32 = list.iter().map(|t| t.unread).sum();
            app.set_total_unread(total as i32);
            badge::set_count(total);
            ui.threads.set_vec(list.iter().map(|t| thread_item(t, &ui.avatars)).collect::<Vec<_>>());
            header_avatar(app, ui);
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
                    ui.sender_avatars.clear();
                    ui.revealed.clear();
                    sigwin_clear_plain_styles();
                    ui.open_key = key.clone();
                    ui.reply_to = None;
                    ui.picked.clear();
                    ui.mention_rows.clear();
                    ui.typing.clear();
                    if ui.editing.take().is_some() {
                        app.set_draft(SharedString::new());
                    }
                }
                switched
            });
            if switched {
                app.set_pin_index(0);
                app.set_flash_id(SharedString::new());
                app.set_reply_preview(SharedString::new());
                app.set_edit_preview(SharedString::new());
                app.set_menu_id(SharedString::new());
                app.set_mention_items(ModelRc::default());
                close_stickers(app);
                UI.with_borrow(|ui| header_avatar(app, ui));
            }
            app.set_current_key(key.into());
            if !switched && scroll != Scroll::Bottom && UI.with_borrow(|ui| ui.older.is_some()) {
                // an older page is being anchored: show the newest rows when it lands
                UI.with_borrow_mut(|ui| ui.older = Some((items, can_older)));
                return;
            }
            UI.with_borrow_mut(|ui| ui.older = None);
            let anchor = UI.with_borrow(|ui| ui.last.first().map(|m| m.id.clone()));
            match anchor {
                Some(anchor) if scroll == Scroll::Older && !switched && app.get_jump_id().is_empty() => {
                    hold_older(app, anchor, items, can_older)
                }
                _ => show_rows(app, items, can_older, scroll, switched),
            }
        }
        Event::Pins(list) => {
            let items: Vec<PinItem> = list
                .iter()
                .map(|p| PinItem { id: p.id.as_str().into(), sender: p.sender.as_str().into(), text: p.text.as_str().into() })
                .collect();
            if app.get_pin_index() >= items.len() as i32 {
                app.set_pin_index(0);
            }
            app.set_pins(ModelRc::new(VecModel::from(items)));
        }
        Event::JumpTo(id) => jump_to_row(app, id),
        Event::Avatar(t) => UI.with_borrow_mut(|ui| {
            let img = (t.width > 0).then(|| {
                Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height))
            });
            if let Some(uuid) = t.key.strip_prefix(avatar::READ_PREFIX) {
                let Some(img) = img else { return };
                ui.sender_avatars.insert(t.key.clone(), img);
                for (i, m) in ui.last.iter().enumerate() {
                    if m.readers.iter().take(types::MAX_READER_PHOTOS).any(|(u, _)| u == uuid) {
                        if let Some(mut row) = slint::Model::row_data(&*ui.messages, i) {
                            row.readers = reader_items(m, &ui.sender_avatars);
                            slint::Model::set_row_data(&*ui.messages, i, row);
                        }
                    }
                }
                return;
            }
            if let Some(uuid) = t.key.strip_prefix(avatar::MSG_PREFIX) {
                let Some(img) = img else { return };
                ui.sender_avatars.insert(uuid.to_string(), img.clone());
                for (i, m) in ui.last.iter().enumerate() {
                    if m.show_avatar && m.sender_key == uuid {
                        if let Some(mut row) = slint::Model::row_data(&*ui.messages, i) {
                            row.has_avatar = true;
                            row.avatar = img.clone();
                            slint::Model::set_row_data(&*ui.messages, i, row);
                        }
                    }
                }
                if ui.typing.first().is_some_and(|(u, _)| u == uuid) {
                    sync_typing(ui);
                }
                return;
            }
            let changed = match img.clone() {
                Some(i) => {
                    ui.avatars.insert(t.key.clone(), i);
                    true
                }
                None => ui.avatars.remove(&t.key).is_some(),
            };
            if !changed {
                return;
            }
            for i in 0..slint::Model::row_count(&*ui.threads) {
                if let Some(mut row) = slint::Model::row_data(&*ui.threads, i) {
                    if row.key.as_str() == t.key {
                        row.has_photo = img.is_some();
                        row.photo = img.clone().unwrap_or_default();
                        slint::Model::set_row_data(&*ui.threads, i, row);
                    }
                }
            }
            if t.key == ui.open_key {
                header_avatar(app, ui);
            }
        }),
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
        Event::Typing { group, who } => {
            let near_bottom = app.get_near_bottom();
            UI.with_borrow_mut(|ui| {
                ui.typing = who;
                ui.typing_group = group;
                sync_typing(ui);
            });
            if near_bottom {
                stick_to_bottom(app);
            }
        }
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
        Event::Account { name, number, device, receipts, typing } => {
            let state = |v: Option<bool>| if v == Some(false) { "꺼짐" } else { "켜짐" };
            let pending = if receipts.is_none() || typing.is_none() { " (아직 받지 못함)" } else { "" };
            let phone = format!("{} · {} — 휴대폰 Signal 앱의 설정을 따릅니다{pending}", state(receipts), state(typing));
            app.set_account_name(name.into());
            app.set_account_number(number.into());
            app.set_account_device(device.into());
            app.set_account_phone(phone.into());
        }
        Event::Storage { db, attachments, thumbs } => {
            app.set_storage_text(
                format!("메시지 DB {}  ·  첨부파일 {}  ·  썸네일 캐시 {}", size_text(db), size_text(attachments), size_text(thumbs)).into(),
            );
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

fn size_text(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    match b as f64 {
        x if x >= 1024.0 * MB => format!("{:.1}GB", x / 1024.0 / MB),
        x if x >= 10.0 * MB => format!("{:.0}MB", x / MB),
        x if x >= 0.1 * MB => format!("{:.1}MB", x / MB),
        x => format!("{:.0}KB", (x / 1024.0).ceil()),
    }
}

/// Light / dark per the setting. "System" is Slint's default binding at startup; switching
/// back to it at runtime reads the current Windows setting (the binding can't be restored).
fn apply_theme(app: &AppWindow, startup: bool) {
    if headless::active() {
        return; // SIGWIN_HEADLESS_THEME decides
    }
    match UI.with_borrow(|ui| ui.settings.theme) {
        1 => app.invoke_set_dark(false),
        2 => app.invoke_set_dark(true),
        _ if !startup => app.invoke_set_dark(settings::system_dark()),
        _ => {}
    }
}

/// Shows the spoilers of message `id` (clicked).
fn reveal_spoilers(app: &AppWindow, id: &str) {
    use slint::Model;
    let dark = app.global::<Theme>().get_dark();
    UI.with_borrow_mut(|ui| {
        ui.revealed.insert(id.to_string());
        if let Some(i) = ui.last.iter().position(|m| m.id == id) {
            if i < ui.messages.row_count() {
                let item = to_item(&ui.last[i], &ui.thumbs, &ui.sender_avatars, &ui.revealed, dark);
                ui.messages.set_row_data(i, item);
            }
        }
    });
}

/// Rebuild the message rows (their mention colors depend on the theme).
fn refresh_rows(app: &AppWindow) {
    let dark = app.global::<Theme>().get_dark();
    UI.with_borrow_mut(|ui| {
        let rows: Vec<MsgItem> = ui.last.iter().map(|m| to_item(m, &ui.thumbs, &ui.sender_avatars, &ui.revealed, dark)).collect();
        ui.messages.set_vec(rows);
        ui.typing_row = false;
        sync_typing(ui);
    });
}

/// Window scale = the monitor's DPI scale x the zoom setting. winit only knows the former and
/// resets to it on DPI changes (`winhook` calls this again then).
pub(crate) fn apply_zoom(app: &AppWindow) {
    if headless::active() {
        return;
    }
    let h = hwnd(app);
    if h == 0 {
        types::set_ui_scale(app.window().scale_factor());
        return;
    }
    let zoom = UI.with_borrow(|ui| ui.settings.zoom) as f32 / 100.0;
    let sf = monitor_scale(h) * zoom;
    let w = app.window();
    if (w.scale_factor() - sf).abs() > 0.001 {
        let size = w.size();
        w.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: sf });
        w.dispatch_event(slint::platform::WindowEvent::Resized { size: size.to_logical(sf) });
        winhook::repaint_all(app);
    }
    // thumbnails are pre-scaled to physical pixels: the renderer samples images nearest-neighbor
    types::set_ui_scale(sf);
}

#[cfg(windows)]
fn monitor_scale(hwnd: isize) -> f32 {
    // SAFETY: plain query on our own window
    let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd as _) };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

#[cfg(not(windows))]
fn monitor_scale(_: isize) -> f32 {
    1.0
}

/// Settings -> the page's controls.
fn show_settings(app: &AppWindow, s: &settings::Settings) {
    app.set_set_notify(s.notify);
    app.set_set_preview(s.preview as i32);
    app.set_set_badge(s.badge);
    app.set_set_theme(s.theme as i32);
    app.set_set_zoom(settings::ZOOMS.iter().position(|&z| z == s.zoom).unwrap_or(1) as i32);
    app.set_enter_sends(s.enter_sends);
}

/// `SIGWIN_DATA`, else `%LOCALAPPDATA%\Sigwin\data`. The folder of the old name
/// (`lite-signal\lite-signal\data`) is moved there once.
fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(d) = std::env::var("SIGWIN_DATA") {
            return PathBuf::from(d);
        }
        let local = |org: &str, app: &str| {
            directories::ProjectDirs::from("dev", org, app).map(|p| p.data_local_dir().to_path_buf())
        };
        let Some(dir) = local("", "Sigwin") else { return PathBuf::from("sigwin-data") };
        if let Some(old) = local("lite-signal", "lite-signal").filter(|o| o.exists() && !dir.exists()) {
            let moved = dir.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::rename(&old, &dir));
            match moved {
                Ok(()) => {
                    tracing::info!("moved data folder {} -> {}", old.display(), dir.display());
                    // drop the now-empty lite-signal\lite-signal and lite-signal folders
                    for p in old.ancestors().skip(1).take(2) {
                        let _ = std::fs::remove_dir(p);
                    }
                }
                Err(e) => {
                    // e.g. an old build still has the DB open: keep using it rather than relink
                    tracing::warn!("can't move data folder {}: {e}; using it in place", old.display());
                    return old;
                }
            }
        }
        dir
    })
    .clone()
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
    if !headless::active() {
        toast::register();
    }

    let app = AppWindow::new()?;
    if headless::active() {
        app.invoke_set_dark(headless::dark());
    }
    let age_timer = Box::leak(Box::new(slint::Timer::default()));
    age_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(30), || {
        UI.with_borrow(|ui| refresh_times(ui));
    });
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
    if std::env::var_os("SIGWIN_DEMO").is_some() {
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
    } else {
        let _backend = backend::spawn(data_dir(), cmd_rx, emit);
    }

    let paste_dir = data_dir().join("paste");
    clip::clean(&paste_dir);
    let settings_path = settings::path(&data_dir());
    let prefs = settings::Settings::load(&settings_path);
    prefs.publish();
    badge::set_enabled(prefs.badge);
    show_settings(&app, &prefs);
    app.set_version(format!("Sigwin {}", env!("CARGO_PKG_VERSION")).into());
    UI.with_borrow_mut(|ui| {
        ui.tx = Some(cmd_tx.clone());
        ui.paste_dir = paste_dir;
        ui.settings = prefs;
        ui.settings_path = settings_path;
    });
    apply_theme(&app, true);
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
    let weak = app.as_weak();
    app.on_load_older(move || {
        // not while a jump or an older page is settling
        if weak.upgrade().is_some_and(|a| !a.get_jump_id().is_empty()) {
            return;
        }
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
    app.on_format_toggle({
        let weak = app.as_weak();
        move |style, text, anchor, cursor| {
            let Some(app) = weak.upgrade() else { return };
            let (new, a, b) = format::toggle(&text, anchor.max(0) as usize, cursor.max(0) as usize, style as u8);
            let was_empty = text.is_empty();
            app.set_draft(new.into());
            app.invoke_composer_select(a as i32, b as i32);
            if was_empty {
                app.invoke_composing(true);
            }
        }
    });

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
        let Some((mine, can_edit, can_delete, pinned)) = UI.with_borrow(|ui| {
            let m = ui.last.iter().find(|m| m.id == id.as_str())?;
            (!m.system && !m.deleted && !m.pending).then(|| (m.my_reaction.clone(), m.can_edit, m.can_delete, m.pinned))
        }) else {
            return;
        };
        app.set_menu_pinned(pinned);
        app.set_menu_pin_step(false);
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
            // keep the message's mentions: their "@name"s are sent as mentions again
            ui.picked = m
                .mentions
                .iter()
                .filter_map(|(s, e, uuid)| Some((uuid.clone(), m.text.get(s + 1..*e)?.to_string())))
                .collect();
            // its styles come back as markers
            let text = format::to_markup(&m.text, &m.styles);
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
    app.on_pin_message(move |id, secs| {
        s(Command::Pin { id: id.to_string(), secs: (secs > 0).then_some(secs as u32) });
    });

    let s = send.clone();
    app.on_unpin(move |id| s(Command::Unpin(id.to_string())));

    let s = send.clone();
    let weak = app.as_weak();
    let demo = std::env::var_os("SIGWIN_DEMO").is_some();
    app.on_pin_clicked(move |id| match weak.upgrade() {
        // no backend: demo pin ids are row ids
        Some(app) if demo => jump_to_row(&app, id.to_string()),
        _ => s(Command::JumpTo(id.to_string())),
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
            let body = if m.text.is_empty() {
                m.attachment.clone()
            } else {
                view::mask_spoilers(m.text.clone(), &m.styles).replace('\n', " ")
            };
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

    let s = send.clone();
    let weak = app.as_weak();
    app.on_open_settings(move || {
        let Some(app) = weak.upgrade() else { return };
        app.set_menu_id(SharedString::new());
        app.set_mention_items(ModelRc::default());
        // typing must not go to the composer hidden under the page
        app.invoke_blur_composer();
        app.set_settings_open(true);
        s(Command::SettingsOpened);
    });

    let weak = app.as_weak();
    app.on_settings_changed(move || {
        let Some(app) = weak.upgrade() else { return };
        let new = settings::Settings {
            notify: app.get_set_notify(),
            preview: app.get_set_preview().clamp(0, 2) as u8,
            badge: app.get_set_badge(),
            theme: app.get_set_theme().clamp(0, 2) as u8,
            zoom: settings::ZOOMS.get(app.get_set_zoom().max(0) as usize).copied().unwrap_or(100),
            enter_sends: app.get_enter_sends(),
        };
        let (old, path) = UI.with_borrow_mut(|ui| (std::mem::replace(&mut ui.settings, new.clone()), ui.settings_path.clone()));
        if old == new {
            return;
        }
        new.publish();
        badge::set_enabled(new.badge);
        if old.theme != new.theme {
            apply_theme(&app, false);
            refresh_rows(&app);
        }
        if old.zoom != new.zoom {
            apply_zoom(&app);
        }
        new.save(&path);
    });

    let s = send.clone();
    app.on_clear_thumbs(move || s(Command::ClearThumbs));

    let weak = app.as_weak();
    app.global::<Links>().on_open(move |url| match url.strip_prefix(SPOILER_URL) {
        Some(id) => reveal_spoilers(&weak.unwrap(), id),
        None => link::open(&url),
    });

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
        // the zoom setting on top of the monitor's scale; also tells the backend the scale
        apply_zoom(&app);
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
        badge::set_hwnd(h);
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
fn demo(emit: backend::Emit, mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>) {
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
            show_sender: !outgoing && i % 4 == 1,
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
    view::mark_avatar_runs(&mut items);
    for i in [150usize, 290] {
        items[i].pinned = true;
    }
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
            view::mark_avatar_runs(&mut rows);
            rows
        };
        emit(Event::Messages { key: "c:demo-1".into(), items: window(start), can_older: true, scroll: Scroll::Bottom });
        let emit = emit.clone();
        std::thread::spawn(move || {
            while let Some(c) = cmd_rx.blocking_recv() {
                if matches!(c, Command::LoadOlder) && start > 0 {
                    std::thread::sleep(delay);
                    start = start.saturating_sub(60);
                    emit(Event::Messages { key: "c:demo-1".into(), items: window(start), can_older: start > 0, scroll: Scroll::Older });
                }
            }
        });
    } else {
        emit(Event::Messages { key: "c:demo-1".into(), items: items.clone(), can_older: false, scroll: Scroll::Bottom });
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
                let _ = toast::notification().summary("selftest").body("toast").show();
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
            installed: i < 2 && std::env::var("SIGWIN_DEMO_STICKERS").as_deref() != Ok("add"),
            cover_key: format!("sp_p{i}_0"),
        })
        .collect();
    let items: Vec<types::StickerDto> =
        (0..20).map(|i| types::StickerDto { key: format!("sp_p0_{i}"), id: i, emoji: String::new() }).collect();
    // the panel is opened by main(); give it a moment before filling it
    slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
        emit(Event::StickerPacks(packs));
        if std::env::var("SIGWIN_DEMO_STICKERS").as_deref() != Ok("add") {
            emit(Event::StickerGrid { pack: "p0".into(), items });
        }
        for i in 0..20u32 {
            emit(Event::Thumb(Arc::new(demo_blob(&format!("sp_p0_{i}"), 96, (i * 12) as u8))));
        }
        emit(Event::Thumb(Arc::new(demo_blob("sp_p1_0", 96, 200))));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(ids: &[&str]) -> Vec<MsgDto> {
        ids.iter().map(|s| MsgDto { id: s.trim_end_matches('*').into(), text: s.to_string(), ..Default::default() }).collect()
    }

    #[test]
    fn rows_update_in_place() {
        use slint::Model;
        let cases: [(&[&str], &[&str]); 6] = [
            (&["1", "2", "3"], &["1", "2*", "3", "4"]),
            (&["1", "2", "3"], &["0", "1", "3*"]),
            (&["1", "2", "3"], &["1", "2", "3"]),
            (&["1", "2", "3"], &[]),
            (&[], &["1"]),
            (&["1", "2", "2b", "3"], &["1", "9", "3"]),
        ];
        for (old, new) in cases {
            let mut ui = Ui::default();
            ui.last = rows(old);
            ui.messages.set_vec(ui.last.iter().map(|m| to_item(m, &HashMap::new(), &HashMap::new(), &Default::default(), false)).collect::<Vec<_>>());
            let new = rows(new);
            update_rows(&mut ui, &new, false);
            let got: Vec<String> = ui.messages.iter().map(|m| m.text.to_string()).collect();
            let want: Vec<String> = new.iter().map(|m| m.text.clone()).collect();
            assert_eq!(got, want, "{old:?} -> {new:?}");
        }
    }
}
