//! Certificate pinning against a real TLS handshake.
//!
//! Setup failures here are test failures, so `unwrap` is allowed in this file.
#![allow(clippy::unwrap_used)]
//!
//! A local HTTPS server presents a self-signed certificate. A client that pins
//! that certificate must reach it; one that pins another certificate, or that
//! trusts only the system roots, must be refused.

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use studio_core::connection::{HttpPolicy, ServerUrl};
use studio_core::http::{HttpSettings, client};
use studio_core::subsonic::{ApiError, ApiKey, Auth, SubsonicClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const BODY: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1",
    "openSubsonicExtensions":[{"name":"apikeyauth","versions":[1]}]}}"#;

/// A self-signed certificate for `localhost`, with its key and PEM.
struct Identity {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    pem: String,
}

fn identity() -> Identity {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    Identity {
        cert_der: certified.cert.der().clone(),
        key_der: PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
        pem: certified.cert.pem(),
    }
}

/// Serves a fixed JSON answer over TLS with `identity` on a free local port.
async fn serve(identity: &Identity) -> u16 {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut request = [0u8; 4096];
                let _ = tls.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
                    BODY.len()
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

fn client_on(port: u16, pin: Option<String>) -> SubsonicClient {
    let http = client(&HttpSettings {
        policy: HttpPolicy::HTTPS_ONLY,
        pinned_certificate_pem: pin.map(String::into_bytes),
    })
    .unwrap();
    let server =
        ServerUrl::parse(&format!("https://localhost:{port}"), HttpPolicy::HTTPS_ONLY).unwrap();
    SubsonicClient::new(
        http,
        server,
        Auth::ApiKey(ApiKey::new("tsk_pin_test").unwrap()),
    )
}

#[tokio::test]
async fn a_pinned_certificate_is_accepted() {
    let id = identity();
    let port = serve(&id).await;
    let info = client_on(port, Some(id.pem.clone()))
        .server_info()
        .await
        .unwrap();
    assert!(info.has_extension("apikeyauth"));
}

#[tokio::test]
async fn a_different_pinned_certificate_is_refused() {
    let served = identity();
    let other = identity();
    let port = serve(&served).await;
    let result = client_on(port, Some(other.pem.clone())).server_info().await;
    assert!(matches!(result, Err(ApiError::Unreachable)), "{result:?}");
}

#[tokio::test]
async fn without_a_pin_an_unknown_certificate_is_refused() {
    let served = identity();
    let port = serve(&served).await;
    let result = client_on(port, None).server_info().await;
    assert!(matches!(result, Err(ApiError::Unreachable)), "{result:?}");
}
