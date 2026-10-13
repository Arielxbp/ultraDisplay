//! Blocking TLS server stream split into independent read and write halves, so the
//! session's reader thread (control messages) and encoder thread (video) don't block each
//! other the way a single `rustls::StreamOwned` would.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use rustls::ServerConnection;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::identity::HostIdentity;

pub fn server_config(identity: &HostIdentity) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(identity.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone())),
        )
        .context("loading host certificate")?;
    Ok(Arc::new(config))
}

/// Completes the TLS handshake on `tcp` (with a timeout) and returns the two halves.
pub fn accept(
    config: Arc<rustls::ServerConfig>,
    mut tcp: TcpStream,
) -> anyhow::Result<(TlsReader, TlsWriter)> {
    let mut conn = ServerConnection::new(config)?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp).context("TLS handshake")?;
    }
    tcp.set_read_timeout(None)?;

    let conn = Arc::new(Mutex::new(conn));
    Ok((
        TlsReader {
            conn: conn.clone(),
            tcp: tcp.try_clone()?,
        },
        TlsWriter { conn, tcp },
    ))
}

pub struct TlsReader {
    conn: Arc<Mutex<ServerConnection>>,
    tcp: TcpStream,
}

impl Read for TlsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut raw = [0u8; 16 * 1024];
        loop {
            {
                let mut conn = self.conn.lock().unwrap();
                match conn.reader().read(buf) {
                    Ok(n) => return Ok(n), // 0 = clean close_notify
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            // Wait for more ciphertext without holding the lock, so the writer keeps going.
            let n = self.tcp.read(&mut raw)?;
            if n == 0 {
                return Ok(0);
            }
            let mut conn = self.conn.lock().unwrap();
            conn.read_tls(&mut &raw[..n])?;
            conn.process_new_packets().map_err(io::Error::other)?;
            while conn.wants_write() {
                conn.write_tls(&mut self.tcp)?;
            }
        }
    }
}

pub struct TlsWriter {
    conn: Arc<Mutex<ServerConnection>>,
    tcp: TcpStream,
}

impl Write for TlsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let n = conn.writer().write(buf)?;
        while conn.wants_write() {
            conn.write_tls(&mut self.tcp)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        conn.writer().flush()?;
        while conn.wants_write() {
            conn.write_tls(&mut self.tcp)?;
        }
        Ok(())
    }
}
