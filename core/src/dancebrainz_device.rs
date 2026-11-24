//! Studio as a paired DanceBrainz device (#16).
//!
//! The owner creates a pairing code on the DanceBrainz account page and
//! pastes the link (`dancebrainz://pair?server=…&token=…`) into Studio.
//! Studio sends a fresh public key with the code and waits until the owner
//! approves the device on the account page. From then on every write is
//! signed with the key (a DPoP proof, see [`crate::dpop`]); no token is ever
//! issued or stored. The private key is kept in the Secret Service by the
//! caller.
//!
//! Every answer is untrusted: bodies are bounded, ids and qualifiers are
//! checked, and texts are cut to a sane length.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::connection::{HttpPolicy, ServerUrl};
use crate::dancebrainz::{DEFAULT_SERVER, Mbid, Qualifier};
use crate::dpop::{DeviceKey, DpopError};
use crate::http::{BodyError, read_body};
use crate::pairing::{DEVICE_NAME, InputError, PairingToken, parse_rfc3339_ms};

/// Largest answer read from DanceBrainz here. A tag list or the meter values
/// are a few kilobytes.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;

/// Longest name of a tag kept.
const MAX_NAME_BYTES: usize = 512;

/// Most suggestions asked for.
const SUGGEST_LIMIT: &str = "25";

/// Wait between checks on the owner's approval: 3 s, 5 s, then 10 s.
pub const POLL_DELAYS: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// Longest a pairing waits when the server gives no expiry. The code itself
/// lasts five minutes.
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// The tag type of a meter.
pub const METER_KIND: &str = "meter";

/// A pasted pairing link: the server and its one-time code.
#[derive(Debug, PartialEq, Eq)]
pub struct PairingLink {
    pub server: ServerUrl,
    pub token: PairingToken,
}

/// Reads what the user pasted: the `dancebrainz://pair?server=…&token=…`
/// link, or the bare code for the default server. The server must be https.
pub fn parse_link(input: &str, policy: HttpPolicy) -> Result<PairingLink, InputError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(InputError::Empty);
    }
    if !text.starts_with("dancebrainz://") {
        let server = ServerUrl::parse(DEFAULT_SERVER, HttpPolicy::HTTPS_ONLY)
            .map_err(InputError::BadServer)?;
        return Ok(PairingLink {
            server,
            token: PairingToken::new(text)?,
        });
    }
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
    let server = ServerUrl::parse(server.as_deref().unwrap_or(DEFAULT_SERVER), policy)
        .map_err(InputError::BadServer)?;
    Ok(PairingLink { server, token })
}

/// Errors talking to DanceBrainz as a device. Carry no keys and no codes.
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("the pairing code is not valid; create a new one on the account page")]
    PairingInvalid,
    #[error("the pairing code has expired; create a new one on the account page")]
    PairingExpired,
    #[error("this desktop is already paired with the account")]
    DeviceExists,
    #[error("the device is waiting for approval on the account page")]
    Waiting,
    #[error("the device was removed from the DanceBrainz account; pair again")]
    DeviceUnknown,
    #[error("DanceBrainz did not accept the device's signature; check the clock")]
    ProofRefused,
    #[error("DanceBrainz is busy; the change is sent later")]
    Busy,
    #[error("DanceBrainz refused the change ({0})")]
    Refused(String),
    #[error("DanceBrainz does not know this")]
    NotFound,
    #[error("DanceBrainz could not be reached")]
    Unreachable,
    #[error("DanceBrainz did not answer in time")]
    Timeout,
    #[error("DanceBrainz sent an invalid answer")]
    Invalid,
    #[error("DanceBrainz sent an answer that is too large")]
    TooLarge,
    #[error("DanceBrainz refused the request (HTTP {0})")]
    Http(u16),
    #[error(transparent)]
    Key(#[from] DpopError),
}

impl DeviceError {
    /// The request never got an answer, so later requests would fail too.
    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Unreachable | Self::Timeout | Self::Busy)
    }

    /// The pairing itself is gone; nothing can be sent until the user pairs
    /// again.
    pub fn is_unpaired(&self) -> bool {
        matches!(
            self,
            Self::DeviceUnknown | Self::Waiting | Self::PairingExpired
        )
    }
}

/// The server's answer to a pairing: the device waits for the owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDevice {
    pub device_id: i64,
    /// When the owner's approval window closes, in epoch milliseconds.
    pub expires_at_ms: Option<i64>,
}

/// One tag on a recording, as DanceBrainz has it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentTag {
    pub tag_id: i64,
    pub kind: String,
    pub name: String,
    /// The name of the value this one sits under, such as the meter of a
    /// sub-meter. `None` at the top level.
    pub parent: Option<String>,
    pub qualifier: Option<Qualifier>,
}

/// One value of a tag type, such as a meter or a sub-meter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagValue {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
}

/// A dance the autocomplete offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    pub tag_id: i64,
    pub name: String,
    /// The path a tag write takes, `group/dance`. Names may contain `/`, so
    /// this is only ever sent back, never split.
    pub path: String,
}

/// The caller's shoes rating of a recording and what others gave.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ratings {
    pub mine: Option<u8>,
    pub median: Option<f64>,
    pub raters: u32,
}

/// What became of a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    Applied,
    /// A moderated server holds it for review, as this change.
    Held(Option<i64>),
}

/// Where a held change stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeState {
    Pending,
    Applied,
    Rejected,
}

/// Starts a pairing: sends the public key of `key` with the code. The device
/// can act once the owner approved it; see [`Device::wait_for_approval`].
///
/// `http` must come from [`crate::http::client`], so the https rules apply.
pub async fn pair(
    http: &reqwest::Client,
    link: &PairingLink,
    key: &DeviceKey,
) -> Result<PendingDevice, DeviceError> {
    let url = link
        .server
        .endpoint("api/v1/pairings/complete")
        .map_err(|_| DeviceError::Invalid)?;
    let response = http
        .post(url)
        .json(&serde_json::json!({
            "token": link.token.expose(),
            "device": DEVICE_NAME,
            "jwk": key.public_jwk(),
        }))
        .send()
        .await
        .map_err(map_transport)?;
    let status = response.status().as_u16();
    let body = read(response).await?;
    if status != 202 {
        return Err(refusal(status, &body));
    }
    #[derive(Deserialize)]
    struct PendingDto {
        device_id: i64,
        #[serde(default)]
        expires_at: Option<String>,
    }
    let dto: PendingDto = serde_json::from_slice(&body).map_err(|_| DeviceError::Invalid)?;
    if dto.device_id <= 0 {
        return Err(DeviceError::Invalid);
    }
    Ok(PendingDevice {
        device_id: dto.device_id,
        expires_at_ms: dto.expires_at.as_deref().and_then(parse_rfc3339_ms),
    })
}

/// A paired DanceBrainz device: signs every request with its key.
#[derive(Clone, Debug)]
pub struct Device {
    http: reqwest::Client,
    base: ServerUrl,
    key: std::sync::Arc<DeviceKey>,
}

#[derive(Deserialize)]
struct TagRefDto {
    #[serde(default)]
    id: i64,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct EntryDto {
    tag: TagRefDto,
    #[serde(default)]
    path: Vec<TagRefDto>,
    #[serde(default)]
    qualifier: Option<String>,
}

#[derive(Deserialize)]
struct EntriesDto {
    #[serde(default)]
    tags: Vec<EntryDto>,
}

#[derive(Deserialize)]
struct ChangeDto {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize)]
struct OutcomeDto {
    change: ChangeDto,
}

impl Device {
    /// The device for `base`, signing with `key`. `http` must come from
    /// [`crate::http::client`].
    pub fn new(http: reqwest::Client, base: ServerUrl, key: DeviceKey) -> Self {
        Self {
            http,
            base,
            key: std::sync::Arc::new(key),
        }
    }

    pub fn server(&self) -> &ServerUrl {
        &self.base
    }

    /// Asks whether the owner approved the device: `Ok(true)` when it may act,
    /// `Ok(false)` while it waits.
    pub async fn approved(&self) -> Result<bool, DeviceError> {
        match self
            .call(reqwest::Method::GET, "api/v1/devices/me", None, &[])
            .await
        {
            Ok(_) => Ok(true),
            Err(DeviceError::Waiting) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Waits until the owner approved the device, or the window closed.
    pub async fn wait_for_approval(
        &self,
        expires_at_ms: Option<i64>,
        delays: &[Duration],
    ) -> Result<(), DeviceError> {
        let started = std::time::Instant::now();
        let mut attempt = 0usize;
        loop {
            if self.approved().await? {
                return Ok(());
            }
            let expired = expires_at_ms.is_some_and(|at| crate::sync::now_ms() >= at);
            if expired || started.elapsed() >= MAX_WAIT {
                return Err(DeviceError::PairingExpired);
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

    /// The tags `mbid` has on DanceBrainz now.
    pub async fn tags(&self, mbid: &Mbid) -> Result<Vec<CurrentTag>, DeviceError> {
        let body = self
            .call(
                reqwest::Method::GET,
                &format!("api/v1/recordings/{}/tags", mbid.as_str()),
                None,
                &[],
            )
            .await?;
        let dto: EntriesDto = serde_json::from_slice(&body).map_err(|_| DeviceError::Invalid)?;
        Ok(dto.tags.into_iter().filter_map(current_tag).collect())
    }

    /// Adds the dance at `path` to `mbid`, keeping its other tags.
    pub async fn add_dance(
        &self,
        mbid: &Mbid,
        path: &str,
        qualifier: Qualifier,
    ) -> Result<Written, DeviceError> {
        let body = serde_json::json!({"tags": [{
            "type": crate::dancebrainz::DANCE_KIND,
            "path": path,
            "qualifier": qualifier.as_str(),
        }]});
        self.write(reqwest::Method::POST, mbid, None, body).await
    }

    /// Replaces all tags of `kind` on `mbid` with `tags`. There is no version
    /// check on the server, so the caller reads first.
    pub async fn replace(
        &self,
        mbid: &Mbid,
        kind: &'static str,
        tags: &[(i64, Option<Qualifier>)],
    ) -> Result<Written, DeviceError> {
        let items: Vec<serde_json::Value> = tags
            .iter()
            .map(|(id, qualifier)| match qualifier {
                Some(q) => serde_json::json!({"tag_id": id, "qualifier": q.as_str()}),
                None => serde_json::json!({"tag_id": id}),
            })
            .collect();
        let body = serde_json::json!({ "tags": items });
        self.write(reqwest::Method::PUT, mbid, Some(kind), body)
            .await
    }

    /// Sets the caller's shoes rating of `mbid` (1 to 5), or clears it.
    pub async fn set_shoes(&self, mbid: &Mbid, value: Option<u8>) -> Result<(), DeviceError> {
        let path = format!("api/v1/recordings/{}/shoes", mbid.as_str());
        match value {
            Some(value) if (1..=5).contains(&value) => {
                let body = serde_json::json!({ "value": value });
                self.call(reqwest::Method::PUT, &path, Some(body), &[])
                    .await?;
            }
            Some(_) => return Err(DeviceError::Invalid),
            None => {
                self.call(reqwest::Method::DELETE, &path, None, &[]).await?;
            }
        }
        Ok(())
    }

    /// The shoes ratings of `mbid`, with the caller's own.
    pub async fn ratings(&self, mbid: &Mbid) -> Result<Ratings, DeviceError> {
        #[derive(Deserialize)]
        struct RatingsDto {
            #[serde(default)]
            shoes_median: Option<f64>,
            #[serde(default)]
            shoes_raters: Option<i64>,
            #[serde(default)]
            my_shoes: Option<i64>,
        }
        let body = self
            .call(
                reqwest::Method::GET,
                &format!("api/v1/recordings/{}/ratings", mbid.as_str()),
                None,
                &[],
            )
            .await?;
        let dto: RatingsDto = serde_json::from_slice(&body).map_err(|_| DeviceError::Invalid)?;
        Ok(Ratings {
            mine: dto
                .my_shoes
                .and_then(|v| u8::try_from(v).ok())
                .filter(|v| (1..=5).contains(v)),
            median: dto.shoes_median.filter(|m| (1.0..=5.0).contains(m)),
            raters: dto
                .shoes_raters
                .and_then(|r| u32::try_from(r).ok())
                .unwrap_or(0),
        })
    }

    /// Every meter and sub-meter DanceBrainz has.
    pub async fn meter_values(&self) -> Result<Vec<TagValue>, DeviceError> {
        #[derive(Deserialize)]
        struct ValueDto {
            id: i64,
            #[serde(default)]
            name: String,
            #[serde(default)]
            parent_id: Option<i64>,
        }
        let body = self
            .call(
                reqwest::Method::GET,
                &format!("api/v1/tag-types/{METER_KIND}/tags"),
                None,
                &[],
            )
            .await?;
        let values: Vec<ValueDto> =
            serde_json::from_slice(&body).map_err(|_| DeviceError::Invalid)?;
        Ok(values
            .into_iter()
            .filter(|v| v.id > 0)
            .filter_map(|v| {
                Some(TagValue {
                    id: v.id,
                    name: name(&v.name)?,
                    parent_id: v.parent_id.filter(|p| *p > 0),
                })
            })
            .collect())
    }

    /// Dances whose name is like `text`, closest first.
    pub async fn suggest_dances(&self, text: &str) -> Result<Vec<Suggestion>, DeviceError> {
        #[derive(Deserialize)]
        struct SuggestionDto {
            tag: TagRefDto,
            #[serde(default)]
            path: Option<String>,
        }
        let text: String = text.chars().filter(|c| !c.is_control()).take(100).collect();
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let body = self
            .call(
                reqwest::Method::GET,
                &format!(
                    "api/v1/tag-types/{}/suggest",
                    crate::dancebrainz::DANCE_KIND
                ),
                None,
                &[("name", text.trim()), ("limit", SUGGEST_LIMIT)],
            )
            .await?;
        // The server answers `null` when nothing is close.
        let found: Option<Vec<SuggestionDto>> =
            serde_json::from_slice(&body).map_err(|_| DeviceError::Invalid)?;
        Ok(found
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.tag.id > 0)
            .filter_map(|s| {
                let tag_name = name(&s.tag.name)?;
                let path = s
                    .path
                    .as_deref()
                    .and_then(name)
                    .unwrap_or_else(|| tag_name.clone());
                Some(Suggestion {
                    tag_id: s.tag.id,
                    name: tag_name,
                    path,
                })
            })
            .collect())
    }

    /// Where the held change `id` stands.
    pub async fn change_state(&self, id: i64) -> Result<ChangeState, DeviceError> {
        let body = self
            .call(
                reqwest::Method::GET,
                &format!("api/v1/changes/{id}"),
                None,
                &[],
            )
            .await?;
        let dto: ChangeDto = serde_json::from_slice(&body).map_err(|_| DeviceError::Invalid)?;
        Ok(match dto.status.as_str() {
            "applied" => ChangeState::Applied,
            "rejected" => ChangeState::Rejected,
            _ => ChangeState::Pending,
        })
    }

    async fn write(
        &self,
        method: reqwest::Method,
        mbid: &Mbid,
        kind: Option<&'static str>,
        body: serde_json::Value,
    ) -> Result<Written, DeviceError> {
        let query: Vec<(&str, &str)> = kind.map(|k| ("type", k)).into_iter().collect();
        let answer = self
            .call(
                method,
                &format!("api/v1/recordings/{}/tags", mbid.as_str()),
                Some(body),
                &query,
            )
            .await?;
        let outcome: OutcomeDto =
            serde_json::from_slice(&answer).map_err(|_| DeviceError::Invalid)?;
        Ok(match outcome.change.status.as_str() {
            "pending" => Written::Held(outcome.change.id.filter(|id| *id > 0)),
            _ => Written::Applied,
        })
    }

    /// One signed request. Returns the body of a 2xx answer.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        query: &[(&str, &str)],
    ) -> Result<Vec<u8>, DeviceError> {
        let mut url = self.base.endpoint(path).map_err(|_| DeviceError::Invalid)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        // The proof is bound to the address without its query.
        let proof = self
            .key
            .proof(method.as_str(), &url, crate::sync::now_ms() / 1000)?;
        let mut request = self.http.request(method, url).header("DPoP", proof);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(map_transport)?;
        let status = response.status().as_u16();
        let body = read(response).await?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(refusal(status, &body))
        }
    }
}

/// A tag of an answer, or `None` when it does not parse.
fn current_tag(dto: EntryDto) -> Option<CurrentTag> {
    if dto.tag.id <= 0 {
        return None;
    }
    let parent = dto
        .path
        .iter()
        .rev()
        .find(|p| p.id != dto.tag.id)
        .and_then(|p| name(&p.name));
    Some(CurrentTag {
        tag_id: dto.tag.id,
        kind: name(&dto.tag.kind)?,
        name: name(&dto.tag.name)?,
        parent,
        qualifier: dto.qualifier.as_deref().and_then(Qualifier::parse),
    })
}

/// A name from the server: no control characters, not empty, bounded.
fn name(text: &str) -> Option<String> {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return None;
    }
    let mut end = clean.len().min(MAX_NAME_BYTES);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    Some(clean[..end].to_owned())
}

/// The error for a refused request, from its status and error code.
fn refusal(status: u16, body: &[u8]) -> DeviceError {
    #[derive(Deserialize)]
    struct Envelope {
        error: ErrorDto,
    }
    #[derive(Deserialize)]
    struct ErrorDto {
        #[serde(default)]
        code: String,
    }
    let code = serde_json::from_slice::<Envelope>(body)
        .map(|e| e.error.code)
        .unwrap_or_default();
    match code.as_str() {
        "pairing_invalid" => return DeviceError::PairingInvalid,
        "pairing_expired" => return DeviceError::PairingExpired,
        "pairing_pending" => return DeviceError::Waiting,
        "device_exists" => return DeviceError::DeviceExists,
        "device_unknown" => return DeviceError::DeviceUnknown,
        "proof_invalid" | "proof_replayed" => return DeviceError::ProofRefused,
        _ => {}
    }
    match status {
        404 => DeviceError::NotFound,
        429 | 503 => DeviceError::Busy,
        400 | 403 | 409 | 422 => {
            // The code is the server's stable word for the reason; it is shown,
            // so only a short plain word passes.
            let shown = if !code.is_empty()
                && code.len() <= 64
                && code.chars().all(|c| c.is_ascii_lowercase() || c == '_')
            {
                code
            } else {
                format!("HTTP {status}")
            };
            DeviceError::Refused(shown)
        }
        other => DeviceError::Http(other),
    }
}

async fn read(response: reqwest::Response) -> Result<Vec<u8>, DeviceError> {
    read_body(response, MAX_ANSWER_BYTES)
        .await
        .map_err(|error| match error {
            BodyError::TooLarge => DeviceError::TooLarge,
            BodyError::Transport(err) => map_transport(err),
        })
}

fn map_transport(error: reqwest::Error) -> DeviceError {
    if error.is_timeout() {
        DeviceError::Timeout
    } else {
        tracing::debug!(error = %error.without_url(), "DanceBrainz request failed");
        DeviceError::Unreachable
    }
}

#[cfg(test)]
pub(crate) mod tests;
