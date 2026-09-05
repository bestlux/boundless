use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::{
    commands::{
        ClipboardBrokerExchangeCommand, DiagnosticsDumpCommand, DiagnosticsDumpReply,
        FeatureSetCommand, FileTransferActionCommand, HotkeySetCommand, HotkeyTriggerCommand,
        ImportTrustBundleCommand, InputBrokerAttachCommand, InputBrokerDetachCommand,
        InputBrokerExchangeCommand, InputCaptureTargetCommand, InputCaptureTargetReply,
        InputOwnerCommand, InputOwnerReply, LayoutReply, LayoutSetCommand, NearbyJoinStartCommand,
        NearbyJoinStatusCommand, NearbyPairingDecisionCommand, NearbyRequestCodeCommand,
        NearbySubmitCodeCommand, OperationReply, PairJoinCommand, PairJoinReply, PairingCodeReply,
        PairingCodeRequest, RemovePeerCommand, RotateTrustCommand, SafeResetCommand,
        SendClipboardImageCommand, SendClipboardTextCommand, SendFileCommand, SendInputKeyCommand,
        SendInputMoveCommand, SetAntiIdleConfigCommand, SetFileTransferConfigCommand,
        SetInputHandoffConfigCommand,
    },
    queries::{
        AntiIdleConfigSnapshot, AntiIdleStatusSnapshot, ClipboardBrokerExchangeSnapshot,
        ConsoleSnapshot, FileTransferConfigSnapshot, InputBrokerAttachSnapshot,
        InputBrokerExchangeSnapshot, NearbyJoinStatusSnapshot, NearbyPairingCompletionSnapshot,
        NearbyRequestCodeStartSnapshot, StatusSnapshot, TransportEventSnapshot,
        TrustBundleSnapshot, UiSnapshot,
    },
};

#[async_trait]
pub trait ControlPlaneApp: Send + Sync {
    async fn paired_test_consent(
        &self,
        _peer_id: String,
        _duration_seconds: u32,
    ) -> Result<crate::paired_testing::PairedTestConsent> {
        anyhow::bail!("paired transport tests unavailable")
    }
    async fn get_paired_test_consent(&self) -> Result<crate::paired_testing::PairedTestConsent> {
        anyhow::bail!("paired transport tests unavailable")
    }
    async fn run_paired_test(
        &self,
        _options: crate::paired_testing::PairedTestOptions,
    ) -> Result<crate::paired_testing::PairedTestReport> {
        anyhow::bail!("paired transport tests unavailable")
    }
    async fn status_snapshot(&self) -> Result<StatusSnapshot>;
    async fn ui_snapshot(&self) -> Result<UiSnapshot>;
    async fn console_snapshot(&self) -> Result<ConsoleSnapshot>;
    async fn create_pairing_code(&self, request: PairingCodeRequest) -> Result<PairingCodeReply>;
    async fn join_with_pairing_code(&self, command: PairJoinCommand) -> Result<PairJoinReply>;
    async fn set_layout(&self, command: LayoutSetCommand) -> Result<OperationReply>;
    async fn list_peers(&self) -> Result<Vec<crate::queries::UiPairedPeer>>;
    async fn remove_peer(&self, command: RemovePeerCommand) -> Result<OperationReply>;
    async fn layout(&self) -> Result<LayoutReply>;
    async fn features(&self) -> Result<std::collections::BTreeMap<String, bool>>;
    async fn set_feature(&self, command: FeatureSetCommand) -> Result<OperationReply>;
    async fn anti_idle_config(&self) -> Result<AntiIdleConfigSnapshot>;
    async fn anti_idle_status(&self) -> Result<AntiIdleStatusSnapshot>;
    async fn set_anti_idle_config(
        &self,
        command: SetAntiIdleConfigCommand,
    ) -> Result<OperationReply>;
    async fn file_transfer_config(&self) -> Result<FileTransferConfigSnapshot>;
    async fn set_file_transfer_config(
        &self,
        command: SetFileTransferConfigCommand,
    ) -> Result<OperationReply>;
    async fn cancel_file_transfer(
        &self,
        command: FileTransferActionCommand,
    ) -> Result<OperationReply>;
    async fn retry_file_transfer(
        &self,
        command: FileTransferActionCommand,
    ) -> Result<OperationReply>;
    async fn clear_completed_file_transfers(&self) -> Result<OperationReply>;
    async fn set_input_handoff_config(
        &self,
        command: SetInputHandoffConfigCommand,
    ) -> Result<OperationReply>;
    async fn set_hotkey(&self, command: HotkeySetCommand) -> Result<OperationReply>;
    async fn trigger_hotkey_action(&self, command: HotkeyTriggerCommand) -> Result<OperationReply>;
    async fn export_trust_bundle(&self) -> Result<TrustBundleSnapshot>;
    async fn import_trust_bundle(
        &self,
        command: ImportTrustBundleCommand,
    ) -> Result<OperationReply>;
    async fn rotate_trust(&self, command: RotateTrustCommand) -> Result<OperationReply>;
    async fn dump_diagnostics(
        &self,
        command: DiagnosticsDumpCommand,
    ) -> Result<DiagnosticsDumpReply>;
    async fn safe_reset(&self, command: SafeResetCommand) -> Result<OperationReply>;
    async fn send_clipboard_text(
        &self,
        command: SendClipboardTextCommand,
    ) -> Result<OperationReply>;
    async fn send_clipboard_image(
        &self,
        command: SendClipboardImageCommand,
    ) -> Result<OperationReply>;
    async fn send_file(&self, command: SendFileCommand) -> Result<OperationReply>;
    async fn send_input_move(&self, command: SendInputMoveCommand) -> Result<OperationReply>;
    async fn send_input_key(&self, command: SendInputKeyCommand) -> Result<OperationReply>;
    async fn transport_events(&self) -> Result<Vec<TransportEventSnapshot>>;
    async fn input_owner(&self) -> Result<InputOwnerReply>;
    async fn claim_input_owner(&self, command: InputOwnerCommand) -> Result<InputOwnerReply>;
    async fn release_input_owner(&self, command: InputOwnerCommand) -> Result<InputOwnerReply>;
    async fn input_capture_target(&self) -> Result<InputCaptureTargetReply>;
    async fn set_input_capture_target(
        &self,
        command: InputCaptureTargetCommand,
    ) -> Result<InputCaptureTargetReply>;
    async fn clear_input_capture_target(&self) -> Result<InputCaptureTargetReply>;
    async fn attach_input_broker(
        &self,
        command: InputBrokerAttachCommand,
    ) -> Result<InputBrokerAttachSnapshot>;
    async fn exchange_input_broker(
        &self,
        command: InputBrokerExchangeCommand,
    ) -> Result<InputBrokerExchangeSnapshot>;
    async fn exchange_clipboard_broker(
        &self,
        command: ClipboardBrokerExchangeCommand,
    ) -> Result<ClipboardBrokerExchangeSnapshot>;
    async fn detach_input_broker(
        &self,
        command: InputBrokerDetachCommand,
    ) -> Result<OperationReply>;
    async fn request_nearby_pairing_code(
        &self,
        command: NearbyRequestCodeCommand,
    ) -> Result<NearbyRequestCodeStartSnapshot>;
    async fn submit_nearby_pairing_code(
        &self,
        command: NearbySubmitCodeCommand,
    ) -> Result<NearbyPairingCompletionSnapshot>;
    async fn start_nearby_pairing_join(
        &self,
        command: NearbyJoinStartCommand,
    ) -> Result<NearbyJoinStatusSnapshot>;
    async fn check_nearby_pairing_join(
        &self,
        command: NearbyJoinStatusCommand,
    ) -> Result<NearbyJoinStatusSnapshot>;
    async fn approve_nearby_pairing_request(
        &self,
        command: NearbyPairingDecisionCommand,
    ) -> Result<OperationReply>;
    async fn reject_nearby_pairing_request(
        &self,
        command: NearbyPairingDecisionCommand,
    ) -> Result<OperationReply>;
}

pub type SharedControlPlaneApp = Arc<dyn ControlPlaneApp>;
