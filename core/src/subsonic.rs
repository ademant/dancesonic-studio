//! The OpenSubsonic REST client for folksonic (and other Subsonic servers).
//!
//! Every request carries `f=json`, `c=DanceSonicStudio` and `v`. Credentials
//! are sent in headers only, never in the URL. Token auth (`u`/`t`/`s`) and
//! the plain `p=` parameter are not implemented on purpose.

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::connection::ServerUrl;
use crate::dpop::DeviceKey;

/// Client name sent as `c`.
pub const CLIENT_NAME: &str = "DanceSonicStudio";

/// The Subsonic API version this client implements, sent as `v`.
pub const API_VERSION: &str = "1.16.1";

/// Largest answer read from a server, in bytes. Server JSON is untrusted;
/// this bounds memory use whatever the server sends.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Longest error message from a server that is shown or kept, in bytes.
const MAX_MESSAGE_BYTES: usize = 200;

/// A device or user API key (`X-Api-Key`).
///
/// Held in a [`SecretString`]: `Debug` prints `[REDACTED]`, and the value is
/// zeroed when dropped.
#[derive(Clone)]
pub struct ApiKey(SecretString);

/// Why an API key string was refused. Carries no part of the key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApiKeyError {
    #[error("the API key is empty")]
    Empty,
    #[error("the API key is too long")]
    TooLong,
    #[error("the API key contains spaces or control characters")]
    BadCharacters,
}

impl ApiKey {
    /// Longest key accepted. Keys are short tokens; anything near this long
    /// is not a key.
    pub const MAX_LEN: usize = 512;

    /// Checks a key typed or received from a server.
    pub fn new(value: impl Into<String>) -> Result<Self, ApiKeyError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ApiKeyError::Empty);
        }
        if value.len() > Self::MAX_LEN {
            return Err(ApiKeyError::TooLong);
        }
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ApiKeyError::BadCharacters);
        }
        Ok(Self(SecretString::from(value)))
    }

    /// The key itself. Use only to build a request header.
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey([REDACTED])")
    }
}

/// How a request is authenticated.
#[derive(Clone, Debug)]
pub enum Auth {
    /// The key in the `X-Api-Key` header.
    ApiKey(ApiKey),
    /// A paired device: a DPoP proof per request, signed with the device key.
    /// No bearer secret is sent.
    Device(DeviceKey),
}

/// Errors from a server call. Messages never contain keys or full URLs.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("the server could not be reached")]
    Unreachable,
    #[error("the server did not answer in time")]
    Timeout,
    #[error("the server refused the request (HTTP {0})")]
    Http(u16),
    #[error("the server refused the request: {message}")]
    Refused {
        /// The Subsonic error code, when the server sent one.
        code: Option<u32>,
        message: String,
    },
    #[error("the server sent an invalid answer: {0}")]
    Invalid(&'static str),
    #[error("the server answer is larger than {MAX_RESPONSE_BYTES} bytes")]
    TooLarge,
    #[error("the server does not offer {0}")]
    Missing(&'static str),
    #[error("the device key could not sign the request")]
    Signing,
}

/// What the server says about itself, from `getOpenSubsonicExtensions`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// Server software name, such as `folksonic` or `navidrome`.
    pub server_type: Option<String>,
    pub server_version: Option<String>,
    /// Extension names the server advertises.
    pub extensions: Vec<String>,
}

impl ServerInfo {
    /// Whether the server advertises an extension.
    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.iter().any(|e| e == name)
    }
}

/// A client for one server, with the credentials it uses.
#[derive(Clone)]
pub struct SubsonicClient {
    http: reqwest::Client,
    server: ServerUrl,
    auth: Auth,
}

impl std::fmt::Debug for SubsonicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubsonicClient")
            .field("server", &self.server)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

/// A server answer together with the envelope fields around it.
#[derive(Debug)]
pub struct Answer<T> {
    pub server_type: Option<String>,
    pub server_version: Option<String>,
    pub payload: T,
}

impl SubsonicClient {
    /// A client for `server`, built on a client from [`crate::http::client`].
    pub fn new(http: reqwest::Client, server: ServerUrl, auth: Auth) -> Self {
        Self { http, server, auth }
    }

    /// The server this client talks to.
    pub fn server(&self) -> &ServerUrl {
        &self.server
    }

    /// Asks the server what it offers. Needs no sign-in.
    pub async fn server_info(&self) -> Result<ServerInfo, ApiError> {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase")]
        struct Payload {
            #[serde(default)]
            open_subsonic_extensions: Vec<ExtensionDto>,
        }
        #[derive(Deserialize)]
        struct ExtensionDto {
            name: String,
        }

        let answer: Answer<Payload> = self.get("getOpenSubsonicExtensions", &[]).await?;
        Ok(ServerInfo {
            server_type: answer.server_type,
            server_version: answer.server_version,
            extensions: answer
                .payload
                .open_subsonic_extensions
                .into_iter()
                .map(|e| e.name)
                .collect(),
        })
    }

    /// One GET call to `rest/<endpoint>.view`, parsed as a Subsonic answer.
    ///
    /// `query` holds the endpoint's own parameters; the standard ones are added
    /// here. The key goes in a header, never in the URL.
    pub async fn get<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        query: &[(&str, &str)],
    ) -> Result<Answer<T>, ApiError> {
        let mut url = self
            .server
            .endpoint(&format!("rest/{endpoint}.view"))
            .map_err(|_| ApiError::Invalid("the endpoint address"))?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("f", "json");
            pairs.append_pair("c", CLIENT_NAME);
            pairs.append_pair("v", API_VERSION);
            for (name, value) in query {
                pairs.append_pair(name, value);
            }
        }
        let credential = match &self.auth {
            Auth::ApiKey(key) => ("X-Api-Key", key.expose().to_owned()),
            Auth::Device(device) => {
                let proof = device
                    .proof("GET", &url, crate::sync::now_ms() / 1000)
                    .map_err(|_| ApiError::Signing)?;
                ("DPoP", proof)
            }
        };
        let request = self.http.get(url).header(credential.0, credential.1);
        let response = request.send().await.map_err(map_transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ApiError::Http(status.as_u16()));
        }
        let body = read_limited(response).await?;
        parse_answer(&body)
    }
}

/// Maps a transport error without its URL, so no key-bearing address reaches
/// an error message or log.
fn map_transport(error: reqwest::Error) -> ApiError {
    if error.is_timeout() {
        ApiError::Timeout
    } else {
        // `without_url` removes the address from the error's text.
        tracing::debug!(error = %error.without_url(), "request failed");
        ApiError::Unreachable
    }
}

/// Reads a response body up to [`MAX_RESPONSE_BYTES`].
async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, ApiError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ApiError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_transport)? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(ApiError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The Subsonic JSON envelope: `{"subsonic-response": {...}}`.
#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(rename = "subsonic-response")]
    response: Response<T>,
}

#[derive(Deserialize)]
struct Response<T> {
    status: String,
    #[serde(default, rename = "type")]
    server_type: Option<String>,
    #[serde(default, rename = "serverVersion")]
    server_version: Option<String>,
    #[serde(default)]
    error: Option<ErrorDto>,
    #[serde(flatten)]
    payload: T,
}

#[derive(Deserialize)]
struct ErrorDto {
    #[serde(default)]
    code: Option<u32>,
    #[serde(default)]
    message: Option<String>,
}

/// Parses an answer body. A `status` of `failed` becomes [`ApiError::Refused`].
pub(crate) fn parse_answer<T: serde::de::DeserializeOwned>(
    body: &[u8],
) -> Result<Answer<T>, ApiError> {
    let envelope: Envelope<T> =
        serde_json::from_slice(body).map_err(|_| ApiError::Invalid("not Subsonic JSON"))?;
    let response = envelope.response;
    match response.status.as_str() {
        "ok" => Ok(Answer {
            server_type: response.server_type,
            server_version: response.server_version,
            payload: response.payload,
        }),
        "failed" => {
            let (code, message) = response.error.map_or((None, None), |e| (e.code, e.message));
            Err(ApiError::Refused {
                code,
                message: clean_message(message.as_deref()),
            })
        }
        _ => Err(ApiError::Invalid("unknown status")),
    }
}

/// A server message made safe to show: no control characters, bounded length.
fn clean_message(message: Option<&str>) -> String {
    let text: String = message
        .unwrap_or("no message")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    truncate_to_bytes(&text, MAX_MESSAGE_BYTES)
}

/// Cuts a string to at most `max` bytes, on a character boundary.
pub(crate) fn truncate_to_bytes(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Empty {}

    #[test]
    fn api_keys_are_checked_and_redacted() {
        assert!(matches!(ApiKey::new(""), Err(ApiKeyError::Empty)));
        assert!(matches!(
            ApiKey::new("a b"),
            Err(ApiKeyError::BadCharacters)
        ));
        assert!(matches!(
            ApiKey::new("a\nb"),
            Err(ApiKeyError::BadCharacters)
        ));
        assert!(matches!(
            ApiKey::new("x".repeat(ApiKey::MAX_LEN + 1)),
            Err(ApiKeyError::TooLong)
        ));
        let key = ApiKey::new("tsk_secret_value").unwrap();
        assert_eq!(key.expose(), "tsk_secret_value");
        assert!(!format!("{key:?}").contains("secret_value"));
    }

    #[test]
    fn ok_answers_return_the_payload_and_server_fields() {
        let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1",
            "type":"folksonic","serverVersion":"1.2.3","openSubsonicExtensions":[]}}"#;
        let answer: Answer<Empty> = parse_answer(body).unwrap();
        assert_eq!(answer.server_type.as_deref(), Some("folksonic"));
        assert_eq!(answer.server_version.as_deref(), Some("1.2.3"));
        assert_eq!(answer.payload, Empty {});
    }

    #[test]
    fn failed_status_becomes_a_refusal_with_the_server_message() {
        let body = br#"{"subsonic-response":{"status":"failed",
            "error":{"code":40,"message":"Wrong username or password\u0007"}}}"#;
        match parse_answer::<Empty>(body) {
            Err(ApiError::Refused { code, message }) => {
                assert_eq!(code, Some(40));
                assert_eq!(message, "Wrong username or password");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn failed_status_without_error_object_still_refuses() {
        let body = br#"{"subsonic-response":{"status":"failed"}}"#;
        assert!(matches!(
            parse_answer::<Empty>(body),
            Err(ApiError::Refused { code: None, .. })
        ));
    }

    #[test]
    fn invalid_bodies_are_refused_without_detail() {
        assert!(matches!(
            parse_answer::<Empty>(b"<html>"),
            Err(ApiError::Invalid(_))
        ));
        assert!(matches!(
            parse_answer::<Empty>(br#"{"subsonic-response":{"status":"maybe"}}"#),
            Err(ApiError::Invalid(_))
        ));
    }

    #[test]
    fn long_messages_are_cut_on_a_character_boundary() {
        let long = "ä".repeat(300);
        let cut = clean_message(Some(&long));
        assert!(cut.len() <= MAX_MESSAGE_BYTES);
        assert!(cut.chars().all(|c| c == 'ä'));
    }
}
