//! mDNS advertising (spec §6.1): the Timer announces itself as `_oft._tcp` so clients can
//! find it without being told an address.

use crate::timer::Timer;
use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// DNS-SD service type, as `mdns-sd` and most browsers spell it.
pub const SERVICE_TYPE: &str = "_oft._tcp.local.";

/// How often the TXT record is checked against the Timer's state.
const REFRESH: Duration = Duration::from_secs(2);

/// A running advertisement. Call [`Advertiser::shutdown`] to say goodbye on the network.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
    refresh: tokio::task::JoinHandle<()>,
}

/// DNS label made from a Timer id: ASCII letters, digits and `-`.
fn host_label(id: &str) -> String {
    let s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let s = s.trim_matches('-');
    if s.is_empty() { "oft".into() } else { s.chars().take(63).collect() }
}

fn service_info(timer: &Timer, addr: SocketAddr, txt: &[(&'static str, String)]) -> mdns_sd::Result<ServiceInfo> {
    let host = format!("{}.local.", host_label(&timer.cfg.id));
    if addr.ip().is_unspecified() {
        // Listening on every interface: let the daemon fill in and track the host's addresses.
        Ok(ServiceInfo::new(SERVICE_TYPE, &timer.cfg.id, &host, (), addr.port(), txt)?.enable_addr_auto())
    } else {
        ServiceInfo::new(SERVICE_TYPE, &timer.cfg.id, &host, addr.ip(), addr.port(), txt)
    }
}

/// Advertise `timer`, which is serving on `addr` (the bound address). The instance name is
/// the Timer id. The TXT record follows the Timer: it is re-announced when seats or Nodes
/// change. Must be called inside a tokio runtime.
pub fn advertise(timer: Arc<Timer>, addr: SocketAddr) -> mdns_sd::Result<Advertiser> {
    let daemon = ServiceDaemon::new()?;
    if addr.is_ipv4() {
        // An IPv4 listener is not reachable on IPv6 addresses; do not advertise them.
        daemon.disable_interface(vec![IfKind::IPv6, IfKind::LoopbackV6])?;
    }
    let mut txt = timer.discovery_txt();
    let info = service_info(&timer, addr, &txt)?;
    let fullname = info.get_fullname().to_string();
    daemon.register(info)?;

    let d = daemon.clone();
    let refresh = tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH);
        loop {
            tick.tick().await;
            let now = timer.discovery_txt();
            if now != txt {
                match service_info(&timer, addr, &now).and_then(|i| d.register(i)) {
                    Ok(()) => txt = now,
                    Err(e) => eprintln!("oftd: mDNS update failed: {e}"),
                }
            }
        }
    });
    Ok(Advertiser { daemon, fullname, refresh })
}

impl Advertiser {
    /// Full instance name, e.g. `oft-3fa2._oft._tcp.local.`.
    pub fn fullname(&self) -> &str {
        &self.fullname
    }

    /// Withdraw the service (sends mDNS goodbye) and stop the daemon. Blocks briefly.
    pub fn shutdown(self) {
        self.refresh.abort();
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
        if let Ok(rx) = self.daemon.shutdown() {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::host_label;

    #[test]
    fn host_label_is_a_dns_label() {
        assert_eq!(host_label("oft-3fa2"), "oft-3fa2");
        assert_eq!(host_label("Gate 1 (start)"), "gate-1--start");
        assert_eq!(host_label("***"), "oft");
    }
}
