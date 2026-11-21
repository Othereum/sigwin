//! The open chat's media page: its pictures and videos, or its files, newest first, read from the
//! media index a page at a time. What it loaded stays while it shows, so the viewer and the row
//! commands find messages outside the window here (`row_message`).

use std::ops::Range;

use super::*;
use crate::db::MediaKind;
use crate::types::{GalleryItemDto, GALLERY_PREFIX};

/// Attachments read per page: a few screens of cells, or of file rows.
const PAGE_MEDIA: u32 = 120;
const PAGE_FILES: u32 = 60;

pub(super) struct Gallery {
    pub key: String,
    thread: Thread,
    kind: MediaKind,
    /// what's loaded, newest first: (sent ts, index in its `attachments`)
    items: Vec<(u64, usize)>,
    msgs: HashMap<u64, DataMessage>,
    more: bool,
    /// when the first of the loaded messages disappears
    pub expires: Option<u64>,
    /// items whose thumbnails are wanted
    shown: Range<usize>,
    /// opened before the media backfill got through this chat: nothing is read until it has,
    /// since what's indexed so far is its oldest part
    indexing: bool,
}

impl Gallery {
    pub(super) fn message(&self, ts: u64) -> Option<&DataMessage> {
        self.msgs.get(&ts)
    }
}

impl App {
    pub(super) async fn gallery_open(&mut self, files: bool) {
        let Some(open) = self.current.as_ref() else { return };
        let (key, thread) = (open.key.clone(), open.thread.clone());
        self.gallery_close();
        let kind = if files { MediaKind::Files } else { MediaKind::Media };
        let indexing = self.media_backfill.as_ref().is_some_and(|done| !done.contains(&key));
        let msgs = HashMap::new();
        self.gallery = Some(Gallery { key, thread, kind, items: Vec::new(), msgs, more: true, expires: None, shown: 0..0, indexing });
        self.gallery_page(true).await;
    }

    /// The media backfill is done with thread `key`, or with all (`None`): a page waiting for it
    /// reads its first page now.
    pub(super) async fn gallery_indexed(&mut self, key: Option<&str>) {
        let Some(g) = self.gallery.as_mut().filter(|g| g.indexing && key.is_none_or(|k| k == g.key)) else { return };
        g.indexing = false;
        self.gallery_page(true).await;
    }

    pub(super) async fn gallery_more(&mut self) {
        if self.gallery.as_ref().is_some_and(|g| g.more) {
            self.gallery_page(false).await;
        }
    }

    /// Lets go of everything the page loaded. Downloads it started go on; those waiting for their
    /// turn don't start.
    pub(super) fn gallery_close(&mut self) {
        self.gallery = None;
        self.gallery_requested.borrow_mut().clear();
    }

    /// Reads the next page and sends it to the UI.
    async fn gallery_page(&mut self, reset: bool) {
        let Some(g) = self.gallery.as_ref() else { return };
        let (key, kind) = (g.key.clone(), g.kind);
        if g.indexing {
            (self.emit)(Event::Gallery { files: kind == MediaKind::Files, items: Vec::new(), more: false, reset, indexing: true });
            return;
        }
        let limit = if kind == MediaKind::Media { PAGE_MEDIA } else { PAGE_FILES };
        let mut after = g.items.last().copied();
        let mut got = Vec::new();
        let mut more = true;
        // what turns out to be gone leaves the index on the way, so a page can come up empty
        // while older ones remain
        while got.is_empty() && more {
            let page = match self.index.media_page(&key, kind, after, limit).await {
                Ok(p) => p,
                Err(e) => {
                    warn!("media page: {e:#}");
                    return;
                }
            };
            more = page.len() as u32 == limit;
            after = page.last().copied();
            for (ts, idx) in page {
                if self.gallery_load(ts, idx).await {
                    got.push((ts, idx));
                }
            }
        }
        let Some(g) = self.gallery.as_mut() else { return };
        g.items.extend(&got);
        g.more = more;
        let items = got.iter().filter_map(|&(ts, idx)| item_dto(g, ts, idx)).collect();
        (self.emit)(Event::Gallery { files: kind == MediaKind::Files, items, more, reset, indexing: false });
    }

    /// Reads message `ts` into the page if it still shows attachment `idx`; one that's gone
    /// leaves the index.
    async fn gallery_load(&mut self, ts: u64, idx: usize) -> bool {
        let Some(g) = self.gallery.as_ref() else { return false };
        if let Some(dm) = g.msgs.get(&ts) {
            return dm.attachments.get(idx).is_some();
        }
        let (key, thread) = (g.key.clone(), g.thread.clone());
        let c = self.store.message(&thread, ts).await.ok().flatten();
        let now = view::time::now_ms();
        let found = c.as_ref().and_then(|c| match view::classify(c, self.me) {
            Kind::Msg { dm, .. } if view::expires_at(c).is_none_or(|t| t > now) => Some((dm.clone(), view::expires_at(c))),
            _ => None,
        });
        let Some((dm, expires)) = found else {
            if let Err(e) = self.index.forget_media(&key, ts).await {
                warn!("forget media: {e:#}");
            }
            return false;
        };
        let has = dm.attachments.get(idx).is_some();
        let Some(g) = self.gallery.as_mut() else { return false };
        g.msgs.insert(ts, dm);
        if let Some(t) = expires {
            g.expires = Some(g.expires.map_or(t, |e| e.min(t)));
        }
        has
    }

    /// Items `from..to` are on screen or close: asks for their thumbnails, and lets go of the rest.
    pub(super) fn gallery_shown(&mut self, from: usize, to: usize) {
        let Some(g) = self.gallery.as_mut() else { return };
        if g.kind != MediaKind::Media {
            return;
        }
        let to = to.min(g.items.len());
        g.shown = from.min(to)..to;
        let wanted: Vec<(String, String, AttachmentPointer)> = g.items[g.shown.clone()]
            .iter()
            .filter_map(|&(ts, idx)| {
                let a = g.msgs.get(&ts)?.attachments.get(idx)?;
                let file_key = view::media::attachment_key(a, ts, idx);
                Some((cell_key(&file_key), file_key, a.clone()))
            })
            .collect();
        let key = g.key.clone();
        self.gallery_requested.borrow_mut().retain(|k| wanted.iter().any(|w| w.0 == *k));
        // a stranger's pictures wait for a click until the request is accepted
        let auto = !self.is_request(&key);
        let cell = view::media::GALLERY_CELL;
        for (key, file_key, ptr) in wanted {
            if !self.gallery_requested.borrow_mut().insert(key.clone()) {
                continue;
            }
            if view::media::is_video(&ptr) {
                self.request_video_thumb(key, file_key, ptr, (cell, cell), auto, None);
            } else {
                self.request_thumb(key, file_key, ptr, ThumbShape::Cover(cell, cell), auto, None, None);
            }
        }
    }

    /// The scale changed: the cells in range ask for their pictures again, at the new size.
    pub(super) fn gallery_rescale(&mut self) {
        let Some(shown) = self.gallery.as_ref().map(|g| g.shown.clone()) else { return };
        self.gallery_requested.borrow_mut().clear();
        self.gallery_shown(shown.start, shown.end);
    }

    /// `file_key` is here now, or was asked for: its cell asks for its picture again.
    pub(super) fn gallery_ask_again(&mut self, file_key: &str) {
        let Some(shown) = self.gallery.as_ref().map(|g| g.shown.clone()) else { return };
        let key = cell_key(file_key);
        self.gallery_requested.borrow_mut().remove(&key);
        self.gallery_shown(shown.start, shown.end);
    }

    /// The page's "save" on attachment `index` of message `id`.
    pub(super) fn gallery_save(&self, id: &str, index: usize, owner: isize) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let Some(a) = dm.attachments.get(index) else { return };
        let file_key = view::media::attachment_key(a, ts, index);
        let job = SaveJob { path: attachment_path(&self.paths.attachments, &file_key), file_key, ptr: a.clone() };
        self.ask_where_to_save(Save { id: id.to_string(), line: false, jobs: vec![job] }, owner);
    }

    /// Message `ts` of thread `key` is gone (deleted for everyone): out of the index and the page.
    pub(super) async fn forget_media(&mut self, key: &str, ts: u64) {
        if let Err(e) = self.index.forget_media(key, ts).await {
            warn!("forget media: {e:#}");
        }
        let Some(g) = self.gallery.as_mut().filter(|g| g.key == key) else { return };
        if g.msgs.remove(&ts).is_some() {
            g.items.retain(|i| i.0 != ts);
            self.gallery_resend();
        }
    }

    /// Messages of the page may have disappeared: what's gone leaves it.
    pub(super) async fn gallery_prune(&mut self) {
        let Some(g) = self.gallery.as_mut() else { return };
        let (key, tss): (String, Vec<u64>) = (g.key.clone(), g.msgs.keys().copied().collect());
        g.msgs.clear();
        g.expires = None;
        for ts in tss {
            self.gallery_load(ts, 0).await;
        }
        let Some(g) = self.gallery.as_mut().filter(|g| g.key == key) else { return };
        let (before, msgs) = (g.items.len(), &g.msgs);
        g.items.retain(|i| msgs.contains_key(&i.0));
        if g.items.len() != before {
            self.gallery_resend();
        }
    }

    /// Everything the page has loaded, in place of what the UI shows.
    fn gallery_resend(&mut self) {
        let Some(g) = self.gallery.as_ref() else { return };
        let items = g.items.iter().filter_map(|&(ts, idx)| item_dto(g, ts, idx)).collect();
        let (more, indexing) = (g.more && !g.indexing, g.indexing);
        (self.emit)(Event::Gallery { files: g.kind == MediaKind::Files, items, more, reset: true, indexing });
        let shown = g.shown.clone();
        self.gallery_shown(shown.start, shown.end);
    }
}

/// The key a media page cell knows its picture by.
fn cell_key(file_key: &str) -> String {
    let cell = view::media::GALLERY_CELL;
    format!("{GALLERY_PREFIX}{file_key}@{cell}x{cell}")
}

fn item_dto(g: &Gallery, ts: u64, idx: usize) -> Option<GalleryItemDto> {
    let dm = g.msgs.get(&ts)?;
    let a = dm.attachments.get(idx)?;
    let mut d = GalleryItemDto { id: ts.to_string(), index: idx, month: view::time::month_label(ts), ..Default::default() };
    match g.kind {
        MediaKind::Media => {
            d.key = cell_key(&view::media::attachment_key(a, ts, idx));
            d.video = view::media::is_video(a);
            // what `ViewImage` / `OpenVideo` count: the message's pictures, or its videos
            let same = |x: &AttachmentPointer| if d.video { view::media::is_video(x) } else { view::media::is_image(x) };
            d.nth = view::media::shown_attachments(dm).filter(|(_, x)| same(x)).position(|(i, _)| i == idx)?;
        }
        MediaKind::Files => {
            d.name = view::media::attachment_label(a);
            d.danger = view::media::is_dangerous(a);
            let day = view::time::day_short(ts);
            d.sub = match a.size.filter(|s| *s > 0) {
                Some(s) => format!("{} · {day}", view::media::size_text(s as u64)),
                None => day,
            };
        }
    }
    Some(d)
}
