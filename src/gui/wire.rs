//! Slint callbacks -> `Command`s, and the native window hook.

use super::*;

pub(crate) fn wire(app: &AppWindow, tx: UnboundedSender<Command>) {
    let send = move |c: Command| {
        headless::bump();
        let _ = tx.send(c);
    };
    wire_threads(app, &send);
    wire_composer(app, &send);
    wire_messages(app, &send);
    wire_stickers(app, &send);
    wire_settings(app, &send);
    wire_update(app);
    anim::wire(app);
    whatsnew::wire(app);
    if !headless::active() {
        // no native window in headless; started when the window is first shown
        set_starter(hook_window);
    }
}

/// A square cut of a message's photo at the reply box's size (nearest-neighbor scaling is jagged,
/// so it's resampled once here). None when the pixels can't be read.
fn reply_thumb(src: &Image) -> Option<Image> {
    let buf = src.to_rgba8()?;
    let (w, h) = (buf.width(), buf.height());
    let img = image::RgbaImage::from_raw(w, h, buf.as_bytes().to_vec())?;
    let side = w.min(h);
    let square = image::imageops::crop_imm(&img, (w - side) / 2, (h - side) / 2, side, side).to_image();
    let size = types::physical_px(view::QUOTE_IMAGE.0);
    let small = image::imageops::resize(&square, size, size, image::imageops::FilterType::Lanczos3);
    Some(Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(small.as_raw(), size, size)))
}

/// Linking, the thread list, search and paging.
fn wire_threads(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
    let s = send.clone();
    app.on_start_link(move |name| s(Command::StartLink(name.to_string())));

    let s = send.clone();
    app.on_open_thread(move |key| {
        let key = key.to_string();
        // clicking the open thread again starts over at the bottom, as before
        let place = UI.with_borrow(|ui| if ui.open_key == key { None } else { ui.places.get(&key).map(|p| p.0.clone()) });
        if let Some(id) = place {
            s(Command::RestoreAt(id));
        }
        s(Command::OpenThread(key));
    });

    let s = send.clone();
    let weak = app.as_weak();
    app.on_jump_to_bottom(move || {
        if let Some(app) = weak.upgrade() {
            // unread messages: the backend knows where they start
            if app.get_open_unread() > 0 {
                s(Command::UnreadButton);
            } else if app.get_can_load_newer() {
                s(Command::ToLatest);
            } else {
                stick_to_bottom(&app);
            }
        }
    });

    let s = send.clone();
    let weak = app.as_weak();
    app.on_load_newer(move || {
        // not while a jump or an older page is settling
        if weak.upgrade().is_some_and(|a| !a.get_jump_id().is_empty()) {
            return;
        }
        let go = UI.with_borrow_mut(|ui| !std::mem::replace(&mut ui.loading_newer, true));
        if go {
            s(Command::LoadNewer);
        }
    });

    let s = send.clone();
    app.on_at_bottom_changed(move |at| s(Command::AtBottom(at)));

    let weak = app.as_weak();
    app.on_row_seen(move |id| {
        // not while a jump is settling: the rows on the way were never looked at
        if weak.upgrade().is_none_or(|a| !a.get_jump_id().is_empty() || a.get_reading_hold()) {
            return;
        }
        window::row_seen(&id);
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
    app.on_search_changed(move |q| s(Command::Search(q.to_string())));

    let weak = app.as_weak();
    app.global::<ThreadMenuData>().on_open(move |key, x, y| {
        let Some(app) = weak.upgrade() else { return };
        let Some(mute) = UI.with_borrow(|ui| ui.thread_dtos.iter().find(|t| t.key == key.as_str()).map(|t| t.mute)) else {
            return;
        };
        let now = view::time::now_ms();
        let d = app.global::<ThreadMenuData>();
        d.set_status(if mute.active(now) { types::mute_text(mute.until, now).into() } else { SharedString::new() });
        d.set_overridden(mute.overridden);
        d.set_phone(format!("휴대폰: {}", types::mute_text(mute.phone, now)).into());
        d.set_menu_x(x);
        d.set_menu_y(y);
        d.set_key(key);
    });

    let s = send.clone();
    app.global::<ThreadMenuData>().on_set_mute(move |key, secs| {
        let set = match secs {
            -2 => types::MuteSet::Phone,
            -1 => types::MuteSet::Always,
            0 => types::MuteSet::On,
            n => types::MuteSet::For(n.max(1) as u64),
        };
        s(Command::SetMute { key: key.to_string(), set });
    });

    let weak = app.as_weak();
    app.global::<ThreadMenuData>().on_open_prefs(move |key| {
        let Some(app) = weak.upgrade() else { return };
        let found = UI.with_borrow(|ui| {
            let t = ui.thread_dtos.iter().find(|t| t.key == key.as_str())?;
            Some((t.name.clone(), t.prefs, t.phone_mentions, ui.settings.clone()))
        });
        let Some((name, p, phone_mentions, s)) = found else { return };
        let on_off = |b: bool| if b { "켬" } else { "끔" };
        let index = |b: Option<bool>| match b {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        let preview = ["이름과 내용", "이름만", "숨김"][s.preview.min(2) as usize];
        let d = app.global::<ThreadMenuData>();
        d.set_prefs_name(name.into());
        d.set_prefs_preview(p.preview.map_or(0, |v| v.min(2) as i32 + 1));
        d.set_prefs_sound(index(p.sound));
        d.set_prefs_sound_default(on_off(s.sound).into());
        d.set_prefs_mentions(index(p.mentions));
        d.set_prefs_replies(index(p.replies));
        d.set_prefs_badge(index(p.badge));
        d.set_prefs_preview_default(if s.notify { preview.into() } else { "알림 꺼짐 (앱 설정)".into() });
        d.set_prefs_mentions_default(match phone_mentions {
            Some(b) => format!("{} (휴대폰 설정)", on_off(b)).into(),
            None => on_off(s.muted_mentions).into(),
        });
        d.set_prefs_replies_default(on_off(s.muted_replies).into());
        d.set_prefs_badge_default(on_off(s.badge_muted).into());
        d.set_prefs_key(key);
    });

    let s = send.clone();
    app.global::<ThreadMenuData>().on_set_pref(move |key, item, v| {
        let b = match v {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        };
        let pref = match item {
            0 => types::Pref::Preview((v > 0).then(|| (v - 1).clamp(0, 2) as u8)),
            1 => types::Pref::Mentions(b),
            2 => types::Pref::Replies(b),
            4 => types::Pref::Sound(b),
            _ => types::Pref::Badge(b),
        };
        s(Command::SetPref { key: key.to_string(), pref });
    });
}

/// Composer: sending, attachments, paste, @mentions, formatting, reply.
fn wire_composer(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
    // the composer text before the last edit, to find what an edit inserted
    let prev = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let weak = app.as_weak();
    app.on_clamp_draft(move |text| {
        let mut prev = prev.borrow_mut();
        let Some((fit, cursor)) = crate::view::text::fit_edit(&prev, &text, crate::view::text::MAX_LONG_TEXT_BYTES) else {
            *prev = text.to_string();
            return -1;
        };
        if let Some(app) = weak.upgrade() {
            app.set_draft(fit.as_str().into());
            notice::show(&app, "메시지는 64KB까지 입력할 수 있습니다");
        }
        *prev = fit;
        cursor as i32
    });

    let weak = app.as_weak();
    app.on_link_check(move || {
        if let Some(app) = weak.upgrade() {
            linkcard::edited(&app);
        }
    });

    let weak = app.as_weak();
    app.on_link_dismiss(move || {
        if let Some(app) = weak.upgrade() {
            linkcard::dismiss(&app);
        }
    });

    let s = send.clone();
    let weak = app.as_weak();
    app.on_send(move |text| {
        let Some(app) = weak.upgrade() else { return };
        // before anything is cleared: the text stays in the composer to shorten
        // (mentions and format markers only shrink it, so this never refuses what would fit)
        if text.len() > crate::view::text::MAX_LONG_TEXT_BYTES {
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("메시지가 너무 깁니다")
                .set_description(format!(
                    "한 메시지는 64KB까지 보낼 수 있습니다. 지금 {}KB입니다.\n나눠서 보내 주세요.",
                    text.len().div_ceil(1024)
                ))
                .show();
            return;
        }
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
        // `files` were taken above: with attachments there is no preview
        let link = linkcard::take_for_send(&app, if files.is_empty() { &text } else { "" });
        let quality = sigwin_thumb::photo::Quality::from_index(UI.with_borrow(|ui| ui.settings.photo_quality));
        s(Command::Send { text: text.to_string(), files, quote, mentions, link, quality });
        app.set_mention_items(ModelRc::default());
        app.set_draft(SharedString::new());
        app.set_pending_files(ModelRc::default());
        app.set_pending_has_photo(false);
        UI.with_borrow_mut(|ui| ui.pending_thumbs.clear());
        app.set_reply_preview(SharedString::new());
        drafts::drop_if_empty(&app);
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
                // encoding a large screenshot takes a moment: keep the UI responsive
                std::thread::spawn(move || {
                    let res = clip::save_image(&dir);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        match res {
                            Ok(p) => add_files(&app, vec![p]),
                            Err(e) => notice::show(&app, &format!("붙여넣기 실패: {e:#}")),
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
    let weak = app.as_weak();
    app.on_composing(move |active| {
        if UI.with_borrow(|ui| ui.editing.is_none()) {
            s(Command::Composing(active));
        }
        if let Some(app) = weak.upgrade().filter(|_| !active) {
            drafts::drop_if_empty(&app);
        }
    });

    let weak = app.as_weak();
    app.on_remove_attachment(move |i| {
        if let Some(app) = weak.upgrade() {
            attach::remove(&app, i.max(0) as usize);
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
                if m.sticker {
                    "스티커".to_string()
                } else if !m.attachment.is_empty() {
                    m.attachment.clone()
                } else if m.video {
                    "동영상".to_string()
                } else if !m.image_key.is_empty() {
                    "사진".to_string()
                } else {
                    "(첨부파일)".to_string()
                }
            } else {
                view::text::mask_spoilers(m.text.clone(), &m.styles).replace('\n', " ")
            };
            let key = if !m.image_key.is_empty() { Some(m.image_key.as_str()) } else { m.cells.first().map(|c| c.key.as_str()) };
            let thumb = key.filter(|_| !m.sticker).and_then(|k| ui.thumbs.get(k)).and_then(reply_thumb);
            Some((if who.is_empty() { body } else { format!("{who}\n{body}") }, thumb))
        });
        if let Some((p, thumb)) = preview {
            drafts::set_reply(&app, &p);
            if let Some(t) = thumb {
                app.set_reply_image(t);
                app.set_reply_has_image(true);
            }
            app.invoke_focus_composer();
        }
    });

    let weak = app.as_weak();
    app.on_cancel_reply(move || {
        UI.with_borrow_mut(|ui| ui.reply_to = None);
        if let Some(app) = weak.upgrade() {
            app.set_reply_preview(SharedString::new());
            drafts::drop_if_empty(&app);
        }
    });
}

/// Message actions: menu, reactions, edit, pin, delete, attachments, links.
fn wire_messages(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
    let weak = app.as_weak();
    app.on_open_menu(move |id, x, y| {
        let Some(app) = weak.upgrade() else { return };
        let Some((mine, can_edit, can_delete, pinned, info)) = UI.with_borrow(|ui| {
            let m = ui.last.iter().find(|m| m.id == id.as_str())?;
            (!m.system && !m.deleted && !m.pending).then(|| (m.my_reaction.clone(), m.can_edit, m.can_delete, m.pinned, m.receipt_info.clone()))
        }) else {
            return;
        };
        app.set_menu_info(info.into());
        app.set_menu_pinned(pinned);
        app.set_menu_pin_step(false);
        app.set_menu_picker(false);
        app.set_menu_mine(mine.into());
        app.set_menu_can_edit(can_edit);
        app.set_menu_can_delete(can_delete);
        app.set_menu_x(x);
        app.set_menu_y(y);
        app.set_menu_id(id);
    });

    // the emoji grid exists only while the picker is open: built per tab, dropped on close
    let weak = app.as_weak();
    app.on_picker_tab_changed(move |tab| {
        let Some(app) = weak.upgrade() else { return };
        let all = emoji::picker_tab(tab.max(0) as usize);
        let rows: Vec<ModelRc<SharedString>> = all
            .chunks(emoji::PICKER_COLUMNS)
            .map(|row| ModelRc::new(VecModel::from(row.iter().map(|e| SharedString::from(*e)).collect::<Vec<_>>())))
            .collect();
        app.set_picker_rows(ModelRc::new(VecModel::from(rows)));
    });
    let weak = app.as_weak();
    app.on_picker_closed(move || {
        if let Some(app) = weak.upgrade() {
            app.set_picker_rows(ModelRc::default());
        }
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
        // an edit keeps no preview: the bar of what was being written goes
        linkcard::update(&app);
        app.invoke_focus_composer();
        app.invoke_composer_to_end();
    });

    let weak = app.as_weak();
    app.on_edit_last(move || {
        let Some(app) = weak.upgrade() else { return false };
        // a reply or attachments being written count as a draft, an edit in progress stays
        let id = UI.with_borrow(|ui| {
            if ui.editing.is_some() || ui.reply_to.is_some() || !ui.files.is_empty() {
                return None;
            }
            ui.last.iter().rev().find(|m| m.can_edit).map(|m| m.id.clone())
        });
        let Some(id) = id else { return false };
        app.invoke_edit_message(id.into());
        true
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
    // the paged demo (SIGWIN_DEMO_OLDER) pages in the target like the backend does
    let demo = std::env::var_os("SIGWIN_DEMO").is_some() && std::env::var_os("SIGWIN_DEMO_OLDER").is_none();
    app.on_show_message(move |id| match weak.upgrade() {
        // no backend: demo pin and quote ids are row ids
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

    let s = send.clone();
    let weak = app.as_weak();
    app.on_attachment_clicked(move |id, index| {
        let row = UI.with_borrow(|ui| ui.last.iter().find(|m| m.id == id.as_str()).map(|m| (m.sticker, m.clicked_media(index as usize))));
        match row {
            Some((true, _)) => s(Command::StickerClicked(id.to_string())),
            Some((_, ClickedMedia::Video(index))) => s(Command::OpenVideo { id: id.to_string(), index }),
            Some((_, ClickedMedia::Image(index))) => {
                if let Some(app) = weak.upgrade() {
                    viewer::open(&app, id.to_string(), index);
                }
            }
            None => {}
        }
    });

    app.on_notice_action_clicked(notice::reveal);

    let weak = app.as_weak();
    app.on_file_clicked(move |id| {
        let Some(app) = weak.upgrade() else { return };
        let danger = UI.with_borrow(|ui| ui.last.iter().any(|m| m.id == id.as_str() && m.file_danger));
        if danger {
            let ok = rfd::MessageDialog::new()
                .set_title("위험할 수 있는 파일")
                .set_description("이런 종류의 파일은 컴퓨터에 해를 끼칠 수 있습니다. 보낸 사람을 믿을 수 있을 때만 저장하세요. 저장할까요?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            if ok != rfd::MessageDialogResult::Yes {
                return;
            }
        }
        send_cmd(Command::SaveFiles { id: id.to_string(), owner: window::hwnd(&app) });
    });

    let weak = app.as_weak();
    let s = send.clone();
    app.global::<Links>().on_open(move |url| {
        if let Some(id) = url.strip_prefix(SPOILER_URL) {
            reveal_spoilers(&weak.unwrap(), id);
        } else if url == crate::notices::URL {
            crate::notices::open(&crate::data_dir());
        } else if let Some(id) = url.strip_prefix(MORE_URL) {
            s(Command::ReadMore(id.to_string()));
        } else if let Some(id) = url.strip_prefix(RETRY_URL) {
            s(Command::RetrySend(id.to_string()));
        } else if let Some(id) = url.strip_prefix(DISCARD_URL) {
            s(Command::DiscardFailed(id.to_string()));
        } else {
            link::open(&url);
        }
    });
}

/// Sticker panel and packs.
fn wire_stickers(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
    let s = send.clone();
    let weak = app.as_weak();
    app.on_toggle_stickers(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.get_sticker_open() {
            stickers::close(&app);
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
            stickers::close(&app);
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
}

/// Settings page and unlinking.
fn wire_settings(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
    let s = send.clone();
    app.on_unlink(move || {
        let ok = rfd::MessageDialog::new()
            .set_title("데이터 삭제")
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
    app.global::<SettingsData>().on_changed(move || {
        let Some(app) = weak.upgrade() else { return };
        let d = app.global::<SettingsData>();
        let new = settings::Settings {
            notify: d.get_notify(),
            preview: d.get_preview().clamp(0, 2) as u8,
            sound: d.get_sound(),
            badge_count: d.get_badge_count().clamp(0, 1) as u8,
            theme: d.get_theme().clamp(0, 2) as u8,
            zoom: settings::ZOOMS.get(d.get_zoom().max(0) as usize).copied().unwrap_or(100),
            enter_sends: d.get_enter_sends(),
            autostart: d.get_autostart(),
            start_hidden: d.get_start_hidden(),
            auto_update: d.get_auto_update(),
            muted_mentions: d.get_muted_mentions(),
            muted_replies: d.get_muted_replies(),
            badge_muted: d.get_badge_muted(),
            link_previews: d.get_link_previews(),
            photo_quality: d.get_photo_quality().clamp(0, 1) as u8,
            last_seen_version: UI.with_borrow(|ui| ui.settings.last_seen_version.clone()),
            window: UI.with_borrow(|ui| ui.settings.window),
        };
        let (old, path) = UI.with_borrow_mut(|ui| (std::mem::replace(&mut ui.settings, new.clone()), ui.settings_path.clone()));
        if old == new {
            return;
        }
        new.publish();
        if old.link_previews != new.link_previews {
            linkcard::update(&app);
        }
        if (old.badge_count, old.badge_muted) != (new.badge_count, new.badge_muted) {
            UI.with_borrow(|ui| show_total_unread(&app, ui));
        }
        if (old.autostart, old.start_hidden) != (new.autostart, new.start_hidden) {
            autostart::apply(new.autostart, new.start_hidden, !old.autostart && new.autostart);
        }
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
    app.global::<SettingsData>().on_clear_thumbs(move || s(Command::ClearThumbs));
}

/// The update buttons on the settings page.
fn wire_update(app: &AppWindow) {
    app.global::<SettingsData>().on_restart_for_update(update::restart);
    let weak = app.as_weak();
    app.global::<SettingsData>().on_install_update(move || {
        let Some(app) = weak.upgrade() else { return };
        if !update::install() {
            app.global::<SettingsData>().set_update_installing(false);
        }
    });
    let weak = app.as_weak();
    app.global::<SettingsData>().on_check_update(move || {
        let Some(app) = weak.upgrade() else { return };
        if update::check_now() {
            app.global::<SettingsData>().set_update_status(SharedString::new());
            app.global::<SettingsData>().set_update_checking(true);
        } else {
            app.global::<SettingsData>().set_update_status("설치된 Sigwin에서만 업데이트할 수 있습니다.".into());
        }
    });
}

/// Starts checking for updates of the installed `exe`; the settings page shows what it finds.
pub(crate) fn start_updates(app: &AppWindow, exe: PathBuf) {
    let weak = app.as_weak();
    update::spawn(exe, move |st| {
        let _ = weak.upgrade_in_event_loop(move |app| {
            let d = app.global::<SettingsData>();
            d.set_update_checking(false);
            d.set_update_installing(false);
            match st {
                update::Status::Ready(v) => {
                    d.set_update_available(SharedString::new());
                    d.set_update_version(v.into());
                }
                update::Status::Available(v) => d.set_update_available(v.into()),
                update::Status::UpToDate => {
                    d.set_update_available(SharedString::new());
                    d.set_update_status("최신 버전입니다.".into());
                }
                update::Status::Failed => {
                    d.set_update_status("확인하지 못했습니다. 네트워크를 확인하고 다시 시도하세요.".into())
                }
            }
        });
    });
}


/// Adds files to the composer's pending attachments.
pub(crate) fn add_files(app: &AppWindow, files: Vec<PathBuf>) {
    if files.is_empty() {
        return;
    }
    let (all, why, danger) = UI.with_borrow_mut(|ui| {
        let (ok, why) = attach::admit(&ui.files, files);
        let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let danger = ok.iter().any(|p| view::media::is_dangerous_file(&name(p)));
        ui.files.extend(ok);
        (ui.files.clone(), why, danger)
    });
    if let Some(why) = why {
        notice::show(app, why);
    } else if danger {
        notice::show(app, "이런 종류의 파일은 상대의 공식 앱에서 받지 못할 수 있습니다.");
    }
    attach::show(app, &all);
    app.invoke_focus_composer();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_thumb_is_a_square_of_the_box_size() {
        let src = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(120, 80));
        let t = reply_thumb(&src).expect("pixels readable");
        let side = types::physical_px(view::QUOTE_IMAGE.0);
        assert_eq!((t.size().width, t.size().height), (side, side));
    }
}
