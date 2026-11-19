//! Connecting Studio to a server with an API key.
//!
//! The server is checked first: it must advertise `apiKeyAuthentication` and the change
//! feed. Only when it accepts the key is the key stored, and only then is the
//! catalog synced. A refused connection leaves no secret and no catalog behind.

use std::sync::Arc;

use secrecy::{ExposeSecret, SecretSlice};

use crate::catalog::{self, ApplyReport, ServerId};
use crate::connection::ServerUrl;
use crate::db::{Database, DbError};
use crate::dpop::{DeviceKey, DpopError};
use crate::http::{self, HttpError, HttpSettings};
use crate::library_feed::{FEED_EXTENSION, LibrarySource};
use crate::secrets::{SecretBackend, SecretError, SecretId, SecretKind, SecretVault};
use crate::subsonic::{ApiError, ApiKey, ApiKeyError, Auth, ServerInfo, SubsonicClient};
use crate::sync::{self, SyncError};

/// The extension a server must advertise for API-key sign-in.
pub const AUTH_EXTENSION: &str = "apiKeyAuthentication";

/// What a successful connection produced.
#[derive(Debug)]
pub struct Connected {
    pub server: ServerId,
    pub info: ServerInfo,
    pub report: ApplyReport,
}

/// Errors from connecting. Carry no keys and no full addresses.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Secrets(#[from] SecretError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Sync(#[from] SyncError),
    #[error("the database task stopped unexpectedly")]
    Task(#[from] tokio::task::JoinError),
    #[error("the stored credential is not valid")]
    BadStoredSecret,
    #[error(transparent)]
    Key(#[from] ApiKeyError),
    #[error(transparent)]
    Device(#[from] DpopError),
}

/// Connects to `server` with the given credentials, stores them and syncs the
/// catalog.
///
/// The server is checked first, and the credentials are stored only once it
/// has accepted them. A paired device replaces the API key for the server, so
/// the local copy of that key is removed. The key itself stays valid on the
/// server until the user revokes it on the account page: a device cannot
/// revoke keys.
pub async fn connect<B: SecretBackend>(
    settings: &HttpSettings,
    vault: &SecretVault<B>,
    db: Arc<Database>,
    server: ServerUrl,
    auth: Auth,
) -> Result<Connected, ConnectError> {
    let http = http::client(settings)?;
    let probe = SubsonicClient::new(http.clone(), server.clone(), auth.clone());

    let info = probe.server_info().await?;
    if !info.has_extension(AUTH_EXTENSION) {
        return Err(ApiError::Missing(AUTH_EXTENSION).into());
    }
    if !info.has_extension(FEED_EXTENSION) {
        return Err(ApiError::Missing(FEED_EXTENSION).into());
    }

    // The server has accepted these credentials, so it is safe to keep them.
    let (kind, secret) = match &auth {
        Auth::ApiKey(key) => (
            SecretKind::ServerApiKey,
            SecretSlice::from(key.expose().as_bytes().to_vec()),
        ),
        Auth::Device(device) => (SecretKind::ServerDeviceKey, device.secret()),
    };
    vault
        .store(&SecretId::new(kind, server.key())?, &secret)
        .await?;
    if matches!(auth, Auth::Device(_)) {
        vault
            .delete(&SecretId::new(SecretKind::ServerApiKey, server.key())?)
            .await?;
    }

    let server_id = {
        let db = Arc::clone(&db);
        let address = server.key();
        tokio::task::spawn_blocking(move || catalog::ensure_server(&db, &address, sync::now_ms()))
            .await??
    };

    let source = LibrarySource::new(probe);
    let report = sync::sync_server(&source, db, server_id).await?;
    Ok(Connected {
        server: server_id,
        info,
        report,
    })
}

/// Connects with an API key. See [`connect`].
pub async fn connect_with_api_key<B: SecretBackend>(
    settings: &HttpSettings,
    vault: &SecretVault<B>,
    db: Arc<Database>,
    server: ServerUrl,
    key: ApiKey,
) -> Result<Connected, ConnectError> {
    connect(settings, vault, db, server, Auth::ApiKey(key)).await
}

/// The credentials stored for a server: the paired device when there is one,
/// otherwise the API key. `None` when the server was never connected.
pub async fn saved_auth<B: SecretBackend>(
    vault: &SecretVault<B>,
    server: &ServerUrl,
) -> Result<Option<Auth>, ConnectError> {
    let device_id = SecretId::new(SecretKind::ServerDeviceKey, server.key())?;
    if let Some(secret) = vault.read(&device_id).await? {
        return Ok(Some(Auth::Device(DeviceKey::from_secret(&secret)?)));
    }
    let key_id = SecretId::new(SecretKind::ServerApiKey, server.key())?;
    match vault.read(&key_id).await? {
        Some(secret) => {
            let text = std::str::from_utf8(secret.expose_secret())
                .map_err(|_| ConnectError::BadStoredSecret)?;
            Ok(Some(Auth::ApiKey(ApiKey::new(text)?)))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    /// The name must match the OpenSubsonic extension folksonic advertises
    /// (`getOpenSubsonicExtensions`). It was once `apikeyauth`, which no server
    /// sends, so every API-key connection was refused.
    #[test]
    fn the_auth_extension_has_the_name_servers_advertise() {
        assert_eq!(AUTH_EXTENSION, "apiKeyAuthentication");
    }

    use super::*;
    use crate::connection::HttpPolicy;
    use crate::db::tests::temp_db;
    use crate::dpop::DeviceKey;
    use crate::secrets::BackendError;
    use secrecy::ExposeSecret;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Keeps secrets in memory and records what was stored.
    #[derive(Default)]
    struct MemoryBackend {
        items: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl SecretBackend for MemoryBackend {
        async fn store(&self, id: &SecretId, secret: &SecretSlice<u8>) -> Result<(), BackendError> {
            self.items
                .lock()
                .unwrap()
                .insert(id.label(), secret.expose_secret().to_vec());
            Ok(())
        }

        async fn read(&self, id: &SecretId) -> Result<Option<SecretSlice<u8>>, BackendError> {
            Ok(self
                .items
                .lock()
                .unwrap()
                .get(&id.label())
                .map(|v| SecretSlice::from(v.clone())))
        }

        async fn delete(&self, id: &SecretId) -> Result<(), BackendError> {
            self.items.lock().unwrap().remove(&id.label());
            Ok(())
        }
    }

    fn settings() -> HttpSettings {
        HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        }
    }

    fn envelope(payload: serde_json::Value) -> ResponseTemplate {
        let mut body = json!({"status": "ok", "version": "1.16.1", "type": "folksonic",
            "serverVersion": "test"});
        body.as_object_mut()
            .unwrap()
            .extend(payload.as_object().unwrap().clone());
        ResponseTemplate::new(200).set_body_json(json!({ "subsonic-response": body }))
    }

    async fn mount_server(server: &MockServer, extensions: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/rest/getOpenSubsonicExtensions.view"))
            .respond_with(envelope(json!({"openSubsonicExtensions": extensions})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/getLibraryChanges.view"))
            .and(query_param_is_missing("cursor"))
            .respond_with(envelope(json!({"libraryChanges": {
                "full": true, "serverTime": 42,
                "artists": [{"id": "ar-1", "item": {"id": "ar-1", "name": "Artist"}}],
                "albums": [], "songs": [], "playlists": [], "deleted": []}})))
            .mount(server)
            .await;
    }

    fn full_extensions() -> serde_json::Value {
        json!([
            {"name": "apiKeyAuthentication", "versions": [1]},
            {"name": "tinysonicLibraryChanges", "versions": [1, 2]}
        ])
    }

    #[tokio::test]
    async fn a_good_server_is_stored_and_synced() {
        let server = MockServer::start().await;
        mount_server(&server, full_extensions()).await;
        let (dir, db) = temp_db();
        let db = Arc::new(db);
        let vault = SecretVault::with_backend(MemoryBackend::default());
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let key = ApiKey::new("tsk_connect_test_key").unwrap();

        let connected =
            connect_with_api_key(&settings(), &vault, Arc::clone(&db), url.clone(), key)
                .await
                .unwrap();

        assert_eq!(connected.info.server_type.as_deref(), Some("folksonic"));
        assert_eq!(connected.report.upserted, 1);
        let stored = vault
            .read(&SecretId::new(SecretKind::ServerApiKey, url.key()).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.expose_secret(), b"tsk_connect_test_key");

        // The key must not be written to the database file.
        let file = std::fs::read(db.path()).unwrap();
        assert!(!file.windows(19).any(|w| w == b"tsk_connect_test_key"));
        drop(dir);
    }

    #[tokio::test]
    async fn a_server_without_api_key_authentication_is_refused_and_stores_nothing() {
        let server = MockServer::start().await;
        mount_server(
            &server,
            json!([{"name": "tinysonicLibraryChanges", "versions": [1]}]),
        )
        .await;
        let (_dir, db) = temp_db();
        let vault = SecretVault::with_backend(MemoryBackend::default());
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let result = connect_with_api_key(
            &settings(),
            &vault,
            Arc::new(db),
            url.clone(),
            ApiKey::new("tsk_x").unwrap(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ConnectError::Api(ApiError::Missing("apiKeyAuthentication")))
        ));
        assert!(
            vault
                .read(&SecretId::new(SecretKind::ServerApiKey, url.key()).unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_server_without_the_change_feed_is_refused() {
        let server = MockServer::start().await;
        mount_server(
            &server,
            json!([{"name": "apiKeyAuthentication", "versions": [1]}]),
        )
        .await;
        let (_dir, db) = temp_db();
        let vault = SecretVault::with_backend(MemoryBackend::default());
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let result = connect_with_api_key(
            &settings(),
            &vault,
            Arc::new(db),
            url,
            ApiKey::new("tsk_x").unwrap(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ConnectError::Api(ApiError::Missing(FEED_EXTENSION)))
        ));
    }

    #[tokio::test]
    async fn a_failed_status_from_the_server_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/getOpenSubsonicExtensions.view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "subsonic-response": {"status": "failed",
                    "error": {"code": 40, "message": "Wrong username or password"}}})))
            .mount(&server)
            .await;
        let (_dir, db) = temp_db();
        let vault = SecretVault::with_backend(MemoryBackend::default());
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let result = connect_with_api_key(
            &settings(),
            &vault,
            Arc::new(db),
            url,
            ApiKey::new("tsk_x").unwrap(),
        )
        .await;
        match result {
            Err(ConnectError::Api(ApiError::Refused { code, message })) => {
                assert_eq!(code, Some(40));
                assert!(message.contains("Wrong username"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn a_paired_device_replaces_the_api_key_and_signs_every_request() {
        let server = MockServer::start().await;
        mount_server(&server, full_extensions()).await;
        let (_dir, db) = temp_db();
        let db = Arc::new(db);
        let vault = SecretVault::with_backend(MemoryBackend::default());
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();

        // An API key from an earlier connection is replaced by the device.
        connect_with_api_key(
            &settings(),
            &vault,
            Arc::clone(&db),
            url.clone(),
            ApiKey::new("tsk_old_key").unwrap(),
        )
        .await
        .unwrap();
        let requests_before_device = server.received_requests().await.unwrap().len();
        let device = DeviceKey::generate().unwrap();
        connect(
            &settings(),
            &vault,
            Arc::clone(&db),
            url.clone(),
            Auth::Device(device.clone()),
        )
        .await
        .unwrap();

        assert!(
            vault
                .read(&SecretId::new(SecretKind::ServerApiKey, url.key()).unwrap())
                .await
                .unwrap()
                .is_none(),
            "the old API key must not stay in the Secret Service"
        );
        match saved_auth(&vault, &url).await.unwrap() {
            Some(Auth::Device(restored)) => {
                assert_eq!(restored.thumbprint(), device.thumbprint());
            }
            other => panic!("expected the device, got {other:?}"),
        }

        let requests = server.received_requests().await.unwrap();
        assert!(requests.len() > requests_before_device);
        for request in &requests[requests_before_device..] {
            assert!(request.headers.get("DPoP").is_some(), "{}", request.url);
            assert!(request.headers.get("X-Api-Key").is_none());
            assert!(!request.url.to_string().contains("apiKey"));
        }
    }

    #[tokio::test]
    async fn saved_auth_prefers_the_device_and_is_none_for_unknown_servers() {
        let vault = SecretVault::with_backend(MemoryBackend::default());
        let url = ServerUrl::parse("https://music.example", HttpPolicy::HTTPS_ONLY).unwrap();
        assert!(saved_auth(&vault, &url).await.unwrap().is_none());

        vault
            .store(
                &SecretId::new(SecretKind::ServerApiKey, url.key()).unwrap(),
                &SecretSlice::from(b"tsk_stored".to_vec()),
            )
            .await
            .unwrap();
        match saved_auth(&vault, &url).await.unwrap() {
            Some(Auth::ApiKey(key)) => assert_eq!(key.expose(), "tsk_stored"),
            other => panic!("expected the API key, got {other:?}"),
        }

        vault
            .store(
                &SecretId::new(SecretKind::ServerDeviceKey, url.key()).unwrap(),
                &DeviceKey::generate().unwrap().secret(),
            )
            .await
            .unwrap();
        assert!(matches!(
            saved_auth(&vault, &url).await.unwrap(),
            Some(Auth::Device(_))
        ));
    }
}
