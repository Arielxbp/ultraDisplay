//! UltraDisplay headless daemon: the host runtime without the GUI.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use ud_host::{Config, Host, Placement, service};

#[derive(Parser, Debug)]
#[command(version, about = "Use an Android tablet as a 120 Hz niri output")]
struct Args {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Config file (default: ~/.config/ultradisplay/config.toml). Flags below override it.
    #[arg(long)]
    config: Option<PathBuf>,

    /// TCP port the tablet connects to (through `adb reverse` over USB).
    #[arg(long)]
    port: Option<u16>,

    /// Name of the niri virtual output to create for the tablet.
    #[arg(long)]
    output: Option<String>,

    /// Output scale for the tablet (2800x1752 at 2.0 = 1400x876 logical).
    #[arg(long)]
    scale: Option<f64>,

    /// Placement relative to the monitor at (0, 0): auto, left, right, above, below.
    #[arg(long, value_parser = parse_placement)]
    placement: Option<Placement>,

    /// Logical position X,Y for the tablet output (implies manual placement).
    #[arg(long, value_parser = parse_position)]
    position: Option<(i32, i32)>,

    /// Video bitrate in Mbit/s.
    #[arg(long)]
    bitrate_mbps: Option<u32>,

    /// Upper bound for the stream frame rate.
    #[arg(long)]
    max_fps: Option<u32>,

    /// Keep the virtual output when the tablet disconnects.
    #[arg(long)]
    keep_output: bool,

    /// niri IPC socket (default: $NIRI_SOCKET).
    #[arg(long)]
    niri_socket: Option<PathBuf>,

    /// Don't accept tablets over USB.
    #[arg(long)]
    no_usb: bool,

    /// Don't set up `adb reverse` for USB-connected tablets.
    #[arg(long)]
    no_adb: bool,

    /// Don't accept tablets over Wi-Fi.
    #[arg(long)]
    no_lan: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Install and enable a systemd user unit that starts this headless daemon with the
    /// "Niri (virtual outputs)" session (the GUI app has its own switch for this).
    InstallService,
    /// Stop, disable and remove that unit.
    UninstallService,
}

impl Args {
    fn apply(self, cfg: &mut Config) {
        if let Some(v) = self.port {
            cfg.port = v;
        }
        if let Some(v) = self.output {
            cfg.output = v;
        }
        if let Some(v) = self.scale {
            cfg.scale = v;
        }
        if let Some(v) = self.placement {
            cfg.placement = v;
        }
        if let Some((x, y)) = self.position {
            cfg.placement = Placement::Manual;
            cfg.position = Some([x, y]);
        }
        if let Some(v) = self.bitrate_mbps {
            cfg.bitrate_mbps = v;
        }
        if let Some(v) = self.max_fps {
            cfg.max_fps = v;
        }
        cfg.keep_output |= self.keep_output;
        cfg.usb &= !self.no_usb;
        cfg.adb &= !self.no_adb;
        cfg.lan &= !self.no_lan;
    }
}

/// On SIGINT/SIGTERM, stop the host (removing the virtual output, so niri moves the
/// tablet's windows back) and exit.
fn spawn_signal_handler(host: Arc<Host>) -> anyhow::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};

    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if let Some(sig) = signals.forever().next() {
                log::info!("signal {sig}, shutting down");
                host.stop();
                // Not process::exit: the CUDA driver's atexit teardown can deadlock while
                // another thread still holds its context. The kernel frees everything.
                unsafe { libc::_exit(0) };
            }
        })?;
    Ok(())
}

fn parse_placement(s: &str) -> Result<Placement, String> {
    match s {
        "auto" => Ok(Placement::Auto),
        "left" => Ok(Placement::Left),
        "right" => Ok(Placement::Right),
        "above" => Ok(Placement::Above),
        "below" => Ok(Placement::Below),
        _ => Err("expected auto, left, right, above or below".into()),
    }
}

fn parse_position(s: &str) -> Result<(i32, i32), String> {
    let (x, y) = s.split_once(',').ok_or("expected X,Y")?;
    let parse = |v: &str| v.trim().parse::<i32>().map_err(|e| e.to_string());
    Ok((parse(x)?, parse(y)?))
}

/// Console logging as configured by RUST_LOG (default info), plus the log file at debug.
struct Logger {
    console: env_logger::Logger,
    file: Option<&'static ud_host::logfile::LogFile>,
}

impl log::Log for Logger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        self.console.enabled(meta) || (self.file.is_some() && ud_host::logfile::wants(meta))
    }

    fn log(&self, record: &log::Record) {
        if let Some(file) = self.file {
            file.write(record);
        }
        if self.console.matches(record) {
            self.console.log(record);
        }
    }

    fn flush(&self) {
        self.console.flush();
    }
}

fn init_logging() {
    let console =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let file = ud_host::logfile::get();
    let level = if file.is_some() {
        console.filter().max(log::LevelFilter::Debug)
    } else {
        console.filter()
    };
    let _ = log::set_boxed_logger(Box::new(Logger { console, file }));
    log::set_max_level(level);
    if let Some(file) = file {
        log::info!("logging to {}", file.path().display());
    }
}

fn main() -> anyhow::Result<()> {
    ud_host::stop_fd_inheritance();
    init_logging();
    let args = Args::parse();
    match args.command {
        // Inside the AppImage the headless daemon is reached with the `headless` argument.
        Some(Cmd::InstallService) => {
            let headless: &[&str] = if std::env::var_os("APPIMAGE").is_some() { &["headless"] } else { &[] };
            return service::install(headless, true);
        }
        Some(Cmd::UninstallService) => return service::uninstall(true),
        None => {}
    }

    let config_path = args.config.clone().or_else(Config::default_path);
    let mut cfg = match &config_path {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    let niri_socket = args.niri_socket.clone();
    args.apply(&mut cfg);
    log::debug!("config ({config_path:?}): {cfg:?}");

    // Headless: events are already in the log.
    let host = Arc::new(Host::new(cfg, niri_socket, Arc::new(|_| {}))?);
    spawn_signal_handler(host.clone())?;
    host.start()?;
    loop {
        std::thread::park();
    }
}
