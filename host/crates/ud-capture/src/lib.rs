//! Output capture for UltraDisplay: niri ScreenCast (D-Bus) + PipeWire frames.

pub mod screencast;
pub mod stream;

pub use screencast::{CursorMode, MonitorCast};
pub use stream::{Frame, FrameData, PixelFormat, StopHandle, StreamOptions, stop_channel};
