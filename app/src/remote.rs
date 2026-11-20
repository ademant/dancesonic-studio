//! Remote control by the DanceSonic phone app (#20, #24): the switches,
//! Studio's identity, the hub and its transports, and the user's decisions.
//! The protocol itself is in `studio_core::remote`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use secrecy::SecretString;
use studio_core::db::Database;
use studio_core::remote::devices::{self, Device, DeviceId};
use studio_core::remote::hub::{Hub, HubEvent, Reply, SystemClock};
use studio_core::remote::identity;
use studio_core::remote::messages::{Command, PlayState, Transport};
use studio_core::remote::network::{Announcer, MdnsAnnouncer, NetworkRemote, SystemAddresses};
use studio_core::remote::settings::{self, Settings};
use studio_core::secrets::{Persistence, SecretVault};
use tokio::sync::{mpsc, oneshot};

use crate::secret_service::SecretService;

/// Called when the status, the sessions or the phones change.
pub(crate) type Listener = Rc<dyn Fn()>;

/// What the window does with an authenticated command.
pub(crate) type OnCommand = Rc<dyn Fn(Command, oneshot::Sender<Reply>)>;

/// An open session, for the dialog and the player bar.
#[derive(Clone, Debug)]
pub(crate) struct Connected {
    pub(crate) device: DeviceId,
    pub(crate) name: String,
    pub(crate) transport: Transport,
}

/// Announces nothing: used when mDNS cannot start. Phones then find Studio
/// through the Bluetooth network hint only.
struct Silent;

impl Announcer for Silent {
    fn announce(&mut self, _: &[std::net::IpAddr], _: u16) {}
    fn withdraw(&mut self) {}
}

pub(crate) struct Remote {
    runtime: Arc<tokio::runtime::Runtime>,
    db: Arc<Database>,
    vault: Arc<SecretVault<SecretService>>,
    parent: gtk::ApplicationWindow,
    on_command: OnCommand,
    settings: Cell<Settings>,
    hub: RefCell<Option<Arc<Hub>>>,
    network: RefCell<Option<NetworkRemote>>,
    sessions: RefCell<Vec<Connected>>,
    status: RefCell<String>,
    listeners: RefCell<Vec<(u64, Listener)>>,
    next_listener: Cell<u64>,
}

impl Remote {
    pub(crate) fn new(
        runtime: Arc<tokio::runtime::Runtime>,
        db: Arc<Database>,
        vault: Arc<SecretVault<SecretService>>,
        parent: gtk::ApplicationWindow,
        on_command: OnCommand,
    ) -> Rc<Self> {
        Rc::new(Self {
            runtime,
            db,
            vault,
            parent,
            on_command,
            settings: Cell::new(Settings {
                network: false,
                bluetooth: false,
                port: studio_core::remote::network::DEFAULT_PORT,
            }),
            hub: RefCell::new(None),
            network: RefCell::new(None),
            sessions: RefCell::new(Vec::new()),
            status: RefCell::new("Remote control is off.".to_owned()),
            listeners: RefCell::new(Vec::new()),
            next_listener: Cell::new(0),
        })
    }

    /// Reads the switches and starts what they allow.
    pub(crate) fn start(self: &Rc<Self>) {
        let me = Rc::clone(self);
        glib::spawn_future_local(async move {
            let db = Arc::clone(&me.db);
            let loaded = me
                .runtime
                .spawn_blocking(move || settings::load(&db.reader()?))
                .await;
            match loaded {
                Ok(Ok(settings)) => {
                    me.settings.set(settings);
                    me.apply().await;
                }
                Ok(Err(error)) => tracing::warn!(%error, "remote control settings unreadable"),
                Err(_) => {}
            }
        });
    }

    pub(crate) fn settings(&self) -> Settings {
        self.settings.get()
    }

    /// Turns a transport on or off and stores the choice.
    pub(crate) fn set(self: &Rc<Self>, settings: Settings) {
        self.settings.set(settings);
        let me = Rc::clone(self);
        glib::spawn_future_local(async move {
            let db = Arc::clone(&me.db);
            let saved = me
                .runtime
                .spawn_blocking(move || settings::save(&db, settings))
                .await;
            if !matches!(saved, Ok(Ok(()))) {
                tracing::warn!("remote control settings not saved");
            }
            me.apply().await;
        });
    }

    /// Brings the hub and the transports in line with the switches.
    async fn apply(self: &Rc<Self>) {
        let settings = self.settings.get();
        if !settings.network {
            // Stops the server and withdraws the announcement.
            self.network.borrow_mut().take();
        }
        if !settings.network && !settings.bluetooth {
            self.set_status("Remote control is off.");
            return;
        }
        let Some(hub) = self.hub().await else { return };
        if settings.network && self.network.borrow().is_none() {
            let task_hub = Arc::clone(&hub);
            let started = self
                .runtime
                .spawn(async move {
                    let announcer: Box<dyn Announcer> =
                        match MdnsAnnouncer::new(task_hub.identity(), task_hub.name()) {
                            Ok(mdns) => Box::new(mdns),
                            Err(error) => {
                                tracing::warn!(%error, "mDNS is not available");
                                Box::new(Silent)
                            }
                        };
                    NetworkRemote::start(
                        task_hub,
                        settings.port,
                        Box::new(SystemAddresses),
                        announcer,
                    )
                    .await
                })
                .await;
            match started {
                Ok(Ok(network)) => {
                    // The switch may have been turned off meanwhile.
                    if self.settings.get().network && self.network.borrow().is_none() {
                        let port = network.port();
                        *self.network.borrow_mut() = Some(network);
                        self.set_status(&format!(
                            "Phones on this network can find Studio (port {port})."
                        ));
                    }
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "remote control cannot listen");
                    self.set_status(&format!("Remote control cannot start: {error}"));
                }
                Err(_) => {}
            }
        }
        if let Persistence::SessionOnly { .. } = self.vault.persistence()
            && !self.status().contains("No keyring")
        {
            let status = format!(
                "{} No keyring: Studio's remote control key is kept for this session only, \
                 so phones must pair again after a restart.",
                self.status()
            );
            self.set_status(status.trim_start());
        }
    }

    /// The hub, made on first use with Studio's identity from the keyring.
    async fn hub(self: &Rc<Self>) -> Option<Arc<Hub>> {
        if let Some(hub) = self.hub.borrow().clone() {
            return Some(hub);
        }
        let vault = Arc::clone(&self.vault);
        let loaded = self
            .runtime
            .spawn(async move { identity::load_or_create(&vault).await })
            .await;
        let identity = match loaded {
            Ok(Ok(identity)) => identity,
            Ok(Err(error)) => {
                tracing::warn!(%error, "no remote control key");
                self.set_status(&format!("Remote control cannot start: {error}"));
                return None;
            }
            Err(_) => return None,
        };
        // Another call may have made it while this one waited.
        if let Some(hub) = self.hub.borrow().clone() {
            return Some(hub);
        }
        let (events, mut received) = mpsc::channel(16);
        let name = studio_name();
        let hub = Hub::new(
            Arc::clone(&self.db),
            Arc::new(identity),
            &name,
            Arc::new(SystemClock),
            events,
        );
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Some(event) = received.recv().await {
                let Some(me) = weak.upgrade() else { break };
                me.on_event(event);
            }
        });
        *self.hub.borrow_mut() = Some(Arc::clone(&hub));
        Some(hub)
    }

    fn on_event(self: &Rc<Self>, event: HubEvent) {
        match event {
            HubEvent::PairRequest {
                name,
                transport,
                decision,
            } => self.ask(&name, transport, decision),
            HubEvent::Paired { .. } => self.notify(),
            HubEvent::Command { command, reply, .. } => (self.on_command)(command, reply),
            HubEvent::Connected {
                device,
                name,
                transport,
            } => {
                self.sessions.borrow_mut().push(Connected {
                    device,
                    name,
                    transport,
                });
                self.notify();
            }
            HubEvent::Disconnected { device, transport } => {
                let mut sessions = self.sessions.borrow_mut();
                if let Some(i) = sessions
                    .iter()
                    .position(|s| s.device == device && s.transport == transport)
                {
                    sessions.remove(i);
                }
                drop(sessions);
                self.notify();
            }
        }
    }

    /// Asks the user whether a phone may pair.
    fn ask(&self, name: &str, transport: Transport, decision: oneshot::Sender<bool>) {
        let over = match transport {
            Transport::Ble => "over Bluetooth",
            Transport::Network => "on the local network",
        };
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Allow “{name}” to control Studio?"))
            .detail(format!(
                "A phone scanned Studio's pairing code and asks {over}. \
                 Allow it only if it is your phone. You can revoke it later."
            ))
            .buttons(["Reject", "Allow"])
            .cancel_button(0)
            .default_button(0)
            .build();
        dialog.choose(
            Some(&self.parent),
            gtk::gio::Cancellable::NONE,
            move |answer| {
                // The phone may have left meanwhile; then nobody waits.
                let _ = decision.send(matches!(answer, Ok(1)));
            },
        );
    }

    /// Shows a new pairing code: the QR text and when it expires (Unix s).
    pub(crate) fn new_offer(&self) -> Option<(SecretString, i64)> {
        let hub = self.hub.borrow().clone()?;
        match hub.new_offer() {
            Ok(offer) => Some(offer),
            Err(error) => {
                tracing::warn!(%error, "no pairing code");
                None
            }
        }
    }

    /// Withdraws the shown code.
    pub(crate) fn cancel_offer(&self) {
        if let Some(hub) = self.hub.borrow().as_ref() {
            hub.cancel_offer();
        }
    }

    /// Whether phones can pair now.
    pub(crate) fn can_pair(&self) -> bool {
        self.hub.borrow().is_some()
            && (self.settings.get().network || self.settings.get().bluetooth)
    }

    /// The paired phones.
    pub(crate) async fn devices(&self) -> Vec<Device> {
        let db = Arc::clone(&self.db);
        match self
            .runtime
            .spawn_blocking(move || devices::list(&db.reader()?))
            .await
        {
            Ok(Ok(list)) => list,
            _ => Vec::new(),
        }
    }

    /// Revokes a phone, or forgets it when `forget` is set. Open sessions end.
    pub(crate) async fn revoke(&self, id: DeviceId, forget: bool) {
        let hub = self.hub.borrow().clone();
        let db = Arc::clone(&self.db);
        let done = self
            .runtime
            .spawn_blocking(move || match (hub, forget) {
                (Some(hub), true) => hub.remove(&id),
                (Some(hub), false) => hub.revoke(&id),
                (None, true) => devices::remove(&db, &id),
                (None, false) => devices::revoke(&db, &id, studio_core::sync::now_ms()),
            })
            .await;
        if !matches!(done, Ok(Ok(_))) {
            tracing::warn!("a phone could not be revoked");
        }
        self.notify();
    }

    /// Tells the phones what plays.
    pub(crate) fn publish(&self, state: PlayState) {
        if let Some(hub) = self.hub.borrow().as_ref() {
            hub.publish_state(state);
        }
    }

    /// The open sessions, newest last.
    pub(crate) fn sessions(&self) -> Vec<Connected> {
        self.sessions.borrow().clone()
    }

    pub(crate) fn status(&self) -> String {
        self.status.borrow().clone()
    }

    fn set_status(&self, text: &str) {
        *self.status.borrow_mut() = text.to_owned();
        self.notify();
    }

    /// Calls `listener` whenever the status, the sessions or the phones change.
    pub(crate) fn listen(&self, listener: Rc<dyn Fn()>) -> u64 {
        let id = self.next_listener.get();
        self.next_listener.set(id + 1);
        self.listeners.borrow_mut().push((id, listener));
        id
    }

    pub(crate) fn unlisten(&self, id: u64) {
        self.listeners.borrow_mut().retain(|(i, _)| *i != id);
    }

    fn notify(&self) {
        let listeners: Vec<Rc<dyn Fn()>> = self
            .listeners
            .borrow()
            .iter()
            .map(|(_, l)| Rc::clone(l))
            .collect();
        for listener in listeners {
            listener();
        }
    }
}

/// Studio's name towards phones.
fn studio_name() -> String {
    let host = glib::host_name();
    let name = format!("Studio on {host}");
    name.chars().take(64).collect()
}
