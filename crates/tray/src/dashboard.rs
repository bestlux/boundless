use eframe::egui;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
#[cfg(windows)]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
#[cfg(windows)]
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
#[cfg(windows)]
use windows_sys::core::BOOL;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY, FlashWindowEx, GetForegroundWindow,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, MB_ICONERROR,
    MB_OK, MB_SETFOREGROUND, MessageBoxW, PostMessageW, SW_HIDE, SW_RESTORE, SW_SHOW,
    SetForegroundWindow, ShowWindow, WM_CLOSE,
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
mod dashboard_transfer_center {
    include!("dashboard/transfer_center.rs");
}
mod dashboard_window {
    include!("dashboard/window.rs");
}
mod dashboard_workflow {
    include!("dashboard/workflow.rs");
}

use dashboard_model::{AppMsg, DashboardApp, Tab, TOAST_ERROR_SECS, TOAST_SUCCESS_SECS};
use dashboard_task_runner::{
    DashboardTaskRunner, StartRoleReversalPairingTask, SubmitPairingCodeTask,
};
use dashboard_window::{
    DASHBOARD_WINDOW_TITLE, activate_existing_dashboard_window, find_existing_dashboard_window,
    hide_dashboard_window, native_window_handle_from_creation_context, request_dashboard_exit,
    show_dashboard_window, show_tray_startup_error,
};

pub(super) fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.start_service_elevated {
        return start_boundless_service_elevated_entrypoint();
    }
    let session_id = current_process_session_id().context("failed to resolve tray session id")?;
    let user_sid = current_user_sid_string().context("failed to resolve tray user SID")?;
    if let Some(hwnd) = find_existing_dashboard_window(session_id, &user_sid)? {
        if !activate_existing_dashboard_window(hwnd) {
            show_tray_startup_error("The existing Boundless tray could not be brought forward. Close it from Task Manager, then launch Boundless again.");
            anyhow::bail!("existing legacy tray dashboard could not be shown");
        }
        eprintln!("boundless_tray_single_instance=legacy_existing_activated");
        return Ok(());
    }
    let event_name = tray_single_instance_event_name(&user_sid, session_id);
    let acquisition = match SingleInstanceGuard::acquire(&event_name, &user_sid) {
        Ok(acquisition) => acquisition,
        Err(error) => {
            show_tray_startup_error(
                "Boundless could not establish tray ownership for this Windows session. Close any existing boundlesstray.exe process, then launch Boundless again.",
            );
            return Err(error);
        }
    };
    let single_instance_guard = match acquisition {
        SingleInstanceAcquire::Primary(guard) => guard,
        SingleInstanceAcquire::ExistingSignaled => {
            let Some(hwnd) = find_existing_dashboard_window_with_retry(
                session_id,
                &user_sid,
                Duration::from_secs(3),
            )? else {
                show_tray_startup_error("Boundless is already starting or running, but its dashboard did not become available. Close boundlesstray.exe from Task Manager, then launch Boundless again.");
                anyhow::bail!("existing tray accepted activation but its dashboard window was unavailable");
            };
            if !activate_existing_dashboard_window(hwnd) {
                show_tray_startup_error("The existing Boundless dashboard could not be brought forward. Close it from Task Manager, then launch Boundless again.");
                anyhow::bail!("existing tray dashboard could not be shown");
            }
            eprintln!("boundless_tray_single_instance=existing_activated");
            return Ok(());
        }
    };
    let ctx = Arc::new(AppContext {
        endpoint: cli.endpoint,
        start_daemon: cli.start_daemon,
        daemon_candidates: resolve_boundlessd_candidates(std::env::current_exe().ok()),
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_visible(false)
            .with_inner_size([800.0, 600.0])
            .with_resizable(false)
            .with_icon(make_window_icon()?)
            .with_title(DASHBOARD_WINDOW_TITLE),
        ..Default::default()
    };

    eframe::run_native(
        DASHBOARD_WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(DashboardApp::new(
                cc,
                ctx,
                single_instance_guard,
            )?))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {:?}", e))
}

fn find_existing_dashboard_window_with_retry(
    session_id: u32,
    user_sid: &str,
    timeout: Duration,
) -> Result<Option<HWND>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(hwnd) = find_existing_dashboard_window(session_id, user_sid)? {
            return Ok(Some(hwnd));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn tray_single_instance_event_name(user_sid: &str, session_id: u32) -> String {
    format!(
        "{TRAY_SINGLE_INSTANCE_EVENT_PREFIX}.{user_sid}.{session_id}"
    )
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
        endpoint_candidates: peer.endpoint_candidates.clone(),
    })
}

fn guided_flow_from_manual_input(host: &str, port_text: &str) -> Result<GuidedPairingFlow> {
    Ok(GuidedPairingFlow {
        dialog_title: format!("Manual Pair {}", host),
        host: host.to_string(),
        pairing_port: parse_pairing_port(port_text)?,
        default_alias: String::new(),
        orientation_selector_fallback: host.to_string(),
        endpoint_candidates: Vec::new(),
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

        if self.activation_requested.swap(false, Ordering::SeqCst) {
            show_dashboard_window(self.native_window_handle, ctx);
        }

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

        if self.pending_service_recovery_focus {
            show_dashboard_window(self.native_window_handle, ctx);
            self.pending_service_recovery_focus = false;
        }

        if self.exit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.render_pairing_dialog(&ctx);

        // ── Tick + render toast notifications as floating overlay ───────
        self.tick_toasts();
        if self.has_active_toasts() {
            // Keep repainting so success toasts auto-dismiss on time
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.render_toast_overlay(&ctx);

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Boundless");
                ui.separator();
                ui.selectable_value(&mut self.selected_tab, Tab::Status, "Status & Pairing");
                ui.selectable_value(&mut self.selected_tab, Tab::Layout, "Layout Manager");
                ui.selectable_value(&mut self.selected_tab, Tab::TransferCenter, "Transfer Center");
                ui.selectable_value(&mut self.selected_tab, Tab::Settings, "Settings");
            });
            ui.separator();

            self.render_service_recovery_banner(ui, &ctx);

            let pairing_modal_visible =
                self.pairing_in_progress || self.pairing_challenge.is_some();
            if !pairing_modal_visible && self.pairing_last_error.is_some() {
                self.render_pairing_error(ui, &ctx);
                ui.separator();
            }

            match self.selected_tab {
                Tab::Status => self.render_status_tab(ui, &ctx),
                Tab::Layout => self.render_layout_tab(ui, &ctx),
                Tab::TransferCenter => self.render_transfer_center_tab(ui),
                Tab::Settings => self.render_settings_tab(ui),
            }
        });
    }
}

impl DashboardApp {
    fn render_service_recovery_banner(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(recovery) = self.service_recovery.clone() else {
            return;
        };

        let mut start_requested = false;
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(64, 52, 20))
            .corner_radius(8.0)
            .inner_margin(12.0)
            .stroke(egui::Stroke::new(
                1.0_f32,
                egui::Color32::from_rgb(190, 145, 45),
            ))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("Boundless service needs attention")
                                .strong()
                                .color(egui::Color32::from_rgb(255, 220, 135)),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} (state={})",
                                recovery.offer.message, recovery.offer.state
                            ))
                            .color(egui::Color32::from_rgb(235, 225, 195)),
                        );
                    });
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let label = if recovery.in_progress {
                                "Starting..."
                            } else {
                                &recovery.offer.action_label
                            };
                            if ui
                                .add_enabled(!recovery.in_progress, egui::Button::new(label))
                                .clicked()
                            {
                                start_requested = true;
                            }
                        },
                    );
                });
            });
        ui.add_space(8.0);

        if start_requested {
            if let Some(active) = self.service_recovery.as_mut() {
                active.in_progress = true;
            }
            self.task_runner().recover_boundless_service(
                self.tx.clone(),
                self.ctx.endpoint.clone(),
                ctx.clone(),
            );
        }
    }

    fn render_toast_overlay(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }

        // Collect toast data so we can render without borrowing self
        let toast_data: Vec<(u64, String, bool, f32)> = self
            .toasts
            .iter()
            .map(|t| {
                let age = Instant::now().duration_since(t.created_at).as_secs_f32();
                let max_age = if t.is_error {
                    TOAST_ERROR_SECS as f32
                } else {
                    TOAST_SUCCESS_SECS as f32
                };
                // Fade out in the last 0.5s
                let alpha = ((max_age - age) / 0.5).clamp(0.0, 1.0);
                (t.id, t.message.clone(), t.is_error, alpha)
            })
            .collect();

        let mut dismissed_id = None;

        egui::Area::new(egui::Id::new("toast_overlay"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 8.0))
            .order(egui::Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_max_width(340.0);
                for (id, message, is_error, alpha) in &toast_data {
                    let (bg, border, text_color) = if *is_error {
                        (
                            egui::Color32::from_rgba_unmultiplied(100, 30, 30, (220.0 * alpha) as u8),
                            egui::Color32::from_rgba_unmultiplied(180, 60, 60, (200.0 * alpha) as u8),
                            egui::Color32::from_rgba_unmultiplied(255, 180, 180, (255.0 * alpha) as u8),
                        )
                    } else {
                        (
                            egui::Color32::from_rgba_unmultiplied(25, 80, 50, (220.0 * alpha) as u8),
                            egui::Color32::from_rgba_unmultiplied(50, 160, 90, (200.0 * alpha) as u8),
                            egui::Color32::from_rgba_unmultiplied(180, 255, 200, (255.0 * alpha) as u8),
                        )
                    };
                    egui::Frame::new()
                        .fill(bg)
                        .corner_radius(8.0)
                        .inner_margin(10.0)
                        .stroke(egui::Stroke::new(1.0_f32, border))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(message)
                                            .color(text_color)
                                            .size(12.5),
                                    )
                                    .wrap(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(egui::Button::new(
                                                egui::RichText::new("x")
                                                    .color(text_color)
                                                    .size(11.0),
                                            ).frame(false))
                                            .clicked()
                                        {
                                            dismissed_id = Some(*id);
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(4.0);
                }
            });

        if let Some(id) = dismissed_id {
            self.dismiss_toast(id);
        }
    }
}

/// Trim an ISO 8601 timestamp to a human-friendly `YYYY-MM-DD HH:MM:SS`.
/// Falls back to the original string if the format is unexpected.
fn format_timestamp(iso: &str) -> &str {
    // ISO 8601: "2026-04-12T22:57:03.840916700+00:00"
    //           ↑ 19 chars to get "2026-04-12T22:57:03"
    if iso.len() >= 19 && iso.as_bytes().get(10) == Some(&b'T') {
        &iso[..19]
    } else {
        iso
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
