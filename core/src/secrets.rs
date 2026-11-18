//! Credentials: API keys and device keys.
//!
//! Secrets are kept in the desktop's Secret Service (GNOME Keyring on Ubuntu,
//! Xubuntu and Ubuntu Budgie). They are never written to a file, the database
//! or the config. When no Secret Service is available, [`SecretVault`] keeps
//! them in memory for this session only and reports that through
//! [`SecretVault::persistence`], so the UI can say so.
//!
//! The Secret Service itself is behind [`SecretBackend`]; the D-Bus
//! implementation lives in the app, tests use a fake.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Mutex, PoisonError};

use secrecy::{ExposeSecret, SecretSlice};

/// Longest accepted account name, in bytes. Accounts are server URLs or
/// device names, never anything near this long.
const MAX_ACCOUNT_LEN: usize = 2048;

/// What a secret is for. Stored as the `kind` attribute in the Secret Service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SecretKind {
    /// An OpenSubsonic API key for a music server.
    ServerApiKey,
    /// The private device key of a pairing with a folksonic server (DPoP).
    ServerDeviceKey,
    /// The private device key of a pairing with DanceBrainz (DPoP).
    DanceBrainzDeviceKey,
    /// Studio's long-term key for remote control pairing.
    RemoteIdentityKey,
}

impl SecretKind {
    /// All kinds, for deleting everything that belongs to an account.
    pub const ALL: [SecretKind; 4] = [
        SecretKind::ServerApiKey,
        SecretKind::ServerDeviceKey,
        SecretKind::DanceBrainzDeviceKey,
        SecretKind::RemoteIdentityKey,
    ];

    /// The stable attribute value. Changing it would orphan stored secrets.
    pub fn attribute(self) -> &'static str {
        match self {
            SecretKind::ServerApiKey => "server-api-key",
            SecretKind::ServerDeviceKey => "server-device-key",
            SecretKind::DanceBrainzDeviceKey => "dancebrainz-device-key",
            SecretKind::RemoteIdentityKey => "remote-identity-key",
        }
    }

    /// A human-readable description, used in the item label that keyring
    /// managers such as Seahorse show.
    pub fn description(self) -> &'static str {
        match self {
            SecretKind::ServerApiKey => "API key",
            SecretKind::ServerDeviceKey => "server device key",
            SecretKind::DanceBrainzDeviceKey => "DanceBrainz device key",
            SecretKind::RemoteIdentityKey => "remote control key",
        }
    }
}

/// Identifies one secret: what it is for and which account it belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SecretId {
    kind: SecretKind,
    account: String,
}

impl SecretId {
    /// `account` names the server or service the secret belongs to, for
    /// example a server's base URL. It is not secret, but it is shown in
    /// keyring managers.
    pub fn new(kind: SecretKind, account: impl Into<String>) -> Result<Self, SecretError> {
        let account = account.into();
        if account.is_empty() || account.len() > MAX_ACCOUNT_LEN || account.contains('\0') {
            return Err(SecretError::InvalidAccount);
        }
        Ok(Self { kind, account })
    }

    pub fn kind(&self) -> SecretKind {
        self.kind
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    /// The label a keyring manager shows for this item.
    pub fn label(&self) -> String {
        format!(
            "{}: {} for {}",
            crate::APP_NAME,
            self.kind.description(),
            self.account
        )
    }
}

/// A store for secrets, normally the Secret Service over D-Bus.
///
/// Implementations must not log secret values.
pub trait SecretBackend: Send + Sync {
    /// Stores `secret`, replacing an existing one with the same id.
    fn store(
        &self,
        id: &SecretId,
        secret: &SecretSlice<u8>,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Reads a secret; `None` when there is none.
    fn read(
        &self,
        id: &SecretId,
    ) -> impl Future<Output = Result<Option<SecretSlice<u8>>, BackendError>> + Send;

    /// Deletes a secret. Deleting one that does not exist is not an error.
    fn delete(&self, id: &SecretId) -> impl Future<Output = Result<(), BackendError>> + Send;
}

/// An error from a [`SecretBackend`]. Carries no secret material.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The user dismissed the unlock prompt of the keyring.
    #[error("the keyring was not unlocked")]
    Dismissed,
    /// The Secret Service failed or went away.
    #[error("Secret Service error: {0}")]
    Service(String),
}

/// An error from [`SecretVault`].
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("invalid account name for a secret")]
    InvalidAccount,
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// Where secrets end up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Persistence {
    /// In the Secret Service; they survive a restart.
    SecretService,
    /// In memory only, because no Secret Service is available. They are lost
    /// when Studio closes. `reason` says why, for the UI and the log.
    SessionOnly { reason: String },
}

/// Stores secrets in the Secret Service, or for this session only when there
/// is none. Never falls back to a file.
pub struct SecretVault<B> {
    backend: Option<B>,
    persistence: Persistence,
    session: Mutex<HashMap<SecretId, SecretSlice<u8>>>,
}

impl<B> fmt::Debug for SecretVault<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only the ids of session secrets, never their values.
        let ids: Vec<SecretId> = self.lock_session().keys().cloned().collect();
        f.debug_struct("SecretVault")
            .field("persistence", &self.persistence)
            .field("session_ids", &ids)
            .finish_non_exhaustive()
    }
}

impl<B: SecretBackend> SecretVault<B> {
    /// A vault backed by the Secret Service.
    pub fn with_backend(backend: B) -> Self {
        Self {
            backend: Some(backend),
            persistence: Persistence::SecretService,
            session: Mutex::new(HashMap::new()),
        }
    }

    /// A vault that keeps secrets for this session only, because the Secret
    /// Service could not be reached.
    pub fn session_only(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        tracing::warn!(%reason, "no Secret Service: credentials are kept for this session only");
        Self {
            backend: None,
            persistence: Persistence::SessionOnly { reason },
            session: Mutex::new(HashMap::new()),
        }
    }

    pub fn persistence(&self) -> &Persistence {
        &self.persistence
    }

    pub async fn store(&self, id: &SecretId, secret: &SecretSlice<u8>) -> Result<(), SecretError> {
        match &self.backend {
            Some(backend) => backend.store(id, secret).await?,
            None => {
                self.lock_session().insert(id.clone(), copy_secret(secret));
            }
        }
        tracing::debug!(kind = id.kind().attribute(), "secret stored");
        Ok(())
    }

    pub async fn read(&self, id: &SecretId) -> Result<Option<SecretSlice<u8>>, SecretError> {
        match &self.backend {
            Some(backend) => Ok(backend.read(id).await?),
            None => Ok(self.lock_session().get(id).map(copy_secret)),
        }
    }

    pub async fn delete(&self, id: &SecretId) -> Result<(), SecretError> {
        match &self.backend {
            Some(backend) => backend.delete(id).await?,
            None => {
                self.lock_session().remove(id);
            }
        }
        tracing::debug!(kind = id.kind().attribute(), "secret deleted");
        Ok(())
    }

    /// Deletes every kind of secret that belongs to `account`, as on logout.
    /// Tries all kinds and reports the first error.
    pub async fn delete_account(&self, account: &str) -> Result<(), SecretError> {
        let mut first_error = None;
        for kind in SecretKind::ALL {
            let id = SecretId::new(kind, account)?;
            if let Err(err) = self.delete(&id).await {
                first_error.get_or_insert(err);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl<B> SecretVault<B> {
    fn lock_session(&self) -> std::sync::MutexGuard<'_, HashMap<SecretId, SecretSlice<u8>>> {
        // A panic while holding the lock cannot leave the map half-updated:
        // every operation on it is a single insert, get or remove.
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Copies a secret into a new zeroizing buffer.
pub fn copy_secret(secret: &SecretSlice<u8>) -> SecretSlice<u8> {
    SecretSlice::from(secret.expose_secret().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that stores in memory and can be told to fail.
    #[derive(Default)]
    struct FakeBackend {
        items: Mutex<HashMap<SecretId, Vec<u8>>>,
        fail: bool,
    }

    impl SecretBackend for FakeBackend {
        async fn store(&self, id: &SecretId, secret: &SecretSlice<u8>) -> Result<(), BackendError> {
            if self.fail {
                return Err(BackendError::Dismissed);
            }
            self.items
                .lock()
                .unwrap()
                .insert(id.clone(), secret.expose_secret().to_vec());
            Ok(())
        }

        async fn read(&self, id: &SecretId) -> Result<Option<SecretSlice<u8>>, BackendError> {
            if self.fail {
                return Err(BackendError::Service("gone".into()));
            }
            Ok(self
                .items
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(SecretSlice::from))
        }

        async fn delete(&self, id: &SecretId) -> Result<(), BackendError> {
            if self.fail {
                return Err(BackendError::Service("gone".into()));
            }
            self.items.lock().unwrap().remove(id);
            Ok(())
        }
    }

    fn api_key(account: &str) -> SecretId {
        SecretId::new(SecretKind::ServerApiKey, account).unwrap()
    }

    fn secret(value: &[u8]) -> SecretSlice<u8> {
        SecretSlice::from(value.to_vec())
    }

    fn exposed(secret: Option<SecretSlice<u8>>) -> Option<Vec<u8>> {
        secret.map(|s| s.expose_secret().to_vec())
    }

    #[tokio::test]
    async fn store_read_delete_with_backend() {
        let vault = SecretVault::with_backend(FakeBackend::default());
        assert_eq!(vault.persistence(), &Persistence::SecretService);
        let id = api_key("https://music.example");

        assert_eq!(exposed(vault.read(&id).await.unwrap()), None);
        vault.store(&id, &secret(b"key-1")).await.unwrap();
        assert_eq!(
            exposed(vault.read(&id).await.unwrap()),
            Some(b"key-1".to_vec())
        );
        vault.store(&id, &secret(b"key-2")).await.unwrap();
        assert_eq!(
            exposed(vault.read(&id).await.unwrap()),
            Some(b"key-2".to_vec())
        );
        vault.delete(&id).await.unwrap();
        assert_eq!(exposed(vault.read(&id).await.unwrap()), None);
        // Deleting again is fine.
        vault.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn session_only_keeps_secrets_in_memory() {
        let vault = SecretVault::<FakeBackend>::session_only("no D-Bus");
        assert_eq!(
            vault.persistence(),
            &Persistence::SessionOnly {
                reason: "no D-Bus".into()
            }
        );
        let id = api_key("https://music.example");
        vault.store(&id, &secret(b"key")).await.unwrap();
        assert_eq!(
            exposed(vault.read(&id).await.unwrap()),
            Some(b"key".to_vec())
        );
        vault.delete(&id).await.unwrap();
        assert_eq!(exposed(vault.read(&id).await.unwrap()), None);
    }

    #[tokio::test]
    async fn backend_errors_are_reported_not_hidden_in_the_session() {
        let vault = SecretVault::with_backend(FakeBackend {
            fail: true,
            ..FakeBackend::default()
        });
        let id = api_key("https://music.example");
        assert!(matches!(
            vault.store(&id, &secret(b"key")).await,
            Err(SecretError::Backend(BackendError::Dismissed))
        ));
        assert!(vault.read(&id).await.is_err());
    }

    #[tokio::test]
    async fn delete_account_removes_every_kind_of_that_account_only() {
        let vault = SecretVault::with_backend(FakeBackend::default());
        let account = "https://music.example";
        for kind in SecretKind::ALL {
            let id = SecretId::new(kind, account).unwrap();
            vault.store(&id, &secret(b"s")).await.unwrap();
        }
        let other = api_key("https://other.example");
        vault.store(&other, &secret(b"o")).await.unwrap();

        vault.delete_account(account).await.unwrap();

        for kind in SecretKind::ALL {
            let id = SecretId::new(kind, account).unwrap();
            assert!(vault.read(&id).await.unwrap().is_none());
        }
        assert!(vault.read(&other).await.unwrap().is_some());
    }

    #[test]
    fn account_must_be_reasonable() {
        assert!(SecretId::new(SecretKind::ServerApiKey, "").is_err());
        assert!(SecretId::new(SecretKind::ServerApiKey, "a\0b").is_err());
        assert!(SecretId::new(SecretKind::ServerApiKey, "x".repeat(MAX_ACCOUNT_LEN + 1)).is_err());
        assert!(SecretId::new(SecretKind::ServerApiKey, "https://music.example").is_ok());
    }

    #[tokio::test]
    async fn debug_output_never_contains_secret_values() {
        let vault = SecretVault::<FakeBackend>::session_only("test");
        let id = api_key("https://music.example");
        vault
            .store(&id, &secret(b"TOP-SECRET-VALUE"))
            .await
            .unwrap();
        let debug = format!("{vault:?}");
        assert!(!debug.contains("TOP-SECRET-VALUE"), "{debug}");
        assert!(debug.contains("music.example"));

        let value = vault.read(&id).await.unwrap().unwrap();
        let debug = format!("{value:?}");
        assert!(!debug.contains("TOP-SECRET-VALUE"), "{debug}");
    }

    #[test]
    fn kind_attributes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for kind in SecretKind::ALL {
            assert!(seen.insert(kind.attribute()));
        }
    }
}
