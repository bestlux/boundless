use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{
    anti_idle, clipboard, config::ApiTransport, discovery, hotkeys, input, network, pairing_wire,
    state::AppState,
};

#[derive(Debug, Clone, Default)]
pub struct HostOverrides {
    pub bind: Option<String>,
    pub api_transport: Option<ApiTransport>,
    pub api_pipe_name: Option<String>,
    pub network_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HostRuntimeOptions {
    pub input_runtime_mode: input::InputRuntimeMode,
}

#[derive(Clone)]
pub struct DaemonRuntime {
    pub state: AppState,
    pub snapshot: crate::config::RuntimeConfig,
    pub effective_api_transport: ApiTransport,
}

pub async fn run_with<F, Fut>(overrides: HostOverrides, serve: F) -> Result<()>
where
    F: FnOnce(DaemonRuntime) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    run_with_options(overrides, HostRuntimeOptions::default(), serve).await
}

pub async fn run_with_options<F, Fut>(
    overrides: HostOverrides,
    options: HostRuntimeOptions,
    serve: F,
) -> Result<()>
where
    F: FnOnce(DaemonRuntime) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let runtime = prepare_runtime_with_options(overrides, options).await?;
    run_prepared_runtime_with_options(runtime, options, serve).await
}

pub async fn prepare_runtime_with_options(
    overrides: HostOverrides,
    options: HostRuntimeOptions,
) -> Result<DaemonRuntime> {
    let state = AppState::load_or_create().context("load app state")?;
    prepare_loaded_state_with_options(state, overrides, options).await
}

async fn prepare_loaded_state_with_options(
    state: AppState,
    overrides: HostOverrides,
    options: HostRuntimeOptions,
) -> Result<DaemonRuntime> {
    apply_overrides(&state, overrides).await?;
    input::apply_startup_mode(&state, options.input_runtime_mode).await;
    let _ = state.reconcile_anti_idle_runtime().await;

    let snapshot = state.snapshot().await;
    let configured_api_transport = snapshot.api_transport;
    let effective_api_transport = configured_api_transport.effective();
    if configured_api_transport != effective_api_transport {
        warn!(
            configured = configured_api_transport.as_str(),
            effective = effective_api_transport.as_str(),
            "configured API transport is not supported on this platform; using fallback"
        );
    }

    info!(
        machine_id = %snapshot.machine_id,
        api_bind = %snapshot.api_bind,
        api_transport = effective_api_transport.as_str(),
        api_pipe_name = %snapshot.api_pipe_name,
        network_port = snapshot.network_port,
        protocol = %snapshot.protocol_version,
        "boundless daemon starting"
    );

    Ok(DaemonRuntime {
        state,
        snapshot,
        effective_api_transport,
    })
}

pub async fn start_runtime_tasks(runtime: &DaemonRuntime, options: HostRuntimeOptions) {
    let state = runtime.state.clone();
    let transport_listener = network::prepare_listener(&state).await;

    clipboard::start(state.clone());
    discovery::start(state.clone());
    input::start(state.clone(), options.input_runtime_mode);
    anti_idle::start(state.clone());
    hotkeys::start(state.clone());
    pairing_wire::start(state.clone());
    network::start(state, transport_listener);
}

pub fn runtime_task_health_json(runtime: &DaemonRuntime) -> String {
    crate::runtime_tasks::task_health_json(&runtime.state.runtime_task_snapshots()).to_string()
}

pub async fn shutdown_runtime(runtime: &DaemonRuntime) {
    shutdown_runtime_state(&runtime.state).await;
}

async fn run_prepared_runtime_with_options<F, Fut>(
    runtime: DaemonRuntime,
    options: HostRuntimeOptions,
    serve: F,
) -> Result<()>
where
    F: FnOnce(DaemonRuntime) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    start_runtime_tasks(&runtime, options).await;

    let runtime_state = runtime.state.clone();
    let result = serve(DaemonRuntime {
        state: runtime.state,
        snapshot: runtime.snapshot,
        effective_api_transport: runtime.effective_api_transport,
    })
    .await;

    shutdown_runtime_state(&runtime_state).await;
    result
}

async fn shutdown_runtime_state(runtime_state: &AppState) {
    runtime_state.begin_transport_session_shutdown();
    runtime_state.shutdown_runtime_tasks().await;
    let aborted_transport_sessions = runtime_state
        .abort_all_transport_sessions_for_shutdown()
        .await;
    if aborted_transport_sessions > 0 {
        info!(
            aborted_transport_sessions,
            "aborted transport sessions during daemon shutdown"
        );
    }
}

pub async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        warn!(%error, "ctrl_c signal error");
    }
    info!("shutdown requested");
}

async fn apply_overrides(state: &AppState, overrides: HostOverrides) -> Result<()> {
    if let Some(bind) = overrides.bind {
        bind.parse::<std::net::SocketAddr>()
            .with_context(|| format!("invalid --bind address {bind}"))?;
        state
            .update_bind(bind)
            .await
            .context("update bind address")?;
    }
    if let Some(transport) = overrides.api_transport {
        state
            .update_api_transport(transport)
            .await
            .context("update API transport")?;
    }
    if let Some(pipe_name) = overrides.api_pipe_name {
        state
            .update_api_pipe_name(pipe_name)
            .await
            .context("update API pipe name")?;
    }
    if let Some(port) = overrides.network_port {
        state
            .update_network_port(port)
            .await
            .context("update network port")?;
    }

    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::config::{RuntimeConfig, save_config_at};

    #[tokio::test]
    async fn service_mode_named_pipe_startup_preserves_diagnostic_input_state() {
        use platform_windows::runtime::{
            current_user_sid_string, named_pipe_incoming_for_allowed_user,
        };

        let root = std::env::temp_dir().join(format!(
            "boundless-service-startup-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
        let pipe_name = format!("boundlessd-api-test-{}", uuid::Uuid::new_v4());
        let allowed_user_sid = current_user_sid_string().expect("current user sid");

        let runtime = prepare_loaded_state_with_options(
            state,
            HostOverrides {
                bind: None,
                api_transport: Some(ApiTransport::NamedPipe),
                api_pipe_name: Some(pipe_name.clone()),
                network_port: Some(0),
            },
            HostRuntimeOptions {
                input_runtime_mode: input::InputRuntimeMode::ServiceSessionUnsupported,
            },
        )
        .await
        .expect("prepare service runtime");

        assert_eq!(runtime.effective_api_transport, ApiTransport::NamedPipe);
        assert_eq!(runtime.snapshot.api_pipe_name, pipe_name);

        let _incoming = named_pipe_incoming_for_allowed_user(
            &runtime.snapshot.api_pipe_name,
            &allowed_user_sid,
        )
        .expect("secure named-pipe bind should succeed");
        start_runtime_tasks(
            &runtime,
            HostRuntimeOptions {
                input_runtime_mode: input::InputRuntimeMode::ServiceSessionUnsupported,
            },
        )
        .await;

        let bundle = runtime.state.control_plane_snapshot_bundle().await;
        assert_eq!(
            bundle.input_capture_backend_mode,
            "service_session_unsupported"
        );
        assert!(!bundle.input_locked);
        assert!(!bundle.input_lock_supported);
        assert!(bundle.active_input_capture_target_peer_id.is_none());

        runtime.state.shutdown_runtime_tasks().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn service_mode_runtime_prep_migrates_stale_local_protocol_config() {
        let root = std::env::temp_dir().join(format!(
            "boundless-service-stale-config-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        std::fs::create_dir_all(&root).expect("create temp root");
        save_config_at(
            &config_path,
            &RuntimeConfig {
                protocol_version: "4.1.0".to_string(),
                ..RuntimeConfig::default()
            },
        )
        .expect("seed stale service config");

        let state = AppState::load_or_create_with_paths(config_path.clone(), security_root)
            .expect("load state should migrate stale local protocol");
        let runtime = prepare_loaded_state_with_options(
            state,
            HostOverrides {
                bind: None,
                api_transport: Some(ApiTransport::NamedPipe),
                api_pipe_name: Some(format!("boundlessd-api-test-{}", uuid::Uuid::new_v4())),
                network_port: Some(0),
            },
            HostRuntimeOptions {
                input_runtime_mode: input::InputRuntimeMode::ServiceSessionUnsupported,
            },
        )
        .await
        .expect("prepare service runtime with migrated config");

        assert_eq!(
            runtime.snapshot.protocol_version,
            core_protocol::PROTOCOL_CURRENT.to_string()
        );
        assert_eq!(runtime.effective_api_transport, ApiTransport::NamedPipe);
        let saved = std::fs::read_to_string(&config_path).expect("read migrated config");
        assert!(saved.contains(&format!(
            r#""protocol_version": "{}""#,
            core_protocol::PROTOCOL_CURRENT
        )));

        let _ = std::fs::remove_dir_all(root);
    }
}
