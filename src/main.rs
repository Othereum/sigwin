#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// first: `tr!` is in scope for the modules after it
#[macro_use]
mod i18n;
mod alloc;
mod attached;
mod autostart;
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
mod hangul;
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
mod shell;
mod stickerpack;
mod toast;
mod tray;
mod types;
mod update;
mod videoframe;
mod view;
mod winhook;

use std::path::PathBuf;
use std::sync::Arc;

use slint::ComponentHandle;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use gui::{
    apply, apply_theme, bind_models, drafts, ensure_shown, open_from_toast, preset_scale, save_geometry, show_settings,
    start_updates, to_front, wire, UI,
};
use types::{Command, Event};

pub use sigwin_ui::*;

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

/// "Sigwin 0.1.3", or "Sigwin 0.1.3 (dev build, 34f43ce)" for a dev build.
fn version_label() -> String {
    let v = env!("CARGO_PKG_VERSION");
    #[cfg(sigwin_dev)]
    return match env!("SIGWIN_GIT_HASH") {
        "" => format!("Sigwin {v} ({})", tr!("dev build")),
        hash => format!("Sigwin {v} ({}, {hash})", tr!("dev build")),
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

/// Logs to stderr, and to the log file in the data dir with `to_file`.
fn init_logging(to_file: bool) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env_lossy()
        .add_directive("libsignal=warn".parse()?)
        .add_directive("presage=warn".parse()?)
        .add_directive("sqlx=error".parse()?);
    // the first run that seals the state files: logs from before `logfile::scrub` go with them
    if to_file && data_dir().join("state.json").exists() {
        let _ = std::fs::remove_dir_all(logfile::dir(&data_dir()));
    }
    let file = if to_file { logfile::LogFile::open(&data_dir()).map_err(|e| eprintln!("log file: {e}")).ok() } else { None };
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(file.map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(f)))
        .init();
    logfile::install_panic_hook();
    tracing::info!("Sigwin {} starting ({})", env!("CARGO_PKG_VERSION"), std::env::current_exe().unwrap_or_default().display());
    Ok(())
}

/// This process becomes the one for the data dir; None if another one already is (brought to
/// the front, unless this is a dev build).
fn claim_instance() -> Option<instance::Instance> {
    // a dev build never hands off: the installed copy coming up instead hides that the
    // build didn't start
    let dev = install::is_dev_build();
    let claimed = instance::claim(&data_dir(), !dev);
    if claimed.is_none() {
        if dev {
            tracing::error!("already running for {}: dev build not started", data_dir().display());
            // scratch-dir runs are automation: no modal box to block on
            if std::env::var_os("SIGWIN_DATA").is_none() {
                install::error(tr!("Sigwin is already running, so the dev build didn't start.\n\nQuit the running Sigwin (installed or another build) and start this one again."));
            }
        } else {
            tracing::info!("already running for {}: brought it to the front", data_dir().display());
        }
    }
    claimed
}

/// Settings, drafts and the command channel into the UI state; settings onto the settings page.
fn load_ui_state(app: &AppWindow, cmd_tx: UnboundedSender<Command>) {
    let paste_dir = data_dir().join("paste");
    clip::clean(&paste_dir);
    attached::set_paste_dir(paste_dir);
    let settings_path = settings::path(&data_dir());
    let prefs = settings::Settings::load(&settings_path);
    prefs.publish();
    if prefs.autostart {
        // keeps the command current (exe moved by a reinstall, flag changed)
        autostart::apply(true, prefs.start_hidden, false);
    }
    show_settings(app, &prefs);
    // the language this run started with: picking another one asks for a restart
    let lang = settings::LANGUAGES.iter().position(|l| *l == prefs.language).unwrap_or(0) as i32;
    app.global::<SettingsData>().set_language_at_start(lang);
    app.global::<SettingsData>().set_version(version_label().into());
    app.global::<SettingsData>().set_current_version(env!("CARGO_PKG_VERSION").into());
    // dev builds: commit hash in the title (the icon: `gui::window::set_dev_icon`, tray)
    #[cfg(sigwin_dev)]
    app.set_dev_tag(match env!("SIGWIN_GIT_HASH") {
        "" => tr!("dev").into(),
        hash => hash.into(),
    });
    UI.with_borrow_mut(|ui| {
        ui.tx = Some(cmd_tx);
        ui.settings = prefs;
        ui.settings_path = settings_path;
        ui.drafts_path = data_dir().join("drafts.enc");
        ui.drafts = drafts::load(&ui.drafts_path);
    });
}

fn main() -> anyhow::Result<()> {
    if picker::run_if_child() || videoframe::run_if_child() {
        return Ok(());
    }
    // the uninstaller may delete the data folder, so it must not hold a file open in it
    let uninstall = std::env::args().any(|a| a == "--uninstall");
    init_logging(!uninstall)?;
    #[cfg(feature = "dhat-heap")]
    let _profile = alloc::profile(&logfile::dir(&data_dir()));
    let prefs = settings::Settings::load(&settings::path(&data_dir()));
    let offscreen = std::env::var_os("SIGWIN_HEADLESS").is_some();
    let lang = i18n::init(offscreen, &prefs.language);
    view::time::set_clock(prefs.clock, offscreen);
    // before the backend builds a row
    view::media::set_sizes(prefs.photo_size, prefs.sticker_size);
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
        match claim_instance() {
            Some(i) => Some(i),
            None => return Ok(()),
        }
    };
    if !headless::active() {
        toast::register();
        // left over from a crash: clicking them would do nothing
        toast::clear();
    }

    let app = AppWindow::new()?;
    // after the first component: creating it selects the system's language
    if let Err(e) = slint::select_bundled_translation(lang) {
        tracing::warn!("UI language {lang}: {e}");
    }
    if headless::active() {
        app.invoke_set_dark(headless::dark());
    }
    bind_models(&app);

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
        // updates first: a backend failing at once calls `update::start`
        let recover = install::installed_self().is_some_and(|exe| start_updates(&app, exe, &data_dir()));
        if recover {
            // the last run died before it could update; this one may die the same way, so the
            // fix gets its chance first (bounded: offline, the app must still come up)
            emit(Event::Loading(tr!("Checking for updates…").into()));
            std::thread::spawn(move || {
                if update::wait_first_check(std::time::Duration::from_secs(120)) {
                    // the loading page offers the restart
                    emit(Event::Loading(String::new()));
                    return;
                }
                let _backend = backend::spawn(data_dir(), cmd_rx, emit);
            });
        } else {
            let _backend = backend::spawn(data_dir(), cmd_rx, emit);
        }
    }
    app.global::<SettingsData>().on_open_logs(|| {
        let dir = logfile::dir(&data_dir());
        if let Err(e) = std::process::Command::new("explorer").arg(&dir).spawn() {
            tracing::warn!("open logs: {e}");
        }
    });

    load_ui_state(&app, cmd_tx.clone());
    apply_theme(&app);
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
        gui::presence::focus_changed(&app, false);
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
    // pasted images: a draft's paste doesn't outlive the run (`clip::clean`)
    clip::clean(attached::paste_dir());
    Ok(())
}
