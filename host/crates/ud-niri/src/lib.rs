//! Minimal niri IPC client: JSON lines over `$NIRI_SOCKET`.
//!
//! Only what UltraDisplay needs: list outputs and manage one virtual output. The virtual
//! output requests come from niri PR #3800 (`CreateVirtualOutput`/`RemoveVirtualOutput`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Deserialize)]
pub struct Output {
    pub name: String,
    pub logical: Option<Logical>,
    pub current_mode: Option<usize>,
    pub modes: Vec<Mode>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Logical {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Mode {
    pub width: u16,
    pub height: u16,
    pub refresh_rate: u32, // millihertz
}

impl Output {
    pub fn mode(&self) -> Option<Mode> {
        self.modes.get(self.current_mode?).copied()
    }
}

pub struct Niri {
    socket: PathBuf,
}

impl Niri {
    /// Uses `$NIRI_SOCKET` unless `socket` is given.
    pub fn new(socket: Option<PathBuf>) -> anyhow::Result<Self> {
        let socket = match socket {
            Some(s) => s,
            None => std::env::var_os("NIRI_SOCKET")
                .map(PathBuf::from)
                .context("NIRI_SOCKET is not set; is niri running?")?,
        };
        Ok(Self { socket })
    }

    fn request(&self, req: Value) -> anyhow::Result<Value> {
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to niri at {}", self.socket.display()))?;
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        stream.write_all(&line)?;

        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        let reply: Value = serde_json::from_str(&reply).context("parsing niri reply")?;
        match reply {
            Value::Object(mut m) if m.contains_key("Ok") => Ok(m.remove("Ok").unwrap()),
            Value::Object(mut m) if m.contains_key("Err") => {
                bail!("niri: {}", m.remove("Err").unwrap())
            }
            other => bail!("unexpected niri reply: {other}"),
        }
    }

    pub fn outputs(&self) -> anyhow::Result<HashMap<String, Output>> {
        let mut resp = self.request(json!("Outputs"))?;
        let outputs = resp
            .get_mut("Outputs")
            .map(Value::take)
            .ok_or_else(|| anyhow!("no Outputs in reply"))?;
        Ok(serde_json::from_value(outputs)?)
    }

    pub fn create_virtual_output(
        &self,
        name: &str,
        width: u16,
        height: u16,
        refresh_hz: u32,
    ) -> anyhow::Result<String> {
        let resp = self
            .request(json!({ "CreateVirtualOutput": {
                "width": width, "height": height, "refresh_rate": refresh_hz, "name": name,
            }}))
            .context("CreateVirtualOutput (does this niri include virtual output support?)")?;
        resp.get("VirtualOutputCreated")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("unexpected CreateVirtualOutput reply: {resp}"))
    }

    pub fn remove_virtual_output(&self, name: &str) -> anyhow::Result<()> {
        self.request(json!({ "RemoveVirtualOutput": { "name": name } }))?;
        Ok(())
    }

    pub fn set_scale(&self, output: &str, scale: f64) -> anyhow::Result<()> {
        self.output_action(output, json!({ "Scale": { "scale": { "Specific": scale } } }))
    }

    pub fn set_position(&self, output: &str, x: i32, y: i32) -> anyhow::Result<()> {
        self.output_action(
            output,
            json!({ "Position": { "position": { "Specific": { "x": x, "y": y } } } }),
        )
    }

    /// Lets niri place the output automatically again.
    pub fn set_position_auto(&self, output: &str) -> anyhow::Result<()> {
        self.output_action(output, json!({ "Position": { "position": "Automatic" } }))
    }

    /// Changes an output's mode (works live on virtual outputs; active screencasts are
    /// renegotiated by niri).
    pub fn set_mode(&self, output: &str, width: u16, height: u16, refresh_hz: u32) -> anyhow::Result<()> {
        self.output_action(
            output,
            json!({ "CustomMode": { "mode": {
                "width": width, "height": height, "refresh": refresh_hz as f64,
            }}}),
        )
    }

    fn output_action(&self, output: &str, action: Value) -> anyhow::Result<()> {
        self.request(json!({ "Output": { "output": output, "action": action } }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_niri_outputs() {
        let json = r#"{
            "HDMI-A-1": {"name":"HDMI-A-1","make":"MSI","model":"x","serial":null,
              "physical_size":[600,340],"modes":[{"width":1920,"height":1080,"refresh_rate":119879,"is_preferred":false}],
              "current_mode":0,"is_custom_mode":false,"vrr_supported":false,"vrr_enabled":false,
              "logical":{"x":0,"y":0,"width":1920,"height":1080,"scale":1.0,"transform":"Normal"}},
            "UD-1": {"name":"UD-1","make":"niri","model":"UD-1","serial":null,"physical_size":null,
              "modes":[{"width":2800,"height":1752,"refresh_rate":120000,"is_preferred":true}],
              "current_mode":0,"is_custom_mode":true,"vrr_supported":false,"vrr_enabled":false,
              "logical":{"x":5000,"y":0,"width":1400,"height":876,"scale":2.0,"transform":"Normal"}}
        }"#;
        let outputs: HashMap<String, Output> = serde_json::from_str(json).unwrap();
        let ud = &outputs["UD-1"];
        assert_eq!(ud.mode().unwrap().refresh_rate, 120000);
        assert_eq!(ud.logical.unwrap().scale, 2.0);
        assert!(outputs["HDMI-A-1"].logical.is_some());
    }
}
