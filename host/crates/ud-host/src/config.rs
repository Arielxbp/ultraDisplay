//! `~/.config/ultradisplay/config.toml`. Every key is optional; CLI flags override it.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// TCP port the tablet connects to (through `adb reverse` over USB).
    pub port: u16,
    /// Name of the niri virtual output created for the tablet.
    pub output: String,
    /// Output scale (2800x1752 at 2.0 = 1400x876 logical, at 1.5 = 1867x1168).
    pub scale: f64,
    /// Where the tablet output goes; presets are relative to the monitor at (0, 0).
    pub placement: Placement,
    /// Logical [x, y] position, used when `placement` is `manual`.
    pub position: Option<[i32; 2]>,
    /// Maximum USB video bitrate in Mbit/s (adapted down on congestion).
    pub bitrate_mbps: u32,
    /// Upper bound for the stream frame rate.
    pub max_fps: u32,
    /// Keep the virtual output when the tablet disconnects.
    pub keep_output: bool,
    /// Accept the tablet over the USB cable (127.0.0.1 via `adb reverse`).
    pub usb: bool,
    /// Set up `adb reverse` automatically for USB-connected tablets.
    pub adb: bool,
    /// Accept paired tablets over Wi-Fi (TLS) and advertise the host via mDNS.
    pub lan: bool,
    /// TCP port of the Wi-Fi (TLS) listener.
    pub lan_port: u16,
    /// Maximum Wi-Fi video bitrate in Mbit/s (adapted down on congestion).
    pub lan_bitrate_mbps: u32,
    /// Keep frames on the GPU (DMA-BUF capture). Falls back to SHM automatically.
    pub zero_copy: bool,
    /// Seconds between the periodic statistics lines in the log (0 = never).
    pub stats_log_secs: u32,
    /// Preferences of the GUI app (ignored by the headless daemon).
    pub gui: GuiPrefs,
}

/// Placement of the tablet output in niri's layout.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    /// niri places it right of the other outputs.
    #[default]
    Auto,
    /// Left of the monitor at (0, 0), top edges aligned.
    Left,
    /// Right of the monitor at (0, 0), top edges aligned.
    Right,
    /// Above the monitor at (0, 0), left edges aligned.
    Above,
    /// Below the monitor at (0, 0), left edges aligned.
    Below,
    /// At `position`.
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GuiPrefs {
    /// Notify when a tablet connects or disconnects.
    pub notify_connection: bool,
    /// Notify about errors (pairing rejected, capture/encoder failures).
    pub notify_errors: bool,
    /// Notify when the bitrate is lowered because the link can't keep up.
    pub notify_bitrate: bool,
}

impl Default for GuiPrefs {
    fn default() -> Self {
        Self {
            notify_connection: true,
            notify_errors: true,
            notify_bitrate: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: ud_proto::DEFAULT_PORT,
            output: "UD-1".into(),
            scale: 2.0,
            placement: Placement::Auto,
            position: None,
            bitrate_mbps: 80,
            max_fps: 120,
            keep_output: false,
            usb: true,
            adb: true,
            lan: true,
            lan_port: ud_proto::DEFAULT_LAN_PORT,
            lan_bitrate_mbps: 40,
            zero_copy: true,
            stats_log_secs: 5,
            gui: GuiPrefs::default(),
        }
    }
}

impl Config {
    /// Whether going from `self` to `new` needs [`crate::Host::restart`] (everything else
    /// applies live).
    pub fn restart_required(&self, new: &Config) -> bool {
        self.output != new.output
    }

    /// The placement in effect. Files from before `placement` existed only set `position`,
    /// which meant manual; `manual` without a position falls back to automatic.
    pub fn effective_placement(&self) -> Placement {
        match (self.placement, self.position) {
            (Placement::Auto, Some(_)) => Placement::Manual,
            (Placement::Manual, None) => Placement::Auto,
            (p, _) => p,
        }
    }

    /// Whether the listeners (transports, ports, adb setup) must be swapped.
    pub fn listeners_differ(&self, new: &Config) -> bool {
        self.usb != new.usb
            || self.adb != new.adb
            || self.lan != new.lan
            || self.port != new.port
            || self.lan_port != new.lan_port
    }

    /// Writes the config (atomically). Comments in a hand-edited file are not kept.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = format!(
            "# UltraDisplay settings (written by the app; see config.example.toml for docs)\n\n{}",
            toml::to_string_pretty(self)?
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("ultradisplay/config.toml"))
    }

    /// Loads `path`, or defaults if it doesn't exist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_defaults() {
        let cfg: Config = toml::from_str("scale = 1.5\nposition = [1920, 0]\n").unwrap();
        assert_eq!(cfg.scale, 1.5);
        assert_eq!(cfg.position, Some([1920, 0]));
        assert_eq!(cfg.output, "UD-1");
        assert_eq!(cfg.bitrate_mbps, 80);
    }

    #[test]
    fn shipped_example_is_valid_and_matches_defaults() {
        let text = include_str!("../../../../packaging/ultradisplay/config.example.toml");
        assert_eq!(toml::from_str::<Config>(text).unwrap(), Config::default());
    }

    #[test]
    fn save_round_trips() {
        let dir = std::env::temp_dir().join(format!("ud-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.lan_bitrate_mbps = 65;
        cfg.position = Some([1920, 0]);
        cfg.gui.notify_bitrate = true;
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        cfg.position = None;
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_output_name_needs_a_restart() {
        let a = Config::default();
        let mut b = a.clone();
        b.lan_bitrate_mbps = 80;
        b.scale = 1.5;
        b.keep_output = true;
        assert!(!a.restart_required(&b) && !a.listeners_differ(&b));
        b.usb = false;
        b.lan_port = 50000;
        assert!(!a.restart_required(&b) && a.listeners_differ(&b));
        b.output = "UD-2".into();
        assert!(a.restart_required(&b));
    }

    #[test]
    fn placement_parses_and_old_position_means_manual() {
        let cfg: Config = toml::from_str("placement = \"below\"\n").unwrap();
        assert_eq!(cfg.effective_placement(), Placement::Below);
        let old: Config = toml::from_str("position = [0, 1080]\n").unwrap();
        assert_eq!(old.effective_placement(), Placement::Manual);
        let manual: Config = toml::from_str("placement = \"manual\"\n").unwrap();
        assert_eq!(manual.effective_placement(), Placement::Auto);
        assert!(toml::from_str::<Config>("placement = \"up\"").is_err());
    }

    #[test]
    fn rejects_typos() {
        assert!(toml::from_str::<Config>("scal = 1.5").is_err());
    }

    #[test]
    fn missing_file_is_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/ultradisplay.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }
}
