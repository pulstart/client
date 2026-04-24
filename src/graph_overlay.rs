use crate::debug_state::ConnectionDebugSnapshot;
use eframe::egui;
use std::collections::VecDeque;
use std::time::Instant;

const MAX_SAMPLES: usize = 360; // 1 minute at ~6 fps sampling
const SAMPLE_INTERVAL_MS: u128 = 166;
const LANE_HEIGHT: f32 = 44.0;
const PANEL_WIDTH: f32 = 380.0;
const LABEL_WIDTH: f32 = 70.0;
const VALUE_WIDTH: f32 = 76.0;

#[derive(Clone, Default)]
struct Sample {
    bitrate_kbps: f32,
    fps: f32,
    latency_ms: f32,
    decode_ms: f32,
    rtt_ms: f32,
    loss_pct: f32,
}

pub struct GraphOverlay {
    samples: VecDeque<Sample>,
    last_push: Instant,
}

impl GraphOverlay {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES + 1),
            last_push: Instant::now() - std::time::Duration::from_secs(1),
        }
    }

    pub fn push(&mut self, snap: &ConnectionDebugSnapshot) {
        let now = Instant::now();
        if now.duration_since(self.last_push).as_millis() < SAMPLE_INTERVAL_MS {
            return;
        }
        self.last_push = now;

        let total = snap.received_packets.saturating_add(snap.lost_packets);
        let loss = if total > 0 {
            snap.lost_packets as f32 * 100.0 / total as f32
        } else {
            0.0
        };

        let s = Sample {
            bitrate_kbps: snap.received_video_kbps,
            fps: snap.present_fps,
            latency_ms: snap.total_latency_ms.unwrap_or(0.0),
            decode_ms: snap.decode_work_ms.unwrap_or(0.0),
            rtt_ms: snap.clock_rtt_ms.unwrap_or(0.0),
            loss_pct: loss,
        };

        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }

    pub fn render(&self, ctx: &egui::Context) {
        let screen = ctx.input(|i| i.content_rect());
        let x = screen.right() - PANEL_WIDTH - 10.0;

        egui::Area::new(egui::Id::new("graph_overlay"))
            .fixed_pos(egui::pos2(x, 10.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(egui::Color32::from_rgba_unmultiplied(12, 12, 12, 230))
                    .corner_radius(4)
                    .show(ui, |ui| {
                        ui.set_width(PANEL_WIDTH);
                        self.render_lane(
                            ui,
                            "Bitrate",
                            egui::Color32::from_rgb(72, 163, 255),
                            "kbps",
                            |s| s.bitrate_kbps,
                        );
                        self.render_lane(
                            ui,
                            "FPS",
                            egui::Color32::from_rgb(56, 214, 118),
                            "fps",
                            |s| s.fps,
                        );
                        self.render_lane(
                            ui,
                            "Latency",
                            egui::Color32::from_rgb(255, 179, 71),
                            "ms",
                            |s| s.latency_ms,
                        );
                        self.render_lane(
                            ui,
                            "Decode",
                            egui::Color32::from_rgb(180, 142, 255),
                            "ms",
                            |s| s.decode_ms,
                        );
                        self.render_lane(
                            ui,
                            "RTT",
                            egui::Color32::from_rgb(78, 205, 196),
                            "ms",
                            |s| s.rtt_ms,
                        );
                        self.render_lane(
                            ui,
                            "Loss",
                            egui::Color32::from_rgb(255, 107, 107),
                            "%",
                            |s| s.loss_pct,
                        );
                    });
            });
    }

    fn render_lane(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        color: egui::Color32,
        unit: &str,
        extract: fn(&Sample) -> f32,
    ) {
        let (_, rect) = ui.allocate_space(egui::vec2(PANEL_WIDTH, LANE_HEIGHT));
        let painter = ui.painter();

        let graph_left = rect.left() + LABEL_WIDTH;
        let graph_right = rect.right() - VALUE_WIDTH;
        let graph_top = rect.top() + 4.0;
        let graph_bottom = rect.bottom() - 4.0;
        let graph_rect = egui::Rect::from_min_max(
            egui::pos2(graph_left, graph_top),
            egui::pos2(graph_right, graph_bottom),
        );

        // Graph background
        painter.rect_filled(
            graph_rect,
            2.0,
            egui::Color32::from_rgba_unmultiplied(24, 24, 24, 200),
        );

        let current = self.samples.back().map(|s| extract(s)).unwrap_or(0.0);

        if !self.samples.is_empty() {
            let values: Vec<f32> = self.samples.iter().map(|s| extract(s)).collect();
            let min_val = values
                .iter()
                .copied()
                .fold(f32::INFINITY, f32::min)
                .min(0.0);
            let max_val = values
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max)
                .max(min_val + 0.1);
            let range = max_val - min_val;

            // Mid grid line
            let mid_y = graph_rect.center().y;
            painter.line_segment(
                [
                    egui::pos2(graph_left, mid_y),
                    egui::pos2(graph_right, mid_y),
                ],
                egui::Stroke::new(0.5, egui::Color32::from_rgba_unmultiplied(80, 80, 80, 80)),
            );

            // Data line
            let graph_w = graph_rect.width();
            let graph_h = graph_rect.height();
            let step = graph_w / (MAX_SAMPLES - 1).max(1) as f32;
            let offset = (MAX_SAMPLES - values.len()) as f32 * step;
            let points: Vec<egui::Pos2> = values
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let x = graph_left + offset + i as f32 * step;
                    let norm = ((v - min_val) / range).clamp(0.0, 1.0);
                    let y = graph_bottom - norm * graph_h;
                    egui::pos2(x, y)
                })
                .collect();

            if points.len() >= 2 {
                painter.add(egui::Shape::line(
                    points.clone(),
                    egui::Stroke::new(1.5, color),
                ));
            }

            // Fill area under line
            let fill_color =
                egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 20);
            if points.len() >= 2 {
                let mut fill = points;
                let first_x = fill.first().unwrap().x;
                let last_x = fill.last().unwrap().x;
                fill.push(egui::pos2(last_x, graph_bottom));
                fill.push(egui::pos2(first_x, graph_bottom));
                painter.add(egui::Shape::convex_polygon(
                    fill,
                    fill_color,
                    egui::Stroke::NONE,
                ));
            }
        }

        // Label
        painter.text(
            egui::pos2(rect.left() + 4.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::monospace(11.0),
            color,
        );

        // Current value
        let value_text = format_value(current, unit);
        painter.text(
            egui::pos2(rect.right() - 4.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            &value_text,
            egui::FontId::monospace(10.0),
            egui::Color32::from_rgb(200, 200, 200),
        );

        // Separator
        painter.line_segment(
            [
                egui::pos2(rect.left() + 4.0, rect.bottom()),
                egui::pos2(rect.right() - 4.0, rect.bottom()),
            ],
            egui::Stroke::new(0.5, egui::Color32::from_rgba_unmultiplied(50, 50, 50, 120)),
        );
    }
}

fn format_value(v: f32, unit: &str) -> String {
    match unit {
        "kbps" => {
            if v >= 10000.0 {
                format!("{:.0}k", v / 1000.0)
            } else if v >= 1000.0 {
                format!("{:.1}k", v / 1000.0)
            } else {
                format!("{:.0} {unit}", v)
            }
        }
        "%" => format!("{:.1}{unit}", v),
        _ => {
            if v >= 100.0 {
                format!("{:.0} {unit}", v)
            } else {
                format!("{:.1} {unit}", v)
            }
        }
    }
}
