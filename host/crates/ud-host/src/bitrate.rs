//! Adaptive bitrate: additive-increase / multiplicative-decrease on send congestion.
//!
//! The signal is the encoder thread's own experience: time spent blocked writing frames
//! to the socket, and captured frames skipped because it was busy. Both rise as soon as
//! the link (USB or Wi-Fi) can't keep up, before latency builds anywhere else.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub frames: u32,
    pub skipped: u32,
    pub write_time: Duration,
}

pub struct Controller {
    min: u32,
    max: u32,
    current: u32,
    good_windows: u32,
}

impl Controller {
    pub fn new(max_bps: u32) -> Self {
        Self {
            min: Self::min_for(max_bps),
            max: max_bps,
            current: max_bps,
            good_windows: 0,
        }
    }

    fn min_for(max_bps: u32) -> u32 {
        (max_bps / 8).max(5_000_000).min(max_bps)
    }

    pub fn max(&self) -> u32 {
        self.max
    }

    /// Changes the cap (e.g. from the GUI). Raising it lets the bitrate climb back up
    /// gradually; lowering it below the current bitrate applies immediately. Returns the
    /// new bitrate if it changed.
    pub fn set_max(&mut self, max_bps: u32) -> Option<u32> {
        if max_bps == self.max {
            return None;
        }
        let was_at_max = self.current == self.max;
        self.max = max_bps;
        self.min = Self::min_for(max_bps);
        let next = if was_at_max { max_bps } else { self.current.clamp(self.min, self.max) };
        (next != self.current).then(|| {
            self.current = next;
            next
        })
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// Feeds one measurement window (about a second). Returns the new bitrate if it changed.
    pub fn update(&mut self, w: Window) -> Option<u32> {
        // Damage-driven capture: an idle desktop says nothing about the link.
        if w.frames < 10 {
            return None;
        }
        let avg_write = w.write_time / w.frames;
        let skip_ratio = w.skipped as f32 / (w.frames + w.skipped) as f32;
        let congested = avg_write > Duration::from_millis(4) || skip_ratio > 0.05;

        let next = if congested {
            self.good_windows = 0;
            (self.current as f32 * 0.75) as u32
        } else {
            self.good_windows += 1;
            if self.good_windows < 3 {
                return None;
            }
            self.good_windows = 0;
            self.current + self.max / 10
        }
        .clamp(self.min, self.max);

        (next != self.current).then(|| {
            self.current = next;
            next
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(frames: u32, skipped: u32, write_ms: u64) -> Window {
        Window {
            frames,
            skipped,
            write_time: Duration::from_millis(write_ms),
        }
    }

    #[test]
    fn backs_off_on_congestion_and_recovers_slowly() {
        let mut c = Controller::new(40_000_000);
        assert_eq!(c.update(window(120, 0, 120)), None); // healthy at max
        assert_eq!(c.update(window(120, 0, 1200)), Some(30_000_000)); // 10 ms/frame
        assert_eq!(c.update(window(100, 20, 100)), Some(22_500_000)); // skipping frames
        assert_eq!(c.update(window(120, 0, 100)), None);
        assert_eq!(c.update(window(120, 0, 100)), None);
        assert_eq!(c.update(window(120, 0, 100)), Some(26_500_000)); // +10% of max
    }

    #[test]
    fn cap_changes_apply() {
        let mut c = Controller::new(40_000_000);
        assert_eq!(c.set_max(60_000_000), Some(60_000_000)); // was at cap: follow it up
        assert_eq!(c.update(window(120, 0, 1200)), Some(45_000_000));
        assert_eq!(c.set_max(30_000_000), Some(30_000_000)); // lowering clamps now
        assert_eq!(c.set_max(30_000_000), None);
    }

    #[test]
    fn ignores_idle_windows_and_respects_bounds() {
        let mut c = Controller::new(40_000_000);
        assert_eq!(c.update(window(2, 0, 500)), None);
        for _ in 0..20 {
            c.update(window(120, 0, 2000));
        }
        assert_eq!(c.current(), 5_000_000);
    }
}
