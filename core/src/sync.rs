//! Catalog sync: ask a [`CatalogSource`] what changed since the last sync and
//! apply it to the database in one transaction.
//!
//! The source decides how to find out: folksonic's change feed
//! (`getLibraryChanges`) where advertised, otherwise paged catalog requests
//! guarded by the server's last-modified time. Either way the source returns
//! a [`ChangeSet`]; when nothing changed it is empty, and the database is not
//! rewritten. The HTTP source is part of the server client (#3).

use std::future::Future;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::catalog::{self, ApplyReport, ChangeSet, ServerId};
use crate::db::{Database, DbError};

/// Where catalog changes come from.
pub trait CatalogSource: Send + Sync {
    /// Everything that changed since `since` (the `server_time` of the last
    /// applied change set), or the whole catalog with `full: true` when
    /// `since` is `None` or too old for the server.
    fn changes(
        &self,
        since: Option<i64>,
    ) -> impl Future<Output = Result<ChangeSet, SourceError>> + Send;
}

/// An error from a [`CatalogSource`]. Must not carry keys or full URLs.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("the server could not be reached: {0}")]
    Unreachable(String),
    #[error("the server refused the request: {0}")]
    Refused(String),
    #[error("the server sent an invalid answer: {0}")]
    Invalid(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("the database task stopped unexpectedly")]
    Task(#[from] tokio::task::JoinError),
}

/// Syncs one server's catalog. Database work runs on Tokio's blocking pool,
/// so this is safe to await from any async task.
pub async fn sync_server<S: CatalogSource>(
    source: &S,
    db: Arc<Database>,
    server: ServerId,
) -> Result<ApplyReport, SyncError> {
    let since = {
        let db = Arc::clone(&db);
        tokio::task::spawn_blocking(move || catalog::sync_since(&db.reader()?, server)).await??
    };
    let changes = source.changes(since).await?;
    if is_empty(&changes) && since.is_some() {
        tracing::debug!("catalog unchanged");
    }
    let report = tokio::task::spawn_blocking(move || {
        catalog::apply_changes(&db, server, &changes, now_ms())
    })
    .await??;
    tracing::info!(
        upserted = report.upserted,
        deleted = report.deleted,
        skipped = report.skipped,
        "catalog synced"
    );
    Ok(report)
}

fn is_empty(c: &ChangeSet) -> bool {
    !c.full
        && c.artists.is_empty()
        && c.albums.is_empty()
        && c.songs.is_empty()
        && c.playlists.is_empty()
        && c.deleted.is_empty()
}

/// The current time in epoch milliseconds; 0 if the clock is before 1970.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::catalog::tests::song;
    use crate::catalog::{ensure_server, songs_of_album};
    use crate::db::tests::temp_db;

    /// Answers with prepared change sets and records the `since` it was asked with.
    struct FakeSource {
        answers: Mutex<Vec<Result<ChangeSet, SourceError>>>,
        asked: Mutex<Vec<Option<i64>>>,
    }

    impl FakeSource {
        fn new(answers: Vec<Result<ChangeSet, SourceError>>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().rev().collect()),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl CatalogSource for FakeSource {
        async fn changes(&self, since: Option<i64>) -> Result<ChangeSet, SourceError> {
            self.asked.lock().unwrap().push(since);
            self.answers.lock().unwrap().pop().unwrap()
        }
    }

    #[tokio::test]
    async fn first_sync_is_full_then_incremental_from_server_time() {
        let (_dir, db) = temp_db();
        let db = Arc::new(db);
        let server = ensure_server(&db, "https://music.example", 1).unwrap();
        let source = FakeSource::new(vec![
            Ok(ChangeSet {
                full: true,
                server_time: 500,
                songs: vec![song("s1", "al1", "An Dro"), song("s2", "al1", "Hanter Dro")],
                ..ChangeSet::default()
            }),
            Ok(ChangeSet {
                server_time: 900,
                ..ChangeSet::default()
            }),
        ]);

        sync_server(&source, Arc::clone(&db), server).await.unwrap();
        let report = sync_server(&source, Arc::clone(&db), server).await.unwrap();

        assert_eq!(*source.asked.lock().unwrap(), [None, Some(500)]);
        assert_eq!(report, ApplyReport::default(), "nothing rewritten");
        let conn = db.reader().unwrap();
        assert_eq!(songs_of_album(&conn, server, "al1").unwrap().len(), 2);
        assert_eq!(catalog::sync_since(&conn, server).unwrap(), Some(900));
    }

    #[tokio::test]
    async fn a_failed_fetch_changes_nothing() {
        let (_dir, db) = temp_db();
        let db = Arc::new(db);
        let server = ensure_server(&db, "https://music.example", 1).unwrap();
        let source = FakeSource::new(vec![Err(SourceError::Unreachable("offline".into()))]);
        assert!(matches!(
            sync_server(&source, Arc::clone(&db), server).await,
            Err(SyncError::Source(SourceError::Unreachable(_)))
        ));
        assert_eq!(
            catalog::sync_since(&db.reader().unwrap(), server).unwrap(),
            None
        );
    }
}
