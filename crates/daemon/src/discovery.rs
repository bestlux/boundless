use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, SocketAddrV6},
};

use anyhow::{Context, Result};
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info, warn};

use core_discovery::{DiscoveryAnnouncement, MDNS_SERVICE_TYPE, mdns_instance_name};

use crate::state::AppState;

pub fn start(state: AppState) {
    tokio::spawn(async move {
        if let Err(error) = run(state).await {
            warn!(error = ?error, "mDNS discovery runtime stopped");
        }
    });
}

async fn run(state: AppState) -> Result<()> {
    let snapshot = state.snapshot().await;
    let local_machine_id = snapshot.machine_id.clone();
    let local_display_name = snapshot.device_name.clone();
    let local_network_port = snapshot.network_port;

    let mdns = ServiceDaemon::new().context("start mDNS daemon")?;
    let service = build_local_service_info(
        &snapshot.machine_id,
        &snapshot.device_name,
        snapshot.network_port,
    )?;
    mdns.register(service)
        .context("register local mDNS service")?;
    let receiver = mdns
        .browse(MDNS_SERVICE_TYPE)
        .context("start mDNS browse")?;

    info!(
        machine_id = %local_machine_id,
        display_name = %local_display_name,
        network_port = local_network_port,
        service_type = MDNS_SERVICE_TYPE,
        "mDNS discovery started"
    );

    let mut fullname_to_machine_id: HashMap<String, String> = HashMap::new();

    loop {
        let event = match receiver.recv_async().await {
            Ok(event) => event,
            Err(error) => {
                warn!(error = ?error, "mDNS discovery receiver closed");
                break;
            }
        };

        match event {
            ServiceEvent::ServiceResolved(resolved) => {
                let Some(announcement) = announcement_from_resolved(&resolved) else {
                    continue;
                };

                if announcement.machine_id == local_machine_id {
                    continue;
                }

                fullname_to_machine_id.insert(
                    resolved.get_fullname().to_string(),
                    announcement.machine_id.clone(),
                );

                let previous = state
                    .set_discovered_endpoint(&announcement.machine_id, announcement.endpoint)
                    .await;

                if previous != Some(announcement.endpoint) {
                    info!(
                        machine_id = %announcement.machine_id,
                        display_name = %announcement.display_name,
                        endpoint = %announcement.endpoint,
                        "mDNS discovered peer endpoint"
                    );
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                if let Some(machine_id) = fullname_to_machine_id.remove(&fullname)
                    && state.clear_discovered_endpoint(&machine_id).await.is_some()
                {
                    info!(
                        machine_id = %machine_id,
                        fullname = %fullname,
                        "mDNS removed peer endpoint"
                    );
                }
            }
            ServiceEvent::SearchStarted(service_type) => {
                debug!(%service_type, "mDNS browse started");
            }
            ServiceEvent::SearchStopped(service_type) => {
                warn!(%service_type, "mDNS browse stopped");
            }
            _ => {}
        }
    }

    let _ = mdns.shutdown();
    Ok(())
}

fn build_local_service_info(
    machine_id: &str,
    display_name: &str,
    network_port: u16,
) -> Result<ServiceInfo> {
    let instance_name = mdns_instance_name(machine_id, display_name);
    let host_name = format!("boundless-{machine_id}.local.");
    let properties = [("machine_id", machine_id), ("display_name", display_name)];

    Ok(ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        &instance_name,
        &host_name,
        (),
        network_port,
        &properties[..],
    )
    .context("build local mDNS service info")?
    .enable_addr_auto())
}

fn announcement_from_resolved(resolved: &ResolvedService) -> Option<DiscoveryAnnouncement> {
    let machine_id = resolved
        .get_property_val_str("machine_id")?
        .trim()
        .to_string();
    if machine_id.is_empty() {
        return None;
    }

    let display_name = resolved
        .get_property_val_str("display_name")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(machine_id.as_str())
        .to_string();

    let endpoint = preferred_endpoint_addr(resolved.get_addresses().iter(), resolved.get_port())?;

    Some(DiscoveryAnnouncement {
        machine_id,
        display_name,
        endpoint,
    })
}

fn preferred_endpoint_addr<'a, I>(addresses: I, port: u16) -> Option<SocketAddr>
where
    I: Iterator<Item = &'a ScopedIp>,
{
    let mut ipv4: Option<SocketAddr> = None;
    let mut ipv6: Option<SocketAddr> = None;

    for address in addresses {
        if address.is_loopback() {
            continue;
        }

        match address {
            ScopedIp::V4(v4) if ipv4.is_none() => {
                ipv4 = Some(SocketAddr::new(IpAddr::V4(*v4.addr()), port));
            }
            ScopedIp::V6(v6) if ipv6.is_none() => {
                let socket = SocketAddrV6::new(*v6.addr(), port, 0, v6.scope_id().index);
                ipv6 = Some(SocketAddr::V6(socket));
            }
            _ => {}
        }
    }

    ipv4.or(ipv6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_endpoint_prefers_ipv4() {
        let addresses: Vec<ScopedIp> = vec![
            "fe80::1".parse::<IpAddr>().expect("ipv6").into(),
            "10.0.0.9".parse::<IpAddr>().expect("ipv4").into(),
        ];
        let endpoint = preferred_endpoint_addr(addresses.iter(), 15100).expect("endpoint");
        assert_eq!(
            endpoint,
            "10.0.0.9:15100".parse::<SocketAddr>().expect("parse")
        )
    }

    #[test]
    fn preferred_endpoint_ignores_loopback() {
        let addresses: Vec<ScopedIp> = vec![
            "127.0.0.1".parse::<IpAddr>().expect("loopback").into(),
            "10.0.0.4".parse::<IpAddr>().expect("ipv4").into(),
        ];
        let endpoint = preferred_endpoint_addr(addresses.iter(), 15100).expect("endpoint");
        assert_eq!(
            endpoint,
            "10.0.0.4:15100".parse::<SocketAddr>().expect("parse")
        )
    }

    #[test]
    fn preferred_endpoint_uses_ipv6_when_ipv4_missing() {
        let addresses: Vec<ScopedIp> = vec!["fe80::1".parse::<IpAddr>().expect("ipv6").into()];
        let endpoint = preferred_endpoint_addr(addresses.iter(), 15100).expect("endpoint");

        match endpoint {
            SocketAddr::V6(v6) => {
                assert_eq!(
                    v6.ip(),
                    &"fe80::1".parse::<std::net::Ipv6Addr>().expect("ipv6")
                );
                assert_eq!(v6.port(), 15100);
            }
            SocketAddr::V4(_) => panic!("expected IPv6 endpoint"),
        }
    }
}
