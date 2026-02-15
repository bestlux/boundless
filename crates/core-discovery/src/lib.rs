use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

pub const MDNS_SERVICE_TYPE: &str = "_boundless._tcp.local.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryAnnouncement {
    pub machine_id: String,
    pub display_name: String,
    pub endpoint: SocketAddr,
}

pub fn mdns_instance_name(machine_id: &str, display_name: &str) -> String {
    format!("boundless-{display_name}-{machine_id}")
}

pub fn parse_manual_target(input: &str, default_port: u16) -> Option<SocketAddr> {
    if let Ok(addr) = input.parse::<SocketAddr>() {
        return Some(addr);
    }

    if let Ok(ip) = input.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, default_port));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ip_without_port() {
        let parsed = parse_manual_target("127.0.0.1", 15100).expect("must parse");
        assert_eq!(parsed.port(), 15100);
    }
}
