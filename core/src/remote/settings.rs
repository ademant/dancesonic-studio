//! The remote control switches. All are off by default.

use rusqlite::{Connection, params};

use crate::db::{Database, DbError};

/// What the user allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// "Allow remote control" on the local network.
    pub network: bool,
    /// "Allow remote control over Bluetooth".
    pub bluetooth: bool,
    /// "Allow remote control through folksonic" (#38): publish Studio's
    /// record and keep a relay connection while signed in.
    pub relay: bool,
    /// The port tried first for the network transport.
    pub port: u16,
}

/// Reads the settings.
pub fn load(conn: &Connection) -> Result<Settings, DbError> {
    Ok(conn.query_row(
        "SELECT network, bluetooth, port, relay FROM remote_settings WHERE id = 1",
        [],
        |r| {
            Ok(Settings {
                network: r.get(0)?,
                bluetooth: r.get(1)?,
                port: r.get(2)?,
                relay: r.get(3)?,
            })
        },
    )?)
}

/// Stores the settings.
pub fn save(db: &Database, settings: Settings) -> Result<(), DbError> {
    db.write(|tx| {
        tx.execute(
            "UPDATE remote_settings SET network = ?1, bluetooth = ?2, port = ?3, relay = ?4
             WHERE id = 1",
            params![
                settings.network,
                settings.bluetooth,
                settings.port,
                settings.relay
            ],
        )?;
        Ok(())
    })
}
