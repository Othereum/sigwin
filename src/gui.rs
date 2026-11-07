//! UI thread: UI-only state, applying backend `Event`s to the window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use i_slint_core::styled_text::{sigwin_clear_plain_styles, sigwin_set_plain_style, sigwin_styled, SigwinStyle};
use slint::{Color, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use tokio::sync::mpsc::UnboundedSender;

use crate::types::{ClickedMedia, Command, Event, MsgDto, Paging, Scroll, ThreadDto};
use crate::*;

pub(crate) mod anim;
pub(crate) mod drafts;
pub(crate) mod linkcard;
pub(crate) mod notice;
mod rows;
mod scroll;
pub(crate) mod viewer;
pub(crate) mod whatsnew;
mod window;
mod wire;

pub(crate) use rows::*;
pub(crate) use scroll::*;
pub(crate) use window::*;
pub(crate) use wire::*;

/// UI-thread state that isn't a Slint property.
#[derive(Default)]
pub(crate) struct Ui {
    pub(crate) threads: Rc<VecModel<ThreadItem>>,
    /// the backend's last thread list; `threads` is this with drafts merged in
    thread_dtos: Vec<ThreadDto>,
    /// unsent composer contents by thread key (`drafts.enc`)
    pub(crate) drafts: HashMap<String, drafts::Draft>,
    pub(crate) drafts_path: PathBuf,
    pub(crate) messages: Rc<VecModel<MsgItem>>,
    /// thumbnails for the open thread only; cleared on thread switch to bound memory
    thumbs: HashMap<String, Image>,
    /// text over pictures, by the same keys
    media_labels: HashMap<String, types::MediaLabel>,
    /// animated stickers' frames (message list, open thread; the hovered picker sticker)
    anims: anim::Anims,
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
    pub(crate) paste_dir: PathBuf,
    open_key: String,
    loading_older: bool,
    loading_newer: bool,
    /// the rows stop short of the thread's newest message: nothing happening now goes below them
    detached: bool,
    /// an older page waiting for its anchor to be measured (`hold_older`)
    older: Option<(Vec<MsgDto>, Paging)>,
    picking: bool,
    pub(crate) packs: Rc<VecModel<PackItem>>,
    pack_dtos: Vec<types::PackDto>,
    pub(crate) stickers: Rc<VecModel<StickerItem>>,
    grid: Vec<types::StickerDto>,
    /// pack cover images (grid images live only in `stickers` rows); dropped when the panel closes
    sticker_thumbs: HashMap<String, Image>,
    pub(crate) tx: Option<UnboundedSender<Command>>,
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
    pub(crate) settings: settings::Settings,
    pub(crate) settings_path: PathBuf,
    /// messages whose spoilers were clicked open, open thread only
    revealed: std::collections::HashSet<String>,
    /// the window sat hidden in the tray and the open thread's rows and images were dropped;
    /// showing it again reloads the thread (`window::visibility`)
    released: bool,
    pub(crate) link: linkcard::State,
}

pub(crate) fn send_cmd(c: Command) {
    UI.with_borrow(|ui| {
        if let Some(tx) = &ui.tx {
            let _ = tx.send(c);
        }
    });
}

pub(crate) fn close_stickers(app: &AppWindow) {
    app.set_sticker_open(false);
    UI.with_borrow_mut(|ui| {
        anim::stop_picker(ui);
        ui.sticker_thumbs.clear();
        ui.grid.clear();
        ui.stickers.set_vec(Vec::new());
        ui.packs.set_vec(Vec::new());
    });
}

thread_local! {
    pub(crate) static UI: RefCell<Ui> = RefCell::default();
    /// stick_to_bottom
    static TIMER: slint::Timer = slint::Timer::default();
    /// jump_to_row
    static JUMP: slint::Timer = slint::Timer::default();
}

/// The open thread's photo (or initial) in the conversation header, from its thread list row.
pub(crate) fn header_avatar(app: &AppWindow, ui: &Ui) {
    let muted = ui.thread_dtos.iter().find(|t| t.key == ui.open_key).is_some_and(|t| t.mute.active(view::now_ms()));
    app.set_current_muted(muted);
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

/// Unread count for the taskbar and tray badges and the window title; chats with notifications off
/// count only when the setting says so.
pub(crate) fn show_total_unread(app: &AppWindow, ui: &Ui) {
    let now = view::now_ms();
    let all = ui.settings.badge_muted;
    let counts = |t: &&ThreadDto| !t.mute.active(now) || t.prefs.badge.unwrap_or(all);
    let counted = ui.thread_dtos.iter().filter(counts);
    let (total, noun) = if ui.settings.badge_count == 1 {
        (counted.filter(|t| t.unread > 0).count() as u32, "대화")
    } else {
        (counted.map(|t| t.unread).sum(), "메시지")
    };
    app.set_total_unread(total as i32);
    badge::set_count(total, noun);
}

pub(crate) fn qr_image(url: &str) -> Option<Image> {
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

pub(crate) fn apply(app: &AppWindow, ev: Event) {
    match ev {
        Event::Loading(t) => {
            app.set_loading_text(t.into());
            app.set_page("loading".into());
        }
        Event::NeedLink => {
            app.set_has_qr(false);
            app.set_page("link".into());
            // no account, so no sync to wait for
            crate::update::start();
            whatsnew::on_need_link();
        }
        Event::LinkQr(url) => {
            if let Some(img) = qr_image(&url) {
                app.set_qr(img);
                app.set_has_qr(true);
            }
        }
        Event::LinkStatus(s) => app.set_link_status(s.into()),
        Event::Ready => {
            app.set_page("main".into());
            whatsnew::on_ready(app);
        }
        Event::Threads(list) => UI.with_borrow_mut(|ui| {
            ui.thread_dtos = list;
            show_total_unread(app, ui);
            drafts::show_threads(ui);
            header_avatar(app, ui);
        }),
        Event::Header { name, sub } => {
            app.set_current_name(name.into());
            app.set_current_sub(sub.into());
        }
        Event::Members(list) => UI.with_borrow_mut(|ui| ui.members = list),
        Event::Messages { key, items, paging, scroll } => {
            let restored = UI.with_borrow_mut(|ui| {
                let switched = ui.open_key != key;
                let mut restored = None;
                if switched {
                    ui.thumbs.clear();
                    ui.media_labels.clear();
                    ui.anims.clear_list();
                    ui.sender_avatars.clear();
                    ui.revealed.clear();
                    sigwin_clear_plain_styles();
                    // the composer moves into the old thread's draft, the new one's comes back
                    restored = Some(drafts::switch(app, ui, &key));
                    ui.open_key = key.clone();
                    ui.mention_rows.clear();
                    ui.typing.clear();
                }
                restored
            });
            let switched = restored.is_some();
            if let Some(d) = restored {
                drafts::restore(app, d);
                app.set_pin_index(0);
                app.set_flash_id(SharedString::new());
                app.set_menu_id(SharedString::new());
                app.set_mention_items(ModelRc::default());
                close_stickers(app);
                UI.with_borrow(|ui| header_avatar(app, ui));
            }
            app.set_current_key(key.into());
            if !switched && scroll != Scroll::Bottom && UI.with_borrow(|ui| ui.older.is_some()) {
                // an older page is being anchored: show the newest rows when it lands
                UI.with_borrow_mut(|ui| ui.older = Some((items, paging)));
                return;
            }
            UI.with_borrow_mut(|ui| ui.older = None);
            let anchor = UI.with_borrow(|ui| ui.last.first().map(|m| m.id.clone()));
            match anchor {
                Some(anchor) if scroll == Scroll::Older && !switched && app.get_jump_id().is_empty() => {
                    hold_older(app, anchor, items, paging)
                }
                _ => {
                    // the window moved away from what's shown: nothing to keep in place
                    let disjoint = UI.with_borrow(|ui| match (ui.last.first(), items.first()) {
                        (Some(old), Some(new)) => !items.iter().any(|m| m.id == old.id) && !ui.last.iter().any(|m| m.id == new.id),
                        _ => false,
                    });
                    show_rows(app, items, paging, scroll, switched || disjoint)
                }
            }
            viewer::close_if_gone();
        }
        Event::NewBelow => app.set_new_below(app.get_new_below() + 1),
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
            if t.key == avatar::ME_SIDEBAR {
                app.set_me_has_photo(img.is_some());
                app.set_me_photo(img.unwrap_or_default());
                return;
            }
            if t.key == avatar::ME_CARD {
                app.set_account_has_photo(img.is_some());
                app.set_account_photo(img.unwrap_or_default());
                return;
            }
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
        Event::StickerThumbs(list) => {
            if !app.get_sticker_open() {
                return;
            }
            UI.with_borrow_mut(|ui| {
                let mut fresh = HashMap::with_capacity(list.len());
                for t in list {
                    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height);
                    fresh.insert(t.key, Image::from_rgba8(buf));
                }
                for (i, p) in ui.pack_dtos.iter().enumerate() {
                    if let Some(img) = fresh.get(&p.cover_key) {
                        ui.sticker_thumbs.insert(p.cover_key.clone(), img.clone());
                        slint::Model::set_row_data(&*ui.packs, i, pack_item(p, &ui.sticker_thumbs));
                    }
                }
                // grid images live only in the model rows; ones for a pack no longer shown are
                // dropped here (the backend sends the grid before its images)
                for (i, s) in ui.grid.iter().enumerate() {
                    if let Some(img) = fresh.get(&s.key) {
                        if let Some(mut row) = slint::Model::row_data(&*ui.stickers, i) {
                            row.img = img.clone();
                            slint::Model::set_row_data(&*ui.stickers, i, row);
                        }
                    }
                }
            });
        }
        Event::LinkPreview { url, card } => linkcard::fetched(app, &url, card),
        Event::Thumb(_) if UI.with_borrow(|ui| ui.released) => {}
        Event::Thumb(t) => {
            let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height);
            let img = Image::from_rgba8(buf);
            UI.with_borrow_mut(|ui| {
                ui.thumbs.insert(t.key.clone(), img.clone());
                for (i, m) in ui.last.iter().enumerate() {
                    let cell = m.cells.iter().position(|c| c.key == t.key);
                    let link = m.link.as_ref().is_some_and(|l| l.image_key == t.key);
                    if cell.is_none() && !link && m.image_key != t.key {
                        continue;
                    }
                    let Some(mut row) = slint::Model::row_data(&*ui.messages, i) else { continue };
                    if link {
                        row.link_image = img.clone();
                    } else if let Some(n) = cell {
                        // a new model, not a change inside the old one: a ListView row didn't
                        // show that until it was scrolled away and back
                        let mut cells: Vec<ImageCell> = slint::Model::iter(&row.cells).collect();
                        cells[n].image = img.clone();
                        row.cells = ModelRc::new(VecModel::from(cells));
                    } else {
                        row.image = img.clone();
                    }
                    slint::Model::set_row_data(&*ui.messages, i, row);
                }
            });
        }
        Event::MediaLabel { .. } if UI.with_borrow(|ui| ui.released) => {}
        Event::MediaLabel { key, label } => {
            UI.with_borrow_mut(|ui| {
                let shown: SharedString = label.text.as_str().into();
                let undownloaded = label.undownloaded;
                let progress = rows::progress_of(&label);
                let ring = rows::ring_of(&label);
                if label == types::MediaLabel::default() {
                    ui.media_labels.remove(&key);
                } else {
                    ui.media_labels.insert(key.clone(), label);
                }
                for (i, m) in ui.last.iter().enumerate() {
                    let cell = m.cells.iter().position(|c| c.key == key);
                    if cell.is_none() && (m.image_key != key || m.sticker) {
                        continue;
                    }
                    let Some(mut row) = slint::Model::row_data(&*ui.messages, i) else { continue };
                    if let Some(n) = cell {
                        // a new model, as for `Thumb`
                        let mut cells: Vec<ImageCell> = slint::Model::iter(&row.cells).collect();
                        cells[n].label = shown.clone();
                        cells[n].undownloaded = undownloaded;
                        cells[n].progress = progress;
                        cells[n].ring = ring.clone();
                        row.cells = ModelRc::new(VecModel::from(cells));
                    } else {
                        row.media_label = shown.clone();
                        row.undownloaded = undownloaded;
                        row.progress = progress;
                        row.ring = ring.clone();
                    }
                    slint::Model::set_row_data(&*ui.messages, i, row);
                }
            });
        }
        Event::ViewerImage(v) => viewer::arrived(app, v),
        Event::ViewerThumb(id, index, t) => viewer::thumb_arrived(&id, index, t),
        Event::Frames(f) => anim::frames_arrived(f),
        Event::Notice(s) => notice::show(app, &s),
        Event::Progress(s) => notice::progress(app, &s),
        Event::ProgressDone => notice::progress_done(app),
        Event::Connection { text, at_once } => notice::connection(app, text, at_once),
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
            // the placeholder when there is no photo: the profile name's first letter
            let letter = if name.is_empty() { "나".to_string() } else { initial(&name) };
            app.set_account_initial(letter.into());
            app.set_account_tint(tint("me"));
            app.set_account_name(name.into());
            app.set_account_number(number.into());
            app.set_account_device(device.into());
            app.set_account_phone(phone.into());
        }
        Event::Storage { db, attachments, thumbs } => {
            app.set_storage_text(
                format!("메시지 DB {}  ·  첨부파일 {}  ·  썸네일 캐시 {}", view::size_text(db), view::size_text(attachments), view::size_text(thumbs)).into(),
            );
        }
        Event::StickerGrid { pack, items } => {
            if !app.get_sticker_open() {
                return;
            }
            app.set_current_pack(pack.into());
            UI.with_borrow_mut(|ui| {
                anim::stop_picker(ui);
                let rows: Vec<StickerItem> = items
                    .iter()
                    .map(|s| StickerItem {
                        id: s.id as i32,
                        emoji: s.emoji.as_str().into(),
                        animated: s.animated,
                        ..Default::default()
                    })
                    .collect();
                ui.stickers.set_vec(rows);
                ui.grid = items;
            });
        }
    }
}

