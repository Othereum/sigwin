//! `MsgDto` / `ThreadDto` -> Slint rows: rich text, mentions, spoilers, links, avatars.

use super::*;

pub(crate) const PALETTE: [u32; 12] = [
    0xc73f00, 0xf6475d, 0xa23474, 0x8f2af4, 0x5b6ce6, 0x3a77c9, 0x0a7b83, 0x197a3d, 0x6b7b14,
    0x8a5c00, 0x9e3b8f, 0x4f6f8f,
];

pub(crate) fn tint(key: &str) -> Color {
    let c = tint_rgb(key);
    Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// A chat's placeholder color as 0xRRGGBB (also used for toast pictures).
pub(crate) fn tint_rgb(key: &str) -> u32 {
    let h = key.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(h as usize) % PALETTE.len()]
}

pub(crate) fn initial(name: &str) -> String {
    name.chars().find(|c| !c.is_whitespace()).map(|c| c.to_uppercase().to_string()).unwrap_or_default()
}

/// Brings the message model from `ui.last` to `items` touching only the rows that differ.
/// Replacing the whole model makes the ListView drop every row and re-seek, which snaps the
/// scroll position to a row boundary on every reload (receipts, reactions, typing).
pub(crate) fn update_rows(ui: &mut Ui, items: &[MsgDto], dark: bool) {
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
    let item = |m: &MsgDto| to_item(m, &ui.thumbs, &ui.anims, &ui.sender_avatars, &ui.revealed, dark);
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
pub(crate) fn refresh_times(ui: &Ui) {
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
pub(crate) fn sync_typing(ui: &mut Ui) {
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

pub(crate) fn pack_item(p: &types::PackDto, thumbs: &HashMap<String, Image>) -> PackItem {
    PackItem {
        id: p.id.as_str().into(),
        title: p.title.as_str().into(),
        installed: p.installed,
        cover: thumbs.get(&p.cover_key).cloned().unwrap_or_default(),
    }
}

/// Prefix of the link that opens a message's spoilers; the message id follows.
pub(crate) const SPOILER_URL: &str = "sigwin-spoiler:";
/// "더 읽기" under a cut-short message
pub(crate) const MORE_URL: &str = "sigwin-more:";

/// Font styles of a message (bold, italic, strikethrough, monospace) as styled text spans.
pub(crate) fn font_spans(m: &MsgDto) -> Vec<(std::ops::Range<usize>, SigwinStyle)> {
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
pub(crate) fn hidden_spoilers(m: &MsgDto, revealed: &std::collections::HashSet<String>) -> Vec<(usize, usize)> {
    if m.deleted || revealed.contains(&m.id) {
        return Vec::new();
    }
    m.styles.iter().filter(|s| s.2 == view::STYLE_SPOILER).map(|&(s, e, _)| (s, e)).collect()
}

/// Message text with the @mentions colored, links underlined and spoilers covered. The styled
/// text is drawn over a TextInput whose own glyphs are transparent, so selection keeps working.
/// Both get the same font styles (the TextInput through `sigwin_set_plain_style`), so they lay
/// out identically; the rest only changes colors, underlines and boxes.
pub(crate) fn rich_text(m: &MsgDto, links: &[(usize, usize)], spoilers: &[(usize, usize)], dark: bool) -> slint::StyledText {
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
pub(crate) fn reader_items(m: &MsgDto, senders: &HashMap<String, Image>) -> ModelRc<ReaderItem> {
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

pub(crate) fn to_item(
    m: &MsgDto,
    thumbs: &HashMap<String, Image>,
    anims: &super::anim::Anims,
    senders: &HashMap<String, Image>,
    revealed: &std::collections::HashSet<String>,
    dark: bool,
) -> MsgItem {
    let image = thumbs.get(&m.image_key).cloned();
    let (frames, frame) = anims.models(&m.image_key);
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
        frames,
        frame,
        img_w: m.img_w as f32,
        img_h: m.img_h as f32,
        deleted: m.deleted,
        day_sep: m.day_sep.as_str().into(),
        system: m.system,
        sticker: m.sticker,
        receipt_info: m.receipt_info.as_str().into(),
        pending: m.pending,
        hide_time: m.hide_time,
        collapse_above: m.collapse_above,
        collapse_below: m.collapse_below,
        more: m.more,
        typing: false,
    }
}

pub(crate) fn thread_item(t: &ThreadDto, avatars: &HashMap<String, Image>) -> ThreadItem {
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
        draft: false,
    }
}

/// Shows the spoilers of message `id` (clicked).
pub(crate) fn reveal_spoilers(app: &AppWindow, id: &str) {
    use slint::Model;
    let dark = app.global::<Theme>().get_dark();
    UI.with_borrow_mut(|ui| {
        ui.revealed.insert(id.to_string());
        if let Some(i) = ui.last.iter().position(|m| m.id == id) {
            if i < ui.messages.row_count() {
                let item = to_item(&ui.last[i], &ui.thumbs, &ui.anims, &ui.sender_avatars, &ui.revealed, dark);
                ui.messages.set_row_data(i, item);
            }
        }
    });
}

/// Rebuild the message rows (their mention colors depend on the theme).
pub(crate) fn refresh_rows(app: &AppWindow) {
    let dark = app.global::<Theme>().get_dark();
    UI.with_borrow_mut(|ui| {
        let rows: Vec<MsgItem> = ui.last.iter().map(|m| to_item(m, &ui.thumbs, &ui.anims, &ui.sender_avatars, &ui.revealed, dark)).collect();
        ui.messages.set_vec(rows);
        ui.typing_row = false;
        sync_typing(ui);
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
            ui.messages.set_vec(ui.last.iter().map(|m| to_item(m, &HashMap::new(), &Default::default(), &HashMap::new(), &Default::default(), false)).collect::<Vec<_>>());
            let new = rows(new);
            update_rows(&mut ui, &new, false);
            let got: Vec<String> = ui.messages.iter().map(|m| m.text.to_string()).collect();
            let want: Vec<String> = new.iter().map(|m| m.text.clone()).collect();
            assert_eq!(got, want, "{old:?} -> {new:?}");
        }
    }
}
