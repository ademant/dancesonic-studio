//! The HTTP client every server request goes through.
//!
//! One place sets the rules: https only unless the warned opt-in allows
//! private http, no redirects, timeouts on every call, no system proxy (a
//! proxy would see the traffic to the server), and an optional pinned
//! certificate for servers with a self-signed certificate.

use std::time::Duration;

use reqwest::{Certificate, redirect};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;

use crate::connection::HttpPolicy;

/// Longest a whole request may take, including reading the answer.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a connection may take to open.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from building the client. Carry no address and no key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HttpError {
    #[error("the pinned certificate is not a valid PEM certificate")]
    BadPin,
    #[error("the HTTP client could not be started")]
    Build,
}

/// How to talk to one server.
#[derive(Clone, Debug, Default)]
pub struct HttpSettings {
    /// Whether plain http is allowed (the warned opt-in).
    pub policy: HttpPolicy,
    /// PEM certificate the server must present. When set, only this
    /// certificate is trusted, so a server that presents any other
    /// certificate is refused, whatever a public CA would say.
    pub pinned_certificate_pem: Option<Vec<u8>>,
}

/// Reads the pinned certificates. Every PEM block must be a certificate, and
/// there must be at least one; anything else is refused rather than ignored,
/// so a bad pin can never silently turn into no pin.
fn pinned_certificates(pem: &[u8]) -> Result<Vec<Certificate>, HttpError> {
    let blocks: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<_, _>>()
        .map_err(|_| HttpError::BadPin)?;
    if blocks.is_empty() {
        return Err(HttpError::BadPin);
    }
    blocks
        .iter()
        .map(|der| Certificate::from_der(der.as_ref()).map_err(|_| HttpError::BadPin))
        .collect()
}

/// Builds the client for one server.
pub fn client(settings: &HttpSettings) -> Result<reqwest::Client, HttpError> {
    let mut builder = reqwest::Client::builder()
        .https_only(!settings.policy.allows_plain_http())
        .redirect(redirect::Policy::none())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("DanceSonicStudio/", env!("CARGO_PKG_VERSION")));
    if let Some(pem) = &settings.pinned_certificate_pem {
        builder = builder.tls_certs_only(pinned_certificates(pem)?);
    }
    builder.build().map_err(|_| HttpError::Build)
}

/// Largest answer read from the sign-in and pairing endpoints, success or
/// error. Their real answers are a few hundred bytes; the limit only keeps a
/// broken or hostile server from filling memory.
pub const MAX_AUTH_BODY_BYTES: usize = 64 * 1024;

/// Errors reading an answer body. Carry no body content.
#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("the server answer is larger than {MAX_AUTH_BODY_BYTES} bytes")]
    TooLarge,
    #[error(transparent)]
    Transport(reqwest::Error),
}

/// Reads a whole answer body, refusing it once it passes `max` bytes. The size
/// is checked before any more memory is taken, so a chunked answer with no
/// `Content-Length` is stopped at the limit too.
pub async fn read_body(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(BodyError::Transport)? {
        // `body.len() <= max` always holds, so the subtraction cannot overflow.
        if chunk.len() > max - body.len() {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serves one chunked answer of `body` on a local port and returns its
    /// URL. The answer has no `Content-Length`, so only the chunk limit applies.
    async fn chunked_server(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for chunk in body.chunks(8 * 1024) {
                socket
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await
                    .unwrap();
                socket.write_all(chunk).await.unwrap();
                socket.write_all(b"\r\n").await.unwrap();
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        });
        format!("http://{address}/answer")
    }

    fn plain_client() -> reqwest::Client {
        client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_chunked_answer_within_the_limit_is_read_whole() {
        let url = chunked_server(vec![b'a'; 20_000]).await;
        let response = plain_client().get(url).send().await.unwrap();
        let body = read_body(response, 32 * 1024).await.unwrap();
        assert_eq!(body.len(), 20_000);
    }

    #[tokio::test]
    async fn a_chunked_answer_over_the_limit_is_refused() {
        let url = chunked_server(vec![b'a'; 20_000]).await;
        let response = plain_client().get(url).send().await.unwrap();
        assert!(matches!(
            read_body(response, 10_000).await,
            Err(BodyError::TooLarge)
        ));
    }

    #[test]
    fn default_client_builds() {
        assert!(client(&HttpSettings::default()).is_ok());
    }

    #[test]
    fn plain_http_client_builds_with_the_opt_in() {
        let settings = HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        };
        assert!(client(&settings).is_ok());
    }

    #[test]
    fn a_pin_that_is_not_a_certificate_is_refused() {
        let settings = HttpSettings {
            policy: HttpPolicy::HTTPS_ONLY,
            pinned_certificate_pem: Some(b"not a certificate".to_vec()),
        };
        assert_eq!(client(&settings).err(), Some(HttpError::BadPin));
    }
}
