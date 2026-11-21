//! The library database: one SQLite file, opened with safe settings and
//! migrated forward on start.
//!
//! One writer connection is shared behind a mutex and used through
//! [`Database::write`], which runs a short `BEGIN IMMEDIATE` transaction.
//! Readers open their own read-only connections with [`Database::reader`].
//! All calls block; never call them on the GTK main thread.

use std::fs::{self, DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

/// How long a connection waits for a lock held by another connection.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema migrations. Entry `n` brings the schema from version `n` to `n + 1`;
/// the version is kept in `PRAGMA user_version`. Never edit a released entry:
/// add a new one, and a test that migrates data from the previous version.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_catalog.sql"),
    include_str!("migrations/0002_dance_cache.sql"),
    include_str!("migrations/0003_audio_cache.sql"),
    include_str!("migrations/0004_play_queue.sql"),
    include_str!("migrations/0005_local_library.sql"),
    include_str!("migrations/0006_offline.sql"),
    include_str!("migrations/0007_remote_devices.sql"),
    include_str!("migrations/0008_remote_settings.sql"),
    include_str!("migrations/0009_remote_relay.sql"),
];

/// The schema version this build creates and understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot prepare the database file {path}: {source}")]
    File {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "the database has schema version {found}, newer than this version of Studio understands ({supported})"
    )]
    TooNew { found: u32, supported: u32 },
    #[error("foreign key check failed after migration: {0} violations")]
    ForeignKeys(usize),
    #[error("invalid data: {0}")]
    Invalid(String),
}

/// The library database.
#[derive(Debug)]
pub struct Database {
    path: PathBuf,
    writer: Mutex<Connection>,
}

impl Database {
    /// Opens or creates the database at `path` and migrates it to
    /// [`SCHEMA_VERSION`]. Creates the parent directory with mode `0700` and
    /// the file with mode `0600`; an existing file with wider permissions is
    /// narrowed.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, DbError> {
        let path = path.into();
        prepare_file(&path)?;
        let mut conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        configure(&conn)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut conn)?;
        Ok(Self {
            path,
            writer: Mutex::new(conn),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs `f` in one `BEGIN IMMEDIATE` transaction on the writer
    /// connection and commits when it returns `Ok`. Keep `f` short: it holds
    /// the only write lock.
    pub fn write<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        // A panic inside `f` drops the transaction, which rolls it back, so
        // the connection is consistent even when the mutex is poisoned.
        let mut conn = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Opens a new read-only connection. Keep it for the lifetime of the
    /// thread or task that reads, rather than opening one per query.
    pub fn reader(&self) -> Result<Connection, DbError> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        configure(&conn)?;
        Ok(conn)
    }
}

/// Creates the directory and the file with private permissions before SQLite
/// opens it. SQLite gives the `-wal` and `-shm` files the same permissions as
/// the database file.
fn prepare_file(path: &Path) -> Result<(), DbError> {
    let file_err = |source| DbError::File {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(file_err)?;
    }
    // SQLITE_OPEN_NOFOLLOW refuses a symlink when SQLite opens the file; this
    // check makes sure the file is not created through one either.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(file_err(std::io::Error::other(
                "the database path is a symbolic link",
            )));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(file_err(err)),
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(file_err)?;
    let mode = fs::metadata(path).map_err(file_err)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(file_err)?;
    }
    Ok(())
}

/// Settings for every connection, reader or writer.
fn configure(conn: &Connection) -> Result<(), DbError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // Defensive mode and untrusted schema: the schema cannot call functions
    // with side effects, and the file cannot be corrupted through pragmas or
    // writable_schema, even by SQL that reaches the database by mistake.
    conn.pragma_update(None, "trusted_schema", "OFF")?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(())
}

/// Migrates to [`SCHEMA_VERSION`].
fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    migrate_to(conn, SCHEMA_VERSION)
}

/// Migrates forward to `target`, one transaction per step, then checks
/// foreign keys. Refuses a database newer than this build.
fn migrate_to(conn: &mut Connection, target: u32) -> Result<(), DbError> {
    let found = schema_version(conn)?;
    if found > SCHEMA_VERSION {
        return Err(DbError::TooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    for version in found..target {
        let sql = MIGRATIONS
            .get(version as usize)
            .ok_or_else(|| DbError::Invalid(format!("no migration from version {version}")))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
        tracing::info!(from = version, to = version + 1, "database migrated");
    }
    let violations = conn
        .prepare("PRAGMA foreign_key_check")?
        .query_map([], |_| Ok(()))?
        .count();
    if violations > 0 {
        return Err(DbError::ForeignKeys(violations));
    }
    Ok(())
}

fn schema_version(conn: &Connection) -> Result<u32, DbError> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A database in a fresh temporary directory, removed with the guard.
    pub(crate) fn temp_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("data").join("library.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn open_creates_private_files_and_current_schema() {
        let (dir, db) = temp_db();
        let data = dir.path().join("data");
        assert_eq!(
            fs::metadata(&data).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(db.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let reader = db.reader().unwrap();
        assert_eq!(schema_version(&reader).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn every_connection_has_the_required_settings() {
        let (_dir, db) = temp_db();
        let check = |conn: &Connection| {
            let fk: i64 = conn
                .pragma_query_value(None, "foreign_keys", |r| r.get(0))
                .unwrap();
            assert_eq!(fk, 1);
            let journal: String = conn
                .pragma_query_value(None, "journal_mode", |r| r.get(0))
                .unwrap();
            assert_eq!(journal, "wal");
            let sync: i64 = conn
                .pragma_query_value(None, "synchronous", |r| r.get(0))
                .unwrap();
            assert_eq!(sync, 1, "NORMAL");
            let trusted: i64 = conn
                .pragma_query_value(None, "trusted_schema", |r| r.get(0))
                .unwrap();
            assert_eq!(trusted, 0);
            assert!(conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap());
        };
        check(&db.reader().unwrap());
        db.write(|tx| {
            check(tx);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn existing_file_with_wide_permissions_is_narrowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(
            fs::metadata(db.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn symlinked_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.db");
        fs::write(&target, b"").unwrap();
        let link = dir.path().join("library.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(Database::open(&link).is_err());
    }

    #[test]
    fn reopening_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        drop(Database::open(&path).unwrap());
        let db = Database::open(&path).unwrap();
        assert_eq!(
            schema_version(&db.reader().unwrap()).unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn newer_schema_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        assert!(matches!(
            Database::open(&path),
            Err(DbError::TooNew { found, .. }) if found == SCHEMA_VERSION + 1
        ));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION + 1);
    }

    #[test]
    fn readers_cannot_write() {
        let (_dir, db) = temp_db();
        let reader = db.reader().unwrap();
        assert!(
            reader
                .execute(
                    "INSERT INTO server (base_url, created_at) VALUES ('https://x', 0)",
                    []
                )
                .is_err()
        );
    }

    #[test]
    fn failed_write_rolls_back() {
        let (_dir, db) = temp_db();
        let result: Result<(), DbError> = db.write(|tx| {
            tx.execute(
                "INSERT INTO server (base_url, created_at) VALUES ('https://x', 0)",
                [],
            )?;
            Err(DbError::Invalid("stop".into()))
        });
        assert!(result.is_err());
        let count: i64 = db
            .reader()
            .unwrap()
            .query_row("SELECT count(*) FROM server", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// Step 0 → 1: the first schema on an empty database.
    #[test]
    fn migration_0_to_1_creates_the_catalog() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 1).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 1);
        for table in [
            "server",
            "artist",
            "album",
            "song",
            "playlist",
            "playlist_entry",
            "song_fts",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table}");
        }
    }

    /// Step 2 → 3 keeps catalog and dance rows and adds the audio cache index,
    /// whose rows go with their server.
    #[test]
    fn migration_2_to_3_keeps_rows_and_adds_the_audio_cache() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 2).unwrap();
        conn.execute_batch(
            "INSERT INTO server (id, base_url, created_at) VALUES (1, 'https://music.example', 1);
             INSERT INTO song (server_id, id, title, size_bytes, synced_at)
                 VALUES (1, 'so-1', 'Tango', 1000, 1);
             INSERT INTO dance_lookup (mbid, fetched_at)
                 VALUES ('00000000-0000-0000-0000-000000000001', 1);",
        )
        .unwrap();
        migrate_to(&mut conn, 3).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 3);
        let kept: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM song) + (SELECT count(*) FROM dance_lookup)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 2);

        let file = format!("{}.audio", "a".repeat(64));
        conn.execute(
            "INSERT INTO audio_cache
                 (server_id, song_id, file_name, size_bytes, content_type, cached_at, played_at)
                 VALUES (1, 'so-1', ?1, 1000, 'audio/ogg', 2, 2)",
            [&file],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO audio_cache
                     (server_id, song_id, file_name, size_bytes, content_type, cached_at, played_at)
                     VALUES (1, 'so-2', '../escape', 1000, 'audio/ogg', 2, 2)",
                [],
            )
            .is_err(),
            "a file name that is not a digest is refused"
        );
        conn.execute("DELETE FROM server", []).unwrap();
        let left: i64 = conn
            .query_row("SELECT count(*) FROM audio_cache", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "cache rows go with their server");
        assert!(
            conn.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// Step 5 → 6 keeps the local library and adds offline copies, with the
    /// default settings row; copy file names can only be hash-shaped.
    #[test]
    fn migration_5_to_6_keeps_rows_and_adds_offline_copies() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 5).unwrap();
        conn.execute_batch(
            "INSERT INTO local_folder (id, path, added_at) VALUES (1, '/music', 1);",
        )
        .unwrap();
        migrate_to(&mut conn, 6).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 6);
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM local_folder", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1);
        let limit: i64 = conn
            .query_row("SELECT limit_bytes FROM offline_settings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(limit, 10 * 1024 * 1024 * 1024);
        let insert = |name: &str| {
            conn.execute(
                "INSERT INTO offline_track (base_url, song_id, file_name, size_bytes, sha256,
                     stored_at) VALUES ('https://music.example', ?1, ?1, 1, ?2, 1)",
                rusqlite::params![name, "0".repeat(64)],
            )
        };
        insert(&format!("{}.audio", "a".repeat(64))).unwrap();
        assert!(
            insert("../../etc/passwd").is_err(),
            "only hash-shaped names"
        );
        assert!(
            conn.execute(
                "INSERT INTO offline_settings (id, limit_bytes) VALUES (2, 1)",
                []
            )
            .is_err(),
            "one settings row"
        );
        assert!(
            conn.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// Step 7 → 8 keeps paired phones and adds the remote switches, off.
    #[test]
    fn migration_7_to_8_keeps_phones_and_starts_with_remote_control_off() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 7).unwrap();
        conn.execute(
            "INSERT INTO remote_device (id, name, device_key, noise_key, last_counter, paired_at)
             VALUES (?1, 'Phone', ?2, ?3, 4, 1)",
            rusqlite::params!["a".repeat(32), vec![4u8; 65], vec![1u8; 32]],
        )
        .unwrap();
        migrate_to(&mut conn, 8).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 8);
        let counter: i64 = conn
            .query_row("SELECT last_counter FROM remote_device", [], |r| r.get(0))
            .unwrap();
        assert_eq!(counter, 4);
        // The columns of schema 8; `settings::load` reads today's schema.
        let (network, bluetooth, port): (bool, bool, i64) = conn
            .query_row(
                "SELECT network, bluetooth, port FROM remote_settings WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(!network && !bluetooth, "off by default");
        assert_eq!(port, 47800);
        assert!(
            conn.execute("UPDATE remote_settings SET port = 80", [])
                .is_err(),
            "no privileged port"
        );
        assert!(
            conn.execute("INSERT INTO remote_settings (id) VALUES (2)", [])
                .is_err(),
            "one settings row"
        );
    }

    /// Step 8 → 9 keeps every paired phone with its keys, counter and
    /// revocation, allows the relay as last transport, and starts with the
    /// relay off.
    #[test]
    fn migration_8_to_9_keeps_phones_and_adds_the_relay_switch() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 8).unwrap();
        conn.execute(
            "UPDATE remote_settings SET network = 1, bluetooth = 1, port = 48000",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO remote_device (id, name, device_key, noise_key, last_counter,
                 paired_at, last_seen_at, last_transport, revoked_at)
             VALUES (?1, 'Phone', ?2, ?3, 41, 1, 2, 'ble', NULL),
                    (?4, 'Old', ?5, ?6, 7, 1, 3, 'network', 9)",
            rusqlite::params![
                "a".repeat(32),
                vec![4u8; 65],
                vec![1u8; 32],
                "b".repeat(32),
                vec![5u8; 65],
                vec![2u8; 32]
            ],
        )
        .unwrap();
        migrate_to(&mut conn, 9).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 9);

        type Row = (String, i64, Option<String>, Option<i64>, Vec<u8>);
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT id, last_counter, last_transport, revoked_at, noise_key
                 FROM remote_device ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "a".repeat(32),
                    41,
                    Some("ble".to_owned()),
                    None,
                    vec![1u8; 32]
                ),
                (
                    "b".repeat(32),
                    7,
                    Some("network".to_owned()),
                    Some(9),
                    vec![2u8; 32]
                ),
            ]
        );
        conn.execute(
            "UPDATE remote_device SET last_transport = 'relay' WHERE id = ?1",
            ["a".repeat(32)],
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE remote_device SET last_transport = 'usb'", [])
                .is_err(),
            "only known transports"
        );
        assert!(
            conn.execute(
                "INSERT INTO remote_device (id, name, device_key, noise_key, last_counter,
                     paired_at) VALUES (?1, 'Dup', ?2, ?3, 0, 1)",
                rusqlite::params!["c".repeat(32), vec![6u8; 65], vec![1u8; 32]],
            )
            .is_err(),
            "noise keys stay unique"
        );

        let settings = crate::remote::settings::load(&conn).unwrap();
        assert!(settings.network && settings.bluetooth, "switches kept");
        assert_eq!(settings.port, 48000);
        assert!(!settings.relay, "relay off by default");
        assert!(
            conn.execute("UPDATE remote_settings SET relay = 2", [])
                .is_err()
        );
    }

    /// Step 6 → 7 keeps offline pins and adds paired phones, which hold
    /// only well-formed public keys and a counter that never goes negative.
    #[test]
    fn migration_6_to_7_keeps_rows_and_adds_remote_devices() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 6).unwrap();
        conn.execute_batch(
            "INSERT INTO offline_pin (base_url, kind, item_id, pinned_at)
                 VALUES ('https://music.example', 'album', 'al-1', 1);",
        )
        .unwrap();
        migrate_to(&mut conn, 7).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 7);
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM offline_pin", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1);
        let insert = |id: &str, key_len: usize, noise: u8, counter: i64| {
            conn.execute(
                "INSERT INTO remote_device (id, name, device_key, noise_key, last_counter,
                     paired_at) VALUES (?1, 'Phone', ?2, ?3, ?4, 1)",
                rusqlite::params![id, vec![4u8; key_len], vec![noise; 32], counter],
            )
        };
        insert(&"a".repeat(32), 65, 1, 0).unwrap();
        assert!(
            insert(&"b".repeat(32), 64, 2, 0).is_err(),
            "SEC1 points only"
        );
        assert!(
            insert(&"c".repeat(32), 65, 1, 0).is_err(),
            "one phone per Noise key"
        );
        assert!(insert(&"d".repeat(32), 65, 3, -1).is_err(), "counter >= 0");
        assert!(insert("short", 65, 4, 0).is_err(), "id shape");
    }

    /// Step 4 → 5 keeps queued plays and adds the local library, whose
    /// tracks go with their folder and never hold a path that leaves it.
    #[test]
    fn migration_4_to_5_keeps_rows_and_adds_the_local_library() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 4).unwrap();
        conn.execute_batch(
            "INSERT INTO server (id, base_url, created_at) VALUES (1, 'https://music.example', 1);
             INSERT INTO play_queue (server_id, song_id, played_at) VALUES (1, 'so-1', 5);",
        )
        .unwrap();
        migrate_to(&mut conn, 5).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 5);
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM play_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1);

        conn.execute(
            "INSERT INTO local_folder (id, path, added_at) VALUES (1, '/music', 1)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO local_folder (path, added_at) VALUES ('relative', 1)",
                []
            )
            .is_err(),
            "a folder path is absolute"
        );
        let print = "0".repeat(64);
        let insert = |rel: &str| {
            conn.execute(
                "INSERT INTO local_track (folder_id, rel_path, size_bytes, modified_ms,
                     fingerprint, scanned_at) VALUES (1, ?1, 1, 1, ?2, 1)",
                rusqlite::params![rel, print],
            )
        };
        insert("Artist/track.flac").unwrap();
        for bad in ["/etc/passwd", "..", "../x", "a/../../x", "a/.."] {
            assert!(insert(bad).is_err(), "{bad} is refused");
        }
        conn.execute("DELETE FROM local_folder", []).unwrap();
        let left: i64 = conn
            .query_row("SELECT count(*) FROM local_track", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "tracks go with their folder");
        assert!(
            conn.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// Step 3 → 4 keeps the cache rows and adds the play queue, whose rows go
    /// with their server and are unique per song and play time.
    #[test]
    fn migration_3_to_4_keeps_rows_and_adds_the_play_queue() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 3).unwrap();
        let file = format!("{}.audio", "a".repeat(64));
        conn.execute_batch(
            "INSERT INTO server (id, base_url, created_at) VALUES (1, 'https://music.example', 1);
             INSERT INTO song (server_id, id, title, size_bytes, synced_at)
                 VALUES (1, 'so-1', 'Tango', 1000, 1);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO audio_cache
                 (server_id, song_id, file_name, size_bytes, content_type, cached_at, played_at)
                 VALUES (1, 'so-1', ?1, 1000, 'audio/ogg', 2, 2)",
            [&file],
        )
        .unwrap();
        migrate_to(&mut conn, 4).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 4);
        let kept: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM song) + (SELECT count(*) FROM audio_cache)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 2);

        conn.execute(
            "INSERT INTO play_queue (server_id, song_id, played_at) VALUES (1, 'so-1', 5)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO play_queue (server_id, song_id, played_at) VALUES (1, 'so-1', 5)",
                [],
            )
            .is_err(),
            "the same play is queued once"
        );
        assert!(
            conn.execute(
                "INSERT INTO play_queue (server_id, song_id, played_at) VALUES (1, '', 6)",
                [],
            )
            .is_err(),
            "an empty song id is refused"
        );
        conn.execute("DELETE FROM server", []).unwrap();
        let left: i64 = conn
            .query_row("SELECT count(*) FROM play_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "queued plays go with their server");
        assert!(
            conn.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// Step 1 → 2 keeps existing catalog rows and adds the dance cache.
    #[test]
    fn migration_1_to_2_keeps_catalog_rows_and_adds_the_dance_cache() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate_to(&mut conn, 1).unwrap();
        conn.execute(
            "INSERT INTO server (base_url, created_at) VALUES ('https://music.example', 1)",
            [],
        )
        .unwrap();
        migrate_to(&mut conn, 2).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 2);
        let servers: i64 = conn
            .query_row("SELECT count(*) FROM server", [], |r| r.get(0))
            .unwrap();
        assert_eq!(servers, 1);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name IN ('dance_lookup', 'dance_tag')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
        assert!(
            conn.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .next()
                .is_none()
        );
    }
}
