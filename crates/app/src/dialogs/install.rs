//! The first thing a copy run from its disk image asks: may it move itself
//! into the Applications folder.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use egui::{Rect, Sense, Vec2};

use super::{Outcome, footer, modal, padded, small_note};
use crate::app::App;
use crate::setup;
use crate::theme::{self, Anchor, Type};
use crate::widgets::{Button, ButtonKind};

#[derive(Default)]
pub struct InstallDialog {
    /// The copy under way.
    job: Option<Receiver<Result<PathBuf, String>>>,
    error: Option<String>,
    /// The copy is in place and this process is on its way out.
    leaving: bool,
}

impl InstallDialog {
    pub(super) fn show(&mut self, _app: &mut App, ctx: &egui::Context) -> Outcome {
        if let Some(job) = &self.job {
            match job.try_recv() {
                Ok(Ok(installed)) => {
                    self.job = None;
                    match setup::relaunch_after_exit(&installed) {
                        Ok(()) => {
                            self.leaving = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(e) => self.error = Some(format!("Siphon is in {} now, but could not be started from there: {e}. Open it from the Applications folder.", installed.display())),
                    }
                }
                Ok(Err(e)) => {
                    self.job = None;
                    self.error = Some(format!("Siphon could not be copied ({e}). Drag it onto the Applications folder in the window it came in."));
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(60)),
                Err(TryRecvError::Disconnected) => {
                    self.job = None;
                    self.error = Some("The copy stopped before it finished. Try again.".to_owned());
                }
            }
        }
        let busy = self.job.is_some() || self.leaving;

        let mut start = false;
        let mut later = false;
        let outcome = modal(ctx, "install", 440.0, |ui| {
            padded(ui, |ui| {
                let (mark, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), Sense::hover());
                crate::logo::paint(ui, Rect::from_min_size(mark.min, Vec2::splat(40.0)));
                let (title, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
                theme::text_in(ui.painter(), title, Anchor::Left, "Move Siphon to the Applications folder?", Type::TITLE, theme::TEXT);
                ui.add_space(6.0);
                let body = "Siphon is running from the disk image it came in. Your browsers are told where Siphon lives so they can hand it downloads, and that place has to outlast the image.";
                let galley = theme::layout_wrapped(ui.painter(), body, Type::BODY, theme::TEXT_2, ui.available_width(), 5);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y), Sense::hover());
                ui.painter().galley(rect.min, galley, theme::TEXT_2);
                if let Some(error) = &self.error {
                    ui.add_space(10.0);
                    small_note(ui, error, theme::BAD);
                }
            });
            footer(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if Button::new(if busy { "Moving\u{2026}" } else { "Move to Applications" }, ButtonKind::Primary).min_width(168.0).show(ui).clicked() {
                        start = true;
                    }
                    if Button::new("Not now", ButtonKind::Secondary).show(ui).clicked() {
                        later = true;
                    }
                });
            });
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });

        if start {
            self.error = None;
            let (sender, receiver) = channel();
            let wake = ctx.clone();
            std::thread::spawn(move || {
                let _ = sender.send(setup::move_to_applications());
                wake.request_repaint();
            });
            self.job = Some(receiver);
        }
        // A copy under way is not walked away from by a stray Escape.
        if busy {
            return Outcome::Stay;
        }
        if later { Outcome::Close } else { outcome }
    }
}
