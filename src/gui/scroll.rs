//! Message list scrolling: stick to bottom, jump to a row, keep place on older pages.

use super::*;

/// Scroll the message list to the bottom and keep it there until its height settles.
/// The ListView estimates unseen rows from the average row height; rows near the bottom
/// only get their real (often taller: quotes, images) height once instantiated, which
/// grows `content-height` after the first scroll.
pub(crate) fn stick_to_bottom(app: &AppWindow) {
    // a jump to a pinned message owns the scroll position until it lands
    if !app.get_jump_id().is_empty() {
        return;
    }
    let weak = app.as_weak();
    let mut last_h = -1.0f32;
    let mut stable = 0u32;
    let mut ticks = 0u32;
    TIMER.with(|t| {
        t.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(16), move || {
            let Some(app) = weak.upgrade() else { return };
            app.invoke_scroll_to_bottom();
            let h = app.invoke_content_height();
            ticks += 1;
            stable = if h == last_h { stable + 1 } else { 0 };
            last_h = h;
            if stable >= 3 || ticks >= 60 {
                TIMER.with(|t| t.stop());
            }
        });
    });
}

/// Scrolls the message list until row `id` is centered in the view, then flashes it.
///
/// Only rows on screen exist, and after every layout the ListView rescales all positions by the
/// average height of those rows, so positions from different frames don't match. So this
/// alternates: one frame the target row reports its on-screen position (y and content-y read
/// together), the next frame acts on it.
/// - Row on screen: shift by the exact distance (a short scroll lays out real row heights).
/// - Row not instantiated: content-y = -(idx + 0.5) * average height. That is the ListView's own
///   "random seek" formula (offset = floor(-content-y / average)) inverted, so for a far target
///   the row lands at the top of the view; a near one just scrolls toward it.
pub(crate) fn jump_to_row(app: &AppWindow, id: String) {
    seek_row(app, id, None);
}

/// Moves row `id` so its bottom edge is `want` px from the top of the list (None: centered,
/// then flashed).
pub(crate) fn seek_row(app: &AppWindow, id: String, want: Option<f32>) {
    let Some(idx) = UI.with_borrow(|ui| ui.last.iter().position(|m| m.id == id)) else { return };
    TIMER.with(|t| t.stop());
    app.set_jump_id(id.as_str().into());
    app.set_jump_found(false);
    app.set_jump_probe(app.get_jump_probe() + 1);
    let weak = app.as_weak();
    let mut ticks = 0u32;
    let mut last_pos: Option<f32> = None;
    JUMP.with(|t| {
        t.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(20), move || {
            let Some(app) = weak.upgrade() else { return };
            ticks += 1;
            if ticks % 2 == 0 {
                // measure (the layout has caught up with the last move)
                app.set_jump_found(false);
                app.set_jump_probe(app.get_jump_probe() + 1);
                return;
            }
            let vh = app.invoke_list_visible_height();
            let mut done = ticks >= 60;
            if app.get_jump_found() {
                let pos = app.get_jump_pos();
                // centered; a row taller than the view starts at the top
                // a `want` is for the row's bottom edge, which doesn't move when its day
                // separator comes or goes
                let (want, near) = match want {
                    Some(w) => (w - app.get_jump_h(), 0.5),
                    None => (((vh - app.get_jump_h()) / 2.0).max(0.0), 4.0),
                };
                // close enough, or the list can't scroll further (row near either end)
                if (pos - want).abs() <= near || last_pos.is_some_and(|p: f32| (p - pos).abs() < near.min(1.0)) {
                    done = true;
                } else {
                    app.invoke_shift_by(pos - want);
                }
                last_pos = Some(pos);
            } else {
                last_pos = None;
                // rows above may have been added since (older history, a reload)
                let jid = app.get_jump_id();
                let idx = UI.with_borrow(|ui| ui.last.iter().position(|m| m.id == jid.as_str())).unwrap_or(idx);
                let n = slint::Model::row_count(&app.get_messages()).max(1) as f32;
                let avg = app.invoke_content_height() / n;
                app.invoke_set_content_y(-(idx as f32 + 0.5) * avg);
            }
            if done {
                JUMP.with(|t| t.stop());
                let id = app.get_jump_id();
                app.set_jump_id(SharedString::new());
                if want.is_some() {
                    tracing::debug!("older page anchored after {ticks} ticks");
                    // still at the top (short page): go on loading
                    app.invoke_check_older();
                    return;
                }
                app.set_flash_id(id.clone());
                let weak = app.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
                    if let Some(app) = weak.upgrade().filter(|a| a.get_flash_id() == id) {
                        app.set_flash_id(SharedString::new());
                    }
                });
            }
        });
    });
}

/// Puts a message list on screen. `rebuild` replaces the whole model (thread switch, anchored
/// older page); otherwise only changed rows are touched, and rows inserted above the ones on
/// screen keep the view where it is.
pub(crate) fn show_rows(app: &AppWindow, items: Vec<MsgDto>, paging: Paging, scroll: Scroll, rebuild: bool) {
    let near_bottom = app.get_near_bottom();
    UI.with_borrow_mut(|ui| {
        ui.loading_older = false;
        ui.loading_newer = false;
        ui.detached = paging.newer;
        if rebuild {
            app.set_new_below(0);
        } else if !near_bottom {
            // count incoming rows that weren't shown before
            let seen: std::collections::HashSet<&str> = ui.last.iter().map(|m| m.id.as_str()).collect();
            let fresh = items.iter().filter(|m| !m.outgoing && !m.system && !seen.contains(m.id.as_str())).count();
            if matches!(scroll, Scroll::KeepIfNotAtBottom) && fresh > 0 {
                app.set_new_below(app.get_new_below() + fresh as i32);
            }
        }
        let dark = app.global::<Theme>().get_dark();
        if rebuild {
            ui.messages.set_vec(items.iter().map(|m| to_item(m, &ui.thumbs, &ui.media_labels, &ui.anims, &ui.sender_avatars, &ui.revealed, dark)).collect::<Vec<_>>());
            ui.typing_row = false;
        } else {
            update_rows(ui, &items, dark);
        }
        sync_typing(ui);
        forget_unshown_thumbs(ui, &items);
        ui.last = items;
    });
    app.set_can_load_older(paging.older);
    app.set_can_load_newer(paging.newer);
    if rebuild && scroll == Scroll::Bottom {
        // otherwise the first frames lay out from the old thread's position
        app.invoke_seek_end();
    }
    match scroll {
        Scroll::Bottom => stick_to_bottom(app),
        Scroll::KeepIfNotAtBottom if near_bottom && !paging.newer => stick_to_bottom(app),
        Scroll::KeepIfNotAtBottom | Scroll::Keep | Scroll::Older => {}
    }
    if rebuild && scroll != Scroll::Older {
        app.invoke_focus_composer();
    }
}

/// Thumbnails of rows that left the list: the window moves through the whole thread, so keeping
/// them would grow with how far the user scrolled. The backend sends them again if they return.
fn forget_unshown_thumbs(ui: &mut Ui, items: &[MsgDto]) {
    use crate::backend::linkpreview::COMPOSER_KEY;
    let shown: std::collections::HashSet<&str> = items
        .iter()
        .flat_map(|m| {
            let link = m.link.as_ref().map(|l| l.image_key.as_str());
            std::iter::once(m.image_key.as_str()).chain(link).chain(m.cells.iter().map(|c| c.key.as_str()))
        })
        .collect();
    let card = ui.link.key.clone();
    ui.thumbs.retain(|k, _| shown.contains(k.as_str()) || k.ends_with(COMPOSER_KEY) || *k == card);
    ui.media_labels.retain(|k, _| shown.contains(k.as_str()));
}

/// An older page arrived: keep the row that was on top (`anchor`, the oldest row so far) at
/// the same place on screen.
///
/// If the anchor isn't instantiated, the view is further down and inserting rows above it
/// keeps the position by itself. If it is, the ListView is at its first row and would show
/// the inserted rows instead, and its content height is only (average row height x rows), so
/// shifting by the height difference lands rows off. So: measure the anchor (one frame),
/// replace the model, seek straight to the anchor's index with the ListView's own estimate
/// (it then lays out from that row), and settle it at the measured spot like a pin jump.
pub(crate) fn hold_older(app: &AppWindow, anchor: String, items: Vec<MsgDto>, paging: Paging) {
    UI.with_borrow_mut(|ui| ui.older = Some((items, paging)));
    TIMER.with(|t| t.stop());
    app.set_jump_id(anchor.as_str().into());
    app.set_jump_found(false);
    app.set_jump_probe(app.get_jump_probe() + 1);
    let weak = app.as_weak();
    JUMP.with(|t| {
        t.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(20), move || {
            let Some(app) = weak.upgrade() else { return };
            let ours = app.get_jump_id() == anchor.as_str();
            let Some((items, paging)) = UI.with_borrow_mut(|ui| ui.older.take()) else {
                if ours {
                    app.set_jump_id(SharedString::new());
                }
                return;
            };
            let found = ours && app.get_jump_found();
            if ours {
                app.set_jump_id(SharedString::new());
            }
            let idx = items.iter().position(|m| m.id == anchor);
            let (Some(idx), true) = (idx, found) else {
                tracing::debug!("older page: anchor not on screen, inserting above");
                show_rows(&app, items, paging, Scroll::Older, false);
                // a short page may leave the top still near (content-y is current after a layout)
                let weak = app.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
                    if let Some(app) = weak.upgrade() {
                        app.invoke_check_older();
                    }
                });
                return;
            };
            let want = app.get_jump_pos() + app.get_jump_h();
            let n = slint::Model::row_count(&app.get_messages()).max(1) as f32;
            let avg = app.invoke_content_height() / n;
            tracing::debug!("older page: {idx} rows above the anchor, its bottom at {want:.1}px");
            show_rows(&app, items, paging, Scroll::Older, true);
            app.invoke_set_content_y_raw(-(idx as f32 + 0.5) * avg);
            seek_row(&app, anchor.clone(), Some(want));
        });
    });
}
