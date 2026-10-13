//! Host TLS identity and the paired-tablet store, in `~/.local/share/ultradisplay/`.
//!
//! Pairing happens over USB (a trusted link): the host gives the tablet its certificate
//! fingerprint and a per-device token. Over Wi-Fi the tablet pins that fingerprint and the
//! host requires the token.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub fn data_dir() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(base.join("ultradisplay"))
}

pub struct HostIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    /// SHA-256 of `cert_der`, lowercase hex.
    pub fingerprint: String,
}

impl HostIdentity {
    /// Short id advertised over mDNS so tablets can recognize their paired host.
    pub fn id(&self) -> &str {
        &self.fingerprint[..16]
    }

    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        let cert_path = dir.join("host-cert.der");
        let key_path = dir.join("host-key.der");
        let (cert_der, key_der) = match (fs::read(&cert_path), fs::read(&key_path)) {
            (Ok(c), Ok(k)) => (c, k),
            _ => {
                let ck = rcgen::generate_simple_self_signed(vec!["ultradisplay".to_owned()])
                    .context("generating host certificate")?;
                let (c, k) = (ck.cert.der().to_vec(), ck.signing_key.serialize_der());
                fs::create_dir_all(dir)?;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
                write_private(&key_path, &k)?;
                fs::write(&cert_path, &c)?;
                log::info!("created host certificate in {}", dir.display());
                (c, k)
            }
        };
        let fingerprint = hex(&Sha256::digest(&cert_der));
        Ok(Self {
            cert_der,
            key_der,
            fingerprint,
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PairedFile {
    #[serde(default)]
    devices: BTreeMap<String, StoredDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDevice {
    name: String,
    token: String,
}

/// A tablet allowed to connect over Wi-Fi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub client_id: String,
    pub name: String,
}

/// Paired tablets, keyed by the app's per-install client id.
pub struct PairedStore {
    path: PathBuf,
    file: Mutex<PairedFile>,
}

impl PairedStore {
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("paired.toml");
        let file = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PairedFile::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn list(&self) -> Vec<PairedDevice> {
        self.file
            .lock()
            .unwrap()
            .devices
            .iter()
            .map(|(id, d)| PairedDevice {
                client_id: id.clone(),
                name: d.name.clone(),
            })
            .collect()
    }

    /// Revokes a tablet's Wi-Fi access (it can pair again over USB).
    pub fn forget(&self, client_id: &str) -> anyhow::Result<()> {
        let mut file = self.file.lock().unwrap();
        if file.devices.remove(client_id).is_some() {
            write_private(&self.path, toml::to_string(&*file)?.as_bytes())?;
            log::info!("forgot tablet {client_id}");
        }
        Ok(())
    }

    /// Whether `client_id` is already paired.
    pub fn is_paired(&self, client_id: &str) -> bool {
        self.file.lock().unwrap().devices.contains_key(client_id)
    }

    /// Token for `client_id`, creating (and saving) one on first pairing.
    pub fn token_for(&self, client_id: &str, name: &str) -> anyhow::Result<String> {
        let mut file = self.file.lock().unwrap();
        if let Some(dev) = file.devices.get(client_id) {
            return Ok(dev.token.clone());
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
        let token = hex(&raw);
        file.devices.insert(
            client_id.to_owned(),
            StoredDevice {
                name: name.to_owned(),
                token: token.clone(),
            },
        );
        let text = toml::to_string(&*file)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        write_private(&self.path, text.as_bytes())?;
        log::info!("paired new tablet {name} ({client_id})");
        Ok(token)
    }

    /// Whether `token` is the one issued to `client_id` (constant-time comparison).
    pub fn verify(&self, client_id: &str, token: &str) -> bool {
        let file = self.file.lock().unwrap();
        let Some(dev) = file.devices.get(client_id) else {
            return false;
        };
        let (a, b) = (dev.token.as_bytes(), token.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ud-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn identity_is_created_once_and_reloaded() {
        let dir = temp_dir("identity");
        let a = HostIdentity::load_or_create(&dir).unwrap();
        let b = HostIdentity::load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.fingerprint.len(), 64);
        let mode = fs::metadata(dir.join("host-key.der")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tokens_are_stable_and_verified() {
        let dir = temp_dir("paired");
        let store = PairedStore::load(&dir).unwrap();
        let t1 = store.token_for("client-a", "SM-T970").unwrap();
        assert_eq!(store.token_for("client-a", "SM-T970").unwrap(), t1);
        assert!(store.verify("client-a", &t1));
        assert!(!store.verify("client-a", "wrong"));
        assert!(!store.verify("client-b", &t1));
        assert_eq!(store.list().len(), 1);

        let reloaded = PairedStore::load(&dir).unwrap();
        assert!(reloaded.verify("client-a", &t1));
        reloaded.forget("client-a").unwrap();
        assert!(!PairedStore::load(&dir).unwrap().verify("client-a", &t1));
        fs::remove_dir_all(dir).unwrap();
    }
}
