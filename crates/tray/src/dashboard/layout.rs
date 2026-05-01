use super::*;

fn layout_peer_ids(peers: &[UiPairedPeer]) -> Vec<String> {
    let mut ids = peers
        .iter()
        .map(|peer| peer.peer_id.clone())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn should_rebuild_layout_model(
    layout_initialized: bool,
    machine_id: &str,
    layout_matrix: &str,
    last_layout_matrix: &str,
    current_peer_ids: &[String],
    last_layout_peer_ids: &[String],
    dragging_peer: bool,
) -> bool {
    (!layout_initialized && !machine_id.is_empty())
        || (layout_initialized
            && !dragging_peer
            && (layout_matrix != last_layout_matrix || current_peer_ids != last_layout_peer_ids))
}

impl DashboardApp {
    pub(super) fn render_layout_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let current_peer_ids = layout_peer_ids(&self.snapshot.paired_peers);
        if should_rebuild_layout_model(
            self.layout_initialized,
            &self.snapshot.machine_id,
            &self.snapshot.layout_matrix,
            &self.last_layout_matrix,
            &current_peer_ids,
            &self.last_layout_peer_ids,
            self.dragging_peer.is_some(),
        ) {
            self.layout_initialized = true;
            self.last_layout_matrix = self.snapshot.layout_matrix.clone();
            self.last_layout_peer_ids = current_peer_ids;
            self.layout_grid.clear();
            self.layout_unassigned.clear();

            let matrix_str = &self.snapshot.layout_matrix;
            let peers = &self.snapshot.paired_peers;
            let local_id = &self.snapshot.machine_id;

            if matrix_str.trim().is_empty() {
                self.layout_grid.insert((3, 3), local_id.clone());
                let mut left_x = 2;
                let mut right_x = 4;
                let mut toggle = true;
                for p in peers {
                    if toggle && left_x >= 0 {
                        self.layout_grid.insert((left_x, 3), p.peer_id.clone());
                        left_x -= 1;
                    } else if right_x < 7 {
                        self.layout_grid.insert((right_x, 3), p.peer_id.clone());
                        right_x += 1;
                    } else if left_x >= 0 {
                        self.layout_grid.insert((left_x, 3), p.peer_id.clone());
                        left_x -= 1;
                    }
                    toggle = !toggle;
                }
            } else {
                let rows: Vec<Vec<String>> = matrix_str
                    .split(';')
                    .map(|r| r.split(',').map(|s| s.trim().to_string()).collect())
                    .collect();
                let h = rows.len() as i32;
                let w = rows.iter().map(|r| r.len()).max().unwrap_or(0) as i32;
                let offset_x = (7 - w) / 2;
                let offset_y = (7 - h) / 2;

                for (y, row) in rows.iter().enumerate() {
                    for (x, token) in row.iter().enumerate() {
                        if token.is_empty() {
                            continue;
                        }
                        let peer_id = if shared_is_local_layout_token(token, local_id, None) {
                            local_id.clone()
                        } else if let Some(p) = resolve_layout_token_peer(token, peers) {
                            p.peer_id.clone()
                        } else {
                            token.clone()
                        };

                        let gx = x as i32 + offset_x;
                        let gy = y as i32 + offset_y;
                        if (0..7).contains(&gx) && (0..7).contains(&gy) {
                            self.layout_grid.insert((gx, gy), peer_id.clone());
                        } else {
                            self.layout_unassigned.push(peer_id.clone());
                        }
                    }
                }
            }

            let all_placed: Vec<String> = self.layout_grid.values().cloned().collect();
            for p in peers {
                if !all_placed.contains(&p.peer_id)
                    && !self.layout_unassigned.contains(&p.peer_id)
                {
                    self.layout_unassigned.push(p.peer_id.clone());
                }
            }
            if !all_placed.contains(local_id) && !self.layout_unassigned.contains(local_id) {
                self.layout_unassigned.push(local_id.clone());
            }
        }

        let local_id = self.snapshot.machine_id.clone();
        let paired_peers = self.snapshot.paired_peers.clone();

        let get_display_name = |id: &str| -> String {
            if id == local_id {
                return "This PC".to_string();
            }
            if let Some(p) = paired_peers.iter().find(|p| p.peer_id == id) {
                return p.display_name.clone();
            }
            short_token(id).to_string()
        };

        let is_peer_connected = |id: &str| -> bool {
            if id == local_id {
                return true; // local is always "connected"
            }
            paired_peers
                .iter()
                .find(|p| p.peer_id == id)
                .is_some_and(|p| p.connected)
        };

        ui.heading("Layout Manager");
        ui.label("Drag devices onto the grid to arrange your multi-machine layout.");
        ui.add_space(8.0);

        let mut drag_stopped = false;
        let mut pointer_pos_at_drop = None;
        let mut cell_rects = Vec::new();
        let mut context_action: Option<LayoutContextAction> = None;

        let cell_size = egui::vec2(120.0, 80.0);
        let mut new_grid = self.layout_grid.clone();
        let mut new_unassigned = self.layout_unassigned.clone();

        // ── Unassigned devices (only shown when non-empty) ──────────────
        if !self.layout_unassigned.is_empty() {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Unassigned Devices").strong());
                    ui.label(
                        egui::RichText::new("  Drag onto the grid to position")
                            .weak()
                            .italics(),
                    );
                });
                ui.add_space(4.0);
                let unassigned_snapshot: Vec<String> = self.layout_unassigned.clone();
                ui.horizontal_wrapped(|ui| {
                    for (i, peer_id) in unassigned_snapshot.iter().enumerate() {
                        let (rect, response) =
                            ui.allocate_exact_size(cell_size, egui::Sense::click_and_drag());

                        let is_being_dragged = self.dragging_peer.is_some() && response.dragged();

                        if response.drag_started() {
                            self.stash_layout_for_undo();
                            self.dragging_peer = Some((peer_id.clone(), (-1, i as i32)));
                            new_unassigned.remove(i);
                        }

                        if response.drag_stopped() {
                            drag_stopped = true;
                            pointer_pos_at_drop = ctx.pointer_interact_pos();
                        }

                        if !is_being_dragged {
                            let painter = ui.painter();
                            let connected = is_peer_connected(peer_id);
                            let bg = if connected {
                                egui::Color32::from_rgb(50, 60, 70)
                            } else {
                                egui::Color32::from_rgb(40, 42, 48)
                            };
                            painter.rect_filled(rect.shrink(4.0), 8.0, bg);
                            painter.rect_stroke(
                                rect.shrink(4.0),
                                8.0,
                                egui::Stroke::new(1.5, egui::Color32::from_rgb(80, 90, 100)),
                                egui::StrokeKind::Inside,
                            );
                            let text = get_display_name(peer_id);
                            let color = if peer_id == &local_id {
                                egui::Color32::LIGHT_BLUE
                            } else {
                                egui::Color32::WHITE
                            };
                            painter.text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                text,
                                egui::FontId::proportional(14.0),
                                color,
                            );
                            // Connection status dot
                            paint_status_dot(painter, rect, connected);
                        }

                        response.on_hover_text("Drag onto the grid to position this device");
                    }
                });
            });
            ui.add_space(12.0);
        }

        // ── Compute dynamic grid bounds ─────────────────────────────────
        let drag_origin = self.dragging_peer.as_ref().and_then(|(_, pos)| {
            if pos.0 >= 0 && pos.1 >= 0 {
                Some(*pos)
            } else {
                None
            }
        });
        let (view_x0, view_y0, view_x1, view_y1) =
            compute_visible_bounds(&self.layout_grid, drag_origin);

        let is_dragging_any = self.dragging_peer.is_some();

        // ── Render grid (scrollable for large layouts) ───────────────────
        egui::ScrollArea::both().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                egui::Frame::canvas(ui.style())
                    .fill(egui::Color32::from_rgb(20, 24, 28))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        ui.add_space(8.0);
                        for y in view_y0..=view_y1 {
                            ui.horizontal(|ui| {
                                ui.add_space(8.0);
                                for x in view_x0..=view_x1 {
                                    let (rect, response) = ui.allocate_exact_size(
                                        cell_size,
                                        egui::Sense::click_and_drag(),
                                    );
                                    cell_rects.push((rect, x, y));

                                    let has_device = self.layout_grid.contains_key(&(x, y));
                                    let is_hovered = response.hovered() && is_dragging_any;
                                    let is_being_dragged = is_dragging_any && response.dragged();

                                    if response.drag_started()
                                        && let Some(peer_id) =
                                            self.layout_grid.get(&(x, y)).cloned()
                                    {
                                        self.stash_layout_for_undo();
                                        self.dragging_peer = Some((peer_id, (x, y)));
                                        new_grid.remove(&(x, y));
                                    }

                                    if response.drag_stopped() {
                                        drag_stopped = true;
                                        pointer_pos_at_drop = ctx.pointer_interact_pos();
                                    }

                                    // ── Right-click context menu ──
                                    if has_device && !is_dragging_any {
                                        let cell_x = x;
                                        let cell_y = y;
                                        response.context_menu(|ui| {
                                            if let Some(peer_id) =
                                                self.layout_grid.get(&(cell_x, cell_y))
                                            {
                                                let name = get_display_name(peer_id);
                                                ui.label(
                                                    egui::RichText::new(&name)
                                                        .strong()
                                                        .size(13.0),
                                                );
                                                ui.separator();
                                                if peer_id != &local_id
                                                    && ui.button("Remove from grid").clicked()
                                                {
                                                    context_action =
                                                        Some(LayoutContextAction::RemoveFromGrid(
                                                            cell_x, cell_y,
                                                        ));
                                                    ui.close();
                                                }
                                                if peer_id == &local_id {
                                                    ui.label(
                                                        egui::RichText::new(
                                                            "This PC must stay on the grid",
                                                        )
                                                        .weak()
                                                        .italics()
                                                        .size(11.0),
                                                    );
                                                }
                                            }
                                        });
                                    }

                                    let painter = ui.painter();

                                    if is_hovered {
                                        // ── Prominent drop-target highlight ──
                                        painter.rect_filled(
                                            rect.shrink(2.0),
                                            6.0,
                                            egui::Color32::from_rgb(40, 90, 155),
                                        );
                                        painter.rect_stroke(
                                            rect.shrink(2.0),
                                            6.0,
                                            egui::Stroke::new(
                                                2.0,
                                                egui::Color32::from_rgb(80, 160, 240),
                                            ),
                                            egui::StrokeKind::Inside,
                                        );
                                    } else if has_device && !is_being_dragged {
                                        // ── Occupied device card ──
                                        if let Some(peer_id) = self.layout_grid.get(&(x, y)) {
                                            let is_local = peer_id == &local_id;
                                            let connected = is_peer_connected(peer_id);
                                            let bg = if is_local {
                                                egui::Color32::from_rgb(25, 65, 110)
                                            } else if connected {
                                                egui::Color32::from_rgb(48, 56, 66)
                                            } else {
                                                egui::Color32::from_rgb(38, 40, 46)
                                            };
                                            painter.rect_filled(rect.shrink(4.0), 8.0, bg);

                                            let border = if is_local {
                                                egui::Color32::from_rgb(70, 150, 220)
                                            } else if connected {
                                                egui::Color32::from_rgb(90, 100, 115)
                                            } else {
                                                egui::Color32::from_rgb(65, 65, 72)
                                            };
                                            painter.rect_stroke(
                                                rect.shrink(4.0),
                                                8.0,
                                                egui::Stroke::new(1.5, border),
                                                egui::StrokeKind::Inside,
                                            );

                                            let text = get_display_name(peer_id);
                                            let text_color = if is_local {
                                                egui::Color32::from_rgb(150, 210, 255)
                                            } else if connected {
                                                egui::Color32::WHITE
                                            } else {
                                                egui::Color32::from_rgb(140, 140, 150)
                                            };
                                            painter.text(
                                                rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                &text,
                                                egui::FontId::proportional(14.0),
                                                text_color,
                                            );

                                            // Connection status dot
                                            paint_status_dot(painter, rect, connected);

                                            // Tooltip
                                            let status_str = if is_local {
                                                "This PC"
                                            } else if connected {
                                                "Connected"
                                            } else {
                                                "Offline"
                                            };
                                            response.on_hover_text(format!(
                                                "{text} - {status_str}\nDrag to reposition | Right-click for options"
                                            ));
                                        }
                                    } else {
                                        // ── Empty cell ──
                                        painter.rect_stroke(
                                            rect.shrink(3.0),
                                            6.0,
                                            egui::Stroke::new(
                                                1.0,
                                                egui::Color32::from_rgb(50, 58, 66),
                                            ),
                                            egui::StrokeKind::Inside,
                                        );
                                        // Show "+" hint in empty cells while user is dragging
                                        if is_dragging_any {
                                            painter.text(
                                                rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                "+",
                                                egui::FontId::proportional(18.0),
                                                egui::Color32::from_rgb(75, 85, 95),
                                            );
                                        }
                                    }
                                }
                                ui.add_space(8.0);
                            });
                        }
                        ui.add_space(8.0);
                    });
            });
        });

        // ── Fallback drag release detection ────────────────────────────
        // If the user releases the mouse outside any allocated cell rect,
        // egui never fires `drag_stopped` on those responses. Detect the
        // primary-button release globally and treat it as a drop so the
        // ghost doesn't stick to the cursor forever.
        if !drag_stopped
            && self.dragging_peer.is_some()
            && ctx.input(|i| i.pointer.any_released())
        {
            drag_stopped = true;
            pointer_pos_at_drop = ctx.pointer_interact_pos();
        }

        // ── Context menu actions ───────────────────────────────────────
        if let Some(action) = context_action {
            match action {
                LayoutContextAction::RemoveFromGrid(cx, cy) => {
                    self.stash_layout_for_undo();
                    if let Some(peer_id) = new_grid.remove(&(cx, cy)) {
                        new_unassigned.push(peer_id);
                    }
                }
            }
        }

        let dropped_file_paths = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.as_ref())
                .filter(|path| path.is_file())
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
        });
        if !dropped_file_paths.is_empty() && let Some(pos) = ctx.pointer_hover_pos() {
            let mut target_peer_id = None;
            for (rect, x, y) in &cell_rects {
                if rect.contains(pos)
                    && let Some(peer_id) = self.layout_grid.get(&(*x, *y)).cloned()
                {
                    target_peer_id = Some(peer_id);
                    break;
                }
            }

            match target_peer_id {
                Some(peer_id) if peer_id == local_id => {
                    self.push_toast("Drop files on a connected peer to send them".to_string(), true);
                }
                Some(peer_id) if is_peer_connected(&peer_id) => {
                    self.task_runner().send_files_to_peer(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        peer_id,
                        dropped_file_paths,
                    );
                }
                Some(peer_id) => {
                    self.push_toast(
                        format!("{} is offline; connect it before sending files", get_display_name(&peer_id)),
                        true,
                    );
                }
                None => {
                    self.push_toast(
                        "Drop files directly on a connected peer tile to send them".to_string(),
                        true,
                    );
                }
            }
        }

        // ── Drag-drop resolution ────────────────────────────────────────
        if drag_stopped && let Some((peer_id, old_pos)) = self.dragging_peer.take() {
            if let Some(pos) = pointer_pos_at_drop {
                let mut dropped_in_cell = None;
                for (rect, x, y) in &cell_rects {
                    if rect.contains(pos) {
                        dropped_in_cell = Some((*x, *y));
                        break;
                    }
                }

                if let Some(new_pos) = dropped_in_cell {
                    if let Some(occupant) = self.layout_grid.get(&new_pos).cloned() {
                        if old_pos.0 == -1 {
                            new_unassigned.insert(old_pos.1 as usize, occupant);
                        } else {
                            new_grid.insert(old_pos, occupant);
                        }
                    }
                    new_grid.insert(new_pos, peer_id);
                } else if old_pos.0 != -1 {
                    new_unassigned.push(peer_id);
                } else {
                    new_unassigned.insert(old_pos.1 as usize, peer_id);
                }
            } else if old_pos.0 != -1 {
                new_grid.insert(old_pos, peer_id);
            } else {
                new_unassigned.insert(old_pos.1 as usize, peer_id);
            }
        }

        self.layout_grid = new_grid;
        self.layout_unassigned = new_unassigned;

        // ── Drag ghost overlay ──────────────────────────────────────────
        if let Some((peer_id, _)) = &self.dragging_peer && let Some(pos) = ctx.pointer_hover_pos()
        {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("drag_layer"),
            ));
            let rect = egui::Rect::from_center_size(pos, cell_size);
            let is_local = peer_id == &local_id;
            let bg_color = if is_local {
                egui::Color32::from_rgb(35, 80, 140)
            } else {
                egui::Color32::from_rgb(65, 75, 90)
            };
            painter.rect_filled(rect.shrink(4.0), 8.0, bg_color);
            painter.rect_stroke(
                rect.shrink(4.0),
                8.0,
                egui::Stroke::new(2.0, egui::Color32::WHITE),
                egui::StrokeKind::Inside,
            );
            let text = get_display_name(peer_id);
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(14.0),
                egui::Color32::WHITE,
            );
        }

        // ── Apply confirmation dialog ──────────────────────────────────
        if self.confirm_apply_pending {
            self.render_apply_confirmation(ctx);
        }

        // ── Action buttons ──────────────────────────────────────────────
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let apply_btn = ui.add(
                egui::Button::new("Apply Layout")
            );
            if apply_btn.clicked() {
                let matrix_str =
                    serialize_layout_matrix(&self.layout_grid, &self.snapshot.machine_id);
                let peers = self
                    .snapshot
                    .paired_peers
                    .iter()
                    .map(|peer| LayoutPeerToken {
                        peer_id: peer.peer_id.clone(),
                        display_name: peer.display_name.clone(),
                    })
                    .collect::<Vec<_>>();
                match validate_layout_matrix_spec(
                    &matrix_str,
                    &self.snapshot.machine_id,
                    None,
                    &peers,
                ) {
                    Ok(()) => {
                        self.confirm_apply_pending = true;
                    }
                    Err(error) => {
                        self.push_toast(error.to_string(), true);
                    }
                }
            }
            apply_btn.on_hover_text("Send this layout to the daemon for immediate use");

            let reset_btn = ui.button("Reset Layout");
            if reset_btn.clicked() {
                self.stash_layout_for_undo();
                self.layout_initialized = false;
                self.snapshot.layout_matrix = String::new();
            }
            reset_btn.on_hover_text("Discard changes and reload from daemon");

            // Undo button
            if self.prev_layout_grid.is_some() {
                let undo_btn = ui.button("Undo");
                if undo_btn.clicked() {
                    self.undo_layout();
                }
                undo_btn.on_hover_text("Revert the last layout change");
            }
        });

        // ── Human-readable layout summary (replaces raw matrix debug) ──
        ui.add_space(8.0);
        if !self.layout_grid.is_empty() {
            let summary = build_layout_summary(
                &self.layout_grid,
                &self.snapshot.machine_id,
                &self.snapshot.paired_peers,
            );
            ui.label(egui::RichText::new(summary).weak().size(12.0));
        }
    }

    fn render_apply_confirmation(&mut self, ctx: &egui::Context) {
        let mut open = true;
        egui::Window::new("Confirm Layout")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .min_width(380.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Apply this layout to the daemon? Active connections will be reconfigured.");
                ui.add_space(8.0);

                // Show preview summary
                let summary = build_layout_summary(
                    &self.layout_grid,
                    &self.snapshot.machine_id,
                    &self.snapshot.paired_peers,
                );
                ui.label(egui::RichText::new(&summary).strong().size(13.0));
                ui.add_space(12.0);

                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        let matrix_str =
                            serialize_layout_matrix(&self.layout_grid, &self.snapshot.machine_id);
                        self.task_runner().apply_layout(
                            self.tx.clone(),
                            self.ctx.endpoint.clone(),
                            matrix_str,
                        );
                        self.confirm_apply_pending = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_apply_pending = false;
                    }
                });
            });
        if !open {
            self.confirm_apply_pending = false;
        }
    }
}

// ── Layout context menu actions ────────────────────────────────────────
enum LayoutContextAction {
    RemoveFromGrid(i32, i32),
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Paint a small colored status dot in the top-right corner of a cell.
fn paint_status_dot(painter: &egui::Painter, rect: egui::Rect, connected: bool) {
    let dot_center = egui::pos2(rect.right() - 12.0, rect.top() + 12.0);
    let dot_color = if connected {
        egui::Color32::from_rgb(60, 200, 90)
    } else {
        egui::Color32::from_rgb(100, 100, 110)
    };
    painter.circle_filled(dot_center, 4.0, dot_color);
}

/// Compute the visible grid region: bounding box of placed devices plus
/// one cell of padding on each side, clamped to [0, 6] and at least 3x3.
pub(super) fn compute_visible_bounds(
    grid: &HashMap<(i32, i32), String>,
    drag_origin: Option<(i32, i32)>,
) -> (i32, i32, i32, i32) {
    if grid.is_empty() && drag_origin.is_none() {
        return (2, 2, 4, 4);
    }

    let (mut min_x, mut max_x, mut min_y, mut max_y) = if let Some((x, y)) = drag_origin {
        (x, x, y, y)
    } else {
        (i32::MAX, i32::MIN, i32::MAX, i32::MIN)
    };
    for &(x, y) in grid.keys() {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }

    // Pad by one cell for expansion drop targets
    let mut x0 = (min_x - 1).max(0);
    let mut y0 = (min_y - 1).max(0);
    let mut x1 = (max_x + 1).min(6);
    let mut y1 = (max_y + 1).min(6);

    // Enforce minimum 3 cells in each dimension
    while x1 - x0 < 2 {
        if x0 > 0 {
            x0 -= 1;
        }
        if x1 - x0 < 2 && x1 < 6 {
            x1 += 1;
        }
    }
    while y1 - y0 < 2 {
        if y0 > 0 {
            y0 -= 1;
        }
        if y1 - y0 < 2 && y1 < 6 {
            y1 += 1;
        }
    }

    (x0, y0, x1, y1)
}

/// Build a human-readable summary like:
///   Layout: Office Desktop (left) -- This PC -- Living Room (right)
fn build_layout_summary(
    grid: &HashMap<(i32, i32), String>,
    local_id: &str,
    peers: &[UiPairedPeer],
) -> String {
    if grid.is_empty() {
        return "No devices placed.".to_string();
    }

    let name_of = |id: &str| -> String {
        if id == local_id {
            return "This PC".to_string();
        }
        if let Some(p) = peers.iter().find(|p| p.peer_id == id) {
            return p.display_name.clone();
        }
        short_token(id).to_string()
    };

    let local_pos = grid
        .iter()
        .find(|(_, id)| id.as_str() == local_id)
        .map(|(pos, _)| *pos);

    let Some((lx, ly)) = local_pos else {
        return format!("{} device(s) placed", grid.len());
    };

    let mut parts = Vec::new();
    if let Some(id) = grid.get(&(lx - 1, ly)) {
        parts.push(format!("{} (left)", name_of(id)));
    }
    parts.push(name_of(local_id));
    if let Some(id) = grid.get(&(lx + 1, ly)) {
        parts.push(format!("{} (right)", name_of(id)));
    }

    let mut extras = Vec::new();
    if let Some(id) = grid.get(&(lx, ly - 1)) {
        extras.push(format!("{} (above)", name_of(id)));
    }
    if let Some(id) = grid.get(&(lx, ly + 1)) {
        extras.push(format!("{} (below)", name_of(id)));
    }

    let line = parts.join("  --  ");
    if extras.is_empty() {
        format!("Layout: {line}")
    } else {
        format!("Layout: {line}  |  {}", extras.join(", "))
    }
}

fn resolve_layout_token_peer<'a>(
    token: &str,
    peers: &'a [UiPairedPeer],
) -> Option<&'a UiPairedPeer> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let token_lower = token.to_ascii_lowercase();
    let matches = peers
        .iter()
        .filter(|peer| {
            peer.peer_id.eq_ignore_ascii_case(token)
                || peer.peer_id.to_ascii_lowercase().starts_with(&token_lower)
                || peer.display_name.eq_ignore_ascii_case(token)
        })
        .collect::<Vec<_>>();

    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_layout_token_peer, should_rebuild_layout_model};
    use crate::windows_app::UiPairedPeer;

    #[test]
    fn rebuilds_when_paired_peer_ids_change_without_layout_matrix_change() {
        let current_peer_ids = vec!["peer-a".to_string(), "peer-b".to_string()];
        let last_peer_ids = vec!["peer-a".to_string()];

        assert!(should_rebuild_layout_model(
            true,
            "local-machine",
            "self",
            "self",
            &current_peer_ids,
            &last_peer_ids,
            false,
        ));
    }

    #[test]
    fn defers_rebuild_while_dragging_even_if_peer_ids_change() {
        let current_peer_ids = vec!["peer-a".to_string(), "peer-b".to_string()];
        let last_peer_ids = vec!["peer-a".to_string()];

        assert!(!should_rebuild_layout_model(
            true,
            "local-machine",
            "self",
            "self",
            &current_peer_ids,
            &last_peer_ids,
            true,
        ));
    }

    #[test]
    fn resolves_layout_token_peer_with_shared_grammar() {
        let peers = vec![
            UiPairedPeer {
                peer_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
                display_name: "Office".to_string(),
                address: "127.0.0.1:15100".to_string(),
                connected: true,
                health_state: "connected".to_string(),
                health_reason: "connected".to_string(),
            },
            UiPairedPeer {
                peer_id: "11111111-2222-3333-4444-555555555555".to_string(),
                display_name: "Laptop".to_string(),
                address: "127.0.0.1:15101".to_string(),
                connected: false,
                health_state: "disconnected".to_string(),
                health_reason: "no recent peer event".to_string(),
            },
        ];

        assert_eq!(
            resolve_layout_token_peer("AAAAAAAA", &peers).map(|peer| peer.peer_id.as_str()),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert_eq!(
            resolve_layout_token_peer("office", &peers).map(|peer| peer.peer_id.as_str()),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert_eq!(
            resolve_layout_token_peer("missing", &peers).map(|peer| peer.peer_id.as_str()),
            None
        );
    }
}
