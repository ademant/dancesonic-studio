use rusqlite::params;
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::db::tests::temp_db;
use crate::http::{HttpSettings, client};

const KEY: &str = "studio-app-key";
const A: &str = "6e2a8f50-58c4-4c44-9a4c-0d7a3c8f1b11";
const B: &str = "0f9d8d5e-1c4b-4a8e-9b0c-2d7e6f5a4b3c";

fn service(server: &MockServer) -> AcoustId {
    let http = client(&HttpSettings {
        policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
        pinned_certificate_pem: None,
    })
    .unwrap();
    let base = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
    AcoustId::at(http, base, KEY)
}

fn print() -> Fingerprint {
    Fingerprint {
        value: "AQADtMmybfGO8NCNEESLnzHyXNOHeHnG".to_owned(),
        duration_sec: 120.0,
    }
}

#[tokio::test]
async fn candidates_come_best_first_once_each_and_checked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/lookup"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(body_string_contains("client=studio-app-key"))
        .and(body_string_contains("meta=recordings"))
        .and(body_string_contains("duration=243"))
        .and(body_string_contains(
            "fingerprint=AQADtMmybfGO8NCNEESLnzHyXNOHeHnG",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ok",
            "results": [
                {"id": "r1", "score": 0.62, "recordings": [
                    {"id": A, "title": "An dro (live)", "duration": 250.4,
                     "artists": [{"name": "Blowzabella"}]},
                    {"id": B, "title": "Bourrée", "duration": 180.6}
                ]},
                {"id": "r2", "score": 0.97, "recordings": [
                    {"id": A.to_uppercase(), "title": " An dro ",
                     "artists": [{"name": "Naragonia", "joinphrase": " & "},
                                 {"name": "Duo\u{7}"}]},
                    {"id": "not-an-mbid", "title": "Broken"}
                ]},
                {"id": "r3", "score": 7.0, "recordings": [{"id": B}]},
                {"id": "r4", "recordings": [{"id": B}]}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let found = service(&server).lookup(&print(), 243).await.unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].mbid.as_str(), A, "in lower case, once");
    assert_eq!(found[0].score, 0.97, "with its best score");
    assert_eq!(found[0].title.as_deref(), Some("An dro"));
    assert_eq!(found[0].artist.as_deref(), Some("Naragonia & Duo"));
    assert_eq!(found[0].duration_sec, None);
    assert_eq!(found[1].mbid.as_str(), B);
    assert_eq!(found[1].score, 0.62, "a score above 1 or none is ignored");
    assert_eq!(found[1].duration_sec, Some(181));
}

#[tokio::test]
async fn the_key_is_never_in_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .mount(&server)
        .await;
    let acoustid = service(&server);
    assert_eq!(acoustid.lookup(&print(), 60).await.unwrap(), []);
    let requests: Vec<Request> = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].url.as_str().contains(KEY));
    assert!(!requests[0].url.as_str().contains("AQAD"));
    assert!(
        !format!("{acoustid:?}").contains(KEY),
        "Debug redacts the key"
    );
}

#[tokio::test]
async fn errors_are_told_apart() {
    let cases = [
        (
            ResponseTemplate::new(400).set_body_json(
                json!({"status": "error", "error": {"code": 4, "message": "invalid API key"}}),
            ),
            AcoustIdError::Key,
        ),
        (
            ResponseTemplate::new(400).set_body_json(
                json!({"status": "error", "error": {"code": 3, "message": "invalid fingerprint"}}),
            ),
            AcoustIdError::Service(3),
        ),
        (ResponseTemplate::new(500), AcoustIdError::Http(500)),
        (
            ResponseTemplate::new(200).set_body_string("<html>"),
            AcoustIdError::Invalid,
        ),
        (
            ResponseTemplate::new(200).set_body_string("x".repeat(MAX_ANSWER + 1)),
            AcoustIdError::TooLarge,
        ),
    ];
    for (response, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(response)
            .mount(&server)
            .await;
        assert_eq!(service(&server).lookup(&print(), 60).await, Err(expected));
    }
}

#[tokio::test]
async fn a_busy_service_is_asked_once_more() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .expect(2)
        .mount(&server)
        .await;
    assert_eq!(
        service(&server).lookup(&print(), 60).await,
        Err(AcoustIdError::RateLimited)
    );
}

#[tokio::test]
async fn requests_keep_below_the_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .mount(&server)
        .await;
    let acoustid = service(&server);
    let start = std::time::Instant::now();
    for _ in 0..3 {
        acoustid.lookup(&print(), 60).await.unwrap();
    }
    assert!(start.elapsed() >= MIN_INTERVAL * 2);
}

#[tokio::test]
async fn a_track_without_a_length_is_not_looked_up() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    for duration in [0, MAX_DURATION_SEC + 1] {
        assert_eq!(
            service(&server).lookup(&print(), duration).await,
            Err(AcoustIdError::NoDuration)
        );
    }
}

#[test]
fn untagged_tracks_come_with_the_fingerprint_of_their_current_file() {
    let (_dir, db) = temp_db();
    db.write(|tx| {
        tx.execute_batch(
            "INSERT INTO local_folder (id, path, added_at) VALUES (1, '/music', 1);
             INSERT INTO local_track (id, folder_id, rel_path, size_bytes, modified_ms,
                 fingerprint, title, duration_ms, scanned_at) VALUES
                 (1, 1, 'b.flac', 10, 20, printf('%064d', 1), 'Fresh', 243400, 1),
                 (2, 1, 'a.flac', 10, 20, printf('%064d', 2), 'Stale', NULL, 1),
                 (3, 1, 'c.flac', 10, 20, printf('%064d', 3), 'None', 90000, 1);",
        )?;
        tx.execute(
            "INSERT INTO local_track (id, folder_id, rel_path, size_bytes, modified_ms,
                 fingerprint, mbid, scanned_at) VALUES (4, 1, 'd.flac', 1, 1, ?1, ?2, 1)",
            params!["4".repeat(64), A],
        )?;
        tx.execute_batch(
            "INSERT INTO local_analysis (track_id, tool_version, size_bytes, modified_ms,
                 analysed_at, fingerprint, fingerprint_sec) VALUES
                 (1, '0.5.0', 10, 20, 1, 'AQADfresh', 120.0),
                 (2, '0.5.0', 9, 20, 1, 'AQADstale', 120.0);",
        )?;
        Ok(())
    })
    .unwrap();

    let list = untagged(&db.reader().unwrap()).unwrap();
    let names: Vec<&str> = list.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, ["a.flac", "b.flac", "c.flac"], "not the tagged one");
    let fresh = &list[1];
    assert_eq!(
        fresh.fingerprint.as_ref().map(|f| f.value.as_str()),
        Some("AQADfresh")
    );
    assert_eq!(fresh.lookup_duration(), Some(243));
    assert_eq!(list[0].fingerprint, None, "the file changed since");
    assert_eq!(list[0].lookup_duration(), None);
    assert_eq!(list[2].fingerprint, None);
    assert_eq!(list[2].lookup_duration(), Some(90));
}

#[test]
fn the_build_key_is_optional() {
    // Whatever this build was given, an empty key counts as none.
    if let Some(key) = app_key() {
        assert!(!key.trim().is_empty());
    }
}
