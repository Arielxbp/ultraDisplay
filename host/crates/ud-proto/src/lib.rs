//! UltraDisplay wire protocol v1. The spec lives in `docs/protocol.md`.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 47800;
/// TLS listener for Wi-Fi clients.
pub const DEFAULT_LAN_PORT: u16 = 47801;
/// mDNS service type advertised for Wi-Fi discovery.
pub const MDNS_SERVICE: &str = "_ultradisplay._tcp.local.";
pub const MAGIC: [u8; 2] = *b"UD";
pub const HEADER_LEN: usize = 8;
pub const MAX_PAYLOAD: u32 = 16 << 20;

pub const FLAG_KEYFRAME: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    Hello = 0x01,
    Config = 0x02,
    Video = 0x03,
    Stats = 0x04,
    RequestIdr = 0x05,
    Ping = 0x06,
    Pong = 0x07,
    Bye = 0x08,
    Pairing = 0x09,
}

impl TryFrom<u8> for MsgType {
    type Error = Error;

    fn try_from(v: u8) -> Result<Self, Error> {
        Ok(match v {
            0x01 => Self::Hello,
            0x02 => Self::Config,
            0x03 => Self::Video,
            0x04 => Self::Stats,
            0x05 => Self::RequestIdr,
            0x06 => Self::Ping,
            0x07 => Self::Pong,
            0x08 => Self::Bye,
            0x09 => Self::Pairing,
            other => return Err(Error::UnknownType(other)),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("bad magic {0:02x?}")]
    BadMagic([u8; 2]),
    #[error("unknown message type {0:#04x}")]
    UnknownType(u8),
    #[error("payload too large: {0} bytes")]
    TooLarge(u32),
    #[error("invalid JSON payload: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unexpected message {0:?}")]
    Unexpected(MsgType),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub protocol: u32,
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rates: Vec<f32>,
    pub codecs: Vec<String>,
    /// Stable per-install id of the tablet app (pairing key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Token from a previous PAIRING; required over Wi-Fi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// Sent by the host over USB (a trusted link) so the tablet can later connect over Wi-Fi.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pairing {
    /// Short host id, also in the mDNS TXT record `id`.
    pub host_id: String,
    pub host_name: String,
    /// SHA-256 of the host's TLS certificate (DER), lowercase hex.
    pub fingerprint: String,
    pub lan_port: u16,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamConfig {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClientStats {
    pub fps: f32,
    pub decode_ms: f32,
    pub dropped: u32,
    #[serde(default)]
    pub queued: u32,
    #[serde(default)]
    pub rtt_ms: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub ty: MsgType,
    pub flags: u8,
    pub len: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0; HEADER_LEN];
        b[..2].copy_from_slice(&MAGIC);
        b[2] = self.ty as u8;
        b[3] = self.flags;
        b[4..].copy_from_slice(&self.len.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Self, Error> {
        let magic = [b[0], b[1]];
        if magic != MAGIC {
            return Err(Error::BadMagic(magic));
        }
        let len = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
        if len > MAX_PAYLOAD {
            return Err(Error::TooLarge(len));
        }
        Ok(Self {
            ty: MsgType::try_from(b[2])?,
            flags: b[3],
            len,
        })
    }
}

/// A decoded message with its raw payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub ty: MsgType,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, Error> {
        Ok(serde_json::from_slice(&self.payload)?)
    }

    pub fn u64(&self) -> Option<u64> {
        Some(u64::from_be_bytes(self.payload.get(..8)?.try_into().ok()?))
    }
}

pub fn read_message(r: &mut impl Read) -> Result<Message, Error> {
    let mut hb = [0; HEADER_LEN];
    r.read_exact(&mut hb)?;
    let h = Header::decode(&hb)?;
    let mut payload = vec![0; h.len as usize];
    r.read_exact(&mut payload)?;
    Ok(Message {
        ty: h.ty,
        flags: h.flags,
        payload,
    })
}

/// Writes one message from several payload parts (avoids concatenating large frames).
pub fn write_parts(w: &mut impl Write, ty: MsgType, flags: u8, parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let len = u32::try_from(len)
        .ok()
        .filter(|l| *l <= MAX_PAYLOAD)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload too large"))?;
    w.write_all(&Header { ty, flags, len }.encode())?;
    for p in parts {
        w.write_all(p)?;
    }
    Ok(())
}

pub fn write_json(w: &mut impl Write, ty: MsgType, body: &impl Serialize) -> io::Result<()> {
    let json = serde_json::to_vec(body).map_err(io::Error::other)?;
    write_parts(w, ty, 0, &[&json])
}

pub fn write_video(w: &mut impl Write, pts_us: u64, keyframe: bool, au: &[u8]) -> io::Result<()> {
    let flags = if keyframe { FLAG_KEYFRAME } else { 0 };
    write_parts(w, MsgType::Video, flags, &[&pts_us.to_be_bytes(), au])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_round_trip() {
        let mut buf = Vec::new();
        write_video(&mut buf, 0x0102030405060708, true, &[0, 0, 0, 1, 0x40]).unwrap();
        assert_eq!(&buf[..4], &[b'U', b'D', 0x03, FLAG_KEYFRAME]);

        let msg = read_message(&mut buf.as_slice()).unwrap();
        assert_eq!(msg.ty, MsgType::Video);
        assert_eq!(msg.flags, FLAG_KEYFRAME);
        assert_eq!(msg.u64(), Some(0x0102030405060708));
        assert_eq!(&msg.payload[8..], &[0, 0, 0, 1, 0x40]);
    }

    #[test]
    fn json_round_trip() {
        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            device: "SM-T970".into(),
            width: 2800,
            height: 1752,
            refresh_rates: vec![120.0, 60.0],
            codecs: vec!["hevc".into()],
            client_id: Some("abc".into()),
            token: None,
        };
        let mut buf = Vec::new();
        write_json(&mut buf, MsgType::Hello, &hello).unwrap();
        let msg = read_message(&mut buf.as_slice()).unwrap();
        assert_eq!(msg.json::<Hello>().unwrap(), hello);
    }

    #[test]
    fn old_hello_without_pairing_fields_parses() {
        let hello: Hello = serde_json::from_str(
            r#"{"protocol":1,"device":"x","width":1,"height":1,"refresh_rates":[],"codecs":[]}"#,
        )
        .unwrap();
        assert_eq!(hello.client_id, None);
        assert_eq!(hello.token, None);
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(matches!(
            Header::decode(&[b'X', b'D', 1, 0, 0, 0, 0, 0]),
            Err(Error::BadMagic(_))
        ));
        assert!(matches!(
            Header::decode(&[b'U', b'D', 0x7f, 0, 0, 0, 0, 0]),
            Err(Error::UnknownType(0x7f))
        ));
        assert!(matches!(
            Header::decode(&[b'U', b'D', 1, 0, 0xff, 0, 0, 0]),
            Err(Error::TooLarge(_))
        ));
    }
}
