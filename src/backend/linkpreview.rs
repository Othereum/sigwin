//! Link previews on outgoing messages: fetching the site, attaching the result.
//!
//! presage fetches a preview by itself for any outgoing message with a link and none set, with
//! no way to turn that off, blocking the send and logging the url when it fails. So every message
//! sent here says what it wants: a preview we made, or `no_preview`. Logs never name the link.

use std::io::Cursor;

use presage::model::previews::{self, PreviewContent};
use presage::proto::Preview;

use super::*;
use crate::link;
use crate::types::{LinkCard, Thumb};

/// A site that hasn't answered by then gets no preview. A send waits for this at most.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Size of the image in the composer's bar (logical px).
pub(crate) const COMPOSER_IMAGE: u32 = 44;
/// Suffix of the key of that image, after the card's.
pub(crate) const COMPOSER_KEY: &str = "-c";

pub(super) struct Fetched {
    content: PreviewContent,
    url: String,
    dims: Option<(u32, u32)>,
    card: LinkCard,
}

/// The composer's link: being fetched (`result` None), or done.
pub(super) struct LinkDraft {
    url: String,
    task: JoinHandle<()>,
    result: Option<Option<Arc<Fetched>>>,
}

/// What a message's job does about its link.
#[derive(Clone)]
pub(super) enum LinkSend {
    None,
    Ready(Arc<Fetched>),
    /// sent before the composer's fetch finished
    Fetch(String),
}

impl LinkSend {
    pub(super) fn card(&self) -> Option<LinkCard> {
        match self {
            Self::Ready(f) => Some(f.card.clone()),
            _ => None,
        }
    }
}

impl App {
    /// The composer's link changed.
    pub(super) fn link_preview(&mut self, url: Option<String>) {
        if let Some(old) = self.link_draft.take() {
            old.task.abort();
        }
        let Some(url) = url else { return };
        self.link_seq += 1;
        let key = format!("link-draft-{}", self.link_seq);
        let tx = self.tx.clone();
        let u = url.clone();
        let task = tokio::task::spawn_local(async move {
            let res = fetch(u.clone(), key).await;
            let _ = tx.send(Internal::LinkFetched(u, res.map(Box::new)));
        });
        self.link_draft = Some(LinkDraft { url, task, result: None });
    }

    pub(super) fn link_fetched(&mut self, url: String, res: Option<Box<(Fetched, Vec<Thumb>)>>) {
        let Some(draft) = self.link_draft.as_mut().filter(|d| d.url == url) else { return };
        let (fetched, thumbs) = match res {
            Some(b) => {
                let (f, t) = *b;
                (Some(Arc::new(f)), t)
            }
            None => (None, Vec::new()),
        };
        for t in thumbs {
            (self.emit)(Event::Thumb(Arc::new(t)));
        }
        let card = fetched.as_ref().map(|f| f.card.clone());
        draft.result = Some(fetched);
        (self.emit)(Event::LinkPreview { url, card });
    }

    /// What the message being sent does about `url`; the composer's fetch ends here either way.
    pub(super) fn link_for_send(&mut self, url: Option<String>) -> LinkSend {
        let draft = self.link_draft.take();
        if let Some(d) = &draft {
            d.task.abort();
        }
        let Some(url) = url else { return LinkSend::None };
        match draft.filter(|d| d.url == url).and_then(|d| d.result) {
            Some(Some(fetched)) => LinkSend::Ready(fetched),
            // the site gave nothing a moment ago
            Some(None) => LinkSend::None,
            None => LinkSend::Fetch(url),
        }
    }
}

/// Contacts the link's site. A client of its own, dropped after: nothing stays in memory.
async fn fetch(url: String, key: String) -> Option<(Fetched, Vec<Thumb>)> {
    let started = Instant::now();
    let client = previews::client_builder().build().ok()?;
    let res = tokio::time::timeout(FETCH_TIMEOUT, previews::generate_preview_from_url(url.as_str(), &client)).await;
    debug!(ms = started.elapsed().as_millis() as u64, ok = matches!(res, Ok(Ok(_))), "link preview fetched");
    let mut content = res.ok()?.ok()?;
    let title = content.title.as_deref().map(|t| link::clean_preview_text(t, view::media::LINK_TITLE_MAX)).filter(|t| !t.is_empty())?;
    let description = content.description.as_deref().map(|d| link::clean_preview_text(d, view::media::LINK_DESC_MAX)).unwrap_or_default();
    content.title = Some(title.clone());
    content.description = (!description.is_empty()).then(|| description.clone());

    let image = content.image.clone();
    let k = key.clone();
    let decoded = match image {
        Some(bytes) => tokio::task::spawn_blocking(move || card_images(&bytes, &k)).await.ok().flatten(),
        None => None,
    };
    let (dims, size, thumbs) = match decoded {
        Some((dims, size, thumbs)) => (Some(dims), size, thumbs),
        None => {
            // what we can't show, we don't send
            content.image = None;
            content.image_content_type = None;
            (None, (0, 0), Vec::new())
        }
    };
    let card = LinkCard {
        domain: link::domain(&url),
        url: url.clone(),
        title,
        description,
        image_key: if dims.is_some() { key } else { String::new() },
        img_w: size.0,
        img_h: size.1,
    };
    Some((Fetched { content, url, dims, card }, thumbs))
}

/// The image's size, its size on the card, and thumbnails for the card and the composer.
fn card_images(bytes: &[u8], key: &str) -> Option<((u32, u32), (u32, u32), Vec<Thumb>)> {
    let dims = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()?;
    let img = sigwin_thumb::decode_thumb(bytes, view::media::PHOTO_THUMB_MAX).ok()?;
    let rgba = sigwin_thumb::to_rgba(&img);
    let size = view::media::link_image_size(Some(dims.0), Some(dims.1));
    let small = crate::types::physical_px(COMPOSER_IMAGE);
    let thumb = |key: String, w, h| {
        let cell = sigwin_thumb::cover(&rgba, w, h);
        Thumb { key, width: cell.width(), height: cell.height(), rgba: cell.into_raw() }
    };
    let thumbs = vec![
        thumb(key.to_string(), crate::types::physical_px(size.0), crate::types::physical_px(size.1)),
        thumb(format!("{key}{COMPOSER_KEY}"), small, small),
    ];
    Some((dims, size, thumbs))
}

/// Stops presage's own fetch for a message without a preview: it skips messages that have one.
/// Receivers discard this one and keep the message. The url must be there, even empty: Signal
/// iOS drops a whole message whose preview has no url field.
fn no_preview() -> Preview {
    Preview { url: Some(String::new()), ..Default::default() }
}

/// The `DataMessage::preview` of a message with this `body`.
pub(super) async fn for_message(m: &Mgr, body: &str, has_files: bool, link: LinkSend, ts: u64, attachments_dir: &Path) -> Vec<Preview> {
    let urls = previews::extract_urls_from_text_block(body);
    if urls.is_empty() {
        // nothing for presage to fetch, nothing to preview
        return Vec::new();
    }
    let fetched = match link {
        // receivers don't show a preview next to attachments
        _ if has_files => None,
        LinkSend::None => None,
        LinkSend::Ready(f) => Some(f),
        LinkSend::Fetch(url) => fetch(url, String::new()).await.map(|(f, _)| Arc::new(f)),
    };
    // a long message's body keeps only its start, and receivers look for the url there
    let Some(f) = fetched.filter(|f| urls.contains(&f.url)) else { return vec![no_preview()] };
    let image = upload_image(m, &f, ts, attachments_dir).await;
    let c = &f.content;
    vec![Preview { url: Some(f.url.clone()), title: c.title.clone(), image, description: c.description.clone(), date: c.date }]
}

/// The preview is still useful without its image. Cached right away: our own row needs no download.
async fn upload_image(m: &Mgr, f: &Fetched, ts: u64, attachments_dir: &Path) -> Option<AttachmentPointer> {
    let (mut spec, data) = previews::build_preview_image(&f.content)?;
    (spec.width, spec.height) = f.dims.map_or((None, None), |(w, h)| (Some(w), Some(h)));
    let ptr = match m.upload_attachment(spec, data.clone()).await {
        Ok(Ok(ptr)) => ptr,
        _ => {
            warn!("link preview image upload failed");
            return None;
        }
    };
    let key = view::media::attachment_key(&ptr, ts, view::media::LINK_IMAGE_INDEX);
    if let Err(e) = sigwin_vault::write(&attachment_path(attachments_dir, &key), &data) {
        warn!("cache link preview image: {e}");
    }
    Some(ptr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_preview_has_a_url_field_and_shows_nothing() {
        let p = no_preview();
        assert_eq!(p.url.as_deref(), Some(""));
        let dm = DataMessage { body: Some("https://www.youtube.com/watch?v=x".into()), preview: vec![p], ..Default::default() };
        assert!(view::media::link_card(&dm, 1).is_none());
    }
}
