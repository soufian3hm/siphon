//! Check a finished file against the checksum its publisher lists.

use std::time::Duration;

use egui::{CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2};
use siphon_core::checksum::{self, HashAlgorithm};
use siphon_core::{DownloadId, HashJob, Manager};

use super::{Outcome, field_label, footer, modal, padded, title_bar};
use crate::app::App;
use crate::icons::Icon;
use crate::theme::{self, Anchor, Type, Weight};
use crate::widgets::{self, Button, ButtonKind, Field};

pub struct VerifyDialog {
    id: DownloadId,
    algorithm: HashAlgorithm,
    job: Option<HashJob>,
    digest: Option<Result<String, String>>,
    expected: String,
    first_frame: bool,
}

impl VerifyDialog {
    pub fn new(manager: &Manager, id: DownloadId) -> Self {
        let algorithm = HashAlgorithm::default();
        Self { id, algorithm, job: manager.checksum(id, algorithm), digest: None, expected: String::new(), first_frame: true }
    }

    fn restart(&mut self, manager: &Manager) {
        self.digest = None;
        // Replacing the job stops the one in flight.
        self.job = manager.checksum(self.id, self.algorithm);
    }

    pub(super) fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        let Some(view) = app.view(self.id).cloned() else { return Outcome::Close };
        if let Some(result) = self.job.as_ref().and_then(|job| job.take()) {
            self.digest = Some(result);
            self.job = None;
        }
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(60));
        }
        // A pasted checksum says which algorithm it wants.
        if let Some(wanted) = HashAlgorithm::guess(&self.expected)
            && wanted != self.algorithm
        {
            self.algorithm = wanted;
            self.restart(&app.manager);
        }

        let mut copy = false;
        let mut switch: Option<HashAlgorithm> = None;
        let outcome = modal(ctx, "verify", 540.0, |ui| {
            if title_bar(ui, "Verify file", Some(&view.file_name)) {
                return Outcome::Close;
            }
            padded(ui, |ui| {
                let mut chosen = self.algorithm;
                let options: Vec<(HashAlgorithm, &str)> = HashAlgorithm::ALL.iter().map(|a| (*a, a.label())).collect();
                ui.horizontal(|ui| {
                    if widgets::segmented(ui, "verify-algorithm", &options, &mut chosen) {
                        switch = Some(chosen);
                    }
                });
                ui.add_space(14.0);

                field_label(ui, "CHECKSUM OF THE FILE");
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 58.0), Sense::hover());
                ui.painter().rect(rect, CornerRadius::same(theme::R_PANEL), theme::FIELD, Stroke::new(1.0, theme::white(18)), StrokeKind::Inside);
                let inner = rect.shrink2(Vec2::new(14.0, 0.0));
                match (&self.digest, &self.job) {
                    (Some(Ok(digest)), _) => {
                        // Split in two lines of equal length so the eye can compare halves.
                        let text_rect = Rect::from_min_max(inner.min, egui::pos2(inner.right() - 40.0, inner.bottom()));
                        let half = digest.len().div_ceil(2);
                        let ty = Type::mono(12.0, Weight::Regular);
                        if theme::measure(ui.painter(), digest, ty) <= text_rect.width() {
                            theme::text_in(ui.painter(), text_rect, Anchor::Left, digest, ty, theme::TEXT);
                        } else {
                            theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text_rect.left(), rect.top() + 11.0), Vec2::new(text_rect.width(), 18.0)), Anchor::Left, &digest[..half], ty, theme::TEXT);
                            theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text_rect.left(), rect.top() + 29.0), Vec2::new(text_rect.width(), 18.0)), Anchor::Left, &digest[half..], ty, theme::TEXT);
                        }
                        theme::region(ui, Rect::from_min_size(egui::pos2(rect.right() - 38.0, rect.center().y - 14.0), Vec2::splat(28.0)), |ui| {
                            if widgets::icon_button(ui, Icon::Copy, "Copy").clicked() {
                                copy = true;
                            }
                        });
                    }
                    (Some(Err(error)), _) => {
                        theme::text_in(ui.painter(), inner, Anchor::Left, error, Type::SMALL, theme::BAD);
                    }
                    (None, Some(job)) => {
                        let fraction = job.fraction();
                        theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(inner.left(), rect.top() + 12.0), Vec2::new(inner.width(), 18.0)), Anchor::Left, &format!("Reading the file  \u{00B7}  {}", crate::format::percent(fraction)), Type::SMALL, theme::TEXT_2);
                        widgets::progress(ui, Rect::from_min_size(egui::pos2(inner.left(), rect.bottom() - 18.0), Vec2::new(inner.width(), 3.0)), Some(fraction), theme::ACCENT);
                    }
                    (None, None) => {
                        theme::text_in(ui.painter(), inner, Anchor::Left, "The file is no longer where it was saved.", Type::SMALL, theme::TEXT_2);
                    }
                }
                ui.add_space(16.0);

                field_label(ui, "CHECKSUM FROM THE PUBLISHER");
                let response = Field::new(&mut self.expected).hint("Paste it here to compare").mono().id(egui::Id::unique("verify-expected")).show(ui);
                if self.first_frame {
                    self.first_frame = false;
                    response.request_focus();
                }

                let pasted = checksum::normalize(&self.expected);
                if let Some(Ok(digest)) = &self.digest {
                    if !pasted.is_empty() && pasted.len() == digest.len() {
                        ui.add_space(12.0);
                        if checksum::matches(digest, &self.expected) {
                            widgets::callout(ui, theme::OK, Icon::CheckCircle, "They match", "The file is, byte for byte, the one this checksum was published for.");
                        } else {
                            widgets::callout(ui, theme::BAD, Icon::Alert, "They do not match", "The file was damaged on the way or is not the one this checksum belongs to. Download it again before trusting it.");
                        }
                    } else if !self.expected.trim().is_empty() && pasted.is_empty() {
                        ui.add_space(12.0);
                        widgets::callout(ui, theme::TEXT_2, Icon::Alert, "That does not look like a checksum", "MD5 has 32 characters, SHA-1 has 40 and SHA-256 has 64, all of them 0\u{2013}9 and a\u{2013}f.");
                    }
                }
            });
            footer(ui, |ui| {
                if Button::new("Done", ButtonKind::Secondary).min_width(96.0).show(ui).clicked() {
                    ui.close();
                }
            });
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });

        if let Some(algorithm) = switch {
            self.algorithm = algorithm;
            self.restart(&app.manager);
        }
        if copy && let Some(Ok(digest)) = &self.digest {
            ctx.copy_text(digest.clone());
            app.toast("Checksum copied");
        }
        outcome
    }
}
