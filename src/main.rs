#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alloc;
mod avatar;
mod badge;
mod backend;
mod clip;
mod demo;
mod picker;
mod reg;
mod db;
mod emoji;
mod headless;
mod format;
mod install;
mod instance;
mod gui;
mod link;
mod mention;
mod secret;
mod settings;
mod toast;
mod types;
mod update;
mod view;
mod winhook;

use std::path::PathBuf;
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc};
use tokio::sync::mpsc::unbounded_channel;

use gui::*;
use types::{Command, Event};

slint::include_modules!();

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

/// `SIGWIN_DATA`, else `%LOCALAPPDATA%\Sigwin\data`.
fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(d) = std::env::var("SIGWIN_DATA") {
            return PathBuf::from(d);
        }
        directories::ProjectDirs::from("dev", "", "Sigwin")
            .map_or_else(|| PathBuf::from("sigwin-data"), |p| p.data_local_dir().to_path_buf())
    })
    .clone()
}

fn main() -> anyhow::Result<()> {
    if picker::run_if_child() {
        return Ok(());
    }
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env_lossy()
        .add_directive("libsignal=warn".parse()?)
        .add_directive("presage=warn".parse()?)
        .add_directive("sqlx=error".parse()?);
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    if std::env::args().any(|a| a == "--uninstall") {
        install::uninstall(&data_dir());
        return Ok(());
    }
    // started by the update restart: the old process still holds the instance lock briefly
    update::wait_for_previous();
    headless::install_if_requested()?;
    let demo =std::env::var_os("SIGWIN_DEMO").is_some();
    if !headless::active() && !demo && !install::run(&data_dir()) {
        return Ok(());
    }
    // headless runs are offscreen checks: they never hand off to a running app
    let instance = if headless::active() {
        None
    } else {
        match instance::claim(&data_dir()) {
            Some(i) => Some(i),
            None => {
                tracing::info!("already running for {}: brought it to the front", data_dir().display());
                return Ok(());
            }
        }
    };
    if !headless::active() {
        toast::register();
        // left over from a crash: clicking them would do nothing
        toast::clear();
    }

    let app = AppWindow::new()?;
    if headless::active() {
        app.invoke_set_dark(headless::dark());
    }
    let age_timer = Box::leak(Box::new(slint::Timer::default()));
    age_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(30), || {
        UI.with_borrow(|ui| {
            refresh_times(ui);
            drafts::refresh_draft_times(ui);
        });
    });
    UI.with_borrow(|ui| {
        app.set_threads(ModelRc::from(ui.threads.clone()));
        app.set_messages(ModelRc::from(ui.messages.clone()));
        app.set_packs(ModelRc::from(ui.packs.clone()));
        app.set_stickers(ModelRc::from(ui.stickers.clone()));
    });

    let (cmd_tx, cmd_rx) = unbounded_channel::<Command>();
    let weak = app.as_weak();
    let emit: backend::Emit = Arc::new(move |ev: Event| {
        let _ = weak.upgrade_in_event_loop(move |app| apply(&app, ev));
    });
    if let Some(instance) = instance {
        let weak = app.as_weak();
        instance.serve(
            move || {
                let _ = weak.upgrade_in_event_loop(|app| to_front(&app));
            },
            // installer or uninstaller: leave through the normal exit path below
            || {
                let _ = slint::quit_event_loop();
            },
        );
    }
    let weak = app.as_weak();
    toast::init(backend::Paths::new(&data_dir()).open, move |hit| {
        let _ = weak.upgrade_in_event_loop(move |app| open_from_toast(&app, hit));
    });
    if demo {
        demo::start(&app, emit, cmd_rx);
    } else {
        let _backend = backend::spawn(data_dir(), cmd_rx, emit);
        if let Some(exe) = install::installed_self() {
            let weak = app.as_weak();
            update::spawn(exe, move |v| {
                let _ = weak.upgrade_in_event_loop(move |app| app.set_update_version(v.into()));
            });
        }
    }
    app.on_restart_for_update(update::restart);

    let paste_dir = data_dir().join("paste");
    clip::clean(&paste_dir);
    let settings_path = settings::path(&data_dir());
    let prefs = settings::Settings::load(&settings_path);
    prefs.publish();
    badge::set_enabled(prefs.badge);
    show_settings(&app, &prefs);
    app.set_version(format!("Sigwin {}", env!("CARGO_PKG_VERSION")).into());
    UI.with_borrow_mut(|ui| {
        ui.tx = Some(cmd_tx.clone());
        ui.paste_dir = paste_dir;
        ui.settings = prefs;
        ui.settings_path = settings_path;
        ui.drafts_path = data_dir().join("drafts.json");
        ui.drafts = drafts::load(&ui.drafts_path);
    });
    apply_theme(&app, true);
    wire(&app, cmd_tx);
    app.run()?;
    // like Signal's `fastClear` on unload: a toast we can no longer act on goes away
    if !headless::active() {
        toast::clear_now();
    }
    drafts::save_on_exit(&app);
    // plain copies of opened attachments; the backend also empties this on start (crashes,
    // files another app still held)
    backend::files::clear_open(&data_dir());
    Ok(())
}
