//! Client for the `org.gnome.Mutter.ScreenCast` D-Bus API, which niri implements.
//!
//! Talking to it directly (instead of going through xdg-desktop-portal) avoids the
//! interactive output picker and hands us a PipeWire node id for a named output.

use std::collections::HashMap;

use anyhow::{Context, bail};
use zbus::blocking::Connection;
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, Value};

const SERVICE: &str = "org.gnome.Mutter.ScreenCast";

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
trait ScreenCast {
    fn create_session(&self, properties: HashMap<&str, Value<'_>>)
    -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Session",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait Session {
    fn record_monitor(
        &self,
        connector: &str,
        properties: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast.Stream",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait Stream {
    #[zbus(signal)]
    fn pipe_wire_stream_added(&self, node_id: u32) -> zbus::Result<()>;
}

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum CursorMode {
    Hidden = 0,
    Embedded = 1,
    Metadata = 2,
}

/// A running screencast of one output. The cast stops when this is dropped.
pub struct MonitorCast {
    // Kept alive: niri tears the session down when the owning connection goes away.
    _conn: Connection,
    session: SessionProxyBlocking<'static>,
    node_id: u32,
}

impl MonitorCast {
    /// Starts casting `connector` (e.g. `"UD-1"`) and waits for its PipeWire node.
    pub fn start(connector: &str, cursor_mode: CursorMode) -> anyhow::Result<Self> {
        let conn = Connection::session().context("connecting to the D-Bus session bus")?;

        let screen_cast = ScreenCastProxyBlocking::new(&conn)
            .context("niri's ScreenCast D-Bus service is not available")?;
        let session_path = screen_cast
            .create_session(HashMap::new())
            .context("CreateSession")?;
        let session = SessionProxyBlocking::builder(&conn)
            .destination(SERVICE)?
            .path(session_path)?
            .build()?;

        let props = HashMap::from([("cursor-mode", Value::U32(cursor_mode as u32))]);
        let stream_path = session
            .record_monitor(connector, props)
            .with_context(|| format!("RecordMonitor({connector})"))?;
        let stream = StreamProxyBlocking::builder(&conn)
            .destination(SERVICE)?
            .path(stream_path)?
            .build()?;

        // Subscribe before Start so the signal can't be missed.
        let mut added = stream.receive_pipe_wire_stream_added()?;
        session.start().context("Session.Start")?;

        let Some(signal) = added.next() else {
            bail!("D-Bus connection closed before PipeWireStreamAdded");
        };
        let node_id = signal.args()?.node_id;
        log::info!("casting {connector} as PipeWire node {node_id}");

        Ok(Self {
            _conn: conn,
            session,
            node_id,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }
}

impl Drop for MonitorCast {
    fn drop(&mut self) {
        if let Err(err) = self.session.stop() {
            log::warn!("error stopping screencast session: {err}");
        }
    }
}
