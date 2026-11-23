//! "Music folders" (#10): the local folders Studio reads, with a way to add
//! one through the file chooser portal and to remove one. Removing only
//! forgets the folder; its files stay on disk.
//!
//! Also the settings of offline copies (#17): their folder, the space they
//! may take, and a check of their content.

use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use gtk::{gio, glib};
use studio_core::db::Database;
use studio_core::local_library::LocalFolder;
use studio_core::offline::Settings;

use crate::session;

/// What the dialog asks the window to do after a change.
pub(crate) type OnChange = Rc<dyn Fn()>;

struct Dialog {
    window: gtk::Window,
    list: gtk::ListBox,
    status: gtk::Label,
    add: gtk::Button,
    offline_folder: gtk::Label,
    offline_used: gtk::Label,
    limit: gtk::SpinButton,
    /// Set while code moves `limit`, so it does not save.
    updating: std::cell::Cell<bool>,
    settings: std::cell::RefCell<Option<Settings>>,
    runtime: Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    on_change: OnChange,
}

/// Shows the dialog over `parent`.
pub(crate) fn present(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    on_change: OnChange,
) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Music folders and offline copies")
        .default_width(560)
        .default_height(520)
        .build();
    let intro = gtk::Label::builder()
        .label(
            "Studio lists the audio files in these folders and watches them for changes. \
             It never deletes them, and changes them only when you edit their tags.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Music folders")]);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .build();
    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    let add = gtk::Button::with_label("Add folder…");
    let close = gtk::Button::with_label("Close");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&add);

    // Offline copies of server tracks (#17).
    let offline_heading = gtk::Label::builder()
        .label("Offline copies of server tracks")
        .css_classes(["heading"])
        .xalign(0.0)
        .margin_top(6)
        .build();
    let offline_folder = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    let change = gtk::Button::with_label("Change…");
    let reset = gtk::Button::with_label("Default");
    reset.set_tooltip_text(Some("Use the folder under Studio's data directory"));
    let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    folder_row.append(&offline_folder);
    folder_row.append(&change);
    folder_row.append(&reset);
    let limit = gtk::SpinButton::with_range(1.0, 2000.0, 1.0);
    limit.update_property(&[gtk::accessible::Property::Label("Size limit in gigabytes")]);
    let offline_used = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["dim-label"])
        .build();
    let check = gtk::Button::with_label("Check copies");
    check.set_tooltip_text(Some(
        "Read every copy and download again the ones that changed on disk",
    ));
    let limit_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    limit_row.append(&gtk::Label::new(Some("Use up to")));
    limit_row.append(&limit);
    limit_row.append(&gtk::Label::new(Some("GB")));
    limit_row.append(&offline_used);
    limit_row.append(&check);
    let offline_note = gtk::Label::builder()
        .label("Changing the folder deletes the copies in the old one; they are downloaded again.")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();

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
    body.append(&scroll);
    body.append(&buttons);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    body.append(&offline_heading);
    body.append(&folder_row);
    body.append(&limit_row);
    body.append(&offline_note);
    body.append(&status);
    body.append(&end);
    window.set_child(Some(&body));

    let dialog = Rc::new(Dialog {
        window,
        list,
        status,
        add,
        offline_folder,
        offline_used,
        limit,
        updating: std::cell::Cell::new(false),
        settings: std::cell::RefCell::default(),
        runtime: Arc::clone(runtime),
        db,
        on_change,
    });
    {
        let window = dialog.window.clone();
        close.connect_clicked(move |_| window.close());
    }
    {
        let weak = Rc::downgrade(&dialog);
        dialog.add.connect_clicked(move |_| {
            if let Some(dialog) = weak.upgrade() {
                dialog.choose();
            }
        });
    }
    {
        let weak = Rc::downgrade(&dialog);
        change.connect_clicked(move |_| {
            if let Some(dialog) = weak.upgrade() {
                dialog.choose_offline_folder();
            }
        });
    }
    {
        let weak = Rc::downgrade(&dialog);
        reset.connect_clicked(move |_| {
            if let Some(dialog) = weak.upgrade() {
                dialog.save_offline(|s| s.folder = None);
            }
        });
    }
    {
        let weak = Rc::downgrade(&dialog);
        dialog.limit.connect_value_changed(move |spin| {
            let Some(dialog) = weak.upgrade() else { return };
            if dialog.updating.get() {
                return;
            }
            let bytes = (spin.value().max(1.0) * 1e9) as u64;
            dialog.save_offline(move |s| s.limit_bytes = bytes);
        });
    }
    {
        let weak = Rc::downgrade(&dialog);
        check.connect_clicked(move |button| {
            let Some(dialog) = weak.upgrade() else { return };
            button.set_sensitive(false);
            dialog.status.set_text("Checking the offline copies…");
            let button = button.clone();
            dialog.spawn(
                session::verify_offline(Arc::clone(&dialog.db)),
                move |me, result| {
                    button.set_sensitive(true);
                    me.status.set_text(&match result {
                        Ok((checked, 0)) => format!("All {checked} offline copies are intact."),
                        Ok((checked, broken)) => format!(
                            "{broken} of {checked} offline copies had changed; they are downloaded again."
                        ),
                        Err(error) => format!("The copies could not be checked: {error}"),
                    });
                    me.refresh_offline();
                    (me.on_change)();
                },
            );
        });
    }
    // The window holds the dialog state until it closes.
    {
        let keep = std::cell::RefCell::new(Some(Rc::clone(&dialog)));
        dialog.window.connect_destroy(move |_| {
            keep.borrow_mut().take();
        });
    }
    dialog.refresh();
    dialog.refresh_offline();
    dialog.window.present();
}

impl Dialog {
    /// Runs `work` on the runtime and `done` on the main thread afterwards.
    fn spawn<T: Send + 'static>(
        self: &Rc<Self>,
        work: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) {
        let handle = self.runtime.spawn(work);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            if let (Ok(value), Some(me)) = (handle.await, weak.upgrade()) {
                done(&me, value);
            }
        });
    }

    fn refresh(self: &Rc<Self>) {
        self.spawn(
            session::folders(Arc::clone(&self.db)),
            |me, folders| match folders {
                Ok(folders) => me.show(&folders),
                Err(error) => me
                    .status
                    .set_text(&format!("The folders could not be read: {error}")),
            },
        );
    }

    fn show(self: &Rc<Self>, folders: &[LocalFolder]) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        if folders.is_empty() {
            let empty = gtk::Label::builder()
                .label("No folders yet.")
                .css_classes(["dim-label"])
                .margin_top(12)
                .margin_bottom(12)
                .build();
            self.list.append(&empty);
        }
        for folder in folders {
            let path = folder.path.display().to_string();
            let label = gtk::Label::builder()
                .label(&path)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .tooltip_text(&path)
                .build();
            let remove = gtk::Button::builder()
                .icon_name("list-remove-symbolic")
                .tooltip_text("Remove from Studio (the files stay)")
                .build();
            remove.update_property(&[gtk::accessible::Property::Label(&format!("Remove {path}"))]);
            let id = folder.id;
            let weak = Rc::downgrade(self);
            remove.connect_clicked(move |_| {
                if let Some(me) = weak.upgrade() {
                    me.remove(id);
                }
            });
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(6);
            row.set_margin_end(6);
            row.append(&label);
            row.append(&remove);
            self.list.append(&row);
        }
    }

    /// Opens the folder chooser; on GNOME, Xfce and Budgie it goes through
    /// the file chooser portal where one runs.
    fn choose(self: &Rc<Self>) {
        let chooser = gtk::FileDialog::builder()
            .title("Add a music folder")
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        chooser.select_folder(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                let (Some(me), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                let Some(path) = file.path() else {
                    me.status
                        .set_text("Only folders on this computer can be added.");
                    return;
                };
                me.add(path);
            },
        );
    }

    fn refresh_offline(self: &Rc<Self>) {
        self.spawn(
            session::offline_status(Arc::clone(&self.db)),
            |me, status| match status {
                Ok((settings, dir, used)) => {
                    me.offline_folder.set_text(&dir.display().to_string());
                    me.offline_folder
                        .set_tooltip_text(Some(&dir.display().to_string()));
                    me.updating.set(true);
                    me.limit
                        .set_value((settings.limit_bytes as f64 / 1e9).round());
                    me.updating.set(false);
                    me.offline_used
                        .set_text(&format!("{} MB used", used / 1_000_000));
                    *me.settings.borrow_mut() = Some(settings);
                }
                Err(error) => me
                    .status
                    .set_text(&format!("The offline settings could not be read: {error}")),
            },
        );
    }

    /// Saves the offline settings with `change` applied.
    fn save_offline(self: &Rc<Self>, change: impl FnOnce(&mut Settings)) {
        let Some(mut settings) = self.settings.borrow().clone() else {
            return;
        };
        change(&mut settings);
        self.spawn(
            session::set_offline_settings(Arc::clone(&self.db), settings),
            |me, result| {
                if let Err(error) = result {
                    me.status
                        .set_text(&format!("The offline settings could not be saved: {error}"));
                }
                me.refresh_offline();
                (me.on_change)();
            },
        );
    }

    fn choose_offline_folder(self: &Rc<Self>) {
        let chooser = gtk::FileDialog::builder()
            .title("Folder for offline copies")
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        chooser.select_folder(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                let (Some(me), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                match file.path() {
                    Some(path) => me.save_offline(move |s| s.folder = Some(path)),
                    None => me
                        .status
                        .set_text("Only folders on this computer can be used."),
                }
            },
        );
    }

    fn add(self: &Rc<Self>, path: std::path::PathBuf) {
        self.add.set_sensitive(false);
        self.status.set_text("Reading the folder…");
        self.spawn(
            session::add_folder(Arc::clone(&self.db), path),
            |me, result| {
                me.add.set_sensitive(true);
                me.status.set_text(&match result {
                    Ok(report) => scan_text(&report),
                    Err(error) => format!("The folder could not be added: {error}"),
                });
                me.refresh();
                (me.on_change)();
            },
        );
    }

    fn remove(self: &Rc<Self>, folder: i64) {
        self.spawn(
            session::remove_folder(Arc::clone(&self.db), folder),
            |me, result| {
                if let Err(error) = result {
                    me.status
                        .set_text(&format!("The folder could not be removed: {error}"));
                }
                me.refresh();
                (me.on_change)();
            },
        );
    }
}

/// A short summary of a scan for the status line.
pub(crate) fn scan_text(report: &studio_core::local_library::ScanReport) -> String {
    let mut text = format!(
        "{} new, {} changed, {} moved, {} gone.",
        report.added, report.updated, report.moved, report.removed
    );
    if report.unreadable > 0 {
        text.push_str(&format!(
            " {} files without readable tags are listed by file name.",
            report.unreadable
        ));
    }
    text
}
