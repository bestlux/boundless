use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, SocketAddrV6},
};

use anyhow::{Context, Result};
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info, warn};

use core_discovery::{MDNS_SERVICE_TYPE, mdns_instance_name};

use crate::{
    runtime_tasks::{RuntimeTaskOwner, RuntimeTaskShutdown, RuntimeTaskSpec},
    state::AppState,
};

pub fn start(state: AppState) {
    let task_state = state.clone();
    state.spawn_runtime_task(
        RuntimeTaskSpec::new(
            "discovery.mdns",
            RuntimeTaskOwner::Discovery,
            RuntimeTaskShutdown::AbortOnDaemonShutdown,
        ),
        async move {
            if let Err(error) = run(task_state).await {
                warn!(error = ?error, "mDNS discovery runtime stopped");
            }
        },
    );
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
    state.set_mdns_active(true).await;

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
                    .set_discovered_endpoints(
                        &announcement.machine_id,
                        &announcement.display_name,
                        announcement.endpoint_candidates.clone(),
                    )
                    .await;

                if previous
                    .as_ref()
                    .map(|item| item.endpoint_candidates.as_slice())
                    != Some(announcement.endpoint_candidates.as_slice())
                {
                    info!(
                        machine_id = %announcement.machine_id,
                        display_name = %announcement.display_name,
                        endpoint = %announcement.endpoint(),
                        endpoint_candidates = ?announcement.endpoint_candidates,
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
    state.set_mdns_active(false).await;
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

#[derive(Debug, Clone)]
struct ResolvedDiscoveryAnnouncement {
    machine_id: String,
    display_name: String,
    endpoint_candidates: Vec<SocketAddr>,
}

impl ResolvedDiscoveryAnnouncement {
    fn endpoint(&self) -> SocketAddr {
        self.endpoint_candidates[0]
    }
}

fn announcement_from_resolved(resolved: &ResolvedService) -> Option<ResolvedDiscoveryAnnouncement> {
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

    let endpoint_candidates =
        endpoint_candidates_from_addrs(resolved.get_addresses().iter(), resolved.get_port());
    if endpoint_candidates.is_empty() {
        return None;
    }

    Some(ResolvedDiscoveryAnnouncement {
        machine_id,
        display_name,
        endpoint_candidates,
    })
}

fn endpoint_candidates_from_addrs<'a, I>(addresses: I, port: u16) -> Vec<SocketAddr>
where
    I: Iterator<Item = &'a ScopedIp>,
{
    let mut ipv4 = Vec::<SocketAddr>::new();
    let mut ipv6 = Vec::<SocketAddr>::new();

    for address in addresses {
        if address.is_loopback() {
            continue;
        }

        match address {
            ScopedIp::V4(v4) => {
                let endpoint = SocketAddr::new(IpAddr::V4(*v4.addr()), port);
                if !ipv4.contains(&endpoint) {
                    ipv4.push(endpoint);
                }
            }
            ScopedIp::V6(v6) => {
                let socket = SocketAddrV6::new(*v6.addr(), port, 0, v6.scope_id().index);
                let endpoint = SocketAddr::V6(socket);
                if !ipv6.contains(&endpoint) {
                    ipv6.push(endpoint);
                }
            }
            _ => {}
        }
    }

    interleave_endpoint_families(ipv6, ipv4)
}

fn interleave_endpoint_families(
    first_family: Vec<SocketAddr>,
    second_family: Vec<SocketAddr>,
) -> Vec<SocketAddr> {
    let mut candidates = Vec::with_capacity(first_family.len() + second_family.len());
    let max_len = first_family.len().max(second_family.len());
    for index in 0..max_len {
        if let Some(endpoint) = first_family.get(index) {
            candidates.push(*endpoint);
        }
        if let Some(endpoint) = second_family.get(index)
            && !candidates.contains(endpoint)
        {
            candidates.push(*endpoint);
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_candidates_interleave_ipv6_then_ipv4() {
        let addresses: Vec<ScopedIp> = vec![
            "fe80::1".parse::<IpAddr>().expect("ipv6").into(),
            "10.0.0.9".parse::<IpAddr>().expect("ipv4").into(),
            "fe80::2".parse::<IpAddr>().expect("ipv6").into(),
            "10.0.0.10".parse::<IpAddr>().expect("ipv4").into(),
        ];
        let endpoints = endpoint_candidates_from_addrs(addresses.iter(), 15100);
        assert_eq!(
            endpoints,
            vec![
                "[fe80::1]:15100".parse::<SocketAddr>().expect("parse"),
                "10.0.0.9:15100".parse::<SocketAddr>().expect("parse"),
                "[fe80::2]:15100".parse::<SocketAddr>().expect("parse"),
                "10.0.0.10:15100".parse::<SocketAddr>().expect("parse"),
            ]
        )
    }

    #[test]
    fn preferred_endpoint_ignores_loopback() {
        let addresses: Vec<ScopedIp> = vec![
            "127.0.0.1".parse::<IpAddr>().expect("loopback").into(),
            "10.0.0.4".parse::<IpAddr>().expect("ipv4").into(),
        ];
        let endpoints = endpoint_candidates_from_addrs(addresses.iter(), 15100);
        assert_eq!(
            endpoints,
            vec!["10.0.0.4:15100".parse::<SocketAddr>().expect("parse")]
        )
    }

    #[test]
    fn endpoint_candidates_use_ipv6_when_ipv4_missing() {
        let addresses: Vec<ScopedIp> = vec!["fe80::1".parse::<IpAddr>().expect("ipv6").into()];
        let endpoints = endpoint_candidates_from_addrs(addresses.iter(), 15100);

        match endpoints[0] {
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
