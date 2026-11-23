//! "Match untagged files" (#14): looks up the fingerprints of local files
//! without a recording MBID on AcoustID and offers the recordings it knows.
//! The user picks one per file, or none; only the files with a pick get the
//! MBID, through the tag editor's preview, write and undo.
//!
//! The lookups belong to the dialog: closing it stops them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;
use studio_core::acoustid::{self, AcoustId, AcoustIdError, Candidate, Untagged};
use studio_core::db::Database;
use studio_core::tag_editor::{self, Edit, Field, Preview};

use crate::tag_dialog::{self, OnWritten};

/// One file with a fingerprint, and what was found for it.
struct Row {
    file: Untagged,
    choice: gtk::DropDown,
    names: gtk::StringList,
    /// The candidates behind `choice`, after the "Don't match" entry.
    candidates: RefCell<Vec<Candidate>>,
    /// AcoustID answered for this file, with or without candidates.
    looked_up: Cell<bool>,
}

struct Dialog {
    window: gtk::Window,
    summary: gtk::Label,
    list: gtk::ListBox,
    status: gtk::Label,
    changes: gtk::Label,
    look_up: gtk::Button,
    stop: gtk::Button,
    review: gtk::Button,
    write: gtk::Button,
    rows: RefCell<Vec<Rc<Row>>>,
    preview: RefCell<Option<Preview>>,
    running: RefCell<Option<tokio::task::AbortHandle>>,
    /// Set while code changes a choice, so it does not drop the preview.
    filling: Cell<bool>,
    runtime: Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    on_written: OnWritten,
}

impl Drop for Dialog {
    fn drop(&mut self) {
        if let Some(handle) = self.running.get_mut().take() {
            handle.abort();
        }
    }
}

/// What one lookup found.
enum Found {
    Candidates(Vec<Candidate>),
    Failed(String),
}

/// Shows the dialog over `parent`.
pub(crate) fn present(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    on_written: OnWritten,
) {
    open(parent, runtime, db, on_written);
}

/// Builds and shows the dialog; it keeps itself alive until it is closed.
fn open(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    on_written: OnWritten,
) -> Rc<Dialog> {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Match untagged files")
        .default_width(720)
        .default_height(640)
        .build();
    let intro = gtk::Label::builder()
        .label(
            "Local files without a MusicBrainz recording ID are looked up by their audio \
             fingerprint on AcoustID. Pick the right recording for each file; files left at \
             “Don't match” are not changed. The chosen IDs are written into the files, so \
             they match the server and DanceBrainz.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();
    let summary = gtk::Label::builder().wrap(true).xalign(0.0).build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Files to match")]);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .min_content_height(200)
        .build();
    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    let changes = gtk::Label::builder()
        .xalign(0.0)
        .yalign(0.0)
        .wrap(true)
        .selectable(true)
        .build();
    changes.update_property(&[gtk::accessible::Property::Label("Changes to write")]);
    let changes_scroll = gtk::ScrolledWindow::builder()
        .child(&changes)
        .min_content_height(100)
        .build();

    let look_up = gtk::Button::with_label("Look up");
    let stop = gtk::Button::with_label("Stop");
    stop.set_sensitive(false);
    let review = gtk::Button::with_label("Preview");
    review.set_tooltip_text(Some("Show what writing the chosen IDs would change"));
    review.set_sensitive(false);
    let write = gtk::Button::with_label("Write");
    write.set_sensitive(false);
    let close = gtk::Button::with_label("Close");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.append(&look_up);
    buttons.append(&stop);
    buttons.append(&gtk::Box::builder().hexpand(true).build());
    buttons.append(&review);
    buttons.append(&write);
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
    body.append(&summary);
    body.append(&scroll);
    body.append(&status);
    body.append(&changes_scroll);
    body.append(&buttons);
    window.set_child(Some(&body));

    let me = Rc::new(Dialog {
        window,
        summary,
        list,
        status,
        changes,
        look_up,
        stop,
        review,
        write,
        rows: RefCell::default(),
        preview: RefCell::default(),
        running: RefCell::default(),
        filling: Cell::new(false),
        runtime: Arc::clone(runtime),
        db,
        on_written,
    });
    let weak = Rc::downgrade(&me);
    me.look_up.connect_clicked(move |_| {
        if let Some(me) = weak.upgrade() {
            me.start_lookups();
        }
    });
    let weak = Rc::downgrade(&me);
    me.stop.connect_clicked(move |_| {
        if let Some(me) = weak.upgrade() {
            me.stop_lookups("Stopped.");
        }
    });
    let weak = Rc::downgrade(&me);
    me.review.connect_clicked(move |_| {
        if let Some(me) = weak.upgrade() {
            me.make_preview();
        }
    });
    let weak = Rc::downgrade(&me);
    me.write.connect_clicked(move |_| {
        if let Some(me) = weak.upgrade() {
            me.write_preview();
        }
    });
    let window = me.window.clone();
    close.connect_clicked(move |_| window.close());
    let keep = RefCell::new(Some(Rc::clone(&me)));
    me.window.connect_close_request(move |_| {
        if let Some(me) = keep.borrow_mut().take() {
            me.stop_lookups("");
        }
        glib::Propagation::Proceed
    });
    me.load();
    me.window.present();
    me
}

/// Runs blocking work on the runtime and hands its result to `done` on the
/// main thread.
fn run<T: Send + 'static>(
    runtime: &tokio::runtime::Runtime,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let handle = runtime.spawn_blocking(work);
    glib::spawn_future_local(async move {
        match handle.await {
            Ok(value) => done(value),
            Err(error) => tracing::warn!(%error, "a background task stopped"),
        }
    });
}

impl Dialog {
    /// Lists the untagged files.
    fn load(self: &Rc<Self>) {
        self.summary.set_text("Looking for files without an MBID…");
        self.look_up.set_sensitive(false);
        let db = Arc::clone(&self.db);
        let weak = Rc::downgrade(self);
        run(
            &self.runtime,
            move || {
                db.reader()
                    .and_then(|conn| acoustid::untagged(&conn))
                    .map_err(|e| e.to_string())
            },
            move |result| {
                if let Some(me) = weak.upgrade() {
                    me.fill(result);
                }
            },
        );
    }

    fn fill(self: &Rc<Self>, result: Result<Vec<Untagged>, String>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();
        let files = match result {
            Ok(files) => files,
            Err(error) => {
                self.summary.set_text(&error);
                return;
            }
        };
        let total = files.len();
        let usable: Vec<Untagged> = files
            .into_iter()
            .filter(|f| f.fingerprint.is_some() && f.lookup_duration().is_some())
            .collect();
        let mut summary = match total {
            0 => "Every local file has a recording MBID.".to_owned(),
            1 => "1 local file has no recording MBID.".to_owned(),
            n => format!("{n} local files have no recording MBID."),
        };
        let without = total - usable.len();
        if without > 0 {
            let has = if without == 1 { "has" } else { "have" };
            summary.push_str(&format!(
                " {without} of them {has} no fingerprint yet: analyse the local files with a \
                 folksentia that computes fingerprints first."
            ));
        }
        let key = acoustid::app_key();
        if key.is_none() {
            summary.push_str(
                " This build of Studio has no AcoustID application key, so it cannot \
                 look fingerprints up.",
            );
        }
        self.summary.set_text(&summary);
        for file in usable {
            let row = self.add_row(file);
            self.rows.borrow_mut().push(row);
        }
        self.look_up
            .set_sensitive(key.is_some() && !self.rows.borrow().is_empty());
        self.update_review();
    }

    fn add_row(self: &Rc<Self>, file: Untagged) -> Rc<Row> {
        let heading = match (&file.title, &file.artist) {
            (Some(title), Some(artist)) => format!("{title} — {artist}"),
            (Some(title), None) => title.clone(),
            _ => file.name.clone(),
        };
        let title = gtk::Label::builder()
            .label(&heading)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let name = gtk::Label::builder()
            .label(&file.name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label"])
            .build();
        let names = gtk::StringList::new(&["Not looked up yet"]);
        let choice = gtk::DropDown::builder()
            .model(&names)
            .width_request(300)
            .sensitive(false)
            .build();
        choice.update_property(&[gtk::accessible::Property::Label(&format!(
            "Recording for {heading}"
        ))]);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.append(&title);
        text.append(&name);
        let line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        line.append(&text);
        line.append(&choice);
        self.list.append(&line);
        let weak = Rc::downgrade(self);
        choice.connect_selected_notify(move |_| {
            if let Some(me) = weak.upgrade()
                && !me.filling.get()
            {
                me.drop_preview();
                me.update_review();
            }
        });
        Rc::new(Row {
            file,
            choice,
            names,
            candidates: RefCell::default(),
            looked_up: Cell::new(false),
        })
    }

    /// Looks up every file not looked up yet, one after another.
    fn start_lookups(self: &Rc<Self>) {
        let Some(key) = acoustid::app_key() else {
            return;
        };
        let http = match studio_core::http::client(&studio_core::http::HttpSettings::default()) {
            Ok(http) => http,
            Err(error) => {
                self.status.set_text(&error.to_string());
                return;
            }
        };
        let service = match AcoustId::new(http, key) {
            Ok(service) => service,
            Err(error) => {
                self.status.set_text(&error.to_string());
                return;
            }
        };
        let jobs: Vec<(usize, Untagged)> = self
            .rows
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, row)| !row.looked_up.get())
            .map(|(i, row)| (i, row.file.clone()))
            .collect();
        let count = jobs.len();
        let (sender, receiver) = async_channel::unbounded::<(usize, Found)>();
        let task = self.runtime.spawn(async move {
            for (index, file) in jobs {
                let (Some(print), Some(duration)) =
                    (file.fingerprint.as_ref(), file.lookup_duration())
                else {
                    continue;
                };
                let found = match service.lookup(print, duration).await {
                    Ok(candidates) => Found::Candidates(candidates),
                    Err(error) => {
                        let stop = matches!(error, AcoustIdError::Key);
                        let message = error.to_string();
                        if sender.send((index, Found::Failed(message))).await.is_err() || stop {
                            break;
                        }
                        continue;
                    }
                };
                if sender.send((index, found)).await.is_err() {
                    break;
                }
            }
        });
        *self.running.borrow_mut() = Some(task.abort_handle());
        self.look_up.set_sensitive(false);
        self.stop.set_sensitive(true);
        self.status.set_text(&format!("Looking up 0 of {count}…"));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut done = 0;
            while let Ok((index, found)) = receiver.recv().await {
                let Some(me) = weak.upgrade() else { return };
                done += 1;
                me.show_found(index, found);
                me.status
                    .set_text(&format!("Looking up {done} of {count}…"));
            }
            if let Some(me) = weak.upgrade() {
                me.stop_lookups(&format!(
                    "Looked up {done} of {count} files. Pick a recording where one fits."
                ));
            }
        });
    }

    fn stop_lookups(&self, text: &str) {
        if let Some(handle) = self.running.borrow_mut().take() {
            handle.abort();
        }
        self.stop.set_sensitive(false);
        let left = self.rows.borrow().iter().any(|row| !row.looked_up.get());
        self.look_up
            .set_sensitive(acoustid::app_key().is_some() && left);
        if !text.is_empty() {
            self.status.set_text(text);
        }
    }

    fn show_found(&self, index: usize, found: Found) {
        let rows = self.rows.borrow();
        let Some(row) = rows.get(index) else { return };
        self.filling.set(true);
        let labels: Vec<String> = match &found {
            Found::Candidates(list) if list.is_empty() => vec!["No recording found".to_owned()],
            Found::Candidates(list) => std::iter::once("Don't match".to_owned())
                .chain(list.iter().map(candidate_label))
                .collect(),
            Found::Failed(error) => vec![format!("Not looked up: {error}")],
        };
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        row.names.splice(0, row.names.n_items(), &labels);
        row.choice.set_selected(0);
        if let Found::Candidates(list) = found {
            row.looked_up.set(true);
            row.choice.set_sensitive(!list.is_empty());
            *row.candidates.borrow_mut() = list;
        }
        self.filling.set(false);
    }

    /// The files with a chosen recording, and its MBID.
    fn chosen(&self) -> Vec<(i64, Edit)> {
        self.rows
            .borrow()
            .iter()
            .filter_map(|row| {
                let index = usize::try_from(row.choice.selected())
                    .ok()?
                    .checked_sub(1)?;
                let candidates = row.candidates.borrow();
                let candidate = candidates.get(index)?;
                let mut edit = Edit::new();
                edit.set(Field::RecordingMbid, candidate.mbid.as_str())
                    .ok()?;
                Some((row.file.track_id, edit))
            })
            .collect()
    }

    fn update_review(&self) {
        let chosen = self.chosen().len();
        self.review.set_sensitive(chosen > 0);
        self.review.set_label(&match chosen {
            0 => "Preview".to_owned(),
            n => format!("Preview {n}"),
        });
    }

    fn make_preview(self: &Rc<Self>) {
        let edits = self.chosen();
        if edits.is_empty() {
            return;
        }
        self.review.set_sensitive(false);
        let db = Arc::clone(&self.db);
        let weak = Rc::downgrade(self);
        run(
            &self.runtime,
            move || tag_editor::preview(&db, &edits).map_err(|e| e.to_string()),
            move |result| {
                let Some(me) = weak.upgrade() else { return };
                me.update_review();
                match result {
                    Ok(preview) => {
                        me.changes.set_text(&tag_dialog::describe(&preview));
                        let files = preview.files.len();
                        me.write.set_label(&match files {
                            1 => "Write 1 file".to_owned(),
                            n => format!("Write {n} files"),
                        });
                        me.write.set_sensitive(files > 0);
                        *me.preview.borrow_mut() = Some(preview);
                    }
                    Err(error) => me.status.set_text(&error),
                }
            },
        );
    }

    fn drop_preview(&self) {
        self.preview.borrow_mut().take();
        self.changes.set_text("");
        self.write.set_label("Write");
        self.write.set_sensitive(false);
    }

    fn write_preview(self: &Rc<Self>) {
        let Some(preview) = self.preview.borrow_mut().take() else {
            return;
        };
        self.write.set_sensitive(false);
        self.review.set_sensitive(false);
        self.status.set_text("Writing…");
        let db = Arc::clone(&self.db);
        let weak = Rc::downgrade(self);
        run(
            &self.runtime,
            move || {
                tag_editor::write(&db, &preview, studio_core::sync::now_ms())
                    .map_err(|e| e.to_string())
            },
            move |result| {
                let Some(me) = weak.upgrade() else { return };
                match result {
                    Ok(report) => {
                        let line = tag_dialog::report_line(&report, "Wrote", "written");
                        me.changes.set_text(&tag_dialog::failures(&report));
                        me.status
                            .set_text(&format!("{line} “Edit tags” can undo this write."));
                        (me.on_written)(&line);
                        // The matched files have an MBID now.
                        me.load();
                    }
                    Err(error) => me.status.set_text(&error),
                }
            },
        );
    }
}

/// A candidate as the list shows it: score, title, artist.
fn candidate_label(candidate: &Candidate) -> String {
    let score = (candidate.score * 100.0).round();
    let title = candidate.title.as_deref().unwrap_or("Untitled");
    match &candidate.artist {
        Some(artist) => format!("{score:.0} %: {title} — {artist}"),
        None => format!("{score:.0} %: {title}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn pump_until(limit_ms: u64, done: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let end = std::time::Instant::now() + Duration::from_millis(limit_ms);
        while std::time::Instant::now() < end && !done() {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Lists two untagged tracks, one with a fingerprint, and saves a
    /// picture to `SHOTS`. Without an application key in the build, the
    /// lookup is not offered.
    #[test]
    #[ignore = "local: needs a display and SHOTS"]
    fn the_dialog_lists_the_files_it_can_match() {
        let shots = std::path::PathBuf::from(std::env::var("SHOTS").expect("SHOTS"));
        let base = shots.join("match");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");
        let db = Arc::new(Database::open(base.join("library.db")).expect("db"));
        db.write(|tx| {
            tx.execute_batch(
                "INSERT INTO local_folder (id, path, added_at) VALUES (1, '/music', 1);
                 INSERT INTO local_track (id, folder_id, rel_path, size_bytes, modified_ms,
                     fingerprint, title, artist, duration_ms, scanned_at) VALUES
                     (1, 1, 'Bal/an dro.flac', 10, 20, printf('%064d', 1), 'An dro',
                      'Blowzabella', 243000, 1),
                     (2, 1, 'Bal/scottish.flac', 10, 20, printf('%064d', 2), NULL, NULL,
                      180000, 1);
                 INSERT INTO local_analysis (track_id, tool_version, size_bytes, modified_ms,
                     analysed_at, fingerprint, fingerprint_sec)
                     VALUES (1, '0.5.0', 10, 20, 1, 'AQADtMmybfGO8NCN', 120.0);",
            )?;
            Ok(())
        })
        .expect("rows");
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime"),
        );
        gtk::init().expect("gtk");
        let parent = gtk::ApplicationWindow::builder().build();
        parent.present();
        let dialog = open(&parent, &runtime, Arc::clone(&db), Rc::new(|_| {}));
        pump_until(5000, || dialog.rows.borrow().len() == 1);
        let summary = dialog.summary.text();
        assert!(
            summary
                .starts_with("2 local files have no recording MBID. 1 of them has no fingerprint"),
            "{summary}"
        );
        assert_eq!(dialog.rows.borrow()[0].file.name, "Bal/an dro.flac");
        assert_eq!(dialog.look_up.is_sensitive(), acoustid::app_key().is_some());
        assert!(!dialog.review.is_sensitive(), "nothing chosen");
        pump_until(400, || false);
        let widget: &gtk::Widget = dialog.window.upcast_ref();
        let paintable = gtk::WidgetPaintable::new(Some(widget));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(widget.width()),
            f64::from(widget.height()),
        );
        if let (Some(node), Some(renderer)) = (
            snapshot.to_node(),
            widget.native().and_then(|n| n.renderer()),
        ) {
            let path = shots.join("match-1-list.png");
            renderer
                .render_texture(&node, None)
                .save_to_png(&path)
                .expect("png");
            eprintln!("SHOT {}", path.display());
        }
    }
}
