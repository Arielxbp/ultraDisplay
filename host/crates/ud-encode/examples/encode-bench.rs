//! Spike: encode synthetic 2800x1752 frames and report per-frame encode latency.
//!
//! Usage: encode-bench [FRAMES] [OUT.h265]   (defaults: 600, no file)

use std::io::Write;
use std::time::{Duration, Instant};

use ud_encode::{Codec, EncoderConfig, NvEncoder};

const W: u32 = 2800;
const H: u32 = 1752;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let frames: u64 = args.next().map_or(Ok(600), |s| s.parse())?;
    let mut out = args.next().map(std::fs::File::create).transpose()?;

    let mut enc = NvEncoder::new(EncoderConfig {
        codec: Codec::Hevc,
        width: W,
        height: H,
        fps: 120,
        bitrate_bps: 80_000_000,
    })?;

    let stride = W * 4;
    let mut frame = vec![0u8; (stride * H) as usize];
    let (mut total, mut worst) = (Duration::ZERO, Duration::ZERO);
    let mut bytes = 0usize;
    let mut keyframes = 0;

    let mut first_half_bytes = 0usize;
    for i in 0..frames {
        if i == frames / 2 {
            first_half_bytes = bytes;
            enc.set_bitrate(20_000_000)?; // exercise runtime reconfigure
        }
        // A moving diagonal gradient plus a sweeping bar: changes every pixel each frame.
        let shift = (i * 8) as u32;
        for y in 0..H {
            let row = &mut frame[(y * stride) as usize..][..stride as usize];
            for x in 0..W {
                let px = &mut row[(x * 4) as usize..][..4];
                let bar = ((x + shift) % 400) < 40;
                px[0] = ((x + y + shift) & 0xff) as u8;
                px[1] = if bar { 255 } else { (y & 0xff) as u8 };
                px[2] = ((x.wrapping_sub(shift)) & 0xff) as u8;
                px[3] = 255;
            }
        }

        let t = Instant::now();
        enc.encode(&frame, stride, i, |au| {
            bytes += au.data.len();
            keyframes += au.keyframe as u32;
            if let Some(f) = out.as_mut() {
                f.write_all(au.data).unwrap();
            }
        })?;
        let dt = t.elapsed();
        total += dt;
        worst = worst.max(dt);
    }

    let half_secs = (frames / 2) as f64 / 120.0;
    println!(
        "bitrate: first half {:.1} Mbit/s (80 target), second half {:.1} Mbit/s (20 target)",
        first_half_bytes as f64 * 8.0 / half_secs / 1e6,
        (bytes - first_half_bytes) as f64 * 8.0 / half_secs / 1e6,
    );
    let avg = total / frames as u32;
    println!(
        "{frames} frames: encode avg {:.2} ms, worst {:.2} ms (max sustainable {:.0} fps), \
         {:.1} Mbit/s at 120 fps, {keyframes} keyframe(s)",
        avg.as_secs_f64() * 1e3,
        worst.as_secs_f64() * 1e3,
        1.0 / avg.as_secs_f64(),
        bytes as f64 * 8.0 / (frames as f64 / 120.0) / 1e6,
    );
    Ok(())
}
