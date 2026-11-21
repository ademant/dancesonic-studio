//! The main window (#8): library sidebar, track list, track detail and the
//! player bar. Plain GTK4 widgets and symbolic icons, so it follows the theme
//! on GNOME, Xfce and Budgie.
//!
//! All database and network work runs on the Tokio runtime; results come back
//! to the main thread through [`Window::spawn`].

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::glib::BoxedAnyObject;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use studio_core::catalog::{Song, letter_of};
use studio_core::local_library::FolderWatcher;
use studio_core::offline::PinKind;
use studio_core::plays::PlayCounter;
use studio_core::queue::SongId;
use studio_core::tempo::{MAX_RATE, MIN_RATE, Rate};

use crate::mpris::{Command, Mpris, NowPlaying};
use crate::player::{Player, PlayerEvent, Track};
use crate::seek_bar::SeekBar;
use crate::session::{self, Catalog, Selection, Session};
use crate::{Banner, Services, connection_dialog, folders_dialog};

mod remote_commands;

/// How often the position is read from the engine.
const POSITION_TICK: Duration = Duration::from_millis(250);

/// A watched folder is scanned again once its files were quiet this long.
const FOLDER_QUIET: Duration = Duration::from_secs(2);

/// A sidebar entry. Sections (letters) select nothing.
struct Node {
    title: String,
    selection: Option<Selection>,
    children: Vec<Rc<Node>>,
}

pub(crate) struct Window {
    pub(crate) window: gtk::ApplicationWindow,
    runtime: Arc<tokio::runtime::Runtime>,
    banner: Banner,
    services: RefCell<Option<Services>>,
    session: RefCell<Option<Session>>,
    player: RefCell<Option<Player>>,
    status: gtk::Label,
    sidebar: Sidebar,
    tracks: TrackList,
    detail: Detail,
    bar: PlayerBar,
    /// The songs in the player's queue, in queue order.
    queue: RefCell<Vec<Song>>,
    now_playing: Cell<Option<usize>>,
    playing: Cell<bool>,
    rate: Cell<Rate>,
    /// Bumped on every new selection, so a late answer for an old one is dropped.
    request: Cell<u64>,
    /// The desktop's media controls (#7).
    mpris: Mpris,
    /// Says when the current track has played long enough to report (#31).
    plays: RefCell<PlayCounter>,
    /// Watches the local music folders (#10).
    watcher: RefCell<Option<FolderWatcher>>,
    /// Remote control by phones (#20, #24).
    remote: RefCell<Option<Rc<crate::remote::Remote>>>,
    /// The state the phones were last told, and when.
    published: RefCell<(Option<studio_core::remote::messages::PlayState>, i64)>,
}

struct Sidebar {
    widget: gtk::Box,
    library: gio::ListStore,
    dances: gio::ListStore,
    playlists: gio::ListStore,
    selections: Vec<gtk::SingleSelection>,
}

struct TrackList {
    widget: gtk::Box,
    view: gtk::ColumnView,
    store: gio::ListStore,
    sorted: gtk::SortListModel,
    selection: gtk::SingleSelection,
    title: gtk::Label,
    /// "Keep offline" for the album or playlist shown (#17).
    keep: gtk::CheckButton,
    /// Set while code changes `keep`, so it does not pin.
    updating: Cell<bool>,
    /// What the list shows, with its title.
    shown: RefCell<Option<(String, Selection)>>,
}

struct Detail {
    widget: gtk::ScrolledWindow,
    cover: gtk::Picture,
    title: gtk::Label,
    artist: gtk::Label,
    album: gtk::Label,
    length: gtk::Label,
    tempo: gtk::Label,
    dances: gtk::Label,
    shoes: gtk::Label,
    playlist_choice: gtk::DropDown,
    playlist_names: gtk::StringList,
    playlist_ids: RefCell<Vec<String>>,
    add_button: gtk::Button,
    song: RefCell<Option<Song>>,
}

struct PlayerBar {
    widget: gtk::Box,
    seek: SeekBar,
    previous: gtk::Button,
    play: gtk::Button,
    next: gtk::Button,
    time: gtk::Label,
    now: gtk::Label,
    tempo_mode: gtk::ToggleButton,
    bpm_mode: gtk::ToggleButton,
    speed: gtk::Scale,
    speed_label: gtk::Label,
    reset: gtk::Button,
    /// "BT" or "Wi-Fi" while a phone controls Studio.
    remote: gtk::Label,
    /// Set while code moves the speed slider, so it does not change the rate.
    updating: Cell<bool>,
}

impl Window {
    pub(crate) fn new(app: &gtk::Application, runtime: &Arc<tokio::runtime::Runtime>) -> Rc<Self> {
        let banner = Banner::new();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .margin_start(6)
            .margin_end(6)
            .margin_bottom(6)
            .build();
        let sidebar = Sidebar::new(&status);
        let tracks = TrackList::new();
        let detail = Detail::new();
        let bar = PlayerBar::new();

        let inner = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&tracks.widget)
            .end_child(&detail.widget)
            .resize_end_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .build();
        let outer = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&sidebar.widget)
            .end_child(&inner)
            .resize_start_child(false)
            .shrink_start_child(false)
            .position(280)
            .build();
        inner.set_position(620);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&banner.revealer);
        content.append(&outer);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&bar.widget);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title(studio_core::APP_NAME)
            .default_width(1200)
            .default_height(800)
            .child(&content)
            .build();

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search title, artist, release")
            .width_chars(36)
            .build();
        search.update_property(&[gtk::accessible::Property::Label("Search the library")]);
        let connect_button = gtk::Button::with_label("Connect…");
        let sync_button = icon_button(
            "view-refresh-symbolic",
            "Update the library from the server",
        );
        let folders_button = icon_button("folder-music-symbolic", "Music folders on this computer");
        let remote_button = icon_button("phone-symbolic", "Remote control by a phone");
        let header = gtk::HeaderBar::new();
        header.pack_start(&connect_button);
        header.pack_start(&sync_button);
        header.pack_start(&folders_button);
        header.pack_end(&remote_button);
        header.set_title_widget(Some(&search));
        window.set_titlebar(Some(&header));

        let (mpris, commands) = Mpris::start(runtime);
        let me = Rc::new(Self {
            window,
            runtime: Arc::clone(runtime),
            banner,
            services: RefCell::default(),
            session: RefCell::default(),
            player: RefCell::default(),
            status,
            sidebar,
            tracks,
            detail,
            bar,
            queue: RefCell::default(),
            now_playing: Cell::new(None),
            plays: RefCell::default(),
            watcher: RefCell::default(),
            remote: RefCell::default(),
            published: RefCell::default(),
            playing: Cell::new(false),
            rate: Cell::new(Rate::NORMAL),
            request: Cell::new(0),
            mpris,
        });
        let weak = Rc::downgrade(&me);
        glib::spawn_future_local(async move {
            while let Ok(command) = commands.recv().await {
                let Some(me) = weak.upgrade() else { break };
                me.on_command(command);
            }
        });
        // Every handler holds the state weakly; the window holds it strongly
        // until it is destroyed, which also ends playback.
        let keep = RefCell::new(Some(Rc::clone(&me)));
        me.window.connect_destroy(move |_| {
            keep.borrow_mut().take();
        });
        me.start_player();
        me.connect_signals(&connect_button, &sync_button, &search);
        let weak = Rc::downgrade(&me);
        folders_button.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.show_folders();
            }
        });
        let weak = Rc::downgrade(&me);
        remote_button.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.show_remote();
            }
        });
        me.show_detail(None);
        me.update_speed_controls();
        me
    }

    pub(crate) fn banner(&self) -> &Banner {
        &self.banner
    }

    /// Runs `work` on the runtime and hands its result to `done` on the main
    /// thread, as long as the window still exists.
    fn spawn<T: Send + 'static>(
        self: &Rc<Self>,
        work: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) {
        let handle = self.runtime.spawn(work);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(value) => {
                    if let Some(me) = weak.upgrade() {
                        done(&me, value);
                    }
                }
                Err(error) => tracing::warn!(%error, "a background task stopped"),
            }
        });
    }

    fn start_player(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        // Events may come from inside a player call; handle them afterwards,
        // so the handler never re-enters the player.
        let player = Player::new(move |event| {
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(me) = weak.upgrade() {
                    me.on_player_event(event);
                }
            });
        });
        match player {
            Ok(player) => *self.player.borrow_mut() = Some(player),
            Err(error) => {
                tracing::error!(%error, "no audio engine");
                self.banner
                    .show(&format!("Playback is not available: {error}"));
            }
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(POSITION_TICK, move || {
            let Some(me) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            me.tick();
            glib::ControlFlow::Continue
        });
    }

    fn connect_signals(
        self: &Rc<Self>,
        connect_button: &gtk::Button,
        sync_button: &gtk::Button,
        search: &gtk::SearchEntry,
    ) {
        let weak = Rc::downgrade(self);
        connect_button.connect_clicked(move |_| {
            let Some(me) = weak.upgrade() else { return };
            let services = me.services.borrow().clone();
            match services {
                Some(services) => {
                    let again = Rc::downgrade(&me);
                    connection_dialog::present(
                        &me.window,
                        &me.runtime,
                        services,
                        Rc::new(move || {
                            if let Some(me) = again.upgrade() {
                                me.open_session();
                            }
                        }),
                        {
                            let weak = Rc::downgrade(&me);
                            Rc::new(move |key: &str| {
                                let me = weak.upgrade()?;
                                let remote = me.remote.borrow().clone()?;
                                remote.sign_out(key)
                            })
                        },
                    );
                }
                None => me
                    .banner
                    .show("Studio is still starting. Try again in a moment."),
            }
        });

        let weak = Rc::downgrade(self);
        sync_button.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.sync();
            }
        });

        let weak = Rc::downgrade(self);
        search.connect_search_changed(move |entry| {
            let Some(me) = weak.upgrade() else { return };
            let text = entry.text().trim().to_owned();
            if text.is_empty() {
                return;
            }
            me.show_songs(format!("Search: {text}"), Selection::Search(text));
        });

        for selection in &self.sidebar.selections {
            let weak = Rc::downgrade(self);
            selection.connect_selection_changed(move |selection, _, _| {
                let Some(me) = weak.upgrade() else { return };
                let Some(row) = selection.selected_item().and_downcast::<gtk::TreeListRow>() else {
                    return;
                };
                let Some(node) = node_of(&row) else { return };
                if let Some(chosen) = node.selection.clone() {
                    me.show_songs(node.title.clone(), chosen);
                }
            });
        }

        let weak = Rc::downgrade(self);
        self.tracks.view.connect_activate(move |_, position| {
            if let Some(me) = weak.upgrade() {
                me.play_list(position);
            }
        });
        let weak = Rc::downgrade(self);
        self.tracks
            .selection
            .connect_selection_changed(move |selection, _, _| {
                let Some(me) = weak.upgrade() else { return };
                let song = selection
                    .selected_item()
                    .and_downcast::<BoxedAnyObject>()
                    .map(|o| o.borrow::<Song>().clone());
                if song.is_some() {
                    me.show_detail(song);
                }
            });

        let weak = Rc::downgrade(self);
        self.detail.add_button.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.add_to_playlist();
            }
        });

        let weak = Rc::downgrade(self);
        self.tracks.keep.connect_toggled(move |check| {
            let Some(me) = weak.upgrade() else { return };
            if !me.tracks.updating.get() {
                me.set_kept_offline(check.is_active());
            }
        });

        self.connect_player_bar();
    }

    /// Pins or unpins the album or playlist shown, then updates the copies.
    fn set_kept_offline(self: &Rc<Self>, keep: bool) {
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        let Some((title, selection)) = self.tracks.shown.borrow().clone() else {
            return;
        };
        let Some((kind, item)) = pin_of(&selection) else {
            return;
        };
        self.status.set_text(if keep {
            "Downloading for offline use…"
        } else {
            "Removing offline copies…"
        });
        self.spawn(
            async move { session.set_pinned(kind, item, keep).await },
            move |me, result| {
                me.show_offline_result(result);
                let current = me.tracks.shown.borrow().clone();
                if current.as_ref().is_some_and(|(_, s)| *s == selection) {
                    me.show_songs(title, selection);
                }
            },
        );
    }

    fn show_offline_result(&self, result: Result<studio_core::offline::SyncReport, String>) {
        self.status.set_text(&match result {
            Ok(report) => {
                let mut text = format!(
                    "Offline: {} downloaded, {} removed, {} MB used.",
                    report.downloaded,
                    report.removed,
                    report.used_bytes / 1_000_000
                );
                if report.over_limit > 0 {
                    text.push_str(&format!(
                        " {} tracks do not fit within the size limit.",
                        report.over_limit
                    ));
                }
                if report.failed > 0 {
                    text.push_str(&format!(
                        " {} could not be downloaded; the next update tries again.",
                        report.failed
                    ));
                }
                text
            }
            Err(error) => format!("Offline copies could not be updated: {error}"),
        });
    }

    fn connect_player_bar(self: &Rc<Self>) {
        let bar = &self.bar;
        let weak = Rc::downgrade(self);
        bar.play.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.toggle_play();
            }
        });
        let weak = Rc::downgrade(self);
        bar.next.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.with_player(|p| p.next());
            }
        });
        let weak = Rc::downgrade(self);
        bar.previous.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.with_player(|p| p.previous());
            }
        });
        let weak = Rc::downgrade(self);
        bar.seek.connect_seek(move |target| {
            if let Some(me) = weak.upgrade() {
                me.seek(target);
            }
        });
        let weak = Rc::downgrade(self);
        bar.speed.connect_value_changed(move |scale| {
            let Some(me) = weak.upgrade() else { return };
            if me.bar.updating.get() {
                return;
            }
            let factor = if me.bar.bpm_mode.is_active() {
                match me.current_bpm() {
                    Some(bpm) => scale.value() / bpm,
                    None => 1.0,
                }
            } else {
                scale.value() / 100.0
            };
            me.set_rate(factor);
        });
        let weak = Rc::downgrade(self);
        bar.reset.connect_clicked(move |_| {
            if let Some(me) = weak.upgrade() {
                me.set_rate(1.0);
            }
        });
        let weak = Rc::downgrade(self);
        bar.bpm_mode.connect_toggled(move |_| {
            if let Some(me) = weak.upgrade() {
                me.update_speed_controls();
            }
        });
    }

    /// Calls the player, showing a failure in the banner.
    fn with_player(
        &self,
        action: impl FnOnce(&mut Player) -> Result<(), crate::player::PlayerError>,
    ) {
        let result = match self.player.borrow_mut().as_mut() {
            Some(player) => action(player),
            None => return,
        };
        if let Err(error) = result {
            tracing::warn!(%error, "player refused");
            self.status.set_text(&error.to_string());
        }
    }

    /// The services are ready: open the last server.
    pub(crate) fn set_services(self: &Rc<Self>, services: Services) {
        self.start_remote(&services);
        *self.services.borrow_mut() = Some(services);
        self.open_session();
        self.watch_folders();
    }

    fn open_session(self: &Rc<Self>) {
        let Some(services) = self.services.borrow().clone() else {
            return;
        };
        self.status.set_text("Opening the library…");
        self.spawn(session::open(services), |me, opened| match opened {
            Ok(Some((session, tracks))) => {
                if let Some(player) = me.player.borrow_mut().as_mut() {
                    player.set_tracks(tracks);
                }
                if let Some(remote) = me.remote.borrow().as_ref() {
                    remote.set_server(Some(crate::remote::RelayServer {
                        http: session::settings_for(session.client.server().base().as_str()),
                        client: session.client.clone(),
                    }));
                }
                *me.session.borrow_mut() = Some(session);
                me.reload_catalog();
                me.sync();
            }
            Ok(None) => me
                .status
                .set_text("Not connected. Use Connect… to add a server."),
            Err(error) => {
                tracing::warn!(%error, "the library could not be opened");
                me.status
                    .set_text(&format!("The library could not be opened: {error}"));
            }
        });
    }

    /// The music folders dialog (#10).
    fn show_folders(self: &Rc<Self>) {
        let Some(services) = self.services.borrow().clone() else {
            self.banner
                .show("Studio is still starting. Try again in a moment.");
            return;
        };
        let weak = Rc::downgrade(self);
        folders_dialog::present(
            &self.window,
            &self.runtime,
            services.db,
            Rc::new(move || {
                if let Some(me) = weak.upgrade() {
                    me.reload_catalog();
                    me.watch_folders();
                    me.keep_offline();
                }
            }),
        );
    }

    /// Brings the offline copies in line with the pins and the settings.
    fn keep_offline(self: &Rc<Self>) {
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        self.spawn(async move { session.sync_offline().await }, |me, result| {
            me.show_offline_result(result);
        });
    }

    /// Scans the folders once for what changed while Studio was closed, and
    /// (re)starts watching them.
    fn watch_folders(self: &Rc<Self>) {
        let Some(services) = self.services.borrow().clone() else {
            return;
        };
        self.spawn(session::folders(services.db), |me, folders| {
            let folders = match folders {
                Ok(folders) => folders,
                Err(error) => {
                    tracing::warn!(%error, "the music folders could not be read");
                    return;
                }
            };
            // The old watcher stops when it is dropped.
            me.watcher.borrow_mut().take();
            if folders.is_empty() {
                return;
            }
            let (sender, changes) = async_channel::unbounded::<i64>();
            match FolderWatcher::start(&folders, FOLDER_QUIET, move |folder| {
                // The receiver is gone only when the window is.
                let _ = sender.send_blocking(folder);
            }) {
                Ok(watcher) => *me.watcher.borrow_mut() = Some(watcher),
                Err(error) => {
                    tracing::warn!(%error, "the music folders cannot be watched");
                    me.status.set_text(&format!(
                        "Music folders are not watched for changes: {error}"
                    ));
                }
            }
            let weak = Rc::downgrade(me);
            glib::spawn_future_local(async move {
                while let Ok(folder) = changes.recv().await {
                    let Some(me) = weak.upgrade() else { break };
                    me.rescan(Some(folder));
                }
            });
            me.rescan(None);
        });
    }

    /// Scans one music folder, or all for `None`, then shows the result.
    fn rescan(self: &Rc<Self>, folder: Option<i64>) {
        let Some(services) = self.services.borrow().clone() else {
            return;
        };
        self.spawn(
            session::rescan(services.db, folder),
            |me, result| match result {
                Ok(report) if report.added + report.updated + report.moved + report.removed > 0 => {
                    me.status.set_text(&format!(
                        "Music folders: {}",
                        folders_dialog::scan_text(&report)
                    ));
                    me.reload_catalog();
                }
                Ok(_) => {}
                Err(error) => me
                    .status
                    .set_text(&format!("The music folders could not be read: {error}")),
            },
        );
    }

    fn sync(self: &Rc<Self>) {
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        self.status.set_text("Updating the library…");
        self.spawn(async move { session.sync().await }, |me, result| {
            let synced = result.is_ok();
            me.status.set_text(&match result {
                Ok(text) => text,
                Err(error) => format!("The library could not be updated: {error}"),
            });
            me.reload_catalog();
            // Pinned albums and playlists may have changed (#17).
            if synced && let Some(session) = me.session.borrow().clone() {
                me.spawn(async move { session.sync_offline().await }, |me, result| {
                    if let Ok(report) = &result
                        && report.downloaded + report.removed + report.failed + report.over_limit
                            == 0
                    {
                        return;
                    }
                    me.show_offline_result(result);
                });
            }
        });
        // Plays made while offline go out now that the server answers again.
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        self.spawn(async move { session.send_plays().await }, |_, ()| {});
        self.rescan(None);
    }

    fn reload_catalog(self: &Rc<Self>) {
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        self.spawn(
            async move { session.catalog().await },
            |me, result| match result {
                Ok(catalog) => me.fill_sidebar(&catalog),
                Err(error) => me
                    .status
                    .set_text(&format!("The library could not be read: {error}")),
            },
        );
    }

    fn fill_sidebar(&self, catalog: &Catalog) {
        let open: Vec<_> = self.sidebar.selections.iter().map(expanded).collect();
        replace(&self.sidebar.library, library_nodes(catalog));
        replace(
            &self.sidebar.dances,
            lettered(catalog.dances.iter().map(|d| {
                leaf(
                    format!("{} ({})", d.name, d.songs),
                    &d.name,
                    Selection::Dance(d.tag_id),
                )
            })),
        );
        replace(
            &self.sidebar.playlists,
            lettered(
                catalog
                    .playlists
                    .iter()
                    .map(|(id, name)| leaf(name.clone(), name, Selection::Playlist(id.clone()))),
            ),
        );
        for (selection, open) in self.sidebar.selections.iter().zip(&open) {
            expand(selection, open);
        }
        let names: Vec<&str> = catalog.playlists.iter().map(|(_, n)| n.as_str()).collect();
        let count = self.detail.playlist_names.n_items();
        self.detail.playlist_names.splice(0, count, &names);
        *self.detail.playlist_ids.borrow_mut() =
            catalog.playlists.iter().map(|(id, _)| id.clone()).collect();
        self.update_add_button();
    }

    fn show_songs(self: &Rc<Self>, title: String, selection: Selection) {
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        let request = self.request.get() + 1;
        self.request.set(request);
        *self.tracks.shown.borrow_mut() = Some((title.clone(), selection.clone()));
        self.tracks.keep.set_visible(false);
        if let Some((kind, item)) = pin_of(&selection) {
            let reader = session.clone();
            self.spawn(
                async move { reader.pinned(kind, item).await },
                move |me, on| {
                    if me.request.get() == request {
                        me.tracks.updating.set(true);
                        me.tracks.keep.set_active(on);
                        me.tracks.updating.set(false);
                        me.tracks.keep.set_visible(true);
                    }
                },
            );
        }
        self.spawn(
            async move { session.songs(selection).await },
            move |me, result| {
                if me.request.get() != request {
                    return;
                }
                match result {
                    Ok(songs) => {
                        me.tracks
                            .title
                            .set_text(&format!("{title} · {} tracks", songs.len()));
                        let items: Vec<BoxedAnyObject> =
                            songs.into_iter().map(BoxedAnyObject::new).collect();
                        me.tracks.store.splice(0, me.tracks.store.n_items(), &items);
                    }
                    Err(error) => me
                        .status
                        .set_text(&format!("The tracks could not be read: {error}")),
                }
            },
        );
    }

    /// Plays the track list in its shown order, from `position`.
    fn play_list(self: &Rc<Self>, position: u32) {
        let model = &self.tracks.sorted;
        let mut songs = Vec::new();
        let mut queue = Vec::new();
        let mut start = 0;
        for index in 0..model.n_items() {
            let Some(item) = model.item(index).and_downcast::<BoxedAnyObject>() else {
                continue;
            };
            let song = item.borrow::<Song>().clone();
            let Ok(id) = SongId::parse(&song.id) else {
                continue;
            };
            if index == position {
                start = queue.len();
            }
            queue.push(Track {
                song: id,
                gain_db: song.track_gain,
                peak: song.track_peak,
                local: song.local_path.clone(),
            });
            songs.push(song);
        }
        if queue.is_empty() {
            return;
        }
        *self.queue.borrow_mut() = songs;
        self.with_player(|p| p.load(queue, start));
    }

    fn toggle_play(self: &Rc<Self>) {
        if self.now_playing.get().is_none() {
            let position = self.tracks.selection.selected();
            if position != gtk::INVALID_LIST_POSITION {
                self.play_list(position);
            } else if self.tracks.sorted.n_items() > 0 {
                self.play_list(0);
            }
            return;
        }
        if self.playing.get() {
            self.with_player(|p| p.pause());
            self.playing.set(false);
        } else {
            self.with_player(|p| p.resume());
            self.playing.set(true);
        }
        self.update_play_button();
    }

    fn on_player_event(self: &Rc<Self>, event: PlayerEvent) {
        match event {
            PlayerEvent::TrackChanged(index) => {
                self.now_playing.set(Some(index));
                self.playing.set(true);
                let song = self.queue.borrow().get(index).cloned();
                self.on_track_changed(song);
            }
            PlayerEvent::Finished => {
                self.plays.borrow_mut().stop();
                self.playing.set(false);
                self.now_playing.set(None);
                self.bar.now.set_text("");
                self.mpris.set_track(None, false, false);
            }
            PlayerEvent::Failed(message) => {
                self.status.set_text(&format!("Playback failed: {message}"));
            }
        }
        self.update_play_button();
    }

    fn on_track_changed(self: &Rc<Self>, song: Option<Song>) {
        let Some(song) = song else { return };
        let artist = song.artist_name.clone().unwrap_or_default();
        self.bar.now.set_text(&if artist.is_empty() {
            song.title.clone()
        } else {
            format!("{} — {artist}", song.title)
        });
        self.bar.seek.set_waveform(None);
        // BPM mode keeps the target BPM, so the new track's rate follows it.
        if self.bar.bpm_mode.is_active() {
            let target = self.bar.speed.value();
            match song.bpm {
                Some(bpm) if bpm > 0.0 => self.set_rate(target / bpm),
                _ => self.bar.tempo_mode.set_active(true),
            }
        }
        self.update_speed_controls();
        if self.detail.song.borrow().is_none() {
            self.show_detail(Some(song.clone()));
        }
        let index = self.now_playing.get().unwrap_or(0);
        let now = NowPlaying {
            index,
            title: song.title.clone(),
            artist: song.artist_name.clone(),
            album: song.album_name.clone(),
            length_ms: song.duration_ms,
            art_url: None,
        };
        let has_next = index + 1 < self.queue.borrow().len();
        // Previous always works: on the first track it restarts it.
        self.mpris.set_track(Some(now.clone()), true, has_next);
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        if !song.on_server {
            // A local-only track: nothing to ask the server or report to it.
            self.plays.borrow_mut().stop();
            return;
        }
        if let Some(cover) = song.cover_art.clone() {
            let reader = session.clone();
            self.spawn(
                async move { reader.cover_file(cover).await },
                move |me, art_url| {
                    if art_url.is_some() && me.now_playing.get() == Some(index) {
                        let now = NowPlaying { art_url, ..now };
                        me.mpris.set_track(Some(now), true, has_next);
                    }
                },
            );
        }
        let Ok(id) = SongId::parse(&song.id) else {
            self.plays.borrow_mut().stop();
            return;
        };
        self.plays
            .borrow_mut()
            .start(id.clone(), studio_core::sync::now_ms(), song.duration_ms);
        let reporter = session.clone();
        let playing = id.clone();
        self.spawn(
            async move { reporter.now_playing(playing).await },
            |_, ()| {},
        );
        let expected = song.id.clone();
        self.spawn(
            async move { session.waveform(id).await },
            move |me, wave| {
                let current = me
                    .now_playing
                    .get()
                    .and_then(|i| me.queue.borrow().get(i).map(|s| s.id.clone()));
                if current.as_deref() == Some(expected.as_str()) {
                    me.bar.seek.set_waveform(wave.as_ref().map(|w| w.peaks()));
                }
            },
        );
    }

    fn tick(self: &Rc<Self>) {
        self.publish_remote();
        let position = self.player.borrow().as_ref().and_then(Player::position_ms);
        let Some((position, length)) = position else {
            return;
        };
        let length = length
            .or_else(|| self.current_song().and_then(|s| s.duration_ms))
            .unwrap_or(0);
        if self.now_playing.get().is_some() {
            let counted = self.plays.borrow_mut().position(position);
            if let (Some(play), Some(session)) = (counted, self.session.borrow().clone()) {
                self.spawn(async move { session.record_play(play).await }, |_, ()| {});
            }
            self.mpris.set_position(position);
            self.bar.seek.set_position(position, length);
            self.bar
                .time
                .set_text(&format!("{} / {}", clock(position), clock(length)));
        }
    }

    fn current_song(&self) -> Option<Song> {
        let index = self.now_playing.get()?;
        self.queue.borrow().get(index).cloned()
    }

    fn current_bpm(&self) -> Option<f64> {
        self.current_song()?.bpm.filter(|b| *b > 0.0)
    }

    fn set_rate(&self, factor: f64) {
        let rate = Rate::new(factor.clamp(MIN_RATE, MAX_RATE)).unwrap_or(Rate::NORMAL);
        self.rate.set(rate);
        self.with_player(|p| p.set_rate(rate));
        self.mpris.set_rate(rate.factor());
        self.update_speed_controls();
    }

    /// Seeks in the current track and tells the media controls.
    fn seek(&self, target_ms: u64) {
        self.with_player(|p| p.seek_to(target_ms));
        self.mpris.seeked(target_ms);
    }

    /// A command from the desktop's media controls.
    fn on_command(self: &Rc<Self>, command: Command) {
        tracing::debug!(?command, "media control");
        match command {
            Command::Play if !self.playing.get() => self.toggle_play(),
            Command::Pause if self.playing.get() => self.toggle_play(),
            Command::Play | Command::Pause => {}
            Command::PlayPause => self.toggle_play(),
            Command::Stop => {
                if let Some(player) = self.player.borrow().as_ref() {
                    player.stop();
                }
                self.plays.borrow_mut().stop();
                self.playing.set(false);
                self.now_playing.set(None);
                self.bar.now.set_text("");
                self.mpris.set_track(None, false, false);
                self.update_play_button();
            }
            Command::Next => self.with_player(|p| p.next()),
            Command::Previous => self.with_player(|p| p.previous()),
            Command::SeekBy(offset) => {
                let position = self.player.borrow().as_ref().and_then(Player::position_ms);
                let Some((position, length)) = position else {
                    return;
                };
                let target = position.saturating_add_signed(offset);
                // The spec: seeking past the end goes to the next track.
                if length.is_some_and(|l| target >= l) {
                    self.with_player(|p| p.next());
                } else {
                    self.seek(target);
                }
            }
            Command::SetPosition { index, position_ms } => {
                if self.now_playing.get() == Some(index) {
                    self.seek(position_ms);
                }
            }
            Command::SetRate(rate) => self.set_rate(rate),
            Command::SetVolume(volume) => {
                if let Some(player) = self.player.borrow().as_ref() {
                    player.set_volume(volume);
                    self.mpris.set_volume(player.volume());
                }
            }
            Command::Raise => self.window.present(),
            Command::Quit => self.window.close(),
        }
    }

    /// Puts the speed slider, its label and the mode switch in line with the
    /// rate and the current track's BPM.
    fn update_speed_controls(&self) {
        let bar = &self.bar;
        let rate = self.rate.get();
        let bpm = self.current_bpm();
        bar.bpm_mode.set_sensitive(bpm.is_some());
        if bpm.is_none() && bar.bpm_mode.is_active() {
            // Without a BPM, BPM mode cannot work: fall back to Tempo.
            bar.tempo_mode.set_active(true);
        }
        bar.updating.set(true);
        match bpm.filter(|_| bar.bpm_mode.is_active()) {
            Some(bpm) => {
                bar.speed.set_range(bpm * MIN_RATE, bpm * MAX_RATE);
                bar.speed.set_increments(1.0, 5.0);
                bar.speed.set_value(bpm * rate.factor());
            }
            None => {
                bar.speed.set_range(MIN_RATE * 100.0, MAX_RATE * 100.0);
                bar.speed.set_increments(1.0, 5.0);
                bar.speed.set_value(rate.percent());
            }
        }
        bar.updating.set(false);
        let percent = format!("{:.0} %", rate.percent());
        bar.speed_label.set_text(&match rate.bpm_of(bpm) {
            Some(target) => format!("{percent} ⇄ {target:.0} BPM"),
            None => percent,
        });
        bar.reset.set_sensitive(rate != Rate::NORMAL);
    }

    fn update_play_button(&self) {
        let (icon, label) = if self.playing.get() {
            ("media-playback-pause-symbolic", "Pause")
        } else {
            ("media-playback-start-symbolic", "Play")
        };
        self.mpris
            .set_playing(self.now_playing.get().map(|_| self.playing.get()));
        self.bar.play.set_icon_name(icon);
        self.bar.play.set_tooltip_text(Some(label));
        self.bar
            .play
            .update_property(&[gtk::accessible::Property::Label(label)]);
    }

    fn show_detail(self: &Rc<Self>, song: Option<Song>) {
        let d = &self.detail;
        *d.song.borrow_mut() = song.clone();
        d.cover.set_paintable(None::<&gdk::Paintable>);
        let Some(song) = song else {
            d.title.set_text("No track selected");
            for label in [
                &d.artist, &d.album, &d.length, &d.tempo, &d.dances, &d.shoes,
            ] {
                label.set_text("");
            }
            self.update_add_button();
            return;
        };
        d.title.set_text(&song.title);
        d.artist
            .set_text(song.artist_name.as_deref().unwrap_or("Unknown artist"));
        d.album.set_text(&match (&song.album_name, song.year) {
            (Some(album), Some(year)) => format!("{album} ({year})"),
            (Some(album), None) => album.clone(),
            (None, _) => String::new(),
        });
        d.length
            .set_text(&song.duration_ms.map(clock).unwrap_or_default());
        d.tempo.set_text(&match song.bpm {
            Some(bpm) => format!("{bpm:.0} BPM"),
            None => "Tempo not known".to_owned(),
        });
        d.dances.set_text("…");
        // Shoe ratings come with #16 (phase 4).
        d.shoes.set_text("Not rated yet");
        self.update_add_button();

        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        let expected = song.id.clone();
        let reader = session.clone();
        let mbid = song.mbid.clone();
        self.spawn(async move { reader.dances_of(mbid).await }, {
            let expected = expected.clone();
            move |me, names| {
                if me.detail_is(&expected) {
                    me.detail.dances.set_text(&if names.is_empty() {
                        "No dance known".to_owned()
                    } else {
                        names.join(", ")
                    });
                }
            }
        });
        if let Some(cover) = song.cover_art.clone() {
            self.spawn(
                async move { session.cover(cover).await },
                move |me, bytes| {
                    let Some(bytes) = bytes else { return };
                    if !me.detail_is(&expected) {
                        return;
                    }
                    match gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                        Ok(texture) => me.detail.cover.set_paintable(Some(&texture)),
                        Err(error) => tracing::debug!(%error, "cover could not be decoded"),
                    }
                },
            );
        }
    }

    fn detail_is(&self, song_id: &str) -> bool {
        self.detail
            .song
            .borrow()
            .as_ref()
            .is_some_and(|s| s.id == song_id)
    }

    fn update_add_button(&self) {
        let d = &self.detail;
        // Server playlists can only hold songs the server has.
        let has =
            d.song.borrow().as_ref().is_some_and(|s| s.on_server) && d.playlist_names.n_items() > 0;
        d.add_button.set_sensitive(has);
        d.playlist_choice
            .set_sensitive(d.playlist_names.n_items() > 0);
    }

    fn add_to_playlist(self: &Rc<Self>) {
        let d = &self.detail;
        let Some(song) = d.song.borrow().clone() else {
            return;
        };
        let Some(playlist) = d
            .playlist_ids
            .borrow()
            .get(d.playlist_choice.selected() as usize)
            .cloned()
        else {
            return;
        };
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        let Ok(id) = SongId::parse(&song.id) else {
            return;
        };
        d.add_button.set_sensitive(false);
        self.status.set_text("Adding to the playlist…");
        self.spawn(
            async move { session.add_to_playlist(playlist, id).await },
            |me, result| {
                me.update_add_button();
                me.status.set_text(&match result {
                    Ok(()) => "Added to the playlist.".to_owned(),
                    Err(error) => format!("The track could not be added: {error}"),
                });
                me.reload_catalog();
            },
        );
    }
}

impl Sidebar {
    fn new(status: &gtk::Label) -> Self {
        let stack = gtk::Stack::new();
        let switcher = gtk::StackSwitcher::builder()
            .stack(&stack)
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let mut selections = Vec::new();
        let mut stores = Vec::new();
        for (name, title) in [
            ("library", "Library"),
            ("dances", "Dances"),
            ("playlists", "Playlists"),
        ] {
            let store = gio::ListStore::new::<BoxedAnyObject>();
            let tree = gtk::TreeListModel::new(store.clone(), false, false, |item| {
                let node = item
                    .downcast_ref::<BoxedAnyObject>()?
                    .borrow::<Rc<Node>>()
                    .clone();
                if node.children.is_empty() {
                    return None;
                }
                let children = gio::ListStore::new::<BoxedAnyObject>();
                for child in &node.children {
                    children.append(&BoxedAnyObject::new(Rc::clone(child)));
                }
                Some(children.upcast())
            });
            let selection = gtk::SingleSelection::builder()
                .model(&tree)
                .autoselect(false)
                .can_unselect(true)
                .build();
            let view = gtk::ListView::new(Some(selection.clone()), Some(tree_factory()));
            view.update_property(&[gtk::accessible::Property::Label(title)]);
            // Enter on a letter or artist opens or closes it.
            view.connect_activate(|view, position| {
                if let Some(row) = view
                    .model()
                    .and_then(|m| m.item(position))
                    .and_downcast::<gtk::TreeListRow>()
                    && row.is_expandable()
                {
                    row.set_expanded(!row.is_expanded());
                }
            });
            let scroll = gtk::ScrolledWindow::builder()
                .child(&view)
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build();
            stack.add_titled(&scroll, Some(name), title);
            selections.push(selection);
            stores.push(store);
        }
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.set_width_request(220);
        widget.append(&switcher);
        widget.append(&stack);
        widget.append(status);
        let mut stores = stores.into_iter();
        let mut next = || {
            stores
                .next()
                .unwrap_or_else(gio::ListStore::new::<BoxedAnyObject>)
        };
        Self {
            widget,
            library: next(),
            dances: next(),
            playlists: next(),
            selections,
        }
    }
}

/// Rows with a disclosure triangle and a label.
fn tree_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let expander = gtk::TreeExpander::new();
        expander.set_child(Some(&label));
        item.set_child(Some(&expander));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else {
            return;
        };
        let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        expander.set_list_row(Some(&row));
        if let (Some(label), Some(node)) =
            (expander.child().and_downcast::<gtk::Label>(), node_of(&row))
        {
            label.set_text(&node.title);
            if node.selection.is_none() {
                label.add_css_class("heading");
            } else {
                label.remove_css_class("heading");
            }
        }
    });
    factory
}

fn node_of(row: &gtk::TreeListRow) -> Option<Rc<Node>> {
    let item = row.item().and_downcast::<BoxedAnyObject>()?;
    let node = item.borrow::<Rc<Node>>().clone();
    Some(node)
}

/// The titles from the top down to `row`.
fn path_of(row: &gtk::TreeListRow) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = Some(row.clone());
    while let Some(row) = current {
        if let Some(node) = node_of(&row) {
            path.push(node.title.clone());
        }
        current = row.parent();
    }
    path.reverse();
    path
}

/// The entries that are open, so a reload can open them again.
fn expanded(selection: &gtk::SingleSelection) -> std::collections::HashSet<Vec<String>> {
    (0..selection.n_items())
        .filter_map(|i| selection.item(i).and_downcast::<gtk::TreeListRow>())
        .filter(gtk::TreeListRow::is_expanded)
        .map(|row| path_of(&row))
        .collect()
}

/// Opens the entries in `open` again, parents first.
fn expand(selection: &gtk::SingleSelection, open: &std::collections::HashSet<Vec<String>>) {
    let mut index = 0;
    // The count grows while entries open, so it is read on every step.
    while index < selection.n_items() {
        if let Some(row) = selection.item(index).and_downcast::<gtk::TreeListRow>()
            && row.is_expandable()
            && open.contains(&path_of(&row))
        {
            row.set_expanded(true);
        }
        index += 1;
    }
}

fn replace(store: &gio::ListStore, nodes: Vec<Rc<Node>>) {
    let items: Vec<BoxedAnyObject> = nodes.into_iter().map(BoxedAnyObject::new).collect();
    store.splice(0, store.n_items(), &items);
}

/// A sidebar entry: `title` is shown, `key` decides its letter.
fn leaf(title: String, key: &str, selection: Selection) -> (String, Rc<Node>) {
    (
        letter_of(key),
        Rc::new(Node {
            title,
            selection: Some(selection),
            children: Vec::new(),
        }),
    )
}

/// Groups entries under their first letter, keeping their order within a
/// letter.
fn lettered(entries: impl Iterator<Item = (String, Rc<Node>)>) -> Vec<Rc<Node>> {
    let mut letters: BTreeMap<String, Vec<Rc<Node>>> = BTreeMap::new();
    for (letter, node) in entries {
        letters.entry(letter).or_default().push(node);
    }
    letters
        .into_iter()
        .map(|(letter, children)| {
            Rc::new(Node {
                title: letter,
                selection: None,
                children,
            })
        })
        .collect()
}

/// Where a song can be played from (#10).
fn location(song: &Song) -> &'static str {
    match (song.on_server, song.local_path.is_some()) {
        (true, true) if song.kept_offline => "Offline",
        (true, true) => "Both",
        (true, false) => "Server",
        (false, _) => "Local",
    }
}

/// The album or playlist a selection keeps offline (#17).
fn pin_of(selection: &Selection) -> Option<(PinKind, String)> {
    match selection {
        Selection::Album(id) => Some((PinKind::Album, id.clone())),
        Selection::Playlist(id) => Some((PinKind::Playlist, id.clone())),
        _ => None,
    }
}

/// Letter → artist → release. Artists with tracks only in local folders are
/// merged by name with the server's, or listed on their own (#10).
fn library_nodes(catalog: &Catalog) -> Vec<Rc<Node>> {
    let mut by_artist: BTreeMap<&str, Vec<Rc<Node>>> = BTreeMap::new();
    for album in &catalog.albums {
        let Some(artist) = album.artist_id.as_deref() else {
            continue;
        };
        let title = match album.year {
            Some(year) => format!("{} ({year})", album.name),
            None => album.name.clone(),
        };
        by_artist.entry(artist).or_default().push(Rc::new(Node {
            title,
            selection: Some(Selection::Album(album.id.clone())),
            children: Vec::new(),
        }));
    }
    let local_albums = |artist: &studio_core::local_library::LocalArtist| -> Vec<Rc<Node>> {
        artist
            .albums
            .iter()
            .map(|album| {
                Rc::new(Node {
                    title: format!("{album} (local)"),
                    selection: Some(Selection::LocalAlbum {
                        artist: artist.name.clone(),
                        album: album.clone(),
                    }),
                    children: Vec::new(),
                })
            })
            .collect()
    };
    let mut local: Vec<&studio_core::local_library::LocalArtist> =
        catalog.local_artists.iter().collect();
    let server = catalog.artists.iter().map(|artist| {
        let key = artist.sort_name.as_deref().unwrap_or(&artist.name);
        let mut children = by_artist.remove(artist.id.as_str()).unwrap_or_default();
        if let Some(found) = local
            .iter()
            .position(|l| l.name.to_lowercase() == artist.name.to_lowercase())
        {
            children.extend(local_albums(local.remove(found)));
        }
        (
            key.to_lowercase(),
            Rc::new(Node {
                title: artist.name.clone(),
                selection: Some(Selection::Artist(artist.id.clone())),
                children,
            }),
        )
    });
    let mut entries: Vec<(String, Rc<Node>)> = server.collect();
    entries.extend(local.into_iter().map(|artist| {
        (
            artist.name.to_lowercase(),
            Rc::new(Node {
                title: artist.name.clone(),
                selection: Some(Selection::LocalArtist(artist.name.clone())),
                children: local_albums(artist),
            }),
        )
    }));
    // Server and local-only artists share their letters, sorted by name.
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    lettered(
        entries
            .into_iter()
            .map(|(key, node)| (letter_of(&key), node)),
    )
}

impl TrackList {
    fn new() -> Self {
        let store = gio::ListStore::new::<BoxedAnyObject>();
        let view = gtk::ColumnView::builder()
            .show_row_separators(false)
            .reorderable(false)
            .build();
        view.update_property(&[gtk::accessible::Property::Label("Tracks")]);
        let sorted = gtk::SortListModel::new(Some(store.clone()), view.sorter());
        let selection = gtk::SingleSelection::builder()
            .model(&sorted)
            .autoselect(false)
            .can_unselect(true)
            .build();
        view.set_model(Some(&selection));

        add_column(
            &view,
            "Name",
            true,
            0.0,
            |s| s.title.clone(),
            |a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        );
        add_column(
            &view,
            "BPM",
            false,
            1.0,
            |s| s.bpm.map_or_else(|| "—".to_owned(), |b| format!("{b:.0}")),
            |a, b| {
                a.bpm
                    .partial_cmp(&b.bpm)
                    .unwrap_or(std::cmp::Ordering::Equal)
            },
        );
        // Shoe ratings come with #16; until then every track shows "—".
        add_column(
            &view,
            "Shoes",
            false,
            0.5,
            |_| "—".to_owned(),
            |_, _| std::cmp::Ordering::Equal,
        );
        add_column(
            &view,
            "Length",
            false,
            1.0,
            |s| s.duration_ms.map(clock).unwrap_or_default(),
            |a, b| a.duration_ms.cmp(&b.duration_ms),
        );
        add_column(
            &view,
            "Where",
            false,
            0.0,
            |s| location(s).to_owned(),
            |a, b| location(a).cmp(location(b)),
        );

        let title = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["heading"])
            .margin_start(12)
            .margin_top(6)
            .margin_bottom(6)
            .label("Select an artist, dance or playlist")
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .build();
        title.set_hexpand(true);
        let keep = gtk::CheckButton::builder()
            .label("Keep offline")
            .tooltip_text("Download these tracks so they play without the server")
            .visible(false)
            .margin_end(12)
            .build();
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        head.append(&title);
        head.append(&keep);
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&head);
        widget.append(&scroll);
        Self {
            widget,
            view,
            store,
            sorted,
            selection,
            title,
            keep,
            updating: Cell::new(false),
            shown: RefCell::default(),
        }
    }
}

/// A text column that sorts with `order`; clicking the header again reverses.
fn add_column(
    view: &gtk::ColumnView,
    title: &str,
    expand: bool,
    xalign: f32,
    text: impl Fn(&Song) -> String + 'static,
    order: impl Fn(&Song, &Song) -> std::cmp::Ordering + 'static,
) {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            let label = gtk::Label::builder()
                .xalign(xalign)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(label), Some(song)) = (
            item.child().and_downcast::<gtk::Label>(),
            item.item().and_downcast::<BoxedAnyObject>(),
        ) else {
            return;
        };
        label.set_text(&text(&song.borrow::<Song>()));
    });
    let sorter = gtk::CustomSorter::new(move |a, b| {
        match (
            a.downcast_ref::<BoxedAnyObject>(),
            b.downcast_ref::<BoxedAnyObject>(),
        ) {
            (Some(a), Some(b)) => order(&a.borrow::<Song>(), &b.borrow::<Song>()).into(),
            _ => gtk::Ordering::Equal,
        }
    });
    let column = gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .sorter(&sorter)
        .expand(expand)
        .resizable(true)
        .build();
    view.append_column(&column);
}

impl Detail {
    fn new() -> Self {
        let cover = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .height_request(200)
            .alternative_text("Cover")
            .build();
        let label = |classes: &[&str]| {
            gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .css_classes(classes.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>())
                .build()
        };
        let title = label(&["title-3"]);
        let artist = label(&[]);
        let album = label(&["dim-label"]);
        let length = label(&[]);
        let tempo = label(&[]);
        let dances = label(&[]);
        let shoes = label(&[]);
        let playlist_names = gtk::StringList::new(&[]);
        let playlist_choice = gtk::DropDown::builder()
            .model(&playlist_names)
            .hexpand(true)
            .build();
        playlist_choice.update_property(&[gtk::accessible::Property::Label("Playlist")]);
        let add_button = gtk::Button::with_label("Add");
        add_button.set_tooltip_text(Some("Add this track to the chosen playlist"));

        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(12)
            .build();
        let mut row = 0;
        let mut put = |caption: &str, widget: &gtk::Widget| {
            let caption = gtk::Label::builder()
                .label(caption)
                .xalign(0.0)
                .valign(gtk::Align::Start)
                .css_classes(["dim-label"])
                .build();
            grid.attach(&caption, 0, row, 1, 1);
            grid.attach(widget, 1, row, 1, 1);
            row += 1;
        };
        put("Length", length.upcast_ref());
        put("Tempo", tempo.upcast_ref());
        put("Dance", dances.upcast_ref());
        put("Shoes", shoes.upcast_ref());
        let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        add_row.append(&playlist_choice);
        add_row.append(&add_button);
        put("Playlists", add_row.upcast_ref());

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        column.append(&cover);
        column.append(&title);
        column.append(&artist);
        column.append(&album);
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        column.append(&grid);
        let widget = gtk::ScrolledWindow::builder()
            .child(&column)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(260)
            .build();
        Self {
            widget,
            cover,
            title,
            artist,
            album,
            length,
            tempo,
            dances,
            shoes,
            playlist_choice,
            playlist_names,
            playlist_ids: RefCell::default(),
            add_button,
            song: RefCell::default(),
        }
    }
}

impl PlayerBar {
    fn new() -> Self {
        let seek = SeekBar::new();
        let previous = icon_button("media-skip-backward-symbolic", "Previous track");
        let play = icon_button("media-playback-start-symbolic", "Play");
        let next = icon_button("media-skip-forward-symbolic", "Next track");
        let time = gtk::Label::builder()
            .label("0:00 / 0:00")
            .css_classes(["numeric"])
            .build();
        let now = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let tempo_mode = gtk::ToggleButton::builder()
            .label("Tempo")
            .active(true)
            .tooltip_text("Speed as percent of the original")
            .build();
        let bpm_mode = gtk::ToggleButton::builder()
            .label("BPM")
            .group(&tempo_mode)
            .tooltip_text("Speed as a target BPM, from the track's BPM")
            .build();
        let modes = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["linked"])
            .build();
        modes.append(&tempo_mode);
        modes.append(&bpm_mode);
        let speed = gtk::Scale::with_range(gtk::Orientation::Horizontal, 50.0, 150.0, 1.0);
        speed.set_draw_value(false);
        speed.set_width_request(220);
        speed.add_mark(100.0, gtk::PositionType::Bottom, None);
        speed.update_property(&[gtk::accessible::Property::Label("Speed")]);
        let speed_label = gtk::Label::builder()
            .width_chars(16)
            .xalign(0.0)
            .css_classes(["numeric"])
            .build();
        let reset = icon_button("edit-undo-symbolic", "Back to the original speed (100 %)");
        let remote = gtk::Label::builder()
            .css_classes(["dim-label"])
            .visible(false)
            .build();

        let transport = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        transport.append(&previous);
        transport.append(&play);
        transport.append(&next);
        transport.append(&time);
        transport.append(&now);
        transport.append(&remote);
        transport.append(&modes);
        transport.append(&speed);
        transport.append(&speed_label);
        transport.append(&reset);

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        widget.append(&seek.widget);
        widget.append(&transport);
        Self {
            widget,
            seek,
            previous,
            play,
            next,
            time,
            now,
            tempo_mode,
            bpm_mode,
            speed,
            speed_label,
            reset,
            remote,
            updating: Cell::new(false),
        }
    }
}

/// A flat button with a symbolic icon, a tooltip and a screen-reader label.
fn icon_button(icon: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// `m:ss`, or `h:mm:ss` from an hour.
fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_core::catalog::{Album, Artist};

    #[test]
    fn clock_shows_minutes_and_hours() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(221_400), "3:41");
        assert_eq!(clock(3_725_000), "1:02:05");
    }

    #[test]
    fn the_library_is_grouped_by_letter_then_artist_then_release() {
        let artist = |id: &str, name: &str| Artist {
            id: id.into(),
            name: name.into(),
            sort_name: None,
            mbid: None,
        };
        let album = |id: &str, artist: &str, name: &str| Album {
            id: id.into(),
            artist_id: Some(artist.into()),
            artist_name: None,
            name: name.into(),
            sort_name: None,
            year: None,
            mbid: None,
            cover_art: None,
        };
        let catalog = Catalog {
            artists: vec![
                artist("a1", "alba"),
                artist("a2", "Blowzabella"),
                artist("a3", "Bourdon"),
            ],
            albums: vec![album("r1", "a2", "Pingha Frenzy"), album("r2", "a1", "Ar")],
            ..Catalog::default()
        };
        let tree = library_nodes(&catalog);
        let titles: Vec<&str> = tree.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["A", "B"]);
        assert!(tree.iter().all(|n| n.selection.is_none()));
        let b: Vec<&str> = tree[1].children.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(b, ["Blowzabella", "Bourdon"]);
        assert_eq!(tree[1].children[0].children[0].title, "Pingha Frenzy");
        assert_eq!(
            tree[1].children[0].children[0].selection,
            Some(Selection::Album("r1".into()))
        );
        assert!(tree[1].children[1].children.is_empty());
    }

    /// Local-only artists (#10) join a server artist of the same name, or
    /// get their own entry among the server's.
    #[test]
    fn local_artists_are_merged_into_the_library() {
        use studio_core::local_library::LocalArtist;
        let catalog = Catalog {
            artists: vec![
                Artist {
                    id: "a1".into(),
                    name: "Blowzabella".into(),
                    sort_name: None,
                    mbid: None,
                },
                Artist {
                    id: "a2".into(),
                    name: "Bourdon".into(),
                    sort_name: None,
                    mbid: None,
                },
            ],
            local_artists: vec![
                LocalArtist {
                    name: "blowzabella".into(),
                    albums: vec!["Demo".into()],
                },
                LocalArtist {
                    name: "Bal Corsé".into(),
                    albums: vec!["Live".into()],
                },
            ],
            ..Catalog::default()
        };
        let tree = library_nodes(&catalog);
        let b: Vec<&str> = tree[0].children.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(b, ["Bal Corsé", "Blowzabella", "Bourdon"]);
        assert_eq!(
            tree[0].children[0].selection,
            Some(Selection::LocalArtist("Bal Corsé".into()))
        );
        let merged = &tree[0].children[1];
        assert_eq!(merged.selection, Some(Selection::Artist("a1".into())));
        assert_eq!(merged.children[0].title, "Demo (local)");
        assert_eq!(
            merged.children[0].selection,
            Some(Selection::LocalAlbum {
                artist: "blowzabella".into(),
                album: "Demo".into()
            })
        );
    }
}

/// Opt-in check of the whole window against a local fake folksonic: browse,
/// play, waveform, speed. Saves screenshots to `SHOTS` (a directory). Signs in
/// with a session-only keyring, so nothing reaches the desktop's keyring.
#[cfg(test)]
mod ui_check {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use studio_core::connection::{HttpPolicy, ServerUrl};
    use studio_core::dancebrainz::{DanceTag, Mbid, RecordingDances};
    use studio_core::db::Database;
    use studio_core::http::HttpSettings;
    use studio_core::secrets::SecretVault;
    use studio_core::subsonic::ApiKey;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::secret_service::SecretService;

    const MBID_A: &str = "11111111-1111-4111-8111-111111111111";
    const MBID_B: &str = "22222222-2222-4222-8222-222222222222";

    fn envelope(payload: &str) -> String {
        format!(
            r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"folksonic",
            "serverVersion":"test","openSubsonic":true{payload}}}}}"#
        )
    }

    fn song(id: &str, title: &str, album: &str, artist: &str, bpm: &str, mbid: &str) -> String {
        format!(
            r#"{{"id":"{id}","updated":5,"item":{{"id":"{id}","title":"{title}","albumId":"{album}",
            "artistId":"{artist}","artist":"x","album":"y","duration":315,"bpm":{bpm},
            "musicBrainzId":{mbid},"coverArt":"{album}","suffix":"opus"}}}}"#
        )
    }

    fn answer(path: &str) -> Option<(&'static str, String)> {
        let json = "application/json";
        Some(match path {
            "/rest/getOpenSubsonicExtensions.view" => (
                json,
                envelope(
                    r#","openSubsonicExtensions":[{"name":"apiKeyAuthentication","versions":[1]},
                    {"name":"tinysonicLibraryChanges","versions":[1]},
                    {"name":"tinysonicAnalysis","versions":[1]}]"#,
                ),
            ),
            "/rest/getLibraryChanges.view" => {
                let songs = [
                    song(
                        "so-1",
                        "An dro Pleiben",
                        "al-1",
                        "ar-1",
                        "132",
                        &format!("\"{MBID_A}\""),
                    ),
                    song(
                        "so-2",
                        "Bourrée de Chaumont",
                        "al-1",
                        "ar-1",
                        "null",
                        "null",
                    ),
                    song(
                        "so-3",
                        "Mazurka pour Lili",
                        "al-2",
                        "ar-2",
                        "118",
                        &format!("\"{MBID_B}\""),
                    ),
                    song("so-4", "Scottish à Virmoux", "al-2", "ar-2", "96", "null"),
                ]
                .join(",");
                (
                    json,
                    envelope(&format!(
                        r#","libraryChanges":{{"full":true,"serverTime":1000,
                        "artists":[{{"id":"ar-1","updated":5,"item":{{"id":"ar-1","name":"Blowzabella"}}}},
                                   {{"id":"ar-2","updated":5,"item":{{"id":"ar-2","name":"Émile et les autres"}}}}],
                        "albums":[{{"id":"al-1","updated":5,"item":{{"id":"al-1","name":"Pingha Frenzy","artistId":"ar-1","year":1983,"coverArt":"al-1"}}}},
                                  {{"id":"al-2","updated":5,"item":{{"id":"al-2","name":"Bal folk","artistId":"ar-2","year":2019,"coverArt":"al-2"}}}}],
                        "songs":[{songs}],
                        "playlists":[{{"id":"pl-1","updated":7}}],"starred":[],"deleted":[]}}"#
                    )),
                )
            }
            "/rest/scrobble.view" => (json, envelope("")),
            "/rest/getPlaylist.view" => (
                json,
                envelope(
                    r#","playlist":{"id":"pl-1","name":"Friday set","entry":[{"id":"so-3"},{"id":"so-1"}]}"#,
                ),
            ),
            "/api/analysis" => {
                let peaks: Vec<String> = (0..600)
                    .map(|i| {
                        let x = f64::from(i);
                        let envelope = (x / 600.0 * std::f64::consts::PI).sin();
                        let beat = 0.6 + 0.4 * (x / 3.0).sin().abs();
                        ((envelope * beat * 255.0) as u8).to_string()
                    })
                    .collect();
                (
                    json,
                    format!(
                        r#"{{"id":"so-1","waveform":{{"bins":600,"peaks":[{}]}}}}"#,
                        peaks.join(",")
                    ),
                )
            }
            _ => return None,
        })
    }

    /// The `scrobble` calls the fake server got, as request targets.
    static SCROBBLES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    /// Answers the Subsonic calls above, the cover from `COVER` and the
    /// audio from `TRACK` with ranges.
    fn fake_server(
        rt: &tokio::runtime::Runtime,
        audio: Arc<Vec<u8>>,
        cover: Arc<Vec<u8>>,
    ) -> ServerUrl {
        let listener = rt
            .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let count = Arc::new(AtomicU64::new(0));
        rt.spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (audio, cover) = (Arc::clone(&audio), Arc::clone(&cover));
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    let target = head.split(' ').nth(1).unwrap_or("/").to_owned();
                    let path = target.split('?').next().unwrap_or("/").to_owned();
                    if path == "/rest/scrobble.view" {
                        SCROBBLES.lock().expect("scrobbles").push(target.clone());
                    }
                    let (status, kind, body): (&str, &str, Vec<u8>) = if path == "/rest/stream.view"
                        || path == "/rest/download.view"
                    {
                        let start = head
                            .to_lowercase()
                            .lines()
                            .find_map(|l| l.strip_prefix("range: bytes=").map(str::to_owned))
                            .and_then(|r| r.split('-').next().and_then(|s| s.parse::<usize>().ok()));
                        let len = audio.len();
                        let first = start.unwrap_or(0);
                        let extra = format!("Content-Range: bytes {first}-{}/{len}\r\n", len - 1);
                        let status = if start.is_some() { "206 Partial Content" } else { "200 OK" };
                        let answer = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: audio/ogg\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
                            len - first,
                            if start.is_some() { extra } else { String::new() }
                        );
                        if socket.write_all(answer.as_bytes()).await.is_ok() {
                            let _ = socket.write_all(&audio[first..]).await;
                        }
                        return;
                    } else if path == "/rest/getCoverArt.view" {
                        ("200 OK", "image/png", cover.to_vec())
                    } else {
                        match answer(&path) {
                            Some((kind, body)) => ("200 OK", kind, body.into_bytes()),
                            None => ("404 Not Found", "text/plain", b"no".to_vec()),
                        }
                    };
                    let answer = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if socket.write_all(answer.as_bytes()).await.is_ok() {
                        let _ = socket.write_all(&body).await;
                    }
                });
            }
        });
        ServerUrl::parse(
            &format!("http://127.0.0.1:{port}"),
            HttpPolicy::ALLOW_PRIVATE_HTTP,
        )
        .expect("url")
    }

    fn save(widget: &gtk::Widget, path: &std::path::Path) {
        let paintable = gtk::WidgetPaintable::new(Some(widget));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(widget.width()),
            f64::from(widget.height()),
        );
        // The window is on the user's display; while it is not drawn (for
        // example on another workspace) there is no picture to save.
        let Some(node) = snapshot.to_node() else {
            eprintln!("SHOT skipped, nothing drawn: {}", path.display());
            return;
        };
        let renderer = widget
            .native()
            .and_then(|n| n.renderer())
            .expect("renderer");
        let texture = renderer.render_texture(&node, None);
        texture.save_to_png(path).expect("png");
        eprintln!("SHOT {}", path.display());
    }

    const MPRIS: [&str; 6] = [
        "--session",
        "--dest",
        "org.mpris.MediaPlayer2.DanceSonicStudio",
        "--object-path",
        "/org/mpris/MediaPlayer2",
        "--method",
    ];

    fn gdbus(args: &[&str]) -> String {
        let out = std::process::Command::new("gdbus")
            .arg("call")
            .args(MPRIS)
            .args(args)
            .output()
            .expect("gdbus");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn mpris_get(property: &str) -> String {
        gdbus(&[
            "org.freedesktop.DBus.Properties.Get",
            "org.mpris.MediaPlayer2.Player",
            property,
        ])
    }

    fn mpris_set(property: &str, value: &str) {
        let value = format!("<{value}>");
        gdbus(&[
            "org.freedesktop.DBus.Properties.Set",
            "org.mpris.MediaPlayer2.Player",
            property,
            &value,
        ]);
    }

    /// Calls a method of the Player interface. gdbus would block the main
    /// loop that answers it only for the window; the server runs on Tokio.
    fn mpris_call(method: &str, args: &[&str]) {
        let method = format!("org.mpris.MediaPlayer2.Player.{method}");
        let mut all = vec![method.as_str()];
        all.extend_from_slice(args);
        gdbus(&all);
    }

    fn find_button(widget: &gtk::Widget, label: &str) -> Option<gtk::Button> {
        if let Some(button) = widget.downcast_ref::<gtk::Button>()
            && button.label().as_deref() == Some(label)
        {
            return Some(button.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            if let Some(found) = find_button(&c, label) {
                return Some(found);
            }
            child = c.next_sibling();
        }
        None
    }

    fn find_picture(widget: &gtk::Widget) -> Option<gtk::Picture> {
        if let Some(picture) = widget.downcast_ref::<gtk::Picture>() {
            return Some(picture.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            if let Some(found) = find_picture(&c) {
                return Some(found);
            }
            child = c.next_sibling();
        }
        None
    }

    fn pump(millis: u64) {
        let context = glib::MainContext::default();
        let end = std::time::Instant::now() + Duration::from_millis(millis);
        while std::time::Instant::now() < end {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "local: needs a display, the GStreamer plugins, TRACK, COVER and SHOTS"]
    fn the_window_browses_plays_and_shows_the_waveform() {
        let audio = Arc::new(std::fs::read(std::env::var("TRACK").expect("TRACK")).expect("TRACK"));
        let cover = Arc::new(std::fs::read(std::env::var("COVER").expect("COVER")).expect("COVER"));
        let shots = std::path::PathBuf::from(std::env::var("SHOTS").expect("SHOTS"));
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime"),
        );
        let url = fake_server(&runtime, audio, cover);
        let dir = shots.join("data");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Arc::new(Database::open(dir.join("library.db")).expect("db"));
        let vault = Arc::new(SecretVault::<SecretService>::session_only("ui check"));
        let settings = HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        };
        runtime
            .block_on(studio_core::connect::connect_with_api_key(
                &settings,
                &vault,
                Arc::clone(&db),
                url,
                ApiKey::new("tsk_ui_check").expect("key"),
            ))
            .expect("connect");
        // Fresh dances, so the window's sync does not ask DanceBrainz.
        let tag = |tag_id, name: &str| DanceTag {
            tag_id,
            kind: "dance".into(),
            name: name.into(),
            path: vec![name.into()],
            qualifier: None,
            updated_at: "2026-11-20".into(),
        };
        let answers = [
            RecordingDances {
                mbid: Mbid::parse(MBID_A).expect("mbid"),
                tags: vec![tag(41, "An dro")],
            },
            RecordingDances {
                mbid: Mbid::parse(MBID_B).expect("mbid"),
                tags: vec![tag(12, "Mazurka")],
            },
        ];
        let now = studio_core::sync::now_ms();
        db.write(|tx| studio_core::dancebrainz::store(tx, &answers, now))
            .expect("dances");

        // A music folder (#10): Mazurka (the recording of so-3) on disk, and
        // a track the server does not have.
        let music = shots.join("music");
        let _ = std::fs::remove_dir_all(&music);
        std::fs::create_dir_all(&music).expect("music");
        let source = std::env::var("TRACK").expect("TRACK");
        tag_copy(
            &source,
            &music.join("mazurka.opus"),
            "Mazurka",
            "x",
            Some(MBID_B),
        );
        tag_copy(
            &source,
            &music.join("only.opus"),
            "Only here",
            "Zéphyr",
            None,
        );
        let folder = studio_core::local_library::add_folder(&db, &music, now).expect("folder");
        studio_core::local_library::scan_folder(&db, &folder, now).expect("scan");
        // Offline copies (#17) go to the scratch folder, not the user's data.
        let kept = shots.join("kept");
        let _ = std::fs::remove_dir_all(&kept);
        std::fs::create_dir_all(&kept).expect("kept");
        studio_core::offline::set_settings(
            &db,
            &kept,
            &studio_core::offline::Settings {
                folder: Some(kept.clone()),
                limit_bytes: 1_000_000_000,
            },
        )
        .expect("offline settings");

        gtk::init().expect("gtk");
        let app = gtk::Application::builder()
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).expect("register");
        let window = Window::new(&app, &runtime);
        window.window.present();
        window.set_services(Services { vault, db });
        pump(2500);

        // Library: open "B", then Blowzabella, then select the release.
        let rows = window.sidebar.selections[0].clone();
        let row = |i| {
            rows.item(i)
                .and_downcast::<gtk::TreeListRow>()
                .expect("row")
        };
        assert_eq!(node_of(&row(0)).expect("node").title, "B");
        row(0).set_expanded(true);
        row(1).set_expanded(true);
        rows.set_selected(2);
        pump(800);
        assert_eq!(window.tracks.sorted.n_items(), 2, "songs of Pingha Frenzy");
        // A sync keeps open entries open.
        window.sync();
        pump(1500);
        assert!(
            row(0).is_expanded() && row(1).is_expanded(),
            "B and Blowzabella stay open"
        );
        window.tracks.selection.set_selected(0);
        pump(800);
        save(window.window.upcast_ref(), &shots.join("1-library.png"));

        window.play_list(0);
        pump(3000);
        assert_eq!(window.now_playing.get(), Some(0));
        // The server hears what plays now (#31); it does not count yet.
        let scrobbles = SCROBBLES.lock().expect("scrobbles").clone();
        assert!(
            scrobbles
                .iter()
                .any(|t| t.contains("id=so-1") && t.contains("submission=false")),
            "{scrobbles:?}"
        );
        assert!(
            !scrobbles.iter().any(|t| t.contains("submission=true")),
            "{scrobbles:?}"
        );
        assert!(window.bar.seek.widget.visible_child_name().as_deref() == Some("wave"));
        window.bar.bpm_mode.set_active(true);
        window.bar.speed.set_value(140.0);
        pump(1500);
        let rate = window.rate.get().factor();
        assert!((rate - 140.0 / 132.0).abs() < 0.01, "rate {rate}");
        save(window.window.upcast_ref(), &shots.join("2-playing-bpm.png"));

        // Media controls (#7) over the real session bus.
        let status = mpris_get("PlaybackStatus");
        assert!(status.contains("Playing"), "{status}");
        let meta = mpris_get("Metadata");
        assert!(meta.contains("An dro Pleiben"), "{meta}");
        assert!(
            meta.contains("mpris:artUrl") && meta.contains("file://"),
            "{meta}"
        );
        assert!(meta.contains("/org/dancesonic/Studio/track/0"), "{meta}");
        mpris_call("PlayPause", &[]);
        pump(800);
        assert!(!window.playing.get(), "PlayPause pauses");
        assert!(mpris_get("PlaybackStatus").contains("Paused"));
        mpris_call("Play", &[]);
        pump(800);
        assert!(window.playing.get(), "Play resumes");
        mpris_set("Rate", "1.25");
        pump(800);
        assert!(
            (window.rate.get().factor() - 1.25).abs() < 1e-9,
            "Rate from MPRIS"
        );
        mpris_set("Volume", "0.5");
        pump(500);
        assert!(mpris_get("Volume").contains("0.5"));
        mpris_call(
            "SetPosition",
            &["/org/dancesonic/Studio/track/0", "60000000"],
        );
        pump(1200);
        let at = window
            .player
            .borrow()
            .as_ref()
            .and_then(Player::position_ms);
        assert!(
            at.is_some_and(|(p, _)| (60_000..63_000).contains(&p)),
            "{at:?}"
        );
        window.bar.bpm_mode.set_active(true);
        window.bar.speed.set_value(140.0);
        pump(500);

        // Dances view and its songs.
        let stack = window
            .sidebar
            .widget
            .observe_children()
            .item(1)
            .and_downcast::<gtk::Stack>()
            .expect("stack");
        stack.set_visible_child_name("dances");
        let dances = window.sidebar.selections[1].clone();
        let first = dances
            .item(0)
            .and_downcast::<gtk::TreeListRow>()
            .expect("row");
        first.set_expanded(true);
        dances.set_selected(1);
        pump(1000);
        assert_eq!(
            window.tracks.sorted.n_items(),
            1,
            "songs of the first dance"
        );
        save(window.window.upcast_ref(), &shots.join("3-dances.png"));

        // A song without BPM: BPM mode falls back to Tempo. The speed carries
        // over to the new track (a regression: the seek that set it was sent
        // before the pipeline was ready, refused, and the track never started).
        window.set_rate(1.5);
        window.next_without_bpm();
        pump(2500);
        let before = window
            .player
            .borrow()
            .as_ref()
            .and_then(Player::position_ms);
        pump(2000);
        let after = window
            .player
            .borrow()
            .as_ref()
            .and_then(Player::position_ms);
        let (Some((before, _)), Some((after, _))) = (before, after) else {
            panic!("no position");
        };
        let speed = (after - before) as f64 / 2000.0;
        assert!((speed - 1.5).abs() < 0.15, "speed {speed}");
        assert!(window.bar.tempo_mode.is_active());
        assert!(!window.bar.bpm_mode.is_sensitive());
        save(window.window.upcast_ref(), &shots.join("4-no-bpm.png"));

        // Local folders (#10): the same recording is listed once, as both.
        let session = window.session.borrow().clone().expect("session");
        let songs = runtime
            .block_on(session.songs(Selection::Album("al-2".into())))
            .expect("songs");
        let mazurka = songs.iter().find(|s| s.id == "so-3").expect("so-3");
        assert_eq!(location(mazurka), "Both");
        assert!(songs.iter().all(|s| s.on_server), "no second copy");
        let letters: Vec<String> = (0..window.sidebar.library.n_items())
            .filter_map(|i| {
                window
                    .sidebar
                    .library
                    .item(i)
                    .and_downcast::<BoxedAnyObject>()
            })
            .map(|o| o.borrow::<Rc<Node>>().title.clone())
            .collect();
        assert!(letters.contains(&"Z".to_owned()), "{letters:?}");
        window.show_songs("Zéphyr".into(), Selection::LocalArtist("Zéphyr".into()));
        pump(800);
        assert_eq!(window.tracks.sorted.n_items(), 1);
        window.play_list(0);
        pump(2500);
        assert_eq!(window.now_playing.get(), Some(0));
        let local = window.current_song().expect("song");
        assert_eq!(location(&local), "Local");
        let at = window
            .player
            .borrow()
            .as_ref()
            .and_then(Player::position_ms);
        assert!(at.is_some_and(|(p, _)| p > 500), "plays from disk: {at:?}");
        let scrobbles = SCROBBLES.lock().expect("scrobbles").clone();
        assert!(
            !scrobbles.iter().any(|t| t.contains("local")),
            "a local-only track is not reported: {scrobbles:?}"
        );
        save(window.window.upcast_ref(), &shots.join("5-local.png"));

        // Keep an album offline (#17): Scottish is downloaded; Mazurka
        // already plays from the music folder.
        window.show_songs("Bal folk".into(), Selection::Album("al-2".into()));
        pump(800);
        assert!(window.tracks.keep.is_visible() && !window.tracks.keep.is_active());
        window.tracks.keep.set_active(true);
        pump(4000);
        let shown: Vec<(String, &str)> = (0..window.tracks.sorted.n_items())
            .filter_map(|i| {
                window
                    .tracks
                    .sorted
                    .item(i)
                    .and_downcast::<BoxedAnyObject>()
            })
            .map(|o| {
                let song = o.borrow::<Song>();
                (song.id.clone(), location(&song))
            })
            .collect();
        assert!(shown.contains(&("so-4".to_owned(), "Offline")), "{shown:?}");
        assert!(shown.contains(&("so-3".to_owned(), "Both")), "{shown:?}");
        // Both songs of the album are kept, also the one in a music folder,
        // which may be on a drive that is not always there.
        let stored: Vec<String> = std::fs::read_dir(kept.join(studio_core::offline::FOLDER_NAME))
            .expect("offline folder")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        assert_eq!(stored.len(), 2, "{stored:?}");
        assert!(
            stored
                .iter()
                .all(|n| n.len() == 70 && n.ends_with(".audio")),
            "named by a hash: {stored:?}"
        );
        save(window.window.upcast_ref(), &shots.join("6-offline.png"));

        // Remote control (#20): off until turned on, then a pairing code,
        // and a phone's commands carried out by the window.
        let remote = window.remote.borrow().clone().expect("remote control");
        assert!(!remote.settings().network, "off by default");
        let mut on = remote.settings();
        on.network = true;
        remote.set(on);
        pump(2000);
        assert!(
            remote.status().contains("can find Studio"),
            "{}",
            remote.status()
        );
        window.show_remote();
        pump(300);
        let dialog = gtk::Window::list_toplevels()
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Window>().ok())
            .find(|w| w.title().as_deref() == Some("Remote control"))
            .expect("the remote control dialog");
        find_button(dialog.upcast_ref(), "Pair a phone…")
            .expect("pair button")
            .emit_clicked();
        pump(300);
        let code = find_picture(dialog.upcast_ref()).expect("QR code");
        assert!(code.paintable().is_some() && code.is_visible());
        save(dialog.upcast_ref(), &shots.join("7-remote.png"));
        dialog.close();
        pump(200);

        use studio_core::remote::messages::Command as Remote;
        let ask = |command: Remote| {
            let (tx, mut rx) = tokio::sync::oneshot::channel();
            window.on_remote_command(command, tx);
            pump(1500);
            rx.try_recv().expect("an answer")
        };
        let ids =
            || -> Vec<String> { window.queue.borrow().iter().map(|s| s.id.clone()).collect() };
        assert_eq!(
            ask(Remote::PlaySongs {
                songs: vec!["so-2".into(), "so-1".into()],
                start: 1
            }),
            Ok(None)
        );
        assert_eq!(ids(), ["so-2", "so-1"]);
        assert_eq!(window.now_playing.get(), Some(1));
        assert_eq!(
            ask(Remote::Enqueue {
                songs: vec!["so-4".into()]
            }),
            Ok(None)
        );
        assert_eq!(ids(), ["so-2", "so-1", "so-4"], "appended");
        assert_eq!(window.now_playing.get(), Some(1), "without a restart");
        assert_eq!(
            ask(Remote::PlaySongs {
                songs: vec!["no-such-song".into()],
                start: 0
            }),
            Err("unknown_song")
        );
        assert_eq!(ask(Remote::SetRate { rate: 1.2 }), Ok(None));
        assert!((window.rate.get().factor() - 1.2).abs() < 1e-9);
        assert_eq!(ask(Remote::Pause), Ok(None));
        assert!(!window.playing.get());
        let published = window.published.borrow().0.clone().expect("state sent");
        assert_eq!(published.song.as_deref(), Some("so-1"));
        assert!(!published.playing);
        assert_eq!(published.queue.len(), 3);
        let artists = ask(Remote::LibraryArtists).expect("artists").expect("data");
        assert!(
            artists.as_array().is_some_and(|a| !a.is_empty()),
            "{artists}"
        );

        // Bluetooth (#24) on this computer's adapter.
        let mut both = remote.settings();
        both.bluetooth = true;
        remote.set(both);
        pump(3000);
        assert!(
            remote.status().contains("over Bluetooth"),
            "{}",
            remote.status()
        );
        let mut off = remote.settings();
        off.network = false;
        off.bluetooth = false;
        remote.set(off);
        pump(500);
        assert_eq!(remote.status(), "Remote control is off.");
        window.window.destroy();
        pump(300);
    }

    /// Copies an audio file and gives it a title, artist and recording MBID.
    fn tag_copy(
        source: &str,
        target: &std::path::Path,
        title: &str,
        artist: &str,
        mbid: Option<&str>,
    ) {
        use lofty::config::WriteOptions;
        use lofty::file::TaggedFileExt;
        use lofty::tag::{Accessor, ItemKey, TagExt};
        std::fs::copy(source, target).expect("copy");
        let file = lofty::read_from_path(target).expect("read");
        let mut tag = file
            .primary_tag()
            .cloned()
            .unwrap_or_else(|| lofty::tag::Tag::new(file.primary_tag_type()));
        tag.set_title(title.into());
        tag.set_artist(artist.into());
        tag.set_album("Démo".into());
        tag.remove_key(ItemKey::AlbumArtist);
        if let Some(mbid) = mbid {
            tag.insert_text(ItemKey::MusicBrainzRecordingId, mbid.into());
        }
        tag.save_to_path(target, WriteOptions::default())
            .expect("tag");
    }

    impl Window {
        /// Skips to the next song of the queue (Bourrée, without BPM).
        fn next_without_bpm(&self) {
            self.with_player(|p| p.next());
        }
    }
}
