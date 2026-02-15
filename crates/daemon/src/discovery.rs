use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

use anyhow::{Context, Result};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
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

    let endpoint_ip =
        preferred_endpoint_ip(resolved.get_addresses().iter().map(|ip| ip.to_ip_addr()))?;
    let endpoint = SocketAddr::new(endpoint_ip, resolved.get_port());

    Some(DiscoveryAnnouncement {
        machine_id,
        display_name,
        endpoint,
    })
}

fn preferred_endpoint_ip<I>(addresses: I) -> Option<IpAddr>
where
    I: Iterator<Item = IpAddr>,
{
    let mut ipv4: Option<IpAddr> = None;
    let mut ipv6: Option<IpAddr> = None;

    for address in addresses {
        if address.is_loopback() {
            continue;
        }

        if address.is_ipv4() && ipv4.is_none() {
            ipv4 = Some(address);
        } else if address.is_ipv6() && ipv6.is_none() {
            ipv6 = Some(address);
        }
    }

    ipv4.or(ipv6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_endpoint_prefers_ipv4() {
        let ip = preferred_endpoint_ip(
            [
                "fe80::1".parse().expect("ipv6"),
                "10.0.0.9".parse().expect("ipv4"),
            ]
            .into_iter(),
        )
        .expect("ip");
        assert_eq!(ip, "10.0.0.9".parse::<IpAddr>().expect("parse"));
    }

    #[test]
    fn preferred_endpoint_ignores_loopback() {
        let ip = preferred_endpoint_ip(
            [
                "127.0.0.1".parse().expect("loopback"),
                "10.0.0.4".parse().expect("ipv4"),
            ]
            .into_iter(),
        )
        .expect("ip");
        assert_eq!(ip, "10.0.0.4".parse::<IpAddr>().expect("parse"));
    }
}
