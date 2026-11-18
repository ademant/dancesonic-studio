//! Signing out of a server.

use std::sync::Arc;

use crate::catalog::{self, ServerId};
use crate::db::{Database, DbError};
use crate::secrets::{SecretBackend, SecretError, SecretVault};

#[derive(Debug, thiserror::Error)]
pub enum SignOutError {
    #[error(transparent)]
    Secrets(#[from] SecretError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("the database task stopped unexpectedly")]
    Task(#[from] tokio::task::JoinError),
}

/// Signs out of a server: deletes its secrets, then its cached catalog.
///
/// Secrets go first, so a failure never leaves a usable key behind a server
/// the user believes is gone. User data that is not part of the catalog
/// (local playlists, analysis, the edit queue) is kept; clearing it is a
/// separate, explicit choice.
pub async fn sign_out<B: SecretBackend>(
    vault: &SecretVault<B>,
    db: Arc<Database>,
    server: ServerId,
) -> Result<(), SignOutError> {
    let base_url = {
        let db = Arc::clone(&db);
        tokio::task::spawn_blocking(move || catalog::server_base_url(&db.reader()?, server))
            .await??
    };
    let Some(base_url) = base_url else {
        return Ok(());
    };
    vault.delete_account(&base_url).await?;
    tokio::task::spawn_blocking(move || catalog::forget_server(&db, server)).await??;
    tracing::info!("signed out of a server");
    Ok(())
}

#[cfg(test)]
mod tests {
    use secrecy::SecretSlice;

    use super::*;
    use crate::catalog::{ChangeSet, apply_changes, ensure_server, tests::song};
    use crate::db::tests::temp_db;
    use crate::secrets::{SecretId, SecretKind};

    struct NoBackend;
    impl SecretBackend for NoBackend {
        async fn store(
            &self,
            _: &SecretId,
            _: &SecretSlice<u8>,
        ) -> Result<(), crate::secrets::BackendError> {
            Ok(())
        }
        async fn read(
            &self,
            _: &SecretId,
        ) -> Result<Option<SecretSlice<u8>>, crate::secrets::BackendError> {
            Ok(None)
        }
        async fn delete(&self, _: &SecretId) -> Result<(), crate::secrets::BackendError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn sign_out_deletes_key_and_catalog() {
        let (_dir, db) = temp_db();
        let db = Arc::new(db);
        let url = "https://music.example";
        let server = ensure_server(&db, url, 1).unwrap();
        let changes = ChangeSet {
            full: true,
            server_time: 1,
            songs: vec![song("s1", "al1", "An Dro")],
            ..ChangeSet::default()
        };
        apply_changes(&db, server, &changes, 2).unwrap();
        let vault = SecretVault::<NoBackend>::session_only("test");
        let key = SecretId::new(SecretKind::ServerApiKey, url).unwrap();
        vault
            .store(&key, &SecretSlice::from(b"k".to_vec()))
            .await
            .unwrap();

        sign_out(&vault, Arc::clone(&db), server).await.unwrap();

        assert!(vault.read(&key).await.unwrap().is_none());
        let n: i64 = db
            .reader()
            .unwrap()
            .query_row("SELECT count(*) FROM song", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        // Signing out twice is harmless.
        sign_out(&vault, db, server).await.unwrap();
    }
}
