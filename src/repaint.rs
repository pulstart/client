use eframe::egui;
use std::time::Duration;

/// Wakes the window(s) showing one stream. Background threads cannot use
/// `Context::request_repaint`: it targets whichever viewport ran last, which
/// is the root window for every thread.
#[derive(Clone)]
pub struct Repaint {
    ctx: egui::Context,
    window: egui::ViewportId,
    /// Glow uploads need the root `eframe::Frame`, so a glow-rendered aux
    /// window also needs the root pass to run.
    also_root: bool,
}

impl Repaint {
    pub fn root(ctx: egui::Context) -> Self {
        Self {
            ctx,
            window: egui::ViewportId::ROOT,
            also_root: false,
        }
    }

    pub fn window(ctx: egui::Context, window: egui::ViewportId, also_root: bool) -> Self {
        Self {
            ctx,
            window,
            also_root,
        }
    }

    pub fn request_repaint(&self) {
        self.ctx.request_repaint_of(self.window);
        if self.also_root && self.window != egui::ViewportId::ROOT {
            self.ctx.request_repaint_of(egui::ViewportId::ROOT);
        }
    }

    pub fn request_repaint_after(&self, delay: Duration) {
        self.ctx.request_repaint_after_for(delay, self.window);
        if self.also_root && self.window != egui::ViewportId::ROOT {
            self.ctx
                .request_repaint_after_for(delay, egui::ViewportId::ROOT);
        }
    }
}
