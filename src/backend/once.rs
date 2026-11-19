//! View-once pictures: used up once shown, here or on another of the user's devices.

use presage::proto::sync_message::ViewOnceOpen;

use super::files::remove_files;
use super::receive::Outgoing;
use super::*;

impl App {
    /// The viewer showed row `id`'s view-once picture: it goes from this device and the phone,
    /// and the sender hears it was viewed (unless read receipts are off, or it is a request).
    pub(super) async fn view_once_seen(&mut self, id: &str) {
        let Some(open) = self.current.as_ref() else { return };
        let Ok(ts) = id.parse::<u64>() else { return };
        let Some((_, c)) = open.window.iter().find(|(t, _)| *t == ts) else { return };
        let Kind::Msg { dm, outgoing: false } = view::classify(c, self.me) else { return };
        if !dm.is_view_once.unwrap_or(false) {
            return;
        }
        let (thread, key, c) = (open.thread.clone(), open.key.clone(), c.clone());
        let (sender, sent) = (c.metadata.sender, c.timestamp());
        info!("view-once picture shown");
        self.erase_view_once(&thread, c).await;
        let mut out = Vec::new();
        if self.persist.read_receipts != Some(false) && !self.is_request(&key) {
            out.push(Outgoing::Viewed { to: sender, timestamps: vec![sent] });
        }
        out.push(Outgoing::ViewOnceOpen { sender, timestamp: sent });
        self.send_now(out);
    }

    /// Another of our devices opened a view-once message.
    pub(super) async fn view_once_opened(&mut self, v: &ViewOnceOpen) {
        let (Some(aci), Some(ts)) = (v.parse_sender_aci(), v.timestamp) else { return };
        let sender = ServiceId::Aci(aci);
        let Ok(Some(thread)) = self.store.thread_for_sender_and_timestamp(&sender, ts).await else { return };
        let Ok(Some(c)) = self.store.message(&thread, ts).await else { return };
        if view::data_message(&c).is_some_and(|dm| dm.is_view_once.unwrap_or(false)) {
            info!("view-once opened on another device");
            self.erase_view_once(&thread, c).await;
        }
    }

    /// Drops what the message held of the picture, its pointer (with the key) and any cached
    /// copy, keeping the row: it then reads as viewed, as a backup's viewed ones do.
    pub(super) async fn erase_view_once(&mut self, thread: &Thread, mut c: Content) {
        let ts = c.metadata.client_timestamp.timestamp_millis() as u64;
        remove_files(&c, ts, &self.paths.attachments, &self.paths.thumbs);
        let dm = match &mut c.body {
            ContentBody::DataMessage(dm) => dm,
            ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::Sent(s)), .. }) => match s.message.as_mut() {
                Some(dm) => dm,
                None => return,
            },
            _ => return,
        };
        if dm.attachments.is_empty() {
            return;
        }
        dm.attachments.clear();
        if let Err(e) = self.store.save_message(thread, c).await {
            warn!("erase view-once: {e:#}");
            return;
        }
        if self.current.as_ref().is_some_and(|o| o.thread == *thread) {
            if let Err(e) = self.reload_window().await {
                warn!("reload after view-once: {e:#}");
            }
            self.emit_messages(Scroll::Keep).await;
        }
    }
}
