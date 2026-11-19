//! The dialog where a user connects Studio to a folksonic server (#26).
//!
//! Pairing comes first. A pasted pairing link fills in the server address and
//! shows the verification code once the pairing has started. Password and
//! API-key sign-in are behind "Advanced" and are never the default.
//!
//! Network and database work runs on the background runtime. Each result comes
//! back to the main thread through a glib future, and widgets are touched only
//! there.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;
use secrecy::SecretString;
use studio_core::account;
use studio_core::catalog;
use studio_core::connect::{self, Connected};
use studio_core::connection::{HttpPolicy, ServerUrl};
use studio_core::http::{self, HttpSettings};
use studio_core::pairing::{self, Confirmation, InputError, PendingPairing};
use studio_core::secrets::Persistence;
use studio_core::sign_in;
use studio_core::subsonic::{ApiKey, Auth};

use crate::Services;

/// Opens the connection dialog over `parent`.
pub(crate) fn present(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    services: Services,
) {
    let dialog = Dialog::new(parent, Arc::clone(runtime), services);
    dialog.window.present();
}

/// The settings for one HTTP client, from the warned opt-in checkbox.
fn settings(policy: HttpPolicy) -> HttpSettings {
    HttpSettings {
        policy,
        pinned_certificate_pem: None,
    }
}

/// A button aligned to the end of a vertical box.
fn end_aligned(button: &gtk::Button) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::End)
        .build();
    row.append(button);
    row
}

/// What the user sees while a background task runs.
enum Busy {
    Idle,
    Working(&'static str),
}

struct Dialog {
    window: gtk::Window,
    runtime: Arc<tokio::runtime::Runtime>,
    services: Services,
    optin: gtk::CheckButton,
    paste: gtk::Entry,
    server: gtk::Entry,
    note: gtk::Label,
    pair_button: gtk::Button,
    pairing_box: gtk::Box,
    code: gtk::Label,
    countdown: gtk::Label,
    matches_button: gtk::Button,
    cancel_button: gtk::Button,
    username: gtk::Entry,
    password: gtk::PasswordEntry,
    sign_in_button: gtk::Button,
    api_key: gtk::PasswordEntry,
    api_key_button: gtk::Button,
    sign_out_button: gtk::Button,
    spinner: gtk::Spinner,
    status: gtk::Label,
    /// A pairing waiting for the verification code to be confirmed.
    pending: RefCell<Option<PendingPairing>>,
    /// The task that completes a confirmed pairing; cancelling aborts it.
    running: RefCell<Option<tokio::task::AbortHandle>>,
    expires_at_ms: Cell<Option<i64>>,
}

impl Dialog {
    fn new(
        parent: &gtk::ApplicationWindow,
        runtime: Arc<tokio::runtime::Runtime>,
        services: Services,
    ) -> Rc<Self> {
        let window = gtk::Window::builder()
            .transient_for(parent)
            .modal(true)
            .title("Connect to a server")
            .default_width(520)
            .build();

        let optin = gtk::CheckButton::with_label(
            "Allow plain http to a server on my private network (not encrypted)",
        );
        let paste = gtk::Entry::builder()
            .placeholder_text("Paste the pairing link from the account page")
            .hexpand(true)
            .build();
        let server = gtk::Entry::builder()
            .placeholder_text("Server address, such as https://music.example")
            .hexpand(true)
            .build();
        let note = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .build();
        let pair_button = gtk::Button::with_label("Pair this desktop");
        pair_button.add_css_class("suggested-action");
        pair_button.set_sensitive(false);

        let code = gtk::Label::builder().selectable(true).xalign(0.5).build();
        code.add_css_class("title-1");
        let countdown = gtk::Label::builder().xalign(0.5).build();
        let matches_button = gtk::Button::with_label("Code matches");
        matches_button.add_css_class("suggested-action");
        let cancel_button = gtk::Button::with_label("Cancel pairing");
        let pairing_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .visible(false)
            .build();
        pairing_box.append(&gtk::Label::builder()
            .label("Compare this code with the one on the account page. Confirm only if they match.")
            .wrap(true)
            .xalign(0.0)
            .build());
        pairing_box.append(&code);
        pairing_box.append(&countdown);
        let confirm_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .build();
        confirm_row.append(&cancel_button);
        confirm_row.append(&matches_button);
        pairing_box.append(&confirm_row);

        let username = gtk::Entry::builder().placeholder_text("Username").build();
        let password = gtk::PasswordEntry::builder()
            .placeholder_text("Password")
            .show_peek_icon(true)
            .build();
        let sign_in_button = gtk::Button::with_label("Sign in with password");
        let api_key = gtk::PasswordEntry::builder()
            .placeholder_text("API key")
            .show_peek_icon(true)
            .build();
        let api_key_button = gtk::Button::with_label("Connect with API key");
        let sign_out_button = gtk::Button::with_label("Sign out of this server");
        let advanced = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        advanced.append(&gtk::Label::builder()
            .label("Only needed when pairing is not possible. The password is used once to create an API key and is not stored.")
            .wrap(true)
            .xalign(0.0)
            .build());
        advanced.append(&username);
        advanced.append(&password);
        advanced.append(&end_aligned(&sign_in_button));
        advanced.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        advanced.append(&api_key);
        advanced.append(&end_aligned(&api_key_button));
        let expander = gtk::Expander::builder()
            .label("Advanced")
            .child(&advanced)
            .build();

        let spinner = gtk::Spinner::new();
        let status = gtk::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .build();
        let footer = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        footer.append(&spinner);
        footer.append(&status);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        body.append(&gtk::Label::builder()
            .label("Connect Studio to your folksonic server. Open the account page on the server, create a pairing link and paste it here.")
            .wrap(true)
            .xalign(0.0)
            .build());
        body.append(&paste);
        body.append(&note);
        body.append(&server);
        body.append(&optin);
        body.append(&pair_button);
        body.append(&pairing_box);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&expander);
        body.append(&sign_out_button);
        body.append(&footer);
        window.set_child(Some(&body));

        if let Persistence::SessionOnly { reason } = services.vault.persistence() {
            status.set_text(&format!(
                "No keyring is available, so the connection is kept only until Studio closes. ({reason})"
            ));
        }

        let dialog = Rc::new(Self {
            window,
            runtime,
            services,
            optin,
            paste,
            server,
            note,
            pair_button,
            pairing_box,
            code,
            countdown,
            matches_button,
            cancel_button,
            username,
            password,
            sign_in_button,
            api_key,
            api_key_button,
            sign_out_button,
            spinner,
            status,
            pending: RefCell::new(None),
            running: RefCell::new(None),
            expires_at_ms: Cell::new(None),
        });
        dialog.connect_signals();
        dialog
    }

    fn connect_signals(self: &Rc<Self>) {
        let me = Rc::clone(self);
        self.paste.connect_changed(move |_| me.on_paste_changed());
        let me = Rc::clone(self);
        self.server.connect_changed(move |_| me.on_paste_changed());
        let me = Rc::clone(self);
        self.optin.connect_toggled(move |_| me.on_paste_changed());
        let me = Rc::clone(self);
        self.pair_button.connect_clicked(move |_| me.on_pair());
        let me = Rc::clone(self);
        self.matches_button
            .connect_clicked(move |_| me.on_code_matches());
        let me = Rc::clone(self);
        self.cancel_button
            .connect_clicked(move |_| me.on_cancel_pairing());
        let me = Rc::clone(self);
        self.sign_in_button
            .connect_clicked(move |_| me.on_password_sign_in());
        let me = Rc::clone(self);
        self.api_key_button
            .connect_clicked(move |_| me.on_api_key_connect());
        let me = Rc::clone(self);
        self.sign_out_button
            .connect_clicked(move |_| me.on_sign_out());
        self.on_paste_changed();
    }

    fn policy(&self) -> HttpPolicy {
        if self.optin.is_active() {
            HttpPolicy::ALLOW_PRIVATE_HTTP
        } else {
            HttpPolicy::HTTPS_ONLY
        }
    }

    /// Checks what was pasted as the user types, and says what it is. A pasted
    /// pairing link also fills in the server address; the user confirms that
    /// address with the Pair button.
    fn on_paste_changed(&self) {
        let text = self.paste.text();
        match pairing::parse_input(&text, self.policy()) {
            Ok(input) => {
                if let Some(server) = &input.server {
                    self.server.set_text(&server.key());
                }
                self.note.set_text(
                    "Pairing link recognised. Check the server address, then press Pair this desktop.",
                );
                self.pair_button.set_sensitive(self.is_idle());
            }
            Err(InputError::Empty) => {
                self.note.set_text("");
                self.pair_button.set_sensitive(false);
            }
            Err(error) => {
                self.note.set_text(&error.to_string());
                self.pair_button.set_sensitive(false);
            }
        }
    }

    fn is_idle(&self) -> bool {
        self.running.borrow().is_none()
    }

    fn set_busy(&self, busy: Busy) {
        let working = matches!(busy, Busy::Working(_));
        match busy {
            Busy::Idle => {
                self.spinner.stop();
                self.status.set_text("");
            }
            Busy::Working(text) => {
                self.spinner.start();
                self.status.set_text(text);
            }
        }
        for button in [
            &self.pair_button,
            &self.sign_in_button,
            &self.api_key_button,
            &self.sign_out_button,
        ] {
            button.set_sensitive(!working);
        }
    }

    fn show_status(&self, text: &str) {
        self.spinner.stop();
        self.status.set_text(text);
    }

    fn server_address(&self) -> Result<ServerUrl, String> {
        ServerUrl::parse(&self.server.text(), self.policy()).map_err(|e| e.to_string())
    }

    /// Starts a pairing: sends the public key with the link's token and then
    /// shows the verification code.
    fn on_pair(self: &Rc<Self>) {
        let policy = self.policy();
        let token = match pairing::parse_input(&self.paste.text(), policy) {
            Ok(input) => input.token,
            Err(error) => return self.show_status(&error.to_string()),
        };
        let server = match self.server_address() {
            Ok(server) => server,
            Err(message) => return self.show_status(&message),
        };
        self.set_busy(Busy::Working("Asking the server for a pairing…"));
        let task = self.runtime.spawn(async move {
            let http = http::client(&settings(policy)).map_err(|e| e.to_string())?;
            PendingPairing::begin(&http, server, token)
                .await
                .map_err(|e| e.to_string())
        });
        let me = Rc::clone(self);
        glib::spawn_future_local(async move {
            let outcome = task
                .await
                .unwrap_or_else(|_| Err("The pairing task stopped.".into()));
            me.set_busy(Busy::Idle);
            match outcome {
                Ok(pending) => me.show_code(pending),
                Err(message) => me.show_status(&message),
            }
        });
    }

    fn show_code(self: &Rc<Self>, pending: PendingPairing) {
        self.code.set_text(pending.code());
        self.expires_at_ms.set(pending.expires_at_ms());
        self.pairing_box.set_visible(true);
        self.pair_button.set_sensitive(false);
        self.show_status("Compare the code, then confirm.");
        *self.pending.borrow_mut() = Some(pending);
        self.update_countdown();

        let weak: Weak<Self> = Rc::downgrade(self);
        glib::timeout_add_seconds_local(1, move || {
            let Some(me) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if me.pending.borrow().is_none() {
                return glib::ControlFlow::Break;
            }
            me.update_countdown();
            glib::ControlFlow::Continue
        });
    }

    fn update_countdown(&self) {
        let Some(expires) = self.expires_at_ms.get() else {
            self.countdown.set_text("");
            return;
        };
        let left = (expires - studio_core::sync::now_ms()) / 1000;
        if left <= 0 {
            self.countdown
                .set_text("This code has expired. Create a new pairing link.");
            self.matches_button.set_sensitive(false);
        } else {
            self.countdown
                .set_text(&format!("Expires in {}:{:02}", left / 60, left % 60));
        }
    }

    /// The owner's code matches: confirm, wait for the account page if it has
    /// not confirmed yet, then connect with the device key.
    fn on_code_matches(self: &Rc<Self>) {
        let Some(pending) = self.pending.borrow_mut().take() else {
            return;
        };
        self.pairing_box.set_visible(false);
        let policy = self.policy();
        self.set_busy(Busy::Working(
            "Waiting for the confirmation on the account page…",
        ));
        let services = self.services.clone();
        let task = self.runtime.spawn(async move {
            let http = http::client(&settings(policy)).map_err(|e| e.to_string())?;
            let confirmation = pending
                .answer(&http, true)
                .await
                .map_err(|e| e.to_string())?;
            if let Confirmation::WaitingForOwner = confirmation {
                pending
                    .wait_for_owner(&http)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let auth = Auth::Device(pending.device_key().clone());
            connect::connect(
                &settings(policy),
                services.vault.as_ref(),
                Arc::clone(&services.db),
                pending.server().clone(),
                auth,
            )
            .await
            .map_err(|e| e.to_string())
        });
        self.finish_connection(task);
    }

    /// Cancels a confirmed-but-waiting pairing. The link stays usable on the
    /// server, as it does when the phone cancels.
    fn on_cancel_pairing(self: &Rc<Self>) {
        self.pending.borrow_mut().take();
        self.expires_at_ms.set(None);
        self.pairing_box.set_visible(false);
        if let Some(handle) = self.running.borrow_mut().take() {
            handle.abort();
        }
        self.set_busy(Busy::Idle);
        self.show_status("Pairing cancelled. The pairing link is still valid until it expires.");
        self.on_paste_changed();
    }

    fn on_password_sign_in(self: &Rc<Self>) {
        let policy = self.policy();
        let server = match self.server_address() {
            Ok(server) => server,
            Err(message) => return self.show_status(&message),
        };
        let username = self.username.text().to_string();
        let password = SecretString::from(self.password.text().to_string());
        self.password.set_text("");
        self.set_busy(Busy::Working("Signing in…"));
        let services = self.services.clone();
        let task = self.runtime.spawn(async move {
            let http = http::client(&settings(policy)).map_err(|e| e.to_string())?;
            let key = sign_in::exchange_password(&http, &server, &username, &password)
                .await
                .map_err(|e| e.to_string())?;
            connect::connect_with_api_key(
                &settings(policy),
                services.vault.as_ref(),
                Arc::clone(&services.db),
                server,
                key,
            )
            .await
            .map_err(|e| e.to_string())
        });
        self.finish_connection(task);
    }

    fn on_api_key_connect(self: &Rc<Self>) {
        let policy = self.policy();
        let server = match self.server_address() {
            Ok(server) => server,
            Err(message) => return self.show_status(&message),
        };
        let key = match ApiKey::new(self.api_key.text().to_string()) {
            Ok(key) => key,
            Err(error) => return self.show_status(&error.to_string()),
        };
        self.api_key.set_text("");
        self.set_busy(Busy::Working("Connecting…"));
        let services = self.services.clone();
        let task = self.runtime.spawn(async move {
            connect::connect_with_api_key(
                &settings(policy),
                services.vault.as_ref(),
                Arc::clone(&services.db),
                server,
                key,
            )
            .await
            .map_err(|e| e.to_string())
        });
        self.finish_connection(task);
    }

    fn on_sign_out(self: &Rc<Self>) {
        let server = match self.server_address() {
            Ok(server) => server,
            Err(message) => return self.show_status(&message),
        };
        self.set_busy(Busy::Working("Signing out…"));
        let services = self.services.clone();
        let task = self.runtime.spawn(async move {
            let id = {
                let db = Arc::clone(&services.db);
                let key = server.key();
                tokio::task::spawn_blocking(move || -> Result<_, String> {
                    let conn = db.reader().map_err(|e| e.to_string())?;
                    catalog::find_server(&conn, &key).map_err(|e| e.to_string())
                })
                .await
                .map_err(|e| e.to_string())??
            };
            match id {
                Some(id) => account::sign_out(services.vault.as_ref(), Arc::clone(&services.db), id)
                    .await
                    .map(|()| "Signed out. The server's API key stays valid until revoked on the account page.".to_owned())
                    .map_err(|e| e.to_string()),
                None => Ok("This server is not connected.".to_owned()),
            }
        });
        let me = Rc::clone(self);
        glib::spawn_future_local(async move {
            let outcome = task
                .await
                .unwrap_or_else(|_| Err("The sign-out task stopped.".into()));
            me.set_busy(Busy::Idle);
            me.show_status(&match outcome {
                Ok(text) => text,
                Err(message) => message,
            });
        });
    }

    /// Waits for a connection task and reports its result.
    fn finish_connection(
        self: &Rc<Self>,
        task: tokio::task::JoinHandle<Result<Connected, String>>,
    ) {
        *self.running.borrow_mut() = Some(task.abort_handle());
        let me = Rc::clone(self);
        glib::spawn_future_local(async move {
            let outcome = task.await;
            me.running.borrow_mut().take();
            me.expires_at_ms.set(None);
            match outcome {
                Ok(Ok(connected)) => {
                    me.set_busy(Busy::Idle);
                    let kind = connected
                        .info
                        .server_type
                        .unwrap_or_else(|| "server".to_owned());
                    me.show_status(&format!(
                        "Connected to {kind}. {} catalog items updated.",
                        connected.report.upserted
                    ));
                    me.pending.borrow_mut().take();
                    me.pairing_box.set_visible(false);
                    me.on_paste_changed();
                }
                Ok(Err(message)) => {
                    me.set_busy(Busy::Idle);
                    me.show_status(&message);
                    me.on_paste_changed();
                }
                // Cancelled by the user: the cancel handler has said so already.
                Err(error) if error.is_cancelled() => {}
                Err(_) => {
                    me.set_busy(Busy::Idle);
                    me.show_status("The connection task stopped unexpectedly.");
                }
            }
        });
    }
}
