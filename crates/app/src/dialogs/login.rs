//! Sign in to a site a download is waiting on.

use egui::{Key, Sense, Vec2};
use siphon_core::DownloadId;

use super::{Outcome, field_label, footer, modal, padded, small_note, title_bar};
use crate::app::App;
use crate::theme::{self, Type};
use crate::widgets::{Button, ButtonKind, Field};

pub struct LoginDialog {
    id: DownloadId,
    host: String,
    user: String,
    password: String,
    error: Option<String>,
    first_frame: bool,
}

impl LoginDialog {
    pub fn new(id: DownloadId, url: &str) -> Self {
        Self { id, host: siphon_core::organize::host_of(url).unwrap_or_default(), user: String::new(), password: String::new(), error: None, first_frame: true }
    }

    pub(super) fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        if app.view(self.id).is_none() || self.host.is_empty() {
            return Outcome::Close;
        }
        let mut submit = false;
        let outcome = modal(ctx, "login", 440.0, |ui| {
            if title_bar(ui, "Sign in", Some(&self.host)) {
                return Outcome::Close;
            }
            padded(ui, |ui| {
                let text = "This server asks for an account before it sends the file. The login is remembered for this site; the password is kept in your keychain, not in Siphon\u{2019}s settings.";
                let galley = theme::layout_wrapped(ui.painter(), text, Type::BODY, theme::TEXT_2, ui.available_width(), 5);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y), Sense::hover());
                ui.painter().galley(rect.min, galley, theme::TEXT_2);
                ui.add_space(16.0);

                field_label(ui, "USER NAME");
                let response = Field::new(&mut self.user).id(egui::Id::unique("login-user")).show(ui);
                if self.first_frame {
                    self.first_frame = false;
                    response.request_focus();
                }
                ui.add_space(12.0);
                field_label(ui, "PASSWORD");
                let response = Field::new(&mut self.password).password().id(egui::Id::unique("login-password")).show(ui);
                if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    submit = true;
                }
                if let Some(error) = &self.error {
                    ui.add_space(10.0);
                    small_note(ui, error, theme::BAD);
                }
            });
            footer(ui, |ui| {
                ui.add_enabled_ui(!self.user.trim().is_empty(), |ui| {
                    if Button::new("Sign in and continue", ButtonKind::Primary).min_width(170.0).show(ui).clicked() {
                        submit = true;
                    }
                });
                if Button::new("Cancel", ButtonKind::Ghost).show(ui).clicked() {
                    ui.close();
                }
            });
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });

        if submit && !self.user.trim().is_empty() {
            match app.manager.save_login(&self.host, self.user.trim(), &self.password) {
                Ok(()) => {
                    app.manager.retry(self.id);
                    app.toast(format!("Signed in to {}", self.host));
                    return Outcome::Close;
                }
                Err(e) => self.error = Some(format!("The login could not be saved: {e}")),
            }
        }
        outcome
    }
}
