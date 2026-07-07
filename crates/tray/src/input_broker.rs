// User-session input broker host.
//
// When the local control plane is owned by the LocalSystem service (which
// truthfully reports `service_session_unsupported` for interactive input),
// this broker runs in the tray's interactive user session, captures local
// input with the shared hook pump, relays it to the service over the existing
// allowed-user named pipe, and injects authenticated incoming frames with
// SendInput. The service remains the trust/network/routing authority; this
// covers the normal unlocked desktop only (no lock screen, secure desktop,
// UAC prompts, or elevated apps).

use ipc_api::boundless::v1::{
    ClipboardBrokerApplyReport, ClipboardBrokerExchangeRequest, ClipboardBrokerPayload,
    InputBrokerAttachRequest, InputBrokerDetachRequest, InputBrokerExchangeRequest,
    clipboard_broker_payload, control_plane_service_client::ControlPlaneServiceClient,
};
use ipc_api::broker_events::{broker_events_from_input_events, input_events_from_broker_events};
use platform_windows::clipboard_backend::WindowsClipboardBackend;
use platform_windows::input::{
    HookControlAction, HookInputPump, current_process_can_use_interactive_input,
    input_records_for_event, send_input_records, virtual_screen_bounds,
};
use tonic::transport::Channel;

const INPUT_BROKER_SERVICE_UNSUPPORTED_MODE: &str = "service_session_unsupported";
const INPUT_BROKER_SUPERVISOR_RETRY: Duration = Duration::from_secs(3);
const INPUT_BROKER_ACTIVE_POLL: Duration = Duration::from_millis(8);
const INPUT_BROKER_IDLE_POLL: Duration = Duration::from_millis(40);
const CLIPBOARD_BROKER_POLL: Duration = Duration::from_millis(200);

enum BrokerSessionEnd {
    NotNeeded,
    Detached,
}

pub(super) fn spawn_input_broker_supervisor(endpoint: String) {
    let _ = std::thread::Builder::new()
        .name("boundless-input-broker".to_string())
        .spawn(move || input_broker_supervisor_loop(&endpoint));
}

fn input_broker_supervisor_loop(endpoint: &str) {
    loop {
        match run_input_broker_session(endpoint) {
            Ok(BrokerSessionEnd::NotNeeded) | Ok(BrokerSessionEnd::Detached) => {}
            Err(error) => eprintln!("boundless input broker session ended: {error:#}"),
        }
        std::thread::sleep(INPUT_BROKER_SUPERVISOR_RETRY);
    }
}

fn run_input_broker_session(endpoint: &str) -> Result<BrokerSessionEnd> {
    // Fail closed: never broker interactive input from a non-interactive
    // (session 0) process, even if a daemon would accept it.
    if !current_process_can_use_interactive_input().unwrap_or(false) {
        return Ok(BrokerSessionEnd::NotNeeded);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create tokio runtime for input broker")?;
    runtime.block_on(async move {
        let mut client = connect_control_plane(endpoint).await?;
        let backend_mode = client
            .get_ui_snapshot(Empty {})
            .await?
            .into_inner()
            .input_runtime
            .map(|runtime| runtime.capture_backend_mode)
            .unwrap_or_default();
        if backend_mode != INPUT_BROKER_SERVICE_UNSUPPORTED_MODE {
            // A user-session daemon owns capture/injection directly; a broker
            // would double-capture. Stay detached and re-check later.
            return Ok(BrokerSessionEnd::NotNeeded);
        }

        let mut pump =
            HookInputPump::start(|_source| {}).context("install user-session capture hooks")?;

        // The daemon authorizes this attach against the verified pipe client
        // identity (our process token SID and session), not anything we send.
        let attach = client
            .attach_input_broker(InputBrokerAttachRequest {
                broker_version: env!("CARGO_PKG_VERSION").to_string(),
                lock_supported: true,
            })
            .await?
            .into_inner();
        if !attach.accepted {
            eprintln!("boundless input broker attach rejected: {}", attach.message);
            return Ok(BrokerSessionEnd::NotNeeded);
        }
        let broker_token = attach.broker_token;

        let mut input_client = client.clone();
        let mut clipboard_client = client.clone();
        let loop_result = tokio::select! {
            result = input_broker_exchange_loop(&mut input_client, &broker_token, &mut pump) => result,
            result = clipboard_broker_exchange_loop(&mut clipboard_client, &broker_token) => result,
        };

        // Best-effort cleanup: release the local lock, flush synthetic
        // release events for anything still held, and detach explicitly.
        let _ = pump.set_lock_active(false);
        let release_events = pump.drain_release_events();
        if !release_events.is_empty() {
            let _ = client
                .exchange_input_broker(InputBrokerExchangeRequest {
                    broker_token: broker_token.clone(),
                    captured_events: broker_events_from_input_events(&release_events),
                    ..Default::default()
                })
                .await;
        }
        let _ = client
            .detach_input_broker(InputBrokerDetachRequest {
                broker_token: broker_token.clone(),
            })
            .await;

        loop_result.map(|_| BrokerSessionEnd::Detached)
    })
}

async fn input_broker_exchange_loop(
    client: &mut ControlPlaneServiceClient<Channel>,
    broker_token: &str,
    pump: &mut HookInputPump,
) -> Result<()> {
    let mut injected_frame_count = 0u32;
    let mut inject_failure_count = 0u32;

    loop {
        let captured = pump.poll_events();
        let escape_unlock_count = pump
            .drain_control_actions()
            .into_iter()
            .filter(|action| matches!(action, HookControlAction::EscapeUnlock))
            .count() as u32;
        let cursor = pump
            .cursor_position()
            .or_else(|| platform_windows::input::cursor_position().ok().flatten());
        let bounds = virtual_screen_bounds();

        let reply = client
            .exchange_input_broker(InputBrokerExchangeRequest {
                broker_token: broker_token.to_string(),
                captured_events: broker_events_from_input_events(&captured),
                cursor_valid: cursor.is_some(),
                cursor_x: cursor.map(|(x, _)| x).unwrap_or_default(),
                cursor_y: cursor.map(|(_, y)| y).unwrap_or_default(),
                bounds_valid: bounds.is_some(),
                bounds_left: bounds.map(|bounds| bounds.0).unwrap_or_default(),
                bounds_top: bounds.map(|bounds| bounds.1).unwrap_or_default(),
                bounds_right: bounds.map(|bounds| bounds.2).unwrap_or_default(),
                bounds_bottom: bounds.map(|bounds| bounds.3).unwrap_or_default(),
                escape_unlock_count,
                lock_active: pump.lock_active(),
                dropped_event_count: pump.take_dropped_event_count(),
                injected_frame_count,
                inject_failure_count,
            })
            .await?
            .into_inner();
        if !reply.accepted {
            bail!("input broker exchange rejected: {}", reply.message);
        }
        injected_frame_count = 0;
        inject_failure_count = 0;

        if pump.lock_active() != reply.lock_should_be_active
            && let Err(error) = pump.set_lock_active(reply.lock_should_be_active)
        {
            eprintln!(
                "boundless input broker failed to update local input lock: {error:#}"
            );
        }

        let had_inject_frames = !reply.inject_frames.is_empty();
        for frame in &reply.inject_frames {
            let (events, undecodable) = input_events_from_broker_events(&frame.events);
            if undecodable > 0 || inject_input_events(&events).is_err() {
                inject_failure_count = inject_failure_count.saturating_add(1);
            } else {
                injected_frame_count = injected_frame_count.saturating_add(1);
            }
        }

        let poll = if reply.capture_active || had_inject_frames || !captured.is_empty() {
            INPUT_BROKER_ACTIVE_POLL
        } else {
            INPUT_BROKER_IDLE_POLL
        };
        tokio::time::sleep(poll).await;
    }
}

fn inject_input_events(events: &[core_input::InputEvent]) -> Result<()> {
    let mut records = Vec::new();
    for event in events {
        records.extend(input_records_for_event(event));
    }
    send_input_records(&records)
}

async fn clipboard_broker_exchange_loop(
    client: &mut ControlPlaneServiceClient<Channel>,
    broker_token: &str,
) -> Result<()> {
    let mut last_sequence: Option<u64> = None;
    let mut apply_report: Option<ClipboardBrokerApplyReport> = None;

    loop {
        let local_payload = read_clipboard_payload_if_changed(&mut last_sequence).await;
        let reply = client
            .exchange_clipboard_broker(ClipboardBrokerExchangeRequest {
                broker_token: broker_token.to_string(),
                local_payload: local_payload.map(clipboard_payload_to_proto),
                apply_report: apply_report.take(),
            })
            .await?
            .into_inner();
        if !reply.accepted {
            bail!("clipboard broker exchange rejected: {}", reply.message);
        }
        if !reply.message.is_empty() {
            eprintln!("boundless clipboard broker: {}", reply.message);
        }

        if let Some(remote_payload) = reply.remote_payload {
            let result = write_clipboard_payload(remote_payload).await;
            apply_report = Some(ClipboardBrokerApplyReport {
                source_peer_id: reply.remote_source_peer_id,
                hash: reply.remote_hash,
                applied: result.is_ok(),
                message: result.err().map(|error| format!("{error:#}")).unwrap_or_default(),
            });
        }

        tokio::time::sleep(CLIPBOARD_BROKER_POLL).await;
    }
}

async fn read_clipboard_payload_if_changed(
    last_sequence: &mut Option<u64>,
) -> Option<core_clipboard::ClipboardPayload> {
    let sequence = tokio::task::spawn_blocking(|| {
        let mut backend = WindowsClipboardBackend;
        backend.sequence_number()
    })
    .await
    .ok()
    .flatten();
    if let Some(sequence) = sequence {
        if *last_sequence == Some(sequence) {
            return None;
        }
        *last_sequence = Some(sequence);
    }

    tokio::task::spawn_blocking(|| {
        let mut backend = WindowsClipboardBackend;
        backend.read_payload()
    })
    .await
    .ok()
    .and_then(|result| result.ok())
    .flatten()
}

async fn write_clipboard_payload(payload: ClipboardBrokerPayload) -> Result<()> {
    let payload = clipboard_payload_from_proto(payload).context("clipboard broker payload empty")?;
    tokio::task::spawn_blocking(move || {
        let mut backend = WindowsClipboardBackend;
        backend.write_payload(&payload)
    })
    .await
    .context("clipboard write task panicked")?
}

fn clipboard_payload_from_proto(
    payload: ClipboardBrokerPayload,
) -> Option<core_clipboard::ClipboardPayload> {
    match payload.payload? {
        clipboard_broker_payload::Payload::Text(text) => {
            Some(core_clipboard::ClipboardPayload::Text(text))
        }
        clipboard_broker_payload::Payload::ImageBmp(image_bmp) => {
            Some(core_clipboard::ClipboardPayload::Image(image_bmp))
        }
    }
}

fn clipboard_payload_to_proto(payload: core_clipboard::ClipboardPayload) -> ClipboardBrokerPayload {
    ClipboardBrokerPayload {
        payload: Some(match payload {
            core_clipboard::ClipboardPayload::Text(text) => {
                clipboard_broker_payload::Payload::Text(text)
            }
            core_clipboard::ClipboardPayload::Image(image_bmp) => {
                clipboard_broker_payload::Payload::ImageBmp(image_bmp)
            }
        }),
    }
}
