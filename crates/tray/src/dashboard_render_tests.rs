// This harness never constructs eframe, a native window, or DashboardApp::new.
// test_app supplies a recording command runner; fixture UI cannot access IPC,
// start a broker, open a file dialog, or mutate a real Boundless installation.
use super::dashboard_test_support::{
    sample_discovered_peer, sample_first_run_snapshot, sample_paired_snapshot, test_app,
};
use super::*;
use egui::epaint::{ImageData, Primitive, Shape, Vertex};
use image::{Rgba, RgbaImage};
use std::path::PathBuf;

const ARTIFACT_DIR: &str = "BOUNDLESS_UI_ARTIFACT_DIR";

struct DashboardHarness {
    app: DashboardApp,
    context: egui::Context,
    textures: HashMap<egui::TextureId, egui::ColorImage>,
    size: [u32; 2],
    native_pixels_per_point: f32,
    frame_number: u32,
    render_duration: Duration,
    text: Vec<(String, egui::Rect)>,
    output: egui::FullOutput,
}

impl DashboardHarness {
    fn new(snapshot: UiSnapshot, tab: Tab, size: [u32; 2]) -> Self {
        let mut app = test_app();
        app.apply_app_msg(AppMsg::SnapshotUpdated(Box::new(snapshot)));
        app.selected_tab = tab;
        let context = egui::Context::default();
        context.set_visuals(egui::Visuals::dark());
        configure_dashboard_style(&context);
        Self {
            app,
            context,
            textures: HashMap::new(),
            size,
            native_pixels_per_point: 1.0,
            frame_number: 0,
            render_duration: Duration::ZERO,
            text: Vec::new(),
            output: egui::FullOutput::default(),
        }
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        for id in &self.output.textures_delta.free {
            self.textures.remove(id);
        }
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(self.size[0] as f32, self.size[1] as f32),
            )),
            time: Some(f64::from(self.frame_number) / 20.0),
            events,
            ..Default::default()
        };
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .expect("root viewport exists")
            .native_pixels_per_point = Some(self.native_pixels_per_point);
        self.frame_number += 1;
        // This is the same root-Ui entry point that eframe invokes, without
        // its native integration or the application's runtime logic callback.
        let started = Instant::now();
        self.output = self.context.run_ui(input, |ui| self.app.render_content(ui));
        self.render_duration += started.elapsed();
        self.text.clear();
        for clipped in &self.output.shapes {
            collect_text(&clipped.shape, clipped.clip_rect, &mut self.text);
        }
        for (id, delta) in &self.output.textures_delta.set {
            let ImageData::Color(image) = &delta.image;
            if let Some([x, y]) = delta.pos {
                let texture = self
                    .textures
                    .get_mut(id)
                    .expect("partial texture update must follow allocation");
                assert!(x + image.size[0] <= texture.size[0]);
                assert!(y + image.size[1] <= texture.size[1]);
                for row in 0..image.size[1] {
                    let start = (y + row) * texture.size[0] + x;
                    let source = row * image.size[0];
                    texture.pixels[start..start + image.size[0]]
                        .copy_from_slice(&image.pixels[source..source + image.size[0]]);
                }
            } else {
                self.textures.insert(*id, image.as_ref().clone());
            }
        }
        // Texture frees are deferred until the next frame: egui explicitly
        // allows this frame's meshes to refer to a texture being released.
    }

    fn settle(&mut self) {
        for _ in 0..3 {
            self.frame(Vec::new());
        }
    }

    fn assert_text(&self, expected: &str) {
        assert!(
            self.text.iter().any(|(text, _)| text.contains(expected)),
            "expected visible text {expected:?}; rendered text: {:?}",
            self.text
        );
    }

    fn click(&mut self, label: &str) {
        self.settle();
        let (_, rect) = self
            .text
            .iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("no visible control {label:?}; text: {:?}", self.text));
        let pos = rect.center();
        self.frame(vec![egui::Event::PointerMoved(pos)]);
        self.frame(vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        self.frame(vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        self.settle();
    }

    fn scroll_to_bottom(&mut self) {
        self.frame(vec![
            egui::Event::PointerMoved(egui::pos2(
                self.size[0] as f32 / 2.0,
                self.size[1] as f32 / 2.0,
            )),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -2000.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        for _ in 0..10 {
            self.frame(Vec::new());
        }
    }

    fn image(&self) -> RgbaImage {
        let physical_size = self
            .size
            .map(|points| (points as f32 * self.output.pixels_per_point).round() as u32);
        let mut image =
            RgbaImage::from_pixel(physical_size[0], physical_size[1], Rgba([0, 0, 0, 255]));
        for primitive in self
            .context
            .tessellate(self.output.shapes.clone(), self.output.pixels_per_point)
        {
            let Primitive::Mesh(mesh) = primitive.primitive else {
                panic!("offscreen renderer does not support native paint callbacks");
            };
            let texture = self
                .textures
                .get(&mesh.texture_id)
                .expect("every painted mesh must have a texture");
            let clip = primitive.clip_rect * self.output.pixels_per_point;
            for triangle in mesh.indices.chunks_exact(3) {
                let vertices = [triangle[0], triangle[1], triangle[2]]
                    .map(|index| mesh.vertices[index as usize]);
                paint_triangle(
                    &mut image,
                    vertices,
                    texture,
                    clip,
                    self.output.pixels_per_point,
                );
            }
        }
        image
    }

    fn save_if_requested(&self, name: &str) {
        let Some(directory) = std::env::var_os(ARTIFACT_DIR).filter(|value| !value.is_empty())
        else {
            return;
        };
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("create requested UI artifact directory");
        let capture_started = Instant::now();
        let image = self.image();
        let path = directory.join(format!("{name}.png"));
        image.save(&path).expect("save actual egui render as PNG");
        // This companion file makes cropped or missing controls inspectable
        // without OCR and contains only deterministic, synthetic fixture data.
        let text = self
            .text
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(directory.join(format!("{name}.txt")), text)
            .expect("save fixture's visible text");
        let timing = serde_json::json!({
            "fixture": name,
            "width": image.width(),
            "height": image.height(),
            "viewport_points": self.size,
            "pixels_per_point": self.output.pixels_per_point,
            "dark_mode": self.context.global_style().visuals.dark_mode,
            "egui_frame_count": self.frame_number,
            "egui_frames_elapsed_ms": self.render_duration.as_secs_f64() * 1000.0,
            "software_capture_elapsed_ms": capture_started.elapsed().as_secs_f64() * 1000.0,
        });
        std::fs::write(
            directory.join(format!("{name}.json")),
            serde_json::to_string_pretty(&timing).expect("serialize fixture timing"),
        )
        .expect("save fixture capture timing");
        eprintln!("dashboard_render={}", path.display());
    }
}

fn collect_text(shape: &Shape, clip: egui::Rect, text: &mut Vec<(String, egui::Rect)>) {
    match shape {
        Shape::Text(shape) => {
            let visible = shape.visual_bounding_rect().intersect(clip);
            if visible.is_positive() {
                text.push((shape.galley.job.text.clone(), visible));
            }
        }
        Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, clip, text);
            }
        }
        _ => {}
    }
}

fn edge(a: egui::Pos2, b: egui::Pos2, point: egui::Pos2) -> f32 {
    (b.x - a.x) * (point.y - a.y) - (b.y - a.y) * (point.x - a.x)
}

fn top_left(a: egui::Pos2, b: egui::Pos2) -> bool {
    b.y < a.y || (b.y == a.y && b.x > a.x)
}

fn sample_texture(texture: &egui::ColorImage, uv: egui::Pos2) -> [f32; 4] {
    // The egui font atlas uses linear filtering, with texel centers at n + 0.5.
    let x =
        (uv.x * texture.size[0] as f32 - 0.5).clamp(0.0, texture.size[0].saturating_sub(1) as f32);
    let y =
        (uv.y * texture.size[1] as f32 - 0.5).clamp(0.0, texture.size[1].saturating_sub(1) as f32);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(texture.size[0] - 1);
    let y1 = (y0 + 1).min(texture.size[1] - 1);
    let dx = x.fract();
    let dy = y.fract();
    let mut sampled = [0.0; 4];
    for (x, y, weight) in [
        (x0, y0, (1.0 - dx) * (1.0 - dy)),
        (x1, y0, dx * (1.0 - dy)),
        (x0, y1, (1.0 - dx) * dy),
        (x1, y1, dx * dy),
    ] {
        let texel = texture.pixels[y * texture.size[0] + x].to_array();
        for (channel, value) in sampled.iter_mut().zip(texel) {
            *channel += f32::from(value) * weight / 255.0;
        }
    }
    sampled
}

fn paint_triangle(
    image: &mut RgbaImage,
    mut vertices: [Vertex; 3],
    texture: &egui::ColorImage,
    clip: egui::Rect,
    pixels_per_point: f32,
) {
    for vertex in &mut vertices {
        vertex.pos = egui::pos2(
            vertex.pos.x * pixels_per_point,
            vertex.pos.y * pixels_per_point,
        );
    }
    let mut area = edge(vertices[0].pos, vertices[1].pos, vertices[2].pos);
    if area.abs() < f32::EPSILON {
        return;
    }
    if area < 0.0 {
        vertices.swap(1, 2);
        area = -area;
    }
    let [a, b, c] = vertices;
    let bounds = egui::Rect::from_min_max(a.pos.min(b.pos).min(c.pos), a.pos.max(b.pos).max(c.pos))
        .intersect(clip)
        .intersect(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(image.width() as f32, image.height() as f32),
        ));
    if !bounds.is_positive() {
        return;
    }
    let colors = vertices.map(|vertex| vertex.color.to_array());
    let included_edges = [
        top_left(b.pos, c.pos),
        top_left(c.pos, a.pos),
        top_left(a.pos, b.pos),
    ];
    for y in bounds.min.y.floor() as u32..bounds.max.y.ceil() as u32 {
        for x in bounds.min.x.floor() as u32..bounds.max.x.ceil() as u32 {
            let point = egui::pos2(x as f32 + 0.5, y as f32 + 0.5);
            if !clip.contains(point) {
                continue;
            }
            let edges = [
                edge(b.pos, c.pos, point),
                edge(c.pos, a.pos, point),
                edge(a.pos, b.pos, point),
            ];
            if edges
                .iter()
                .zip(included_edges)
                .any(|(&value, include)| value < 0.0 || (value == 0.0 && !include))
            {
                continue;
            }
            // The top-left rule assigns each shared edge to exactly one
            // triangle, avoiding dark seams from double alpha blending.
            let weights = edges.map(|value| value / area);
            let uv = egui::pos2(
                a.uv.x * weights[0] + b.uv.x * weights[1] + c.uv.x * weights[2],
                a.uv.y * weights[0] + b.uv.y * weights[1] + c.uv.y * weights[2],
            );
            let texel = sample_texture(texture, uv);
            let mut source = [0.0; 4];
            for (channel, source_channel) in source.iter_mut().enumerate() {
                *source_channel = texel[channel]
                    * (0..3)
                        .map(|index| f32::from(colors[index][channel]) * weights[index] / 255.0)
                        .sum::<f32>();
            }
            // egui's glow backend multiplies and blends in gamma space,
            // using premultiplied sRGBA colors for vertices and textures.
            let destination = image.get_pixel_mut(x, y);
            for (channel, value) in destination.0.iter_mut().enumerate() {
                *value = (source[channel] * 255.0 + f32::from(*value) * (1.0 - source[3]))
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
}

fn connected_snapshot() -> UiSnapshot {
    let mut snapshot = sample_paired_snapshot();
    snapshot.layout_matrix = "self,peer-1234".to_string();
    snapshot.paired_peers[0].connected = true;
    snapshot.paired_peers[0].health_state = "connected".to_string();
    snapshot.paired_peers[0].health_reason.clear();
    snapshot
}

fn sample_transfers() -> Vec<UiFileTransfer> {
    [
        ("completed", "incoming", "Design notes.pdf", 512_000),
        ("active", "outgoing", "Project assets.zip", 1_400_000),
        ("failed", "outgoing", "Trip photos.zip", 0),
    ]
    .into_iter()
    .map(|(state, direction, name, transferred)| UiFileTransfer {
        transfer_id: format!("fixture-{state}"),
        previous_transfer_id: String::new(),
        direction: direction.to_string(),
        peer_id: "peer-1234".to_string(),
        file_name: name.to_string(),
        state: state.to_string(),
        transferred_bytes: transferred,
        total_bytes: if state == "completed" {
            512_000
        } else {
            5_600_000
        },
        failure_reason: if state == "failed" {
            "The other PC disconnected. Reconnect and try again.".to_string()
        } else {
            String::new()
        },
        source_path: format!(r"C:\Fixture\{name}"),
        final_path: if state == "completed" {
            format!(r"C:\Users\Test\Downloads\Boundless\{name}")
        } else {
            String::new()
        },
        queued_at: "2026-09-04T12:00:00Z".to_string(),
        updated_at: "2026-09-04T12:00:02Z".to_string(),
    })
    .collect()
}

#[test]
fn dashboard_product_fixtures_render_offscreen() {
    for (name, tab) in [
        ("first_run", Tab::Status),
        ("paired", Tab::Status),
        ("stale", Tab::Status),
        ("layout", Tab::Layout),
        ("transfers", Tab::TransferCenter),
        ("sharing", Tab::Settings),
        ("support", Tab::Support),
    ] {
        let mut snapshot = if name == "first_run" {
            sample_first_run_snapshot()
        } else {
            connected_snapshot()
        };
        if name == "first_run" {
            snapshot.discovered_peers.push(sample_discovered_peer());
        }
        if name == "transfers" {
            snapshot.file_transfers = sample_transfers();
        }
        let mut harness = DashboardHarness::new(snapshot, tab, [1100, 800]);
        if name == "stale" {
            harness.app.apply_app_msg(AppMsg::SnapshotError(
                "The local service stopped responding.".to_string(),
            ));
        }
        harness.settle();
        harness.assert_text("Boundless");
        for navigation in ["Home", "Arrange PCs", "Files", "Sharing", "Support"] {
            harness.assert_text(navigation);
        }
        if name == "stale" {
            harness.assert_text("Boundless needs attention");
        }
        assert!(
            harness.app.task_runner.recorded_commands().is_empty(),
            "merely rendering {name} must not dispatch a command"
        );
        // Tessellation is always exercised; raster output is opt-in to keep
        // ordinary tests fast and avoid writing files into a user's profile.
        assert!(
            !harness
                .context
                .tessellate(
                    harness.output.shapes.clone(),
                    harness.output.pixels_per_point,
                )
                .is_empty()
        );
        harness.save_if_requested(name);
        harness.size = [800, 600];
        harness.settle();
        harness.assert_text("Support");
        harness.save_if_requested(&format!("{name}_compact"));
        if matches!(name, "first_run" | "sharing" | "transfers") {
            harness.scroll_to_bottom();
            harness.save_if_requested(&format!("{name}_compact_scrolled"));
        }
        if name == "first_run" {
            harness.click("Connect by address");
            harness.save_if_requested("first_run_manual_compact");
        }
    }
}

#[test]
fn dashboard_navigation_works_through_real_pointer_input() {
    let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Status, [1100, 800]);
    for (label, expected_tab) in [
        ("Arrange PCs", Tab::Layout),
        ("Files", Tab::TransferCenter),
        ("Sharing", Tab::Settings),
        ("Support", Tab::Support),
        ("Home", Tab::Status),
    ] {
        harness.click(label);
        assert_eq!(
            harness.app.selected_tab, expected_tab,
            "navigation: {label}"
        );
    }
    assert!(harness.app.task_runner.recorded_commands().is_empty());
}

#[test]
fn dashboard_density_fixtures_use_scaled_font_atlas_and_logical_input() {
    for (name, density) in [("paired_scaled_150", 1.5), ("paired_scaled_200", 2.0)] {
        let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Status, [800, 600]);
        harness.native_pixels_per_point = density;
        harness.settle();
        assert_eq!(harness.output.pixels_per_point, density);
        harness.assert_text("Your PCs");
        harness.save_if_requested(name);
        harness.click("Arrange PCs");
        assert_eq!(harness.app.selected_tab, Tab::Layout);
        harness.assert_text("Arrange your PCs");
        assert!(harness.app.task_runner.recorded_commands().is_empty());
    }
}

#[test]
fn dashboard_light_theme_renders_first_run_files_and_support() {
    for (name, tab) in [
        ("first_run_light", Tab::Status),
        ("files_light", Tab::TransferCenter),
        ("support_light", Tab::Support),
    ] {
        let mut snapshot = if name == "first_run_light" {
            let mut snapshot = sample_first_run_snapshot();
            snapshot.discovered_peers.push(sample_discovered_peer());
            snapshot
        } else {
            connected_snapshot()
        };
        if name == "files_light" {
            snapshot.file_transfers = sample_transfers();
        }
        let mut harness = DashboardHarness::new(snapshot, tab, [800, 600]);
        harness.context.set_theme(egui::Theme::Light);
        harness.settle();
        assert!(!harness.context.global_style().visuals.dark_mode);
        harness.assert_text("Boundless");
        harness.save_if_requested(name);
        assert!(harness.app.task_runner.recorded_commands().is_empty());
    }
}

#[test]
fn configured_active_text_has_readable_contrast_in_both_themes() {
    fn luminance(color: egui::Color32) -> f64 {
        let [r, g, b, _] = color.to_array();
        let linear = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }
    let contrast = |foreground: egui::Color32, background: egui::Color32| {
        let a = luminance(foreground);
        let b = luminance(background);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    };
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Status, [800, 600]);
        harness.context.set_theme(theme);
        harness.settle();
        let style = harness.context.global_style();
        let visuals = &style.visuals;
        let foreground = visuals
            .override_text_color
            .expect("configured product text color");
        for (name, background) in [
            ("panel", visuals.panel_fill),
            ("button", visuals.widgets.inactive.weak_bg_fill),
            ("hovered button", visuals.widgets.hovered.weak_bg_fill),
            ("pressed button", visuals.widgets.active.weak_bg_fill),
            ("selected navigation", visuals.selection.bg_fill),
        ] {
            let ratio = contrast(foreground, background);
            assert!(
                ratio >= 4.5,
                "{theme:?} {name} text contrast is {ratio:.2}:1"
            );
        }
        let weak = visuals
            .weak_text_color
            .expect("configured secondary text color");
        assert!(
            contrast(weak, visuals.panel_fill) >= 4.5,
            "{theme:?} secondary text contrast"
        );
    }
}

#[test]
fn file_sender_records_peer_once_and_picker_cancel_leaves_no_transfer() {
    let mut harness = DashboardHarness::new(connected_snapshot(), Tab::TransferCenter, [1100, 800]);
    harness.click("Send file...");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "choose_and_send_file");
    assert_eq!(commands[0].1["peer_id"], "peer-1234");
    assert!(harness.app.file_send_pending);
    harness.click("Send file...");
    assert_eq!(harness.app.task_runner.recorded_commands().len(), 1);
    harness
        .app
        .apply_app_msg(AppMsg::FileSendComplete(Ok(None)));
    harness.settle();
    assert!(!harness.app.file_send_pending);
    assert!(harness.app.snapshot.file_transfers.is_empty());
    assert!(harness.app.toasts.is_empty());
    assert_eq!(harness.app.task_runner.recorded_commands().len(), 1);
    harness.click("Send file...");
    assert_eq!(harness.app.task_runner.recorded_commands().len(), 2);
    harness.app.apply_app_msg(AppMsg::FileSendComplete(Err(
        "Fixture selection failed".to_string()
    )));
    harness.settle();
    assert!(!harness.app.file_send_pending);
    harness.assert_text("Fixture selection failed");

    let mut disconnected =
        DashboardHarness::new(sample_paired_snapshot(), Tab::TransferCenter, [1100, 800]);
    disconnected.click("Send file...");
    assert!(disconnected.app.task_runner.recorded_commands().is_empty());
    disconnected.assert_text("Connect a paired PC before sending a file.");
}

#[test]
fn disabled_file_sharing_blocks_send_and_toggle_preserves_folder_draft() {
    let mut snapshot = connected_snapshot();
    snapshot.features.insert("transfer_file".to_string(), false);
    let mut harness = DashboardHarness::new(snapshot, Tab::TransferCenter, [1100, 800]);
    let draft = r"C:\Fixture\Folder Not Saved";
    harness.app.file_receive_dir_edit = draft.to_string();
    harness.click("Send file...");
    harness.assert_text("File sharing is off.");
    assert!(harness.app.task_runner.recorded_commands().is_empty());
    harness.click("Open sharing settings");
    assert_eq!(harness.app.selected_tab, Tab::Settings);
    harness.click("Share files");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "set_feature");
    assert_eq!(commands[0].1["name"], "transfer_file");
    assert_eq!(commands[0].1["enabled"], true);
    assert_eq!(harness.app.file_receive_dir_edit, draft);
}

#[test]
fn primary_pause_action_dispatches_once_without_native_runtime() {
    let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Status, [1100, 800]);
    harness.click("Pause input");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 1, "one click must issue exactly one action");
    assert_eq!(commands[0].0, "set_input_sharing");
    assert_eq!(commands[0].1["enabled"], false);
    assert!(harness.app.input_pause_requested);
    harness.assert_text("Input pause requested");
    assert!(harness.app.input_change_pending);
    assert!(
        !harness
            .text
            .iter()
            .any(|(text, _)| text == "Input sharing paused")
    );
    harness
        .app
        .apply_app_msg(AppMsg::InputSharingComplete(false));
    harness.settle();
    assert!(!harness.app.input_change_pending);
    harness.assert_text("Input sharing paused");
}

#[test]
fn forgetting_a_pc_requires_confirmation_and_cancel_preserves_trust() {
    let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Status, [1100, 800]);
    harness.click("PC details");
    harness.click("Forget this PC...");
    assert_eq!(
        harness.app.pending_peer_removal.as_deref(),
        Some("peer-1234")
    );
    harness.assert_text("Forget Office Desktop?");
    assert!(harness.app.task_runner.recorded_commands().is_empty());
    harness.click("Cancel");
    assert!(harness.app.pending_peer_removal.is_none());
    assert!(harness.app.task_runner.recorded_commands().is_empty());
    assert_eq!(harness.app.snapshot.paired_peers.len(), 1);
    harness.click("Forget this PC...");
    harness.click("Forget PC");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "remove_peer");
    assert_eq!(commands[0].1["peer_id"], "peer-1234");
    assert!(harness.app.pending_peer_removal.is_none());
}

#[test]
fn losing_current_status_does_not_show_remembered_peer_as_connected() {
    let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Status, [1100, 800]);
    harness.settle();
    harness.assert_text("Connected");
    harness
        .app
        .apply_app_msg(AppMsg::SnapshotError("Connection closed".to_string()));
    harness.settle();
    harness.assert_text("Boundless needs attention");
    harness.assert_text("Status unavailable");
    assert!(!harness.text.iter().any(|(text, _)| text == "Connected"));
    assert!(harness.app.task_runner.recorded_commands().is_empty());
    harness
        .app
        .apply_app_msg(AppMsg::SnapshotUpdated(Box::new(connected_snapshot())));
    harness.settle();
    harness.assert_text("Connected");
    assert!(harness.app.snapshot_error.is_none());
}

#[test]
fn home_readiness_distinguishes_arrangement_backend_and_sharing_state() {
    for (name, expected) in [
        (
            "unarranged",
            "Arrange your PCs before switching at screen edges.",
        ),
        ("input_unavailable", "desktop input is not ready"),
        ("paused", "Keyboard and mouse sharing is paused."),
        ("edge_disabled", "Screen-edge switching is off in Sharing."),
        (
            "ready",
            "connected and arranged. Move across a shared screen edge",
        ),
    ] {
        let mut snapshot = connected_snapshot();
        match name {
            "unarranged" => snapshot.layout_matrix = "self".to_string(),
            "input_unavailable" => {
                snapshot.input_runtime.capture_backend_mode =
                    "service_session_unsupported".to_string();
            }
            "paused" => {
                snapshot.features.insert("share_input".to_string(), false);
            }
            "edge_disabled" => {
                snapshot.features.insert("easy_mouse".to_string(), false);
            }
            _ => {}
        }
        let mut harness = DashboardHarness::new(snapshot, Tab::Status, [1100, 800]);
        harness.settle();
        harness.assert_text(expected);
        assert!(harness.app.task_runner.recorded_commands().is_empty());
        harness.save_if_requested(&format!("home_{name}"));
        if name == "unarranged" {
            harness.click("Arrange connected PCs");
            assert_eq!(harness.app.selected_tab, Tab::Layout);
        } else if name == "input_unavailable" {
            harness.click("Check input status");
            assert_eq!(harness.app.selected_tab, Tab::Settings);
        }
    }
}

#[test]
fn changing_receive_options_does_not_save_a_folder_draft() {
    let snapshot = connected_snapshot();
    let saved_folder = snapshot.file_transfer_config.receive_dir.clone();
    let draft_folder = r"C:\Fixture\Not Yet Saved";
    let mut harness = DashboardHarness::new(snapshot, Tab::Settings, [1100, 800]);
    harness.click(&saved_folder);
    let select_all = egui::Modifiers {
        ctrl: true,
        command: true,
        ..Default::default()
    };
    harness.frame(vec![
        egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: select_all,
        },
        egui::Event::Text(draft_folder.to_string()),
        egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: select_all,
        },
    ]);
    assert_eq!(harness.app.file_receive_dir_edit, draft_folder);
    assert!(harness.app.task_runner.recorded_commands().is_empty());
    harness.click("Organize received files by sender");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "set_file_transfer_config");
    assert_eq!(commands[0].1["receive_dir"], saved_folder);
    assert_eq!(commands[0].1["organize_by_peer"], true);
    assert_eq!(harness.app.file_receive_dir_edit, draft_folder);
    harness.click("Save Folder");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[1].0, "set_file_transfer_config");
    assert_eq!(commands[1].1["receive_dir"], draft_folder);
    harness.app.file_receive_dir_edit.clear();
    harness
        .app
        .apply_app_msg(AppMsg::SnapshotUpdated(Box::new(connected_snapshot())));
    harness.settle();
    assert!(
        harness.app.file_receive_dir_edit.is_empty(),
        "a fresh snapshot must preserve an intentionally cleared folder draft"
    );
    assert_eq!(harness.app.task_runner.recorded_commands().len(), 2);
}

#[test]
fn support_export_is_recorded_and_completion_is_visible() {
    let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Support, [1100, 800]);
    harness.click("Save redacted report");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "export_support");
    harness.assert_text("Saving report...");
    harness.app.apply_app_msg(AppMsg::SupportExportComplete(
        "Saved fixture report to C:\\Fixture\\support.json".to_string(),
    ));
    harness.settle();
    harness.assert_text("Saved fixture report");
}

#[test]
fn paired_testing_permission_requires_click_and_keeps_failures_explicit() {
    let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Support, [1100, 800]);
    harness.click("Paired connection testing");
    harness.scroll_to_bottom();
    harness.assert_text("Permission status has not been checked in this window.");
    assert!(harness.app.task_runner.recorded_commands().is_empty());
    harness.click("Allow paired testing for 10 minutes");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "paired_testing_permission");
    assert_eq!(
        commands[0].1["change"],
        serde_json::json!(["peer-1234", 600])
    );
    assert!(harness.app.paired_testing_pending);
    harness.assert_text("Checking permission...");
    harness.app.apply_app_msg(AppMsg::PairedTestingUpdated(Ok(
        app_services::paired_testing::PairedTestConsent {
            schema_version: 1,
            peer_id: Some("peer-1234".to_string()),
            enabled: true,
            remaining_seconds: 600,
            remaining_requests: 256,
            remaining_bytes: 16 * 1024 * 1024,
        },
    )));
    harness.settle();
    assert!(!harness.app.paired_testing_pending);
    harness.assert_text("Last reported: allowed for Office Desktop");
    harness.save_if_requested("paired_testing_permission");
    harness.click("Stop paired testing");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[1].1["change"], serde_json::json!(["peer-1234", 0]));
    harness.app.apply_app_msg(AppMsg::PairedTestingUpdated(Err(
        "The local service stopped responding.".to_string(),
    )));
    harness.settle();
    assert!(!harness.app.paired_testing_pending);
    harness
        .assert_text("Could not check or change permission: The local service stopped responding.");
    harness.assert_text("Last reported: allowed for Office Desktop");
    harness.click("Stop paired testing");
    assert_eq!(harness.app.task_runner.recorded_commands().len(), 3);
    harness.app.apply_app_msg(AppMsg::PairedTestingUpdated(Ok(
        app_services::paired_testing::PairedTestConsent {
            schema_version: 1,
            ..Default::default()
        },
    )));
    harness.settle();
    harness.assert_text("Permission is off or has expired.");
    assert!(harness.app.paired_testing_error.is_none());
}

#[test]
fn arranging_without_dragging_keeps_edits_local_until_apply_confirmation() {
    let mut harness = DashboardHarness::new(connected_snapshot(), Tab::Layout, [1100, 800]);
    harness.click("Arrange without dragging");
    harness.click("Move up");
    harness.click("Move left");
    assert!(harness.app.task_runner.recorded_commands().is_empty());
    harness.click("Apply Layout");
    assert!(harness.app.confirm_apply_pending);
    harness.assert_text("Confirm Layout");
    assert!(harness.app.task_runner.recorded_commands().is_empty());
    harness.click("Apply");
    let commands = harness.app.task_runner.recorded_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "apply_layout");
    assert_eq!(commands[0].1["matrix_spec"], "peer-1234;self");
    assert!(!harness.app.confirm_apply_pending);
}

#[derive(serde::Serialize)]
struct FrameMeasurement {
    ui_layout_ns: u64,
    tessellation_ns: u64,
    combined_ns: u64,
}

fn measure_cpu_frame(harness: &mut DashboardHarness) -> FrameMeasurement {
    let previous_render_duration = harness.render_duration;
    harness.frame(Vec::new());
    let ui_layout_ns =
        u64::try_from((harness.render_duration - previous_render_duration).as_nanos())
            .expect("UI frame duration fits in u64 nanoseconds");
    // Move the shapes so cloning the fixture's capture buffer is not counted
    // as product tessellation work. No software PNG painting is measured.
    let shapes = std::mem::take(&mut harness.output.shapes);
    let started = Instant::now();
    let primitives = harness
        .context
        .tessellate(shapes, harness.output.pixels_per_point);
    let tessellation_ns = u64::try_from(started.elapsed().as_nanos())
        .expect("tessellation duration fits in u64 nanoseconds");
    assert!(!std::hint::black_box(primitives).is_empty());
    FrameMeasurement {
        ui_layout_ns,
        tessellation_ns,
        combined_ns: ui_layout_ns + tessellation_ns,
    }
}

fn distribution_ns(mut values: Vec<u64>) -> serde_json::Value {
    assert!(!values.is_empty());
    values.sort_unstable();
    let percentile = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
    serde_json::json!({
        "min": values[0],
        "p50": percentile(50),
        "p95": percentile(95),
        "max": values[values.len() - 1],
    })
}

fn benchmark_git_output(arguments: &[&str]) -> Option<String> {
    // Read-only provenance lookup, outside every measured interval. A machine
    // without Git may still measure the UI; the report then preserves null.
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
#[ignore = "opt-in CPU measurement; set BOUNDLESS_UI_ARTIFACT_DIR and run alone"]
fn dashboard_render_cpu_benchmark() {
    const WARMUP_FRAMES: usize = 30;
    const MEASURED_FRAMES: usize = 200;
    let directory = std::env::var_os(ARTIFACT_DIR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .expect("set BOUNDLESS_UI_ARTIFACT_DIR to save benchmark evidence");
    let mut cases = Vec::new();
    for size in [[1100, 800], [800, 600]] {
        for (name, tab) in [
            ("home", Tab::Status),
            ("arrange", Tab::Layout),
            ("files", Tab::TransferCenter),
        ] {
            let mut snapshot = connected_snapshot();
            if name == "files" {
                snapshot.file_transfers = sample_transfers();
            }
            let mut harness = DashboardHarness::new(snapshot, tab, size);
            for _ in 0..WARMUP_FRAMES {
                std::hint::black_box(measure_cpu_frame(&mut harness));
            }
            let samples: Vec<_> = (0..MEASURED_FRAMES)
                .map(|_| measure_cpu_frame(&mut harness))
                .collect();
            assert!(harness.app.task_runner.recorded_commands().is_empty());
            let combined =
                distribution_ns(samples.iter().map(|sample| sample.combined_ns).collect());
            eprintln!(
                "dashboard_cpu={name} size={}x{} p50_ns={} p95_ns={}",
                size[0], size[1], combined["p50"], combined["p95"],
            );
            cases.push(serde_json::json!({
                "fixture": name,
                "viewport_points": size,
                "pixels_per_point": harness.output.pixels_per_point,
                "warmup_frames": WARMUP_FRAMES,
                "measured_frames": MEASURED_FRAMES,
                "summary_ns": {
                    "ui_layout": distribution_ns(samples.iter().map(|sample| sample.ui_layout_ns).collect()),
                    "tessellation": distribution_ns(samples.iter().map(|sample| sample.tessellation_ns).collect()),
                    "combined": combined,
                },
                "samples_ns": samples,
            }));
        }
    }
    let report = serde_json::json!({
        "schema_version": "boundless.ui_frame_benchmark.v1",
        "measurement": {
            "clock": "std::time::Instant wall time around synchronous CPU work",
            "scope": "real DashboardApp::render_content egui frame/layout plus egui tessellation",
            "combined": "sum of separately timed ui_layout and tessellation stages",
            "excluded": ["fixture bookkeeping", "PNG software painting", "native window", "GPU rendering", "display presentation", "daemon IPC", "input capture and injection"],
            "percentile_method": "nearest rank",
            "hardware_dependent_pass_thresholds": false,
        },
        "provenance": {
            "source_revision": benchmark_git_output(&["rev-parse", "HEAD"]),
            "working_tree_dirty": benchmark_git_output(&["status", "--porcelain=v1", "--untracked-files=normal"]).map(|status| !status.is_empty()),
            "package_version": env!("CARGO_PKG_VERSION"),
            "debug_assertions": cfg!(debug_assertions),
            "target_os": std::env::consts::OS,
            "target_arch": std::env::consts::ARCH,
            "pointer_width_bits": usize::BITS,
            "available_parallelism": std::thread::available_parallelism().ok().map(|value| value.get()),
            "test_binary_path": std::env::current_exe().ok().map(|path| path.display().to_string()),
            "binary_hash_note": "The external evidence runner can SHA256 test_binary_path without adding a benchmark dependency.",
        },
        "cases": cases,
    });
    std::fs::create_dir_all(&directory).expect("create benchmark artifact directory");
    let path = directory.join("ui-frame-benchmark.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&report).expect("serialize benchmark"),
    )
    .expect("save raw benchmark measurements");
    eprintln!("dashboard_cpu_report={}", path.display());
    let marker_cases: Vec<_> = report["cases"]
        .as_array()
        .expect("benchmark cases")
        .iter()
        .map(|case| {
            serde_json::json!({
                "fixture": case["fixture"],
                "viewport_points": case["viewport_points"],
                "pixels_per_point": case["pixels_per_point"],
                "measured_frames": case["measured_frames"],
                "summary_ns": case["summary_ns"],
            })
        })
        .collect();
    let marker = serde_json::json!({
        "schema_version": report["schema_version"],
        "report_path": path.display().to_string(),
        "provenance": report["provenance"],
        "cases": marker_cases,
    });
    println!(
        "BOUNDLESS_UI_BENCHMARK={}",
        serde_json::to_string(&marker).expect("serialize benchmark marker")
    );
}

#[test]
fn software_painter_clips_without_double_blending_shared_edges() {
    let mut image = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]));
    let texture = egui::ColorImage::filled([1, 1], egui::Color32::WHITE);
    let color = egui::Color32::from_rgba_premultiplied(128, 0, 0, 128);
    let vertices = [
        Vertex::untextured(egui::pos2(0.0, 0.0), color),
        Vertex::untextured(egui::pos2(4.0, 0.0), color),
        Vertex::untextured(egui::pos2(4.0, 4.0), color),
        Vertex::untextured(egui::pos2(0.0, 4.0), color),
    ];
    let clip = egui::Rect::from_min_max(egui::pos2(1.0, 1.0), egui::pos2(3.0, 3.0));
    for indices in [[0, 1, 2], [0, 2, 3]] {
        paint_triangle(
            &mut image,
            indices.map(|i| vertices[i]),
            &texture,
            clip,
            1.0,
        );
    }
    for (x, y, pixel) in image.enumerate_pixels() {
        let expected = if (1..3).contains(&x) && (1..3).contains(&y) {
            Rgba([128, 0, 0, 255])
        } else {
            Rgba([0, 0, 0, 255])
        };
        assert_eq!(*pixel, expected, "pixel ({x}, {y})");
    }
}
