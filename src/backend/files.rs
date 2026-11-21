//! Attachment and thumbnail caches (encrypted on disk), opening attachments.

use futures::future::{AbortHandle, Abortable};
use presage::proto::data_message::Sticker;

use super::*;

/// How a message's thumbnail is drawn, in logical px.
pub(super) enum ThumbShape {
    /// the longer side is this long
    Fit(u32),
    /// fills this size, cut off around its center
    Cover(u32, u32),
}

/// What a picture of the open chat's rows is made from (`App::pictures`).
pub(super) enum Picture {
    Sticker(Sticker),
    Cell(view::media::WantedCell),
}

/// The open chat's pictures the UI has near the screen (`Command::PicturesNear`). Only those
/// are made: the window holds hundreds of rows, and a picture made for a row far off screen
/// would be decoded and sent only for the UI to let it go. The rest still get their labels,
/// blurhash and download, and are made once they come near.
#[derive(Clone, Default)]
pub(crate) struct Near {
    keys: Rc<RefCell<HashSet<String>>>,
    /// asked for while not near: made when they come near
    waiting: Rc<RefCell<HashSet<String>>>,
}

impl Near {
    /// Whether picture `key` may be made now; if not, it waits.
    fn admits(&self, key: &str) -> bool {
        if self.keys.borrow().contains(key) {
            return true;
        }
        self.waiting.borrow_mut().insert(key.to_string());
        false
    }
}

impl App {
    /// Asks for picture `key` of thread `thread`'s rows.
    pub(super) fn ask_picture(&self, thread: &str, key: &str, p: &Picture) {
        // a stranger's pictures wait for a click until the request is accepted
        let auto = !self.is_request(thread);
        let near = Some(self.near.clone());
        match p {
            Picture::Sticker(s) => {
                let Some(ptr) = s.data.clone() else { return };
                let shape = ThumbShape::Fit(view::media::sticker_size());
                self.request_thumb(key.into(), key.into(), ptr, shape, auto, Some(s.clone()), near);
            }
            Picture::Cell(c) if view::media::is_video(&c.ptr) => {
                self.request_video_thumb(key.into(), c.file_key.clone(), c.ptr.clone(), c.size, auto, near);
            }
            Picture::Cell(c) => {
                let shape = ThumbShape::Cover(c.size.0, c.size.1);
                self.request_thumb(key.into(), c.file_key.clone(), c.ptr.clone(), shape, auto, None, near);
            }
        }
    }

    /// The UI's pictures near the screen are now `near`: those that waited are made. `again`:
    /// ones the UI let go of off screen, shown again.
    pub(super) fn pictures_near(&self, near: Vec<String>, again: Vec<String>) {
        let Some(thread) = self.current.as_ref().map(|o| o.key.clone()) else { return };
        let mut waiting = self.near.waiting.borrow_mut();
        waiting.retain(|k| self.pictures.contains_key(k));
        let come: Vec<String> = near.iter().filter(|k| waiting.remove(*k)).cloned().collect();
        drop(waiting);
        tracing::debug!(near = near.len(), come = come.len(), again = again.len(), "pictures near");
        *self.near.keys.borrow_mut() = near.into_iter().collect();
        for k in come.iter().chain(&again) {
            let Some(p) = self.pictures.get(k) else { continue };
            self.thumbs_requested.borrow_mut().insert(k.clone());
            self.ask_picture(&thread, k, p);
        }
    }

    /// `key` is what the UI knows the result by, `file_key` the attachment's files on disk.
    /// Not `auto`: it waits for a click whatever its size (a message request's).
    /// `sticker`: a sticker message's, taken from its pack when installed, and from the sticker CDN
    /// when the attachment is gone. `near`: made only while near the screen (the rows').
    #[allow(clippy::too_many_arguments)]
    pub(super) fn request_thumb(
        &self,
        key: String,
        file_key: String,
        ptr: AttachmentPointer,
        shape: ThumbShape,
        auto: bool,
        sticker: Option<Sticker>,
        near: Option<Near>,
    ) {
        let thumb_path = self.paths.thumbs.join(format!("{file_key}.enc"));
        let orig_path = attachment_path(&self.paths.attachments, &file_key);
        let manager = self.manager.clone();
        let store = self.store.clone();
        let emit = self.emit.clone();
        let tx = self.tx.clone();
        let gate = self.thumb_gate.clone();
        let downloads = self.attachment_downloads.clone();
        let own = own_side(&ptr);
        // the same picture can be shown at another size; each shows how far it is
        let mut shown: Vec<String> = self.thumbs_requested.borrow().iter().filter(|k| k.starts_with(&file_key)).cloned().collect();
        // the media page's take turns, and only while they're still on screen there: scrolling
        // through it shouldn't start a download for everything that went by
        let gallery = key.starts_with(crate::types::GALLERY_PREFIX).then(|| {
            shown.push(key.clone());
            (self.gallery_gate.clone(), self.gallery_requested.clone())
        });
        tokio::task::spawn_local(async move {
            let mut labelled = false;
            if !thumb_path.exists() {
                if !orig_path.exists() && !from_installed_pack(&store, sticker.as_ref(), &orig_path).await {
                    labelled = true;
                    // its blurhash until it is here
                    if let Some(t) = ptr.blur_hash.as_deref().and_then(|b| blur_for(&key, b, &ptr, &shape)) {
                        emit(Event::Thumb(Arc::new(t)));
                    }
                    let gone = gone_path(&orig_path).exists();
                    let label = if gone { view::media::gone_label() } else { view::media::undownloaded_label(&ptr) };
                    emit(Event::MediaLabel { key: key.clone(), label });
                    let Some(manager) = manager else { return };
                    // stopped by a click: it stays so until the next one; a big one waits for it
                    if downloads.was_stopped(&file_key) || (!downloads.is_allowed(&file_key) && !(auto && view::media::auto_fetches_image(&ptr))) {
                        tracing::debug!(stopped = downloads.was_stopped(&file_key), "picture waits for a click");
                        return;
                    }
                    let _turn = match gallery {
                        Some((turn, requested)) => {
                            let t = turn.acquire_owned().await;
                            if !requested.borrow().contains(&key) {
                                return;
                            }
                            Some(t)
                        }
                        None => None,
                    };
                    // the same picture shown at another size may be getting it already: waited for
                    let show = Show::Cells(if shown.is_empty() { vec![key.clone()] } else { shown });
                    let res = if gone { Err(Gone.into()) } else { downloads.ensure(&file_key, &ptr, &show, &emit, &orig_path).await };
                    match res {
                        Ok(true) => {}
                        // stopped (it has put its label back)
                        Ok(false) => return,
                        // no retry: the server won't have it later either. Its label says so
                        // unless the sticker CDN has it.
                        Err(e) if e.is::<Gone>() => {
                            if !from_sticker_cdn(&manager, sticker.as_ref(), &orig_path).await {
                                return;
                            }
                        }
                        Err(e) => {
                            warn!("download attachment: {e:#}");
                            let _ = tx.send(Internal::ThumbFailed(key));
                            return;
                        }
                    }
                }
            }
            let k = key.clone();
            // one at a time: an album would otherwise decode all its full-size originals at once
            let permit = gate.acquire_owned().await;
            // asked late, as what's near can change while it waited
            if near.is_some_and(|n| !n.admits(&key)) {
                if labelled {
                    emit(Event::MediaLabel { key, label: Default::default() });
                }
                return;
            }
            let res = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                make_shaped_thumb(&k, &orig_path, &thumb_path, shape, own)
            })
            .await;
            if labelled {
                emit(Event::MediaLabel { key: key.clone(), label: Default::default() });
            }
            match res {
                Ok(Ok(t)) => emit(Event::Thumb(Arc::new(t))),
                _ => {
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
            }
        });
    }

    /// A video's picture, cut to `size` (logical px): its blurhash right away, its first frame once
    /// that can be had. Videos are downloaded one at a time, newest first, and only while the row
    /// is still in the window. `near`: its frame is made only while near the screen (the rows').
    pub(super) fn request_video_thumb(
        &self,
        key: String,
        file_key: String,
        ptr: AttachmentPointer,
        size: (u32, u32),
        auto: bool,
        near: Option<Near>,
    ) {
        let thumb_path = self.paths.thumbs.join(format!("{file_key}.enc"));
        let no_frame = no_frame_path(&self.paths.thumbs, &file_key);
        let length_path = length_path(&self.paths.thumbs, &file_key);
        let orig_path = attachment_path(&self.paths.attachments, &file_key);
        let (w, h) = (crate::types::physical_px(size.0), crate::types::physical_px(size.1));
        let own = ptr.width.zip(ptr.height).filter(|(w, h)| *w > 0 && *h > 0);
        let manager = self.manager.clone();
        let emit = self.emit.clone();
        let tx = self.tx.clone();
        let gate = self.thumb_gate.clone();
        let fetch_gate = self.video_gate.clone();
        let requested =
            if key.starts_with(crate::types::GALLERY_PREFIX) { self.gallery_requested.clone() } else { self.thumbs_requested.clone() };
        let downloads = self.attachment_downloads.clone();
        tokio::task::spawn_local(async move {
            if !thumb_path.exists() {
                if let Some(t) = ptr.blur_hash.as_deref().and_then(|b| blur_thumb(&key, b, w, h)) {
                    emit(Event::Thumb(Arc::new(t)));
                }
                if !orig_path.exists() {
                    emit(Event::MediaLabel { key: key.clone(), label: view::media::undownloaded_label(&ptr) });
                }
                if no_frame.exists() {
                    return;
                }
                if !orig_path.exists() {
                    // what a click asked for goes past the limits on what is fetched by itself
                    let asked = downloads.is_allowed(&file_key);
                    if !asked && !(auto && view::media::auto_fetches_video(&ptr)) {
                        return;
                    }
                    // by itself, videos take turns; a click doesn't wait for them
                    let _turn = if asked { None } else { Some(fetch_gate.acquire_owned().await) };
                    // it left the window meanwhile, or was stopped and stays so
                    if !requested.borrow().contains(&key) || downloads.was_stopped(&file_key) {
                        return;
                    }
                    if !orig_path.exists() {
                        if manager.is_none() {
                            return;
                        }
                        let show = Show::Cells(vec![key.clone()]);
                        match downloads.ensure(&file_key, &ptr, &show, &emit, &orig_path).await {
                            Ok(true) => {}
                            Ok(false) => return,
                            Err(e) => {
                                warn!("download video: {e:#}");
                                let _ = tx.send(Internal::ThumbFailed(key));
                                return;
                            }
                        }
                    }
                }
            }
            let permit = gate.acquire_owned().await;
            if near.is_some_and(|n| !n.admits(&key)) {
                return;
            }
            let k = key.clone();
            let emit_done = emit.clone();
            let res = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let t = make_video_thumb(&k, &orig_path, &thumb_path, &length_path, &no_frame, w, h, own)?;
                emit_done(Event::Thumb(Arc::new(t)));
                // the picture doesn't wait for this: a thumbnail cached before lengths were kept
                // has to read its video again
                let text = view::media::length_text(video_length(&length_path, &orig_path));
                // having the frame doesn't mean having the video
                let undownloaded = !orig_path.exists();
                emit_done(Event::MediaLabel { key: k, label: crate::types::MediaLabel { text, undownloaded, progress: None } });
                anyhow::Ok(())
            })
            .await;
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("video frame: {e:#}");
                    // it was downloaded, so its size no longer applies
                    emit(Event::MediaLabel { key: key.clone(), label: Default::default() });
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
                Err(_) => {
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
            }
        });
    }

    /// A click on video `index` of row `id`: plays it in the default app if it's here, and
    /// otherwise stops it if it's on its way or gets it. It doesn't play by itself when that's
    /// done: the user may be doing something else by then, and it would take the focus and start
    /// making noise.
    pub(super) async fn open_video(&mut self, id: &str, index: usize) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let Some((i, a)) = view::media::shown_attachments(dm).filter(|(_, a)| view::media::is_video(a)).nth(index) else { return };
        let file_key = view::media::attachment_key(a, ts, i);
        let path = attachment_path(&self.paths.attachments, &file_key);
        if !path.exists() {
            return self.toggle_download(&file_key).await;
        }
        let (tx, open_dir, name) = (self.tx.clone(), self.paths.open.clone(), view::media::open_name(a));
        tokio::task::spawn_local(async move { open_outside(&tx, path, &open_dir, name).await });
    }

    /// A click on a picture or video that isn't here: stops its download if it's on its way, and
    /// otherwise lets it download, past the limits on what is fetched by itself. The rows ask for
    /// their pictures again, so that is where it happens: one place downloads, however it started.
    async fn toggle_download(&mut self, file_key: &str) {
        if self.attachment_downloads.stop(file_key) {
            info!("download stopped by a click");
            return;
        }
        info!("download asked for by a click");
        self.attachment_downloads.allow(file_key);
        self.thumbs_requested.borrow_mut().retain(|k| !k.starts_with(file_key));
        self.emit_messages(Scroll::Keep).await;
        self.gallery_ask_again(file_key);
    }

    /// Downloads (or reads from the cache) a long message's full text.
    pub(super) fn request_long_text(&self, key: String, ptr: AttachmentPointer) {
        let path = attachment_path(&self.paths.attachments, &key);
        let (downloads, emit, tx) = (self.attachment_downloads.clone(), self.emit.clone(), self.tx.clone());
        let online = self.manager.is_some();
        tokio::task::spawn_local(async move {
            let here = if path.exists() {
                Ok(true)
            } else if online {
                downloads.ensure(&key, &ptr, &Show::Quiet, &emit, &path).await
            } else {
                Err(anyhow::anyhow!("offline"))
            };
            let data = match here {
                Ok(true) => sigwin_vault::read(&path).map_err(anyhow::Error::from),
                Ok(false) => Err(anyhow::anyhow!("stopped")),
                Err(e) => Err(e),
            };
            match data {
                Ok(d) => {
                    let text = view::text::trim_body(&String::from_utf8_lossy(&d), view::text::MAX_LONG_TEXT_BYTES).to_string();
                    let _ = tx.send(Internal::LongText(key, text));
                }
                Err(e) => {
                    warn!("download long text: {e:#}");
                    let _ = tx.send(Internal::ThumbFailed(key));
                }
            }
        });
    }

    /// The message's file line: its pictures and videos have their own place, in the row and the
    /// viewer. A file is never opened for the user (it could be a program): they choose where it
    /// goes, one file by name, several into a folder. `all` adds the pictures and videos (the
    /// hover toolbar's save).
    pub(super) fn save_files(&self, id: &str, owner: isize, all: bool) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let jobs: Vec<SaveJob> = view::media::shown_attachments(dm)
            .filter(|(_, a)| all || !view::media::is_media(a))
            .map(|(i, a)| {
                let file_key = view::media::attachment_key(a, ts, i);
                SaveJob { path: attachment_path(&self.paths.attachments, &file_key), file_key, ptr: a.clone() }
            })
            .collect();
        // the row's file line shows how far it is, and a click on it stops it; a row of pictures
        // alone has no line, so that goes to the notice
        let line = view::media::shown_attachments(dm).any(|(_, a)| !view::media::is_media(a));
        self.ask_where_to_save(Save { id: id.to_string(), line, jobs }, owner);
    }

    /// The user picks where `save` goes: a file's path for one, a folder for several.
    pub(super) fn ask_where_to_save(&self, save: Save, owner: isize) {
        let tx = self.tx.clone();
        match save.jobs.as_slice() {
            [] => {}
            [SaveJob { ptr, .. }] => crate::picker::spawn_save(owner, view::media::saved_name(ptr), move |dst| {
                if let Some(dst) = dst {
                    let _ = tx.send(Internal::SaveFiles(save, SaveTo::File(dst)));
                }
            }),
            _ => crate::picker::spawn_folder(owner, move |dir| {
                if let Some(dir) = dir {
                    let _ = tx.send(Internal::SaveFiles(save, SaveTo::Folder(dir)));
                }
            }),
        }
    }

    /// Downloads what isn't here yet and writes the files where the user chose.
    pub(super) fn save_files_to(&self, save: Save, to: SaveTo) {
        let downloads = self.attachment_downloads.clone();
        let online = self.manager.is_some();
        let (tx, emit) = (self.tx.clone(), self.emit.clone());
        tokio::task::spawn_local(async move {
            let Save { id, line, jobs } = save;
            let key = crate::types::file_line_key(&id);
            let show = if line { Show::Cells(vec![key.clone()]) } else { Show::Toast };
            let run = downloads.begin_save(&id);
            let mut saved = Vec::new();
            for SaveJob { file_key, path, ptr } in &jobs {
                if !path.exists() {
                    if !online {
                        break;
                    }
                    downloads.saving(&id, run, file_key);
                    let mut got = downloads.ensure(file_key, ptr, &show, &emit, path).await;
                    // stopped by someone else (a click on the same picture): the save still wants it.
                    // Stopped from the file line: nothing more is saved
                    if matches!(got, Ok(false)) && !downloads.save_stopped(&id, run) {
                        got = downloads.ensure(file_key, ptr, &show, &emit, path).await;
                    }
                    if !line {
                        let _ = tx.send(Internal::ProgressDone);
                    }
                    if downloads.save_stopped(&id, run) {
                        break;
                    }
                    match got {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(e) => {
                            let _ = tx.send(Internal::Notice(tr!("Download failed: {}", format!("{e:#}"))));
                            continue;
                        }
                    }
                }
                let dst = match &to {
                    SaveTo::File(f) => f.clone(),
                    SaveTo::Folder(dir) => free_name(dir, &view::media::saved_name(ptr)),
                };
                match sigwin_vault::decrypt_file(path, &dst) {
                    Ok(()) => {
                        mark_from_internet(&dst);
                        saved.push(dst)
                    }
                    Err(e) => {
                        let _ = tx.send(Internal::Notice(tr!("Save failed: {}", e)));
                        saved.clear();
                        break;
                    }
                }
            }
            downloads.end_save(&id, run);
            // the line goes back to its name and size, however it ended
            if line {
                emit(Event::MediaLabel { key, label: Default::default() });
            }
            if let Some(last) = saved.pop() {
                let text = tr!("Saved {n} attachment." | "Saved {n} attachments." % saved.len() + 1);
                let _ = tx.send(Internal::Saved(text, last));
            }
        });
    }

    /// The message of row `id` in the open window.
    /// Message `id` of the window, or of the media page (which reaches back past the window).
    pub(super) fn row_message(&self, id: &str) -> Option<(u64, &DataMessage)> {
        let open = self.current.as_ref()?;
        let ts = id.parse::<u64>().ok()?;
        if let Some((_, c)) = open.window.iter().find(|(t, _)| *t == ts) {
            return match view::classify(c, self.me) {
                Kind::Msg { dm, .. } => Some((ts, dm)),
                _ => None,
            };
        }
        self.gallery.as_ref()?.message(ts).map(|dm| (ts, dm))
    }

    /// Image `index` of row `id`, to save or open elsewhere: cache path and pointer. Never a
    /// view-once one.
    pub(super) fn viewed(&self, id: &str, index: usize) -> Option<(PathBuf, AttachmentPointer)> {
        if self.row_message(id)?.1.is_view_once.unwrap_or(false) {
            return None;
        }
        let (_, path, ptr) = self.viewed_key(id, index)?;
        Some((path, ptr))
    }

    /// `viewed`, with the attachment's file key too, and a view-once picture's.
    fn viewed_key(&self, id: &str, index: usize) -> Option<(String, PathBuf, AttachmentPointer)> {
        let (ts, dm) = self.row_message(id)?;
        let (i, a) = viewer_images(dm, ts).into_iter().nth(index)?;
        let key = view::media::attachment_key(a, ts, i);
        let path = attachment_path(&self.paths.attachments, &key);
        Some((key, path, a.clone()))
    }

    /// A click on image `index` of row `id` that isn't here: stops its download or starts it, as
    /// for a video.
    pub(super) async fn toggle_image_download(&mut self, id: &str, index: usize) {
        let Some((file_key, path, _)) = self.viewed_key(id, index) else { return };
        if !path.exists() {
            self.toggle_download(&file_key).await;
        }
    }

    pub(super) fn view_image(&mut self, id: &str, index: usize, fit: (u32, u32)) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let once = dm.is_view_once.unwrap_or(false);
        let count = viewer_images(dm, ts).len();
        if count == 0 {
            if once {
                // opened on another device meanwhile, or past its time
                self.notice(tr!("This view-once media can't be opened anymore."));
            }
            return;
        }
        let index = index.min(count - 1);
        let Some((file_key, path, mut ptr)) = self.viewed_key(id, index) else { return };
        if once {
            // the blurhash would be a preview of what is meant to be seen once
            ptr.blur_hash = None;
        }
        let id = id.to_string();
        let emit = self.emit.clone();
        let tx = self.tx.clone();
        let open_dir = self.paths.open.clone();
        let online = self.manager.is_some();
        let downloads = self.attachment_downloads.clone();
        // its cells in the list show the download too
        let mut keys: Vec<String> = self.thumbs_requested.borrow().iter().filter(|k| k.starts_with(&file_key)).cloned().collect();
        // what played before stops now; a GIF opened here plays until the next one replaces it
        let start = self.new_gif_stop();
        tokio::task::spawn_local(async move {
            // going to a picture that isn't here asks for it: its blurhash shows meanwhile
            if !path.exists() {
                if !online {
                    let _ = tx.send(Internal::Notice(tr!("This image hasn't been downloaded yet.").into()));
                    return;
                }
                let pending = crate::types::viewer_key(&file_key);
                let pixels = placeholder(&ptr, fit);
                emit(Event::ViewerImage(crate::types::ViewerImage { id: id.clone(), index, count, pixels, animated: false, pending: pending.clone(), once }));
                keys.push(pending);
                match downloads.ensure(&file_key, &ptr, &Show::Cells(keys), &emit, &path).await {
                    Ok(true) => {
                        // the list makes its thumbnails from it now
                        if !once {
                            let _ = tx.send(Internal::Fetched(file_key));
                        }
                    }
                    // stopped: the blurhash stays, and its mark starts it again
                    Ok(false) => return,
                    Err(e) => {
                        warn!("download attachment: {e:#}");
                        let _ = tx.send(Internal::Notice(tr!("Download failed: {}", format!("{e:#}"))));
                        return;
                    }
                }
            }
            // a GIF that played before lets go of its file first
            let Some(turn) = start.turn().await else { return };
            let p = path.clone();
            enum Decoded {
                Still(crate::types::ViewerPixels),
                Gif(Vec<u8>, crate::types::ViewerPixels),
            }
            let decoded = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let bytes = sigwin_vault::read(&p)?;
                if sigwin_thumb::is_animated_gif(&bytes) {
                    let Some(first) = super::gif::first_frame(&bytes, fit) else { return Ok(None) };
                    return Ok(Some(Decoded::Gif(bytes, first)));
                }
                let Some(view) = sigwin_thumb::decode_view(&bytes, fit.0, fit.1, VIEW_MAX_PIXELS)? else {
                    return Ok(None);
                };
                drop(bytes);
                Ok(Some(Decoded::Still(crate::types::ViewerPixels::from(view))))
            })
            .await;
            match decoded {
                Ok(Ok(Some(Decoded::Still(pixels)))) => {
                    let _ = tx.send(Internal::ProgressDone);
                    emit(Event::ViewerImage(crate::types::ViewerImage { id, index, count, pixels, animated: false, pending: String::new(), once }));
                }
                Ok(Ok(Some(Decoded::Gif(bytes, pixels)))) => {
                    let _ = tx.send(Internal::ProgressDone);
                    let px = pixels.full().0.max(pixels.full().1);
                    emit(Event::ViewerImage(crate::types::ViewerImage { id: id.clone(), index, count, pixels, animated: true, pending: String::new(), once }));
                    if !start.stopped() {
                        let slot = Arc::new(crate::types::GifSlot::new(id, super::gif::next_gen(), crate::types::GifTarget::Viewer(index)));
                        super::gif::stream(bytes, px, slot, start.stop, emit, None, turn);
                    }
                }
                other => {
                    if let Ok(Err(e)) = other {
                        warn!("view image: {e:#}");
                    }
                    if once {
                        // an outside app could keep a copy: not used up, the phone can still open it
                        let _ = std::fs::remove_file(&path);
                        let _ = tx.send(Internal::Notice(tr!("This picture can't be shown here. Open it on your phone.").into()));
                        return;
                    }
                    // animated (not a GIF), huge or undecodable: an outside app may do better
                    open_outside(&tx, path, &open_dir, view::media::open_name(&ptr)).await;
                }
            }
        });
    }

    /// One at a time, in order. Failures leave a blank cell: notices belong to the image being
    /// viewed.
    pub(super) fn viewer_thumbs(&self, id: &str, px: u32) {
        let Some((ts, dm)) = self.row_message(id) else { return };
        let images = viewer_images(dm, ts).into_iter();
        let jobs: Vec<(String, Option<String>, Option<u32>)> =
            images.map(|(i, a)| (view::media::attachment_key(a, ts, i), a.blur_hash.clone(), own_side(a))).collect();
        let id = id.to_string();
        let emit = self.emit.clone();
        let (attachments, thumbs) = (self.paths.attachments.clone(), self.paths.thumbs.clone());
        tokio::task::spawn_local(async move {
            for (index, (key, blur, own)) in jobs.into_iter().enumerate() {
                let orig = attachment_path(&attachments, &key);
                let thumb = thumbs.join(format!("{key}.enc"));
                // not here: its blurhash, until going to it gets it
                if !thumb.exists() && !orig.exists() {
                    if let Some(t) = blur.as_deref().and_then(|b| blur_thumb(&key, b, px, px)) {
                        emit(Event::ViewerThumb(id.clone(), index, t));
                    }
                    continue;
                }
                let made = tokio::task::spawn_blocking(move || {
                    let target = Target::Cover(px, px);
                    let source = thumb_source(|| Ok(sigwin_vault::read(&orig)?), view::media::PHOTO_THUMB_MAX, &thumb, target, own)?;
                    anyhow::Ok(target.draw(&key, source))
                })
                .await;
                match made {
                    Ok(Ok(t)) => emit(Event::ViewerThumb(id.clone(), index, t)),
                    Ok(Err(e)) => warn!("viewer thumbnail: {e:#}"),
                    Err(e) => warn!("viewer thumbnail: {e}"),
                }
            }
        });
    }

    pub(super) fn open_viewed(&self, id: &str, index: usize) {
        let Some((path, ptr)) = self.viewed(id, index) else { return };
        let tx = self.tx.clone();
        let open_dir = self.paths.open.clone();
        tokio::task::spawn_local(async move {
            open_outside(&tx, path, &open_dir, view::media::open_name(&ptr)).await;
        });
    }

    /// Puts the picture on the clipboard. It is decoded in full here, so a limit well under
    /// `VIEW_MAX_PIXELS` applies: the clipboard gets the PNG and the DIB, both held at once.
    pub(super) fn copy_viewed(&self, id: &str, index: usize) {
        let Some((path, _)) = self.viewed(id, index) else { return };
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let done = (|| -> anyhow::Result<()> {
                let bytes = sigwin_vault::read(&path).map_err(|_| anyhow::anyhow!(tr!("This image hasn't been downloaded yet.")))?;
                let img = sigwin_thumb::decode_within(&bytes, CLIP_MAX_PIXELS)
                    .map_err(|_| anyhow::anyhow!(tr!("This image is too large to copy.")))?;
                drop(bytes);
                let rgba = img.to_rgba8();
                drop(img);
                let png = sigwin_thumb::png_rgba(&rgba)?;
                crate::clip::set_image(&png, rgba.as_raw(), rgba.width(), rgba.height())
            })();
            let _ = tx.send(Internal::Notice(match done {
                Ok(()) => tr!("Copied.").into(),
                Err(e) => tr!("Copy failed: {}", format!("{e:#}")),
            }));
        });
    }

    pub(super) fn save_viewed(&self, id: &str, index: usize, owner: isize) {
        let Some((path, ptr)) = self.viewed(id, index) else { return };
        let tx = self.tx.clone();
        crate::picker::spawn_save(owner, view::media::saved_name(&ptr), move |dst| {
            let Some(dst) = dst else { return };
            let _ = tx.send(match sigwin_vault::decrypt_file(&path, &dst) {
                Ok(()) => {
                    mark_from_internet(&dst);
                    Internal::Saved(tr!("Saved.").into(), dst)
                }
                Err(e) => Internal::Notice(tr!("Save failed: {}", e)),
            });
        });
    }
}

/// A file to save: its key, where it is (or will be) cached, and what the message says of it.
pub(super) struct SaveJob {
    pub file_key: String,
    pub path: PathBuf,
    pub ptr: AttachmentPointer,
}

/// The files of row `id` the user asked to save. `line`: the row has a file line, which shows
/// how far it is.
pub(super) struct Save {
    pub id: String,
    pub line: bool,
    pub jobs: Vec<SaveJob>,
}

/// Where the user chose to save files: one file's whole path, or a folder for several.
pub(super) enum SaveTo {
    File(PathBuf),
    Folder(PathBuf),
}

/// `name` in `dir`, or `name (2)`, `name (3)`… if it is taken: saving several files never
/// overwrites one.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .unwrap_or(first)
}

/// Safety limit, not a budget: more pixels than this are abnormal input and go to an outside app.
/// The peak is 7 bytes per pixel for photos, 8 with transparency; the viewer holds the original at 4
/// until it closes.
const VIEW_MAX_PIXELS: u64 = 100_000_000;

/// The most pixels copied to the clipboard (about 150MB of RGBA while it is encoded).
const CLIP_MAX_PIXELS: u64 = 36_000_000;

/// What presage calls itself to the CDN: its own constant is private, and a download that says
/// something else would stand out for no reason.
pub(super) const PRESAGE_USER_AGENT: &str = "presage-rs-0.8.0-dev";

/// For downloads: the CDN takes no credentials.
pub(super) fn cdn_service() -> presage::libsignal_service::push_service::PushService {
    presage::libsignal_service::push_service::PushService::new(SignalServers::Production, None, PRESAGE_USER_AGENT)
}

/// Where a download shows how far it is.
pub(super) enum Show {
    /// on these pictures (their keys in the UI): a size and a ring, and what they were when it ends
    Cells(Vec<String>),
    /// in the notice at the bottom (a file being saved, which has no picture)
    Toast,
    /// nowhere
    Quiet,
}

/// The only place attachments are downloaded: pictures, videos, files being saved and long texts
/// all come through `ensure`, so one is never fetched twice, and a click on a picture can stop
/// whichever of them is on its way.
///
/// One that was stopped isn't downloaded by itself again while the app runs, or it would start
/// over when its row comes back; the user's next click on it (`allow`) lets it.
#[derive(Clone, Default)]
pub(super) struct AttachmentDownloads {
    running: Rc<RefCell<HashMap<String, AbortHandle>>>,
    stopped: Rc<RefCell<HashSet<String>>>,
    /// asked for by a click: downloaded whatever the size or type that is fetched by itself
    allowed: Rc<RefCell<HashSet<String>>>,
    /// a download ended (done, failed or stopped)
    ended: Rc<tokio::sync::Notify>,
    /// row id -> the save of its files under way: which run that is (a stopped one may still be
    /// winding down when the next begins) and the file it is downloading
    saves: Rc<RefCell<HashMap<String, (u64, Option<String>)>>>,
    save_runs: Rc<std::cell::Cell<u64>>,
    /// file key -> more places that show a download on its way, which came to wait for it (the
    /// viewer, for a picture the list is getting)
    watchers: Rc<RefCell<HashMap<String, Vec<String>>>>,
}

impl AttachmentDownloads {
    fn is_running(&self, file_key: &str) -> bool {
        self.running.borrow().contains_key(file_key)
    }

    fn was_stopped(&self, file_key: &str) -> bool {
        self.stopped.borrow().contains(file_key)
    }

    /// Whether a click asked for it, so the limits on what is fetched by itself don't apply.
    fn is_allowed(&self, file_key: &str) -> bool {
        self.allowed.borrow().contains(file_key)
    }

    /// How many a click asked for (what a fixture checks).
    pub(super) fn allowed_count(&self) -> usize {
        self.allowed.borrow().len()
    }

    /// The user asked for it (a click): it may start again after a stop, and past the limits.
    fn allow(&self, file_key: &str) {
        self.stopped.borrow_mut().remove(file_key);
        self.allowed.borrow_mut().insert(file_key.to_string());
    }

    /// False if it isn't on its way.
    fn stop(&self, file_key: &str) -> bool {
        let Some(handle) = self.running.borrow_mut().remove(file_key) else { return false };
        handle.abort();
        self.stopped.borrow_mut().insert(file_key.to_string());
        self.allowed.borrow_mut().remove(file_key);
        self.ended.notify_waiters();
        true
    }

    /// A save of row `id`'s files begins; what it returns names this one.
    pub(super) fn begin_save(&self, id: &str) -> u64 {
        let run = self.save_runs.get() + 1;
        self.save_runs.set(run);
        self.saves.borrow_mut().insert(id.to_string(), (run, None));
        run
    }

    /// The save is downloading `file_key` now: that is what stopping it stops.
    pub(super) fn saving(&self, id: &str, run: u64, file_key: &str) {
        if let Some((r, current)) = self.saves.borrow_mut().get_mut(id) {
            if *r == run {
                *current = Some(file_key.to_string());
            }
        }
    }

    /// Whether the save was stopped (or another took its place): it saves nothing more.
    pub(super) fn save_stopped(&self, id: &str, run: u64) -> bool {
        self.saves.borrow().get(id).is_none_or(|(r, _)| *r != run)
    }

    pub(super) fn end_save(&self, id: &str, run: u64) {
        let mut saves = self.saves.borrow_mut();
        if saves.get(id).is_some_and(|(r, _)| *r == run) {
            saves.remove(id);
        }
    }

    /// A click on the file line of row `id`: its save stops, and the download it waits for.
    pub(super) fn stop_save(&self, id: &str) {
        let Some((_, current)) = self.saves.borrow_mut().remove(id) else { return };
        info!("save stopped by a click");
        if let Some(file_key) = current {
            self.stop(&file_key);
        }
    }

    /// The attachment is at `dest` when this returns true. One that is on its way already is
    /// waited for, not downloaded again; false: it was stopped or failed meanwhile, which is not
    /// for this caller to start over. Nothing on its way and not here: downloaded now, `show`ing
    /// how far it is; false when stopped.
    pub(super) async fn ensure(&self, file_key: &str, ptr: &AttachmentPointer, show: &Show, emit: &Emit, dest: &Path) -> anyhow::Result<bool> {
        if self.is_running(file_key) {
            if let Show::Cells(keys) = show {
                let mut watchers = self.watchers.borrow_mut();
                let shown = watchers.entry(file_key.to_string()).or_default();
                shown.extend(keys.iter().filter(|k| !shown.contains(k)).cloned().collect::<Vec<_>>());
            }
            while self.is_running(file_key) {
                self.ended.notified().await;
            }
            return Ok(dest.exists());
        }
        if dest.exists() {
            return Ok(true);
        }
        if gone_path(dest).exists() {
            return Err(Gone.into());
        }
        self.run(file_key, ptr, show, emit, dest).await
    }

    /// Downloads and saves it, as `Manager::get_attachment` does, but saying how far along it is.
    /// `false`: it was stopped. When it doesn't finish what shows it goes back to how it was.
    async fn run(&self, file_key: &str, ptr: &AttachmentPointer, show: &Show, emit: &Emit, dest: &Path) -> anyhow::Result<bool> {
        let (handle, registration) = AbortHandle::new_pair();
        self.running.borrow_mut().insert(file_key.to_string(), handle.clone());
        self.stopped.borrow_mut().remove(file_key);
        self.watchers.borrow_mut().remove(file_key);
        let show_now = |label: crate::types::MediaLabel| match show {
            Show::Cells(keys) => {
                let watchers = self.watchers.borrow().get(file_key).cloned().unwrap_or_default();
                for key in keys.iter().chain(watchers.iter().filter(|k| !keys.contains(k))) {
                    emit(Event::MediaLabel { key: key.clone(), label: label.clone() });
                }
            }
            Show::Toast => {
                if let Some(percent) = label.progress {
                    emit(Event::Progress(tr!("Downloading attachment… {}%", percent)));
                }
            }
            Show::Quiet => {}
        };
        show_now(view::media::downloading_label(ptr, 0, 0));
        let started = std::time::Instant::now();
        // saved before it counts as done: whoever waits for it finds the file
        let res = Abortable::new(download(ptr, dest, |got, percent| show_now(view::media::downloading_label(ptr, got, percent))), registration).await;
        let res = match res {
            Ok(Ok(())) => Ok(Ok(())),
            Ok(Err(e)) if is_not_found(&e) => {
                if let Err(e) = std::fs::write(gone_path(dest), []) {
                    warn!("save gone marker: {e}");
                }
                Ok(Err(Gone.into()))
            }
            Ok(Err(e)) => Ok(Err(e)),
            Err(aborted) => Err(aborted),
        };
        // a stopped one is already out of the list, and a new one may have taken its place
        if !handle.is_aborted() {
            self.running.borrow_mut().remove(file_key);
            self.ended.notify_waiters();
        }
        // however it ended, what showed its progress stops showing it here, whoever started it:
        // waiting for the caller to clear it left a full ring on cells a different caller got
        // the file for (the viewer)
        show_now(match &res {
            Ok(Ok(())) => Default::default(),
            Ok(Err(e)) if e.is::<Gone>() => view::media::gone_label(),
            _ => view::media::undownloaded_label(ptr),
        });
        // unless a new one has started meanwhile, with watchers of its own
        if !self.is_running(file_key) {
            self.watchers.borrow_mut().remove(file_key);
        }
        info!(
            result = match &res {
                Ok(Ok(())) => "done",
                Ok(Err(e)) if e.is::<Gone>() => "gone",
                Ok(Err(_)) => "failed",
                Err(_) => "stopped",
            },
            kb = ptr.size.unwrap_or(0) / 1024,
            ms = started.elapsed().as_millis() as u64,
            "attachment download"
        );
        match res {
            Ok(res) => res.map(|()| true),
            Err(_) => Ok(false),
        }
    }
}

/// Downloads the attachment into the vault file `dest`, decrypted and sealed again a piece at a
/// time on a thread of its own, so a large one is never whole in memory. `progress` gets the
/// bytes so far and their percent, each time that goes up.
async fn download(ptr: &AttachmentPointer, dest: &Path, mut progress: impl FnMut(u64, u8)) -> anyhow::Result<()> {
    use futures::AsyncReadExt;
    // history imported at link time has none for media the phone had downloaded: the MAC
    // checked on decrypting still authenticates it
    let digest = ptr.digest.clone();
    let key: [u8; 64] = ptr.key().try_into().context("attachment key")?;
    // no credentials: the CDN doesn't take any. Not kept: downloads are rare, and an idle client
    // and its connections would stay in memory.
    let mut service = cdn_service();
    let mut download = service.get_attachment(ptr).await?;
    let total = download.content_length.or(ptr.size.map(u64::from));
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let size = ptr.size.map(u64::from);
    let dest = dest.to_path_buf();
    // a download that ends early drops `tx` before its end mark: the file is dropped with it
    let write = tokio::task::spawn_blocking(move || sigwin_vault::write_from(&dest, opening::Opening::new(opening::Pieces(rx), key, digest, size)));
    let mut got = 0u64;
    let mut last = 0;
    loop {
        let mut chunk = vec![0; 64 * 1024];
        let n = download.stream.read(&mut chunk).await.context("read attachment")?;
        if n == 0 {
            break;
        }
        chunk.truncate(n);
        got += n as u64;
        // the writer gave up: its error says why
        if tx.send(Some(chunk)).await.is_err() {
            break;
        }
        let now = percent(got, total);
        if now != last {
            last = now;
            progress(got, now);
        }
    }
    let _ = tx.send(None).await;
    drop(tx);
    Ok(write.await.context("decrypt attachment")??)
}

/// How much of `total` bytes `got` is. An unknown total stays at 0.
fn percent(got: u64, total: Option<u64>) -> u8 {
    total.filter(|t| *t > 0).map_or(0, |t| (got.saturating_mul(100) / t).min(100) as u8)
}

/// Decrypts a cached attachment to `open/` and opens it with the default app, under `name`
/// (`view::media::open_name`). `None`: a kind the shell might run, which stays here.
async fn open_outside(tx: &UnboundedSender<Internal>, path: PathBuf, open_dir: &Path, name: Option<String>) {
    let Some(name) = name else {
        let _ = tx.send(Internal::ProgressDone);
        let _ = tx.send(Internal::Notice(tr!("This attachment can't be opened from here. You can save it instead.").into()));
        return;
    };
    let dir = open_dir.to_path_buf();
    match tokio::task::spawn_blocking(move || decrypt_for_open(&path, &dir, &name)).await {
        Ok(Ok(plain)) => {
            let _ = tx.send(Internal::ProgressDone);
            crate::shell::open(plain.as_os_str());
        }
        Ok(Err(e)) => {
            let _ = tx.send(Internal::Notice(tr!("Couldn't open the attachment: {}", e)));
        }
        Err(_) => {}
    }
}

/// Downloaded attachments and thumbnails of an expired message.
pub(super) fn remove_files(c: &Content, ts: u64, attachments: &Path, thumbs: &Path) {
    let Some(dm) = view::data_message(c) else { return };
    let mut keys: Vec<String> = dm.attachments.iter().enumerate().map(|(i, a)| view::media::attachment_key(a, ts, i)).collect();
    if let Some(data) = dm.sticker.as_ref().and_then(|s| s.data.as_ref()) {
        keys.push(view::media::attachment_key(data, ts, 99));
    }
    keys.extend(dm.preview.iter().filter_map(|p| p.image.as_ref()).map(|a| view::media::attachment_key(a, ts, view::media::LINK_IMAGE_INDEX)));
    for k in keys {
        let _ = std::fs::remove_file(attachment_path(attachments, &k));
        let _ = std::fs::remove_file(gone_path(&attachment_path(attachments, &k)));
        let _ = std::fs::remove_file(thumbs.join(format!("{k}.enc")));
        let _ = std::fs::remove_file(no_frame_path(thumbs, &k));
        let _ = std::fs::remove_file(length_path(thumbs, &k));
    }
}

/// A video's seconds, encrypted like everything else derived from an attachment. Kept next to its
/// frame, which is what the cache is emptied of.
fn length_path(thumbs: &Path, key: &str) -> PathBuf {
    thumbs.join(format!("{key}.len"))
}

fn save_length(path: &Path, seconds: u32) {
    if let Err(e) = sigwin_vault::write_cache(path, &seconds.to_le_bytes()) {
        warn!("save video length: {e}");
    }
}

/// The saved length, or else the one read from the video if that is here. 0: not known. A video
/// that doesn't say isn't asked again.
fn video_length(path: &Path, orig: &Path) -> u32 {
    if let Some(saved) = sigwin_vault::read(path).ok().and_then(|b| <[u8; 4]>::try_from(b).ok()) {
        return u32::from_le_bytes(saved);
    }
    if !orig.exists() {
        return 0;
    }
    match crate::videoframe::first_frame(orig, view::media::PHOTO_THUMB_MAX) {
        Ok(v) => {
            let seconds = v.map_or(0, |v| v.seconds);
            save_length(path, seconds);
            seconds
        }
        Err(e) => {
            warn!("video length: {e:#}");
            0
        }
    }
}

/// The CDN doesn't have the attachment (404): it keeps them only for some weeks, so it never will.
#[derive(Debug)]
pub(super) struct Gone;

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(tr!("This attachment is no longer available on the server"))
    }
}

impl std::error::Error for Gone {}

fn is_not_found(e: &anyhow::Error) -> bool {
    use presage::libsignal_service::prelude::ServiceError;
    e.chain().any(|c| match c.downcast_ref::<ServiceError>() {
        Some(ServiceError::NotFoundError) => true,
        Some(ServiceError::Http(r)) => r.status().is_some_and(|s| s.as_u16() == 404),
        _ => false,
    })
}

/// Empty file next to where the attachment would be, saying the server no longer has it: without
/// it every visit to the row asked the CDN again.
fn gone_path(attachment: &Path) -> PathBuf {
    attachment.with_extension("gone")
}

/// Empty file saying a video's frame can't be decoded here, so it isn't tried on every visit.
/// Emptying the thumbnail cache forgets it (a codec may have been installed since).
fn no_frame_path(thumbs: &Path, key: &str) -> PathBuf {
    thumbs.join(format!("{key}.none"))
}

/// The blurhash is in the message and is never saved. Drawn small, then enlarged smoothly: it has
/// no detail to lose, and the cost of drawing grows with every pixel.
fn blur_thumb(key: &str, hash: &str, w: u32, h: u32) -> Option<Thumb> {
    const DRAWN: u32 = 32;
    let (bw, bh) = if w >= h { (DRAWN, (DRAWN * h / w.max(1)).max(1)) } else { ((DRAWN * w / h.max(1)).max(1), DRAWN) };
    let cell = sigwin_thumb::cover(&sigwin_thumb::blurhash(hash, bw, bh)?, w, h);
    Some(Thumb { key: key.to_string(), width: cell.width(), height: cell.height(), rgba: cell.into_raw() })
}

/// The blurhash of a picture shown with `shape`, at the size its box has in the list.
/// What the viewer shows of a picture on its way: its blurhash (or plain dark without one) at
/// the size the picture will have, within `fit`.
/// The pictures the viewer steps through: (index in `attachments`, pointer). A view-once
/// message's one picture while it can still be opened.
fn viewer_images(dm: &DataMessage, ts: u64) -> Vec<(usize, &AttachmentPointer)> {
    if dm.is_view_once.unwrap_or(false) {
        return view::media::view_once_photo(dm, ts, view::time::now_ms()).into_iter().collect();
    }
    view::media::shown_attachments(dm).filter(|(_, a)| view::media::is_image(a)).collect()
}

fn placeholder(ptr: &AttachmentPointer, fit: (u32, u32)) -> crate::types::ViewerPixels {
    let (w, h) = match (ptr.width, ptr.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => (4, 3),
    };
    let scale = (fit.0 as f64 / w as f64).min(fit.1 as f64 / h as f64);
    // a picture with no size given takes the box; a known one never grows past its own size
    let scale = if ptr.width.is_some() { scale.min(1.0) } else { scale };
    let (sw, sh) = (((w as f64 * scale) as u32).max(1), ((h as f64 * scale) as u32).max(1));
    let img = ptr
        .blur_hash
        .as_deref()
        .and_then(|b| sigwin_thumb::blurhash(b, 32, 32))
        .map(|b| sigwin_thumb::cover(&b, sw, sh))
        .unwrap_or_else(|| image::RgbaImage::from_pixel(sw, sh, image::Rgba([0x2a, 0x2a, 0x2a, 0xff])));
    let original = slint::SharedPixelBuffer::clone_from_slice(img.as_raw(), sw, sh);
    crate::types::ViewerPixels { original, fitted: None, opaque: true }
}

fn blur_for(key: &str, hash: &str, ptr: &AttachmentPointer, shape: &ThumbShape) -> Option<Thumb> {
    let (w, h) = match *shape {
        ThumbShape::Cover(w, h) => (w, h),
        // the longer side px long, as the sticker will be
        ThumbShape::Fit(px) => match (ptr.width, ptr.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => {
                let s = f64::from(px) / f64::from(w.max(h));
                (((f64::from(w) * s).round() as u32).max(1), ((f64::from(h) * s).round() as u32).max(1))
            }
            _ => (px, px),
        },
    };
    blur_thumb(key, hash, crate::types::physical_px(w), crate::types::physical_px(h))
}

/// `own`: the video's size as its sender gave it.
#[allow(clippy::too_many_arguments)]
fn make_video_thumb(
    key: &str,
    orig: &Path,
    thumb: &Path,
    length: &Path,
    no_frame: &Path,
    w: u32,
    h: u32,
    own: Option<(u32, u32)>,
) -> anyhow::Result<Thumb> {
    let target = Target::Cover(w, h);
    let cached = if thumb.exists() { load_cached(thumb).ok() } else { None };
    // having the frame doesn't mean having the video
    let frame = match cached {
        Some(f) if !orig.exists() || !too_small(&f, target, own.map(|(w, h)| w.max(h))) => f,
        _ => {
            let side = target.side(own.unwrap_or((w, h))).max(view::media::PHOTO_THUMB_MAX);
            let Some(v) = crate::videoframe::first_frame(orig, side)? else {
                let _ = std::fs::write(no_frame, []);
                anyhow::bail!("not decodable here");
            };
            save_cached(thumb, sigwin_thumb::png_rgba(&v.frame));
            save_length(length, v.seconds);
            v.frame
        }
    };
    Ok(target.draw(key, frame))
}

fn make_shaped_thumb(key: &str, orig: &Path, thumb: &Path, shape: ThumbShape, own: Option<u32>) -> anyhow::Result<Thumb> {
    let target = shape.target();
    let source = thumb_source(|| Ok(sigwin_vault::read(orig)?), view::media::PHOTO_THUMB_MAX, thumb, target, own)?;
    Ok(target.draw(key, source))
}

/// The longer side of an attachment as its sender gave it.
fn own_side(ptr: &AttachmentPointer) -> Option<u32> {
    Some(ptr.width?.max(ptr.height?)).filter(|s| *s > 0)
}

/// What a picture is drawn at, physical px.
#[derive(Clone, Copy)]
pub(super) enum Target {
    /// the longer side is this long
    Fit(u32),
    /// fills this size, cut off around its center
    Cover(u32, u32),
}

impl ThumbShape {
    fn target(&self) -> Target {
        let px = crate::types::physical_px;
        match *self {
            ThumbShape::Fit(fit) => Target::Fit(px(fit)),
            ThumbShape::Cover(w, h) => Target::Cover(px(w), px(h)),
        }
    }
}

impl Target {
    /// The longer side a source of `size` needs to be drawn without being enlarged.
    fn side(self, size: (u32, u32)) -> u32 {
        match self {
            Target::Fit(px) => px,
            Target::Cover(w, h) => sigwin_thumb::cover_side(size, (w, h)),
        }
    }

    /// Resampled to exactly its size: the software renderer would scale it nearest-neighbor,
    /// which shreds stickers.
    pub(super) fn draw(self, key: &str, source: image::RgbaImage) -> Thumb {
        let img = match self {
            Target::Fit(px) => sigwin_thumb::fit(source, px),
            Target::Cover(w, h) => sigwin_thumb::cover(&source, w, h),
        };
        Thumb { key: key.to_string(), width: img.width(), height: img.height(), rgba: img.into_raw() }
    }
}

/// Whether a cached source would be enlarged for `target` while the original has more. That
/// comes from `own`, the sender's word for its longer side: a cache holds small pictures enlarged
/// (`decode_thumb_for`), so it can't tell. Without it, the cache is kept.
fn too_small(cached: &image::RgbaImage, target: Target, own: Option<u32>) -> bool {
    let side = cached.width().max(cached.height());
    side < target.side(cached.dimensions()) && own.is_some_and(|o| o > side)
}

/// A cached (encrypted) PNG.
pub(crate) fn load_cached(path: &Path) -> anyhow::Result<image::RgbaImage> {
    Ok(sigwin_thumb::decode_rgba(&sigwin_vault::read(path)?)?)
}

pub(crate) fn save_cached(path: &Path, png: image::ImageResult<Vec<u8>>) {
    if let Err(e) = png.map_err(anyhow::Error::from).and_then(|b| Ok(sigwin_vault::write_cache(path, &b)?)) {
        warn!("save {}: {e}", path.display());
    }
}

/// What a picture is drawn from at `target`: its cached PNG, or one decoded from `bytes` (sniffing
/// the format, file names may lie) and cached. That holds at least `floor` on its longer side,
/// and enough for `target` as far as the original has it. A cache made for a smaller size is
/// made again (`too_small`, with `own`), unless the original can't be had.
pub(super) fn thumb_source(
    bytes: impl FnOnce() -> anyhow::Result<Vec<u8>>,
    floor: u32,
    thumb: &Path,
    target: Target,
    own: Option<u32>,
) -> anyhow::Result<image::RgbaImage> {
    let cached = if thumb.exists() { load_cached(thumb).ok() } else { None };
    let cached = match cached {
        Some(c) if !too_small(&c, target, own) => return Ok(c),
        c => c,
    };
    let made = bytes().and_then(|b| Ok(sigwin_thumb::decode_thumb_for(&b, floor, |size| target.side(size))?));
    match (made, cached) {
        (Ok(img), _) => {
            save_cached(thumb, sigwin_thumb::png(&img));
            Ok(sigwin_thumb::to_rgba(&img))
        }
        (Err(_), Some(c)) => Ok(c),
        (Err(e), None) => Err(e),
    }
}

/// Writes a sticker's bytes from its installed pack to `path`, its attachment file. The attachment
/// CDN drops files after some weeks (old stickers 404), while an installed pack keeps every
/// sticker; it also saves the download. Reads the whole pack row, but only once per sticker.
async fn from_installed_pack(store: &SqliteStore, sticker: Option<&Sticker>, path: &Path) -> bool {
    let Some((pack, id)) = sticker.and_then(|s| Some((s.pack_id.as_ref()?, s.sticker_id?))) else { return false };
    let Ok(Some(p)) = store.sticker_pack(pack).await else { return false };
    let Some(bytes) = p.manifest.stickers.into_iter().find(|s| s.id == id).and_then(|s| s.bytes) else { return false };
    save_sticker(path, &bytes)
}

/// Writes a sticker's bytes from the sticker CDN to `path`, its attachment file. That CDN keeps
/// a pack for good, unlike the attachment CDN. The pack isn't installed: installing syncs it to
/// the user's other devices.
async fn from_sticker_cdn(manager: &Mgr, sticker: Option<&Sticker>, path: &Path) -> bool {
    let Some((pack, key, id)) = sticker.and_then(|s| Some((s.pack_id.as_ref()?, s.pack_key.as_ref()?, s.sticker_id?))) else {
        return false;
    };
    let started = std::time::Instant::now();
    match manager.download_sticker(pack, key, id).await {
        Ok(bytes) => {
            info!(kb = bytes.len() / 1024, ms = started.elapsed().as_millis() as u64, "sticker from its pack");
            save_sticker(path, &bytes)
        }
        Err(e) => {
            warn!("download sticker from its pack: {e}");
            false
        }
    }
}

fn save_sticker(path: &Path, bytes: &[u8]) -> bool {
    match sigwin_vault::write(path, bytes) {
        Ok(()) => true,
        Err(e) => {
            warn!("save sticker: {e}");
            false
        }
    }
}

pub(super) fn attachment_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.enc"))
}

/// Plain copy of an attachment for an outside app, in its own folder under `open/` (emptied on
/// every start; files still open in another app then are left until the next one).
pub(super) fn decrypt_for_open(src: &Path, open_dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).map_err(|e| std::io::Error::other(e.to_string()))?;
    let dir = open_dir.join(hex::encode(r));
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join(name);
    sigwin_vault::decrypt_file(src, &dst)?;
    mark_from_internet(&dst);
    Ok(dst)
}

/// Marks a plain copy of something received as downloaded from the internet, as browsers do,
/// so Windows and other apps warn before running it or open it in protected mode. Written once
/// the file is complete: the mark is a stream of that file.
pub(super) fn mark_from_internet(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        if let Err(e) = std::fs::write(&stream, "[ZoneTransfer]\r\nZoneId=3\r\n") {
            // FAT and some network drives have no streams: the file is still there
            warn!("mark of the web: {e}");
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

/// 1: everything encrypted, thumbnails still named `.png`. 2: thumbnails named `.enc`.
pub(super) const CACHE_FORMAT: u32 = 2;

/// Converts older caches: attachments lived in `<key>/<original name>`, thumbnails were plain
/// PNGs, then encrypted but still named `.png`. Idempotent; Err leaves the rest for the next start.
pub(super) fn seal_legacy_files(attachments: &Path, thumbs: &Path) -> anyhow::Result<()> {
    let mut failed = 0;
    for e in std::fs::read_dir(attachments)?.flatten() {
        let p = e.path();
        if p.is_dir() {
            let dst = attachment_path(attachments, &e.file_name().to_string_lossy());
            let src = std::fs::read_dir(&p)?.flatten().map(|f| f.path()).find(|f| f.is_file());
            if let Some(src) = src.filter(|_| !dst.exists()) {
                if let Err(e) = sigwin_vault::encrypt_file(&src, &dst, true) {
                    // not the path: it ends in the attachment's own name
                    warn!("encrypt a cached attachment: {e}");
                    failed += 1;
                    continue;
                }
            }
            let _ = std::fs::remove_dir_all(&p);
        }
    }
    for e in std::fs::read_dir(thumbs)?.flatten() {
        let p = e.path();
        if !p.is_file() || p.extension().is_none_or(|x| x != "png") {
            continue;
        }
        let dst = p.with_extension("enc");
        let res = if sigwin_vault::is_sealed(&p) {
            std::fs::rename(&p, &dst)
        } else {
            sigwin_vault::encrypt_file(&p, &dst, false).and_then(|_| std::fs::remove_file(&p))
        };
        if let Err(e) = res {
            // only a cache: drop it rather than keep it plain
            warn!("encrypt {}: {e}", p.display());
            let _ = std::fs::remove_file(&p);
        }
    }
    anyhow::ensure!(failed == 0, "{failed} attachments left unencrypted");
    Ok(())
}

pub(super) fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// Deletes the plain copies of opened attachments under the data folder `dir`.
pub fn clear_open(dir: &Path) {
    clear_dir(&Paths::new(dir).open);
}

/// Removes the folder's contents, keeping the folder.
pub(super) fn clear_dir(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_several_files_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("sigwin-free-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_name(&dir, "a.pdf"), dir.join("a.pdf"));
        std::fs::write(dir.join("a.pdf"), b"x").unwrap();
        assert_eq!(free_name(&dir, "a.pdf"), dir.join("a (2).pdf"));
        std::fs::write(dir.join("a (2).pdf"), b"x").unwrap();
        assert_eq!(free_name(&dir, "a.pdf"), dir.join("a (3).pdf"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stopping_a_video_is_remembered_until_it_is_asked_for_again() {
        let d = AttachmentDownloads::default();
        assert!(!d.stop("v"));
        assert!(!d.was_stopped("v"));

        let (handle, _registration) = AbortHandle::new_pair();
        d.running.borrow_mut().insert("v".into(), handle.clone());
        assert!(d.is_running("v"));
        assert!(d.stop("v"));
        assert!(handle.is_aborted());
        assert!(!d.is_running("v") && d.was_stopped("v"));
        // a second click isn't another stop: it downloads again
        assert!(!d.stop("v"));
    }

    #[test]
    fn a_caller_waits_for_a_download_on_its_way() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        rt.block_on(tokio::task::LocalSet::new().run_until(async {
            let d = AttachmentDownloads::default();
            let dest = std::env::temp_dir().join(format!("sigwin-ensure-{}", std::process::id()));
            let _ = std::fs::remove_file(&dest);
            let emit: Emit = Arc::new(|_| {});
            let wait = |d: &AttachmentDownloads| {
                let (d, dest, emit) = (d.clone(), dest.clone(), emit.clone());
                tokio::task::spawn_local(async move { d.ensure("k", &AttachmentPointer::default(), &Show::Quiet, &emit, &dest).await.unwrap() })
            };

            // it finishes: the waiter finds the file, and nothing was downloaded for it
            let (handle, _registration) = AbortHandle::new_pair();
            d.running.borrow_mut().insert("k".into(), handle);
            let waiter = wait(&d);
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished());
            std::fs::write(&dest, b"x").unwrap();
            d.running.borrow_mut().remove("k");
            d.ended.notify_waiters();
            assert!(waiter.await.unwrap());

            // it is stopped: the waiter doesn't start it over
            std::fs::remove_file(&dest).unwrap();
            let (handle, _registration) = AbortHandle::new_pair();
            d.running.borrow_mut().insert("k".into(), handle);
            let waiter = wait(&d);
            tokio::task::yield_now().await;
            assert!(d.stop("k"));
            assert!(!waiter.await.unwrap());
        }));
    }

    #[test]
    fn a_save_stopped_from_its_line_stops_its_download_and_stays_stopped() {
        let d = AttachmentDownloads::default();
        let run = d.begin_save("row");
        d.saving("row", run, "f");
        let (handle, _registration) = AbortHandle::new_pair();
        d.running.borrow_mut().insert("f".into(), handle);
        assert!(!d.save_stopped("row", run));
        d.stop_save("row");
        // the download it waited for is stopped, and the save doesn't start it over
        assert!(!d.is_running("f") && d.was_stopped("f"));
        assert!(d.save_stopped("row", run));

        // a new save of the same row isn't the old one: the old one, winding down, can't end it
        let next = d.begin_save("row");
        assert!(d.save_stopped("row", run) && !d.save_stopped("row", next));
        d.end_save("row", run);
        assert!(!d.save_stopped("row", next));
        d.end_save("row", next);
        assert!(d.save_stopped("row", next));
    }

    #[test]
    fn progress_is_a_percentage_of_what_is_known() {
        assert_eq!(percent(0, Some(100)), 0);
        assert_eq!(percent(45, Some(100)), 45);
        assert_eq!(percent(100, Some(100)), 100);
        // more than was announced
        assert_eq!(percent(150, Some(100)), 100);
        assert_eq!(percent(50, None), 0);
        assert_eq!(percent(50, Some(0)), 0);
    }

    #[test]
    fn video_lengths_are_sealed_and_read_back() {
        sigwin_vault::init([3; 32]);
        let dir = std::env::temp_dir().join(format!("sigwin-length-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (path, orig) = (length_path(&dir, "k"), dir.join("not-here.enc"));
        // nothing saved, and no video to read it from: unknown, and not remembered as unknown
        assert_eq!(video_length(&path, &orig), 0);
        assert!(!path.exists());

        save_length(&path, 75);
        assert_eq!(video_length(&path, &orig), 75);
        assert!(sigwin_vault::is_sealed(&path));
        // a broken file is read as missing
        std::fs::write(&path, b"junk").unwrap();
        assert_eq!(video_length(&path, &orig), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_too_small_for_its_picture_is_made_again() {
        sigwin_vault::init([3; 32]);
        let dir = std::env::temp_dir().join(format!("sigwin-thumbsize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = |w, h| sigwin_thumb::png_rgba(&image::RgbaImage::from_pixel(w, h, image::Rgba([90, 90, 90, 255]))).unwrap();
        let (big, small) = (png(1200, 900), png(100, 100));
        let thumb = dir.join("t.enc");
        let long = |img: &image::RgbaImage| img.width().max(img.height());
        let source = |bytes: &[u8], target, own| thumb_source(|| Ok(bytes.to_vec()), 450, &thumb, target, own).unwrap();

        // made for a small cell: the floor
        assert_eq!(long(&source(&big, Target::Cover(150, 150), Some(1200))), 450);
        // a bigger box: made again, enough to fill it, and kept so
        assert_eq!(long(&source(&big, Target::Cover(800, 600), Some(1200))), 800);
        assert_eq!(long(&load_cached(&thumb).unwrap()), 800);
        // never past the original
        assert_eq!(long(&source(&big, Target::Cover(1600, 1200), Some(1200))), 1200);
        // without the sender's size, or when it's no bigger, the cache stays
        let _ = std::fs::remove_file(&thumb);
        source(&big, Target::Cover(150, 150), None);
        assert_eq!(long(&source(&big, Target::Cover(800, 600), None)), 450);
        // a small picture is held enlarged to the floor, and not made again
        let _ = std::fs::remove_file(&thumb);
        assert_eq!(long(&source(&small, Target::Cover(100, 100), Some(100))), 450);
        assert_eq!(long(&source(&small, Target::Fit(675), Some(100))), 450);
        // the original gone: the cache it had
        let _ = std::fs::remove_file(&thumb);
        source(&big, Target::Cover(150, 150), Some(1200));
        let gone = thumb_source(|| anyhow::bail!("gone"), 450, &thumb, Target::Cover(800, 600), Some(1200)).unwrap();
        assert_eq!(long(&gone), 450);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seals_legacy_cache_files() {
        sigwin_vault::init([3; 32]);
        let dir = std::env::temp_dir().join(format!("sigwin-seal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (att, thumbs) = (dir.join("attachments"), dir.join("thumbs"));
        std::fs::create_dir_all(att.join("abc")).unwrap();
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(att.join("abc").join("계약서.pdf"), b"pdf bytes").unwrap();
        std::fs::write(thumbs.join("abc.png"), b"png bytes").unwrap();
        sigwin_vault::write(&thumbs.join("new.png"), b"already sealed").unwrap();

        seal_legacy_files(&att, &thumbs).unwrap();
        assert!(!att.join("abc").exists());
        assert_eq!(sigwin_vault::read(&attachment_path(&att, "abc")).unwrap(), b"pdf bytes");
        assert_eq!(sigwin_vault::read(&thumbs.join("abc.enc")).unwrap(), b"png bytes");
        assert_eq!(sigwin_vault::read(&thumbs.join("new.enc")).unwrap(), b"already sealed");
        assert!(!thumbs.join("abc.png").exists() && !thumbs.join("new.png").exists());

        let open = dir.join("open");
        let plain = decrypt_for_open(&attachment_path(&att, "abc"), &open, "계약서.pdf").unwrap();
        assert_eq!(plain.file_name().unwrap(), "계약서.pdf");
        assert_eq!(std::fs::read(&plain).unwrap(), b"pdf bytes");
        #[cfg(windows)]
        {
            let mut zone = plain.as_os_str().to_owned();
            zone.push(":Zone.Identifier");
            assert_eq!(std::fs::read_to_string(zone).unwrap(), "[ZoneTransfer]\r\nZoneId=3\r\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
