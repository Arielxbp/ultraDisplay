//! Spike: cast a niri output and report the delivered frame rate.
//!
//! Usage: cast-probe [OUTPUT] [SECONDS]   (defaults: UD-1, 10)

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use ud_capture::{CursorMode, MonitorCast, StreamOptions, stream};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let output = args.next().unwrap_or_else(|| "UD-1".to_owned());
    let seconds: u64 = args.next().map_or(Ok(10), |s| s.parse())?;

    let cast = MonitorCast::start(&output, CursorMode::Embedded)?;
    let opts = StreamOptions {
        width: 2800,
        height: 1752,
        max_fps: 120,
        dmabuf_modifiers: Vec::new(), // SHM: this probe reads pixels on the CPU
    };

    let start = Instant::now();
    let mut window_start = start;
    let mut window_frames = 0u32;
    let mut total = 0u64;
    let mut worst_gap = Duration::ZERO;
    let mut last: Option<Instant> = None;

    let (_stop, stop_signal) = ud_capture::stop_channel();
    stream::run(cast.node_id(), &opts, stop_signal, move |frame| {
        let now = frame.received;
        if let Some(prev) = last {
            worst_gap = worst_gap.max(now - prev);
        }
        last = Some(now);
        window_frames += 1;
        total += 1;

        if now - window_start >= Duration::from_secs(1) {
            let fps = window_frames as f64 / (now - window_start).as_secs_f64();
            println!(
                "{:>5.1} fps  {}x{} stride {} {:?}  worst gap {:.2} ms",
                fps,
                frame.width,
                frame.height,
                frame.stride,
                frame.format,
                worst_gap.as_secs_f64() * 1e3
            );
            window_start = now;
            window_frames = 0;
            worst_gap = Duration::ZERO;
        }

        if now - start >= Duration::from_secs(seconds) {
            let avg = total as f64 / (now - start).as_secs_f64();
            println!("total {total} frames, average {avg:.1} fps");
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })?;

    drop(cast);
    Ok(())
}
