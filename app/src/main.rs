//! DanceSonic Studio: the GTK4 user interface.

use gtk::glib;
use gtk::prelude::*;
use studio_core::{APP_ID, APP_NAME};
use tracing_subscriber::EnvFilter;

fn main() -> glib::ExitCode {
    init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

/// Logs to stderr. The level comes from `RUST_LOG`, default `info`.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// Shows the main window, or raises it if the application is already running.
fn build_window(app: &gtk::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(APP_NAME)
        .default_width(1200)
        .default_height(800)
        .build();
    window.present();
}
