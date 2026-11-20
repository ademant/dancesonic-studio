//! DanceSonic Studio: the GTK4 user interface.

mod connection_dialog;
mod mpris;
mod player;
mod secret_service;
mod seek_bar;
mod session;
mod track_feed;
mod window;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;
use studio_core::APP_ID;
use studio_core::db::Database;
use studio_core::secrets::{Persistence, SecretVault};
use tracing_subscriber::EnvFilter;

use crate::secret_service::SecretService;

/// What the app opens at start and keeps for its lifetime.
#[derive(Clone, Debug)]
pub(crate) struct Services {
    vault: Arc<SecretVault<SecretService>>,
    db: Arc<Database>,
}

fn main() -> glib::ExitCode {
    init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    // Network, D-Bus and database work runs here, never on the GTK main thread.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("studio-worker")
        .enable_all()
        .build()
    {
        Ok(runtime) => Arc::new(runtime),
        Err(err) => {
            tracing::error!(%err, "cannot start the background runtime");
            return glib::ExitCode::FAILURE;
        }
    };

    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| activate(app, &runtime));
    app.run()
}

/// Logs to stderr. The level comes from `RUST_LOG`, default `info`.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// Shows the main window, or raises it if the application is already running.
fn activate(app: &gtk::Application, runtime: &Arc<tokio::runtime::Runtime>) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let window = window::Window::new(app, runtime);
    window.window.present();
    start_services(runtime, window);
}

/// Connects to the Secret Service and opens the database in the background,
/// then reports problems in the banner.
fn start_services(runtime: &Arc<tokio::runtime::Runtime>, window: Rc<window::Window>) {
    let db_path = database_path();
    let startup = runtime.spawn(async move {
        let vault = match SecretService::connect().await {
            Ok(backend) => SecretVault::with_backend(backend),
            Err(err) => SecretVault::session_only(err.to_string()),
        };
        let db = tokio::task::spawn_blocking(move || Database::open(db_path)).await;
        (vault, db)
    });

    glib::spawn_future_local(async move {
        let banner = window.banner().clone();
        let (vault, db) = match startup.await {
            Ok(result) => result,
            Err(err) => {
                tracing::error!(%err, "start-up task failed");
                banner.show("Studio could not start its background services.");
                return;
            }
        };
        let mut warnings = Vec::new();
        if let Persistence::SessionOnly { .. } = vault.persistence() {
            warnings.push(
                "No keyring (Secret Service) is available. Sign-ins are kept only until \
                 Studio closes."
                    .to_string(),
            );
        }
        let db = match db {
            Ok(Ok(db)) => Some(Arc::new(db)),
            Ok(Err(err)) => {
                tracing::error!(%err, "cannot open the library database");
                warnings.push(format!("The library database could not be opened: {err}"));
                None
            }
            Err(err) => {
                tracing::error!(%err, "database task failed");
                warnings.push("The library database could not be opened.".to_string());
                None
            }
        };
        if !warnings.is_empty() {
            banner.show(&warnings.join("\n"));
        }
        if let Some(db) = db {
            window.set_services(Services {
                vault: Arc::new(vault),
                db,
            });
        }
    });
}

/// `$XDG_DATA_HOME/dancesonic-studio/library.db`.
fn database_path() -> PathBuf {
    glib::user_data_dir()
        .join("dancesonic-studio")
        .join("library.db")
}

/// A warning line at the top of the window, hidden until needed. Standard
/// widgets only, so it follows the system theme.
#[derive(Clone)]
struct Banner {
    revealer: gtk::Revealer,
    label: gtk::Label,
}

impl Banner {
    fn new() -> Self {
        let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
        let label = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .hexpand(true)
            .selectable(true)
            .build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        row.append(&icon);
        row.append(&label);
        let frame = gtk::Frame::builder().child(&row).build();
        let revealer = gtk::Revealer::builder()
            .child(&frame)
            .reveal_child(false)
            .build();
        Self { revealer, label }
    }

    fn show(&self, text: &str) {
        self.label.set_text(text);
        self.revealer.set_reveal_child(true);
    }
}
