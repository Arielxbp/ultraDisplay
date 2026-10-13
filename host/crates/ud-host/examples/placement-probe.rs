//! Verification harness: change the tablet output's placement live while a client streams
//! over the USB port (run build/zctest/client.py against port 47863 alongside). Run it
//! only against the isolated test niri (scripts/test-niri.sh).
use std::sync::Arc;
use std::time::Duration;

use ud_host::{Config, Event, Host, Placement};

fn layout() -> String {
    let niri = ud_niri::Niri::new(None).unwrap();
    let mut outs: Vec<_> = niri.outputs().unwrap().into_values().collect();
    outs.sort_by(|a, b| a.name.cmp(&b.name));
    outs.iter()
        .filter_map(|o| o.logical.map(|l| format!("{} {}x{} at {},{}", o.name, l.width, l.height, l.x, l.y)))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let mut cfg = Config::default();
    (cfg.port, cfg.lan, cfg.adb, cfg.scale) = (47863, false, false, 1.5);
    cfg.placement = Placement::Below;
    let (tx, rx) = std::sync::mpsc::channel();
    let niri = std::env::var_os("NIRI_SOCKET").map(Into::into);
    let host = Host::new(cfg.clone(), niri, Arc::new(move |e| { let _ = tx.send(e); }))?;
    host.start()?;
    std::thread::sleep(Duration::from_secs(4)); // client connects
    println!("connected with Below @1.5: {}", layout());

    let steps: [(&str, Placement, Option<[i32; 2]>, f64); 7] = [
        ("Left", Placement::Left, None, 1.5),
        ("Right", Placement::Right, None, 1.5),
        ("Above", Placement::Above, None, 1.5),
        ("Above, scale 2.0", Placement::Above, None, 2.0),
        ("Manual 300,-900", Placement::Manual, Some([300, -900]), 2.0),
        ("Automatic", Placement::Auto, None, 2.0),
        ("Below", Placement::Below, None, 2.0),
    ];
    for (label, placement, position, scale) in steps {
        (cfg.placement, cfg.position, cfg.scale) = (placement, position, scale);
        host.update_config(cfg.clone());
        std::thread::sleep(Duration::from_millis(500));
        println!("{label:18}: {}", layout());
    }
    let reconnects = rx.try_iter().filter(|e| matches!(e, Event::Disconnected { .. })).count();
    println!("reconnects during the test: {reconnects}");
    host.stop();
    std::thread::sleep(Duration::from_millis(300));
    println!("after stop: {}", layout());
    Ok(())
}
