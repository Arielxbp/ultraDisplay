//! The host runtime: USB/Wi-Fi listeners, adb and mDNS helpers, and the session loop
//! (one tablet at a time). Embedders receive [`Event`]s and can change the [`Config`]
//! while it runs; most settings apply live, the rest via [`Host::restart`].

use std::io::Write;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;

use crate::config::{Config, Placement};
use crate::identity::{self, HostIdentity, PairedDevice, PairedStore};
use crate::session::{self, Conn};
use crate::{adb, mdns, tls};

pub use crate::session::Transport;

/// What the host reports to its embedder (GUI, logs).
#[derive(Debug, Clone)]
pub enum Event {
    /// Listeners (re)configured; `None` = that transport is off.
    Listening { usb_port: Option<u16>, lan_port: Option<u16> },
    Stopped,
    Connected { device: String, transport: Transport },
    Streaming { width: u32, height: u32, fps: u32, zero_copy: bool },
    /// About once a second while frames flow.
    /// `queue_ms`: video waiting in the network send queue (worst in the window); this is
    /// latency on top of capture/encode/decode.
    /// `net_rtt_ms`: the kernel's smoothed TCP round trip (network + the tablet's ACKs).
    HostStats {
        fps: f32,
        mbps: f32,
        target_mbps: u32,
        encode_ms: f32,
        skipped: u64,
        queue_ms: f32,
        net_rtt_ms: f32,
    },
    /// About twice a second, as reported by the tablet.
    ClientStats { fps: f32, decode_ms: f32, rtt_ms: f32 },
    BitrateChanged { target_mbps: u32, lowered: bool },
    /// The stream's frame rate changed mid-session (max fps setting).
    FpsChanged { fps: u32 },
    Disconnected { device: String, error: Option<String> },
    Rejected { reason: String },
    Paired { device: String },
    Error(String),
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// State shared with sessions (read live by the encoder and teardown code).
pub(crate) struct Shared {
    pub config: RwLock<Config>,
    pub niri_socket: Option<PathBuf>,
    pub events: EventSink,
    pub identity: HostIdentity,
    pub store: PairedStore,
    pub host_name: String,
    pub busy: AtomicBool,
    /// Socket and transport of the running session, to end it from outside.
    pub session: Mutex<Option<(TcpStream, Transport)>>,
    /// The streaming session's adjustable state (set once the virtual output exists).
    pub live: Mutex<Option<Arc<LiveSession>>>,
}

/// What can be changed on a running session.
pub(crate) struct LiveSession {
    pub output: String,
    pub width: u32,
    pub height: u32,
    /// Refresh rates the tablet's panel supports (Hz).
    pub refresh_rates: Vec<u32>,
    /// Current stream frame rate; the encoder follows it.
    pub fps: std::sync::atomic::AtomicU32,
}

/// Stream rate: the setting, capped by the panel's highest refresh rate (a 120 Hz panel
/// shows 90 fps content fine; tablets often report only their current mode).
pub(crate) fn pick_fps(refresh_rates: &[u32], max_fps: u32) -> u32 {
    let panel = refresh_rates.iter().copied().max().unwrap_or(60);
    max_fps.min(panel).max(1)
}

#[cfg(test)]
mod tests {
    use super::pick_fps;

    #[test]
    fn fps_is_setting_capped_by_panel() {
        assert_eq!(pick_fps(&[120], 120), 120);
        assert_eq!(pick_fps(&[120], 90), 90);
        assert_eq!(pick_fps(&[120, 60], 60), 60);
        assert_eq!(pick_fps(&[60], 120), 60);
        assert_eq!(pick_fps(&[], 120), 60);
    }
}

impl Shared {
    pub fn emit(&self, event: Event) {
        (self.events)(event);
    }
}

/// Everything that accepts connections; replaced as a unit when transports/ports change,
/// without touching a running session.
struct Listeners {
    stop: Arc<AtomicBool>,
    wake: Vec<SocketAddr>,
    threads: Vec<JoinHandle<()>>,
    adb: Option<adb::Watcher>,
    _mdns: Option<mdns::Advertisement>,
}

impl Listeners {
    fn spawn(cfg: &Config, shared: &Arc<Shared>, conns: &mpsc::SyncSender<Conn>) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut l = Self {
            stop: stop.clone(),
            wake: Vec::new(),
            threads: Vec::new(),
            adb: None,
            _mdns: None,
        };
        if cfg.usb {
            let usb = TcpListener::bind((Ipv4Addr::LOCALHOST, cfg.port))
                .with_context(|| format!("listening on 127.0.0.1:{} (USB)", cfg.port))?;
            l.wake.push(usb.local_addr()?);
            log::info!(
                "USB: listening on 127.0.0.1:{} (output {}, scale {}, up to {} Mbit/s)",
                cfg.port,
                cfg.output,
                cfg.scale,
                cfg.bitrate_mbps
            );
            l.threads.push(spawn_acceptor(usb, None, shared.clone(), stop.clone(), conns.clone()));
            l.adb = cfg.adb.then(|| adb::Watcher::spawn(cfg.port));
        } else {
            log::info!("USB: off");
        }
        if cfg.lan {
            let lan = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, cfg.lan_port)) {
                Ok(lan) => lan,
                Err(e) => {
                    l.stop();
                    return Err(anyhow::Error::new(e)
                        .context(format!("listening on 0.0.0.0:{} (Wi-Fi)", cfg.lan_port)));
                }
            };
            l.wake.push(SocketAddr::from((Ipv4Addr::LOCALHOST, cfg.lan_port)));
            log::info!(
                "Wi-Fi: listening on 0.0.0.0:{} (TLS, paired tablets only, up to {} Mbit/s)",
                cfg.lan_port,
                cfg.lan_bitrate_mbps
            );
            let tls = tls::server_config(&shared.identity)?;
            l.threads.push(spawn_acceptor(lan, Some(tls), shared.clone(), stop.clone(), conns.clone()));
            l._mdns = match mdns::advertise(shared.identity.id(), cfg.lan_port) {
                Ok(ad) => Some(ad),
                Err(err) => {
                    log::warn!("mDNS advertisement failed (tablets can still use the IP): {err:#}");
                    None
                }
            };
        } else {
            log::info!("Wi-Fi: off");
        }
        Ok(l)
    }

    fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept() calls so the acceptors see the stop flag.
        for addr in &self.wake {
            let _ = TcpStream::connect_timeout(addr, Duration::from_millis(200));
        }
        if let Some(adb) = self.adb {
            adb.stop();
        }
        for t in self.threads {
            let _ = t.join();
        }
    }
}

struct Runtime {
    stop: Arc<AtomicBool>,
    conns: mpsc::SyncSender<Conn>,
    sessions: JoinHandle<()>,
    listeners: Listeners,
}

pub struct Host {
    shared: Arc<Shared>,
    runtime: Mutex<Option<Runtime>>,
}

impl Host {
    pub fn new(config: Config, niri_socket: Option<PathBuf>, events: EventSink) -> anyhow::Result<Self> {
        let data_dir = identity::data_dir()?;
        let shared = Shared {
            config: RwLock::new(config),
            niri_socket,
            events,
            identity: HostIdentity::load_or_create(&data_dir)?,
            store: PairedStore::load(&data_dir)?,
            host_name: mdns::hostname(),
            busy: AtomicBool::new(false),
            session: Mutex::new(None),
            live: Mutex::new(None),
        };
        Ok(Self {
            shared: Arc::new(shared),
            runtime: Mutex::new(None),
        })
    }

    pub fn is_running(&self) -> bool {
        self.runtime.lock().unwrap().is_some()
    }

    /// Binds the listeners and starts accepting tablets.
    pub fn start(&self) -> anyhow::Result<()> {
        let mut rt = self.runtime.lock().unwrap();
        if rt.is_some() {
            return Ok(());
        }
        let cfg = self.shared.config.read().unwrap().clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (conns, incoming) = mpsc::sync_channel::<Conn>(0);
        let listeners = Listeners::spawn(&cfg, &self.shared, &conns)?;

        let shared = self.shared.clone();
        let session_stop = stop.clone();
        let sessions = thread::Builder::new()
            .name("sessions".into())
            .spawn(move || {
                // Ends when the runtime and all acceptors dropped their senders.
                for conn in incoming {
                    if session_stop.load(Ordering::SeqCst) {
                        continue; // accepted just before stop(); drop it
                    }
                    shared.busy.store(true, Ordering::SeqCst);
                    session::run(conn, shared.clone());
                    shared.busy.store(false, Ordering::SeqCst);
                }
            })?;

        self.emit_listening(&cfg);
        *rt = Some(Runtime {
            stop,
            conns,
            sessions,
            listeners,
        });
        Ok(())
    }

    /// Ends any session (removing the virtual output) and closes the listeners.
    pub fn stop(&self) {
        let Some(rt) = self.runtime.lock().unwrap().take() else {
            return;
        };
        rt.stop.store(true, Ordering::SeqCst);
        self.end_session(|_| true);
        rt.listeners.stop();
        drop(rt.conns);
        let _ = rt.sessions.join();
        log::info!("stopped listening");
        self.shared.emit(Event::Stopped);
    }

    pub fn restart(&self) -> anyhow::Result<()> {
        self.stop();
        self.start()
    }

    pub fn config(&self) -> Config {
        self.shared.config.read().unwrap().clone()
    }

    /// Applies `new` to the running host:
    /// - bitrate caps, scale, placement/position and keep-output apply live;
    /// - USB/Wi-Fi on/off, ports and adb setup swap the listeners immediately, ending a
    ///   session whose transport was turned off (others keep streaming);
    /// - max fps and zero-copy apply from the next connection.
    ///
    /// Returns true if a [`Host::restart`] is needed for the rest (the output name).
    pub fn update_config(&self, new: Config) -> bool {
        let old = std::mem::replace(&mut *self.shared.config.write().unwrap(), new.clone());

        let streaming = self.shared.session.lock().unwrap().is_some();
        let placement = new.effective_placement();
        let placement_changed = (old.effective_placement(), old.position) != (placement, new.position);
        // Presets depend on the output's logical size, so a scale change moves it too.
        let is_preset = !matches!(placement, Placement::Auto | Placement::Manual);
        let scale_changed = old.scale != new.scale;
        if streaming && (scale_changed || placement_changed) {
            match ud_niri::Niri::new(self.shared.niri_socket.clone()) {
                Ok(niri) => {
                    if scale_changed && let Err(err) = niri.set_scale(&new.output, new.scale) {
                        log::warn!("applying scale: {err:#}");
                    }
                    if (placement_changed || (scale_changed && is_preset))
                        && let Err(err) =
                            crate::placement::apply(&niri, &new.output, placement, new.position)
                    {
                        log::warn!("applying placement: {err:#}");
                    }
                }
                Err(err) => log::warn!("niri IPC: {err:#}"),
            }
        }

        if old.max_fps != new.max_fps {
            self.apply_max_fps(new.max_fps);
        }
        if old.listeners_differ(&new) {
            self.reconfigure_listeners(&old, &new);
        }
        old.restart_required(&new)
    }

    /// Switches a running stream to `max_fps` (capped by the panel): niri changes the
    /// virtual output's mode (renegotiating the cast) and the encoder follows.
    fn apply_max_fps(&self, max_fps: u32) {
        let Some(live) = self.shared.live.lock().unwrap().clone() else {
            return;
        };
        let fps = pick_fps(&live.refresh_rates, max_fps);
        if fps == live.fps.load(Ordering::Relaxed) {
            return;
        }
        let res = ud_niri::Niri::new(self.shared.niri_socket.clone()).and_then(|niri| {
            niri.set_mode(&live.output, live.width as u16, live.height as u16, fps)
        });
        match res {
            Ok(()) => {
                log::info!("frame rate -> {fps} Hz");
                live.fps.store(fps, Ordering::Relaxed);
                self.shared.emit(Event::FpsChanged { fps });
            }
            Err(err) => log::warn!("changing the frame rate: {err:#}"),
        }
    }

    fn reconfigure_listeners(&self, old: &Config, new: &Config) {
        // A session over a transport that was just turned off (or moved) ends now.
        self.end_session(|t| match t {
            Transport::Usb => !new.usb || new.port != old.port,
            Transport::Lan => !new.lan || new.lan_port != old.lan_port,
        });
        if old.usb && (!new.usb || new.port != old.port) {
            // Without this the tablet still reaches the old port through adb.
            adb::remove_reverse_all(old.port);
        }

        let mut guard = self.runtime.lock().unwrap();
        let Some(rt) = guard.as_mut() else { return };
        let old_listeners = std::mem::replace(
            &mut rt.listeners,
            Listeners {
                stop: Arc::new(AtomicBool::new(true)),
                wake: Vec::new(),
                threads: Vec::new(),
                adb: None,
                _mdns: None,
            },
        );
        old_listeners.stop();
        match Listeners::spawn(new, &self.shared, &rt.conns) {
            Ok(l) => rt.listeners = l,
            Err(err) => {
                log::error!("{err:#}");
                self.shared.emit(Event::Error(format!("{err:#}")));
            }
        }
        drop(guard);
        self.emit_listening(new);
    }

    /// Ends the running session if `which` returns true for its transport.
    fn end_session(&self, which: impl Fn(Transport) -> bool) {
        if let Some((sock, transport)) = self.shared.session.lock().unwrap().as_ref()
            && which(*transport)
        {
            log::info!("ending the {transport:?} session");
            let _ = sock.shutdown(Shutdown::Both);
        }
    }

    fn emit_listening(&self, cfg: &Config) {
        self.shared.emit(Event::Listening {
            usb_port: cfg.usb.then_some(cfg.port),
            lan_port: cfg.lan.then_some(cfg.lan_port),
        });
    }

    pub fn paired_devices(&self) -> Vec<PairedDevice> {
        self.shared.store.list()
    }

    pub fn forget_device(&self, client_id: &str) -> anyhow::Result<()> {
        self.shared.store.forget(client_id)
    }

    /// See [`crate::net::slow_start_after_idle`]: true means Wi-Fi will lag after pauses
    /// until `net.ipv4.tcp_slow_start_after_idle` is set to 0.
    pub fn tcp_idle_slowdown(&self) -> bool {
        crate::net::slow_start_after_idle()
    }

    pub fn host_name(&self) -> &str {
        &self.shared.host_name
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Accepts connections on `listener` (TLS-wrapping them when `tls` is set) and hands them
/// to the session loop, or turns them away while another tablet is streaming.
fn spawn_acceptor(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    conns: mpsc::SyncSender<Conn>,
) -> JoinHandle<()> {
    let name = if tls.is_some() { "accept-lan" } else { "accept-usb" };
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let stream = match stream {
                    Ok(s) => s,
                    Err(err) => {
                        log::warn!("accept failed: {err}");
                        continue;
                    }
                };
                let peer = stream.peer_addr().ok();
                let conn = match wrap(stream, tls.clone()) {
                    Ok(c) => c,
                    Err(err) => {
                        log::warn!("connection from {peer:?} failed: {err:#}");
                        continue;
                    }
                };
                if shared.busy.load(Ordering::SeqCst) {
                    log::info!("rejecting {peer:?}: another tablet is streaming");
                    let mut w = conn.writer;
                    let _ = ud_proto::write_parts(&mut w, ud_proto::MsgType::Bye, 0, &[b"busy"]);
                    let _ = w.flush();
                    continue;
                }
                log::info!("connection from {peer:?} over {:?}", conn.transport);
                if conns.send(conn).is_err() {
                    return;
                }
            }
        })
        .expect("spawning acceptor")
}

fn wrap(stream: TcpStream, tls: Option<Arc<rustls::ServerConfig>>) -> anyhow::Result<Conn> {
    let socket = stream.try_clone()?;
    Ok(match tls {
        None => Conn {
            transport: Transport::Usb,
            reader: Box::new(stream.try_clone()?),
            writer: Box::new(stream),
            socket,
        },
        Some(config) => {
            let (reader, writer) = tls::accept(config, stream)?;
            Conn {
                transport: Transport::Lan,
                reader: Box::new(reader),
                writer: Box::new(writer),
                socket,
            }
        }
    })
}
