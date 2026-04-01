use super::*;

impl DashboardApp {
    pub(super) fn render_layout_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if (!self.layout_initialized && !self.snapshot.machine_id.is_empty())
            || (self.layout_initialized
                && self.snapshot.layout_matrix != self.last_layout_matrix
                && self.dragging_peer.is_none())
        {
            self.layout_initialized = true;
            self.last_layout_matrix = self.snapshot.layout_matrix.clone();
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
                        } else if let Some(p) = peers
                            .iter()
                            .find(|p| p.display_name == *token || p.peer_id == *token)
                        {
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

        let get_display_name = |id: &str| -> String {
            if id == self.snapshot.machine_id {
                return "This PC".to_string();
            }
            if let Some(p) = self.snapshot.paired_peers.iter().find(|p| p.peer_id == id) {
                return p.display_name.clone();
            }
            short_token(id).to_string()
        };

        ui.heading("Visual Layout Manager");
        ui.label("Drag and drop devices onto the grid to configure your layout.");
        ui.add_space(8.0);

        let mut drag_stopped = false;
        let mut pointer_pos_at_drop = None;
        let mut cell_rects = Vec::new();

        let cell_size = egui::vec2(90.0, 60.0);
        let mut new_grid = self.layout_grid.clone();
        let mut new_unassigned = self.layout_unassigned.clone();

        ui.group(|ui| {
            ui.label("Unassigned Devices");
            ui.horizontal_wrapped(|ui| {
                if self.layout_unassigned.is_empty() {
                    ui.label(egui::RichText::new("None").italics());
                }
                for (i, peer_id) in self.layout_unassigned.iter().enumerate() {
                    let (rect, response) =
                        ui.allocate_exact_size(cell_size, egui::Sense::click_and_drag());

                    let is_being_dragged = self.dragging_peer.is_some() && response.dragged();

                    if response.drag_started() {
                        self.dragging_peer = Some((peer_id.clone(), (-1, i as i32)));
                        new_unassigned.remove(i);
                    }

                    if response.drag_stopped() {
                        drag_stopped = true;
                        pointer_pos_at_drop = ctx.pointer_interact_pos();
                    }

                    let painter = ui.painter();
                    if !is_being_dragged {
                        painter.rect_filled(
                            rect.shrink(4.0),
                            6.0,
                            egui::Color32::from_rgb(50, 60, 70),
                        );
                        painter.rect_stroke(
                            rect.shrink(4.0),
                            6.0,
                            egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
                            egui::StrokeKind::Inside,
                        );
                        let text = get_display_name(peer_id);
                        let color = if peer_id == &self.snapshot.machine_id {
                            egui::Color32::LIGHT_BLUE
                        } else {
                            egui::Color32::WHITE
                        };
                        let mut job = egui::text::LayoutJob::simple(
                            text,
                            egui::FontId::proportional(12.0),
                            color,
                            rect.width() - 8.0,
                        );
                        job.halign = egui::Align::Center;
                        let galley = painter.layout_job(job);
                        painter.galley(rect.center() - galley.size() / 2.0, galley, color);
                    }
                }
            });
        });

        ui.add_space(16.0);

        ui.vertical_centered(|ui| {
            egui::Frame::canvas(ui.style())
                .fill(egui::Color32::from_rgb(25, 30, 35))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                    for y in 0..7 {
                        ui.horizontal(|ui| {
                            for x in 0..7 {
                                let (rect, response) =
                                    ui.allocate_exact_size(cell_size, egui::Sense::click_and_drag());
                                cell_rects.push((rect, x, y));

                                let is_hovered =
                                    response.hovered() && self.dragging_peer.is_some();
                                let is_being_dragged =
                                    self.dragging_peer.is_some() && response.dragged();

                                if response.drag_started()
                                    && let Some(peer_id) = self.layout_grid.get(&(x, y))
                                {
                                    self.dragging_peer = Some((peer_id.clone(), (x, y)));
                                    new_grid.remove(&(x, y));
                                }

                                if response.drag_stopped() {
                                    drag_stopped = true;
                                    pointer_pos_at_drop = ctx.pointer_interact_pos();
                                }

                                let painter = ui.painter();
                                if is_hovered {
                                    painter.rect_filled(
                                        rect.shrink(2.0),
                                        4.0,
                                        egui::Color32::from_rgb(40, 50, 60),
                                    );
                                }
                                painter.rect_stroke(
                                    rect.shrink(2.0),
                                    4.0,
                                    egui::Stroke::new(
                                        1.0,
                                        egui::Color32::from_rgb(45, 50, 55),
                                    ),
                                    egui::StrokeKind::Inside,
                                );

                                if let Some(peer_id) = self.layout_grid.get(&(x, y))
                                    && !is_being_dragged
                                {
                                    let is_local = peer_id == &self.snapshot.machine_id;
                                    let bg_color = if is_local {
                                        egui::Color32::from_rgb(30, 70, 110)
                                    } else {
                                        egui::Color32::from_rgb(50, 60, 70)
                                    };
                                    painter.rect_filled(rect.shrink(4.0), 6.0, bg_color);
                                    let border_color = if is_local {
                                        egui::Color32::LIGHT_BLUE
                                    } else {
                                        egui::Color32::DARK_GRAY
                                    };
                                    painter.rect_stroke(
                                        rect.shrink(4.0),
                                        6.0,
                                        egui::Stroke::new(1.5, border_color),
                                        egui::StrokeKind::Inside,
                                    );
                                    let text = get_display_name(peer_id);
                                    let mut job = egui::text::LayoutJob::simple(
                                        text,
                                        egui::FontId::proportional(12.0),
                                        egui::Color32::WHITE,
                                        rect.width() - 8.0,
                                    );
                                    job.halign = egui::Align::Center;
                                    let galley = painter.layout_job(job);
                                    painter.galley(
                                        rect.center() - galley.size() / 2.0,
                                        galley,
                                        egui::Color32::WHITE,
                                    );
                                }
                            }
                        });
                    }
                });
        });

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

        if let Some((peer_id, _)) = &self.dragging_peer && let Some(pos) = ctx.pointer_hover_pos()
        {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("drag_layer"),
            ));
            let rect = egui::Rect::from_center_size(pos, cell_size);
            let is_local = peer_id == &self.snapshot.machine_id;
            let bg_color = if is_local {
                egui::Color32::from_rgb(40, 90, 140)
            } else {
                egui::Color32::from_rgb(70, 80, 90)
            };
            painter.rect_filled(rect.shrink(4.0), 6.0, bg_color);
            painter.rect_stroke(
                rect.shrink(4.0),
                6.0,
                egui::Stroke::new(2.0, egui::Color32::WHITE),
                egui::StrokeKind::Inside,
            );
            let text = get_display_name(peer_id);
            let mut job = egui::text::LayoutJob::simple(
                text,
                egui::FontId::proportional(12.0),
                egui::Color32::WHITE,
                rect.width() - 8.0,
            );
            job.halign = egui::Align::Center;
            let galley = painter.layout_job(job);
            painter.galley(rect.center() - galley.size() / 2.0, galley, egui::Color32::WHITE);
        }

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if ui.button("Apply Layout").clicked() {
                match validate_layout_before_apply(&self.layout_grid, &self.snapshot.machine_id) {
                    Ok(()) => {
                        let matrix_str =
                            serialize_layout_matrix(&self.layout_grid, &self.snapshot.machine_id);
                        self.task_runner().apply_layout(
                            self.tx.clone(),
                            self.ctx.endpoint.clone(),
                            matrix_str,
                        );
                    }
                    Err(error) => {
                        self.last_error = Some(error.to_string());
                        self.last_message_is_error = true;
                    }
                }
            }

            if ui.button("Reset Layout").clicked() {
                self.layout_initialized = false;
                self.snapshot.layout_matrix = String::new();
            }
        });

        ui.add_space(8.0);
        ui.label(format!("Current Matrix: {}", self.snapshot.layout_matrix));
    }
}
