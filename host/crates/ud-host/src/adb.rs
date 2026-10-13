//! USB transport: follow adb device events and set up `adb reverse` on every authorized
//! tablet, so the app's connection to 127.0.0.1:PORT reaches us. The user starts the app.

use std::collections::HashSet;
use std::io::{BufReader, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;

/// Background thread following `adb track-devices`; stopped with [`Watcher::stop`].
pub struct Watcher {
    state: Arc<Mutex<State>>,
    thread: JoinHandle<()>,
}

/// Stop flag and current child under one lock, so stop() can't miss a child that is
/// being spawned at the same moment (it would then block the thread's read forever).
#[derive(Default)]
struct State {
    stopped: bool,
    child: Option<Child>,
}

impl Watcher {
    pub fn spawn(port: u16) -> Self {
        let state: Arc<Mutex<State>> = Arc::default();
        let thread = {
            let state = state.clone();
            thread::Builder::new()
                .name("adb-watch".into())
                .spawn(move || {
                    let stopped = || state.lock().unwrap().stopped;
                    while !stopped() {
                        if let Err(err) = track(port, &state) {
                            log::debug!("adb track-devices: {err:#}");
                        }
                        // adb server restarted or adb is missing: retry.
                        for _ in 0..20 {
                            if stopped() {
                                return;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                })
                .expect("spawning adb watcher")
        };
        Self { state, thread }
    }

    pub fn stop(self) {
        {
            let mut st = self.state.lock().unwrap();
            st.stopped = true;
            if let Some(child) = st.child.as_mut() {
                let _ = child.kill();
            }
        }
        let _ = self.thread.join();
    }
}

/// Runs `adb track-devices`, which prints the full device list on every change, until it
/// exits (or is killed by [`Watcher::stop`]).
fn track(port: u16, state: &Mutex<State>) -> anyhow::Result<()> {
    let mut cmd = Command::new("adb");
    cmd.arg("track-devices").stdout(Stdio::piped()).stderr(Stdio::null());
    // Die with us even if we're killed without cleanup (no orphaned track-devices).
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut out = {
        let mut st = state.lock().unwrap();
        if st.stopped {
            return Ok(());
        }
        let mut child = cmd.spawn().context("running adb (is android-tools installed?)")?;
        let out = BufReader::new(child.stdout.take().unwrap());
        st.child = Some(child);
        out
    };

    let mut known = HashSet::new();
    let result = loop {
        let list = match read_update(&mut out) {
            Ok(list) => list,
            Err(err) => break Err(err),
        };
        let now = parse_devices(&list);
        for serial in now.difference(&known) {
            match reverse(serial, port) {
                Ok(()) => log::info!("adb device {serial}: reverse tcp:{port} ready"),
                Err(err) => log::warn!("adb reverse for {serial} failed: {err:#}"),
            }
        }
        for serial in known.difference(&now) {
            log::info!("adb device {serial} disconnected");
        }
        known = now;
    };
    if let Some(mut child) = state.lock().unwrap().child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

/// Removes our `adb reverse` rule from every connected device (best effort, in the
/// background), so a tablet can't keep reaching a port we no longer serve.
pub fn remove_reverse_all(port: u16) {
    thread::spawn(move || {
        let Ok(out) = Command::new("adb").arg("devices").output() else {
            return;
        };
        let spec = format!("tcp:{port}");
        for serial in parse_devices(&String::from_utf8_lossy(&out.stdout)) {
            let removed = Command::new("adb")
                .args(["-s", &serial, "reverse", "--remove", &spec])
                .output()
                .is_ok_and(|o| o.status.success());
            if removed {
                log::info!("adb device {serial}: removed reverse {spec}");
            }
        }
    });
}

/// One update: 4 hex digits of length, then that many bytes of `serial\tstate\n` lines.
fn read_update(r: &mut impl Read) -> anyhow::Result<String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).context("adb track-devices ended")?;
    let len = usize::from_str_radix(std::str::from_utf8(&len)?, 16)?;
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Serials in the `device` (authorized, online) state.
fn parse_devices(list: &str) -> HashSet<String> {
    list.lines()
        .filter_map(|l| {
            let (serial, state) = l.split_once('\t')?;
            (state.trim() == "device").then(|| serial.to_owned())
        })
        .collect()
}

fn reverse(serial: &str, port: u16) -> anyhow::Result<()> {
    let spec = format!("tcp:{port}");
    let out = Command::new("adb")
        .args(["-s", serial, "reverse", &spec, &spec])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_track_devices_updates() {
        let mut stream: &[u8] = b"0026R52R5029CET\tdevice\nEMU-1\tunauthorized\n0000";
        let first = read_update(&mut stream).unwrap();
        assert_eq!(parse_devices(&first), HashSet::from(["R52R5029CET".to_owned()]));
        let second = read_update(&mut stream).unwrap();
        assert!(parse_devices(&second).is_empty());
    }
}
