use eframe::egui;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
#[cfg(windows)]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(windows)]
use windows_sys::Win32::Foundation::HWND;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow, WM_CLOSE,
};

mod dashboard_layout {
    include!("dashboard/layout.rs");
}
mod dashboard_model {
    include!("dashboard/model.rs");
}
mod dashboard_task_runner {
    include!("dashboard/task_runner.rs");
}
mod dashboard_window {
    include!("dashboard/window.rs");
}
mod dashboard_workflow {
    include!("dashboard/workflow.rs");
}

use dashboard_model::{AppMsg, DashboardApp, Tab};
use dashboard_task_runner::{DashboardTaskRunner, SubmitPairingCodeTask};
use dashboard_window::{
    hide_dashboard_window, native_window_handle_from_creation_context, request_dashboard_exit,
    show_dashboard_window,
};

pub(super) fn run() -> Result<()> {
    let cli = Cli::parse();
    let ctx = Arc::new(AppContext {
        endpoint: cli.endpoint,
        start_daemon: cli.start_daemon,
        daemon_candidates: resolve_boundlessd_candidates(std::env::current_exe().ok()),
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_visible(false)
            .with_inner_size([760.0, 560.0])
            .with_icon(make_window_icon()?)
            .with_title("Boundless Dashboard"),
        ..Default::default()
    };

    eframe::run_native(
        "Boundless Dashboard",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(DashboardApp::new(cc, ctx)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {:?}", e))
}

fn validate_pairing_code(code: &str) -> Result<()> {
    if code.trim().is_empty() {
        anyhow::bail!("pairing code cannot be empty");
    }
    Ok(())
}

fn guided_flow_from_discovered_peer(peer: &UiDiscoveredPeer) -> Result<GuidedPairingFlow> {
    let (host, pairing_port) = host_and_pairing_port_from_endpoint(&peer.endpoint)
        .context("Failed to parse peer endpoint")?;

    Ok(GuidedPairingFlow {
        dialog_title: format!("Pair with {}", peer.display_name),
        host,
        pairing_port,
        default_alias: peer.display_name.clone(),
        orientation_selector_fallback: peer.display_name.clone(),
    })
}

fn guided_flow_from_manual_input(host: &str, port_text: &str) -> Result<GuidedPairingFlow> {
    Ok(GuidedPairingFlow {
        dialog_title: format!("Manual Pair {}", host),
        host: host.to_string(),
        pairing_port: parse_pairing_port(port_text)?,
        default_alias: String::new(),
        orientation_selector_fallback: host.to_string(),
    })
}

fn should_offer_first_run_onboarding(snapshot: &UiSnapshot) -> bool {
    snapshot.daemon_online
        && !snapshot.machine_id.trim().is_empty()
        && snapshot.paired_peers.is_empty()
        && snapshot.pending_requests.is_empty()
        && snapshot.layout_matrix.trim() == CANONICAL_LOCAL_LAYOUT_TOKEN
}

fn should_hide_on_close(exit_requested: bool, tray_available: bool) -> bool {
    tray_available && !exit_requested
}

impl eframe::App for DashboardApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.exit_requested |= self.exit_requested_signal.load(Ordering::SeqCst);

        if ctx.input(|input| input.viewport().close_requested()) {
            if should_hide_on_close(self.exit_requested, self._tray_icon.is_some()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                hide_dashboard_window(self.native_window_handle, ctx);
            } else {
                self.exit_requested = true;
            }
        }

        while let Ok(msg) = self.rx.try_recv() {
            self.apply_app_msg(msg);
        }

        if self.pending_onboarding_focus {
            show_dashboard_window(self.native_window_handle, ctx);
            self.pending_onboarding_focus = false;
            self.onboarding_focus_shown = true;
        }

        if self.exit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.render_pairing_dialog(&ctx);

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Boundless");
                ui.separator();
                ui.selectable_value(&mut self.selected_tab, Tab::Status, "Status & Pairing");
                ui.selectable_value(&mut self.selected_tab, Tab::Layout, "Layout Manager");
                ui.selectable_value(&mut self.selected_tab, Tab::Settings, "Settings");
            });
            ui.separator();

            if let Some(err) = &self.last_error {
                let color = if self.last_message_is_error {
                    egui::Color32::LIGHT_RED
                } else {
                    egui::Color32::LIGHT_GREEN
                };
                ui.add(egui::Label::new(egui::RichText::new(err).color(color)));
                if self.pairing_retry_available
                    && !self.pairing_in_progress
                    && let Some(flow) = self.pairing_flow.clone()
                    && ui.button("Retry Pairing Request").clicked()
                {
                    self.start_pairing(flow, ctx.clone());
                }
                ui.separator();
            }

            match self.selected_tab {
                Tab::Status => self.render_status_tab(ui, &ctx),
                Tab::Layout => self.render_layout_tab(ui, &ctx),
                Tab::Settings => self.render_settings_tab(ui),
            }
        });
    }
}

fn build_dashboard_tray_icon() -> Result<TrayIcon> {
    let menu = Menu::new();
    menu
        .append(&MenuItem::with_id(
            ACTION_DASHBOARD,
            "Dashboard",
            true,
            None,
        ))
        .context("add dashboard menu item")?;
    menu.append(&PredefinedMenuItem::separator())
        .context("add tray separator")?;
    menu
        .append(&MenuItem::with_id(ACTION_QUIT, "Quit", true, None))
        .context("add quit menu item")?;

    let icon = make_tray_icon().context("build tray icon image")?;
    TrayIconBuilder::new()
        .with_tooltip("Boundless")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .context("build tray icon")
}
