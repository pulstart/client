use crate::debug_state::{loss_percent, MetricsSnapshot};
use eframe::egui;
use std::collections::VecDeque;
use std::time::Instant;

const MAX_SAMPLES: usize = 360; // 1 minute at ~6 fps sampling
const SAMPLE_INTERVAL_MS: u128 = 166;
const LANE_HEIGHT: f32 = 44.0;
const PANEL_WIDTH: f32 = 380.0;
const LABEL_WIDTH: f32 = 70.0;
const VALUE_WIDTH: f32 = 76.0;
const NUM_LANES: usize = 6;

/// Lane indices — also index into `lane_scales`.
const LANE_BITRATE: usize = 0;
const LANE_FPS: usize = 1;
const LANE_LATENCY: usize = 2;
const LANE_DECODE: usize = 3;
const LANE_RTT: usize = 4;
const LANE_LOSS: usize = 5;

const WARN_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 80, 80);

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
    /// Per-lane vertical scale (peak). Rises instantly to a new peak, decays
    /// slowly back down — so one transient spike doesn't permanently squash the
    /// lane, and a settled lane doesn't jitter its scale every frame.
    lane_scales: [f32; NUM_LANES],
}

impl GraphOverlay {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES + 1),
            last_push: Instant::now() - std::time::Duration::from_secs(1),
            lane_scales: [0.0; NUM_LANES],
        }
    }

    pub fn push(&mut self, m: &MetricsSnapshot) {
        let now = Instant::now();
        if now.duration_since(self.last_push).as_millis() < SAMPLE_INTERVAL_MS {
            return;
        }
        self.last_push = now;

        let s = Sample {
            bitrate_kbps: m.received_video_kbps,
            fps: m.present_fps,
            // Plot the recent *peak* latency, not the smoothed mean, so a brief
            // hitch between sample ticks still shows up as a spike.
            latency_ms: m
                .latency_recent_max_ms
                .or(m.total_latency_ms)
                .unwrap_or(0.0),
            decode_ms: m.decode_work_ms.unwrap_or(0.0),
            rtt_ms: m.clock_rtt_ms.unwrap_or(0.0),
            loss_pct: loss_percent(
                m.received_packets,
                m.lost_packets,
                m.dropped_frames,
                m.completed_frames,
            ),
        };

        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }

    /// Returns the overlay's screen rect so the caller can register it as a HUD
    /// hit region (keeps the OS cursor shown when the pointer is over the graph).
    pub fn render(&mut self, ctx: &egui::Context) -> egui::Rect {
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
                            LANE_BITRATE,
                            None,
                            |s| s.bitrate_kbps,
                        );
                        self.render_lane(
                            ui,
                            "FPS",
                            egui::Color32::from_rgb(56, 214, 118),
                            "fps",
                            LANE_FPS,
                            None,
                            |s| s.fps,
                        );
                        self.render_lane(
                            ui,
                            "Latency",
                            egui::Color32::from_rgb(255, 179, 71),
                            "ms",
                            LANE_LATENCY,
                            Some(120.0),
                            |s| s.latency_ms,
                        );
                        self.render_lane(
                            ui,
                            "Decode",
                            egui::Color32::from_rgb(180, 142, 255),
                            "ms",
                            LANE_DECODE,
                            None,
                            |s| s.decode_ms,
                        );
                        self.render_lane(
                            ui,
                            "RTT",
                            egui::Color32::from_rgb(78, 205, 196),
                            "ms",
                            LANE_RTT,
                            Some(100.0),
                            |s| s.rtt_ms,
                        );
                        self.render_lane(
                            ui,
                            "Loss",
                            egui::Color32::from_rgb(255, 107, 107),
                            "%",
                            LANE_LOSS,
                            Some(1.0),
                            |s| s.loss_pct,
                        );
                    });
            })
            .response
            .rect
    }

    #[allow(clippy::too_many_arguments)]
    fn render_lane(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        color: egui::Color32,
        unit: &str,
        lane_idx: usize,
        warn_above: Option<f32>,
        extract: fn(&Sample) -> f32,
    ) {
        let (_, rect) = ui.allocate_space(egui::vec2(PANEL_WIDTH, LANE_HEIGHT));

        let graph_left = rect.left() + LABEL_WIDTH;
        let graph_right = rect.right() - VALUE_WIDTH;
        let graph_top = rect.top() + 4.0;
        let graph_bottom = rect.bottom() - 4.0;
        let graph_rect = egui::Rect::from_min_max(
            egui::pos2(graph_left, graph_top),
            egui::pos2(graph_right, graph_bottom),
        );

        let current = self.samples.back().map(extract).unwrap_or(0.0);
        let warn = warn_above.map(|t| current >= t).unwrap_or(false);

        // Window peak + mean in a single pass (baseline is always 0, so the peak
        // alone defines the scale). No intermediate Vec — this runs every frame.
        let mut window_max = 0.0f32;
        let mut sum = 0.0f32;
        for s in &self.samples {
            let v = extract(s);
            if v > window_max {
                window_max = v;
            }
            sum += v;
        }
        let n = self.samples.len();
        let avg = if n > 0 { sum / n as f32 } else { 0.0 };

        // Sticky scale: snap up to a new peak, otherwise ease back down toward it.
        let scale = {
            let sc = &mut self.lane_scales[lane_idx];
            if window_max > *sc {
                *sc = window_max;
            } else {
                *sc = (*sc * 0.94 + window_max * 0.06).max(window_max);
            }
            (*sc).max(0.1)
        };

        let painter = ui.painter();

        // Graph background
        painter.rect_filled(
            graph_rect,
            2.0,
            egui::Color32::from_rgba_unmultiplied(24, 24, 24, 200),
        );

        if n > 0 {
            let graph_w = graph_rect.width();
            let graph_h = graph_rect.height();
            let y_for = |v: f32| graph_bottom - (v / scale).clamp(0.0, 1.0) * graph_h;

            // Mid grid line
            let mid_y = graph_rect.center().y;
            painter.line_segment(
                [
                    egui::pos2(graph_left, mid_y),
                    egui::pos2(graph_right, mid_y),
                ],
                egui::Stroke::new(0.5, egui::Color32::from_rgba_unmultiplied(80, 80, 80, 80)),
            );

            // Average line (faint, lane-colored).
            let avg_y = y_for(avg);
            painter.line_segment(
                [
                    egui::pos2(graph_left, avg_y),
                    egui::pos2(graph_right, avg_y),
                ],
                egui::Stroke::new(
                    0.5,
                    egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 90),
                ),
            );

            // Peak label so the auto-scaled height is readable.
            painter.text(
                egui::pos2(graph_right - 2.0, graph_top + 1.0),
                egui::Align2::RIGHT_TOP,
                format_value(window_max, unit),
                egui::FontId::monospace(8.0),
                egui::Color32::from_rgba_unmultiplied(150, 150, 150, 180),
            );

            // Data line
            let step = graph_w / (MAX_SAMPLES - 1).max(1) as f32;
            let x_offset = (MAX_SAMPLES - n) as f32 * step;
            let points: Vec<egui::Pos2> = self
                .samples
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    egui::pos2(graph_left + x_offset + i as f32 * step, y_for(extract(s)))
                })
                .collect();

            let line_color = if warn { WARN_COLOR } else { color };
            if points.len() >= 2 {
                painter.add(egui::Shape::line(
                    points.clone(),
                    egui::Stroke::new(1.5, line_color),
                ));

                // Fill area under line
                let mut fill = points;
                let first_x = fill.first().unwrap().x;
                let last_x = fill.last().unwrap().x;
                fill.push(egui::pos2(last_x, graph_bottom));
                fill.push(egui::pos2(first_x, graph_bottom));
                painter.add(egui::Shape::convex_polygon(
                    fill,
                    egui::Color32::from_rgba_unmultiplied(
                        line_color.r(),
                        line_color.g(),
                        line_color.b(),
                        20,
                    ),
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

        // Current value (red when over its warn threshold).
        painter.text(
            egui::pos2(rect.right() - 4.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            format_value(current, unit),
            egui::FontId::monospace(10.0),
            if warn {
                WARN_COLOR
            } else {
                egui::Color32::from_rgb(200, 200, 200)
            },
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
