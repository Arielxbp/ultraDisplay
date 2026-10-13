//! Advertise the Wi-Fi listener as `_ultradisplay._tcp` so paired tablets find the host.

use anyhow::Context;
use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};

/// Keeps the advertisement alive; dropping it unregisters.
pub struct Advertisement {
    daemon: ServiceDaemon,
}

pub fn advertise(host_id: &str, port: u16) -> anyhow::Result<Advertisement> {
    let host = hostname();
    let daemon = ServiceDaemon::new().context("starting mDNS responder")?;
    // Don't advertise container/VM bridge addresses the tablet can't reach.
    let virtual_ifs: Vec<IfKind> = virtual_interfaces().into_iter().map(IfKind::Name).collect();
    if !virtual_ifs.is_empty() {
        log::debug!("mDNS: skipping virtual interfaces {virtual_ifs:?}");
        daemon.disable_interface(virtual_ifs)?;
    }
    let props = [("id", host_id), ("proto", "1")];
    let info = ServiceInfo::new(
        ud_proto::MDNS_SERVICE,
        &host,
        &format!("{host}.local."),
        "",
        port,
        &props[..],
    )?
    .enable_addr_auto();
    daemon.register(info).context("registering mDNS service")?;
    log::info!("advertising {host} as {} on port {port}", ud_proto::MDNS_SERVICE);
    Ok(Advertisement { daemon })
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// Interfaces created by Docker, libvirt, VPNs and the like.
fn virtual_interfaces() -> Vec<String> {
    const PREFIXES: &[&str] = &["docker", "br-", "veth", "virbr", "vnet", "tun", "tap", "wg", "tailscale", "zt"];
    std::fs::read_dir("/sys/class/net")
        .map(|dir| {
            dir.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| PREFIXES.iter().any(|p| n.starts_with(p)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ultradisplay-host".to_owned())
}
