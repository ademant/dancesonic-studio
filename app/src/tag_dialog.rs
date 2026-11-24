//! "Edit tags" (#15): changes the tags of local files, for one file or for
//! all local files of the list shown. Nothing is written before the user has
//! seen the preview of every change; the last write can be undone.
//!
//! Fields that belong to one recording (title, track number, recording MBID,
//! BPM) are offered for one file only.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;
use studio_core::db::Database;
use studio_core::local_library::LocalTags;
use studio_core::tag_editor::{self, Edit, Field, Preview, TagError, WriteReport};

/// Called on the main thread after a write or an undo, with a line for the
/// status bar. The window reloads the library then.
pub(crate) type OnWritten = Rc<dyn Fn(&str)>;

/// The files the dialog was opened for.
#[derive(Clone, Debug)]
pub(crate) struct Scope {
    /// The selected local track.
    pub(crate) one: i64,
    /// All local tracks of the list shown, `one` among them.
    pub(crate) all: Vec<i64>,
    /// The list's title, for "All N files in …".
    pub(crate) list: String,
}

/// A field's value as the dialog starts from it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Start {
    /// The same in every file (`None`: empty in every file).
    Same(Option<String>),
    /// Different between the files.
    Various,
}

struct Dialog {
    window: gtk::Window,
    scope: Scope,
    only_one: gtk::CheckButton,
    every: gtk::CheckButton,
    entries: Vec<(Field, gtk::Entry)>,
    analysed: gtk::Button,
    analysed_bpm: Cell<Option<f64>>,
    status: gtk::Label,
    changes: gtk::Label,
    preview_button: gtk::Button,
    write_button: gtk::Button,
    undo_button: gtk::Button,
    start: RefCell<BTreeMap<Field, Start>>,
    preview: RefCell<Option<Preview>>,
    /// Set while code fills the entries, so they do not drop the preview.
    filling: Cell<bool>,
    /// Bumped on every load, so a late answer for an old scope is dropped.
    load: Cell<u64>,
    runtime: Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    on_written: OnWritten,
}

/// Shows the dialog over `parent`.
pub(crate) fn present(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    scope: Scope,
    on_written: OnWritten,
) {
    open(parent, runtime, db, scope, on_written);
}

/// Builds and shows the dialog; it keeps itself alive until it is closed.
fn open(
    parent: &gtk::ApplicationWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    scope: Scope,
    on_written: OnWritten,
) -> Rc<Dialog> {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Edit tags")
        .default_width(620)
        .default_height(640)
        .build();
    let intro = gtk::Label::builder()
        .label(
            "Changes the tags in the file itself. You see every change before it is \
             written, and you can undo the last write.",
        )
        .wrap(true)
        .xalign(0.0)
        .build();

    let only_one = gtk::CheckButton::with_label("This file");
    let every = gtk::CheckButton::with_label(&format!(
        "All {} local files in “{}”",
        scope.all.len(),
        scope.list
    ));
    every.set_group(Some(&only_one));
    only_one.set_active(true);
    let scope_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    scope_row.append(&only_one);
    scope_row.append(&every);
    scope_row.set_visible(scope.all.len() > 1);

    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(12)
        .build();
    let analysed = gtk::Button::with_label("Use analysed");
    analysed.set_tooltip_text(Some("Use the tempo folksentia found for this file"));
    analysed.set_visible(false);
    let mut entries = Vec::new();
    for (row, field) in (0..).zip(Field::ALL) {
        let caption = gtk::Label::builder()
            .label(field.label())
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        let entry = gtk::Entry::builder().hexpand(true).build();
        entry.update_property(&[gtk::accessible::Property::Label(field.label())]);
        grid.attach(&caption, 0, row, 1, 1);
        grid.attach(&entry, 1, row, 1, 1);
        if field == Field::Bpm {
            grid.attach(&analysed, 2, row, 1, 1);
        }
        entries.push((field, entry));
    }

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
        .vexpand(true)
        .min_content_height(120)
        .build();

    let undo_button = gtk::Button::with_label("Undo last write");
    let preview_button = gtk::Button::with_label("Preview");
    let write_button = gtk::Button::with_label("Write");
    write_button.set_sensitive(false);
    let close = gtk::Button::with_label("Close");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let spacer = gtk::Box::builder().hexpand(true).build();
    buttons.append(&undo_button);
    buttons.append(&spacer);
    buttons.append(&preview_button);
    buttons.append(&write_button);
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
    body.append(&scope_row);
    body.append(&grid);
    body.append(&status);
    body.append(&changes_scroll);
    body.append(&buttons);
    window.set_child(Some(&body));

    let me = Rc::new(Dialog {
        window,
        scope,
        only_one,
        every,
        entries,
        analysed,
        analysed_bpm: Cell::new(None),
        status,
        changes,
        preview_button,
        write_button,
        undo_button,
        start: RefCell::default(),
        preview: RefCell::default(),
        filling: Cell::new(false),
        load: Cell::new(0),
        runtime: Arc::clone(runtime),
        db,
        on_written,
    });

    let weak = Rc::downgrade(&me);
    // GTK turns the old choice off before it turns the new one on; this
    // handler fires after both in either direction.
    me.every.connect_toggled(move |_| {
        if let Some(me) = weak.upgrade() {
            me.load();
        }
    });
    for (_, entry) in &me.entries {
        let weak = Rc::downgrade(&me);
        entry.connect_changed(move |_| {
            if let Some(me) = weak.upgrade()
                && !me.filling.get()
            {
                me.drop_preview();
            }
        });
    }
    let weak = Rc::downgrade(&me);
    me.analysed.connect_clicked(move |_| {
        if let Some(me) = weak.upgrade()
            && let Some(bpm) = me.analysed_bpm.get()
        {
            me.entry(Field::Bpm).set_text(&format!("{}", bpm.round()));
        }
    });
    let weak = Rc::downgrade(&me);
    me.preview_button.connect_clicked(move |_| {
        if let Some(me) = weak.upgrade() {
            me.make_preview();
        }
    });
    let weak = Rc::downgrade(&me);
    me.write_button.connect_clicked(move |_| {
        if let Some(me) = weak.upgrade() {
            me.write();
        }
    });
    let weak = Rc::downgrade(&me);
    me.undo_button.connect_clicked(move |_| {
        if let Some(me) = weak.upgrade() {
            me.undo();
        }
    });
    let window = me.window.clone();
    close.connect_clicked(move |_| window.close());
    // The dialog owns itself until it is closed.
    let keep = RefCell::new(Some(Rc::clone(&me)));
    me.window.connect_close_request(move |_| {
        keep.borrow_mut().take();
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
    fn tracks(&self) -> Vec<i64> {
        if self.every.is_active() {
            self.scope.all.clone()
        } else {
            vec![self.scope.one]
        }
    }

    fn entry(&self, field: Field) -> &gtk::Entry {
        // Every field has its entry; see `present`.
        &self
            .entries
            .iter()
            .find(|(f, _)| *f == field)
            .unwrap_or(&self.entries[0])
            .1
    }

    /// Reads the files' tags and fills the fields from them.
    fn load(self: &Rc<Self>) {
        self.drop_preview();
        let load = self.load.get() + 1;
        self.load.set(load);
        let one = self.only_one.is_active();
        for (field, entry) in &self.entries {
            entry.set_sensitive(false);
            entry.set_tooltip_text(
                (!one && field.per_track())
                    .then_some("This belongs to one recording; change it for one file at a time"),
            );
        }
        self.status.set_text("Reading the files…");
        let db = Arc::clone(&self.db);
        let tracks = self.tracks();
        let weak = Rc::downgrade(self);
        run(
            &self.runtime,
            move || read_start(&db, &tracks),
            move |result| {
                let Some(me) = weak.upgrade() else { return };
                if me.load.get() != load {
                    return;
                }
                me.fill(result, one);
            },
        );
    }

    fn fill(self: &Rc<Self>, result: Result<Loaded, String>, one: bool) {
        let loaded = match result {
            Ok(loaded) => loaded,
            Err(error) => {
                self.status.set_text(&error);
                return;
            }
        };
        self.filling.set(true);
        for (field, entry) in &self.entries {
            match loaded.start.get(field) {
                Some(Start::Same(value)) => {
                    entry.set_text(value.as_deref().unwrap_or_default());
                    entry.set_placeholder_text(None);
                }
                Some(Start::Various) | None => {
                    entry.set_text("");
                    entry.set_placeholder_text(Some("(differs between the files)"));
                }
            }
            entry.set_sensitive(loaded.files > 0 && (one || !field.per_track()));
        }
        self.filling.set(false);
        *self.start.borrow_mut() = loaded.start;
        self.analysed_bpm.set(loaded.analysed_bpm.filter(|_| one));
        self.analysed
            .set_visible(one && loaded.analysed_bpm.is_some());
        self.undo_button.set_sensitive(loaded.undo > 0);
        self.undo_button.set_tooltip_text(Some(&match loaded.undo {
            0 => "Nothing to undo".to_owned(),
            1 => "Restore the file of the last write".to_owned(),
            n => format!("Restore the {n} files of the last write"),
        }));
        self.status.set_text(&match (loaded.files, loaded.missing) {
            (0, _) => "The file cannot be read, so its tags cannot be changed.".to_owned(),
            (_, 0) => "Change the fields, then preview the changes.".to_owned(),
            (_, n) => format!("{n} files cannot be read and are left out."),
        });
    }

    /// The edit the fields describe: every field whose text differs from
    /// what the files hold. A field that differs between the files and was
    /// left empty is not changed.
    fn edit(&self) -> Result<Edit, TagError> {
        let start = self.start.borrow();
        let mut edit = Edit::new();
        for (field, entry) in &self.entries {
            if !entry.is_sensitive() {
                continue;
            }
            let text = entry.text();
            let changed = match start.get(field) {
                Some(Start::Same(value)) => field.parse(&text)? != *value,
                Some(Start::Various) | None => !text.trim().is_empty(),
            };
            if changed {
                edit.set(*field, &text)?;
            }
        }
        Ok(edit)
    }

    fn make_preview(self: &Rc<Self>) {
        let edit = match self.edit() {
            Ok(edit) => edit,
            Err(error) => {
                self.status.set_text(&error.to_string());
                return;
            }
        };
        if edit.is_empty() {
            self.status.set_text("No field was changed.");
            return;
        }
        let edits: Vec<(i64, Edit)> = self
            .tracks()
            .into_iter()
            .map(|id| (id, edit.clone()))
            .collect();
        self.status.set_text("Reading the files…");
        self.preview_button.set_sensitive(false);
        let db = Arc::clone(&self.db);
        let weak = Rc::downgrade(self);
        run(
            &self.runtime,
            move || tag_editor::preview(&db, &edits).map_err(|e| e.to_string()),
            move |result| {
                let Some(me) = weak.upgrade() else { return };
                me.preview_button.set_sensitive(true);
                match result {
                    Ok(preview) => me.show_preview(preview),
                    Err(error) => me.status.set_text(&error),
                }
            },
        );
    }

    fn show_preview(&self, preview: Preview) {
        self.changes.set_text(&describe(&preview));
        let files = preview.files.len();
        self.status.set_text(&match files {
            0 => "Nothing to write.".to_owned(),
            1 => "Check the change, then write it.".to_owned(),
            n => format!("Check the changes to {n} files, then write them."),
        });
        self.write_button.set_label(&match files {
            1 => "Write 1 file".to_owned(),
            n => format!("Write {n} files"),
        });
        self.write_button.set_sensitive(files > 0);
        *self.preview.borrow_mut() = Some(preview);
    }

    fn drop_preview(&self) {
        self.preview.borrow_mut().take();
        self.changes.set_text("");
        self.write_button.set_label("Write");
        self.write_button.set_sensitive(false);
    }

    fn write(self: &Rc<Self>) {
        let Some(preview) = self.preview.borrow_mut().take() else {
            return;
        };
        self.set_busy(true, "Writing…");
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
                me.done(result, "Wrote", "written");
            },
        );
    }

    fn undo(self: &Rc<Self>) {
        self.set_busy(true, "Restoring…");
        let db = Arc::clone(&self.db);
        let weak = Rc::downgrade(self);
        run(
            &self.runtime,
            move || tag_editor::undo(&db, studio_core::sync::now_ms()).map_err(|e| e.to_string()),
            move |result| {
                let Some(me) = weak.upgrade() else { return };
                me.done(result, "Restored", "restored");
            },
        );
    }

    fn done(self: &Rc<Self>, result: Result<WriteReport, String>, did: &str, done: &str) {
        self.set_busy(false, "");
        match result {
            Ok(report) => {
                let line = report_line(&report, did, done);
                self.load();
                self.changes.set_text(&failures(&report));
                self.status.set_text(&line);
                (self.on_written)(&line);
            }
            Err(error) => self.status.set_text(&error),
        }
    }

    fn set_busy(&self, busy: bool, text: &str) {
        for button in [&self.preview_button, &self.undo_button] {
            button.set_sensitive(!busy);
        }
        self.write_button.set_sensitive(false);
        if busy {
            self.status.set_text(text);
        }
    }
}

/// What the dialog starts from.
struct Loaded {
    start: BTreeMap<Field, Start>,
    /// Files read.
    files: usize,
    /// Files that could not be read.
    missing: usize,
    /// The BPM folksentia found, for one file.
    analysed_bpm: Option<f64>,
    /// Files the last write changed.
    undo: usize,
}

/// Reads the tags of `tracks` and what each field holds in all of them.
fn read_start(db: &Database, tracks: &[i64]) -> Result<Loaded, String> {
    let mut tags: Vec<LocalTags> = Vec::new();
    let mut missing = 0;
    for id in tracks {
        match tag_editor::current_tags(db, *id) {
            Ok(Some(found)) => tags.push(found),
            Ok(None) | Err(_) => missing += 1,
        }
    }
    let start = Field::ALL
        .into_iter()
        .map(|field| {
            let mut values = tags.iter().map(|t| field.of(t));
            let first = values.next().flatten();
            let same = values.all(|v| v == first);
            (
                field,
                if same {
                    Start::Same(first)
                } else {
                    Start::Various
                },
            )
        })
        .collect();
    let conn = db.reader().map_err(|e| e.to_string())?;
    let analysed_bpm = match tracks {
        [one] => studio_core::analysis::of_track(&conn, *one)
            .ok()
            .flatten()
            .and_then(Result::ok)
            .and_then(|a| a.bpm),
        _ => None,
    };
    Ok(Loaded {
        start,
        files: tags.len(),
        missing,
        analysed_bpm,
        undo: tag_editor::undo_count(&conn).map_err(|e| e.to_string())?,
    })
}

/// The changes of a preview, one file after another, for the user.
pub(crate) fn describe(preview: &Preview) -> String {
    let shown = |value: &Option<String>| match value {
        Some(text) => format!("“{text}”"),
        None => "(empty)".to_owned(),
    };
    let mut text = String::new();
    for file in &preview.files {
        text.push_str(&file.name);
        text.push('\n');
        for change in &file.changes {
            text.push_str(&format!(
                "    {}: {} → {}\n",
                change.field.label(),
                shown(&change.before),
                shown(&change.after)
            ));
        }
    }
    if preview.files.iter().any(|f| f.on_server) {
        text.push_str(
            "\nThe server also has some of these recordings. If it scans this folder, \
             it shows the change after its next scan.\n",
        );
    }
    if preview.unchanged > 0 {
        text.push_str(&format!(
            "\n{} files already hold these values.\n",
            preview.unchanged
        ));
    }
    if !preview.skipped.is_empty() {
        text.push_str("\nLeft out:\n");
        for skipped in &preview.skipped {
            text.push_str(&format!("    {}: {}\n", skipped.name, skipped.reason));
        }
    }
    text
}

/// A line for the status bar about a write or an undo.
pub(crate) fn report_line(report: &WriteReport, did: &str, done: &str) -> String {
    match (report.written, report.failed.len()) {
        (1, 0) => format!("{did} the tags of 1 file."),
        (n, 0) => format!("{did} the tags of {n} files."),
        (n, f) => format!("{did} the tags of {n} files; {f} could not be {done}."),
    }
}

/// The files a write or an undo left alone, and why.
pub(crate) fn failures(report: &WriteReport) -> String {
    report
        .failed
        .iter()
        .map(|f| format!("{}: {}\n", f.name, f.reason))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lofty::config::WriteOptions;
    use lofty::tag::{ItemKey, Tag, TagExt, TagType};
    use studio_core::local_library::{add_folder, read_tags, scan_folder};

    use super::*;

    fn pump_until(limit_ms: u64, done: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let end = std::time::Instant::now() + Duration::from_millis(limit_ms);
        while std::time::Instant::now() < end && !done() {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
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

    /// A second of silence as WAV, with an ID3v2 tag.
    fn tagged_wav(path: &std::path::Path, title: &str, extra: u8) {
        let rate: u32 = 8_000;
        let mut data = vec![0u8; 16_000];
        data[0] = extra;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + 16_000u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&data);
        std::fs::write(path, bytes).expect("wav");
        let mut tag = Tag::new(TagType::Id3v2);
        tag.insert_text(ItemKey::TrackTitle, title.into());
        tag.insert_text(ItemKey::TrackArtist, "Blowzabella".into());
        tag.insert_text(ItemKey::AlbumTitle, "Pingha".into());
        tag.save_to_path(path, WriteOptions::default())
            .expect("tag");
    }

    /// Edits one file, then both files of a list, undoes the last write,
    /// and saves pictures to `SHOTS`.
    #[test]
    #[ignore = "local: needs a display and SHOTS"]
    fn the_dialog_previews_writes_and_undoes() {
        let shots = std::path::PathBuf::from(std::env::var("SHOTS").expect("SHOTS"));
        let base = shots.join("tags");
        let _ = std::fs::remove_dir_all(&base);
        let music = base.join("music");
        std::fs::create_dir_all(&music).expect("music");
        tagged_wav(&music.join("an dro.wav"), "An dro", 1);
        tagged_wav(&music.join("bourrée.wav"), "Bourrée", 2);
        let db = Arc::new(Database::open(base.join("library.db")).expect("db"));
        let now = studio_core::sync::now_ms();
        let folder = add_folder(&db, &music, now).expect("folder");
        scan_folder(&db, &folder, now).expect("scan");
        let id = |name: &str| -> i64 {
            db.reader()
                .expect("reader")
                .query_row(
                    "SELECT id FROM local_track WHERE rel_path = ?1",
                    [name],
                    |r| r.get(0),
                )
                .expect("track")
        };
        let (a, b) = (id("an dro.wav"), id("bourrée.wav"));
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
        let written = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen = Rc::clone(&written);
        let dialog = open(
            &parent,
            &runtime,
            Arc::clone(&db),
            Scope {
                one: a,
                all: vec![a, b],
                list: "Pingha".into(),
            },
            Rc::new(move |text| seen.borrow_mut().push(text.to_owned())),
        );
        let ready = "Change the fields, then preview the changes.";
        pump_until(5000, || dialog.status.text() == ready);
        assert_eq!(dialog.entry(Field::Title).text(), "An dro");
        assert!(!dialog.undo_button.is_sensitive(), "nothing to undo yet");
        save(
            dialog.window.upcast_ref(),
            &shots.join("tags-1-one-file.png"),
        );

        dialog.entry(Field::Title).set_text("An dro retourné");
        dialog.entry(Field::Year).set_text("1983");
        dialog.preview_button.emit_clicked();
        pump_until(5000, || dialog.write_button.is_sensitive());
        let changes = dialog.changes.text();
        assert!(
            changes.contains("Title: “An dro” → “An dro retourné”"),
            "{changes}"
        );
        assert!(changes.contains("Year: (empty) → “1983”"), "{changes}");
        assert_eq!(dialog.write_button.label().as_deref(), Some("Write 1 file"));
        save(
            dialog.window.upcast_ref(),
            &shots.join("tags-2-preview.png"),
        );

        dialog.write_button.emit_clicked();
        pump_until(5000, || !written.borrow().is_empty());
        assert_eq!(written.borrow()[0], "Wrote the tags of 1 file.");
        let tags = read_tags(&music.join("an dro.wav")).expect("tags");
        assert_eq!(tags.title.as_deref(), Some("An dro retourné"));
        assert_eq!(tags.year, Some(1983));

        // All files of the list: per-recording fields are not offered.
        dialog.every.set_active(true);
        pump_until(5000, || {
            dialog.status.text() == ready && dialog.undo_button.is_sensitive()
        });
        assert!(!dialog.entry(Field::Title).is_sensitive());
        assert_eq!(dialog.entry(Field::Album).text(), "Pingha");
        assert_eq!(dialog.entry(Field::Year).text(), "", "differs");
        dialog.entry(Field::Album).set_text("Bal");
        dialog.preview_button.emit_clicked();
        pump_until(5000, || dialog.write_button.is_sensitive());
        assert_eq!(
            dialog.write_button.label().as_deref(),
            Some("Write 2 files")
        );
        save(dialog.window.upcast_ref(), &shots.join("tags-3-list.png"));
        dialog.write_button.emit_clicked();
        pump_until(5000, || written.borrow().len() == 2);
        assert_eq!(written.borrow()[1], "Wrote the tags of 2 files.");

        dialog.undo_button.emit_clicked();
        pump_until(5000, || written.borrow().len() == 3);
        assert_eq!(written.borrow()[2], "Restored the tags of 2 files.");
        for name in ["an dro.wav", "bourrée.wav"] {
            let tags = read_tags(&music.join(name)).expect("tags");
            assert_eq!(tags.album.as_deref(), Some("Pingha"), "{name}");
        }
        assert_eq!(
            read_tags(&music.join("an dro.wav"))
                .expect("tags")
                .title
                .as_deref(),
            Some("An dro retourné"),
            "only the last write is undone"
        );
        pump_until(3000, || !dialog.undo_button.is_sensitive());
        save(dialog.window.upcast_ref(), &shots.join("tags-4-undone.png"));
    }
}
