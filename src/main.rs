#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alloc;
mod autostart;
mod away;
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
mod logfile;
mod notices;
mod mention;
mod sealed;
mod secret;
mod settings;
mod toast;
mod tray;
mod types;
mod update;
mod videoframe;
mod view;
mod whatsnew;
mod winhook;

use std::path::PathBuf;
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString};
use tokio::sync::mpsc::unbounded_channel;

use gui::{
    apply, apply_theme, drafts, ensure_shown, open_from_toast, preset_scale, refresh_times, save_geometry, show_settings,
    to_front, wire, UI,
};
use types::{Command, Event};

pub use sigwin_ui::*;

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

/// "Sigwin 0.1.3", or "Sigwin 0.1.3 (개발 빌드, 34f43ce)" for a dev build.
fn version_label() -> String {
    let v = env!("CARGO_PKG_VERSION");
    #[cfg(sigwin_dev)]
    return match env!("SIGWIN_GIT_HASH") {
        "" => format!("Sigwin {v} (개발 빌드)"),
        hash => format!("Sigwin {v} (개발 빌드, {hash})"),
    };
    #[cfg(not(sigwin_dev))]
    format!("Sigwin {v}")
}


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
    if picker::run_if_child() || videoframe::run_if_child() {
        return Ok(());
    }
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env_lossy()
        .add_directive("libsignal=warn".parse()?)
        .add_directive("presage=warn".parse()?)
        .add_directive("sqlx=error".parse()?);
    // the uninstaller may delete the data folder, so it must not hold a file open in it
    let uninstall = std::env::args().any(|a| a == "--uninstall");
    // the first run that seals the state files: logs from before `logfile::scrub` go with them
    if !uninstall && data_dir().join("state.json").exists() {
        let _ = std::fs::remove_dir_all(logfile::dir(&data_dir()));
    }
    let file = if uninstall {
        None
    } else {
        logfile::LogFile::open(&data_dir()).map_err(|e| eprintln!("log file: {e}")).ok()
    };
    {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(filter)
            .with(logfile::Noise)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(file.map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(f)))
            .init();
    }
    logfile::install_panic_hook();
    tracing::info!("Sigwin {} starting ({})", env!("CARGO_PKG_VERSION"), std::env::current_exe().unwrap_or_default().display());
    if uninstall {
        install::uninstall(&data_dir());
        return Ok(());
    }
    // started by the update restart: the old process still holds the instance lock briefly
    update::wait_for_previous();
    headless::install_if_requested()?;
    let demo =std::env::var_os("SIGWIN_DEMO").is_some();
    // a backend with no account and no network, fed by scenario steps
    let fixture = std::env::var_os("SIGWIN_FIXTURE").is_some();
    anyhow::ensure!(!fixture || std::env::var_os("SIGWIN_DATA").is_some(), "SIGWIN_FIXTURE requires SIGWIN_DATA (a scratch dir)");
    if !headless::active() && !demo && !fixture && !install::run(&data_dir()) {
        return Ok(());
    }
    // headless runs are offscreen checks: they never hand off to a running app
    let instance = if headless::active() {
        None
    } else {
        // a dev build never hands off: the installed copy coming up instead hides that the
        // build didn't start
        let dev = install::is_dev_build();
        match instance::claim(&data_dir(), !dev) {
            Some(i) => Some(i),
            None if dev => {
                tracing::error!("already running for {}: dev build not started", data_dir().display());
                // scratch-dir runs are automation: no modal box to block on
                if std::env::var_os("SIGWIN_DATA").is_none() {
                    install::error("Sigwin이 이미 실행 중이라 개발 빌드를 시작하지 않았습니다.

실행 중인 Sigwin(설치본 또는 다른 빌드)을 종료한 뒤 다시 실행하세요.");
                }
                return Ok(());
            }
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
        headless::bump();
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
    // before anything reads the encrypted files; a failure is the backend's to show
    if let Err(e) = backend::unlock(&data_dir()) {
        tracing::warn!("unlock: {e:#}");
    }
    if demo {
        demo::start(&app, emit, cmd_rx);
    } else if fixture {
        let _backend = backend::spawn_offline(data_dir(), cmd_rx, emit);
        gui::scenario_install(&app);
    } else {
        let _backend = backend::spawn(data_dir(), cmd_rx, emit);
        if let Some(exe) = install::installed_self() {
            let weak = app.as_weak();
            update::spawn(exe, move |st| {
                let _ = weak.upgrade_in_event_loop(move |app| {
                    app.set_update_checking(false);
                    app.set_update_installing(false);
                    match st {
                        update::Status::Ready(v) => {
                            app.set_update_available(SharedString::new());
                            app.set_update_version(v.into());
                        }
                        update::Status::Available(v) => app.set_update_available(v.into()),
                        update::Status::UpToDate => {
                            app.set_update_available(SharedString::new());
                            app.set_update_status("최신 버전입니다.".into());
                        }
                        update::Status::Failed => {
                            app.set_update_status("확인하지 못했습니다. 네트워크를 확인하고 다시 시도하세요.".into())
                        }
                    }
                });
            });
        }
    }
    app.on_restart_for_update(update::restart);
    let weak = app.as_weak();
    app.on_install_update(move || {
        let Some(app) = weak.upgrade() else { return };
        if !update::install() {
            app.set_update_installing(false);
        }
    });
    let weak = app.as_weak();
    app.on_check_update(move || {
        let Some(app) = weak.upgrade() else { return };
        if update::check_now() {
            app.set_update_status(SharedString::new());
            app.set_update_checking(true);
        } else {
            app.set_update_status("설치된 Sigwin에서만 업데이트할 수 있습니다.".into());
        }
    });
    app.on_open_logs(|| {
        let dir = logfile::dir(&data_dir());
        if let Err(e) = std::process::Command::new("explorer").arg(&dir).spawn() {
            tracing::warn!("open logs: {e}");
        }
    });

    let paste_dir = data_dir().join("paste");
    clip::clean(&paste_dir);
    let settings_path = settings::path(&data_dir());
    let prefs = settings::Settings::load(&settings_path);
    prefs.publish();
    if prefs.autostart {
        // keeps the command current (exe moved by a reinstall, flag changed)
        autostart::apply(true, prefs.start_hidden, false);
    }
    show_settings(&app, &prefs);
    app.set_version(version_label().into());
    // dev builds: commit hash in the title (the icon: `gui::window::set_dev_icon`, tray)
    #[cfg(sigwin_dev)]
    app.set_dev_tag(match env!("SIGWIN_GIT_HASH") {
        "" => "개발".into(),
        hash => hash.into(),
    });
    UI.with_borrow_mut(|ui| {
        ui.tx = Some(cmd_tx.clone());
        ui.paste_dir = paste_dir;
        ui.settings = prefs;
        ui.settings_path = settings_path;
        ui.drafts_path = data_dir().join("drafts.enc");
        ui.drafts = drafts::load(&ui.drafts_path);
    });
    apply_theme(&app, true);
    wire(&app, cmd_tx.clone());
    if !headless::active() {
        let weak = app.as_weak();
        tray::install(move || {
            if let Some(app) = weak.upgrade() {
                to_front(&app);
            }
        });
    }
    if std::env::args().any(|a| a == autostart::BACKGROUND_FLAG) && !headless::active() && tray::active() {
        // autostart: the window stays unshown until the tray icon is used. It isn't in the
        // foreground, so notifications must not wait for a focus event that never comes.
        let _ = cmd_tx.send(Command::WindowFocus(false));
        preset_scale(UI.with_borrow(|ui| ui.settings.zoom));
        slint::run_event_loop_until_quit()?;
        let _ = app.hide();
    } else {
        if !headless::active() {
            ensure_shown(&app);
        }
        app.run()?;
    }
    save_geometry(&app);
    tray::remove();
    // like Signal's `fastClear` on unload: a toast we can no longer act on goes away
    if !headless::active() {
        toast::clear_now();
    }
    drafts::save_on_exit(&app);
    if headless::failed() {
        // a scenario check failed: say so to whoever ran it
        std::process::exit(1);
    }
    // plain copies of opened attachments; the backend also empties this on start (crashes,
    // files another app still held)
    backend::files::clear_open(&data_dir());
    // pasted images, plain as well: a draft's paste doesn't outlive the run anyway (`clip::clean`)
    clip::clean(&data_dir().join("paste"));
    Ok(())
}
