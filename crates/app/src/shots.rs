//! Scripted screenshots.
//!
//! When `SIPHON_SHOTS` names a plan file, the application follows it: each
//! line is `<seconds> <command> [argument]`, executed once the given time
//! since launch has passed. It exists so the interface can be checked
//! against real engine state without anyone clicking through it by hand.
//!
//! Further commands, added with the queue and the link tools: `later <url>`,
//! `batch <text>`, `grab <url>`, `explore <depth>`, `pick <count>`, `peek`,
//! `schedule`, `queue <start|stop>`, `verify <row>`, `site <host>|<limit>|<user>`,
//! `sort`, `countdown`, `filter scheduled`, `install` (the move-to-Applications
//! question).
//!
//! Commands: `add <url>`, `select <row>`, `deselect`, `filter <all|active|done|attention>`,
//! `new <url>`, `capture <url>|<name>|<size>|<page title>`, `swap <row>`,
//! `settings <tab>`, `remove <row>`, `pause <row>`, `resume <row>`,
//! `search <text>`, `close`, `shot <file.png>`, `quit`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use siphon_core::{Capture, Manager, NewDownload, Request};

use crate::app::{Action, App, Filter};
use crate::dialogs::{self, Dialog};

/// Writes a rendered frame as a PNG.
pub fn save_png(image: &egui::ColorImage, path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let [width, height] = image.size;
    let buffer = image::RgbaImage::from_raw(width as u32, height as u32, image.as_raw().to_vec())
        .ok_or("frame has an unexpected size")?;
    buffer.save(path).map_err(|e| e.to_string())
}

struct Step {
    at: f32,
    command: String,
    argument: String,
}

pub struct Script {
    steps: Vec<Step>,
    next: usize,
    started: Instant,
    out_dir: PathBuf,
}

impl Script {
    pub fn from_env() -> Option<Self> {
        let plan = PathBuf::from(std::env::var_os("SIPHON_SHOTS")?);
        let text = std::fs::read_to_string(&plan).ok()?;
        let steps = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|line| {
                let mut parts = line.splitn(3, ' ');
                let at = parts.next()?.parse().ok()?;
                let command = parts.next()?.to_owned();
                Some(Step { at, command, argument: parts.next().unwrap_or("").trim().to_owned() })
            })
            .collect();
        Some(Self { steps, next: 0, started: Instant::now(), out_dir: plan.parent().map(Path::to_owned).unwrap_or_default() })
    }

    /// Keeps frames coming while there are steps left.
    pub fn before_frame(&mut self, ctx: &egui::Context, _manager: &Manager) {
        if self.next < self.steps.len() {
            ctx.request_repaint();
        }
    }

    pub fn after_frame(&mut self, app: &mut App, ctx: &egui::Context) {
        while let Some(step) = self.steps.get(self.next) {
            if self.started.elapsed().as_secs_f32() < step.at {
                break;
            }
            self.next += 1;
            let arg = step.argument.as_str();
            let row = |app: &App| arg.parse::<usize>().ok().and_then(|i| app.visible.get(i).copied());
            match step.command.as_str() {
                "add" => {
                    app.manager.add(NewDownload::new(Request::new(arg)));
                }
                "select" => {
                    if let Some(id) = row(app) {
                        app.select_only(id);
                    }
                }
                "deselect" => {
                    app.selection.clear();
                    app.focus = None;
                }
                "filter" => {
                    app.filter = match arg {
                        "active" => Filter::Active,
                        "done" => Filter::Completed,
                        "attention" => Filter::Attention,
                        "scheduled" => Filter::Scheduled,
                        _ => Filter::All,
                    }
                }
                "new" => app.actions.push(Action::NewDownload(Some(arg.to_owned()))),
                "capture" => {
                    let mut parts = arg.split('|');
                    let url = parts.next().unwrap_or("").to_owned();
                    let name = parts.next().map(str::to_owned).filter(|s| !s.is_empty());
                    let size = parts.next().and_then(|s| s.parse().ok());
                    let page = parts.next().map(str::to_owned);
                    app.manager.capture(Capture {
                        request: Request::new(url).with_referer("https://example.com/downloads"),
                        suggested_name: name,
                        size,
                        page_title: page,
                        ..Default::default()
                    });
                }
                "swap" => {
                    if let Some(id) = row(app) {
                        app.actions.push(Action::SwapLink(id));
                    }
                }
                "install" => {
                    app.close_dialog();
                    app.dialog = Some(Dialog::Install(dialogs::InstallDialog::default()));
                }
                "settings" => {
                    app.close_dialog();
                    app.dialog = Some(Dialog::Settings(Box::new(dialogs::SettingsDialog::new(&app.manager).on_tab(arg.parse().unwrap_or(0)))));
                }
                "remove" => {
                    if let Some(id) = row(app) {
                        app.actions.push(Action::AskRemove(vec![id]));
                    }
                }
                "pause" => {
                    if let Some(id) = row(app) {
                        app.manager.pause(id);
                    }
                }
                "resume" => {
                    if let Some(id) = row(app) {
                        app.manager.resume(id);
                    }
                }
                "later" => {
                    app.manager.add(NewDownload { request: Request::new(arg), later: true, ..Default::default() });
                }
                "batch" => app.actions.push(Action::Batch(arg.replace("\\n", "\n"))),
                "grab" => app.actions.push(Action::GrabLinks(arg.to_owned())),
                "schedule" => app.actions.push(Action::Schedule),
                "queue" => app.actions.push(if arg == "stop" { Action::StopQueue } else { Action::StartQueue }),
                "verify" => {
                    if let Some(id) = row(app) {
                        app.actions.push(Action::Verify(id));
                    }
                }
                "minimize" => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                "confirm" => {
                    if let Some(Dialog::Add(mut dialog)) = app.dialog.take() {
                        dialog.script_confirm(app);
                    }
                }
                "peek" => {
                    let manager = app.manager.clone();
                    if let Some(Dialog::Add(dialog)) = app.dialog.as_mut() {
                        dialog.script_peek(&manager);
                    }
                }
                "explore" => {
                    if let Some(Dialog::Grab(mut dialog)) = app.dialog.take() {
                        dialog.script_explore(app, arg.parse().unwrap_or(0));
                        app.dialog = Some(Dialog::Grab(dialog));
                    }
                }
                "pick" => {
                    if let Some(Dialog::Grab(dialog)) = app.dialog.as_mut() {
                        dialog.script_pick(siphon_core::Category::Image, arg.parse().unwrap_or(3));
                    }
                }
                "site" => {
                    let mut parts = arg.split('|');
                    let mut settings = app.manager.settings();
                    settings.sites.push(siphon_core::organize::SiteRule {
                        pattern: parts.next().unwrap_or("").to_owned(),
                        connections: parts.next().and_then(|n| n.parse().ok()),
                        username: parts.next().unwrap_or("").to_owned(),
                    });
                    let _ = app.manager.set_settings(settings);
                }
                "sort" => {
                    let mut settings = app.manager.settings();
                    settings.sort_by_type = true;
                    let _ = app.manager.set_settings(settings);
                }
                // Quit, never Sleep: a stalled script must not put the machine to sleep.
                "countdown" => {
                    app.countdown = match arg {
                        "off" => None,
                        _ => Some(crate::app::Countdown { action: siphon_core::schedule::WhenDone::Quit, until: std::time::Instant::now() + std::time::Duration::from_secs(600), completed: 6, failed: 0 }),
                    }
                }
                "search" => app.search = arg.to_owned(),
                "close" => app.close_dialog(),
                "shot" => {
                    let path = self.out_dir.join(arg);
                    ctx.request_screenshot(move |image| {
                        if let Err(e) = save_png(&image, &path) {
                            eprintln!("screenshot failed: {e}");
                        }
                    });
                }
                "quit" => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                other => eprintln!("unknown script command: {other}"),
            }
        }
    }
}
