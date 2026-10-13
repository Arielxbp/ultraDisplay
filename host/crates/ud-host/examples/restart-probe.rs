//! Verification harness: change max fps live while a client streams over the USB port
//! (run build/zctest/client.py against port 47862 alongside), with motion on UD-1.
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ud_host::{Config, Event, Host};

fn ud1_mode() -> String {
    let out = Command::new("niri-virtual").args(["msg", "outputs"]).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let mut lines = s.lines().skip_while(|l| !l.contains("(UD-1)"));
    lines.nth(1).unwrap_or("").trim().to_owned()
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let mut cfg = Config::default();
    (cfg.port, cfg.lan, cfg.adb) = (47862, false, false);
    let (tx, rx) = std::sync::mpsc::channel();
    let niri = std::env::var_os("NIRI_SOCKET").map(Into::into);
    let host = Host::new(cfg.clone(), niri, Arc::new(move |e| { let _ = tx.send(e); }))?;
    host.start()?;
    std::thread::sleep(Duration::from_secs(4)); // client connects, vkcube starts on UD-1
    while rx.try_recv().is_ok() {}
    let mut reconnects = 0;
    let mut measure = |secs: u64| {
        let (mut sum, mut n) = (0.0, 0);
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::HostStats { fps, .. }) => { sum += fps; n += 1; }
                Ok(Event::Disconnected { .. }) => reconnects += 1,
                Ok(Event::FpsChanged { fps }) => println!("  event: FpsChanged {{ fps: {fps} }}"),
                _ => {}
            }
        }
        if n > 0 { sum / n as f32 } else { 0.0 }
    };
    println!("streaming at default (120): {:.0} fps sent, {}", measure(4), ud1_mode());
    for max in [60, 90, 120] {
        cfg.max_fps = max;
        host.update_config(cfg.clone());
        let _ = measure(1); // settle
        println!("max_fps={max}: {:.0} fps sent, {}", measure(3), ud1_mode());
    }
    println!("reconnects during the test: {reconnects}");
    host.stop();
    Ok(())
}
