//! [`SecretBackend`] on the freedesktop Secret Service (GNOME Keyring).
//!
//! Talks D-Bus directly through `oo7::dbus`, never oo7's file keyring, and
//! only over an encrypted session: secrets never cross the bus in plain text.

use std::collections::HashMap;

use oo7::dbus::{Collection, Service};
use secrecy::{ExposeSecret, SecretSlice};
use studio_core::APP_ID;
use studio_core::secrets::{BackendError, SecretBackend, SecretId};

/// Attribute names on every item. `xdg:schema` lets keyring managers group
/// Studio's items; `kind` and `account` identify one secret.
const SCHEMA_ATTR: &str = "xdg:schema";
const KIND_ATTR: &str = "kind";
const ACCOUNT_ATTR: &str = "account";

#[derive(Debug)]
pub(crate) struct SecretService {
    collection: Collection,
}

impl SecretService {
    /// Connects to the Secret Service and opens the default collection.
    /// Fails when there is no Secret Service on the session bus, or it does
    /// not support encrypted sessions.
    pub(crate) async fn connect() -> Result<Self, BackendError> {
        let service = Service::encrypted().await.map_err(service_error)?;
        let collection = service.default_collection().await.map_err(service_error)?;
        Ok(Self { collection })
    }

    /// Unlocks the collection if needed; the desktop shows its own prompt.
    async fn unlocked(&self) -> Result<&Collection, BackendError> {
        if self.collection.is_locked().await.map_err(service_error)? {
            self.collection.unlock(None).await.map_err(service_error)?;
        }
        Ok(&self.collection)
    }
}

fn attributes(id: &SecretId) -> HashMap<&str, &str> {
    HashMap::from([
        (SCHEMA_ATTR, APP_ID),
        (KIND_ATTR, id.kind().attribute()),
        (ACCOUNT_ATTR, id.account()),
    ])
}

/// Maps oo7 errors without passing on anything but the error's kind.
fn service_error(err: oo7::dbus::Error) -> BackendError {
    match err {
        oo7::dbus::Error::Dismissed => BackendError::Dismissed,
        other => BackendError::Service(other.to_string()),
    }
}

impl SecretBackend for SecretService {
    async fn store(&self, id: &SecretId, secret: &SecretSlice<u8>) -> Result<(), BackendError> {
        let collection = self.unlocked().await?;
        collection
            .create_item(
                &id.label(),
                &attributes(id),
                oo7::Secret::blob(secret.expose_secret()),
                true,
                None,
            )
            .await
            .map_err(service_error)?;
        Ok(())
    }

    async fn read(&self, id: &SecretId) -> Result<Option<SecretSlice<u8>>, BackendError> {
        let collection = self.unlocked().await?;
        let items = collection
            .search_items(&attributes(id))
            .await
            .map_err(service_error)?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        if items.len() > 1 {
            tracing::warn!(
                count = items.len(),
                kind = id.kind().attribute(),
                "several keyring items for one secret; using the first"
            );
        }
        let secret = item.secret().await.map_err(service_error)?;
        Ok(Some(SecretSlice::from(secret.as_bytes().to_vec())))
    }

    async fn delete(&self, id: &SecretId) -> Result<(), BackendError> {
        let collection = self.unlocked().await?;
        for item in collection
            .search_items(&attributes(id))
            .await
            .map_err(service_error)?
        {
            item.delete(None).await.map_err(service_error)?;
        }
        Ok(())
    }
}

/// Against the real Secret Service of the desktop session. Run on each
/// target desktop with `cargo test -p dancesonic-studio -- --ignored`.
#[cfg(test)]
mod tests {
    use super::*;
    use studio_core::secrets::SecretKind;

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a desktop session with a Secret Service"]
    async fn round_trip_in_the_real_keyring() {
        let backend = SecretService::connect().await.unwrap();
        let account = format!("test://dancesonic-studio/{}", std::process::id());
        let id = SecretId::new(SecretKind::ServerApiKey, account).unwrap();

        backend
            .store(&id, &SecretSlice::from(b"first".to_vec()))
            .await
            .unwrap();
        backend
            .store(&id, &SecretSlice::from(b"second".to_vec()))
            .await
            .unwrap();
        let read = backend.read(&id).await.unwrap().unwrap();
        assert_eq!(read.expose_secret(), b"second", "replaced, not duplicated");

        backend.delete(&id).await.unwrap();
        assert!(backend.read(&id).await.unwrap().is_none());
    }
}
