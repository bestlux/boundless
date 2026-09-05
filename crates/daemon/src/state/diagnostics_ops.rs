use super::*;

#[derive(Debug, Default)]
struct PairingRejectionCounts {
    code_attempts: usize,
    nonce_attempts: usize,
    code_and_nonce_attempts: usize,
    expired: usize,
    manual_or_policy: usize,
    other: usize,
}

fn classify_pairing_rejection(message: &str) -> &'static str {
    if message.contains("verification code rejected: too many attempts") {
        "code_attempts"
    } else if message.contains("verification nonce rejected: too many attempts") {
        "nonce_attempts"
    } else if message.contains("verification code and nonce rejected: too many attempts") {
        "code_and_nonce_attempts"
    } else if message.contains("verification code expired") {
        "expired"
    } else if message.contains("nearby pairing request rejected") {
        "manual_or_policy"
    } else {
        "other"
    }
}

impl AppState {
    pub async fn safe_reset(&self, network_only: bool, all: bool) -> Result<()> {
        let input_enabled = self
            .mutate_config_and_save(|config| {
                if all {
                    let machine_id = config.machine_id.clone();
                    let device_name = config.device_name.clone();
                    *config = RuntimeConfig::default();
                    config.machine_id = machine_id;
                    config.device_name = device_name;
                } else if network_only {
                    config.peers.clear();
                }

                Ok((
                    config.features.get("share_input").copied().unwrap_or(true),
                    true,
                ))
            })
            .await?;

        self.transport.clear().await;
        self.outbound_file_transfers.write().await.clear();
        self.file_transfer_records.write().await.clear();
        self.clipboard.clear().await;
        self.input.reset(input_enabled).await;
        self.input_broker.detach_any();
        self.pairing.clear().await;
        self.invalidate_cached_layout_matrix().await;

        Ok(())
    }

    pub async fn diagnostics_dump(&self, output_path: Option<String>) -> Result<String> {
        let lease = self.user_io_lease().await?;
        let target = if let Some(path) = output_path {
            PathBuf::from(path)
        } else {
            #[cfg(windows)]
            {
                lease.default_diagnostics_dir().await?
            }
            #[cfg(not(windows))]
            {
                dirs::data_local_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("Boundless")
                    .join("diagnostics")
            }
        };

        let stamp = format!(
            "{}-{}",
            Utc::now().format("%Y%m%d-%H%M%S"),
            uuid::Uuid::new_v4()
        );
        let file_path = target.join(format!("dump-{stamp}.txt"));
        let manifest_path = target.join(format!("dump-{stamp}.redaction.txt"));

        let snapshot = self.snapshot().await;
        let trust_count = self
            .trusted_records()
            .await
            .map(|items| items.len())
            .unwrap_or(0);
        let event_count = self.transport.transport_event_count();
        let input_owner = self
            .input_owner()
            .await
            .unwrap_or_else(|| "none".to_string());
        let input_capture_target = self
            .input_capture_target()
            .await
            .unwrap_or_else(|| "none".to_string());
        let input_handoff = self.input_handoff_config().await;
        let input_capture_backend_mode = self.input_capture_backend_mode().await;
        let (pending_inject_frames, pending_inject_high_water) =
            self.pending_inject_frame_stats().await;
        let elevated_injector_status = self.input_broker.elevated_injector_status();
        let pairing_diagnostics = self.pairing_diagnostics_report().await;

        let report = format!(
            "Boundless Diagnostics\nMachine: [redacted-machine-id]\nFingerprint: [redacted-fingerprint]\nPeers: {}\nTrusted CAs: {}\nTransport Events: {}\nInput Owner: {}\nInput Capture Target: {}\nInput Capture Backend Mode: {}\nInput Pending Inject Frames: {}\nInput Pending Inject High Water: {}\nElevated Injector State: {}\nElevated Injector Reason: {}\nElevated Injector Signature Trust: {}\nInput Handoff: block_screen_corners={} corner_block_px={} relative_mouse={} hide_cursor_at_edge={} draw_cursor_marker={}\nAPI: [redacted-endpoint]\nTransport Port: {}\nProtocol: {}\n{}\nRedaction Manifest: sidecar redaction manifest written next to this dump\n",
            snapshot.peers.len(),
            trust_count,
            event_count,
            redact_identifier(&input_owner),
            redact_identifier(&input_capture_target),
            input_capture_backend_mode,
            pending_inject_frames,
            pending_inject_high_water,
            elevated_injector_status.state,
            elevated_injector_status.reason,
            elevated_injector_status.signature_trust,
            input_handoff.block_screen_corners,
            input_handoff.corner_block_px,
            input_handoff.relative_mouse,
            input_handoff.hide_cursor_at_edge,
            input_handoff.draw_cursor_marker,
            snapshot.network_port,
            snapshot.protocol_version,
            pairing_diagnostics
        );
        let manifest = "default_redaction=true\nredacted=machine_id,fingerprint,api_bind,peer_ids,request_ids,lockout_ips,local_paths,trust_material\nfull_diagnostics_opt_in=not_enabled\n";

        let output = file_path.display().to_string();
        lease
            .run_sync(move || {
                use std::io::Write;
                std::fs::create_dir_all(&target)?;
                // Exclusive creates prevent replacing a prior export or following
                // an attacker-created final-file link. Parent traversal is still
                // checked under the captured user's token by Windows.
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&file_path)?;
                let mut sidecar = match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&manifest_path)
                {
                    Ok(file) => file,
                    Err(error) => {
                        drop(file);
                        let _ = std::fs::remove_file(&file_path);
                        return Err(error.into());
                    }
                };
                file.write_all(report.as_bytes())?;
                sidecar.write_all(manifest.as_bytes())?;
                Ok(())
            })
            .await?;
        Ok(output)
    }

    async fn pairing_diagnostics_report(&self) -> String {
        use std::fmt::Write as _;

        let now = Utc::now();

        let pending_requests = self.pairing.pending_requests.read().await;
        let pending_total = pending_requests.len();
        let mut pending_manual = 0_usize;
        let mut pending_code_challenge = 0_usize;
        for record in pending_requests.values() {
            match record.mode {
                PendingNearbyPairingMode::ManualApproval => pending_manual += 1,
                PendingNearbyPairingMode::CodeChallenge { .. } => pending_code_challenge += 1,
            }
        }
        drop(pending_requests);

        let decisions = self.pairing.decisions.read().await;
        let decisions_total = decisions.len();
        let mut decisions_approved = 0_usize;
        let mut decisions_rejected = 0_usize;
        let mut rejection_counts = PairingRejectionCounts::default();
        let mut recent_rejections = Vec::<(chrono::DateTime<Utc>, String, String)>::new();
        for (request_id, record) in decisions.iter() {
            match &record.decision {
                NearbyPairingDecision::Approved { .. } => decisions_approved += 1,
                NearbyPairingDecision::Rejected { message } => {
                    decisions_rejected += 1;
                    match classify_pairing_rejection(message) {
                        "code_attempts" => rejection_counts.code_attempts += 1,
                        "nonce_attempts" => rejection_counts.nonce_attempts += 1,
                        "code_and_nonce_attempts" => rejection_counts.code_and_nonce_attempts += 1,
                        "expired" => rejection_counts.expired += 1,
                        "manual_or_policy" => rejection_counts.manual_or_policy += 1,
                        _ => rejection_counts.other += 1,
                    }
                    recent_rejections.push((
                        record.decided_at,
                        request_id.clone(),
                        message.clone(),
                    ));
                }
            }
        }
        drop(decisions);
        recent_rejections.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        recent_rejections.truncate(5);

        let failure_window =
            chrono::TimeDelta::seconds(NEARBY_PAIRING_CODE_SUBMISSION_FAILURE_WINDOW_SECONDS);
        let failures_by_ip = self.pairing.code_submission_failures_by_ip.read().await;
        let mut failure_window_ips = 0_usize;
        let mut failure_window_attempts = 0_usize;
        for timestamps in failures_by_ip.values() {
            let active_attempts = timestamps
                .iter()
                .filter(|timestamp| **timestamp + failure_window >= now)
                .count();
            if active_attempts > 0 {
                failure_window_ips += 1;
                failure_window_attempts += active_attempts;
            }
        }
        drop(failures_by_ip);

        let lockouts_by_ip = self.pairing.code_submission_lockout_by_ip.read().await;
        let mut active_lockouts = Vec::<(IpAddr, i64)>::new();
        for (ip, until) in lockouts_by_ip.iter() {
            if *until >= now {
                active_lockouts.push((*ip, (*until - now).num_seconds()));
            }
        }
        drop(lockouts_by_ip);
        active_lockouts.sort_by_key(|entry| std::cmp::Reverse(entry.1));

        let mut report = String::new();
        let _ = writeln!(report, "Pairing Diagnostics");
        let _ = writeln!(report, "pairing_pending_total={pending_total}");
        let _ = writeln!(report, "pairing_pending_manual={pending_manual}");
        let _ = writeln!(
            report,
            "pairing_pending_code_challenge={pending_code_challenge}"
        );
        let _ = writeln!(report, "pairing_decisions_total={decisions_total}");
        let _ = writeln!(report, "pairing_decisions_approved={decisions_approved}");
        let _ = writeln!(report, "pairing_decisions_rejected={decisions_rejected}");
        let _ = writeln!(
            report,
            "pairing_rejections_code_attempts={}",
            rejection_counts.code_attempts
        );
        let _ = writeln!(
            report,
            "pairing_rejections_nonce_attempts={}",
            rejection_counts.nonce_attempts
        );
        let _ = writeln!(
            report,
            "pairing_rejections_code_and_nonce_attempts={}",
            rejection_counts.code_and_nonce_attempts
        );
        let _ = writeln!(
            report,
            "pairing_rejections_expired={}",
            rejection_counts.expired
        );
        let _ = writeln!(
            report,
            "pairing_rejections_manual_or_policy={}",
            rejection_counts.manual_or_policy
        );
        let _ = writeln!(
            report,
            "pairing_rejections_other={}",
            rejection_counts.other
        );
        let _ = writeln!(
            report,
            "pairing_submission_failure_window_ips={failure_window_ips}"
        );
        let _ = writeln!(
            report,
            "pairing_submission_failure_window_attempts={failure_window_attempts}"
        );
        let _ = writeln!(
            report,
            "pairing_submission_lockout_ips={}",
            active_lockouts.len()
        );
        for (index, (_ip, remaining_seconds)) in active_lockouts.iter().enumerate() {
            let _ = writeln!(
                report,
                "pairing_submission_lockout_{}=ip:[redacted-ip],remaining_seconds:{}",
                index + 1,
                (*remaining_seconds).max(0)
            );
        }
        for (index, (decided_at, request_id, message)) in recent_rejections.iter().enumerate() {
            let _ = writeln!(
                report,
                "pairing_recent_rejection_{}=request_id:{},decided_at:{},message:{}",
                index + 1,
                redact_identifier(request_id),
                decided_at.to_rfc3339(),
                message.replace('\n', " ")
            );
        }

        report.trim_end().to_string()
    }

    pub(crate) async fn connected_peer_ids(&self) -> Vec<String> {
        self.config
            .read()
            .await
            .peers
            .iter()
            .filter(|peer| peer.connected)
            .map(|peer| peer.peer_id.clone())
            .collect()
    }

    pub(crate) async fn validated_clipboard_payload_hash(
        &self,
        payload: &ClipboardPayload,
    ) -> Result<Option<String>> {
        if let ClipboardPayload::Image(image_bmp) = payload {
            validate_bmp_payload(image_bmp).context("invalid clipboard BMP payload")?;
        }

        let policy = ClipboardPolicy {
            enabled: self
                .config
                .read()
                .await
                .features
                .get("share_clipboard")
                .copied()
                .unwrap_or(true),
            ..ClipboardPolicy::default()
        };

        match validate_payload(policy, payload) {
            Ok(()) => Ok(Some(payload_hash_hex(payload))),
            Err(ClipboardPolicyError::Disabled) => Ok(None),
            Err(error) => Err(anyhow::anyhow!(error)),
        }
    }
}

fn redact_identifier(value: &str) -> String {
    if value == "none" || value.is_empty() {
        value.to_string()
    } else {
        "[redacted-id]".to_string()
    }
}
