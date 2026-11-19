//! The open chat's media page (`ui/gallery.slint`): rows of cells under month headings, or a list
//! of files. Its rows have fixed heights, so what's on screen is worked out here from the scroll
//! position: only those items (and a few screens around them) hold a thumbnail.

use std::ops::Range;
use std::sync::Arc;

use sigwin_ui::{GalleryCell, GalleryData, GalleryRow};

use super::*;
use crate::types::GalleryItemDto;

struct Page {
    files: bool,
    items: Vec<GalleryItemDto>,
    more: bool,
    /// older items were asked for and aren't here yet
    asked: bool,
    cols: usize,
    rows: Rc<VecModel<GalleryRow>>,
    /// per row: its top and bottom (px from the list's top) and the items it shows
    spans: Vec<(f32, f32, Range<usize>)>,
    thumbs: HashMap<String, Image>,
    labels: rows::MediaStates,
    /// items whose thumbnails are wanted
    shown: Range<usize>,
    /// the top of what's on screen and its height
    view: (f32, f32),
}

thread_local! {
    static PAGE: RefCell<Option<Page>> = const { RefCell::new(None) };
}

/// Screens above and below the view whose thumbnails are kept: scrolling a little back finds
/// them still there.
const MARGIN_SCREENS: f32 = 2.0;
/// Rows of cells left below the view when older items are asked for.
const MORE_ROWS_LEFT: usize = 6;

pub(crate) fn wire(app: &AppWindow) {
    let d = app.global::<GalleryData>();
    d.set_cell(view::media::GALLERY_CELL as f32);
    let weak = app.as_weak();
    d.on_show(move || {
        if let Some(app) = weak.upgrade() {
            open(&app, false);
        }
    });
    let weak = app.as_weak();
    d.on_close(move || {
        if let Some(app) = weak.upgrade() {
            close(&app);
        }
    });
    let weak = app.as_weak();
    d.on_switch(move |tab| {
        if let Some(app) = weak.upgrade() {
            open(&app, tab == 1);
        }
    });
    let weak = app.as_weak();
    d.on_layout(move |cols| {
        let Some(app) = weak.upgrade() else { return };
        let changed = PAGE.with_borrow_mut(|p| {
            let Some(p) = p.as_mut().filter(|p| p.cols != cols.max(1) as usize) else { return false };
            p.cols = cols.max(1) as usize;
            true
        });
        if changed {
            rebuild(&app);
        }
    });
    d.on_scrolled(move |top, height| {
        PAGE.with_borrow_mut(|p| {
            if let Some(p) = p.as_mut() {
                p.view = (top, height);
            }
        });
        update_shown();
    });
    let weak = app.as_weak();
    d.on_open_cell(move |c| {
        let Some(app) = weak.upgrade() else { return };
        let (id, nth) = (c.id.to_string(), c.nth.max(0) as usize);
        if c.video {
            send_cmd(Command::OpenVideo { id, index: nth });
        } else {
            viewer::open(&app, id, nth);
        }
    });
    let weak = app.as_weak();
    d.on_show_in_chat(move |id| {
        let Some(app) = weak.upgrade() else { return };
        close(&app);
        send_cmd(Command::JumpTo(id.to_string()));
    });
    let weak = app.as_weak();
    d.on_save(move |id, index| {
        let Some(app) = weak.upgrade() else { return };
        send_cmd(Command::GallerySave { id: id.to_string(), index: index.max(0) as usize, owner: window::hwnd(&app) });
    });
}

/// Opens the page on the chat's pictures, or its `files`, in place of what it showed.
fn open(app: &AppWindow, files: bool) {
    // switching tabs keeps the scroll view, which won't report its size again
    let (cols, view) = PAGE.with_borrow(|p| p.as_ref().map_or((0, (0.0, 0.0)), |p| (p.cols, p.view)));
    let rows = Rc::new(VecModel::default());
    PAGE.with_borrow_mut(|p| {
        *p = Some(Page {
            files,
            items: Vec::new(),
            more: false,
            asked: false,
            cols,
            rows: rows.clone(),
            spans: Vec::new(),
            thumbs: HashMap::new(),
            labels: Default::default(),
            shown: 0..0,
            view,
        })
    });
    let d = app.global::<GalleryData>();
    d.set_rows(ModelRc::from(rows));
    d.set_loading(true);
    d.set_indexing(false);
    d.set_tab(files as i32);
    d.set_chat(app.get_current_name());
    d.set_open(true);
    send_cmd(Command::GalleryOpen { files });
}

/// Leaves the page and lets go of everything it held.
pub(crate) fn close(app: &AppWindow) {
    let d = app.global::<GalleryData>();
    d.set_open(false);
    d.set_rows(ModelRc::default());
    if PAGE.with_borrow_mut(Option::take).is_some() {
        send_cmd(Command::GalleryClose);
    }
}

/// Whether the page shows message `id`: the viewer may show its pictures.
pub(crate) fn shows(id: &str) -> bool {
    PAGE.with_borrow(|p| p.as_ref().is_some_and(|p| p.items.iter().any(|i| i.id == id)))
}

pub(crate) fn arrived(app: &AppWindow, files: bool, items: Vec<GalleryItemDto>, more: bool, reset: bool, indexing: bool) {
    let mine = PAGE.with_borrow_mut(|p| {
        // a late answer for the other tab
        let Some(p) = p.as_mut().filter(|p| p.files == files) else { return false };
        if reset {
            p.items = items;
        } else {
            p.items.extend(items);
        }
        p.more = more;
        p.asked = false;
        true
    });
    if mine {
        let d = app.global::<GalleryData>();
        d.set_loading(false);
        d.set_indexing(indexing);
        rebuild(app);
    }
}

/// Lays the items out in rows again (new items, or another width).
fn rebuild(app: &AppWindow) {
    let d = app.global::<GalleryData>();
    let (head, cell_row, file_row) = (d.get_head_h(), d.get_cell() + d.get_gap(), d.get_file_h());
    PAGE.with_borrow_mut(|p| {
        let Some(p) = p.as_mut() else { return };
        if !p.files && p.cols == 0 {
            // the page hasn't been laid out yet: its width says how many fit in a row
            return;
        }
        let mut rows = Vec::new();
        let mut spans = Vec::new();
        let mut y = 0.0;
        let mut i = 0;
        while i < p.items.len() {
            let month = p.items[i].month.clone();
            spans.push((y, y + head, i..i));
            y += head;
            rows.push(GalleryRow { month: month.as_str().into(), ..Default::default() });
            let end = p.items[i..].iter().position(|it| it.month != month).map_or(p.items.len(), |n| i + n);
            let step = if p.files { 1 } else { p.cols };
            let height = if p.files { file_row } else { cell_row };
            for from in (i..end).step_by(step) {
                let to = (from + step).min(end);
                spans.push((y, y + height, from..to));
                y += height;
                rows.push(row(p, from..to));
            }
            i = end;
        }
        p.spans = spans;
        p.rows.set_vec(rows);
    });
    update_shown();
}

fn row(p: &Page, items: Range<usize>) -> GalleryRow {
    if p.files {
        let it = &p.items[items.start];
        return GalleryRow {
            id: it.id.as_str().into(),
            index: it.index as i32,
            name: it.name.as_str().into(),
            sub: it.sub.as_str().into(),
            danger: it.danger,
            ..Default::default()
        };
    }
    let cells: Vec<GalleryCell> = p.items[items]
        .iter()
        .map(|it| GalleryCell {
            id: it.id.as_str().into(),
            index: it.index as i32,
            nth: it.nth as i32,
            video: it.video,
            photo: p.thumbs.get(&it.key).cloned().unwrap_or_default(),
            state: p.labels.model(&it.key),
        })
        .collect();
    GalleryRow { cells: ModelRc::new(VecModel::from(cells)), ..Default::default() }
}

/// Works out which items are on screen or close, and tells the backend; asks for older items
/// when the view nears the end of what's loaded.
fn update_shown() {
    let ask = PAGE.with_borrow_mut(|p| {
        let p = p.as_mut()?;
        let (top, height) = p.view;
        let (from, to) = (top - height * MARGIN_SCREENS, top + height * (1.0 + MARGIN_SCREENS));
        let mut range = usize::MAX..0;
        for (a, b, items) in &p.spans {
            if *b > from && *a < to && !items.is_empty() {
                range.start = range.start.min(items.start);
                range.end = range.end.max(items.end);
            }
        }
        if range.start > range.end {
            range = 0..0;
        }
        let near_end = {
            let bottom = top + height;
            let left = p.spans.iter().filter(|(a, _, items)| *a >= bottom && !items.is_empty()).count();
            left < MORE_ROWS_LEFT
        };
        let more = p.more && !p.asked && near_end && height > 0.0;
        if more {
            p.asked = true;
        }
        let changed = range != p.shown;
        if changed && !p.files {
            let old = std::mem::replace(&mut p.shown, range.clone());
            // what left the range lets go of its picture; its row is drawn without it
            let gone: Vec<usize> = old.filter(|i| !range.contains(i)).collect();
            for &i in &gone {
                if let Some(it) = p.items.get(i) {
                    p.thumbs.remove(&it.key);
                }
            }
            let keep: std::collections::HashSet<&str> = p.items[range.clone()].iter().map(|it| it.key.as_str()).collect();
            p.labels.retain(|k| keep.contains(k));
            for (r, (_, _, items)) in p.spans.iter().enumerate() {
                if gone.iter().any(|i| items.contains(i)) {
                    slint::Model::set_row_data(&*p.rows, r, row(p, items.clone()));
                }
            }
        } else {
            p.shown = range.clone();
        }
        Some((changed && !p.files, range, more))
    });
    let Some((changed, range, more)) = ask else { return };
    if changed {
        send_cmd(Command::GalleryShown { from: range.start, to: range.end });
    }
    if more {
        send_cmd(Command::GalleryMore);
    }
}

/// A cell's picture: kept if the cell is still in range, its row drawn again.
pub(crate) fn thumb_arrived(t: Arc<types::Thumb>) {
    PAGE.with_borrow_mut(|p| {
        let Some(p) = p.as_mut() else { return };
        let Some(i) = p.items[p.shown.clone()].iter().position(|it| it.key == t.key).map(|i| i + p.shown.start) else { return };
        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&t.rgba, t.width, t.height);
        p.thumbs.insert(t.key.clone(), Image::from_rgba8(buf));
        // a model nested in a row is replaced, not changed in place
        if let Some(r) = p.spans.iter().position(|(_, _, items)| items.contains(&i)) {
            slint::Model::set_row_data(&*p.rows, r, row(p, p.spans[r].2.clone()));
        }
    });
}

pub(crate) fn label_arrived(key: String, label: types::MediaLabel) {
    PAGE.with_borrow(|p| {
        if let Some(p) = p.as_ref() {
            // every cell in range has its model, so this is in place
            p.labels.set(&key, &label);
        }
    });
}
