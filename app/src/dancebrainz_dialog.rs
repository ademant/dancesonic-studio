//! The DanceBrainz account and the queue of changes (#16).
//!
//! Pairing: the user creates a pairing code on the DanceBrainz account page
//! and pastes the link here; the owner then approves "DanceSonic Studio" on
//! that page. The list below shows every change not yet confirmed by
//! DanceBrainz with its state, and lets the user dismiss one.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;
use studio_core::dance_edits::State;

use crate::Services;
use crate::contribute::{self, PairState, QueueRow};

/// Called after the pairing or the queue changed, with a line for the
/// status bar.
pub(crate) type OnChanged = Rc<dyn Fn(&str)>;

struct Dialog {
    window: gtk::Window,
    runtime: Arc<tokio::runtime::Runtime>,
    services: Services,
    paired: gtk::Label,
    link: gtk::Entry,
    pair: gtk::Button,
    forget: gtk::Button,
    spinner: gtk::Spinner,
    queue: gtk::ListBox,
    send: gtk::Button,
    status: gtk::Label,
    /// The pairing that waits for the owner; aborted when the dialog closes.
    waiting: RefCell<Option<tokio::task::AbortHandle>>,
    on_changed: OnChanged,
}

/// Shows the dialog.
pub(crate) fn present(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    services: Services,
    on_changed: OnChanged,
) {
    open(parent, runtime, services, on_changed);
}

/// Builds and shows the dialog; it keeps itself alive until it is closed.
fn open(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    services: Services,
    on_changed: OnChanged,
) -> Rc<Dialog> {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("DanceBrainz")
        .default_width(600)
        .default_height(560)
        .build();
    let paired = gtk::Label::builder().xalign(0.0).wrap(true).build();
    let how = gtk::Label::builder()
        .label(
            "To pair, open your account page on DanceBrainz, create a pairing code \
             and paste its link here. Then approve “DanceSonic Studio” on that page \
             within five minutes.",
        )
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    let link = gtk::Entry::builder()
        .placeholder_text("dancebrainz://pair?…")
        .hexpand(true)
        .input_purpose(gtk::InputPurpose::Url)
        .build();
    link.update_property(&[gtk::accessible::Property::Label("Pairing link")]);
    let pair = gtk::Button::with_label("Pair");
    let spinner = gtk::Spinner::new();
    let forget = gtk::Button::with_label("Forget this pairing");
    let pair_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    pair_row.append(&link);
    pair_row.append(&pair);
    pair_row.append(&spinner);

    let queue = gtk::ListBox::new();
    queue.set_selection_mode(gtk::SelectionMode::None);
    queue.add_css_class("boxed-list");
    let scrolled = gtk::ScrolledWindow::builder()
        .child(&queue)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let send = gtk::Button::with_label("Send now");
    send.set_halign(gtk::Align::Start);
    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    column.append(&paired);
    column.append(&how);
    column.append(&pair_row);
    column.append(&forget);
    column.append(
        &gtk::Label::builder()
            .label("Changes not confirmed yet")
            .xalign(0.0)
            .css_classes(["heading"])
            .margin_top(12)
            .build(),
    );
    column.append(&scrolled);
    column.append(&send);
    column.append(&status);
    column.append(&close);
    window.set_child(Some(&column));

    let dialog = Rc::new(Dialog {
        window,
        runtime: Arc::clone(runtime),
        services,
        paired,
        link,
        pair,
        forget,
        spinner,
        queue,
        send,
        status,
        waiting: RefCell::default(),
        on_changed,
    });
    let weak = Rc::downgrade(&dialog);
    dialog.pair.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.start_pairing();
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.link.connect_activate(move |_| {
        if let Some(d) = weak.upgrade() {
            d.start_pairing();
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.forget.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.forget();
        }
    });
    let weak = Rc::downgrade(&dialog);
    dialog.send.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.send_now();
        }
    });
    let weak = Rc::downgrade(&dialog);
    close.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.window.close();
        }
    });
    let keep = RefCell::new(Some(Rc::clone(&dialog)));
    dialog.window.connect_close_request(move |_| {
        if let Some(d) = keep.borrow_mut().take()
            && let Some(waiting) = d.waiting.borrow_mut().take()
        {
            waiting.abort();
        }
        glib::Propagation::Proceed
    });
    dialog.window.present();
    dialog.refresh();
    dialog
}

impl Dialog {
    fn spawn<T: Send + 'static>(
        self: &Rc<Self>,
        work: impl std::future::Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) -> tokio::task::AbortHandle {
        let handle = self.runtime.spawn(work);
        let abort = handle.abort_handle();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            if let Ok(value) = handle.await
                && let Some(me) = weak.upgrade()
            {
                done(&me, value);
            }
        });
        abort
    }

    /// Reads the pairing and the queue again.
    fn refresh(self: &Rc<Self>) {
        let db = Arc::clone(&self.services.db);
        self.spawn(contribute::state(db), |me, state| match state {
            Ok((state, _)) => me.show_state(&state),
            Err(error) => me.status.set_text(&error),
        });
        let db = Arc::clone(&self.services.db);
        self.spawn(contribute::queue(db), |me, rows| match rows {
            Ok(rows) => me.show_queue(rows),
            Err(error) => me.status.set_text(&error),
        });
    }

    fn show_state(&self, state: &PairState) {
        let waiting = self.waiting.borrow().is_some();
        match state {
            PairState::Paired(server) => {
                self.paired.set_text(&format!(
                    "Studio is paired with {server} and sends changes as that account."
                ));
                self.forget.set_visible(true);
                self.send.set_sensitive(true);
            }
            PairState::Unpaired => {
                self.paired.set_text(
                    "Studio is not paired with DanceBrainz. Changes wait in the queue until it is.",
                );
                self.forget.set_visible(false);
                self.send.set_sensitive(false);
            }
        }
        self.link.set_sensitive(!waiting);
        self.pair.set_sensitive(!waiting);
        self.spinner.set_spinning(waiting);
    }

    fn show_queue(self: &Rc<Self>, rows: Vec<QueueRow>) {
        self.queue.remove_all();
        if rows.is_empty() {
            self.queue.append(
                &gtk::Label::builder()
                    .label("Nothing waits: DanceBrainz has every change.")
                    .xalign(0.0)
                    .margin_start(6)
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        for row in rows {
            let track = row
                .title
                .clone()
                .unwrap_or_else(|| row.edit.mbid.as_str().to_owned());
            let text = gtk::Label::builder()
                .label(format!("{}: {}", track, row.edit.edit.describe()))
                .xalign(0.0)
                .wrap(true)
                .hexpand(true)
                .build();
            let state = gtk::Label::builder()
                .label(row.edit.state.label())
                .xalign(0.0)
                .wrap(true)
                .css_classes([if row.edit.state.needs_attention() {
                    "heading"
                } else {
                    "dim-label"
                }])
                .build();
            let detail = gtk::Box::new(gtk::Orientation::Vertical, 2);
            detail.append(&text);
            detail.append(&state);
            if let Some(error) = &row.edit.error {
                let why = gtk::Label::builder()
                    .label(error)
                    .xalign(0.0)
                    .wrap(true)
                    .css_classes(["dim-label"])
                    .build();
                detail.append(&why);
            }
            let dismiss = gtk::Button::from_icon_name("edit-delete-symbolic");
            dismiss.add_css_class("flat");
            dismiss.set_valign(gtk::Align::Center);
            let tip = if row.edit.state == State::Queued {
                "Do not send this change"
            } else {
                "Remove this from the list"
            };
            dismiss.set_tooltip_text(Some(tip));
            dismiss.update_property(&[gtk::accessible::Property::Label(tip)]);
            let id = row.edit.id;
            let weak = Rc::downgrade(self);
            dismiss.connect_clicked(move |_| {
                if let Some(d) = weak.upgrade() {
                    d.dismiss(id);
                }
            });
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            line.set_margin_start(6);
            line.set_margin_end(6);
            line.set_margin_top(4);
            line.set_margin_bottom(4);
            line.append(&detail);
            line.append(&dismiss);
            self.queue.append(&line);
        }
    }

    fn start_pairing(self: &Rc<Self>) {
        let text = self.link.text().to_string();
        if text.trim().is_empty() || self.waiting.borrow().is_some() {
            return;
        }
        self.status
            .set_text("Waiting for the approval on the DanceBrainz account page…");
        self.link.set_sensitive(false);
        self.pair.set_sensitive(false);
        self.spinner.set_spinning(true);
        let abort = self.spawn(
            contribute::pair(self.services.clone(), text),
            |me, paired| {
                me.waiting.borrow_mut().take();
                match paired {
                    Ok(server) => {
                        me.link.set_text("");
                        let text = format!("Paired with DanceBrainz at {server}.");
                        me.status.set_text(&text);
                        (me.on_changed)(&text);
                        me.send_now();
                    }
                    Err(error) => me.status.set_text(&format!("Not paired: {error}")),
                }
                me.refresh();
            },
        );
        *self.waiting.borrow_mut() = Some(abort);
    }

    fn forget(self: &Rc<Self>) {
        self.spawn(contribute::forget(self.services.clone()), |me, done| {
            match done {
                Ok(()) => {
                    let text = "Studio is no longer paired with DanceBrainz. Remove the device \
                                on the account page too.";
                    me.status.set_text(text);
                    (me.on_changed)(text);
                }
                Err(error) => me.status.set_text(&error),
            }
            me.refresh();
        });
    }

    fn send_now(self: &Rc<Self>) {
        self.send.set_sensitive(false);
        self.status.set_text("Sending…");
        self.spawn(contribute::send(self.services.clone()), |me, sent| {
            let text = match sent {
                Ok(Some(report)) => contribute::report_text(&report),
                Ok(None) => "Studio is not paired with DanceBrainz.".to_owned(),
                Err(error) => error,
            };
            me.status.set_text(&text);
            (me.on_changed)(&text);
            me.refresh();
        });
    }

    fn dismiss(self: &Rc<Self>, id: i64) {
        let db = Arc::clone(&self.services.db);
        self.spawn(contribute::dismiss(db, id), |me, done| {
            if let Err(error) = done {
                me.status.set_text(&error);
            }
            (me.on_changed)("");
            me.refresh();
        });
    }
}

#[cfg(test)]
mod tests {
    use studio_core::dance_edits::{Edit, queue};
    use studio_core::dancebrainz::Mbid;

    use super::*;

    fn pump_until(limit_ms: u64, done: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let end = std::time::Instant::now() + std::time::Duration::from_millis(limit_ms);
        while std::time::Instant::now() < end && !done() {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn save(widget: &gtk::Widget, path: &std::path::Path) {
        pump_until(400, || false);
        let paintable = gtk::WidgetPaintable::new(Some(widget));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(widget.width()),
            f64::from(widget.height()),
        );
        let Some(node) = snapshot.to_node() else {
            eprintln!("SHOT skipped, nothing drawn: {}", path.display());
            return;
        };
        let renderer = widget
            .native()
            .and_then(|n| n.renderer())
            .expect("renderer");
        renderer
            .render_texture(&node, None)
            .save_to_png(path)
            .expect("png");
        eprintln!("SHOT {}", path.display());
    }

    fn services(db: &Arc<studio_core::db::Database>) -> Services {
        Services {
            vault: Arc::new(studio_core::secrets::SecretVault::<
                crate::secret_service::SecretService,
            >::session_only("ui check")),
            db: Arc::clone(db),
        }
    }

    fn runtime() -> Arc<tokio::runtime::Runtime> {
        Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime"),
        )
    }

    /// Shows the unpaired account and a queue with a waiting change and a
    /// conflict, dismisses one, and saves pictures to `SHOTS`.
    #[test]
    #[ignore = "local: needs a display and SHOTS"]
    fn the_queue_shows_each_change_with_its_state() {
        let shots = std::path::PathBuf::from(std::env::var("SHOTS").expect("SHOTS"));
        let base = shots.join("dancebrainz");
        let _ = std::fs::remove_dir_all(&base);
        let db = Arc::new(studio_core::db::Database::open(base.join("library.db")).expect("db"));
        let mbid = Mbid::parse("0f8b7c4e-2a7b-4a8e-9a42-3d7c5b1e9f10").unwrap();
        db.write(|tx| {
            queue(tx, &mbid, &Edit::SetShoes { to: Some(4) }, 1)?;
            queue(
                tx,
                &mbid,
                &Edit::RemoveDance {
                    name: "Bourrée".into(),
                    shown: vec!["Bourrée".into()],
                },
                1,
            )?;
            tx.execute(
                "UPDATE dance_edit SET state = 'conflict', error = 'The dances changed.'
                 WHERE field = 'dance'",
                [],
            )?;
            tx.execute(
                "UPDATE dance_edit SET error = 'DanceBrainz could not be reached'
                 WHERE field = 'shoes'",
                [],
            )?;
            Ok(())
        })
        .expect("queue");
        let runtime = runtime();

        gtk::init().expect("gtk");
        let parent = gtk::ApplicationWindow::builder().build();
        parent.present();
        let dialog = open(&parent, &runtime, services(&db), Rc::new(|_| {}));
        let rows = |d: &Dialog| {
            let mut n = 0;
            let mut child = d.queue.first_child();
            while let Some(row) = child {
                n += 1;
                child = row.next_sibling();
            }
            n
        };
        pump_until(5000, || rows(&dialog) == 2);
        pump_until(2000, || dialog.paired.text().contains("not paired"));
        assert!(!dialog.forget.is_visible());
        assert!(!dialog.send.is_sensitive());
        save(
            dialog.window.upcast_ref(),
            &shots.join("dancebrainz-1-queue.png"),
        );

        let first = studio_core::dance_edits::edits(&db.reader().unwrap()).unwrap()[0].id;
        dialog.dismiss(first);
        pump_until(5000, || rows(&dialog) == 1);
        assert_eq!(
            studio_core::dance_edits::edits(&db.reader().unwrap())
                .unwrap()
                .len(),
            1
        );
    }
}
