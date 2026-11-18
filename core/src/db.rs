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
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_catalog.sql")];

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
}
