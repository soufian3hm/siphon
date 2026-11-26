//! Add many downloads at once: pasted lists, numbered series, imported files.

use std::path::PathBuf;

use egui::{CornerRadius, Frame, Rect, Sense, Stroke, Vec2};
use siphon_core::{Manager, NewDownload, Request, Settings, batch, filename};

use super::{Outcome, field_label, folder_row, footer, modal, padded, small_note, title_bar};
use crate::app::App;
use crate::format;
use crate::icons::Icon;
use crate::theme::{self, Anchor, Type};
use crate::widgets::{self, Button, ButtonKind};

pub struct BatchDialog {
    text: String,
    /// The text `links` was worked out from.
    parsed: Option<String>,
    links: Vec<String>,
    problem: Option<String>,
    dir: String,
    dir_touched: bool,
    first_frame: bool,
    settings: Settings,
}

impl BatchDialog {
    pub fn new(manager: &Manager, text: String) -> Self {
        let settings = manager.settings();
        Self {
            text,
            parsed: None,
            links: Vec::new(),
            problem: None,
            dir: settings.download_dir.to_string_lossy().into_owned(),
            dir_touched: false,
            first_frame: true,
            settings,
        }
    }

    fn parse(&mut self) {
        if self.parsed.as_deref() == Some(self.text.as_str()) {
            return;
        }
        let (links, problem) = batch::collect(&self.text);
        self.links = links;
        self.problem = problem.map(|p| {
            let text = p.to_string();
            let mut chars = text.chars();
            chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or(text) + "."
        });
        self.parsed = Some(self.text.clone());
    }

    pub(super) fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        self.parse();
        let count = self.links.len();
        let mut go: Option<bool> = None;
        let mut import = false;
        let outcome = modal(ctx, "batch", 600.0, |ui| {
            if title_bar(ui, "Add many downloads", Some("One address per line. [1-20] and {a,b,c} stand for a series.")) {
                return Outcome::Close;
            }
            padded(ui, |ui| {
                field_label(ui, "ADDRESSES");
                let id = egui::Id::unique("batch-text");
                let response = widgets::text_area(ui, &mut self.text, "https://example.com/photos/img[001-120].jpg", 132.0, id);
                if self.first_frame {
                    self.first_frame = false;
                    response.request_focus();
                }
                ui.add_space(12.0);

                if let Some(problem) = &self.problem {
                    widgets::callout(ui, theme::WARN, Icon::Alert, "Part of this was left out", problem);
                    ui.add_space(12.0);
                }

                // What the text stands for, so a pattern can be checked before
                // a hundred downloads start.
                let summary = match count {
                    0 => "NOTHING TO ADD YET".to_owned(),
                    1 => "1 DOWNLOAD".to_owned(),
                    n => format!("{n} DOWNLOADS"),
                };
                field_label(ui, &summary);
                Frame::new().fill(theme::FIELD).stroke(Stroke::new(1.0, theme::white(16))).corner_radius(CornerRadius::same(theme::R_PANEL)).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_height(110.0);
                    if self.links.is_empty() {
                        let rect = ui.max_rect();
                        theme::text_in(ui.painter(), rect, Anchor::Center, "Addresses you type or paste are listed here", Type::SMALL, theme::TEXT_3);
                        return;
                    }
                    egui::ScrollArea::vertical().id_salt("batch-preview").auto_shrink([false, false]).show_rows(ui, 22.0, self.links.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for link in &self.links[range] {
                            let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::hover());
                            let name = telling_name(link);
                            let host_w = 190.0;
                            theme::text_in(ui.painter(), Rect::from_min_max(row.min, egui::pos2(row.right() - host_w - 22.0, row.bottom())), Anchor::Left, &name, Type::SMALL, theme::TEXT);
                            theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(row.right() - host_w - 10.0, row.top()), egui::pos2(row.right() - 10.0, row.bottom())), Anchor::Right, &format::host(link), Type::SMALL, theme::TEXT_3);
                        }
                    });
                });
                ui.add_space(16.0);

                field_label(ui, if self.settings.sort_by_type && !self.dir_touched { "SAVE TO  \u{00B7}  SORTED BY TYPE" } else { "SAVE TO" });
                folder_row(ui, &mut self.dir, &mut self.dir_touched);
                if self.settings.sort_by_type && !self.dir_touched {
                    ui.add_space(6.0);
                    small_note(ui, "Each file goes to the folder for its type. Pick a folder here to keep them together instead.", theme::TEXT_3);
                }
            });

            footer(ui, |ui| {
                ui.add_enabled_ui(count > 0 && !self.dir.trim().is_empty(), |ui| {
                    let label = match count {
                        0 | 1 => "Download".to_owned(),
                        n => format!("Download {n}"),
                    };
                    if Button::new(&label, ButtonKind::Primary).icon(Icon::Download).min_width(124.0).show(ui).clicked() {
                        go = Some(false);
                    }
                    if Button::new("Download later", ButtonKind::Secondary).icon(Icon::Clock).show(ui).on_hover_text("Keep them for the queue. They start when the queue does.").clicked() {
                        go = Some(true);
                    }
                });
                if Button::new("Cancel", ButtonKind::Ghost).show(ui).clicked() {
                    ui.close();
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if Button::new("Import from a file\u{2026}", ButtonKind::Ghost).icon(Icon::Document).show(ui).clicked() {
                        import = true;
                    }
                });
            });
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });

        if import && let Some(text) = pick_links_file() {
            match text {
                Ok(text) => {
                    if !self.text.trim().is_empty() && !self.text.ends_with('\n') {
                        self.text.push('\n');
                    }
                    // Keep only the addresses: the file may be a whole web page.
                    self.text.push_str(&batch::extract_links(&text).join("\n"));
                }
                Err(e) => app.toast_error(e),
            }
        }
        if let Some(later) = go {
            // With sorting on and no folder chosen, each file finds its own.
            let dir = (self.dir_touched || !self.settings.sort_by_type).then(|| PathBuf::from(self.dir.trim()));
            let ids = app.manager.add_many(self.links.iter().map(|link| NewDownload { request: Request::new(link.clone()), dir: dir.clone(), later, ..Default::default() }).collect());
            app.toast(match (ids.len(), later) {
                (1, false) => "Added 1 download".to_owned(),
                (n, false) => format!("Added {n} downloads"),
                (1, true) => "Kept 1 download for the queue".to_owned(),
                (n, true) => format!("Kept {n} downloads for the queue"),
            });
            if later {
                app.filter = crate::app::Filter::Scheduled;
            }
            return Outcome::Close;
        }
        outcome
    }
}

/// The file name with the folder it sits in, so the members of a series
/// (`week01/slides.pdf`, `week02/slides.pdf`) can be told apart.
fn telling_name(link: &str) -> String {
    let name = filename::from_url(link).unwrap_or_else(|| link.to_owned());
    let folder = url::Url::parse(link).ok().and_then(|url| {
        let segments: Vec<String> = url.path_segments()?.filter(|s| !s.is_empty()).map(str::to_owned).collect();
        (segments.len() >= 2).then(|| segments[segments.len() - 2].clone())
    });
    match folder {
        Some(folder) => format!("{folder}/{name}"),
        None => name,
    }
}

/// Lets the user choose a text or HTML file and reads it. `None` when they
/// cancelled.
pub fn pick_links_file() -> Option<Result<String, String>> {
    const LIMIT: u64 = 32 * 1024 * 1024;
    let path = rfd::FileDialog::new().set_title("Import links").add_filter("Text and web pages", &["txt", "html", "htm", "csv", "json", "md", "lst", "list"]).add_filter("Any file", &["*"]).pick_file()?;
    Some(match std::fs::metadata(&path) {
        Ok(meta) if meta.len() > LIMIT => Err("That file is too large to be a list of links".to_owned()),
        Ok(_) => std::fs::read(&path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).map_err(|e| format!("Could not read the file: {e}")),
        Err(e) => Err(format!("Could not read the file: {e}")),
    })
}
