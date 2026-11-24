use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::dancebrainz_device::tests::{MBID, device};

const OTHER: &str = "11111111-2222-4333-8444-555555555555";

fn temp_database() -> (tempfile::TempDir, Arc<Database>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("data").join("library.db")).unwrap();
    (dir, Arc::new(db))
}

fn mbid() -> Mbid {
    Mbid::parse(MBID).unwrap()
}

fn add(name: &str, qualifier: &str) -> Edit {
    Edit::AddDance {
        path: format!("group/{name}"),
        name: name.to_owned(),
        qualifier: qualifier.to_owned(),
    }
}

fn remove(name: &str, shown: &[&str]) -> Edit {
    Edit::RemoveDance {
        name: name.to_owned(),
        shown: shown.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn meter(meter: &str, sub: Option<&str>) -> MeterChoice {
    MeterChoice {
        meter: meter.to_owned(),
        sub: sub.map(str::to_owned),
    }
}

fn put(db: &Database, mbid: &Mbid, edit: &Edit) {
    db.write(|tx| queue(tx, mbid, edit, 1)).unwrap();
}

fn listed(db: &Database) -> Vec<Edit> {
    edits(&db.reader().unwrap())
        .unwrap()
        .into_iter()
        .map(|e| e.edit)
        .collect()
}

fn tag(kind: &str, id: i64, name: &str, path: &[&str]) -> DanceTag {
    DanceTag {
        tag_id: id,
        kind: kind.to_owned(),
        name: name.to_owned(),
        path: path.iter().map(|s| (*s).to_owned()).collect(),
        qualifier: (kind == DANCE_KIND).then_some(Qualifier::Both),
        updated_at: "x".to_owned(),
    }
}

/// DanceBrainz answers the recording's tags with these dances and meter.
async fn tags_on_server(
    server: &MockServer,
    dances: &[(i64, &str, &str)],
    meter: serde_json::Value,
) {
    let mut tags: Vec<serde_json::Value> = dances
        .iter()
        .map(|(id, name, q)| {
            serde_json::json!({"tag": {"id": id, "type": "dance", "name": name}, "qualifier": q})
        })
        .collect();
    if !meter.is_null() {
        tags.push(meter);
    }
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/recordings/{MBID}/tags")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"tags": tags})))
        .mount(server)
        .await;
}

fn applied() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(serde_json::json!({"change": {"id": 5, "status": "applied"}}))
}

#[test]
fn the_queue_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("library.db");
    {
        let db = Database::open(&file).unwrap();
        put(&db, &mbid(), &add("Bourrée", "both"));
        put(&db, &mbid(), &Edit::SetShoes { to: Some(3) });
    }
    let db = Database::open(&file).unwrap();
    assert_eq!(
        listed(&db),
        [add("Bourrée", "both"), Edit::SetShoes { to: Some(3) }]
    );
    assert_eq!(waiting_count(&db.reader().unwrap()).unwrap(), 2);
}

#[test]
fn a_newer_setting_replaces_one_not_sent() {
    let (_dir, db) = temp_database();
    let other = Mbid::parse(OTHER).unwrap();
    put(&db, &mbid(), &Edit::SetShoes { to: Some(3) });
    put(&db, &other, &Edit::SetShoes { to: Some(1) });
    put(&db, &mbid(), &Edit::SetShoes { to: None });
    put(
        &db,
        &mbid(),
        &Edit::SetMeter {
            to: Some(meter("8/4", None)),
            shown: None,
        },
    );
    put(
        &db,
        &mbid(),
        &Edit::SetMeter {
            to: Some(meter("8/4", Some("3+3+2"))),
            shown: None,
        },
    );
    assert_eq!(
        listed(&db),
        [
            Edit::SetShoes { to: Some(1) },
            Edit::SetShoes { to: None },
            Edit::SetMeter {
                to: Some(meter("8/4", Some("3+3+2"))),
                shown: None
            },
        ]
    );
}

#[test]
fn adding_and_removing_a_dance_cancel_each_other() {
    let (_dir, db) = temp_database();
    // Only queued, so removing it only cancels the addition.
    put(&db, &mbid(), &add("Scottish", "learn"));
    put(&db, &mbid(), &remove("Scottish", &["Bourrée"]));
    assert!(listed(&db).is_empty());

    // On DanceBrainz: the removal is queued; adding it back cancels it.
    put(&db, &mbid(), &remove("Bourrée", &["Bourrée"]));
    put(&db, &mbid(), &remove("Bourrée", &["Bourrée"]));
    assert_eq!(listed(&db).len(), 1, "one removal, not two");
    put(&db, &mbid(), &add("Bourrée", "disco"));
    assert_eq!(listed(&db), [add("Bourrée", "disco")]);
    put(&db, &mbid(), &add("Bourrée", "both"));
    assert_eq!(listed(&db), [add("Bourrée", "both")], "the newer qualifier");
}

#[test]
fn odd_changes_are_refused() {
    let (_dir, db) = temp_database();
    for bad in [
        Edit::SetShoes { to: Some(0) },
        Edit::SetShoes { to: Some(6) },
        add("Bourrée", "sometimes"),
        add(" ", "both"),
        add(&"x".repeat(600), "both"),
        Edit::SetMeter {
            to: Some(meter("", None)),
            shown: None,
        },
    ] {
        assert!(
            db.write(|tx| queue(tx, &mbid(), &bad, 1)).is_err(),
            "{bad:?}"
        );
    }
    assert!(listed(&db).is_empty());
}

#[test]
fn a_full_queue_takes_no_more() {
    let (_dir, db) = temp_database();
    db.write(|tx| {
        tx.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
             INSERT INTO dance_edit (mbid, field, edit, queued_at)
             SELECT ?2, 'shoes', '{\"set_shoes\":{\"to\":1}}', 1 FROM n",
            params![MAX_QUEUE, OTHER],
        )?;
        Ok(())
    })
    .unwrap();
    assert!(
        db.write(|tx| queue(tx, &mbid(), &add("Bourrée", "both"), 1))
            .is_err()
    );
}

#[test]
fn the_window_shows_queued_changes_over_the_cache() {
    let (_dir, db) = temp_database();
    put(&db, &mbid(), &add("Scottish", "learn"));
    put(&db, &mbid(), &remove("Bourrée", &["Bourrée", "Mazurka"]));
    put(
        &db,
        &mbid(),
        &Edit::SetMeter {
            to: None,
            shown: Some(meter("8/4", Some("3+3+2"))),
        },
    );
    put(&db, &mbid(), &Edit::SetShoes { to: Some(5) });
    let cached = [
        tag("dance", 4, "Bourrée", &["Bourrée"]),
        tag("dance", 6, "Mazurka", &["Mazurka"]),
        tag("meter", 31, "3+3+2", &["8/4", "3+3+2"]),
    ];
    assert_eq!(meter_of(&cached), Some(meter("8/4", Some("3+3+2"))));
    let queued = edits_of(&db.reader().unwrap(), &mbid()).unwrap();
    let view = shown(&cached, &queued);
    let names: Vec<(&str, bool, Option<State>)> = view
        .dances
        .iter()
        .map(|d| (d.name.as_str(), d.removing, d.change))
        .collect();
    assert_eq!(
        names,
        [
            ("Bourrée", true, Some(State::Queued)),
            ("Mazurka", false, None),
            ("Scottish", false, Some(State::Queued)),
        ]
    );
    assert_eq!(view.meter, None, "the queued clearing shows");
    assert_eq!(view.shoes, Some(Some(5)));
    assert_eq!(view.states[&Field::Dance].0, State::Queued);
    assert_eq!(State::Queued.label(), "not sent yet");
}

#[test]
fn a_field_shows_what_needs_the_user_first() {
    let edit = |state, field_edit: Edit| QueuedEdit {
        id: 1,
        mbid: mbid(),
        edit: field_edit,
        state,
        error: None,
        change_id: None,
        queued_at: 1,
    };
    let states = field_states(&[
        edit(State::Held, add("A", "both")),
        edit(State::Conflict, remove("B", &[])),
        edit(State::Queued, add("C", "both")),
        edit(State::Held, Edit::SetShoes { to: Some(2) }),
    ]);
    assert_eq!(states[&Field::Dance].0, State::Conflict);
    assert_eq!(states[&Field::Shoes].0, State::Held);
    assert!(!states.contains_key(&Field::Meter));
    assert_eq!(State::Held.label(), "held for review");
    assert_eq!(State::Conflict.label(), "changed on DanceBrainz, not sent");
}

#[test]
fn a_sub_meter_is_found_under_its_own_meter() {
    let value = |id, name: &str, parent| TagValue {
        id,
        name: name.to_owned(),
        parent_id: parent,
    };
    let values = [
        value(30, "8/4", None),
        value(20, "3/4", None),
        value(21, "3+3+2", Some(20)),
        value(31, "3+3+2", Some(30)),
    ];
    assert_eq!(
        resolve_meter(&values, &meter("8/4", Some("3+3+2"))),
        Some(31)
    );
    assert_eq!(
        resolve_meter(&values, &meter("3/4", Some("3+3+2"))),
        Some(21)
    );
    assert_eq!(resolve_meter(&values, &meter("3/4", None)), Some(20));
    assert_eq!(resolve_meter(&values, &meter("5/4", None)), None);
    assert_eq!(resolve_meter(&values, &meter("8/4", Some("2+2"))), None);
}

#[tokio::test]
async fn sent_changes_leave_the_queue() {
    let server = MockServer::start().await;
    tags_on_server(&server, &[(4, "Bourrée", "both")], serde_json::Value::Null).await;
    Mock::given(method("PUT"))
        .and(path(format!("/api/v1/recordings/{MBID}/shoes")))
        .and(body_json(serde_json::json!({"value": 3})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/recordings/{MBID}/tags")))
        .respond_with(applied())
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, db) = temp_database();
    put(&db, &mbid(), &Edit::SetShoes { to: Some(3) });
    put(&db, &mbid(), &add("Scottish", "learn"));
    // Already on DanceBrainz as asked: nothing to send.
    put(&db, &mbid(), &add("Bourrée", "both"));
    let report = flush(Arc::clone(&db), &device(&server)).await.unwrap();
    assert_eq!(report.sent, 3);
    assert_eq!(report.changed, [mbid()]);
    assert!(listed(&db).is_empty());
}

#[tokio::test]
async fn a_removal_keeps_the_other_dances_and_their_qualifiers() {
    let server = MockServer::start().await;
    tags_on_server(
        &server,
        &[(4, "Bourrée", "both"), (6, "Mazurka", "learn")],
        serde_json::Value::Null,
    )
    .await;
    Mock::given(method("PUT"))
        .and(path(format!("/api/v1/recordings/{MBID}/tags")))
        .and(query_param("type", "dance"))
        .and(body_json(
            serde_json::json!({"tags": [{"tag_id": 6, "qualifier": "learn"}]}),
        ))
        .respond_with(applied())
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, db) = temp_database();
    put(&db, &mbid(), &remove("Bourrée", &["Bourrée", "Mazurka"]));
    let report = flush(Arc::clone(&db), &device(&server)).await.unwrap();
    assert_eq!(report.sent, 1);
}

#[tokio::test]
async fn a_change_to_a_field_that_changed_on_dancebrainz_is_not_sent() {
    let server = MockServer::start().await;
    tags_on_server(
        &server,
        &[(4, "Bourrée", "both"), (9, "Polka", "both")],
        serde_json::json!({"tag": {"id": 20, "type": "meter", "name": "3/4"}, "path": []}),
    )
    .await;
    Mock::given(method("PUT"))
        .respond_with(applied())
        .expect(0)
        .mount(&server)
        .await;
    let (_dir, db) = temp_database();
    // Someone added Polka since Studio showed the dances.
    put(&db, &mbid(), &remove("Bourrée", &["Bourrée"]));
    // Someone set 3/4 since Studio showed 8/4.
    put(
        &db,
        &mbid(),
        &Edit::SetMeter {
            to: None,
            shown: Some(meter("8/4", None)),
        },
    );
    let report = flush(Arc::clone(&db), &device(&server)).await.unwrap();
    assert_eq!(report.conflicts, 2);
    let left = edits(&db.reader().unwrap()).unwrap();
    assert!(left.iter().all(|e| e.state == State::Conflict));
    assert!(left[0].error.as_deref().unwrap().contains("dances changed"));
    assert!(left[1].error.as_deref().unwrap().contains("meter changed"));

    // A conflict waits for the user, and is not tried again.
    flush(Arc::clone(&db), &device(&server)).await.unwrap();
    assert_eq!(edits(&db.reader().unwrap()).unwrap().len(), 2);
    let id = left[0].id;
    db.write(|tx| dismiss(tx, id)).unwrap();
    assert_eq!(edits(&db.reader().unwrap()).unwrap().len(), 1);
}

#[tokio::test]
async fn a_meter_is_set_by_the_value_under_its_meter() {
    let server = MockServer::start().await;
    tags_on_server(
        &server,
        &[],
        serde_json::json!({"tag": {"id": 30, "type": "meter", "name": "8/4"}, "path": []}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tag-types/meter/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": 30, "name": "8/4"},
            {"id": 20, "name": "3/4"},
            {"id": 21, "name": "3+3+2", "parent_id": 20},
            {"id": 31, "name": "3+3+2", "parent_id": 30}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(query_param("type", "meter"))
        .and(body_json(serde_json::json!({"tags": [{"tag_id": 31}]})))
        .respond_with(applied())
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, db) = temp_database();
    put(
        &db,
        &mbid(),
        &Edit::SetMeter {
            to: Some(meter("8/4", Some("3+3+2"))),
            shown: Some(meter("8/4", None)),
        },
    );
    let other = Mbid::parse(OTHER).unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/recordings/{OTHER}/tags")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"tags": []})))
        .mount(&server)
        .await;
    put(
        &db,
        &other,
        &Edit::SetMeter {
            to: Some(meter("8/4", Some("4+4"))),
            shown: None,
        },
    );
    let report = flush(Arc::clone(&db), &device(&server)).await.unwrap();
    assert_eq!((report.sent, report.conflicts), (1, 1));
    let left = edits(&db.reader().unwrap()).unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].error.as_deref().unwrap().contains("no longer has"));
}

#[tokio::test]
async fn a_held_change_waits_until_it_is_decided() {
    let server = MockServer::start().await;
    tags_on_server(&server, &[], serde_json::Value::Null).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
            "change": {"id": 77, "status": "pending"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/changes/77"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": 77, "status": "pending"})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/changes/77"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": 77, "status": "applied"})),
        )
        .mount(&server)
        .await;
    let (_dir, db) = temp_database();
    put(&db, &mbid(), &add("Scottish", "learn"));
    let device = device(&server);
    let report = flush(Arc::clone(&db), &device).await.unwrap();
    assert_eq!(report.held, 1);
    let held = edits(&db.reader().unwrap()).unwrap();
    assert_eq!((held[0].state, held[0].change_id), (State::Held, Some(77)));
    assert_eq!(waiting_count(&db.reader().unwrap()).unwrap(), 0);

    flush(Arc::clone(&db), &device).await.unwrap();
    assert_eq!(listed(&db).len(), 1, "still pending");
    let report = flush(Arc::clone(&db), &device).await.unwrap();
    assert_eq!(report.sent, 1, "approved");
    assert!(listed(&db).is_empty());
}

#[tokio::test]
async fn a_rejected_change_is_shown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/changes/8"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": 8, "status": "rejected"})),
        )
        .mount(&server)
        .await;
    let (_dir, db) = temp_database();
    put(&db, &mbid(), &add("Scottish", "learn"));
    db.write(|tx| {
        tx.execute("UPDATE dance_edit SET state = 'held', change_id = 8", [])?;
        Ok(())
    })
    .unwrap();
    flush(Arc::clone(&db), &device(&server)).await.unwrap();
    let left = edits(&db.reader().unwrap()).unwrap();
    assert_eq!(left[0].state, State::Rejected);
}

#[tokio::test]
async fn a_refused_change_stays_queued_and_the_rest_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!("/api/v1/recordings/{MBID}/shoes")))
        .respond_with(ResponseTemplate::new(422).set_body_json(
            serde_json::json!({"error": {"code": "invalid_value", "message": "no"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/api/v1/recordings/{OTHER}/shoes")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, db) = temp_database();
    put(&db, &mbid(), &Edit::SetShoes { to: Some(2) });
    put(
        &db,
        &Mbid::parse(OTHER).unwrap(),
        &Edit::SetShoes { to: Some(4) },
    );
    let report = flush(Arc::clone(&db), &device(&server)).await.unwrap();
    assert_eq!((report.sent, report.failed), (1, 1));
    let left = edits(&db.reader().unwrap()).unwrap();
    assert_eq!(left[0].state, State::Queued);
    assert!(left[0].error.as_deref().unwrap().contains("invalid_value"));
}

#[tokio::test]
async fn without_dancebrainz_or_a_pairing_the_queue_waits() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            serde_json::json!({"error": {"code": "device_unknown", "message": "no"}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, db) = temp_database();
    put(&db, &mbid(), &Edit::SetShoes { to: Some(2) });
    put(
        &db,
        &Mbid::parse(OTHER).unwrap(),
        &Edit::SetShoes { to: Some(4) },
    );
    let report = flush(Arc::clone(&db), &device(&server)).await.unwrap();
    assert!(report.stopped.is_some(), "the second is not tried");
    assert_eq!(waiting_count(&db.reader().unwrap()).unwrap(), 2);

    let gone = device(&server);
    drop(server);
    let report = flush(Arc::clone(&db), &gone).await.unwrap();
    assert_eq!(report.failed, 1);
    assert!(report.stopped.is_some());
    assert_eq!(waiting_count(&db.reader().unwrap()).unwrap(), 2);
}

#[test]
fn the_pairing_is_recorded_once_and_forgotten_without_the_queue() {
    let (_dir, db) = temp_database();
    let pairing_of = |server: &str| Pairing {
        server: server.to_owned(),
        device_id: 3,
        approved: false,
        expires_at: Some(9),
    };
    db.write(|tx| save_pairing(tx, &pairing_of("https://a.example/"), 1))
        .unwrap();
    db.write(|tx| save_pairing(tx, &pairing_of("https://b.example/"), 2))
        .unwrap();
    db.write(approve).unwrap();
    let paired = pairing(&db.reader().unwrap()).unwrap().unwrap();
    assert_eq!(paired.server, "https://b.example/");
    assert!(paired.approved);
    assert_eq!(paired.expires_at, None);

    put(&db, &mbid(), &Edit::SetShoes { to: Some(2) });
    db.write(forget_pairing).unwrap();
    assert_eq!(pairing(&db.reader().unwrap()).unwrap(), None);
    assert_eq!(listed(&db).len(), 1, "the queue stays");
}

#[test]
fn a_queued_track_is_named_by_its_title() {
    let (_dir, db) = temp_database();
    db.write(|tx| {
        tx.execute_batch(&format!(
            "INSERT INTO local_folder (id, path, added_at) VALUES (1, '/music', 1);
             INSERT INTO local_track (id, folder_id, rel_path, size_bytes, modified_ms,
                 fingerprint, title, mbid, scanned_at)
                 VALUES (7, 1, 'a.flac', 1, 1, printf('%064d', 0), 'Local tune', '{MBID}', 1);"
        ))?;
        Ok(())
    })
    .unwrap();
    let conn = db.reader().unwrap();
    assert_eq!(
        title_of(&conn, &mbid()).unwrap().as_deref(),
        Some("Local tune")
    );
    assert_eq!(title_of(&conn, &Mbid::parse(OTHER).unwrap()).unwrap(), None);
}
