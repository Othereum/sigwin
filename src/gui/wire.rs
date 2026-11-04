//! Slint callbacks -> `Command`s, and the native window hook.

use super::*;

pub(crate) fn wire(app: &AppWindow, tx: UnboundedSender<Command>) {
    let send = move |c: Command| {
        let _ = tx.send(c);
    };
    wire_threads(app, &send);
    wire_composer(app, &send);
    wire_messages(app, &send);
    wire_stickers(app, &send);
    wire_settings(app, &send);
    anim::wire(app);
    if !headless::active() {
        // no native window in headless; started when the window is first shown
        let send = send.clone();
        set_starter(move |app| hook_window(app, &send));
    }
}

/// Linking, the thread list, search and paging.
fn wire_threads(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
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
    app.on_search_changed(move |q| s(Command::Search(q.to_string())));
}

/// Composer: sending, attachments, paste, @mentions, formatting, reply.
fn wire_composer(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
    // the composer text before the last edit, to find what an edit inserted
    let prev = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let weak = app.as_weak();
    app.on_clamp_draft(move |text| {
        let mut prev = prev.borrow_mut();
        let Some((fit, cursor)) = crate::view::fit_edit(&prev, &text, crate::view::MAX_LONG_TEXT_BYTES) else {
            *prev = text.to_string();
            return -1;
        };
        if let Some(app) = weak.upgrade() {
            app.set_draft(fit.as_str().into());
            app.set_status_text("메시지는 64KB까지 입력할 수 있습니다".into());
        }
        *prev = fit;
        cursor as i32
    });

    let s = send.clone();
    let weak = app.as_weak();
    app.on_send(move |text| {
        let Some(app) = weak.upgrade() else { return };
        // before anything is cleared: the text stays in the composer to shorten
        // (mentions and format markers only shrink it, so this never refuses what would fit)
        if text.len() > crate::view::MAX_LONG_TEXT_BYTES {
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
        s(Command::Send { text: text.to_string(), files, quote, mentions });
        app.set_mention_items(ModelRc::default());
        app.set_draft(SharedString::new());
        app.set_pending_files(ModelRc::default());
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
    app.on_clear_attachments(move || {
        UI.with_borrow_mut(|ui| ui.files.clear());
        if let Some(app) = weak.upgrade() {
            app.set_pending_files(ModelRc::default());
            drafts::drop_if_empty(&app);
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
            drafts::drop_if_empty(&app);
        }
    });
}

/// Message actions: menu, reactions, edit, pin, delete, attachments, links.
fn wire_messages(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
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
    app.on_attachment_clicked(move |id| {
        let is_sticker = UI.with_borrow(|ui| ui.last.iter().any(|m| m.id == id.as_str() && m.sticker));
        if is_sticker {
            s(Command::StickerClicked(id.to_string()));
        } else {
            s(Command::OpenAttachment(id.to_string()));
        }
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
}

/// Settings page and unlinking.
fn wire_settings(app: &AppWindow, send: &(impl Fn(Command) + Clone + 'static)) {
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
            preview: app.get_set_preview().clamp(0, 2) as u8,            badge: app.get_set_badge(),
            theme: app.get_set_theme().clamp(0, 2) as u8,
            zoom: settings::ZOOMS.get(app.get_set_zoom().max(0) as usize).copied().unwrap_or(100),
            enter_sends: app.get_enter_sends(),
            autostart: app.get_set_autostart(),
            start_hidden: app.get_set_start_hidden(),
        };
        let (old, path) = UI.with_borrow_mut(|ui| (std::mem::replace(&mut ui.settings, new.clone()), ui.settings_path.clone()));
        if old == new {
            return;
        }
        new.publish();
        badge::set_enabled(new.badge);
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
    app.on_clear_thumbs(move || s(Command::ClearThumbs));
}


/// Adds files to the composer's pending attachments.
pub(crate) fn add_files(app: &AppWindow, files: Vec<PathBuf>) {
    if files.is_empty() {
        return;
    }
    let names: Vec<SharedString> = UI.with_borrow_mut(|ui| {
        ui.files.extend(files);
        drafts::file_names(&ui.files)
    });
    app.set_pending_files(ModelRc::new(VecModel::from(names)));
    app.invoke_focus_composer();
}
