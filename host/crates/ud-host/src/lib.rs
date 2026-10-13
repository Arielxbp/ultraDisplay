//! UltraDisplay host runtime, shared by the headless daemon (`ultradisplayd`) and the GUI
//! app (`ultradisplay`).

mod adb;
mod bitrate;
pub mod config;
mod frame_slot;
mod host;
pub mod identity;
pub mod logfile;
mod mdns;
mod net;
mod placement;
pub mod service;
mod session;
mod tls;

pub use config::{Config, GuiPrefs, Placement};
pub use host::{Event, EventSink, Host, Transport};
pub use identity::PairedDevice;

/// Marks every inherited fd above stdio close-on-exec. The AppImage runtime passes its
/// mount directory open (fd 1023); an `adb` server started from here would inherit it and
/// keep the old AppImage mounted after we quit. Call first thing in `main`.
pub fn stop_fd_inheritance() {
    unsafe {
        libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC);
    }
}
