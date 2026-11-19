//! Signing in with a username and password, once, to obtain an API key.
//!
//! The password is sent to `POST /api/login`, the server creates an API key
//! with the session, and the session is then ended. Neither the password nor
//! the session is stored; only the key goes on to the caller.

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::connection::ServerUrl;
use crate::http::{BodyError, MAX_AUTH_BODY_BYTES, read_body};
use crate::subsonic::{ApiKey, ApiKeyError};

/// Name of the key created for Studio, as shown on the account page.
pub const KEY_LABEL: &str = "DanceSonic Studio";

/// Longest username accepted, in bytes.
pub const MAX_USERNAME_BYTES: usize = 256;

/// Longest password accepted, in bytes. The server accepts up to 1024.
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Errors from the sign-in exchange. Carry no credentials.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error("the username or password is empty or too long")]
    BadInput,
    #[error("the username or password is wrong, or the account is not active")]
    WrongCredentials,
    #[error("too many attempts from this address; try again later")]
    RateLimited,
    #[error("the server is busy; try again in a moment")]
    Busy,
    #[error("the server could not be reached")]
    Unreachable,
    #[error("the server did not answer in time")]
    Timeout,
    #[error("the server refused the request (HTTP {0})")]
    Http(u16),
    #[error("the server sent an invalid answer")]
    Invalid,
    #[error("the server sent an answer that is too large")]
    TooLarge,
    #[error(transparent)]
    Key(#[from] ApiKeyError),
}

#[derive(Serialize)]
struct LoginBody<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct SessionDto {
    token: String,
}

#[derive(Serialize)]
struct KeyBody<'a> {
    label: &'a str,
}

#[derive(Deserialize)]
struct KeyCreatedDto {
    #[serde(rename = "apiKey")]
    api_key: String,
}

/// Signs in with a username and password and returns a new API key.
///
/// `http` must come from [`crate::http::client`] for this server.
pub async fn exchange_password(
    http: &reqwest::Client,
    server: &ServerUrl,
    username: &str,
    password: &SecretString,
) -> Result<ApiKey, SignInError> {
    let username = username.trim();
    let password_text = password.expose_secret();
    if username.is_empty()
        || username.len() > MAX_USERNAME_BYTES
        || password_text.is_empty()
        || password_text.len() > MAX_PASSWORD_BYTES
    {
        return Err(SignInError::BadInput);
    }

    let session: SessionDto = post_json(
        http,
        server
            .endpoint("api/login")
            .map_err(|_| SignInError::Invalid)?,
        None,
        &LoginBody {
            username,
            password: password_text,
        },
    )
    .await?;
    let session_token = SecretString::from(session.token);

    let created: KeyCreatedDto = post_json(
        http,
        server
            .endpoint("api/keys")
            .map_err(|_| SignInError::Invalid)?,
        Some(&session_token),
        &KeyBody { label: KEY_LABEL },
    )
    .await?;
    let key = ApiKey::new(created.api_key)?;

    // The key does not depend on the session, so a failed sign-out does not
    // fail the sign-in. The session expires on its own in any case.
    if let Err(error) = end_session(http, server, &session_token).await {
        tracing::warn!(%error, "ending the sign-in session failed");
    }
    Ok(key)
}

/// Ends a session. Failures are returned to the caller, which only logs them.
async fn end_session(
    http: &reqwest::Client,
    server: &ServerUrl,
    token: &SecretString,
) -> Result<(), SignInError> {
    let response = http
        .post(
            server
                .endpoint("api/logout")
                .map_err(|_| SignInError::Invalid)?,
        )
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .map_err(map_transport)?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(SignInError::Http(response.status().as_u16()))
    }
}

/// POSTs a JSON body and parses the JSON answer, mapping the status codes the
/// sign-in endpoints use.
async fn post_json<B: Serialize, T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: Url,
    bearer: Option<&SecretString>,
    body: &B,
) -> Result<T, SignInError> {
    let mut request = http.post(url).json(body);
    if let Some(token) = bearer {
        request = request.bearer_auth(token.expose_secret());
    }
    let response = request.send().await.map_err(map_transport)?;
    let status = response.status();
    match status.as_u16() {
        200..=299 => {}
        401 => return Err(SignInError::WrongCredentials),
        429 => {
            // The server answers `server_busy` when its password checks are
            // all in use; the other 429 is the per-address limit.
            let bytes = read_answer(response).await?;
            let text = String::from_utf8_lossy(&bytes);
            return Err(if text.contains("server_busy") {
                SignInError::Busy
            } else {
                SignInError::RateLimited
            });
        }
        503 => return Err(SignInError::Busy),
        other => return Err(SignInError::Http(other)),
    }
    let bytes = read_answer(response).await?;
    serde_json::from_slice(&bytes).map_err(|_| SignInError::Invalid)
}

/// Reads an answer body within [`MAX_AUTH_BODY_BYTES`].
async fn read_answer(response: reqwest::Response) -> Result<Vec<u8>, SignInError> {
    read_body(response, MAX_AUTH_BODY_BYTES)
        .await
        .map_err(|error| match error {
            BodyError::TooLarge => SignInError::TooLarge,
            BodyError::Transport(err) => map_transport(err),
        })
}

fn map_transport(error: reqwest::Error) -> SignInError {
    if error.is_timeout() {
        SignInError::Timeout
    } else {
        tracing::debug!(error = %error.without_url(), "sign-in request failed");
        SignInError::Unreachable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::HttpPolicy;
    use crate::http::{HttpSettings, client};
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    async fn mount_happy_path(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .and(body_json(
                json!({"username": "ada", "password": "correct horse"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"token": "sess_one_time", "username": "ada", "role": "user"}),
            ))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/keys"))
            .and(header("Authorization", "Bearer sess_one_time"))
            .and(body_json(json!({"label": KEY_LABEL})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "apiKey": "tsk_created", "label": KEY_LABEL, "expiresAt": null})))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/logout"))
            .and(header("Authorization", "Bearer sess_one_time"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_password_becomes_a_key_and_the_session_is_ended() {
        let server = MockServer::start().await;
        mount_happy_path(&server).await;
        let key = exchange_password(
            &http(),
            &server_url(&server).await,
            " ada ",
            &SecretString::from("correct horse".to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(key.expose(), "tsk_created");
        // `expect(1)` on the logout mock checks the session was ended.
    }

    #[tokio::test]
    async fn a_failed_logout_does_not_fail_the_sign_in() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"token": "sess_x", "username": "ada", "role": "user"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/keys"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "apiKey": "tsk_ok", "label": "x", "expiresAt": null})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/logout"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let key = exchange_password(
            &http(),
            &server_url(&server).await,
            "ada",
            &SecretString::from("pw".to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(key.expose(), "tsk_ok");
    }

    #[tokio::test]
    async fn wrong_password_is_reported_as_such() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"error": "invalid credentials"})),
            )
            .mount(&server)
            .await;
        let result = exchange_password(
            &http(),
            &server_url(&server).await,
            "ada",
            &SecretString::from("wrong".to_owned()),
        )
        .await;
        assert!(matches!(result, Err(SignInError::WrongCredentials)));
    }

    #[tokio::test]
    async fn busy_and_rate_limited_are_told_apart() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(ResponseTemplate::new(429).set_body_string("server_busy"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(ResponseTemplate::new(429).set_body_string("too many attempts"))
            .mount(&server)
            .await;
        let url = server_url(&server).await;
        let pw = SecretString::from("pw".to_owned());
        assert!(matches!(
            exchange_password(&http(), &url, "ada", &pw).await,
            Err(SignInError::Busy)
        ));
        assert!(matches!(
            exchange_password(&http(), &url, "ada", &pw).await,
            Err(SignInError::RateLimited)
        ));
    }

    #[tokio::test]
    async fn an_oversized_success_answer_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_AUTH_BODY_BYTES + 1]),
            )
            .mount(&server)
            .await;
        let result = exchange_password(
            &http(),
            &server_url(&server).await,
            "ada",
            &SecretString::from("pw".to_owned()),
        )
        .await;
        assert!(matches!(result, Err(SignInError::TooLarge)));
    }

    #[tokio::test]
    async fn an_oversized_key_answer_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"token": "sess_x", "username": "ada", "role": "user"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/keys"))
            .respond_with(
                ResponseTemplate::new(201).set_body_bytes(vec![b'x'; MAX_AUTH_BODY_BYTES + 1]),
            )
            .mount(&server)
            .await;
        let result = exchange_password(
            &http(),
            &server_url(&server).await,
            "ada",
            &SecretString::from("pw".to_owned()),
        )
        .await;
        assert!(matches!(result, Err(SignInError::TooLarge)));
    }

    #[tokio::test]
    async fn an_oversized_rate_limit_answer_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(
                ResponseTemplate::new(429).set_body_bytes(vec![b'x'; MAX_AUTH_BODY_BYTES + 1]),
            )
            .mount(&server)
            .await;
        let result = exchange_password(
            &http(),
            &server_url(&server).await,
            "ada",
            &SecretString::from("pw".to_owned()),
        )
        .await;
        assert!(matches!(result, Err(SignInError::TooLarge)));
    }

    #[tokio::test]
    async fn empty_input_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let url = server_url(&server).await;
        let result =
            exchange_password(&http(), &url, "   ", &SecretString::from("pw".to_owned())).await;
        assert!(matches!(result, Err(SignInError::BadInput)));
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
