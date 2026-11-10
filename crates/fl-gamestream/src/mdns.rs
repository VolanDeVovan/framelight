//! `_nvstream._tcp` advertisement. mdns-sd binds 5353 with address/port reuse, so it
//! coexists with avahi-daemon on the same machine. It follows interface changes by
//! itself (`enable_addr_auto`), which is what brings the host back in clients' lists
//! after the headset's Wi-Fi returns from sleep.

use anyhow::{Context, Result};
use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use std::time::Duration;

const SERVICE: &str = "_nvstream._tcp.local.";

pub struct Advert {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for Advert {
    fn drop(&mut self) {
        // Both calls only queue a command: wait for each, or the daemon is gone before
        // the goodbye packets are out and clients keep a stale entry for over an hour.
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
        if let Ok(done) = self.daemon.shutdown() {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
    }
}

pub fn advertise(name: &str, port: u16) -> Result<Advert> {
    let daemon = ServiceDaemon::new().context("start mDNS daemon")?;
    // The host listens on IPv4 only; an AAAA record would send clients to a dead end.
    daemon.disable_interface(IfKind::IPv6).context("mDNS: IPv4 only")?;
    let host = format!("{}.local.", host_label(name));
    let info = ServiceInfo::new(SERVICE, name, &host, "", port, None::<std::collections::HashMap<String, String>>)
        .context("mDNS service info")?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info).context("register mDNS service")?;
    tracing::info!(service = %fullname, "mDNS: advertising");
    Ok(Advert { daemon, fullname })
}

/// A DNS-safe host label derived from the display name.
fn host_label(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let mut s = s.trim_matches('-').to_string();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    if s.is_empty() { "framelight".into() } else { format!("{s}-gs") }
}
