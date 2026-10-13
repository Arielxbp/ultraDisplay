//! UltraDisplay host runtime, shared by the headless daemon (`ultradisplayd`) and the GUI
//! app (`ultradisplay`).

mod adb;
mod bitrate;
pub mod config;
mod frame_slot;
mod host;
pub mod identity;
mod mdns;
mod net;
mod placement;
pub mod service;
mod session;
mod tls;

pub use config::{Config, GuiPrefs, Placement};
pub use host::{Event, EventSink, Host, Transport};
pub use identity::PairedDevice;
