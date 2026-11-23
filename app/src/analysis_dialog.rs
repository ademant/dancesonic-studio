//! "Analyse local files" (#11): runs folksentia over the local music folders
//! and shows how far it is. Also runs a job list from folksonic against a
//! local copy of its music folder, for its offline analysis (#12).
//!
//! The run belongs to the window, not to the dialog: closing the dialog keeps
//! it going, opening it again shows its progress, and closing the window stops
//! it. Stopping kills the running folksentia processes; results stored so far
//! stay, and the next start continues with what is left.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use gtk::prelude::*;
use gtk::{gio, glib};
use studio_core::analysis::jobs::{self, JobList, JobProgress, JobRun};
use studio_core::analysis::{
    self, AnalysisError, Folksentia, Progress, Selection, Settings, Status,
};
use studio_core::db::Database;

/// Called on the main thread when a run ended, with a line for the status
/// bar. The window reloads the library then, so new BPMs show.
pub(crate) type OnFinished = Rc<dyn Fn(&str)>;

/// The analysis runs of one window: at most one at a time, of the library
/// or of a job list.
pub(crate) struct Analyses {
    runtime: Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    running: RefCell<Option<tokio::task::AbortHandle>>,
    /// The last progress of the running analysis.
    progress: RefCell<Option<Shown>>,
    /// The job list run being set up; kept while the dialog is closed.
    setup: RefCell<JobSetup>,
    dialog: RefCell<Weak<Dialog>>,
    on_finished: OnFinished,
}

/// Progress as the dialog shows it.
#[derive(Clone, Debug, PartialEq)]
struct Shown {
    fraction: f64,
    text: String,
}

/// What a job list run needs (#12).
#[derive(Default)]
struct JobSetup {
    /// The job list and what it holds, or why it cannot be used.
    list: Option<(PathBuf, Result<JobList, String>)>,
    /// The local copy of folksonic's music folder.
    music: Option<PathBuf>,
    /// The result file; an existing one is continued.
    results: Option<PathBuf>,
    size_only: bool,
}

impl Drop for Analyses {
    /// The window is gone: stop the run and its folksentia processes.
    fn drop(&mut self) {
        if let Some(handle) = self.running.get_mut().take() {
            handle.abort();
        }
    }
}

/// What the dialog found when it looked: the settings, the program, and how
/// far the library is analysed.
struct Look {
    settings: Result<Settings, String>,
    tool: Result<Folksentia, String>,
    status: Result<Status, String>,
    /// The local music folders, to offer the first as folksonic's.
    folders: Vec<PathBuf>,
}

struct Dialog {
    window: gtk::Window,
    tool: gtk::Label,
    summary: gtk::Label,
    parallel: gtk::SpinButton,
    retry: gtk::CheckButton,
    newer: gtk::CheckButton,
    start: gtk::Button,
    jobs_file: gtk::Label,
    music: gtk::Label,
    results: gtk::Label,
    size_only: gtk::CheckButton,
    run_jobs: gtk::Button,
    import_help: gtk::Label,
    stop: gtk::Button,
    bar: gtk::ProgressBar,
    progress: gtk::Label,
    /// Set while code moves `parallel`, so it does not save.
    updating: Cell<bool>,
    look: RefCell<Option<Look>>,
    owner: Weak<Analyses>,
}

impl Analyses {
    pub(crate) fn new(
        runtime: &Arc<tokio::runtime::Runtime>,
        db: Arc<Database>,
        on_finished: OnFinished,
    ) -> Rc<Self> {
        Rc::new(Self {
            runtime: Arc::clone(runtime),
            db,
            running: RefCell::default(),
            progress: RefCell::default(),
            setup: RefCell::default(),
            dialog: RefCell::default(),
            on_finished,
        })
    }

    /// Shows the dialog over `parent`, or brings the open one to the front.
    pub(crate) fn present(self: &Rc<Self>, parent: &gtk::ApplicationWindow) {
        if let Some(dialog) = self.dialog.borrow().upgrade() {
            dialog.window.present();
            return;
        }
        let dialog = Dialog::build(parent, Rc::downgrade(self));
        *self.dialog.borrow_mut() = Rc::downgrade(&dialog);
        dialog.show_running(self.progress.borrow().as_ref(), self.is_running());
        dialog.refresh();
        dialog.window.present();
    }

    fn is_running(&self) -> bool {
        self.running.borrow().is_some()
    }

    /// Starts analysing the library, unless a run is going on.
    fn start_library(self: &Rc<Self>, tool: Folksentia, selection: Selection, parallel: usize) {
        let (sender, receiver) = async_channel::unbounded::<Progress>();
        let work = analysis::analyse_library(
            Arc::clone(&self.db),
            tool,
            selection,
            parallel,
            move |progress| {
                // The receiver is gone only when the window is.
                let _ = sender.try_send(progress.clone());
            },
        );
        self.start_run(work, receiver, library_shown, finished_text);
    }

    /// Runs a job list from folksonic (#12), unless a run is going on.
    fn start_jobs(self: &Rc<Self>, tool: Folksentia, list: JobList, run: JobRun) {
        let (sender, receiver) = async_channel::unbounded::<JobProgress>();
        let work = jobs::run(tool, list, run, move |progress| {
            // The receiver is gone only when the window is.
            let _ = sender.try_send(progress.clone());
        });
        self.start_run(work, receiver, jobs_shown, jobs_finished_text);
    }

    /// Runs `work` on the runtime, shows what `receiver` reports, and tells
    /// the window when it ended.
    fn start_run<P: 'static, T: Send + 'static>(
        self: &Rc<Self>,
        work: impl Future<Output = Result<T, AnalysisError>> + Send + 'static,
        receiver: async_channel::Receiver<P>,
        shown: fn(&P) -> Shown,
        finished: fn(&T) -> String,
    ) {
        if self.is_running() {
            return;
        }
        let handle = self.runtime.spawn(work);
        *self.running.borrow_mut() = Some(handle.abort_handle());
        *self.progress.borrow_mut() = None;
        self.update_dialog();

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(progress) = receiver.recv().await {
                let Some(me) = weak.upgrade() else { break };
                *me.progress.borrow_mut() = Some(shown(&progress));
                me.update_dialog();
            }
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = handle.await;
            let Some(me) = weak.upgrade() else { return };
            me.running.borrow_mut().take();
            let text = match result {
                Ok(Ok(done)) => finished(&done),
                Ok(Err(error)) => format!("The analysis stopped: {error}"),
                Err(error) if error.is_cancelled() => {
                    "The analysis was stopped. Start it again to continue.".to_owned()
                }
                Err(error) => format!("The analysis stopped: {error}"),
            };
            me.update_dialog();
            if let Some(dialog) = me.dialog.borrow().upgrade() {
                dialog.progress.set_text(&text);
                dialog.refresh();
            }
            (me.on_finished)(&text);
        });
    }

    /// Stops the running analysis; its folksentia processes are killed.
    fn stop(&self) {
        if let Some(handle) = self.running.borrow().as_ref() {
            handle.abort();
        }
    }

    fn update_dialog(&self) {
        if let Some(dialog) = self.dialog.borrow().upgrade() {
            dialog.show_running(self.progress.borrow().as_ref(), self.is_running());
        }
    }
}

impl Dialog {
    fn build(parent: &gtk::ApplicationWindow, owner: Weak<Analyses>) -> Rc<Self> {
        let window = gtk::Window::builder()
            .transient_for(parent)
            .title("Analyse local files")
            .default_width(560)
            .build();
        let intro = gtk::Label::builder()
            .label(
                "folksentia finds tempo, beats and bars, key, loudness, ReplayGain, silence, \
                 the waveform and a fingerprint of the files in your music folders. \
                 It runs in the background at the lowest priority; you can close this window.",
            )
            .wrap(true)
            .xalign(0.0)
            .build();
        let tool = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        let choose = gtk::Button::with_label("Choose program…");
        choose.set_tooltip_text(Some("Use a folksentia program that is not on the PATH"));
        let search = gtk::Button::with_label("Look for it");
        search.set_tooltip_text(Some("Find folksentia on the PATH again"));
        let tool_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        tool_buttons.append(&choose);
        tool_buttons.append(&search);

        let parallel = gtk::SpinButton::with_range(1.0, f64::from(analysis::MAX_PARALLEL), 1.0);
        parallel.update_property(&[gtk::accessible::Property::Label(
            "folksentia programs at a time",
        )]);
        let parallel_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        parallel_row.append(&gtk::Label::new(Some("Files at a time:")));
        parallel_row.append(&parallel);
        parallel_row.append(&gtk::Label::new(Some("(about 1 GB of memory each)")));

        let summary = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let retry = gtk::CheckButton::with_label("Try failed files again");
        let newer =
            gtk::CheckButton::with_label("Analyse again what another folksentia version analysed");
        let bar = gtk::ProgressBar::new();
        let progress = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let start = gtk::Button::with_label("Analyse");
        start.set_halign(gtk::Align::End);

        // Offline analysis for folksonic (#12).
        let offline_intro = gtk::Label::builder()
            .label(
                "folksonic can have its library analysed on this computer. On the server, \
                 export a job list with \
                 tinysonic analysis export -c /etc/tinysonic/tinysonic.toml -all -out jobs.json \
                 and copy it here. Studio runs it against your copy of the server's music \
                 folder and writes results.jsonl for the server to import.",
            )
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .build();
        let path_label = || {
            gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .build()
        };
        let jobs_file = path_label();
        let music = path_label();
        let results = path_label();
        let open_jobs = gtk::Button::with_label("Open…");
        open_jobs.update_property(&[gtk::accessible::Property::Label("Open a job list")]);
        let choose_music = gtk::Button::with_label("Choose…");
        choose_music.update_property(&[gtk::accessible::Property::Label(
            "Choose the copy of the music folder",
        )]);
        let choose_results = gtk::Button::with_label("Choose…");
        choose_results
            .update_property(&[gtk::accessible::Property::Label("Choose the result file")]);
        let grid = gtk::Grid::builder()
            .column_spacing(12)
            .row_spacing(6)
            .build();
        for (row, (name, label, button)) in [
            ("Job list", &jobs_file, &open_jobs),
            ("Music folder", &music, &choose_music),
            ("Result file", &results, &choose_results),
        ]
        .into_iter()
        .enumerate()
        {
            let row = i32::try_from(row).unwrap_or(0);
            let name = gtk::Label::builder()
                .label(name)
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build();
            button.set_valign(gtk::Align::Center);
            grid.attach(&name, 0, row, 1, 1);
            grid.attach(label, 1, row, 1, 1);
            grid.attach(button, 2, row, 1, 1);
        }
        let size_only = gtk::CheckButton::with_label(
            "Compare sizes only (for a copy that did not keep modification times)",
        );
        let run_jobs = gtk::Button::with_label("Run job list");
        run_jobs.set_halign(gtk::Align::End);
        let import_help = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();

        let stop = gtk::Button::with_label("Stop");
        let close = gtk::Button::with_label("Close");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&stop);
        buttons.append(&close);

        let heading = |text: &str| {
            gtk::Label::builder()
                .label(text)
                .css_classes(["heading"])
                .xalign(0.0)
                .build()
        };
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        body.append(&intro);
        body.append(&heading("Program"));
        body.append(&tool);
        body.append(&tool_buttons);
        body.append(&parallel_row);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&heading("Local files"));
        body.append(&summary);
        body.append(&retry);
        body.append(&newer);
        body.append(&start);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&heading("Job list from folksonic"));
        body.append(&offline_intro);
        body.append(&grid);
        body.append(&size_only);
        body.append(&run_jobs);
        body.append(&import_help);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&bar);
        body.append(&progress);
        body.append(&buttons);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&body)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(760)
            .build();
        window.set_child(Some(&scroll));

        let dialog = Rc::new(Self {
            window,
            tool,
            summary,
            parallel,
            retry,
            newer,
            start,
            jobs_file,
            music,
            results,
            size_only,
            run_jobs,
            import_help,
            stop,
            bar,
            progress,
            updating: Cell::new(false),
            look: RefCell::default(),
            owner,
        });
        {
            let window = dialog.window.clone();
            close.connect_clicked(move |_| window.close());
        }
        let weak = Rc::downgrade(&dialog);
        dialog.start.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.start();
            }
        });
        let weak = Rc::downgrade(&dialog);
        open_jobs.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.choose_jobs();
            }
        });
        let weak = Rc::downgrade(&dialog);
        choose_music.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.choose_music();
            }
        });
        let weak = Rc::downgrade(&dialog);
        choose_results.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.choose_results();
            }
        });
        let weak = Rc::downgrade(&dialog);
        dialog.size_only.connect_toggled(move |check| {
            let Some(me) = weak.upgrade() else { return };
            if let Some(owner) = me.owner.upgrade() {
                owner.setup.borrow_mut().size_only = check.is_active();
            }
            me.show_setup();
        });
        let weak = Rc::downgrade(&dialog);
        dialog.run_jobs.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.start_jobs();
            }
        });
        let weak = Rc::downgrade(&dialog);
        dialog.stop.connect_clicked(move |_| {
            if let Some(owner) = weak.upgrade().and_then(|me| me.owner.upgrade()) {
                owner.stop();
            }
        });
        let weak = Rc::downgrade(&dialog);
        choose.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.choose_program();
            }
        });
        let weak = Rc::downgrade(&dialog);
        search.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.save(|s| s.folksentia = None);
            }
        });
        let weak = Rc::downgrade(&dialog);
        dialog.parallel.connect_value_changed(move |spin| {
            let Some(me) = weak.upgrade() else { return };
            if !me.updating.get() {
                let n = u8::try_from(spin.value_as_int()).unwrap_or(1);
                me.save(move |s| s.parallel = n);
            }
        });
        // The window holds the dialog state until it closes.
        {
            let keep = RefCell::new(Some(Rc::clone(&dialog)));
            dialog.window.connect_destroy(move |_| {
                keep.borrow_mut().take();
            });
        }
        dialog
    }

    /// Runs `work` on the runtime and `done` on the main thread afterwards.
    fn spawn<T: Send + 'static>(
        self: &Rc<Self>,
        work: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let handle = owner.runtime.spawn(work);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            if let (Ok(value), Some(me)) = (handle.await, weak.upgrade()) {
                done(&me, value);
            }
        });
    }

    /// Looks for folksentia and counts what is analysed.
    fn refresh(self: &Rc<Self>) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        self.start.set_sensitive(false);
        self.run_jobs.set_sensitive(false);
        self.tool.set_text("Looking for folksentia…");
        self.spawn(look(Arc::clone(&owner.db)), |me, look| {
            me.show_look(&look);
            if let Some(owner) = me.owner.upgrade() {
                let mut setup = owner.setup.borrow_mut();
                if setup.music.is_none() {
                    setup.music = look.folders.first().cloned();
                }
            }
            *me.look.borrow_mut() = Some(look);
            me.show_setup();
        });
    }

    fn show_look(&self, look: &Look) {
        self.tool.set_text(&match &look.tool {
            Ok(tool) => format!("folksentia {} ({})", tool.version(), tool.path().display()),
            Err(error) => error.clone(),
        });
        if let Ok(settings) = &look.settings {
            self.updating.set(true);
            self.parallel.set_value(f64::from(settings.parallel));
            self.updating.set(false);
        }
        self.summary.set_text(&match &look.status {
            Ok(status) => status_text(status),
            Err(error) => format!("The library could not be read: {error}"),
        });
    }

    /// "Analyse" can start when folksentia was found and nothing runs; "Run
    /// job list" also needs a usable list, a folder and a result file.
    fn update_start(&self) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let running = owner.is_running();
        let ready = self
            .look
            .borrow()
            .as_ref()
            .is_some_and(|l| l.tool.is_ok() && l.settings.is_ok());
        let setup = owner.setup.borrow();
        let jobs_ready = matches!(setup.list, Some((_, Ok(_))))
            && setup.music.is_some()
            && setup.results.is_some();
        self.start.set_sensitive(ready && !running);
        self.run_jobs.set_sensitive(ready && jobs_ready && !running);
        self.stop.set_sensitive(running);
    }

    /// Shows the job list run as it is set up.
    fn show_setup(&self) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        {
            let setup = owner.setup.borrow();
            self.jobs_file.set_text(&match &setup.list {
                None => "None yet".to_owned(),
                Some((path, Ok(list))) => list_text(path, list),
                Some((path, Err(error))) => format!("{}: {error}", path.display()),
            });
            let shown = |path: &Option<PathBuf>| {
                path.as_ref()
                    .map_or_else(|| "None yet".to_owned(), |p| p.display().to_string())
            };
            self.music.set_text(&shown(&setup.music));
            self.results.set_text(&shown(&setup.results));
            if self.size_only.is_active() != setup.size_only {
                self.size_only.set_active(setup.size_only);
            }
            self.import_help.set_text(&import_text(
                setup.results.as_ref().and_then(|p| p.file_name()),
                setup.size_only,
            ));
        }
        self.update_start();
    }

    fn show_running(&self, progress: Option<&Shown>, running: bool) {
        if let Some(progress) = progress {
            self.bar.set_fraction(progress.fraction);
            if running {
                self.progress.set_text(&progress.text);
            }
        } else if running {
            self.bar.set_fraction(0.0);
            self.progress.set_text("Starting…");
        }
        self.update_start();
    }

    fn start(self: &Rc<Self>) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let ready = self.look.borrow().as_ref().and_then(clone_ready);
        let Some((tool, settings)) = ready else {
            return;
        };
        let selection = Selection {
            retry_failed: self.retry.is_active(),
            other_versions: self.newer.is_active(),
        };
        owner.start_library(tool, selection, analysis::parallel_for(settings.parallel));
    }

    fn start_jobs(self: &Rc<Self>) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let ready = self.look.borrow().as_ref().and_then(clone_ready);
        let Some((tool, settings)) = ready else {
            return;
        };
        let (list, run) = {
            let setup = owner.setup.borrow();
            let (Some((_, Ok(list))), Some(music), Some(results)) =
                (&setup.list, &setup.music, &setup.results)
            else {
                return;
            };
            let run = JobRun {
                music_root: music.clone(),
                results: results.clone(),
                size_only: setup.size_only,
                parallel: analysis::parallel_for(settings.parallel),
            };
            (list.clone(), run)
        };
        owner.start_jobs(tool, list, run);
    }

    /// Opens a job list and reads it. A result file next to it is offered.
    fn choose_jobs(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Job lists (JSON)"));
        filter.add_pattern("*.json");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let chooser = gtk::FileDialog::builder()
            .title("Open a job list from folksonic")
            .modal(true)
            .filters(&filters)
            .build();
        let weak = Rc::downgrade(self);
        chooser.open(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                let (Some(me), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                let Some(path) = file.path() else {
                    me.jobs_file
                        .set_text("Only a file on this computer can be opened.");
                    return;
                };
                me.jobs_file.set_text("Reading…");
                let read = path.clone();
                me.spawn(
                    async move {
                        tokio::task::spawn_blocking(move || jobs::load(&read))
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r.map_err(|e| e.to_string()))
                    },
                    move |me, list| {
                        if let Some(owner) = me.owner.upgrade() {
                            let mut setup = owner.setup.borrow_mut();
                            if setup.results.is_none() {
                                setup.results = path.parent().map(|dir| dir.join("results.jsonl"));
                            }
                            setup.list = Some((path, list));
                        }
                        me.show_setup();
                    },
                );
            },
        );
    }

    fn choose_music(self: &Rc<Self>) {
        let chooser = gtk::FileDialog::builder()
            .title("The copy of folksonic's music folder")
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
                if let (Some(path), Some(owner)) = (file.path(), me.owner.upgrade()) {
                    owner.setup.borrow_mut().music = Some(path);
                }
                me.show_setup();
            },
        );
    }

    /// Picks where results go. An existing file is continued, not replaced.
    fn choose_results(self: &Rc<Self>) {
        let chooser = gtk::FileDialog::builder()
            .title("Result file for folksonic")
            .modal(true)
            .initial_name("results.jsonl")
            .accept_label("Choose")
            .build();
        let weak = Rc::downgrade(self);
        chooser.save(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                let (Some(me), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                if let (Some(path), Some(owner)) = (file.path(), me.owner.upgrade()) {
                    owner.setup.borrow_mut().results = Some(path);
                }
                me.show_setup();
            },
        );
    }

    fn choose_program(self: &Rc<Self>) {
        let chooser = gtk::FileDialog::builder()
            .title("Choose the folksentia program")
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        chooser.open(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                let (Some(me), Ok(file)) = (weak.upgrade(), result) else {
                    return;
                };
                match file.path() {
                    Some(path) => me.save(move |s| s.folksentia = Some(path)),
                    None => me
                        .tool
                        .set_text("Only a program on this computer can be used."),
                }
            },
        );
    }

    /// Saves the settings with `change` applied, then looks again.
    fn save(self: &Rc<Self>, change: impl FnOnce(&mut Settings)) {
        let (Some(owner), Some(Ok(mut settings))) = (
            self.owner.upgrade(),
            self.look.borrow().as_ref().map(|l| l.settings.clone()),
        ) else {
            return;
        };
        change(&mut settings);
        let db = Arc::clone(&owner.db);
        self.spawn(
            async move {
                tokio::task::spawn_blocking(move || analysis::set_settings(&db, &settings))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()))
            },
            |me, result| {
                if let Err(error) = result {
                    me.tool
                        .set_text(&format!("The setting could not be saved: {error}"));
                    return;
                }
                me.refresh();
            },
        );
    }
}

/// The program and settings, when folksentia was found.
fn clone_ready(look: &Look) -> Option<(Folksentia, Settings)> {
    Some((look.tool.clone().ok()?, look.settings.clone().ok()?))
}

/// Reads the settings, looks for folksentia and counts the analysed tracks.
async fn look(db: Arc<Database>) -> Look {
    let reader_db = Arc::clone(&db);
    let reader = Arc::clone(&db);
    let settings = tokio::task::spawn_blocking(move || {
        analysis::settings(&reader.reader().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    let tool = match &settings {
        Ok(settings) => Folksentia::find(settings.folksentia.as_deref())
            .await
            .map_err(|e| e.to_string()),
        Err(error) => Err(error.clone()),
    };
    let version = tool.as_ref().ok().map(|t| t.version().to_owned());
    let status = tokio::task::spawn_blocking(move || {
        analysis::status(&db.reader().map_err(|e| e.to_string())?, version.as_deref())
            .map_err(|e| e.to_string())
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    let folders = {
        let db = Arc::clone(&reader_db);
        tokio::task::spawn_blocking(move || {
            let conn = db.reader().ok()?;
            studio_core::local_library::folders(&conn).ok()
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
        .into_iter()
        .map(|f| f.path)
        .collect()
    };
    Look {
        settings,
        tool,
        status,
        folders,
    }
}

fn status_text(status: &Status) -> String {
    if status.tracks == 0 {
        return "No local files yet. Add a folder under Music folders.".to_owned();
    }
    let mut text = format!(
        "{} of {} files analysed, {} to do",
        status.analysed, status.tracks, status.pending
    );
    if status.failed > 0 {
        text.push_str(&format!(", {} failed", status.failed));
    }
    text.push('.');
    if status.other_version > 0 {
        text.push_str(&format!(
            " {} were analysed by another folksentia version.",
            status.other_version
        ));
    }
    text
}

fn fraction(done: usize, total: usize) -> f64 {
    if total == 0 {
        1.0
    } else {
        done as f64 / total as f64
    }
}

fn library_shown(progress: &Progress) -> Shown {
    Shown {
        fraction: fraction(progress.done, progress.total),
        text: progress_text(progress),
    }
}

fn jobs_shown(progress: &JobProgress) -> Shown {
    Shown {
        fraction: fraction(progress.done, progress.total),
        text: jobs_progress_text(progress),
    }
}

/// A job list as the dialog lists it.
fn list_text(path: &std::path::Path, list: &JobList) -> String {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    match &list.created {
        Some(created) => format!("{name}: {} tracks, exported {created}", list.jobs.len()),
        None => format!("{name}: {} tracks", list.jobs.len()),
    }
}

/// How to import the result file on the server.
fn import_text(results: Option<&std::ffi::OsStr>, size_only: bool) -> String {
    let file = results.map_or_else(
        || "results.jsonl".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let flag = if size_only { " -size-only" } else { "" };
    format!(
        "Then copy {file} to the server, check it, and store it:\n\
         tinysonic analysis import -c /etc/tinysonic/tinysonic.toml -dry-run{flag} {file}\n\
         tinysonic analysis import -c /etc/tinysonic/tinysonic.toml{flag} {file}\n\
         The check stores nothing. A stopped run continues the same result file."
    )
}

fn jobs_progress_text(progress: &JobProgress) -> String {
    let mut text = format!("{} of {} jobs", progress.done, progress.total);
    for (count, what) in [
        (progress.failed, "failed"),
        (progress.missing, "missing"),
        (progress.changed, "changed"),
    ] {
        if count > 0 {
            text.push_str(&format!(", {count} {what}"));
        }
    }
    if progress.already > 0 {
        text.push_str(&format!(" ({} were done before)", progress.already));
    }
    if let Some(last) = &progress.last {
        text.push_str(&format!(". Last: {last}"));
    }
    if let Some(error) = &progress.last_error {
        text.push_str(&format!("\nLast error: {error}"));
    }
    text
}

fn jobs_finished_text(progress: &JobProgress) -> String {
    if progress.total == 0 {
        return format!(
            "Job list: all {} jobs are in the result file. Import it on the server.",
            progress.jobs
        );
    }
    let mut text = format!("Job list: {} jobs done", progress.done);
    let mut notes = Vec::new();
    for (count, what) in [
        (progress.failed, "failed"),
        (progress.missing, "not in the music folder"),
        (progress.changed, "changed since the export"),
    ] {
        if count > 0 {
            notes.push(format!("{count} {what}"));
        }
    }
    if !notes.is_empty() {
        text.push_str(&format!(" ({})", notes.join(", ")));
    }
    text.push_str(". Import the result file on the server.");
    text
}

fn progress_text(progress: &Progress) -> String {
    let mut text = format!("{} of {} files", progress.done, progress.total);
    if progress.failed > 0 {
        text.push_str(&format!(", {} failed", progress.failed));
    }
    if let Some(last) = &progress.last {
        text.push_str(&format!(". Last: {last}"));
    }
    if let Some(error) = &progress.last_error {
        text.push_str(&format!("\nLast error: {error}"));
    }
    text
}

fn finished_text(progress: &Progress) -> String {
    match (progress.total, progress.failed) {
        (0, _) => "Analysis: nothing to do.".to_owned(),
        (total, 0) => format!("Analysis: {total} files analysed."),
        (total, failed) => format!(
            "Analysis: {} files analysed, {failed} failed.",
            total - failed
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
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

    fn save(widget: &gtk::Widget, path: &std::path::Path) {
        // Let the window draw first.
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

    /// Opens the dialog with a fake folksentia, analyses three files (one
    /// that folksentia cannot read) and saves pictures to `SHOTS`.
    #[test]
    #[ignore = "local: needs a display and SHOTS"]
    fn the_dialog_analyses_the_local_files() {
        let shots = std::path::PathBuf::from(std::env::var("SHOTS").expect("SHOTS"));
        let base = shots.join("analysis");
        let _ = std::fs::remove_dir_all(&base);
        let music = base.join("music");
        std::fs::create_dir_all(&music).expect("music");
        for name in ["a good.flac", "b broken.flac", "c good.flac"] {
            std::fs::write(music.join(name), b"audio").expect("file");
        }
        let program = base.join("folksentia");
        std::fs::write(
            &program,
            r#"#!/bin/sh
for last; do :; done
case "$1" in --version) printf 'folksentia 0.9.0+gui\nChromaprint no\n'; exit 0 ;; esac
/bin/sleep 1
case "$last" in
  *broken*) printf '{"schema":2,"file":"x","error":"no audio decoded"}'; exit 1 ;;
  *) printf '{"schema":2,"file":"x","bpm":118.0,"beats":[0.5,1.0]}' ;;
esac
"#,
        )
        .expect("program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("mode");

        let db = Arc::new(Database::open(base.join("library.db")).expect("db"));
        let now = studio_core::sync::now_ms();
        let folder = studio_core::local_library::add_folder(&db, &music, now).expect("folder");
        studio_core::local_library::scan_folder(&db, &folder, now).expect("scan");
        analysis::set_settings(
            &db,
            &Settings {
                folksentia: Some(program),
                parallel: 1,
            },
        )
        .expect("settings");
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
        let finished = Rc::new(RefCell::new(String::new()));
        let seen = Rc::clone(&finished);
        let analyses = Analyses::new(
            &runtime,
            Arc::clone(&db),
            Rc::new(move |text| *seen.borrow_mut() = text.to_owned()),
        );
        analyses.present(&parent);
        let dialog = analyses.dialog.borrow().upgrade().expect("dialog");
        pump_until(5000, || dialog.start.is_sensitive());
        assert!(dialog.start.is_sensitive(), "folksentia found");
        assert_eq!(
            dialog.tool.text(),
            format!(
                "folksentia 0.9.0+gui ({})",
                base.join("folksentia")
                    .canonicalize()
                    .expect("path")
                    .display()
            )
        );
        assert_eq!(dialog.summary.text(), "0 of 3 files analysed, 3 to do.");
        save(
            dialog.window.upcast_ref(),
            &shots.join("analysis-1-ready.png"),
        );

        dialog.start.emit_clicked();
        pump_until(1800, || dialog.progress.text().starts_with("1 of 3"));
        assert!(dialog.stop.is_sensitive() && !dialog.start.is_sensitive());
        save(
            dialog.window.upcast_ref(),
            &shots.join("analysis-2-running.png"),
        );

        pump_until(15000, || !finished.borrow().is_empty());
        assert_eq!(*finished.borrow(), "Analysis: 2 files analysed, 1 failed.");
        pump_until(3000, || dialog.summary.text().starts_with("2 of 3"));
        assert_eq!(
            dialog.summary.text(),
            "2 of 3 files analysed, 0 to do, 1 failed."
        );
        save(
            dialog.window.upcast_ref(),
            &shots.join("analysis-3-done.png"),
        );

        // Closing and opening the dialog shows the same state.
        dialog.window.close();
        drop(dialog);
        pump_until(300, || false);
        analyses.present(&parent);
        let dialog = analyses.dialog.borrow().upgrade().expect("dialog");
        pump_until(5000, || dialog.summary.text().starts_with("2 of 3"));
        assert!(!dialog.stop.is_sensitive());

        // A job list from folksonic (#12), run against the same folder.
        let stamp = |name: &str| {
            let meta = std::fs::metadata(music.join(name)).expect("meta");
            let mtime = meta
                .modified()
                .expect("mtime")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after 1970")
                .as_millis();
            serde_json::json!({"id": format!("so-{name}"), "path": name,
                "size": meta.len(), "mtimeMs": mtime as i64, "features": ["bpm"]})
        };
        let list = serde_json::json!({
            "format": "tinysonic-analysis-jobs", "version": 1,
            "created": "2026-11-13T10:00:00Z", "tool": "folksentia", "schema": 1,
            "options": {"downbeats": true},
            "jobs": [stamp("a good.flac"), stamp("b broken.flac"),
                     {"id": "so-x", "path": "gone.flac", "size": 1, "mtimeMs": 1,
                      "features": ["bpm"]}],
        });
        let jobs_path = base.join("jobs.json");
        std::fs::write(&jobs_path, list.to_string()).expect("jobs");
        {
            let read = jobs::load(&jobs_path).expect("list");
            let mut setup = analyses.setup.borrow_mut();
            setup.list = Some((jobs_path.clone(), Ok(read)));
            setup.results = Some(base.join("results.jsonl"));
        }
        dialog.show_setup();
        assert_eq!(
            dialog.music.text(),
            music.canonicalize().expect("music").display().to_string(),
            "the local folder is offered"
        );
        assert_eq!(
            dialog.jobs_file.text(),
            "jobs.json: 3 tracks, exported 2026-11-13T10:00:00Z"
        );
        assert!(dialog.run_jobs.is_sensitive());
        finished.borrow_mut().clear();
        dialog.run_jobs.emit_clicked();
        pump_until(15000, || !finished.borrow().is_empty());
        assert_eq!(
            *finished.borrow(),
            "Job list: 3 jobs done (1 failed, 1 not in the music folder). \
             Import the result file on the server."
        );
        let results = std::fs::read_to_string(base.join("results.jsonl")).expect("results");
        assert_eq!(results.lines().count(), 3);
        save(
            dialog.window.upcast_ref(),
            &shots.join("analysis-4-jobs.png"),
        );
    }

    #[test]
    fn texts_say_how_far_it_is() {
        let status = Status {
            tracks: 10,
            analysed: 6,
            failed: 1,
            pending: 3,
            other_version: 2,
        };
        assert_eq!(
            status_text(&status),
            "6 of 10 files analysed, 3 to do, 1 failed. \
             2 were analysed by another folksentia version."
        );
        assert_eq!(
            status_text(&Status::default()),
            "No local files yet. Add a folder under Music folders."
        );
        let progress = Progress {
            total: 5,
            done: 2,
            failed: 1,
            last: Some("tune.flac".to_owned()),
            last_error: Some("no audio".to_owned()),
        };
        assert_eq!(
            progress_text(&progress),
            "2 of 5 files, 1 failed. Last: tune.flac\nLast error: no audio"
        );
        assert_eq!(
            finished_text(&Progress {
                done: 5,
                ..progress
            }),
            "Analysis: 4 files analysed, 1 failed."
        );

        let jobs = JobProgress {
            jobs: 10,
            already: 4,
            total: 6,
            done: 3,
            failed: 1,
            missing: 1,
            changed: 0,
            last: Some("A/tune.flac".to_owned()),
            last_error: None,
        };
        assert_eq!(
            jobs_progress_text(&jobs),
            "3 of 6 jobs, 1 failed, 1 missing (4 were done before). Last: A/tune.flac"
        );
        assert_eq!(
            jobs_finished_text(&jobs),
            "Job list: 3 jobs done (1 failed, 1 not in the music folder). \
             Import the result file on the server."
        );
        assert_eq!(
            jobs_finished_text(&JobProgress { total: 0, ..jobs }),
            "Job list: all 10 jobs are in the result file. Import it on the server."
        );
        let help = import_text(Some(std::ffi::OsStr::new("out.jsonl")), true);
        assert!(help.contains(
            "tinysonic analysis import -c /etc/tinysonic/tinysonic.toml -dry-run -size-only out.jsonl"
        ));
        assert!(help.contains("tinysonic.toml -size-only out.jsonl\n"));
    }
}
