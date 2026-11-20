//! "Remote control" (#20, #24): the switches, "Pair a phone" with its QR
//! code, and the paired phones with a way to revoke each one.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use secrecy::{ExposeSecret, SecretString};
use studio_core::remote::devices::Device;
use studio_core::remote::messages::Transport;

use crate::remote::Remote;

/// Pixels per QR module, and the quiet zone in modules.
const QR_SCALE: usize = 6;
const QR_QUIET: usize = 4;

struct Dialog {
    window: gtk::Window,
    remote: Rc<Remote>,
    network: gtk::CheckButton,
    bluetooth: gtk::CheckButton,
    status: gtk::Label,
    phones: gtk::ListBox,
    pair: gtk::Button,
    code_box: gtk::Box,
    code: gtk::Picture,
    code_note: gtk::Label,
    /// Set while code changes the switches, so they do not save.
    updating: Cell<bool>,
    /// Bumped for every new code, so an old expiry timer does nothing.
    shown_code: Cell<u64>,
    listener: Cell<Option<u64>>,
    /// The phones listed last; `None` before the first listing.
    known: RefCell<Option<Vec<String>>>,
}

/// Shows the dialog over `parent`.
pub(crate) fn present(parent: &gtk::ApplicationWindow, remote: &Rc<Remote>) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Remote control")
        .default_width(520)
        .default_height(600)
        .build();
    let intro = gtk::Label::builder()
        .label(
            "The DanceSonic app on your phone can control playback here. \
             Phones pair only by scanning Studio's code, and only after you allow them.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    let network = gtk::CheckButton::with_label("Allow remote control on the local network");
    let bluetooth = gtk::CheckButton::with_label("Allow remote control over Bluetooth");
    // Bluetooth comes with #24.
    bluetooth.set_sensitive(false);
    bluetooth.set_tooltip_text(Some("Not available yet"));
    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();

    let pair = gtk::Button::with_label("Pair a phone…");
    pair.set_halign(gtk::Align::Start);
    let code = gtk::Picture::builder()
        .can_shrink(false)
        .halign(gtk::Align::Center)
        .build();
    code.update_property(&[gtk::accessible::Property::Label("Pairing code")]);
    let code_note = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let cancel_code = gtk::Button::with_label("Stop pairing");
    cancel_code.set_halign(gtk::Align::Center);
    let code_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    code_box.append(&code);
    code_box.append(&code_note);
    code_box.append(&cancel_code);
    code_box.set_visible(false);

    let heading = gtk::Label::builder()
        .label("Paired phones")
        .css_classes(["heading"])
        .xalign(0.0)
        .margin_top(6)
        .build();
    let phones = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    phones.update_property(&[gtk::accessible::Property::Label("Paired phones")]);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&phones)
        .vexpand(true)
        .min_content_height(120)
        .build();
    let close = gtk::Button::with_label("Close");
    let end = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    end.set_halign(gtk::Align::End);
    end.append(&close);

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&intro);
    body.append(&network);
    body.append(&bluetooth);
    body.append(&status);
    body.append(&pair);
    body.append(&code_box);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    body.append(&heading);
    body.append(&scroll);
    body.append(&end);
    window.set_child(Some(&body));

    let dialog = Rc::new(Dialog {
        window,
        remote: Rc::clone(remote),
        network,
        bluetooth,
        status,
        phones,
        pair,
        code_box,
        code,
        code_note,
        updating: Cell::new(false),
        shown_code: Cell::new(0),
        listener: Cell::new(None),
        known: RefCell::new(None),
    });

    let weak = Rc::downgrade(&dialog);
    dialog.network.connect_toggled(move |_| {
        if let Some(d) = weak.upgrade() {
            d.save();
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.bluetooth.connect_toggled(move |_| {
        if let Some(d) = weak.upgrade() {
            d.save();
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.pair.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.show_code();
        }
    });
    let weak = Rc::downgrade(&dialog);
    cancel_code.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.hide_code();
        }
    });
    let win = dialog.window.clone();
    close.connect_clicked(move |_| win.close());
    let weak = Rc::downgrade(&dialog);
    dialog.window.connect_close_request(move |_| {
        if let Some(d) = weak.upgrade() {
            d.hide_code();
            if let Some(id) = d.listener.take() {
                d.remote.unlisten(id);
            }
        }
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&dialog);
    let id = remote.listen(Rc::new(move || {
        if let Some(d) = weak.upgrade() {
            d.refresh();
        }
    }));
    dialog.listener.set(Some(id));
    // Every handler holds the state weakly; the window holds it until it is
    // destroyed.
    let keep = RefCell::new(Some(Rc::clone(&dialog)));
    dialog.window.connect_destroy(move |_| {
        keep.borrow_mut().take();
    });
    dialog.refresh();
    dialog.window.present();
}

impl Dialog {
    fn save(&self) {
        if self.updating.get() {
            return;
        }
        let mut settings = self.remote.settings();
        settings.network = self.network.is_active();
        settings.bluetooth = self.bluetooth.is_active();
        if !settings.network && !settings.bluetooth {
            self.hide_code();
        }
        self.remote.set(settings);
    }

    fn refresh(self: &Rc<Self>) {
        let settings = self.remote.settings();
        self.updating.set(true);
        self.network.set_active(settings.network);
        self.bluetooth.set_active(settings.bluetooth);
        self.updating.set(false);
        self.status.set_text(&self.remote.status());
        self.pair.set_sensitive(self.remote.can_pair());
        let me = Rc::clone(self);
        glib::spawn_future_local(async move {
            let devices = me.remote.devices().await;
            // A phone that just paired has used the code up.
            let ids: Vec<String> = devices.iter().map(|d| d.id.as_str().to_owned()).collect();
            let paired = me
                .known
                .borrow()
                .as_ref()
                .is_some_and(|known| ids.iter().any(|id| !known.contains(id)));
            if paired && me.code_box.is_visible() {
                me.hide_code();
            }
            *me.known.borrow_mut() = Some(ids);
            me.fill(&devices);
        });
    }

    fn fill(self: &Rc<Self>, devices: &[Device]) {
        while let Some(row) = self.phones.first_child() {
            self.phones.remove(&row);
        }
        if devices.is_empty() {
            let empty = gtk::Label::builder()
                .label("No phone is paired.")
                .css_classes(["dim-label"])
                .margin_top(6)
                .margin_bottom(6)
                .build();
            self.phones.append(&empty);
            return;
        }
        let sessions = self.remote.sessions();
        for device in devices {
            let name = gtk::Label::builder()
                .label(&device.name)
                .xalign(0.0)
                .build();
            let open: Vec<Transport> = sessions
                .iter()
                .filter(|s| s.device == device.id)
                .map(|s| s.transport)
                .collect();
            let detail = if device.is_revoked() {
                "Revoked".to_owned()
            } else if !open.is_empty() {
                let over: Vec<&str> = open.iter().map(|t| transport_name(*t)).collect();
                format!("Connected over {}", over.join(" and "))
            } else {
                match (device.last_seen_at, device.last_transport.as_deref()) {
                    (Some(ms), Some(t)) => format!(
                        "Last seen {} over {}",
                        when(ms),
                        if t == "ble" {
                            "Bluetooth"
                        } else {
                            "the network"
                        }
                    ),
                    _ => "Not connected yet".to_owned(),
                }
            };
            let detail = gtk::Label::builder()
                .label(&detail)
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build();
            let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
            text.set_hexpand(true);
            text.append(&name);
            text.append(&detail);
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(6);
            row.set_margin_end(6);
            row.append(&text);
            if !device.is_revoked() {
                let revoke = gtk::Button::with_label("Revoke");
                revoke.set_valign(gtk::Align::Center);
                revoke.set_tooltip_text(Some("Refuse this phone from now on"));
                let weak = Rc::downgrade(self);
                let id = device.id.clone();
                revoke.connect_clicked(move |_| {
                    if let Some(d) = weak.upgrade() {
                        let id = id.clone();
                        glib::spawn_future_local(async move {
                            d.remote.revoke(id, false).await;
                        });
                    }
                });
                row.append(&revoke);
            }
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.set_valign(gtk::Align::Center);
            remove.set_tooltip_text(Some("Forget this phone"));
            remove.update_property(&[gtk::accessible::Property::Label(&format!(
                "Forget {}",
                device.name
            ))]);
            let weak = Rc::downgrade(self);
            let id = device.id.clone();
            remove.connect_clicked(move |_| {
                if let Some(d) = weak.upgrade() {
                    let id = id.clone();
                    glib::spawn_future_local(async move {
                        d.remote.revoke(id, true).await;
                    });
                }
            });
            row.append(&remove);
            self.phones.append(&row);
        }
    }

    fn show_code(self: &Rc<Self>) {
        let Some((text, expires)) = self.remote.new_offer() else {
            self.status
                .set_text("Turn on remote control first, then pair a phone.");
            return;
        };
        let Some(texture) = qr_texture(&text) else {
            self.remote.cancel_offer();
            self.status.set_text("The pairing code could not be drawn.");
            return;
        };
        drop(text);
        self.code.set_paintable(Some(&texture));
        let until = glib::DateTime::from_unix_local(expires)
            .and_then(|t| t.format("%H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        self.code_note.set_text(&format!(
            "Scan this code with the DanceSonic app on your phone. \
             It works once, until {until}."
        ));
        self.code_box.set_visible(true);
        self.pair.set_sensitive(false);
        let shown = self.shown_code.get() + 1;
        self.shown_code.set(shown);
        let now = glib::DateTime::now_utc()
            .map(|t| t.to_unix())
            .unwrap_or(expires);
        let left = u32::try_from((expires - now).max(1)).unwrap_or(1);
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(left, move || {
            if let Some(d) = weak.upgrade()
                && d.shown_code.get() == shown
            {
                d.hide_code();
            }
        });
    }

    fn hide_code(&self) {
        self.shown_code.set(self.shown_code.get() + 1);
        self.remote.cancel_offer();
        self.code.set_paintable(gdk::Paintable::NONE);
        self.code_box.set_visible(false);
        self.pair.set_sensitive(self.remote.can_pair());
    }
}

fn transport_name(transport: Transport) -> &'static str {
    match transport {
        Transport::Ble => "Bluetooth",
        Transport::Network => "the network",
    }
}

fn when(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|t| t.format("%x %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Draws a QR code, dark on light with a quiet zone, as a texture. The
/// pairing text is a secret: it goes into the picture and nowhere else.
fn qr_texture(text: &SecretString) -> Option<gdk::MemoryTexture> {
    let code = qrcode::QrCode::with_error_correction_level(
        text.expose_secret().as_bytes(),
        qrcode::EcLevel::M,
    )
    .ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let side = (modules + 2 * QR_QUIET) * QR_SCALE;
    let mut pixels = vec![0xffu8; side * side * 3];
    for (i, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let (mx, my) = (i % modules + QR_QUIET, i / modules + QR_QUIET);
        for y in my * QR_SCALE..(my + 1) * QR_SCALE {
            let row = y * side * 3;
            pixels[row + mx * QR_SCALE * 3..row + (mx + 1) * QR_SCALE * 3].fill(0);
        }
    }
    let side_i32 = i32::try_from(side).ok()?;
    Some(gdk::MemoryTexture::new(
        side_i32,
        side_i32,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(pixels),
        side * 3,
    ))
}
