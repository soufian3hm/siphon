//! The queue of deferred downloads and when it runs.

use std::time::Duration;

use egui::{CornerRadius, Rect, Sense, Stroke, StrokeKind, Vec2};
use siphon_core::Manager;
use siphon_core::schedule::{Schedule, WhenDone, format_minute, parse_minute};

use super::{Outcome, footer, modal, padded, small_note, title_bar};
use crate::app::{Action, App};
use crate::format;
use crate::icons::{self, Icon};
use crate::theme::{self, Anchor, Type};
use crate::widgets::{self, Button, ButtonKind, Field};

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

pub struct ScheduleDialog {
    draft: Schedule,
    saved: Schedule,
    start: String,
    stop: String,
}

impl ScheduleDialog {
    pub fn new(manager: &Manager) -> Self {
        let draft = manager.settings().schedule;
        Self { start: format_minute(draft.start), stop: format_minute(draft.stop), saved: draft.clone(), draft }
    }

    /// Applies the draft as soon as it is whole.
    fn commit(&mut self, app: &mut App) {
        let (start, stop) = (parse_minute(&self.start), parse_minute(&self.stop));
        if let (Some(start), Some(stop)) = (start, stop) {
            self.draft.start = start;
            self.draft.stop = stop;
            if self.draft != self.saved {
                let mut settings = app.manager.settings();
                settings.schedule = self.draft.clone();
                if let Err(e) = app.manager.set_settings(settings) {
                    app.toast_error(e.to_string());
                }
                self.saved = self.draft.clone();
            }
        }
    }

    pub(super) fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        self.commit(app);
        let queue = app.manager.queue();
        if queue.running {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        let headline = match (queue.running, queue.waiting, queue.remaining) {
            (true, _, 1) => "The queue is running  \u{00B7}  1 download left".to_owned(),
            (true, _, n) => format!("The queue is running  \u{00B7}  {n} downloads left"),
            (false, 0, _) => "Nothing is waiting in the queue".to_owned(),
            (false, 1, _) => "1 download is waiting".to_owned(),
            (false, n, _) => format!("{n} downloads are waiting"),
        };
        let next = match (self.saved.enabled, queue.next_start_in) {
            (true, Some(minutes)) => format!("Starts by itself at {}, in {}", format_minute(self.saved.start), format::duration(u64::from(minutes) * 60)),
            (true, None) => "No day is switched on, so it will not start by itself".to_owned(),
            (false, _) => "Starts when you say so".to_owned(),
        };

        let outcome = modal(ctx, "schedule", 540.0, |ui| {
            if title_bar(ui, "Queue", Some("Downloads set aside with \u{201C}Download later\u{201D} run together, now or on a schedule.")) {
                return Outcome::Close;
            }
            padded(ui, |ui| {
                // Where things stand, and the one button that changes it.
                let (card, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 64.0), Sense::hover());
                ui.painter().rect(card, CornerRadius::same(theme::R_PANEL), theme::FIELD, Stroke::new(1.0, theme::white(20)), StrokeKind::Inside);
                let tile = Rect::from_min_size(egui::pos2(card.left() + 14.0, card.top() + 14.0), Vec2::splat(36.0));
                ui.painter().rect(tile, CornerRadius::same(8), theme::RAISED, Stroke::new(1.0, theme::white(22)), StrokeKind::Inside);
                icons::paint(ui.painter(), tile, Icon::Clock, 17.0, if queue.running { theme::ACCENT_HI } else { theme::TEXT_2 });
                let text_x = tile.right() + 12.0;
                let text_w = card.right() - text_x - 136.0;
                theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text_x, card.top() + 13.0), Vec2::new(text_w, 20.0)), Anchor::Left, &headline, Type::BODY_MEDIUM, theme::TEXT);
                theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text_x, card.top() + 34.0), Vec2::new(text_w, 16.0)), Anchor::Left, &next, Type::SMALL, theme::TEXT_2);
                let slot = Rect::from_min_size(egui::pos2(card.right() - 14.0 - 112.0, card.center().y - 14.0), Vec2::new(112.0, 28.0));
                theme::place(ui, slot, egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if queue.running {
                        if Button::new("Stop", ButtonKind::Secondary).icon(Icon::Pause).min_width(112.0).show(ui).clicked() {
                            app.actions.push(Action::StopQueue);
                        }
                    } else {
                        ui.add_enabled_ui(queue.waiting > 0, |ui| {
                            if Button::new("Start now", ButtonKind::Primary).icon(Icon::Play).min_width(112.0).show(ui).clicked() {
                                app.actions.push(Action::StartQueue);
                            }
                        });
                    }
                });
                ui.add_space(8.0);

                let start_ok = parse_minute(&self.start).is_some();
                widgets::setting_row_with(ui, "Start by itself", "At this time, on the days chosen below.", 112.0, false, |ui| {
                    widgets::toggle(ui, &mut self.draft.enabled);
                    ui.add_space(4.0);
                    ui.add_enabled_ui(self.draft.enabled, |ui| {
                        Field::new(&mut self.start).hint("02:00").width(64.0).mono().invalid(!start_ok).id(egui::Id::unique("schedule-start")).show(ui);
                    });
                });

                // The week as seven keys.
                ui.add_enabled_ui(self.draft.enabled, |ui| {
                    let gap = 6.0;
                    let width = ((ui.available_width() - gap * 6.0) / 7.0).floor();
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for (index, label) in DAYS.iter().enumerate() {
                            widgets::toggle_key(ui, label, &mut self.draft.days[index], width);
                        }
                    });
                });
                ui.add_space(12.0);
                let (rule, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                ui.painter().hline(rule.x_range(), rule.center().y, Stroke::new(1.0, theme::white(10)));

                let stop_ok = parse_minute(&self.stop).is_some();
                widgets::setting_row(ui, "Stop at", "What the queue started is paused with its data kept, and carries on at the next start.", 112.0, |ui| {
                    widgets::toggle(ui, &mut self.draft.stop_enabled);
                    ui.add_space(4.0);
                    ui.add_enabled_ui(self.draft.stop_enabled, |ui| {
                        Field::new(&mut self.stop).hint("07:00").width(64.0).mono().invalid(!stop_ok).id(egui::Id::unique("schedule-stop")).show(ui);
                    });
                });
                widgets::setting_row_with(ui, "When the queue is done", "After its last download ends. Not when you stop it yourself.", 150.0, false, |ui| {
                    let options: Vec<(WhenDone, &str)> = WhenDone::ALL.iter().map(|w| (*w, w.label())).collect();
                    widgets::dropdown(ui, "schedule-done", &options, &mut self.draft.when_done, 150.0);
                });
                ui.add_space(8.0);
                small_note(ui, "Siphon has to be open for the schedule to run. Times are in your computer\u{2019}s time zone, in 24-hour form.", theme::TEXT_3);
            });
            footer(ui, |ui| {
                if Button::new("Done", ButtonKind::Secondary).min_width(96.0).show(ui).clicked() {
                    ui.close();
                }
            });
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });
        if matches!(outcome, Outcome::Close) {
            self.commit(app);
        }
        outcome
    }
}
