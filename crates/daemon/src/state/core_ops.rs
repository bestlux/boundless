use super::*;

impl AppState {
    pub fn load_or_create() -> Result<Self> {
        let config_path = config_path();
        let security_root = default_security_root();
        Self::load_or_create_with_paths(config_path, security_root)
    }

    pub fn load_or_create_with_paths(config_path: PathBuf, security_root: PathBuf) -> Result<Self> {
        let config = load_or_create_config_at(&config_path)?;

        let paths = SecurityPaths::for_root(security_root);
        let secret = load_or_create_device_secret(&paths)?;
        ensure_trust_store(&paths)?;
        let advertised_host = std::env::var("BOUNDLESS_ADVERTISE_HOST").ok();
        let identity = ensure_device_identity(
            &paths,
            &config.machine_id,
            &config.device_name,
            advertised_host.as_deref(),
        )?;

        // Ensure self trust record exists. This enables symmetric mTLS setups and local test loops.
        upsert_trust_record(
            &paths,
            TrustRecord {
                machine_id: config.machine_id.clone(),
                ca_cert_pem: identity.ca_cert_pem.clone(),
                added_at: Utc::now(),
            },
        )?;

        let inbox_root = if let Ok(path) = std::env::var("BOUNDLESS_INBOX_ROOT") {
            PathBuf::from(path)
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Boundless")
                .join("inbox")
        };
        std::fs::create_dir_all(&inbox_root)?;

        let fingerprint = fingerprint(&secret);

        info!(
            machine_id = %config.machine_id,
            config_path = %config_path.display(),
            security_root = %paths.root.display(),
            inbox_root = %inbox_root.display(),
            "state loaded"
        );

        let input_enabled = config.features.get("share_input").copied().unwrap_or(true);

        Ok(Self {
            config_path: Arc::new(config_path),
            config: Arc::new(RwLock::new(config)),
            clipboard: Arc::new(ClipboardState::default()),
            pairing: Arc::new(PairingState::default()),
            transport: Arc::new(TransportState::default()),
            discovery: Arc::new(DiscoveryState::default()),
            input: Arc::new(InputState::new(input_enabled)),
            security_paths: Arc::new(paths),
            identity: Arc::new(identity),
            device_fingerprint: Arc::new(fingerprint),
            inbox_root: Arc::new(inbox_root),
            parsed_layout_matrix_cache: Arc::new(RwLock::new(None)),
            input_capture_wake: Arc::new(RuntimeWakeSignal::default()),
            input_inject_wake: Arc::new(RuntimeWakeSignal::default()),
        })
    }

    pub fn fingerprint(&self) -> &str {
        self.device_fingerprint.as_ref().as_str()
    }

    pub fn identity(&self) -> &DeviceIdentity {
        self.identity.as_ref()
    }

    pub async fn trusted_records(&self) -> Result<Vec<TrustRecord>> {
        load_trust_records(&self.security_paths)
    }

    pub async fn export_trust_bundle(&self) -> Result<TrustBundle> {
        let snapshot = self.snapshot().await;

        let advertised_host = std::env::var("BOUNDLESS_ADVERTISE_HOST")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| snapshot.device_name.clone());

        Ok(TrustBundle {
            machine_id: snapshot.machine_id,
            display_name: snapshot.device_name,
            network_address: format!("{advertised_host}:{}", snapshot.network_port),
            ca_cert_pem: self.identity.ca_cert_pem.clone(),
        })
    }

    pub async fn import_trust_bundle(
        &self,
        bundle: TrustBundle,
        alias: Option<String>,
    ) -> Result<()> {
        validate_ca_cert_pem(&bundle.ca_cert_pem)?;
        let default_port = self.config.read().await.network_port;
        let normalized_address = normalize_peer_address(&bundle.network_address, default_port)?;

        upsert_trust_record(
            &self.security_paths,
            TrustRecord {
                machine_id: bundle.machine_id.clone(),
                ca_cert_pem: bundle.ca_cert_pem.clone(),
                added_at: Utc::now(),
            },
        )?;

        let mut config = self.config.write().await;

        if let Some(peer) = config
            .peers
            .iter_mut()
            .find(|p| p.peer_id == bundle.machine_id)
        {
            peer.address = normalized_address;
            peer.display_name = alias.unwrap_or(bundle.display_name);
            peer.connected = false;
            peer.last_seen = Utc::now();
        } else {
            config.peers.push(PeerConfig {
                peer_id: bundle.machine_id,
                display_name: alias.unwrap_or(bundle.display_name),
                address: normalized_address,
                connected: false,
                last_seen: Utc::now(),
            });
        }

        save_config_at(&self.config_path, &config)
    }

    pub async fn snapshot(&self) -> RuntimeConfig {
        self.config.read().await.clone()
    }

    pub(crate) async fn cached_layout_matrix_for_spec(&self, spec: &str) -> Arc<Vec<Vec<String>>> {
        if let Some(cached) = self.parsed_layout_matrix_cache.read().await.as_ref()
            && cached.spec == spec
        {
            return cached.matrix.clone();
        }

        let parsed = Arc::new(parse_layout_matrix(spec));
        let mut cache = self.parsed_layout_matrix_cache.write().await;
        if let Some(cached) = cache.as_ref()
            && cached.spec == spec
        {
            return cached.matrix.clone();
        }
        *cache = Some(ParsedLayoutMatrixCache {
            spec: spec.to_string(),
            matrix: parsed.clone(),
        });
        parsed
    }

    pub(crate) async fn invalidate_cached_layout_matrix(&self) {
        *self.parsed_layout_matrix_cache.write().await = None;
    }

    pub fn subscribe_outgoing_flush_signal(&self) -> watch::Receiver<u64> {
        self.transport.subscribe_outgoing_flush_signal()
    }

    pub(crate) fn notify_outgoing_flush_signal(&self) {
        self.transport.notify_outgoing_flush_signal();
    }

    fn record_runtime_wake(&self, channel: &str, source: &str) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "runtime_wake".to_string(),
            peer_id: "none".to_string(),
            detail: format!("channel={channel} source={source}"),
            size_bytes: 0,
        });
    }

    pub(crate) fn notify_input_capture_wake(&self, source: &str) {
        if self.input_capture_wake.trigger() {
            self.record_runtime_wake("input_capture", source);
            self.input_capture_wake.notify_one();
        }
    }

    pub(crate) fn input_capture_wake_signal(&self) -> Arc<RuntimeWakeSignal> {
        self.input_capture_wake.clone()
    }

    pub(crate) fn input_inject_wake_signal(&self) -> Arc<RuntimeWakeSignal> {
        self.input_inject_wake.clone()
    }

    pub(crate) fn notify_input_inject_wake(&self, source: &str) {
        if self.input_inject_wake.trigger() {
            self.record_runtime_wake("input_inject", source);
            self.input_inject_wake.notify_one();
        }
    }

    pub(crate) fn notify_peer_reconcile_wake(&self, source: &str) {
        if self.transport.peer_reconcile_wake.trigger() {
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "peer_reconcile_trigger".to_string(),
                peer_id: "all".to_string(),
                detail: format!("source={source}"),
                size_bytes: 0,
            });
            self.transport.peer_reconcile_wake.notify_one();
        }
    }

    pub(crate) fn peer_reconcile_wake_signal(&self) -> Arc<RuntimeWakeSignal> {
        self.transport.peer_reconcile_wake.clone()
    }

    pub(crate) fn record_input_queue_high_water(
        &self,
        queue_name: &str,
        peer_id: &str,
        depth: usize,
    ) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_queue_high_water".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("queue={queue_name} depth={depth}"),
            size_bytes: depth as u64,
        });
    }

    pub(crate) fn record_input_queue_overflow_drop(
        &self,
        queue_name: &str,
        peer_id: &str,
        sequence: u64,
        reason: &str,
    ) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_queue_overflow_drop".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("queue={queue_name} sequence={sequence} reason={reason}"),
            size_bytes: 0,
        });
    }

    pub(crate) fn record_input_queue_coalesced(
        &self,
        queue_name: &str,
        peer_id: &str,
        older_sequence: u64,
        newer_sequence: u64,
        merged_event_count: usize,
    ) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_queue_coalesced".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "queue={queue_name} older_sequence={older_sequence} newer_sequence={newer_sequence} merged_events={merged_event_count}"
            ),
            size_bytes: merged_event_count as u64,
        });
    }
}
