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
    // the window moved: rows left one end and came in at the other, which the common prefix and
    // suffix below don't describe. Line both lists up on the rows they share first
    let (drop, add) = match (ui.last.first(), items.first()) {
        (Some(old), Some(new)) if old.id != new.id => match items.iter().position(|m| m.id == old.id) {
            Some(add) => (0, add),
            None => (ui.last.iter().position(|m| m.id == new.id).unwrap_or(0), 0),
        },
        _ => (0, 0),
    };
    for _ in 0..drop {
        ui.messages.remove(0);
    }
    for (k, m) in items[..add].iter().enumerate() {
        ui.messages.insert(k, to_item(m, &ui.thumbs, &ui.media_labels, &ui.anims, &ui.sender_avatars, &ui.revealed, dark));
    }
    let old = &ui.last[drop..];
    let at = add;
    let items = &items[add..];
    let max = old.len().min(items.len());
    let pre = old.iter().zip(items).take_while(|(a, b)| a.id == b.id).count();
    let suf = old.iter().rev().zip(items.iter().rev()).take(max - pre).take_while(|(a, b)| a.id == b.id).count();
    let item = |m: &MsgDto| to_item(m, &ui.thumbs, &ui.media_labels, &ui.anims, &ui.sender_avatars, &ui.revealed, dark);
    for i in (0..pre).chain(old.len() - suf..old.len()) {
        let j = if i < pre { i } else { i + items.len() - old.len() };
        if old[i] != items[j] {
            ui.messages.set_row_data(at + i, item(&items[j]));
        }
    }
    for _ in pre..old.len() - suf {
        ui.messages.remove(at + pre);
    }
    for (k, m) in items[pre..items.len() - suf].iter().enumerate() {
        ui.messages.insert(at + pre + k, item(m));
    }
}

/// Draws again the rows `uses` picks. What arrives late (a picture, a label, frames, an avatar)
/// goes into the caches and through here, so a row is made by `to_item` alone, whether it was
/// built from scratch or filled in afterwards: the two can't come to differ.
pub(crate) fn redraw_rows(ui: &Ui, uses: impl Fn(&MsgDto) -> bool) {
    use slint::Model;
    for (i, m) in ui.last.iter().enumerate().filter(|(_, m)| uses(m)) {
        if i < ui.messages.row_count() {
            ui.messages.set_row_data(i, to_item(m, &ui.thumbs, &ui.media_labels, &ui.anims, &ui.sender_avatars, &ui.revealed, ui.dark));
        }
    }
}

/// Whether the message shows the picture or label cached under `key`, anywhere.
pub(crate) fn shows_key(m: &MsgDto, key: &str) -> bool {
    m.image_key == key
        || m.cells.iter().any(|c| c.key == key)
        || m.link.as_ref().is_some_and(|l| l.image_key == key)
        || (!m.quote_image_key.is_empty() && m.quote_image_key == key)
}

/// Re-derives the relative times of recent bubbles ("3분") as the minutes pass.
pub(crate) fn refresh_times(ui: &Ui) {
    use slint::Model;
    for (i, m) in ui.last.iter().enumerate().rev().take(100) {
        let Ok(ts) = m.id.parse::<u64>() else { continue };
        let t = view::time::bubble_time(ts, &m.time);
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
    if ui.detached {
        return;
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
/// "다시 보내기" / "삭제" under a message that failed to send
pub(crate) const RETRY_URL: &str = "sigwin-retry:";
pub(crate) const DISCARD_URL: &str = "sigwin-discard:";

/// Font styles of a message (bold, italic, strikethrough, monospace) as styled text spans.
pub(crate) fn font_spans(m: &MsgDto) -> Vec<(std::ops::Range<usize>, SigwinStyle)> {
    m.styles
        .iter()
        .filter_map(|&(s, e, k)| {
            let style = match k {
                view::text::STYLE_BOLD => SigwinStyle::Bold,
                view::text::STYLE_ITALIC => SigwinStyle::Italic,
                view::text::STYLE_STRIKE => SigwinStyle::Strike,
                view::text::STYLE_MONO => SigwinStyle::Mono,
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
    m.styles.iter().filter(|s| s.2 == view::text::STYLE_SPOILER).map(|&(s, e, _)| (s, e)).collect()
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
    readers_model(&m.readers, senders, avatar::READ_PREFIX)
}

/// `reader_items` for any list of (uuid, name); the photos are keyed `<prefix><uuid>`.
pub(crate) fn readers_model(readers: &[(String, String)], photos: &HashMap<String, Image>, prefix: &str) -> ModelRc<ReaderItem> {
    let rows: Vec<ReaderItem> = readers
        .iter()
        .take(types::MAX_READER_PHOTOS)
        .map(|(uuid, name)| {
            let photo = photos.get(&format!("{prefix}{uuid}")).cloned();
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

/// The percent a picture's mark shows; -1: none.
pub(crate) fn progress_of(label: &types::MediaLabel) -> i32 {
    label.progress.map_or(-1, i32::from)
}

/// The ring around a video's mark (`VideoMark`, 40px), at the size it's shown.
pub(crate) fn ring_of(label: &types::MediaLabel) -> Image {
    label.progress.map_or_else(Image::default, |percent| {
        Image::from_rgba8(sigwin_thumb::progress_ring(types::physical_px(40), types::physical_px(3) as f32, percent))
    })
}

/// What the pictures say about their downloads, by key. Each key has a one-row model that every
/// cell of that picture shares, and `set` changes it in place: drawing the cell again would
/// give it new elements, and a click that began on the old ones would end on nothing. That
/// happens a few times a second while a picture downloads.
#[derive(Default)]
pub(crate) struct MediaStates {
    models: RefCell<HashMap<String, Rc<VecModel<MediaState>>>>,
}

fn state_of(label: &types::MediaLabel) -> MediaState {
    MediaState { label: label.text.as_str().into(), undownloaded: label.undownloaded, progress: progress_of(label), ring: ring_of(label) }
}

impl MediaStates {
    /// The model a cell of `key` shows, made if there is none yet.
    pub(crate) fn model(&self, key: &str) -> ModelRc<MediaState> {
        let mut models = self.models.borrow_mut();
        let model = models.entry(key.to_string()).or_insert_with(|| Rc::new(VecModel::from(vec![state_of(&Default::default())])));
        ModelRc::from(model.clone())
    }

    /// Changes what `key` says. False: nothing shows it yet, so the rows that do have to be
    /// drawn once to get its model.
    pub(crate) fn set(&self, key: &str, label: &types::MediaLabel) -> bool {
        let found = self.models.borrow().get(key).cloned();
        match found {
            Some(model) => {
                slint::Model::set_row_data(&*model, 0, state_of(label));
                true
            }
            None => {
                self.models.borrow_mut().insert(key.to_string(), Rc::new(VecModel::from(vec![state_of(label)])));
                false
            }
        }
    }

    pub(crate) fn undownloaded(&self, key: &str) -> bool {
        self.models.borrow().get(key).is_some_and(|m| slint::Model::row_data(&**m, 0).is_some_and(|s| s.undownloaded))
    }

    pub(crate) fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.models.borrow_mut().retain(|k, _| keep(k));
    }

    pub(crate) fn clear(&self) {
        self.models.borrow_mut().clear();
    }
}

pub(crate) fn to_item(
    m: &MsgDto,
    thumbs: &HashMap<String, Image>,
    states: &MediaStates,
    anims: &super::anim::Anims,
    senders: &HashMap<String, Image>,
    revealed: &std::collections::HashSet<String>,
    dark: bool,
) -> MsgItem {
    // stickers: the picture and its frames
    let image = thumbs.get(&m.image_key).cloned();
    let (frames, frame) = anims.models(&m.image_key);
    let cells: Vec<ImageCell> = m
        .cells
        .iter()
        .enumerate()
        .map(|(n, c)| ImageCell {
            x: c.x as f32,
            y: c.y as f32,
            w: c.w as f32,
            h: c.h as f32,
            image: thumbs.get(&c.key).cloned().unwrap_or_default(),
            more: c.more as i32,
            video: c.video,
            gif: c.gif,
            state: states.model(&c.key),
            frames: anims.gif_models(&m.id, n).0,
            frame: anims.gif_models(&m.id, n).1,
        })
        .collect();
    let avatar = if m.show_avatar { senders.get(&m.sender_key).cloned() } else { None };
    let reactions: Vec<ReactionItem> = m
        .reactions
        .iter()
        .map(|(e, n, who)| ReactionItem {
            emoji: e.as_str().into(),
            count: *n as i32,
            mine: *e == m.my_reaction,
            who: who.as_str().into(),
        })
        .collect();
    let links = if m.deleted { Vec::new() } else { link::find(&m.text) };
    let fonts = if m.deleted { Vec::new() } else { font_spans(m) };
    if !fonts.is_empty() {
        // the TextInput lays out with the same font styles as the StyledText over it
        sigwin_set_plain_style(&m.text, Some(sigwin_styled(&m.text, &fonts)));
    }
    let spoilers = hidden_spoilers(m, revealed);
    let has_rich = !m.mentions.is_empty() || !links.is_empty() || !spoilers.is_empty();
    let file = crate::view::media::file_card(&m.attachment);
    let card = m.link.as_ref();
    let text_of = |f: fn(&types::LinkCard) -> &String| card.map(f).map_or(SharedString::new(), |s| s.as_str().into());
    MsgItem {
        link_url: text_of(|c| &c.url),
        link_title: text_of(|c| &c.title),
        link_desc: text_of(|c| &c.description),
        link_domain: text_of(|c| &c.domain),
        link_image: card.and_then(|c| thumbs.get(&c.image_key)).cloned().unwrap_or_default(),
        link_img_w: card.map_or(0.0, |c| c.img_w as f32),
        link_img_h: card.map_or(0.0, |c| c.img_h as f32),
        reactions: ModelRc::new(VecModel::from(reactions)),
        replies: m.replies as i32,
        big_emoji: m.big_emoji,
        id: m.id.as_str().into(),
        sender: m.sender.as_str().into(),
        sender_color: tint(&m.sender_key),
        text: m.text.as_str().into(),
        has_rich,
        rich: if has_rich { rich_text(m, &links, &spoilers, dark) } else { Default::default() },
        time: {
            let t = m.id.parse::<u64>().map_or(m.time.clone(), |ts| view::time::bubble_time(ts, &m.time));
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
        quote_author: m.quote_author.as_str().into(),
        quote_id: m.quote_id.as_str().into(),
        has_quote_image: !m.quote_image_key.is_empty(),
        quote_image: thumbs.get(&m.quote_image_key).cloned().unwrap_or_default(),
        attachment: m.attachment.as_str().into(),
        file_name: file.0.into(),
        file_size: file.1.into(),
        file_ext: file.2.into(),
        file_danger: m.file_danger,
        has_image: !m.image_key.is_empty() || !m.cells.is_empty(),
        image: image.unwrap_or_default(),
        frames,
        frame,
        img_w: m.img_w as f32,
        img_h: m.img_h as f32,
        cells: if cells.is_empty() { Default::default() } else { ModelRc::new(VecModel::from(cells)) },
        deleted: m.deleted,
        day_sep: m.day_sep,
        day: m.day,
        unread_divider: m.unread_divider,
        system: m.system,
        sticker: m.sticker,
        receipt_info: m.receipt_info.as_str().into(),
        pending: m.pending,
        failed: m.failed,
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
        muted: t.mute.active(view::time::now_ms()),
        readers: readers_model(&t.readers, avatars, avatar::LIST_READ_PREFIX),
        readers_more: t.readers_more as i32,
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
                let item = to_item(&ui.last[i], &ui.thumbs, &ui.media_labels, &ui.anims, &ui.sender_avatars, &ui.revealed, dark);
                ui.messages.set_row_data(i, item);
            }
        }
    });
}

/// Rebuild the message rows (their mention colors depend on the theme).
pub(crate) fn refresh_rows(app: &AppWindow) {
    let dark = app.global::<Theme>().get_dark();
    UI.with_borrow_mut(|ui| {
        ui.dark = dark;
        let rows: Vec<MsgItem> = ui.last.iter().map(|m| to_item(m, &ui.thumbs, &ui.media_labels, &ui.anims, &ui.sender_avatars, &ui.revealed, dark)).collect();
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
            ui.messages.set_vec(ui.last.iter().map(|m| to_item(m, &HashMap::new(), &Default::default(), &Default::default(), &HashMap::new(), &Default::default(), false)).collect::<Vec<_>>());
            let new = rows(new);
            update_rows(&mut ui, &new, false);
            let got: Vec<String> = ui.messages.iter().map(|m| m.text.to_string()).collect();
            let want: Vec<String> = new.iter().map(|m| m.text.clone()).collect();
            assert_eq!(got, want, "{old:?} -> {new:?}");
        }
    }
}
