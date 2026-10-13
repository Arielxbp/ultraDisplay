//! Logs to stderr (like the daemon) and mirrors each line into the app's Log page.

use std::io::Write;
use std::sync::OnceLock;

static SINK: OnceLock<async_channel::Sender<String>> = OnceLock::new();

struct Logger {
    level: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= self.level
            // Our crates at the configured level; dependencies only for warnings and errors.
            && (meta.target().starts_with("ud")
                || meta.target().starts_with("ultradisplay")
                || meta.level() <= log::Level::Warn)
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let time = gtk::glib::DateTime::now_local()
            .and_then(|t| t.format("%H:%M:%S"))
            .map(|t| t.to_string())
            .unwrap_or_default();
        let line = format!(
            "{time} {:<5} {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
        let _ = writeln!(std::io::stderr(), "{line}");
        if let Some(tx) = SINK.get() {
            let _ = tx.try_send(line);
        }
    }

    fn flush(&self) {}
}

/// Installs the logger; returns the receiver for the Log page.
pub fn init() -> async_channel::Receiver<String> {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(log::LevelFilter::Info);
    let (tx, rx) = async_channel::bounded(4096);
    let _ = SINK.set(tx);
    let _ = log::set_boxed_logger(Box::new(Logger { level }));
    log::set_max_level(level);
    rx
}
