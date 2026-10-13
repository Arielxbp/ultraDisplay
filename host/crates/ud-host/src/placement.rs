//! Turns a [`Placement`] into a logical position for the tablet output, relative to the
//! monitor at (0, 0).

use std::collections::HashMap;

use anyhow::Context;
use ud_niri::{Logical, Niri, Output};

use crate::config::Placement;

/// Positions the output `name` in niri according to `placement`. Call after the scale is
/// set, since presets depend on the output's logical size.
pub fn apply(niri: &Niri, name: &str, placement: Placement, position: Option<[i32; 2]>) -> anyhow::Result<()> {
    let pos = match (placement, position) {
        (Placement::Auto, _) | (Placement::Manual, None) => return niri.set_position_auto(name),
        (Placement::Manual, Some([x, y])) => (x, y),
        (preset, _) => preset_position(&niri.outputs()?, name, preset)?,
    };
    niri.set_position(name, pos.0, pos.1)?;
    log::info!("{name} placed at {},{} ({placement:?})", pos.0, pos.1);
    Ok(())
}

fn preset_position(outputs: &HashMap<String, Output>, name: &str, preset: Placement) -> anyhow::Result<(i32, i32)> {
    let out = outputs.get(name).with_context(|| format!("{name} is not a niri output"))?;
    let mut own = out.logical.with_context(|| format!("{name} has no logical size yet"))?;
    // niri reports the logical size rounded down but checks overlaps with it rounded up
    // (2800 / 1.5 = 1866.7: reported 1866, occupies 1867), so size from the mode.
    if let Some(mode) = out.mode() {
        own.width = (f64::from(mode.width) / own.scale).ceil() as u32;
        own.height = (f64::from(mode.height) / own.scale).ceil() as u32;
    }
    let anchor = anchor(outputs, name).context("no other enabled monitor to place the tablet next to")?;
    Ok(beside(anchor, own, preset))
}

/// The monitor at (0, 0); without one, the top-left-most other output.
fn anchor<'a>(outputs: &'a HashMap<String, Output>, except: &str) -> Option<Logical> {
    let others = outputs.values().filter(|o| o.name != except).filter_map(|o| o.logical);
    others
        .clone()
        .find(|l| (l.x, l.y) == (0, 0))
        .or_else(|| others.min_by_key(|l| (l.y, l.x)))
}

fn beside(anchor: Logical, own: Logical, preset: Placement) -> (i32, i32) {
    match preset {
        Placement::Left => (anchor.x - own.width as i32, anchor.y),
        Placement::Right => (anchor.x + anchor.width as i32, anchor.y),
        Placement::Above => (anchor.x, anchor.y - own.height as i32),
        Placement::Below => (anchor.x, anchor.y + anchor.height as i32),
        Placement::Auto | Placement::Manual => unreachable!("not a preset"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logical(x: i32, y: i32, width: u32, height: u32) -> Logical {
        Logical { x, y, width, height, scale: 1.0 }
    }

    #[test]
    fn presets_are_relative_to_the_origin_monitor() {
        let hdmi = logical(0, 0, 1920, 1080);
        let tablet = logical(5000, 0, 1867, 1168); // 2800x1752 at scale 1.5, rounded up
        assert_eq!(beside(hdmi, tablet, Placement::Left), (-1867, 0));
        assert_eq!(beside(hdmi, tablet, Placement::Right), (1920, 0));
        assert_eq!(beside(hdmi, tablet, Placement::Above), (0, -1168));
        assert_eq!(beside(hdmi, tablet, Placement::Below), (0, 1080));
    }

    #[test]
    fn anchor_prefers_origin_and_skips_the_tablet() {
        let out = |name: &str, l: Logical| Output {
            name: name.into(),
            logical: Some(l),
            current_mode: None,
            modes: vec![],
        };
        let mut outputs = HashMap::from([
            ("UD-1".into(), out("UD-1", logical(-1400, -500, 1400, 876))),
            ("DP-1".into(), out("DP-1", logical(1920, -200, 2560, 1440))),
            ("HDMI-A-1".into(), out("HDMI-A-1", logical(0, 0, 1920, 1080))),
        ]);
        assert_eq!(anchor(&outputs, "UD-1").unwrap().width, 1920);
        outputs.remove("HDMI-A-1");
        assert_eq!(anchor(&outputs, "UD-1").unwrap().width, 2560);
    }
}
