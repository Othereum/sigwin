//! Writing to someone by PNI: a number we never talked with, whose ACI the server won't give us
//! (contact discovery returns only the PNI until they've shared their profile). Their thread is
//! `p:` (`types::thread_key`), filed in `ls_pni` with the number we looked them up by. Their
//! reply comes from their ACI with a verified PNI signature (`Metadata::pni_verified`); then the
//! `p:` thread moves into their `c:` thread, which presage already filed the reply under.

use presage::libsignal_service::prelude::phonenumber;

use super::*;

/// The `p:` thread of `pni`.
pub(super) fn pni_thread(pni: Uuid) -> Thread {
    Thread::Contact(ServiceId::Pni(pni.into()))
}

/// Messages copied per read while moving a thread, so the read stays bounded.
const MOVE_PAGE: u32 = 200;

impl App {
    /// What a PNI thread is called: the number we found them by.
    pub(super) async fn pni_name(&mut self, pni: Uuid) -> String {
        if let Some(n) = self.names.get(&pni) {
            return n.clone();
        }
        let number = self.index.pni_number(pni).await.ok().flatten().flatten();
        let name = number
            .and_then(|e| phonenumber::parse(None, &e).ok())
            .map(|p| super::threads::show_number(&p))
            .unwrap_or_else(|| "알 수 없는 번호".into());
        self.names.insert(pni, name.clone());
        name
    }

    /// A message stored under a `p:` thread we haven't filed: one the phone sent to a number it
    /// never talked with (a sync transcript).
    pub(super) async fn note_pni_thread(&mut self, pni: Uuid, e164: Option<&str>) {
        match self.index.pni_number(pni).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(e) = self.index.pni_put(pni, e164).await {
                    warn!("file a PNI thread: {e:#}");
                }
                self.names.remove(&pni);
                self.threads_dirty = true;
            }
            Err(e) => warn!("PNI thread: {e:#}"),
        }
    }

    /// `aci` replied with a verified signature of `pni`: if we wrote to that PNI, its thread
    /// joins theirs.
    pub(super) async fn pni_proved(&mut self, pni: Uuid, aci: Uuid) {
        match self.index.pni_number(pni).await {
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(e) => return warn!("PNI thread: {e:#}"),
        }
        info!("PNI thread proved, moving it");
        if let Err(e) = self.index.pni_proved(pni, aci).await {
            return warn!("record a proved PNI: {e:#}");
        }
        if let Err(e) = self.merge_pni(pni, aci).await {
            // `ls_pni.aci` stays set: the next start finishes it (`resume_pni_merges`)
            warn!("move a PNI thread: {e:#}");
        }
    }

    /// Finishes moves a previous run didn't get through.
    pub(super) async fn resume_pni_merges(&mut self) {
        let todo = match self.index.pni_merging().await {
            Ok(t) => t,
            Err(e) => return warn!("PNI moves: {e:#}"),
        };
        for (pni, aci) in todo {
            if let Err(e) = self.merge_pni(pni, aci).await {
                warn!("move a PNI thread: {e:#}");
            }
        }
    }

    /// Moves the `p:` thread of `pni` into `aci`'s `c:` thread. Safe to run again after an
    /// interruption: a copy already made is stored over itself, and `ls_pni` is forgotten last.
    async fn merge_pni(&mut self, pni: Uuid, aci: Uuid) -> anyhow::Result<()> {
        let number = self.index.pni_number(pni).await?.flatten();
        let (from, to) = (pni_thread(pni), Thread::Contact(ServiceId::Aci(aci.into())));
        let (fk, tk) = (thread_key(&from), thread_key(&to));
        self.index.move_order(&from, &to).await?;
        let mut after = 0;
        let mut moved = 0usize;
        loop {
            let tss = self.index.page_after(&from, after, MOVE_PAGE).await?;
            let (Some(&first), Some(&last)) = (tss.first(), tss.last()) else { break };
            after = last;
            for c in self.store.messages(&from, first..=last).await?.flatten() {
                self.store.save_message(&to, c).await?;
                moved += 1;
            }
        }
        self.store.clear_thread(&from).await?;
        self.index.rekey_thread(&fk, &tk).await?;
        self.rekey_persist(&fk, &tk);
        // until their profile names them, the number does, as for anyone found by number
        if let Some(p) = number.and_then(|e| phonenumber::parse(None, &e).ok()) {
            if let Err(e) = self.file_found(aci, Some(p), None).await {
                warn!("file a proved PNI's number: {e:#}");
            }
        }
        self.index.pni_forget(pni).await?;
        self.names.remove(&pni);
        self.names.remove(&aci);
        info!(moved, "PNI thread moved");
        (self.emit)(Event::ThreadMoved { from: fk.clone(), to: tk.clone() });
        if self.current.as_ref().is_some_and(|o| o.key == fk) {
            self.open_thread(&tk).await?;
        }
        self.threads_dirty = true;
        Ok(())
    }

    /// What we keep per thread key moves from `from` to `to`; `to`'s own settings win.
    fn rekey_persist(&mut self, from: &str, to: &str) {
        let p = &mut self.persist;
        if let Some(n) = p.unread.remove(from) {
            *p.unread.entry(to.into()).or_default() += n;
        }
        if let Some(t) = p.read_upto.remove(from) {
            let e = p.read_upto.entry(to.into()).or_default();
            *e = (*e).max(t);
        }
        if let Some(m) = p.mute_override.remove(from) {
            p.mute_override.entry(to.into()).or_insert(m);
        }
        if let Some(prefs) = p.thread_prefs.remove(from) {
            p.thread_prefs.entry(to.into()).or_insert(prefs);
        }
        // recomputed for `to`
        for k in [from, to] {
            p.snippets.remove(k);
            p.sweep.remove(k);
        }
        p.avatars.remove(from);
        p.mute_phone.remove(from);
        self.persist_dirty = true;
    }
}
