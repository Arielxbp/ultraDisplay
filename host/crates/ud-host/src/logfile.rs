//! Persistent log at `$XDG_STATE_HOME/ultradisplay/ultradisplay.log` (default
//! `~/.local/state/...`), shared by the app and the headless daemon. The previous run is
//! kept as `ultradisplay.log.1`. It records our crates at debug level (the per-second
//! network diagnostics), whatever the console/Log page level is.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Rotate to `.1` mid-run past this size, so a long session can't fill the disk.
const MAX_BYTES: u64 = 50 << 20;

pub struct LogFile {
    path: PathBuf,
    file: Mutex<(File, u64)>,
}

static LOG: OnceLock<Option<LogFile>> = OnceLock::new();

pub fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("ultradisplay"))
}

/// The log file, opened (and the previous one rotated) on first use. `None` if it can't
/// be created; logging then just goes to the console.
pub fn get() -> Option<&'static LogFile> {
    LOG.get_or_init(|| {
        let dir = state_dir()?;
        let path = dir.join("ultradisplay.log");
        let open = || -> std::io::Result<File> {
            fs::create_dir_all(&dir)?;
            let _ = fs::rename(&path, path.with_extension("log.1"));
            OpenOptions::new().create(true).append(true).open(&path)
        };
        match open() {
            Ok(file) => Some(LogFile { path, file: Mutex::new((file, 0)) }),
            Err(err) => {
                eprintln!("ultradisplay: cannot open log file {}: {err}", path.display());
                None
            }
        }
    })
    .as_ref()
}

/// Whether the file wants `meta`: our crates at debug, dependencies at warn.
pub fn wants(meta: &log::Metadata) -> bool {
    let ours = meta.target().starts_with("ud") || meta.target().starts_with("ultradisplay");
    meta.level() <= if ours { log::Level::Debug } else { log::Level::Warn }
}

impl LogFile {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn write(&self, record: &log::Record) {
        if !wants(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}: {}\n",
            timestamp(),
            record.level(),
            record.target(),
            record.args()
        );
        let mut guard = self.file.lock().unwrap();
        let (file, written) = &mut *guard;
        if *written + line.len() as u64 > MAX_BYTES {
            let _ = fs::rename(&self.path, self.path.with_extension("log.1"));
            if let Ok(f) = OpenOptions::new().create(true).append(true).open(&self.path) {
                *file = f;
                *written = 0;
            }
        }
        if file.write_all(line.as_bytes()).is_ok() {
            *written += line.len() as u64;
        }
    }
}

/// Local time as `2026-10-13 15:48:02.123`.
fn timestamp() -> String {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
        libc::localtime_r(&ts.tv_sec, &mut tm);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ts.tv_nsec / 1_000_000
    )
}
