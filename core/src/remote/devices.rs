//! Phones paired for remote control and their replay counters.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::messages::{MAX_COUNTER, Transport};
use crate::db::{Database, DbError};

/// A paired phone's ID: 32 lowercase hex characters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(String);

impl DeviceId {
    /// A new random ID.
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }

    /// Parses an ID from the wire.
    pub fn parse(text: &str) -> Option<Self> {
        (text.len() == 32 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(text.to_owned()))
    }

    /// The ID text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A paired phone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    /// Uncompressed SEC1 P-256 point.
    pub device_key: Vec<u8>,
    /// X25519 public key.
    pub noise_key: [u8; 32],
    pub last_counter: u64,
    pub paired_at: i64,
    pub last_seen_at: Option<i64>,
    pub last_transport: Option<String>,
    pub revoked_at: Option<i64>,
}

impl Device {
    /// Whether the phone was revoked.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// Why a counter was not accepted.
#[derive(Debug, thiserror::Error)]
pub enum CounterError {
    #[error("unknown phone")]
    NotPaired,
    #[error("the phone was revoked")]
    Revoked,
    #[error("the message was already seen")]
    Replay,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl CounterError {
    /// The wire error code.
    pub fn code(&self) -> &'static str {
        match self {
            CounterError::NotPaired => "not_paired",
            CounterError::Revoked => "revoked",
            CounterError::Replay => "replay",
            CounterError::Db(_) => "failed",
        }
    }
}

const COLUMNS: &str = "id, name, device_key, noise_key, last_counter, paired_at, last_seen_at,
     last_transport, revoked_at";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    let noise: Vec<u8> = row.get(3)?;
    let counter: i64 = row.get(4)?;
    Ok(Device {
        id: DeviceId(row.get(0)?),
        name: row.get(1)?,
        device_key: row.get(2)?,
        noise_key: noise.try_into().unwrap_or([0; 32]),
        last_counter: u64::try_from(counter).unwrap_or(0),
        paired_at: row.get(5)?,
        last_seen_at: row.get(6)?,
        last_transport: row.get(7)?,
        revoked_at: row.get(8)?,
    })
}

/// Stores a phone the user accepted. A phone that pairs again with the same
/// Noise key replaces its old entry, also a revoked one: the user accepted it
/// anew.
pub fn add(
    db: &Database,
    id: &DeviceId,
    name: &str,
    device_key: &[u8],
    noise_key: &[u8; 32],
    now_ms: i64,
) -> Result<(), DbError> {
    db.write(|tx| {
        tx.execute(
            "DELETE FROM remote_device WHERE noise_key = ?1",
            params![noise_key.as_slice()],
        )?;
        tx.execute(
            "INSERT INTO remote_device (id, name, device_key, noise_key, last_counter, paired_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![id.as_str(), name, device_key, noise_key.as_slice(), now_ms],
        )?;
        Ok(())
    })
}

/// All paired phones, newest first.
pub fn list(conn: &Connection) -> Result<Vec<Device>, DbError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM remote_device ORDER BY paired_at DESC, id"
    ))?;
    let rows = stmt.query_map([], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// One phone by ID.
pub fn get(conn: &Connection, id: &DeviceId) -> Result<Option<Device>, DbError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM remote_device WHERE id = ?1"
    ))?;
    Ok(stmt.query_row([id.as_str()], from_row).optional()?)
}

/// One phone by its Noise key.
pub fn by_noise_key(conn: &Connection, key: &[u8]) -> Result<Option<Device>, DbError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM remote_device WHERE noise_key = ?1"
    ))?;
    Ok(stmt.query_row([key], from_row).optional()?)
}

/// Accepts `counter` from a phone if it is higher than every counter before
/// and the phone is not revoked, and stores it durably before returning.
pub fn advance(
    db: &Database,
    id: &DeviceId,
    counter: u64,
    transport: Transport,
    now_ms: i64,
) -> Result<(), CounterError> {
    if counter == 0 || counter > MAX_COUNTER {
        return Err(CounterError::Replay);
    }
    let counter = i64::try_from(counter).map_err(|_| CounterError::Replay)?;
    db.write(|tx| Ok(advance_in(tx, id, counter, transport, now_ms)))?
}

fn advance_in(
    tx: &Transaction<'_>,
    id: &DeviceId,
    counter: i64,
    transport: Transport,
    now_ms: i64,
) -> Result<(), CounterError> {
    let changed = tx
        .execute(
            "UPDATE remote_device SET last_counter = ?2, last_seen_at = ?3, last_transport = ?4
             WHERE id = ?1 AND revoked_at IS NULL AND last_counter < ?2",
            params![id.as_str(), counter, now_ms, transport.as_str()],
        )
        .map_err(DbError::from)?;
    if changed == 1 {
        return Ok(());
    }
    let revoked: Option<Option<i64>> = tx
        .query_row(
            "SELECT revoked_at FROM remote_device WHERE id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(DbError::from)?;
    Err(match revoked {
        None => CounterError::NotPaired,
        Some(Some(_)) => CounterError::Revoked,
        Some(None) => CounterError::Replay,
    })
}

/// Revokes a phone: it is refused from now on, also on open connections.
pub fn revoke(db: &Database, id: &DeviceId, now_ms: i64) -> Result<bool, DbError> {
    db.write(|tx| {
        Ok(tx.execute(
            "UPDATE remote_device SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            params![id.as_str(), now_ms],
        )? == 1)
    })
}

/// Forgets a phone entirely.
pub fn remove(db: &Database, id: &DeviceId) -> Result<bool, DbError> {
    db.write(|tx| Ok(tx.execute("DELETE FROM remote_device WHERE id = ?1", [id.as_str()])? == 1))
}
