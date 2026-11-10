//! Scenario steps of the headless mode (`SIGWIN_FIXTURE`): they drive the real window and
//! the offline backend (`backend/fixture.rs`), and check what came of it.

use super::*;

/// Hands the headless loop what it needs to run scenario steps.
pub(crate) fn install(app: &AppWindow) {
    let weak = app.as_weak();
    let busy = app.as_weak();
    headless::set_driver(headless::Driver {
        exec: Box::new(move |line| {
            let app = weak.upgrade().ok_or_else(|| anyhow::anyhow!("window is gone"))?;
            exec(&app, line)
        }),
        sync: Box::new(|| {
            let _ = backend::fixture::run("query window");
        }),
        busy: Box::new(move || {
            busy.upgrade().is_some_and(|app| app.get_reading_hold() || !app.get_jump_id().is_empty())
        }),
    });
}

fn exec(app: &AppWindow, line: &str) -> anyhow::Result<()> {
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let on_off = |what: &str| -> anyhow::Result<bool> {
        match rest {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => anyhow::bail!("{what}: expected on or off"),
        }
    };
    match cmd {
        "fx" => {
            let answer = backend::fixture::run(rest)?;
            anyhow::ensure!(!answer.starts_with("error"), "fx {rest}: {answer}");
        }
        "open" => {
            let who: u32 = rest.parse()?;
            app.invoke_open_thread(format!("c:{}", backend::fixture::contact(who)).into());
        }
        "jump" => app.invoke_jump_to_bottom(),
        // files into the composer, as if picked: `attach A|B`
        "attach" => wire::add_files(app, rest.split('|').map(|p| p.trim().into()).collect()),
        "focus" => window::focus_changed(app, on_off("focus")?),
        "away" => {
            away::force(Some(on_off("away")?));
            window::refresh_active(app);
        }
        // the window can be seen (not covered), whatever the headless one is
        "shown" => {
            away::force_on_screen(Some(on_off("shown")?));
            window::refresh_active(app);
        }
        "expect" => expect(app, rest)?,
        _ => anyhow::bail!("unknown scenario step: {line}"),
    }
    Ok(())
}

/// `backend WHAT [who=N] OP VALUE` or `ui NAME OP VALUE`.
fn expect(app: &AppWindow, rest: &str) -> anyhow::Result<()> {
    let (target, rest) = rest.split_once(char::is_whitespace).ok_or_else(|| anyhow::anyhow!("expect: nothing to check"))?;
    let words: Vec<&str> = rest.split_whitespace().collect();
    let op_at = words
        .iter()
        .position(|w| ["=", "!=", "<", "<=", ">", ">="].contains(w))
        .ok_or_else(|| anyhow::anyhow!("expect {rest}: no comparison"))?;
    let (subject, op, want) = (&words[..op_at], words[op_at], words[op_at + 1..].join(" "));
    let got = match target {
        "backend" => {
            let answer = backend::fixture::run(&format!("query {}", subject.join(" ")))?;
            anyhow::ensure!(!answer.starts_with("error"), "expect {rest}: {answer}");
            answer
        }
        "ui" => probe(app, subject.first().copied().unwrap_or(""))?,
        _ => anyhow::bail!("expect: backend or ui, not {target}"),
    };
    if !holds(&got, op, &want) {
        headless::fail(&format!("expect {target} {rest}: got {got}"));
    }
    Ok(())
}

/// A value of the window, as text.
fn probe(app: &AppWindow, name: &str) -> anyhow::Result<String> {
    use slint::Model;
    Ok(match name {
        "open-unread" => app.get_open_unread().to_string(),
        "inactive" => app.get_inactive().to_string(),
        "reading-hold" => app.get_reading_hold().to_string(),
        "near-bottom" => app.get_near_bottom().to_string(),
        "rows" => app.get_messages().row_count().to_string(),
        "current-key" => app.get_current_key().to_string(),
        // the request bar shows instead of the composer
        "current-request" => app.get_current_request().to_string(),
        "cells" => app.get_messages().iter().map(|m| m.cells.row_count()).sum::<usize>().to_string(),
        // cells that play in place when the pointer is on them, and those playing now
        "gif-cells" => app.get_messages().iter().flat_map(|m| m.cells.iter().collect::<Vec<_>>()).filter(|c| c.gif).count().to_string(),
        "playing-cells" => app.get_messages().iter().flat_map(|m| m.cells.iter().collect::<Vec<_>>()).filter(|c| c.frames.row_count() > 0).count().to_string(),
        // cells of a photo grid that show nothing (yet)
        "blank-cells" => app.get_messages().iter().flat_map(|m| m.cells.iter().collect::<Vec<_>>()).filter(|c| c.image.size().width == 0).count().to_string(),
        // how far the newest row's end is below the list's visible bottom edge (0: at the bottom)
        "bottom-gap" => (app.invoke_content_height() + app.invoke_list_content_y() - app.invoke_list_visible_height()).round().to_string(),
        // where the "unread" line's row starts, from the top of the visible list
        "divider-top" => app.get_divider_top().round().to_string(),
        // the row across the list's top edge and where its top is (negative: above the edge)
        "top-id" => app.get_view_top_id().to_string(),
        "top-off" => app.get_view_top_off().round().to_string(),
        _ => anyhow::bail!("expect ui: no value called {name}"),
    })
}

fn holds(got: &str, op: &str, want: &str) -> bool {
    match (got.parse::<f64>(), want.parse::<f64>()) {
        (Ok(a), Ok(b)) => match op {
            "=" => (a - b).abs() < 0.5,
            "!=" => (a - b).abs() >= 0.5,
            "<" => a < b,
            "<=" => a <= b,
            ">" => a > b,
            _ => a >= b,
        },
        _ => match op {
            "=" => got == want,
            "!=" => got != want,
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::holds;

    #[test]
    fn comparisons() {
        assert!(holds("25", "=", "25"));
        assert!(holds("3.2", "<=", "4"));
        assert!(holds("false", "=", "false"));
        assert!(holds("delivery=0 read=5", "!=", "delivery=0 read=6"));
        assert!(!holds("abc", "<", "abd"));
    }
}
