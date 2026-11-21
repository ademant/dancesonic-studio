//! Remote control by the DanceSonic phone app (#20, #24, #38): the switches,
//! Studio's identity, the hub and its transports, and the user's decisions.
//! The protocol itself is in `studio_core::remote`.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use secrecy::SecretString;
use studio_core::db::Database;
use studio_core::http::HttpSettings;
use studio_core::remote::devices::{self, Device, DeviceId};
use studio_core::remote::hub::{Hub, HubEvent, Reply, SystemClock};
use studio_core::remote::identity;
use studio_core::remote::messages::{Command, PlayState, Transport};
use studio_core::remote::network::{Announcer, MdnsAnnouncer, NetworkRemote, SystemAddresses};
use studio_core::remote::relay::{RelayError, RelayRemote, RelayStatus};
use studio_core::remote::settings::{self, Settings};
use studio_core::secrets::{Persistence, SecretVault};
use studio_core::subsonic::SubsonicClient;
use tokio::sync::{mpsc, oneshot};

use crate::ble_remote::BleRemote;
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

/// Work that must finish before the caller goes on (withdrawing the relay
/// record before sign-out deletes the key).
pub(crate) type Pending = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The server the relay goes through: the signed-in session's client.
#[derive(Clone)]
pub(crate) struct RelayServer {
    pub(crate) client: SubsonicClient,
    pub(crate) http: HttpSettings,
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
    ble: RefCell<Option<BleRemote>>,
    /// The signed-in server, if any; the relay needs one.
    server: RefCell<Option<RelayServer>>,
    relay: RefCell<Option<RelayRemote>>,
    /// Bumped when the relay is stopped, so a start that finishes later is
    /// thrown away.
    relay_generation: Cell<u64>,
    sessions: RefCell<Vec<Connected>>,
    /// What the network transport, Bluetooth and the keyring say.
    network_status: RefCell<String>,
    ble_status: RefCell<String>,
    relay_status: RefCell<String>,
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
                relay: false,
                port: studio_core::remote::network::DEFAULT_PORT,
            }),
            hub: RefCell::new(None),
            network: RefCell::new(None),
            ble: RefCell::new(None),
            server: RefCell::new(None),
            relay: RefCell::new(None),
            relay_generation: Cell::new(0),
            sessions: RefCell::new(Vec::new()),
            network_status: RefCell::new(String::new()),
            ble_status: RefCell::new(String::new()),
            relay_status: RefCell::new(String::new()),
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
            self.network_status.borrow_mut().clear();
        }
        if !settings.bluetooth {
            // Stops the service and the advertisement.
            self.ble.borrow_mut().take();
            self.ble_status.borrow_mut().clear();
        }
        if !settings.relay || self.server.borrow().is_none() {
            // Withdraws the record in the background.
            if let Some(pending) = self.stop_relay() {
                self.runtime.spawn(pending);
            }
        }
        if !settings.network && !settings.bluetooth && !settings.relay {
            self.notify();
            return;
        }
        let Some(hub) = self.hub().await else { return };
        if settings.relay && self.relay.borrow().is_none() {
            let server = self.server.borrow().clone();
            match server {
                Some(server) => self.start_relay(&hub, server).await,
                None => {
                    *self.relay_status.borrow_mut() =
                        "Remote control through folksonic starts when Studio is signed in to a server."
                            .to_owned();
                }
            }
        }
        if settings.network && self.network.borrow().is_none() {
            self.start_network(&hub, settings.port).await;
        }
        if settings.bluetooth && self.ble.borrow().is_none() {
            self.start_ble(&hub).await;
        }
        self.notify();
    }

    async fn start_network(&self, hub: &Arc<Hub>, port: u16) {
        let task_hub = Arc::clone(hub);
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
                NetworkRemote::start(task_hub, port, Box::new(SystemAddresses), announcer).await
            })
            .await;
        let text = match started {
            Ok(Ok(network)) => {
                // The switch may have been turned off meanwhile.
                if !self.settings.get().network || self.network.borrow().is_some() {
                    return;
                }
                let port = network.port();
                *self.network.borrow_mut() = Some(network);
                format!("Phones on this network can find Studio (port {port}).")
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "remote control cannot listen");
                format!("The network remote cannot start: {error}")
            }
            Err(_) => return,
        };
        *self.network_status.borrow_mut() = text;
    }

    async fn start_ble(&self, hub: &Arc<Hub>) {
        let task_hub = Arc::clone(hub);
        let started = self
            .runtime
            .spawn(async move { crate::ble_remote::start(task_hub).await })
            .await;
        let text = match started {
            Ok(Ok(ble)) => {
                if !self.settings.get().bluetooth || self.ble.borrow().is_some() {
                    return;
                }
                *self.ble.borrow_mut() = Some(ble);
                "Phones nearby can find Studio over Bluetooth.".to_owned()
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "no Bluetooth remote");
                format!("The Bluetooth remote cannot start: {error}.")
            }
            Err(_) => return,
        };
        *self.ble_status.borrow_mut() = text;
    }

    async fn start_relay(self: &Rc<Self>, hub: &Arc<Hub>, server: RelayServer) {
        let generation = self.relay_generation.get();
        let task_hub = Arc::clone(hub);
        let started = self
            .runtime
            .spawn(async move { RelayRemote::start(task_hub, server.client, server.http).await })
            .await;
        let text = match started {
            Ok(Ok(relay)) => {
                let stale = generation != self.relay_generation.get()
                    || !self.settings.get().relay
                    || self.relay.borrow().is_some();
                if stale {
                    // Withdraw what this late start published.
                    self.runtime.spawn(async move {
                        if let Err(error) = relay.stop().await {
                            tracing::warn!(%error, "relay record not withdrawn");
                        }
                    });
                    return;
                }
                self.watch_relay(&relay);
                *self.relay.borrow_mut() = Some(relay);
                "Phones signed in to the same folksonic account can reach Studio through the server."
                    .to_owned()
            }
            Ok(Err(RelayError::NotOffered)) => {
                "This server does not offer remote control through folksonic.".to_owned()
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "no relay remote");
                format!("Remote control through folksonic cannot start: {error}.")
            }
            Err(_) => return,
        };
        *self.relay_status.borrow_mut() = text;
    }

    /// Mirrors the relay connection's state in the status line.
    fn watch_relay(self: &Rc<Self>, relay: &RelayRemote) {
        let mut status = relay.status();
        let (tx, mut rx) = mpsc::channel::<RelayStatus>(4);
        self.runtime.spawn(async move {
            while status.changed().await.is_ok() {
                let now = status.borrow_and_update().clone();
                if tx.send(now).await.is_err() {
                    break;
                }
            }
        });
        let generation = self.relay_generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Some(now) = rx.recv().await {
                let Some(me) = weak.upgrade() else { break };
                if me.relay_generation.get() != generation {
                    break;
                }
                let text = match now {
                    RelayStatus::Connected => "Phones signed in to the same folksonic account can reach Studio through the server.".to_owned(),
                    RelayStatus::Connecting => "Connecting to the folksonic relay…".to_owned(),
                    RelayStatus::Retrying { reason } => {
                        format!("The folksonic relay is not reachable ({reason}); trying again.")
                    }
                };
                *me.relay_status.borrow_mut() = text;
                me.notify();
            }
        });
    }

    /// Stops the relay. The returned work withdraws Studio's record from
    /// folksonic; `None` when the relay was not running.
    fn stop_relay(&self) -> Option<Pending> {
        self.relay_generation.set(self.relay_generation.get() + 1);
        self.relay_status.borrow_mut().clear();
        let relay = self.relay.borrow_mut().take()?;
        Some(Box::pin(async move {
            if let Err(error) = relay.stop().await {
                tracing::warn!(%error, "relay record not withdrawn");
            }
        }))
    }

    /// The signed-in server changed: the relay follows it.
    pub(crate) fn set_server(self: &Rc<Self>, server: Option<RelayServer>) {
        if let Some(pending) = self.stop_relay() {
            self.runtime.spawn(pending);
        }
        *self.server.borrow_mut() = server;
        let me = Rc::clone(self);
        glib::spawn_future_local(async move { me.apply().await });
    }

    /// Signing out of the server with `key`: the relay stops, and the
    /// returned work withdraws Studio's record while the key still works.
    pub(crate) fn sign_out(&self, key: &str) -> Option<Pending> {
        let matches = self
            .server
            .borrow()
            .as_ref()
            .is_some_and(|s| s.client.server().key() == key);
        if !matches {
            return None;
        }
        self.server.borrow_mut().take();
        let pending = self.stop_relay();
        self.notify();
        pending
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
                comparison,
                decision,
            } => self.ask(&name, transport, comparison.as_deref(), decision),
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
    fn ask(
        &self,
        name: &str,
        transport: Transport,
        comparison: Option<&str>,
        decision: oneshot::Sender<bool>,
    ) {
        let detail = match comparison {
            // Pairing through folksonic: no QR code, so the user compares
            // the codes on both screens instead.
            Some(code) => format!(
                "A phone signed in to your folksonic account asks through the server.\n\n\
                 Code: {}\n\n\
                 Allow it only if your phone shows the same code. You can revoke it later.",
                spaced(code)
            ),
            None => {
                let over = transport_phrase(transport);
                format!(
                    "A phone scanned Studio's pairing code and asks {over}. \
                     Allow it only if it is your phone. You can revoke it later."
                )
            }
        };
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Allow “{name}” to control Studio?"))
            .detail(detail)
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

    /// What the dialog says about the transports.
    pub(crate) fn status(&self) -> String {
        let settings = self.settings.get();
        if !settings.network && !settings.bluetooth && !settings.relay {
            return "Remote control is off.".to_owned();
        }
        let mut parts: Vec<String> = [&self.ble_status, &self.relay_status, &self.network_status]
            .iter()
            .map(|s| s.borrow().clone())
            .filter(|s| !s.is_empty())
            .collect();
        if let Persistence::SessionOnly { .. } = self.vault.persistence() {
            parts.push(
                "No keyring: Studio's remote control key is kept for this session only, \
                 so phones must pair again after a restart."
                    .to_owned(),
            );
        }
        parts.join(" ")
    }

    fn set_status(&self, text: &str) {
        *self.network_status.borrow_mut() = text.to_owned();
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

/// How a transport reads after "asks".
pub(crate) fn transport_phrase(transport: Transport) -> &'static str {
    match transport {
        Transport::Ble => "over Bluetooth",
        Transport::Network => "on the local network",
        Transport::Relay => "through folksonic",
    }
}

/// "123456" as "123 456", easier to compare at a glance.
fn spaced(code: &str) -> String {
    if code.len() == 6 && code.is_ascii() {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_owned()
    }
}

/// Studio's name towards phones.
fn studio_name() -> String {
    let host = glib::host_name();
    let name = format!("Studio on {host}");
    name.chars().take(64).collect()
}
