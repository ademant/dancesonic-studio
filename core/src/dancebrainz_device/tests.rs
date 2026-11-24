use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use wiremock::matchers::{body_json, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::http::{HttpSettings, client};

pub(crate) const MBID: &str = "0f8b7c4e-2a7b-4a8e-9a42-3d7c5b1e9f10";

pub(crate) fn http() -> reqwest::Client {
    client(&HttpSettings {
        policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
        pinned_certificate_pem: None,
    })
    .unwrap()
}

pub(crate) fn device(server: &MockServer) -> Device {
    let base = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
    Device::new(http(), base, DeviceKey::generate().unwrap())
}

fn mbid() -> Mbid {
    Mbid::parse(MBID).unwrap()
}

fn refused(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_json(serde_json::json!({"error": {"code": code, "message": "no"}}))
}

/// The claims of the DPoP proof a request carried.
fn claims(request: &wiremock::Request) -> serde_json::Value {
    let proof = request.headers.get("DPoP").unwrap().to_str().unwrap();
    let part = proof.split('.').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
}

#[test]
fn a_pairing_link_names_an_https_server_and_a_code() {
    let link = parse_link(
        " dancebrainz://pair?server=https%3A%2F%2Fdb.example%2F&token=abc_123 ",
        HttpPolicy::HTTPS_ONLY,
    )
    .unwrap();
    assert_eq!(link.server.base().as_str(), "https://db.example/");
    assert_eq!(link.token.expose(), "abc_123");

    let bare = parse_link("abc-123", HttpPolicy::HTTPS_ONLY).unwrap();
    assert_eq!(bare.server.base().as_str(), DEFAULT_SERVER);

    assert_eq!(
        parse_link("", HttpPolicy::HTTPS_ONLY),
        Err(InputError::Empty)
    );
    for bad in [
        "dancebrainz://other?token=abc",
        "dancebrainz://pair?server=https%3A%2F%2Fdb.example%2F",
        "dancebrainz://pair?token=a%20b",
        "not a code",
    ] {
        assert_eq!(
            parse_link(bad, HttpPolicy::HTTPS_ONLY),
            Err(InputError::BadToken),
            "{bad}"
        );
    }
    assert!(matches!(
        parse_link(
            "dancebrainz://pair?server=http%3A%2F%2Fdb.example%2F&token=abc",
            HttpPolicy::HTTPS_ONLY
        ),
        Err(InputError::BadServer(_))
    ));
}

#[test]
fn the_debug_form_of_a_link_hides_the_code() {
    let link = parse_link("secret_code", HttpPolicy::HTTPS_ONLY).unwrap();
    assert!(!format!("{link:?}").contains("secret_code"));
}

#[tokio::test]
async fn pairing_sends_the_public_key_and_waits_for_the_owner() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/pairings/complete"))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
            "status": "pending", "device_id": 9, "expires_at": "2026-11-24T10:05:00Z"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let link = PairingLink {
        server: ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap(),
        token: PairingToken::new("abc").unwrap(),
    };
    let key = DeviceKey::generate().unwrap();
    let pending = pair(&http(), &link, &key).await.unwrap();
    assert_eq!(pending.device_id, 9);
    assert!(pending.expires_at_ms.is_some());

    let sent = &server.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["token"], "abc");
    assert_eq!(body["device"], DEVICE_NAME);
    assert_eq!(body["jwk"]["kty"], "EC");
    assert_eq!(body["jwk"]["crv"], "P-256");
    assert!(body["jwk"].get("d").is_none(), "never the private key");
    assert!(sent.headers.get("Authorization").is_none());
}

#[tokio::test]
async fn a_refused_pairing_says_why() {
    for (status, code, expected) in [
        (401, "pairing_invalid", "PairingInvalid"),
        (410, "pairing_expired", "PairingExpired"),
        (409, "device_exists", "DeviceExists"),
        (422, "invalid_key", "Refused(\"invalid_key\")"),
        (500, "", "Http(500)"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(refused(status, code))
            .mount(&server)
            .await;
        let link = PairingLink {
            server: ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap(),
            token: PairingToken::new("abc").unwrap(),
        };
        let error = pair(&http(), &link, &DeviceKey::generate().unwrap())
            .await
            .unwrap_err();
        assert_eq!(format!("{error:?}"), expected);
    }
}

#[tokio::test]
async fn approval_is_asked_with_a_proof() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/devices/me"))
        .and(header_exists("DPoP"))
        .respond_with(refused(401, "pairing_pending"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/devices/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 9, "name": "DanceSonic Studio", "approved_at": "2026-11-24T10:01:00Z"
        })))
        .mount(&server)
        .await;
    let device = device(&server);
    device
        .wait_for_approval(None, &[Duration::from_millis(1)])
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let claims = claims(&requests[0]);
    assert_eq!(claims["htm"], "GET");
    assert_eq!(claims["htu"], format!("{}/api/v1/devices/me", server.uri()));
    assert!(requests[0].headers.get("Authorization").is_none());
}

#[tokio::test]
async fn a_removed_or_denied_device_stops_waiting() {
    for (code, expected) in [
        ("device_unknown", "DeviceUnknown"),
        ("pairing_expired", "PairingExpired"),
        ("proof_replayed", "ProofRefused"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(refused(401, code))
            .mount(&server)
            .await;
        let error = device(&server)
            .wait_for_approval(None, &[Duration::from_millis(1)])
            .await
            .unwrap_err();
        assert_eq!(format!("{error:?}"), expected);
    }
}

#[tokio::test]
async fn the_tags_of_a_recording_carry_their_parent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/recordings/{MBID}/tags")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "recording": MBID,
            "tags": [
                {"tag": {"id": 5, "type": "dance", "name": "Bourrée"},
                 "path": [{"id": 1, "name": "Bourrée group"}, {"id": 5, "name": "Bourrée"}],
                 "qualifier": "both"},
                {"tag": {"id": 31, "type": "meter", "name": "3+3+2"},
                 "path": [{"id": 30, "name": "8/4"}]},
                {"tag": {"id": 30, "type": "meter", "name": "8/4"}, "path": []},
                {"tag": {"id": 0, "type": "dance", "name": "broken"}},
                {"tag": {"id": 7, "type": "dance", "name": "\u{1}"}},
                {"tag": {"id": 8, "type": "dance", "name": "Odd"}, "qualifier": "sometimes"}
            ]
        })))
        .mount(&server)
        .await;
    let tags = device(&server).tags(&mbid()).await.unwrap();
    assert_eq!(tags.len(), 4, "the broken ones are dropped");
    assert_eq!(tags[0].parent.as_deref(), Some("Bourrée group"));
    assert_eq!(tags[0].qualifier, Some(Qualifier::Both));
    assert_eq!(
        (tags[1].name.as_str(), tags[1].parent.as_deref()),
        ("3+3+2", Some("8/4"))
    );
    assert_eq!(tags[2].parent, None);
    assert_eq!(tags[3].qualifier, None, "an unknown qualifier is dropped");
}

#[tokio::test]
async fn a_dance_is_added_and_a_held_change_is_reported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/recordings/{MBID}/tags")))
        .and(body_json(serde_json::json!({"tags": [
            {"type": "dance", "path": "gavotte/gavotte des montagnes", "qualifier": "learn"}
        ]})))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
            "change": {"id": 41, "status": "pending"}, "warnings": [], "tags": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    let written = device(&server)
        .add_dance(&mbid(), "gavotte/gavotte des montagnes", Qualifier::Learn)
        .await
        .unwrap();
    assert_eq!(written, Written::Held(Some(41)));
}

#[tokio::test]
async fn a_meter_is_replaced_without_a_qualifier() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!("/api/v1/recordings/{MBID}/tags")))
        .and(query_param("type", "meter"))
        .and(body_json(serde_json::json!({"tags": [{"tag_id": 31}]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "change": {"id": 42, "status": "applied"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let device = device(&server);
    let written = device
        .replace(&mbid(), METER_KIND, &[(31, None)])
        .await
        .unwrap();
    assert_eq!(written, Written::Applied);
    let sent = &server.received_requests().await.unwrap()[0];
    let claims = claims(sent);
    assert_eq!(claims["htm"], "PUT");
    assert_eq!(
        claims["htu"],
        format!("{}/api/v1/recordings/{MBID}/tags", server.uri()),
        "the proof leaves the query out"
    );
}

#[tokio::test]
async fn shoes_are_set_and_cleared() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!("/api/v1/recordings/{MBID}/shoes")))
        .and(body_json(serde_json::json!({"value": 4})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/api/v1/recordings/{MBID}/shoes")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let device = device(&server);
    device.set_shoes(&mbid(), Some(4)).await.unwrap();
    device.set_shoes(&mbid(), None).await.unwrap();
    assert!(matches!(
        device.set_shoes(&mbid(), Some(6)).await,
        Err(DeviceError::Invalid)
    ));
}

#[tokio::test]
async fn ratings_are_read_within_their_range() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/recordings/{MBID}/ratings")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "hearts": 2, "shoes_median": 3.5, "shoes_raters": 4, "my_shoes": 9
        })))
        .mount(&server)
        .await;
    let ratings = device(&server).ratings(&mbid()).await.unwrap();
    assert_eq!(
        ratings,
        Ratings {
            mine: None,
            median: Some(3.5),
            raters: 4
        }
    );
}

#[tokio::test]
async fn meter_values_and_suggestions_are_read() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tag-types/meter/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": 30, "name": "8/4", "aliases": []},
            {"id": 31, "name": "3+3+2", "parent_id": 30},
            {"id": -1, "name": "bad"}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tag-types/dance/suggest"))
        .and(query_param("name", "gav"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"tag": {"id": 3, "name": "gavotte des montagnes"},
             "path": "gavotte/gavotte des montagnes", "rank": "prefix"},
            {"tag": {"id": 2, "name": "gavotte"}}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tag-types/dance/suggest"))
        .and(query_param("name", "zzz"))
        .respond_with(ResponseTemplate::new(200).set_body_string("null"))
        .mount(&server)
        .await;
    let device = device(&server);
    let meters = device.meter_values().await.unwrap();
    assert_eq!(meters.len(), 2);
    assert_eq!(meters[1].parent_id, Some(30));

    let found = device.suggest_dances(" gav ").await.unwrap();
    assert_eq!(found[0].path, "gavotte/gavotte des montagnes");
    assert_eq!(
        found[1].path, "gavotte",
        "a top-level value is its own path"
    );
    assert!(device.suggest_dances("zzz").await.unwrap().is_empty());
    assert!(device.suggest_dances("  ").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_held_change_is_followed() {
    let server = MockServer::start().await;
    for (id, status) in [(1, "pending"), (2, "applied"), (3, "rejected")] {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/changes/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": id, "status": status})),
            )
            .mount(&server)
            .await;
    }
    let device = device(&server);
    assert_eq!(device.change_state(1).await.unwrap(), ChangeState::Pending);
    assert_eq!(device.change_state(2).await.unwrap(), ChangeState::Applied);
    assert_eq!(device.change_state(3).await.unwrap(), ChangeState::Rejected);
}

#[tokio::test]
async fn an_odd_error_code_is_not_shown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(refused(422, "Look at <this>"))
        .mount(&server)
        .await;
    let error = device(&server).ratings(&mbid()).await.unwrap_err();
    assert_eq!(format!("{error:?}"), "Refused(\"HTTP 422\")");
}

#[tokio::test]
async fn an_answer_that_is_too_large_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(MAX_ANSWER_BYTES + 1)))
        .mount(&server)
        .await;
    assert!(matches!(
        device(&server).meter_values().await,
        Err(DeviceError::TooLarge)
    ));
}
