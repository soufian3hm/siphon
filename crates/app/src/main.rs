//! Siphon: a segmented download manager.

// No console window behind the application on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod cli;
mod dialogs;
mod format;
mod icons;
mod inspector;
mod list;
mod logo;
mod setup;
mod shots;
mod sidebar;
mod theme;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use siphon_core::ipc::{self, Message};
use siphon_core::secrets::Secrets;
use siphon_core::Manager;

fn data_dir() -> PathBuf {
    std::env::var_os("SIPHON_DATA_DIR").map(PathBuf::from).unwrap_or_else(siphon_core::settings::default_data_dir)
}

/// Brings the application to the front, restoring its window if needed.
/// Works while the window is hidden and nothing is being drawn.
fn come_forward() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    // A burst of captured links is one reason to come forward, not twenty.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    if now.saturating_sub(LAST.swap(now, Ordering::Relaxed)) < 700 {
        return;
    }
    let bundle = std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok()).and_then(|exe| siphon_core::browser::bundle_of(&exe));
    siphon_core::browser::bring_forward(std::process::id(), bundle.as_deref());
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (messages, raise) = match cli::parse(&args) {
        cli::Invocation::Run { messages, raise } => (messages, raise),
        cli::Invocation::Help => {
            print!("{}", cli::USAGE);
            return Ok(());
        }
        cli::Invocation::Version => {
            println!("Siphon {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        // Renders the application icon at every size a bundle needs. Used
        // by the packaging script.
        cli::Invocation::WriteIcon(dir) => {
            if let Err(e) = logo::write_iconset(&dir) {
                eprintln!("could not write the icon: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        cli::Invocation::Install(folder) => match setup::install(folder.as_deref()) {
            Ok(installed) => {
                println!("{}", installed.display());
                return Ok(());
            }
            Err(e) => {
                eprintln!("siphon: {e}");
                std::process::exit(1);
            }
        },
        cli::Invocation::Browsers => {
            let found = setup::survey(&data_dir());
            if found.is_empty() {
                println!("No browser that can run the extension was found.");
            }
            for state in found {
                println!(
                    "{:<22}{:<16}{}",
                    state.browser.name,
                    if state.connected { "connected" } else { "not connected" },
                    if state.extension { "extension added" } else { "extension not added" }
                );
            }
            return Ok(());
        }
        cli::Invocation::Where => {
            let bundle = setup::bundle().map(|bundle| bundle.display().to_string()).unwrap_or_else(|| "not in an application bundle".to_owned());
            println!("{bundle}\n{}", if setup::needs_installing() { "on a disk image: to be moved" } else { "in place" });
            return Ok(());
        }
        cli::Invocation::Usage(problem) => {
            eprintln!("siphon: {problem}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };

    let data_dir = data_dir();
    let script = shots::Script::from_env();

    // One window per user: a second launch hands its requests to the first.
    if script.is_none() && ipc::send(&data_dir, &Message::Ping).is_ok() {
        let mut failed = false;
        for message in &messages {
            if let Err(e) = ipc::send(&data_dir, message) {
                eprintln!("siphon: {e}");
                failed = true;
            }
        }
        if raise {
            let _ = ipc::send(&data_dir, &Message::Raise);
        }
        if failed {
            std::process::exit(1);
        }
        return Ok(());
    }

    let viewport = egui::ViewportBuilder::default()
        .with_title("Siphon")
        .with_app_id("app.siphon.Siphon")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([1000.0, 620.0])
        .with_icon(Arc::new(logo::icon_data(256)))
        // On macOS the content runs under a transparent title bar, so the
        // window controls sit inside the sidebar.
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    let options = eframe::NativeOptions { viewport, ..Default::default() };

    let retire_dir = data_dir.clone();
    let result = eframe::run_native(
        "Siphon",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            // A scratch data directory is a test run: it keeps its passwords
            // in memory and leaves the user's keychain alone.
            let secrets = if std::env::var_os("SIPHON_DATA_DIR").is_some() { Secrets::in_memory() } else { Secrets::system() };
            let manager = Arc::new(Manager::open_with(&data_dir, secrets, move || ctx.request_repaint())?);
            if let Err(e) = ipc::serve(&manager) {
                eprintln!("the browser bridge could not start: {e}");
            }
            // Keep every browser's pointer to the helper current, e.g. after
            // the application was moved, and the extension folder with it.
            // (Not when pointed at a scratch data directory: that is a test
            // run and must leave the user's browsers alone. And not from a
            // disk image: the pointer would die with the image.)
            let installing = setup::needs_installing();
            if std::env::var_os("SIPHON_DATA_DIR").is_none() && !installing {
                setup::connect_browsers(&data_dir);
            }
            // A covered or minimised window is not drawn, and what is not
            // drawn cannot bring itself forward: that goes through the system.
            manager.on_attention(come_forward);
            if script.is_some() {
                // A scripted run needs its frames drawn, so its window must
                // be in front. Asked for once the event loop is running.
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    come_forward();
                });
            }
            // Nothing was running, so this launch is the application: it
            // carries out its own command line.
            for message in messages {
                ipc::dispatch(&manager, message);
            }
            let mut app = app::App::new(manager, script);
            app.first_run(installing);
            Ok(Box::new(app))
        }),
    );
    ipc::retire(&retire_dir);
    result
}
