//! UltraDisplay host app: the streaming host plus a window with live status, settings,
//! paired tablets and the log. Closing the window keeps it running in the tray; the tray
//! menu (or the window menu) has Quit.
//!
//! Usage: ultradisplay [--hidden] [--config PATH] [--page status|settings|tablets|log]

mod logger;
mod notify;
mod tray;
mod ui;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};
use ud_host::{Config, Event, Host};

const APP_ID: &str = "io.github.ultradisplay.Host";

fn main() -> glib::ExitCode {
    ud_host::stop_fd_inheritance();
    let args: Vec<String> = std::env::args().collect();
    let hidden = args.iter().any(|a| a == "--hidden");
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .or_else(Config::default_path);
    let page = args
        .iter()
        .position(|a| a == "--page")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let log_rx = logger::init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    let state: Rc<RefCell<Option<Rc<ui::App>>>> = Rc::default();

    // Runs once, in the primary instance. Launching again just activates it.
    app.connect_startup({
        let state = state.clone();
        move |app| {
            match startup(app, config_path.clone(), log_rx.clone()) {
                Ok(ui) => {
                if let Some(page) = &page {
                    ui.stack.set_visible_child_name(page);
                }
                *state.borrow_mut() = Some(ui);
            }
                Err(err) => {
                    log::error!("{err:#}");
                    app.quit();
                }
            }
        }
    });

    let first_activation = std::cell::Cell::new(true);
    app.connect_activate(move |_| {
        let skip = first_activation.replace(false) && hidden;
        if let Some(ui) = state.borrow().as_ref()
            && !skip
        {
            ui.window.present();
        }
    });

    // GTK must not parse our flags.
    app.run_with_args(&args[..1])
}

fn startup(
    app: &adw::Application,
    config_path: Option<PathBuf>,
    log_rx: async_channel::Receiver<String>,
) -> anyhow::Result<Rc<ui::App>> {
    let config = match &config_path {
        Some(path) => Config::load(path).unwrap_or_else(|err| {
            log::error!("{err:#}; using defaults");
            Config::default()
        }),
        None => Config::default(),
    };
    // The GUI has no separate adb switch: USB on means forwarding is managed for you
    // (`adb = false` is only for the headless daemon, e.g. when you run adb yourself).
    let mut config = config;
    config.adb = true;

    let (ev_tx, ev_rx) = async_channel::unbounded::<Event>();
    let sink = Arc::new(move |e: Event| {
        let _ = ev_tx.send_blocking(e);
    });
    let host = Arc::new(Host::new(config, None, sink)?);
    let ui = ui::App::new(app, host.clone(), config_path);

    let (act_tx, act_rx) = async_channel::unbounded();
    let act_tx_signals = act_tx.clone();
    let tray = tray::spawn(act_tx);
    let has_tray = tray.is_some();

    // Quit: stop streaming (removes the virtual output), then exit.
    let quit = gio::SimpleAction::new("quit", None);
    quit.connect_activate({
        let (app, host, window) = (app.clone(), host.clone(), ui.window.clone());
        move |_, _| {
            log::info!("quitting");
            window.set_visible(false);
            // Stop on a worker thread (removes the virtual output); never let a stuck stop
            // keep the process alive.
            let (tx, rx) = async_channel::bounded::<()>(1);
            let host = host.clone();
            std::thread::spawn(move || {
                host.stop();
                let _ = tx.send_blocking(());
            });
            let app = app.clone();
            glib::spawn_future_local(async move {
                if glib::future_with_timeout(std::time::Duration::from_secs(5), rx.recv())
                    .await
                    .is_err()
                {
                    log::error!("stopping took too long; exiting anyway");
                    // Skip atexit handlers: a stuck thread may hold the CUDA context.
                    unsafe { libc::_exit(1) };
                }
                app.quit();
            });
        }
    });
    app.add_action(&quit);
    // Also reachable as `gapplication action io.github.ultradisplay.Host restart`.
    let restart = gio::SimpleAction::new("restart", None);
    restart.connect_activate({
        let ui = ui.clone();
        move |_, _| ui.restart()
    });
    app.add_action(&restart);
    // SIGTERM (e.g. `systemctl --user stop`) / SIGINT: quit cleanly, via the tray-action
    // channel so it runs on the GTK thread.
    {
        use signal_hook::consts::{SIGINT, SIGTERM};
        let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])?;
        let tx = act_tx_signals;
        std::thread::Builder::new().name("signals".into()).spawn(move || {
            if signals.forever().next().is_some() {
                let _ = tx.send_blocking(tray::Action::Quit);
            }
        })?;
    }

    // Closing the window hides it to the tray (or quits if there's no tray).
    ui.window.set_hide_on_close(has_tray);
    if !has_tray {
        ui.window.connect_close_request({
            let app = app.clone();
            move |_| {
                app.activate_action("quit", None);
                glib::Propagation::Proceed
            }
        });
    }
    // Keep running while the window is hidden.
    std::mem::forget(app.hold());

    // Host events -> UI (+ tray status).
    glib::spawn_future_local({
        let ui = ui.clone();
        async move {
            while let Ok(event) = ev_rx.recv().await {
                ui.handle(event);
                if let Some(tray) = &tray {
                    let (status, streaming) = (ui.status.borrow().clone(), ui.streaming.get());
                    tray.update(move |t| {
                        t.status = status;
                        t.streaming = streaming;
                    });
                }
            }
        }
    });
    glib::spawn_future_local({
        let ui = ui.clone();
        async move {
            while let Ok(line) = log_rx.recv().await {
                ui.append_log(&line);
            }
        }
    });
    glib::spawn_future_local({
        let (ui, app) = (ui.clone(), app.clone());
        async move {
            while let Ok(action) = act_rx.recv().await {
                match action {
                    tray::Action::Show => ui.window.present(),
                    tray::Action::Quit => app.activate_action("quit", None),
                }
            }
        }
    });

    ui.host_op(ui::HostOp::Start);
    Ok(ui)
}
