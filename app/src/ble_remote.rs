//! The Bluetooth LE transport through BlueZ (#24). Only moves frames between
//! BlueZ's characteristic sockets and `studio_core::remote::ble::serve_link`,
//! which does the framing, the Noise session and the protocol.

use std::collections::HashMap;
use std::sync::Arc;

use bluer::adv::{Advertisement, AdvertisementHandle};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicControlEvent,
    CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicWrite,
    CharacteristicWriteMethod, Service, characteristic_control,
};
use bluer::gatt::{CharacteristicReader, CharacteristicWriter};
use bluer::{Address, Uuid};
use futures_util::StreamExt;
use studio_core::remote::ble::{self as core_ble, RX_CHARACTERISTIC, TX_CHARACTERISTIC};
use studio_core::remote::hub::Hub;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

/// Phones that subscribed or wrote but have not done both yet.
const MAX_HALF_OPEN: usize = 8;

/// Why the Bluetooth transport could not start. The text is for the user.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BleStartError {
    #[error("no Bluetooth adapter was found")]
    NoAdapter,
    #[error("Bluetooth is turned off")]
    PoweredOff,
    #[error("this Bluetooth adapter cannot be found by phones (no LE peripheral role)")]
    NoPeripheral,
    #[error("Bluetooth is not available: {0}")]
    Other(String),
}

/// The GATT service and its advertisement. Dropping it stops both.
pub(crate) struct BleRemote {
    _application: ApplicationHandle,
    _advertisement: AdvertisementHandle,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for BleRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BleRemote").finish_non_exhaustive()
    }
}

impl Drop for BleRemote {
    fn drop(&mut self) {
        // Ends every Bluetooth session; the handles unregister the service
        // and the advertisement.
        self.task.abort();
    }
}

fn uuid(text: &str) -> Result<Uuid, BleStartError> {
    Uuid::parse_str(text).map_err(|e| BleStartError::Other(e.to_string()))
}

/// Offers Studio's GATT service and advertises its UUID, and nothing else.
pub(crate) async fn start(hub: Arc<Hub>) -> Result<BleRemote, BleStartError> {
    let session = bluer::Session::new()
        .await
        .map_err(|e| BleStartError::Other(e.to_string()))?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(|_| BleStartError::NoAdapter)?;
    if !adapter.is_powered().await.unwrap_or(false) {
        return Err(BleStartError::PoweredOff);
    }
    let service = Uuid::from_bytes(*hub.identity().ble_service());
    let advertisement = Advertisement {
        service_uuids: [service].into_iter().collect(),
        discoverable: Some(true),
        ..Default::default()
    };
    let advertisement = adapter.advertise(advertisement).await.map_err(|error| {
        tracing::warn!(%error, "cannot advertise over Bluetooth");
        BleStartError::NoPeripheral
    })?;

    let (rx_control, rx_handle) = characteristic_control();
    let (tx_control, tx_handle) = characteristic_control();
    let application = Application {
        services: vec![Service {
            uuid: service,
            primary: true,
            characteristics: vec![
                Characteristic {
                    uuid: uuid(RX_CHARACTERISTIC)?,
                    write: Some(CharacteristicWrite {
                        write: true,
                        write_without_response: true,
                        method: CharacteristicWriteMethod::Io,
                        ..Default::default()
                    }),
                    control_handle: rx_handle,
                    ..Default::default()
                },
                Characteristic {
                    uuid: uuid(TX_CHARACTERISTIC)?,
                    notify: Some(CharacteristicNotify {
                        notify: true,
                        method: CharacteristicNotifyMethod::Io,
                        ..Default::default()
                    }),
                    control_handle: tx_handle,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    let application = adapter
        .serve_gatt_application(application)
        .await
        .map_err(|e| BleStartError::Other(e.to_string()))?;
    tracing::info!("remote control advertised over Bluetooth");

    let task = tokio::spawn(async move {
        // The session must live as long as the service.
        let _session = session;
        let mut rx_control = Box::pin(rx_control);
        let mut tx_control = Box::pin(tx_control);
        let mut half: HashMap<
            Address,
            (Option<CharacteristicReader>, Option<CharacteristicWriter>),
        > = HashMap::new();
        let mut links = JoinSet::new();
        loop {
            while links.try_join_next().is_some() {}
            let address = tokio::select! {
                event = rx_control.next() => match event {
                    Some(CharacteristicControlEvent::Write(request)) => {
                        let address = request.device_address();
                        match request.accept() {
                            Ok(reader) => half.entry(address).or_default().0 = Some(reader),
                            Err(error) => tracing::debug!(%error, "Bluetooth write refused"),
                        }
                        address
                    }
                    Some(CharacteristicControlEvent::Notify(_)) => continue,
                    None => break,
                },
                event = tx_control.next() => match event {
                    Some(CharacteristicControlEvent::Notify(writer)) => {
                        let address = writer.device_address();
                        half.entry(address).or_default().1 = Some(writer);
                        address
                    }
                    Some(CharacteristicControlEvent::Write(_)) => continue,
                    None => break,
                },
            };
            if let Some((Some(_), Some(_))) = half.get(&address)
                && let Some((Some(reader), Some(writer))) = half.remove(&address)
            {
                links.spawn(link(Arc::clone(&hub), reader, writer));
            }
            if half.len() > MAX_HALF_OPEN {
                // A phone that never finished setting up.
                half.retain(|a, _| *a == address);
            }
        }
    });
    Ok(BleRemote {
        _application: application,
        _advertisement: advertisement,
        task,
    })
}

/// One phone's connection: BlueZ sockets on one side, the core on the other.
async fn link(hub: Arc<Hub>, reader: CharacteristicReader, writer: CharacteristicWriter) {
    let mtu = writer.mtu().min(reader.mtu()).max(core_ble::MIN_MTU);
    let (to_core, frames_in) = mpsc::channel::<Vec<u8>>(64);
    let (frames_out, mut from_core) = mpsc::channel::<Vec<u8>>(64);
    let serve = core_ble::serve_link(hub, mtu, frames_in, frames_out);
    tokio::pin!(serve);
    loop {
        tokio::select! {
            result = &mut serve => {
                if let Err(error) = result {
                    tracing::debug!(%error, "Bluetooth session ended");
                }
                break;
            }
            frame = reader.recv() => match frame {
                Ok(frame) if !frame.is_empty() => {
                    if to_core.send(frame).await.is_err() {
                        break;
                    }
                }
                // The phone stopped writing or left.
                _ => break,
            },
            frame = from_core.recv() => {
                let Some(frame) = frame else { break };
                if writer.send(&frame).await.is_err() {
                    break;
                }
            }
        }
    }
    // Frames the session wrote last, such as a `bye`.
    while let Ok(frame) = from_core.try_recv() {
        if writer.send(&frame).await.is_err() {
            break;
        }
    }
}

/// Opt-in check on real hardware: Studio's service is registered and
/// advertised through BlueZ, and withdrawn again.
#[cfg(test)]
mod hardware_check {
    use super::*;
    use studio_core::db::Database;
    use studio_core::remote::hub::SystemClock;
    use studio_core::remote::identity::Identity;

    #[tokio::test]
    #[ignore = "local: needs BlueZ and a Bluetooth adapter that is on"]
    async fn the_service_is_advertised_through_bluez() {
        let dir = std::env::temp_dir().join(format!("studio-ble-{}", std::process::id()));
        let db = Arc::new(Database::open(dir.join("studio.db")).expect("db"));
        let identity = Arc::new(Identity::generate().expect("identity"));
        let service = identity.ble_service_text();
        let (events, _received) = mpsc::channel(4);
        let hub = Hub::new(db, identity, "Studio check", Arc::new(SystemClock), events);
        let ble = start(hub).await.expect("advertised");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let session = bluer::Session::new().await.expect("session");
        let adapter = session.default_adapter().await.expect("adapter");
        let active = adapter
            .active_advertising_instances()
            .await
            .expect("instances");
        eprintln!("ADV service {service}, active instances {active}");
        assert!(active >= 1);
        drop(ble);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
