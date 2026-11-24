//! Pairing this desktop with a folksonic server (device pairing, #4).
//!
//! The user pastes the pairing link from the account page (or the bare
//! token). This module sends the desktop's public key with the token, both
//! sides show the same verification code, and the pairing completes only when
//! both have confirmed. Requests afterwards are signed with the device key
//! (see [`crate::dpop`]).
//!
//! The private key is returned to the caller only after the server has
//! confirmed the pairing; the caller stores it in the Secret Service.

use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::connection::{HttpPolicy, ServerUrl, UrlError};
use crate::dpop::{DeviceKey, DpopError, verification_code};
use crate::http::{BodyError, MAX_AUTH_BODY_BYTES, read_body};

/// Name sent for the device; shown on the account page.
pub const DEVICE_NAME: &str = "DanceSonic Studio";

/// Longest pairing token accepted, in bytes.
pub const MAX_TOKEN_BYTES: usize = 256;

/// Wait between checks on the owner's confirmation: 3 s, 5 s, then 10 s.
pub const POLL_DELAYS: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// Longest a pairing waits for the owner when the server gives no expiry.
const MAX_WAIT: Duration = Duration::from_secs(15 * 60);

/// A pairing token, checked at the boundary.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingToken(String);

/// What a pasted pairing input contained.
#[derive(Debug, PartialEq, Eq)]
pub struct PairingInput {
    /// The server the link points at, when the link names one. A bare token
    /// names none, and the user then supplies the address.
    pub server: Option<ServerUrl>,
    pub token: PairingToken,
}

/// Why a pasted pairing input was refused. Carries no part of the input.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InputError {
    #[error("nothing was pasted")]
    Empty,
    #[error("the pairing token is not valid")]
    BadToken,
    #[error("the pairing link names a server address that is not allowed")]
    BadServer(#[source] UrlError),
}

impl PairingToken {
    /// Checks a token. Tokens are short ASCII words with `_` and `-`.
    pub fn new(value: &str) -> Result<Self, InputError> {
        let ok = !value.is_empty()
            && value.len() <= MAX_TOKEN_BYTES
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(InputError::BadToken)
        }
    }

    /// The token itself. Use only to send it to the server or derive the code.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PairingToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingToken([REDACTED])")
    }
}

/// Reads what the user pasted: a `https://…/pair#token=…` link from the
/// account page, a `tinysonic://pair?…` link, or the bare token.
pub fn parse_input(input: &str, policy: HttpPolicy) -> Result<PairingInput, InputError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(InputError::Empty);
    }
    if text.starts_with("tinysonic://") {
        return parse_app_link(text, policy);
    }
    if let Some(url) = Url::parse(text)
        .ok()
        .filter(|url| matches!(url.scheme(), "https" | "http"))
    {
        return parse_web_link(&url, policy);
    }
    Ok(PairingInput {
        server: None,
        token: PairingToken::new(text)?,
    })
}

fn parse_app_link(text: &str, policy: HttpPolicy) -> Result<PairingInput, InputError> {
    let url = Url::parse(text).map_err(|_| InputError::BadToken)?;
    if url.host_str() != Some("pair") {
        return Err(InputError::BadToken);
    }
    let mut server = None;
    let mut token = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "server" => server = Some(value.into_owned()),
            "token" => token = Some(value.into_owned()),
            _ => {}
        }
    }
    let token = PairingToken::new(&token.ok_or(InputError::BadToken)?)?;
    let server = match server {
        Some(address) => Some(ServerUrl::parse(&address, policy).map_err(InputError::BadServer)?),
        None => None,
    };
    Ok(PairingInput { server, token })
}

fn parse_web_link(url: &Url, policy: HttpPolicy) -> Result<PairingInput, InputError> {
    let token_text = url
        .fragment()
        .and_then(|fragment| {
            url::form_urlencoded::parse(fragment.as_bytes())
                .find(|(name, _)| name == "token")
                .map(|(_, value)| value.into_owned())
        })
        .ok_or(InputError::BadToken)?;
    let token = PairingToken::new(&token_text)?;

    // The link is `<server>/pair#token=…`; the server is everything before `/pair`.
    let path = url.path().trim_end_matches('/');
    let base_path = path.strip_suffix("/pair").ok_or(InputError::BadToken)?;
    let mut base = url.clone();
    base.set_path(&format!("{base_path}/"));
    base.set_fragment(None);
    base.set_query(None);
    let server = ServerUrl::parse(base.as_str(), policy).map_err(InputError::BadServer)?;
    Ok(PairingInput {
        server: Some(server),
        token,
    })
}

/// What the server reports about a paired device.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DeviceInfo {
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
}

/// Errors from pairing. Carry no tokens and no keys.
#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("the pairing code has expired; create a new pairing link on the account page")]
    Expired,
    #[error("the pairing was rejected on the account page")]
    Rejected,
    #[error("this pairing link was already used")]
    TokenUsed,
    #[error("the server is busy with this link; try again in a moment")]
    Busy,
    #[error("the device was removed from the account")]
    DeviceUnknown,
    #[error("the server did not accept the device's signature")]
    ProofRefused,
    #[error("the server could not be reached")]
    Unreachable,
    #[error("the server did not answer in time")]
    Timeout,
    #[error("the server sent an invalid answer")]
    Invalid,
    #[error("the server sent an answer that is too large")]
    TooLarge,
    #[error("the server refused the request (HTTP {0})")]
    Http(u16),
    #[error(transparent)]
    Key(#[from] DpopError),
}

/// A pairing that has been started and waits for both confirmations.
#[derive(Debug)]
pub struct PendingPairing {
    server: ServerUrl,
    token: PairingToken,
    device: DeviceKey,
    code: String,
    expires_at_ms: Option<i64>,
}

/// The server's answer to our confirmation.
#[derive(Debug, PartialEq, Eq)]
pub enum Confirmation {
    /// Our side is recorded; the owner still has to confirm on the account page.
    WaitingForOwner,
    /// Both sides confirmed. The device is paired.
    Paired(DeviceInfo),
}

#[derive(Deserialize)]
struct PendingDto {
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<String>,
}

#[derive(Deserialize)]
struct PairedDto {
    #[serde(default)]
    device: Option<DeviceInfo>,
}

impl PendingPairing {
    /// The six-digit code both sides must show before confirming.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// When the server says the pairing expires, in epoch milliseconds.
    pub fn expires_at_ms(&self) -> Option<i64> {
        self.expires_at_ms
    }

    /// The server the pairing is with.
    pub fn server(&self) -> &ServerUrl {
        &self.server
    }

    /// The device key. Store it only after the pairing completed.
    pub fn device_key(&self) -> &DeviceKey {
        &self.device
    }

    /// Starts a pairing: sends the public key with the token.
    ///
    /// `http` must come from [`crate::http::client`] for this server.
    pub async fn begin(
        http: &reqwest::Client,
        server: ServerUrl,
        token: PairingToken,
    ) -> Result<Self, PairingError> {
        let device = DeviceKey::generate()?;
        let jwk = device.public_jwk();
        let url = server
            .endpoint("api/pair/complete")
            .map_err(|_| PairingError::Invalid)?;
        let response = http
            .post(url)
            .json(&serde_json::json!({
                "token": token.expose(),
                "device": DEVICE_NAME,
                "jwk": jwk,
            }))
            .send()
            .await
            .map_err(map_transport)?;
        let status = response.status().as_u16();
        match status {
            202 => {}
            401 => return Err(PairingError::Expired),
            409 => return Err(PairingError::TokenUsed),
            429 => return Err(PairingError::Busy),
            other => return Err(PairingError::Http(other)),
        }
        let body: PendingDto = read_json(response).await?;
        let expires_at_ms = body.expires_at.as_deref().and_then(parse_rfc3339_ms);
        let code = verification_code(token.expose(), &device.thumbprint());
        Ok(Self {
            server,
            token,
            device,
            code,
            expires_at_ms,
        })
    }

    /// Records this side's answer to the verification code. `matches` is
    /// true only when the code shown here equals the one on the account page.
    pub async fn answer(
        &self,
        http: &reqwest::Client,
        matches: bool,
    ) -> Result<Confirmation, PairingError> {
        let url = self
            .server
            .endpoint("api/pair/confirm")
            .map_err(|_| PairingError::Invalid)?;
        let result = if matches { "match" } else { "cancel" };
        let proof = self.device.proof("POST", &url, now_secs())?;
        let response = http
            .post(url)
            .header("DPoP", proof)
            .json(&serde_json::json!({ "result": result }))
            .send()
            .await
            .map_err(map_transport)?;
        match response.status().as_u16() {
            200 => {
                let body: PairedDto = read_json(response).await?;
                let device = body.device.ok_or(PairingError::Invalid)?;
                Ok(Confirmation::Paired(device))
            }
            202 => Ok(Confirmation::WaitingForOwner),
            401 => Err(PairingError::ProofRefused),
            404 => Err(PairingError::DeviceUnknown),
            409 => Err(PairingError::Rejected),
            410 => Err(PairingError::Expired),
            other => Err(PairingError::Http(other)),
        }
    }

    /// Waits until the owner has confirmed on the account page, checking with
    /// the device key. Returns once the server accepts the device.
    pub async fn wait_for_owner(&self, http: &reqwest::Client) -> Result<DeviceInfo, PairingError> {
        self.wait_for_owner_with(http, &POLL_DELAYS).await
    }

    /// As [`Self::wait_for_owner`], with the wait between checks given explicitly.
    pub async fn wait_for_owner_with(
        &self,
        http: &reqwest::Client,
        delays: &[Duration],
    ) -> Result<DeviceInfo, PairingError> {
        let started = std::time::Instant::now();
        let mut attempt = 0usize;
        loop {
            if let Some(device) = self.check_device(http).await? {
                return Ok(device);
            }
            if self.expired_now() || started.elapsed() >= MAX_WAIT {
                return Err(PairingError::Expired);
            }
            let delay = delays
                .get(attempt)
                .or_else(|| delays.last())
                .copied()
                .unwrap_or(Duration::from_secs(10));
            attempt += 1;
            tokio::time::sleep(delay).await;
        }
    }

    /// One check: `Some` when paired, `None` while the owner has not confirmed.
    async fn check_device(
        &self,
        http: &reqwest::Client,
    ) -> Result<Option<DeviceInfo>, PairingError> {
        let url = self
            .server
            .endpoint("api/device/me")
            .map_err(|_| PairingError::Invalid)?;
        let proof = self.device.proof("GET", &url, now_secs())?;
        let response = http
            .get(url)
            .header("DPoP", proof)
            .send()
            .await
            .map_err(map_transport)?;
        match response.status().as_u16() {
            200 => Ok(Some(read_json(response).await?)),
            401 => {
                let bytes = read_answer(response).await?;
                let text = String::from_utf8_lossy(&bytes);
                if text.contains("pairing_rejected") {
                    Err(PairingError::Rejected)
                } else if text.contains("pairing_expired") {
                    Err(PairingError::Expired)
                } else if text.contains("device_unknown") {
                    Err(PairingError::DeviceUnknown)
                } else if text.contains("proof_invalid") || text.contains("proof_replayed") {
                    Err(PairingError::ProofRefused)
                } else {
                    // `pairing_pending`, or no reason given: keep waiting.
                    Ok(None)
                }
            }
            404 => Err(PairingError::DeviceUnknown),
            other => Err(PairingError::Http(other)),
        }
    }

    fn expired_now(&self) -> bool {
        self.expires_at_ms.is_some_and(|ms| now_ms() >= ms)
    }

    /// The pairing token; kept so the caller can show or discard it.
    pub fn token(&self) -> &PairingToken {
        &self.token
    }
}

fn now_secs() -> i64 {
    now_ms() / 1000
}

fn now_ms() -> i64 {
    crate::sync::now_ms()
}

/// Parses an RFC 3339 timestamp such as `2026-11-19T12:00:00Z` into epoch
/// milliseconds. Accepts `Z` and numeric offsets, with optional fractions.
pub(crate) fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let rest = &text[19..];
    let rest = match rest.strip_prefix('.') {
        Some(fraction) => fraction.trim_start_matches(|c: char| c.is_ascii_digit()),
        None => rest,
    };
    let offset_minutes = match rest {
        "Z" | "z" => 0,
        other => {
            let sign = match other.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hours: i64 = other.get(1..3)?.parse().ok()?;
            let minutes: i64 = other.get(4..6)?.parse().ok()?;
            sign * (hours * 60 + minutes)
        }
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1000)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn map_transport(error: reqwest::Error) -> PairingError {
    if error.is_timeout() {
        PairingError::Timeout
    } else {
        tracing::debug!(error = %error.without_url(), "pairing request failed");
        PairingError::Unreachable
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, PairingError> {
    let bytes = read_answer(response).await?;
    serde_json::from_slice(&bytes).map_err(|_| PairingError::Invalid)
}

/// Reads an answer body within [`MAX_AUTH_BODY_BYTES`].
async fn read_answer(response: reqwest::Response) -> Result<Vec<u8>, PairingError> {
    read_body(response, MAX_AUTH_BODY_BYTES)
        .await
        .map_err(|error| match error {
            BodyError::TooLarge => PairingError::TooLarge,
            BodyError::Transport(err) => map_transport(err),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_checked() {
        assert!(PairingToken::new("tsp_example-pairing-token").is_ok());
        assert_eq!(PairingToken::new("").err(), Some(InputError::BadToken));
        assert_eq!(PairingToken::new("a b").err(), Some(InputError::BadToken));
        assert_eq!(PairingToken::new("a/b").err(), Some(InputError::BadToken));
        assert_eq!(
            PairingToken::new(&"x".repeat(MAX_TOKEN_BYTES + 1)).err(),
            Some(InputError::BadToken)
        );
        assert!(!format!("{:?}", PairingToken::new("tsp_secret").unwrap()).contains("secret"));
    }

    #[test]
    fn a_web_link_from_the_account_page_gives_server_and_token() {
        let input = parse_input(
            "https://music.example/folksonic/pair#token=tsp_abc-123",
            HttpPolicy::HTTPS_ONLY,
        )
        .unwrap();
        assert_eq!(input.token.expose(), "tsp_abc-123");
        assert_eq!(
            input.server.unwrap().key(),
            "https://music.example/folksonic"
        );
    }

    #[test]
    fn a_web_link_at_the_root_names_the_origin() {
        let input = parse_input(
            "https://music.example/pair#token=tsp_1",
            HttpPolicy::HTTPS_ONLY,
        )
        .unwrap();
        assert_eq!(input.server.unwrap().key(), "https://music.example");
    }

    #[test]
    fn an_app_link_gives_server_and_token() {
        let input = parse_input(
            "tinysonic://pair?server=https%3A%2F%2Fmusic.example&token=tsp_xyz",
            HttpPolicy::HTTPS_ONLY,
        )
        .unwrap();
        assert_eq!(input.token.expose(), "tsp_xyz");
        assert_eq!(input.server.unwrap().key(), "https://music.example");
    }

    #[test]
    fn a_bare_token_names_no_server() {
        let input = parse_input("  tsp_bare  ", HttpPolicy::HTTPS_ONLY).unwrap();
        assert_eq!(input.token.expose(), "tsp_bare");
        assert!(input.server.is_none());
    }

    #[test]
    fn plain_http_links_need_the_opt_in_and_a_private_address() {
        assert!(matches!(
            parse_input(
                "http://192.168.1.5:4533/pair#token=tsp_1",
                HttpPolicy::HTTPS_ONLY
            ),
            Err(InputError::BadServer(UrlError::PlainHttpNeedsOptIn))
        ));
        assert!(
            parse_input(
                "http://192.168.1.5:4533/pair#token=tsp_1",
                HttpPolicy::ALLOW_PRIVATE_HTTP
            )
            .is_ok()
        );
        assert!(matches!(
            parse_input(
                "http://music.example/pair#token=tsp_1",
                HttpPolicy::ALLOW_PRIVATE_HTTP
            ),
            Err(InputError::BadServer(UrlError::PlainHttpNotPrivate))
        ));
    }

    #[test]
    fn the_pasted_token_never_reaches_debug_output() {
        let input = parse_input(
            "https://music.example/pair#token=tsp_must_not_leak",
            HttpPolicy::HTTPS_ONLY,
        )
        .unwrap();
        let text = format!("{input:?}");
        assert!(!text.contains("must_not_leak"), "{text}");
        assert!(text.contains("REDACTED"));
    }

    #[test]
    fn links_without_a_token_or_pair_path_are_refused() {
        assert_eq!(
            parse_input("https://music.example/pair", HttpPolicy::HTTPS_ONLY),
            Err(InputError::BadToken)
        );
        assert_eq!(
            parse_input(
                "https://music.example/other#token=tsp_1",
                HttpPolicy::HTTPS_ONLY
            ),
            Err(InputError::BadToken)
        );
        assert_eq!(
            parse_input("tinysonic://other?token=tsp_1", HttpPolicy::HTTPS_ONLY),
            Err(InputError::BadToken)
        );
        assert_eq!(
            parse_input("  ", HttpPolicy::HTTPS_ONLY),
            Err(InputError::Empty)
        );
    }

    #[test]
    fn rfc3339_timestamps_convert_to_epoch_milliseconds() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2000-03-01T00:00:00Z"),
            Some(951_868_800_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2000-03-01T01:00:00+01:00"),
            Some(951_868_800_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2000-03-01T00:00:00.123Z"),
            Some(951_868_800_000)
        );
        assert_eq!(parse_rfc3339_ms("garbage"), None);
    }
    mod flow {
        use super::*;
        use crate::connection::HttpPolicy;
        use crate::http::{HttpSettings, client};
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use serde_json::json;
        use wiremock::matchers::{body_partial_json, header_exists, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const TOKEN: &str = "tsp_flow_test_token";

        fn http() -> reqwest::Client {
            client(&HttpSettings {
                policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
                pinned_certificate_pem: None,
            })
            .unwrap()
        }

        async fn server_url(server: &MockServer) -> ServerUrl {
            ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap()
        }

        fn pending_answer(expires_at: &str) -> ResponseTemplate {
            ResponseTemplate::new(202).set_body_json(json!({
                "status": "pending", "pendingId": 1, "expiresAt": expires_at, "confirm": true}))
        }

        async fn begin_with(server: &MockServer, expires_at: &str) -> PendingPairing {
            Mock::given(method("POST"))
                .and(path("/api/pair/complete"))
                .and(body_partial_json(json!({"token": TOKEN})))
                .respond_with(pending_answer(expires_at))
                .mount(server)
                .await;
            PendingPairing::begin(
                &http(),
                server_url(server).await,
                PairingToken::new(TOKEN).unwrap(),
            )
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn an_oversized_pairing_answer_is_refused() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/pair/complete"))
                .respond_with(ResponseTemplate::new(202).set_body_bytes(vec![
                    b'x';
                    MAX_AUTH_BODY_BYTES
                        + 1
                ]))
                .mount(&server)
                .await;
            let result = PendingPairing::begin(
                &http(),
                server_url(&server).await,
                PairingToken::new(TOKEN).unwrap(),
            )
            .await;
            assert!(matches!(result, Err(PairingError::TooLarge)));
        }

        #[tokio::test]
        async fn begin_sends_only_the_public_key_and_derives_the_code() {
            let server = MockServer::start().await;
            let pending = begin_with(&server, "2999-01-01T00:00:00Z").await;

            let requests = server.received_requests().await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(body["jwk"]["kty"], "EC");
            assert_eq!(body["jwk"]["crv"], "P-256");
            assert!(body.get("apiKey").is_none());
            // Exactly token, device name and the public key; no private part.
            assert_eq!(body.as_object().unwrap().len(), 3);
            assert!(body["jwk"].get("d").is_none());

            assert_eq!(
                pending.code(),
                verification_code(TOKEN, &pending.device_key().thumbprint())
            );
            assert_eq!(pending.code().len(), 6);
        }

        #[tokio::test]
        async fn answering_signs_the_request_with_the_new_key() {
            let server = MockServer::start().await;
            let pending = begin_with(&server, "2999-01-01T00:00:00Z").await;
            Mock::given(method("POST"))
                .and(path("/api/pair/confirm"))
                .and(header_exists("DPoP"))
                .and(body_partial_json(json!({"result": "match"})))
                .respond_with(ResponseTemplate::new(202))
                .expect(1)
                .mount(&server)
                .await;

            let outcome = pending.answer(&http(), true).await.unwrap();
            assert_eq!(outcome, Confirmation::WaitingForOwner);

            let requests = server.received_requests().await.unwrap();
            let confirm = requests
                .iter()
                .find(|r| r.url.path() == "/api/pair/confirm")
                .unwrap();
            let proof = confirm
                .headers
                .get("DPoP")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let header_part = proof.split('.').next().unwrap();
            let header: serde_json::Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_part).unwrap()).unwrap();
            assert_eq!(header["jwk"]["x"], pending.device_key().public_jwk().x);
        }

        #[tokio::test]
        async fn a_confirmation_that_both_sides_gave_pairs_the_device() {
            let server = MockServer::start().await;
            let pending = begin_with(&server, "2999-01-01T00:00:00Z").await;
            Mock::given(method("POST"))
                .and(path("/api/pair/confirm"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "status": "paired", "device": {"id": 9, "name": "DanceSonic Studio"}})))
                .mount(&server)
                .await;
            match pending.answer(&http(), true).await.unwrap() {
                Confirmation::Paired(device) => assert_eq!(device.id, 9),
                other => panic!("expected paired, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn waits_for_the_owner_then_returns_the_device() {
            let server = MockServer::start().await;
            let pending = begin_with(&server, "2999-01-01T00:00:00Z").await;
            Mock::given(method("GET"))
                .and(path("/api/device/me"))
                .and(header_exists("DPoP"))
                .respond_with(ResponseTemplate::new(401).set_body_string("pairing_pending"))
                .up_to_n_times(2)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/device/me"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"id": 4, "name": "DanceSonic Studio"})),
                )
                .mount(&server)
                .await;

            let device = pending
                .wait_for_owner_with(&http(), &[Duration::from_millis(5)])
                .await
                .unwrap();
            assert_eq!(device.id, 4);
        }

        #[tokio::test]
        async fn each_refusal_has_its_own_error() {
            let cases: [(u16, &str, &str); 4] = [
                (401, "pairing_rejected", "Rejected"),
                (401, "device_unknown", "DeviceUnknown"),
                (401, "proof_replayed", "ProofRefused"),
                (401, "pairing_expired", "Expired"),
            ];
            for (status, body, expected) in cases {
                let server = MockServer::start().await;
                let pending = begin_with(&server, "2999-01-01T00:00:00Z").await;
                Mock::given(method("GET"))
                    .and(path("/api/device/me"))
                    .respond_with(ResponseTemplate::new(status).set_body_string(body))
                    .mount(&server)
                    .await;
                let error = pending
                    .wait_for_owner_with(&http(), &[Duration::from_millis(5)])
                    .await
                    .unwrap_err();
                assert_eq!(
                    format!("{error:?}").split('(').next(),
                    Some(expected),
                    "{body}"
                );
            }
        }

        #[tokio::test]
        async fn an_oversized_refusal_answer_is_refused_not_read() {
            let server = MockServer::start().await;
            let pending = begin_with(&server, "2999-01-01T00:00:00Z").await;
            Mock::given(method("GET"))
                .and(path("/api/device/me"))
                .respond_with(ResponseTemplate::new(401).set_body_bytes(vec![
                    b'x';
                    MAX_AUTH_BODY_BYTES
                        + 1
                ]))
                .mount(&server)
                .await;
            let error = pending
                .wait_for_owner_with(&http(), &[Duration::from_millis(5)])
                .await
                .unwrap_err();
            assert!(matches!(error, PairingError::TooLarge), "{error:?}");
        }

        #[tokio::test]
        async fn a_pairing_past_its_expiry_stops_waiting() {
            let server = MockServer::start().await;
            let pending = begin_with(&server, "2000-01-01T00:00:00Z").await;
            Mock::given(method("GET"))
                .and(path("/api/device/me"))
                .respond_with(ResponseTemplate::new(401).set_body_string("pairing_pending"))
                .mount(&server)
                .await;
            let error = pending
                .wait_for_owner_with(&http(), &[Duration::from_millis(5)])
                .await
                .unwrap_err();
            assert!(matches!(error, PairingError::Expired));
        }

        #[tokio::test]
        async fn begin_reports_used_busy_and_unknown_tokens() {
            for (status, expected) in [(401u16, "Expired"), (409, "TokenUsed"), (429, "Busy")] {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/api/pair/complete"))
                    .respond_with(ResponseTemplate::new(status))
                    .mount(&server)
                    .await;
                let error = PendingPairing::begin(
                    &http(),
                    server_url(&server).await,
                    PairingToken::new(TOKEN).unwrap(),
                )
                .await
                .unwrap_err();
                assert_eq!(format!("{error:?}").split('(').next(), Some(expected));
            }
        }
    }
}
