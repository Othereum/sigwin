//! UI thread: UI-only state, applying backend `Event`s to the window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use i_slint_core::styled_text::{sigwin_clear_plain_styles, sigwin_set_plain_style, sigwin_styled, SigwinStyle};
use slint::{Color, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use tokio::sync::mpsc::UnboundedSender;

use crate::types::{ClickedMedia, Command, Event, Find, MsgDto, Paging, ResultDto, Scroll, ThreadDto};
use crate::{autostart, avatar, backend, badge, clip, emoji, format, headless, link, mention, picker, settings, types, update, view, winhook};
use sigwin_ui::{
    AppWindow, ImageCell, ImageViewer, Links, MediaState, MentionItem, MsgItem, PackItem, PendingFile, PreviewItem, PinItem, ReactionItem, ReaderItem, RepliesData, ReplyItem, RequestData,
    FindKind, PackCell, PackData, ResultItem, ResultKind, SearchData, SettingsData, StickerData, StickerItem, TextOps, Theme, ThreadItem, ThreadMenuData,
};

pub(crate) mod anim;
pub(crate) mod attach;
mod calendar;
pub(crate) mod drafts;
pub(crate) mod gallery;
mod held;
pub(crate) mod linkcard;
mod media;
pub(crate) mod notice;
pub(crate) mod packmaker;
pub(crate) mod presence;
mod rows;
mod scenario;
mod scroll;
pub(crate) mod stickers;
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
    /// the badges and title keep their count meanwhile (`Event::CatchingUp`)
    catching_up: bool,
    /// the sidebar's search or contact list, while one is on
    results: Rc<VecModel<ResultItem>>,
    result_dtos: Vec<ResultDto>,
    /// ↑/↓ in the search box opened another chat: the focus stays in the box, to go on
    stepping: bool,
    /// unsent composer contents by thread key (`drafts.enc`)
    pub(crate) drafts: HashMap<String, drafts::Draft>,
    pub(crate) drafts_path: PathBuf,
    pub(crate) messages: Rc<VecModel<MsgItem>>,
    /// thumbnails for the open thread only; cleared on thread switch to bound memory
    thumbs: held::Pictures<{ held::THUMBS }>,
    /// rows on screen, and the pictures off it let go of (`media::row_shown`)
    shown: media::Shown,
    /// text over pictures, by the same keys
    media_labels: rows::MediaStates,
    /// animated stickers' frames (message list, open thread; the hovered picker sticker)
    anims: anim::Anims,
    /// round photos by thread key, for the whole session (~6KB each at 40px)
    avatars: held::Pictures<{ held::AVATARS }>,
    /// photos next to group messages by sender uuid, open thread only
    sender_avatars: held::Pictures<{ held::SENDER_AVATARS }>,
    last: Vec<MsgDto>,
    /// the theme the rows were last drawn for: what arrives late draws its row the same way
    dark: bool,
    /// the furthest row of the open thread reported seen (`ReadUpTo`), to send only what's new
    read_sent: Option<String>,
    files: Vec<PathBuf>,
    /// the server's attachment size limit; None: the default (`attach::admit`)
    send_limit: Option<u64>,
    /// pictures of `files` for the composer: None while being cut (or when there is none)
    pending_thumbs: held::ImageMap<PathBuf, Option<Image>, { held::COMPOSER }>,
    /// the GIF playing in a composer tile: (playback number, file), and the last number used
    pending_gif: Option<(u64, PathBuf)>,
    pending_gif_seq: u64,
    reply_to: Option<String>,
    /// id of our message being edited in the composer
    editing: Option<String>,
    open_key: String,
    /// where the user left each thread not at its bottom: (row id, the row's top px from the
    /// list's top). In memory only, for this run.
    places: HashMap<String, (String, f32)>,
    loading_older: bool,
    loading_newer: bool,
    /// the rows stop short of the thread's newest message: nothing happening now goes below them
    detached: bool,
    /// an older page waiting for its anchor to be measured (`hold_older`)
    older: Option<(Vec<MsgDto>, Paging)>,
    picking: bool,
    pub(crate) sticker: stickers::Panel,
    pub(crate) maker: packmaker::State,
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

/// Gives the window the UI state's models, and starts the timer that ages their times.
pub(crate) fn bind_models(app: &AppWindow) {
    let age_timer = Box::leak(Box::new(slint::Timer::default()));
    age_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(30), || {
        UI.with_borrow(|ui| {
            refresh_times(ui);
            drafts::refresh_draft_times(ui);
        });
    });
    UI.with_borrow(|ui| {
        app.set_threads(ModelRc::from(ui.threads.clone()));
        app.global::<SearchData>().set_results(ModelRc::from(ui.results.clone()));
        app.set_messages(ModelRc::from(ui.messages.clone()));
        app.global::<StickerData>().set_packs(ModelRc::from(ui.sticker.packs.clone()));
        app.global::<StickerData>().set_stickers(ModelRc::from(ui.sticker.stickers.clone()));
        app.global::<PackData>().set_cells(ModelRc::from(ui.maker.cells.clone()));
    });
}

/// Scenario steps (headless with `SIGWIN_FIXTURE`) need the window.
pub(crate) fn scenario_install(app: &AppWindow) {
    scenario::install(app);
}

pub(crate) fn send_cmd(c: Command) {
    headless::bump();
    UI.with_borrow(|ui| {
        if let Some(tx) = &ui.tx {
            let _ = tx.send(c);
        }
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
    let muted = ui.thread_dtos.iter().find(|t| t.key == ui.open_key).is_some_and(|t| t.mute.active(view::time::now_ms()));
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
        app.set_current_photo_small(Image::default());
        return;
    };
    app.set_current_initial(row.initial);
    app.set_current_tint(row.color);
    app.set_current_has_photo(row.has_photo);
    app.set_current_photo_small(if row.has_photo { shrunk(&row.photo, SCOPE_PHOTO) } else { Image::default() });
    app.set_current_photo(row.photo);
}

/// The photo in the search box's "searching in" token (`ScopeToken`), logical px.
const SCOPE_PHOTO: u32 = 20;

/// `photo` resampled to `size`: drawn smaller as it is, it would be sampled nearest-neighbor.
fn shrunk(photo: &Image, size: u32) -> Image {
    let Some(buf) = photo.to_rgba8() else { return Image::default() };
    let Some(img) = image::RgbaImage::from_raw(buf.width(), buf.height(), buf.as_bytes().to_vec()) else { return Image::default() };
    let small = sigwin_thumb::fit(img, types::physical_px(size));
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(small.as_raw(), small.width(), small.height()))
}

/// The open thread's unread count, on the jump button.
pub(crate) fn show_open_unread(app: &AppWindow, ui: &Ui) {
    let n = ui.thread_dtos.iter().find(|t| t.key == ui.open_key).map_or(0, |t| t.unread);
    app.set_open_unread(n as i32);
}

/// Unread count for the taskbar and tray badges and the window title; chats with notifications off
/// count only when the setting says so. The badges go grey when nothing counted would have
/// notified.
pub(crate) fn show_total_unread(app: &AppWindow, ui: &Ui) {
    if ui.catching_up {
        return;
    }
    let now = view::time::now_ms();
    let all = ui.settings.badge_muted;
    let counts = |t: &&ThreadDto| !t.mute.active(now) || t.prefs.badge.unwrap_or(all);
    let counted = || ui.thread_dtos.iter().filter(counts);
    let (total, chats) = if ui.settings.badge_count == 1 {
        (counted().filter(|t| t.unread > 0).count() as u32, true)
    } else {
        (counted().map(|t| t.unread).sum(), false)
    };
    let quiet = !counted().any(|t| t.unread > 0 && (t.loud || !t.mute.active(now)));
    app.set_total_unread(total as i32);
    badge::set_count(total, chats, quiet);
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
        }
        Event::LinkQr(url) => {
            if let Some(img) = qr_image(&url) {
                app.set_qr(img);
                app.set_has_qr(true);
            }
        }
        Event::LinkStatus(s) => app.set_link_status(s.into()),
        Event::HistorySkippable(on) => app.set_history_skippable(on),
        Event::Ready => app.set_page("main".into()),
        Event::Threads(list) => UI.with_borrow_mut(|ui| {
            ui.thread_dtos = list;
            show_open_unread(app, ui);
            show_total_unread(app, ui);
            drafts::show_threads(ui);
            header_avatar(app, ui);
        }),
        Event::Results(list) => UI.with_borrow_mut(|ui| {
            // a late answer to a search the user already left
            if !app.global::<SearchData>().get_active() {
                return;
            }
            let items: Vec<ResultItem> = list.iter().map(|r| result_item(r, &ui.avatars)).collect();
            // row by row, not `set_vec`: that would scroll the list back to the top when a
            // page of found messages is added below
            use slint::Model;
            let shared = ui.results.row_count().min(items.len());
            for (i, item) in items.iter().enumerate().take(shared) {
                ui.results.set_row_data(i, item.clone());
            }
            while ui.results.row_count() > items.len() {
                ui.results.remove(ui.results.row_count() - 1);
            }
            for item in items.into_iter().skip(shared) {
                ui.results.push(item);
            }
            ui.result_dtos = list;
        }),
        Event::ThreadMoved { from, to } => UI.with_borrow_mut(|ui| {
            drafts::rekey(ui, &from, &to);
            ui.places.remove(&from);
            // the composer stays as it is: the messages for `to` don't count as a switch
            if ui.open_key == from {
                ui.open_key = to.clone();
                app.set_current_key(to.into());
            }
        }),
        Event::SendLimit(limit) => UI.with_borrow_mut(|ui| ui.send_limit = limit),
        Event::RecentReactions(list) => {
            let list: Vec<SharedString> = list.iter().map(|e| e.as_str().into()).collect();
            app.set_recent_emoji(ModelRc::new(VecModel::from(list)));
        }
        Event::Header { name, sub, request, common_groups } => {
            app.set_current_name(name.into());
            app.set_current_sub(sub.into());
            app.set_current_request(request);
            app.global::<RequestData>().set_common_groups(common_groups.min(i32::MAX as usize) as i32);
        }
        Event::Members(list) => UI.with_borrow_mut(|ui| ui.members = list),
        Event::Messages { key, items, paging, scroll } => messages_arrived(app, key, items, paging, scroll),
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
        Event::Replies(list) => {
            let items: Vec<ReplyItem> = list
                .iter()
                .map(|r| ReplyItem {
                    id: r.id.as_str().into(),
                    sender: r.sender.as_str().into(),
                    text: r.text.as_str().into(),
                    time: r.time.as_str().into(),
                })
                .collect();
            let d = app.global::<RepliesData>();
            d.set_items(ModelRc::new(VecModel::from(items)));
            d.set_open(true);
        }
        Event::JumpTo(id) => jump_to_row(app, id),
        Event::JumpToDay(id) => jump_to_day(app, id),
        Event::CalendarMarks { key, bounds, marks, first } => calendar::marks_arrived(app, key, bounds, marks, first),
        Event::JumpToUnread(id) => jump_to_unread(app, id),
        Event::Avatar(t) => media::avatar_arrived(app, t),
        Event::StickerThumbs(list) => stickers::thumbs_arrived(app, list),
        Event::LinkPreview { url, card } => linkcard::fetched(app, &url, card),
        Event::Thumb(t) if t.key.starts_with(types::GALLERY_PREFIX) => gallery::thumb_arrived(t),
        Event::Thumb(t) => media::thumb_arrived(t),
        Event::MediaLabel { key, label } if key.starts_with(types::GALLERY_PREFIX) => gallery::label_arrived(key, label),
        Event::MediaLabel { key, label } => media::label_arrived(key, label),
        Event::Gallery { files, items, more, reset, indexing } => gallery::arrived(app, files, items, more, reset, indexing),
        Event::ViewerImage(v) => viewer::arrived(app, v),
        Event::ViewerThumb(id, index, t) => viewer::thumb_arrived(&id, index, t),
        Event::Frames(f) => anim::frames_arrived(f),
        Event::StickerAnimated(key) => anim::sticker_animated(&key),
        Event::Gif(slot) if slot.target == types::GifTarget::Composer => attach::gif_arrived(app, &slot),
        Event::Gif(slot) => anim::gif_arrived(slot),
        Event::Notice(s) => notice::show(app, &s),
        Event::Saved(s, path) => notice::saved(app, &s, path),
        Event::Progress(s) => notice::progress(app, &s),
        Event::ProgressDone => notice::progress_done(app),
        Event::Connection { text, at_once, busy } => notice::connection(app, text, at_once, busy),
        Event::CatchingUp(on) => UI.with_borrow_mut(|ui| {
            ui.catching_up = on;
            show_total_unread(app, ui);
        }),
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
        Event::StickerPacks { packs, recent } => stickers::packs_arrived(app, packs, recent),
        Event::OpenStickerPanel { pack, sticker } => stickers::open_at(app, pack, sticker),
        Event::PackPreview(v) => stickers::preview_arrived(app, v),
        Event::PackAdding { pack, done, total } => {
            let progress = if total == 0 { 0.0 } else { done as f32 / total as f32 };
            stickers::add_progress(app, &pack, progress);
            packmaker::add_progress(app, &pack, progress);
        }
        Event::PackAdded { pack, ok } => {
            stickers::add_ended(app, &pack, ok);
            packmaker::add_ended(app, &pack, ok);
        }
        Event::PreviewSticker { key, animated, failed } => stickers::preview_sticker(&key, animated, failed),
        Event::Account { name, number, device, receipts, typing } => {
            let state = |v: Option<bool>| if v == Some(false) { tr!("Off") } else { tr!("On") };
            let phone = if receipts.is_none() || typing.is_none() {
                tr!("{} · {} — follows the Signal app on your phone (not received yet)", state(receipts), state(typing))
            } else {
                tr!("{} · {} — follows the Signal app on your phone", state(receipts), state(typing))
            };
            // the placeholder when there is no photo: the profile name's first letter
            let letter = initial(if name.is_empty() { tr!("You") } else { &name });
            let d = app.global::<SettingsData>();
            d.set_account_initial(letter.into());
            d.set_account_tint(tint("me"));
            d.set_account_name(name.into());
            d.set_account_number(number.into());
            d.set_account_device(device.into());
            d.set_account_phone(phone.into());
        }
        Event::Storage { db, attachments, thumbs } => {
            app.global::<SettingsData>().set_storage(
                tr!("Message DB {}  ·  Attachments {}  ·  Thumbnail cache {}", view::media::size_text(db), view::media::size_text(attachments), view::media::size_text(thumbs)).into(),
            );
        }
        Event::StickerGrid { pack, items } => stickers::grid_arrived(app, pack, items),
        Event::PackProgress { done, total } => packmaker::progress(app, done, total),
        Event::PackCreated { id, key } => packmaker::created(app, id, key),
        Event::PackFailed(why) => packmaker::failed(app, why),
    }
}

/// A page of the open thread's rows; a different thread than before switches to it.
fn messages_arrived(app: &AppWindow, key: String, items: Vec<MsgDto>, paging: Paging, scroll: Scroll) {
    let restored = UI.with_borrow_mut(|ui| {
        let switched = ui.open_key != key;
        let mut restored = None;
        if switched {
            ui.thumbs.clear();
            ui.media_labels.clear();
            anim::stop_gif(ui);
            ui.anims.clear_list();
            ui.sender_avatars.clear();
            ui.revealed.clear();
            sigwin_clear_plain_styles();
            remember_place(app, ui);
            // the composer moves into the old thread's draft, the new one's comes back
            restored = Some(drafts::switch(app, ui, &key));
            ui.open_key = key.clone();
            ui.read_sent = None;
            presence::thread_switched();
            show_open_unread(app, ui);
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
        stickers::close(app);
        gallery::close(app);
        calendar::close(app);
        UI.with_borrow(|ui| header_avatar(app, ui));
    }
    app.set_current_key(key.into());
    if !switched && !matches!(scroll, Scroll::Bottom | Scroll::Unread) && UI.with_borrow(|ui| ui.older.is_some()) {
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
