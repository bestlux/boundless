use std::{collections::HashMap, net::SocketAddr};
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct DiscoveredPeerEndpoint {
    pub display_name: String,
    pub endpoint: SocketAddr,
}

#[derive(Debug, Default)]
pub(super) struct DiscoveryState {
    pub(super) endpoints: RwLock<HashMap<String, DiscoveredPeerEndpoint>>,
    pub(super) mdns_active: RwLock<bool>,
}

impl DiscoveryState {
    pub(super) async fn clear(&self) {
        self.endpoints.write().await.clear();
        *self.mdns_active.write().await = false;
    }
}
