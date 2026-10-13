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
            pending: Vec::new(),
        },
        TlsWriter { conn, tcp },
    ))
}

pub struct TlsReader {
    conn: Arc<Mutex<ServerConnection>>,
    tcp: TcpStream,
    /// Ciphertext read from the socket that rustls hasn't taken yet: `read_tls` accepts
    /// only ~4 KB per call and refuses more while 16 KB of plaintext is unread.
    pending: Vec<u8>,
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
                if !self.pending.is_empty() {
                    // One chunk at a time, draining plaintext in between (loop top).
                    let mut rest = &self.pending[..];
                    conn.read_tls(&mut rest)?;
                    let used = self.pending.len() - rest.len();
                    self.pending.drain(..used);
                    conn.process_new_packets().map_err(io::Error::other)?;
                    while conn.wants_write() {
                        conn.write_tls(&mut self.tcp)?;
                    }
                    continue;
                }
            }
            // Wait for more ciphertext without holding the lock, so the writer keeps going.
            let n = self.tcp.read(&mut raw)?;
            if n == 0 {
                return Ok(0);
            }
            self.pending.extend_from_slice(&raw[..n]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use std::net::TcpListener;

    #[derive(Debug)]
    struct AcceptAny;

    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// A burst larger than rustls takes per `read_tls` call (and than its plaintext
    /// buffer) must arrive intact; dropping the excess broke decryption on Wi-Fi.
    #[test]
    fn large_client_burst_arrives_intact() {
        let dir = std::env::temp_dir().join(format!("ud-test-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let identity = HostIdentity::load_or_create(&dir).unwrap();
        let config = server_config(&identity).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let data: Vec<u8> = (0..256 * 1024u32).map(|i| (i * 31 % 251) as u8).collect();
        let sent = data.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let client = std::thread::spawn(move || {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let cfg = rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAny))
                .with_no_client_auth();
            let name = ServerName::try_from("ultradisplay").unwrap();
            let conn = rustls::ClientConnection::new(Arc::new(cfg), name).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, TcpStream::connect(addr).unwrap());
            tls.write_all(&sent).unwrap();
            tls.flush().unwrap();
            tls.conn.send_close_notify();
            tls.flush().unwrap();
            // Closing with the server's session ticket unread would reset the connection.
            let _ = done_rx.recv();
        });

        let (tcp, _) = listener.accept().unwrap();
        let (mut reader, _writer) = accept(config, tcp).unwrap();
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        done_tx.send(()).unwrap();
        client.join().unwrap();
        assert_eq!(got.len(), data.len());
        assert!(got == data);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
