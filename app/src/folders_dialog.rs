//! "Music folders" (#10): the local folders Studio reads, with a way to add
//! one through the file chooser portal and to remove one. Removing only
//! forgets the folder; its files stay on disk.

use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use gtk::{gio, glib};
use studio_core::db::Database;
use studio_core::local_library::LocalFolder;

use crate::session;

/// What the dialog asks the window to do after a change.
pub(crate) type OnChange = Rc<dyn Fn()>;

struct Dialog {
    window: gtk::Window,
    list: gtk::ListBox,
    status: gtk::Label,
    add: gtk::Button,
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
        .title("Music folders")
        .default_width(520)
        .default_height(360)
        .build();
    let intro = gtk::Label::builder()
        .label(
            "Studio lists the audio files in these folders and watches them for changes. \
             It never changes or deletes them.",
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
    buttons.append(&close);
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
    body.append(&status);
    body.append(&buttons);
    window.set_child(Some(&body));

    let dialog = Rc::new(Dialog {
        window,
        list,
        status,
        add,
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
    // The window holds the dialog state until it closes.
    {
        let keep = std::cell::RefCell::new(Some(Rc::clone(&dialog)));
        dialog.window.connect_destroy(move |_| {
            keep.borrow_mut().take();
        });
    }
    dialog.refresh();
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
