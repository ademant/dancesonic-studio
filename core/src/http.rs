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

#[cfg(test)]
mod tests {
    use super::*;

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
