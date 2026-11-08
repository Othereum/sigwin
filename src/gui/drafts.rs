//! Message drafts: what was left in the composer when another thread was opened, shown in the
//! thread list as "작성 중: …" like Signal Desktop. Owned by the UI thread, saved to `drafts.enc`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Draft {
    /// composer text, style markers included
    pub(crate) text: String,
    pub(crate) mentions: Vec<mention::Member>,
    /// replying to: (message id, preview line)
    pub(crate) quote: Option<(String, String)>,
    pub(crate) files: Vec<PathBuf>,
    /// when the draft last changed (ms); it replaces the snippet unless a newer message came in
    pub(crate) ts: u64,
}

impl Draft {
    fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.quote.is_none() && self.files.is_empty()
    }

    fn same_content(&self, o: &Draft) -> bool {
        self.text == o.text && self.mentions == o.mentions && self.quote == o.quote && self.files == o.files
    }

    /// The thread list line after "작성 중:" (Signal's `getDraftPreview`).
    fn preview(&self) -> String {
        if !self.text.trim().is_empty() {
            let (plain, styles) = format::parse(&self.text);
            return view::text::mask_spoilers(plain, &styles).trim().replace(['\r', '\n'], " ");
        }
        if !self.files.is_empty() {
            "(첨부 파일)".into()
        } else if self.quote.is_some() {
            "(인용)".into()
        } else {
            "(임시 저장)".into()
        }
    }
}

pub(crate) fn load(path: &Path) -> HashMap<String, Draft> {
    crate::sealed::read(path, &path.with_extension("json")).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(ui: &Ui) {
    let path = &ui.drafts_path;
    if path.as_os_str().is_empty() {
        return;
    }
    let res = serde_json::to_vec(&ui.drafts).map_err(anyhow::Error::from).and_then(|b| Ok(crate::sealed::write(path, &b)?));
    if let Err(e) = res {
        tracing::warn!("saving drafts: {e:#}");
    }
}

/// What the composer holds right now, for thread `ui.open_key`. None while editing a message.
fn capture(app: &AppWindow, ui: &Ui) -> Option<Draft> {
    if ui.editing.is_some() {
        return None;
    }
    let preview = app.get_reply_preview().to_string();
    Some(Draft {
        text: app.get_draft().to_string(),
        mentions: ui.picked.clone(),
        quote: ui.reply_to.clone().map(|id| (id, preview)),
        files: ui.files.clone(),
        ts: 0,
    })
}

/// Files the draft for `key`; the time moves only when the content changed. Returns whether the
/// thread list needs redrawing.
fn put(ui: &mut Ui, key: &str, mut d: Draft) -> bool {
    if key.is_empty() {
        return false;
    }
    if d.is_empty() {
        return ui.drafts.remove(key).is_some();
    }
    match ui.drafts.get(key) {
        Some(old) if old.same_content(&d) => false,
        _ => {
            d.ts = view::time::now_ms();
            ui.drafts.insert(key.to_string(), d);
            true
        }
    }
}

/// Leaving the open thread for `next`: keeps its composer as a draft and returns `next`'s draft
/// (or an empty one) for [`restore`], with the UI state for it already in place.
pub(crate) fn switch(app: &AppWindow, ui: &mut Ui, next: &str) -> Draft {
    let changed = match capture(app, ui) {
        Some(d) => put(ui, &ui.open_key.clone(), d),
        None => false,
    };
    if changed {
        save(ui);
        show_threads(ui);
    }
    let mut d = ui.drafts.get(next).cloned().unwrap_or_default();
    d.files.retain(|p| p.exists());
    ui.editing = None;
    linkcard::reset(ui);
    ui.picked = d.mentions.clone();
    ui.files = d.files.clone();
    ui.reply_to = d.quote.as_ref().map(|q| q.0.clone());
    d
}

/// Puts a draft from [`switch`] into the composer. Outside `UI` borrows: moving the cursor runs
/// the composer's callbacks.
pub(crate) fn restore(app: &AppWindow, d: Draft) {
    app.set_draft(d.text.into());
    linkcard::clear(app);
    app.set_reply_preview(d.quote.map(|q| q.1).unwrap_or_default().into());
    app.set_edit_preview(SharedString::new());
    attach::show(app, &d.files);
    app.invoke_composer_to_end();
}

/// The composer of the open thread may have become empty (sent, cleared): drop its draft.
pub(crate) fn drop_if_empty(app: &AppWindow) {
    UI.with_borrow_mut(|ui| {
        let empty = capture(app, ui).is_some_and(|d| d.is_empty());
        if !empty || !ui.drafts.contains_key(&ui.open_key) {
            return;
        }
        ui.drafts.remove(&ui.open_key.clone());
        save(ui);
        show_threads(ui);
    });
}

/// On exit: the open thread's composer becomes its draft too.
pub(crate) fn save_on_exit(app: &AppWindow) {
    UI.with_borrow_mut(|ui| {
        if let Some(d) = capture(app, ui) {
            if put(ui, &ui.open_key.clone(), d) {
                save(ui);
            }
        }
    });
}

/// The backend's thread list with drafts in place of snippets, sorted by the draft time where
/// the draft is newer than the last message (Signal's `shouldShowDraft`).
pub(crate) fn show_threads(ui: &Ui) {
    let mut list: Vec<(u64, ThreadItem)> = ui
        .thread_dtos
        .iter()
        .map(|t| {
            let mut item = thread_item(t, &ui.avatars);
            let mut ts = t.ts;
            if let Some(d) = ui.drafts.get(&t.key).filter(|d| d.ts >= t.ts) {
                ts = d.ts;
                item.draft = true;
                item.snippet = d.preview().into();
                item.time = view::time::time_short(d.ts).into();
            }
            (ts, item)
        })
        .collect();
    // stable: threads without a draft keep the backend's order
    if list.iter().any(|(_, i)| i.draft) {
        list.sort_by(|a, b| b.0.cmp(&a.0));
    }
    ui.threads.set_vec(list.into_iter().map(|(_, i)| i).collect::<Vec<_>>());
}

/// Ages the times of draft rows ("3분" -> "4분").
pub(crate) fn refresh_draft_times(ui: &Ui) {
    use slint::Model;
    for i in 0..ui.threads.row_count() {
        let Some(mut row) = ui.threads.row_data(i) else { continue };
        let Some(d) = ui.drafts.get(row.key.as_str()).filter(|_| row.draft) else { continue };
        let t = view::time::time_short(d.ts);
        if row.time.as_str() != t {
            row.time = t.into();
            ui.threads.set_row_data(i, row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_strips_markers_and_lines() {
        let d = Draft { text: "**안녕**\n  하세요".into(), ..Default::default() };
        assert_eq!(d.preview(), "안녕   하세요");
        let d = Draft { files: vec!["a.png".into()], ..Default::default() };
        assert_eq!(d.preview(), "(첨부 파일)");
        let d = Draft { quote: Some(("1".into(), "x".into())), ..Default::default() };
        assert_eq!(d.preview(), "(인용)");
    }

    #[test]
    fn put_keeps_time_when_unchanged() {
        let mut ui = Ui::default();
        let d = Draft { text: "a".into(), ..Default::default() };
        assert!(put(&mut ui, "k", d.clone()));
        let ts = ui.drafts["k"].ts;
        assert!(!put(&mut ui, "k", d));
        assert_eq!(ui.drafts["k"].ts, ts);
        assert!(put(&mut ui, "k", Draft::default()));
        assert!(ui.drafts.is_empty());
    }
}
