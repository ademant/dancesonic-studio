//! Contributing dances, meter and shoes to DanceBrainz (#16): the background
//! side. Pairing, the device key in the Secret Service, the edit queue and
//! sending it. Everything here runs on the Tokio runtime.

use std::sync::Arc;

use secrecy::SecretSlice;
use studio_core::connection::{HttpPolicy, ServerUrl};
use studio_core::dance_edits::{self, Edit, Field, FlushReport, MeterChoice, QueuedEdit, Shown};
use studio_core::dancebrainz::{self, DanceBrainz, DanceTag, Mbid};
use studio_core::dancebrainz_device::{self, Device, Ratings, Suggestion, TagValue, parse_link};
use studio_core::db::Database;
use studio_core::dpop::DeviceKey;
use studio_core::http::{self, HttpSettings};
use studio_core::secrets::{SecretId, SecretKind};
use studio_core::sync;

use crate::Services;

/// Where the pairing stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PairState {
    /// Not paired: changes are queued and wait.
    Unpaired,
    /// Paired with the server at this address.
    Paired(String),
}

/// What one recording shows: DanceBrainz's data with the queued changes, and
/// the viewer's shoes rating when the device could read it.
#[derive(Clone, Debug, Default)]
pub(crate) struct TrackView {
    pub(crate) shown: Shown,
    pub(crate) ratings: Option<Ratings>,
}

/// What the edit dialog needs about each track it changes.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub(crate) mbid: Mbid,
    /// The tags DanceBrainz had at the last lookup.
    pub(crate) cached: Vec<DanceTag>,
}

/// One row of the queue list.
#[derive(Clone, Debug)]
pub(crate) struct QueueRow {
    pub(crate) edit: QueuedEdit,
    pub(crate) title: Option<String>,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, studio_core::db::DbError> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| "The database task stopped.".to_owned())?
        .map_err(|e| e.to_string())
}

/// Where the pairing stands, and how many changes wait to be sent.
pub(crate) async fn state(db: Arc<Database>) -> Result<(PairState, i64), String> {
    blocking(move || {
        let conn = db.reader()?;
        let state = match dance_edits::pairing(&conn)? {
            Some(p) if p.approved => PairState::Paired(p.server),
            _ => PairState::Unpaired,
        };
        Ok((state, dance_edits::waiting_count(&conn)?))
    })
    .await
}

/// Pairs with the account whose link the user pasted, and waits until the
/// owner approved the device on the account page. The key is stored only
/// once the device is approved.
pub(crate) async fn pair(services: Services, link: String) -> Result<String, String> {
    let link = parse_link(&link, HttpPolicy::HTTPS_ONLY).map_err(|e| e.to_string())?;
    let http = http::client(&HttpSettings::default()).map_err(|e| e.to_string())?;
    let key = DeviceKey::generate().map_err(|e| e.to_string())?;
    let pending = dancebrainz_device::pair(&http, &link, &key)
        .await
        .map_err(|e| e.to_string())?;
    let secret = key.secret();
    let device = Device::new(http, link.server.clone(), key);
    device
        .wait_for_approval(pending.expires_at_ms, &dancebrainz_device::POLL_DELAYS)
        .await
        .map_err(|e| e.to_string())?;
    let server = link.server.base().as_str().to_owned();
    let id = SecretId::new(SecretKind::DanceBrainzDeviceKey, server.clone())
        .map_err(|e| e.to_string())?;
    services
        .vault
        .store(&id, &secret)
        .await
        .map_err(|e| e.to_string())?;
    let pairing = dance_edits::Pairing {
        server: server.clone(),
        device_id: pending.device_id,
        approved: true,
        expires_at: None,
    };
    let db = Arc::clone(&services.db);
    blocking(move || db.write(|tx| dance_edits::save_pairing(tx, &pairing, sync::now_ms())))
        .await?;
    tracing::info!("paired with DanceBrainz");
    Ok(server)
}

/// Forgets the pairing and its key. The queue stays for the next pairing.
pub(crate) async fn forget(services: Services) -> Result<(), String> {
    let db = Arc::clone(&services.db);
    let paired = blocking(move || dance_edits::pairing(&db.reader()?)).await?;
    if let Some(pairing) = paired {
        let id = SecretId::new(SecretKind::DanceBrainzDeviceKey, pairing.server)
            .map_err(|e| e.to_string())?;
        services
            .vault
            .delete(&id)
            .await
            .map_err(|e| e.to_string())?;
    }
    let db = Arc::clone(&services.db);
    blocking(move || db.write(dance_edits::forget_pairing)).await?;
    Ok(())
}

/// The paired device, when there is one and its key can be read.
async fn device(services: &Services) -> Result<Option<Device>, String> {
    let db = Arc::clone(&services.db);
    let Some(pairing) = blocking(move || dance_edits::pairing(&db.reader()?)).await? else {
        return Ok(None);
    };
    if !pairing.approved {
        return Ok(None);
    }
    let id = SecretId::new(SecretKind::DanceBrainzDeviceKey, pairing.server.clone())
        .map_err(|e| e.to_string())?;
    let Some(secret): Option<SecretSlice<u8>> =
        services.vault.read(&id).await.map_err(|e| e.to_string())?
    else {
        // The key is gone, for example a session-only vault after a restart.
        return Ok(None);
    };
    let key = DeviceKey::from_secret(&secret).map_err(|e| e.to_string())?;
    let server =
        ServerUrl::parse(&pairing.server, HttpPolicy::HTTPS_ONLY).map_err(|e| e.to_string())?;
    Ok(Some(Device::new(
        http::client(&HttpSettings::default()).map_err(|e| e.to_string())?,
        server,
        key,
    )))
}

/// Sends the queue, then reads the changed recordings again so the cache
/// shows them. `None` when Studio is not paired.
pub(crate) async fn send(services: Services) -> Result<Option<FlushReport>, String> {
    let Some(device) = device(&services).await? else {
        return Ok(None);
    };
    let report = dance_edits::flush(Arc::clone(&services.db), &device)
        .await
        .map_err(|e| e.to_string())?;
    if !report.changed.is_empty() {
        let public = DanceBrainz::new(
            http::client(&HttpSettings::default()).map_err(|e| e.to_string())?,
            device.server().clone(),
        );
        if let Err(error) = public
            .reload(Arc::clone(&services.db), report.changed.clone())
            .await
        {
            // The changes are on DanceBrainz; the cache catches up on the
            // next lookup.
            tracing::info!(%error, "changed recordings could not be read again");
        }
    }
    Ok(Some(report))
}

/// Queues `edits` in one transaction, then tries to send them.
pub(crate) async fn change(
    services: Services,
    edits: Vec<(Mbid, Edit)>,
) -> Result<Option<FlushReport>, String> {
    let db = Arc::clone(&services.db);
    blocking(move || {
        db.write(|tx| {
            let now = sync::now_ms();
            for (mbid, edit) in &edits {
                dance_edits::queue(tx, mbid, edit, now)?;
            }
            Ok(())
        })
    })
    .await?;
    send(services).await
}

/// What `mbid` shows, with the viewer's shoes rating when paired.
pub(crate) async fn view(services: Services, mbid: Mbid) -> Result<TrackView, String> {
    let db = Arc::clone(&services.db);
    let key = mbid.clone();
    let shown = blocking(move || {
        let conn = db.reader()?;
        let tags = dancebrainz::dances_of(&conn, &key)?;
        let edits = dance_edits::edits_of(&conn, &key)?;
        Ok(dance_edits::shown(&tags, &edits))
    })
    .await?;
    let ratings = match device(&services).await {
        Ok(Some(device)) => match device.ratings(&mbid).await {
            Ok(ratings) => Some(ratings),
            Err(error) => {
                tracing::debug!(%error, "shoes could not be read");
                None
            }
        },
        _ => None,
    };
    Ok(TrackView { shown, ratings })
}

/// The cached tags of the tracks the edit dialog changes.
pub(crate) async fn targets(db: Arc<Database>, mbids: Vec<Mbid>) -> Result<Vec<Target>, String> {
    blocking(move || {
        let conn = db.reader()?;
        mbids
            .into_iter()
            .map(|mbid| {
                let cached = dancebrainz::dances_of(&conn, &mbid)?;
                Ok(Target { mbid, cached })
            })
            .collect()
    })
    .await
}

/// The meters DanceBrainz has. Needs the pairing, as all of this does.
pub(crate) async fn meters(services: Services) -> Result<Vec<TagValue>, String> {
    let device = device(&services)
        .await?
        .ok_or_else(|| "Pair with DanceBrainz first.".to_owned())?;
    device.meter_values().await.map_err(|e| e.to_string())
}

/// Dances like `text`.
pub(crate) async fn suggest(services: Services, text: String) -> Result<Vec<Suggestion>, String> {
    let device = device(&services)
        .await?
        .ok_or_else(|| "Pair with DanceBrainz first.".to_owned())?;
    device
        .suggest_dances(&text)
        .await
        .map_err(|e| e.to_string())
}

/// The queue with the titles of its tracks.
pub(crate) async fn queue(db: Arc<Database>) -> Result<Vec<QueueRow>, String> {
    blocking(move || {
        let conn = db.reader()?;
        dance_edits::edits(&conn)?
            .into_iter()
            .map(|edit| {
                let title = dance_edits::title_of(&conn, &edit.mbid)?;
                Ok(QueueRow { edit, title })
            })
            .collect()
    })
    .await
}

/// Removes one change from the queue.
pub(crate) async fn dismiss(db: Arc<Database>, id: i64) -> Result<(), String> {
    blocking(move || db.write(|tx| dance_edits::dismiss(tx, id))).await
}

/// One line for the status bar after a send.
pub(crate) fn report_text(report: &FlushReport) -> String {
    let mut parts = Vec::new();
    if report.sent > 0 {
        parts.push(format!("{} sent", report.sent));
    }
    if report.held > 0 {
        parts.push(format!("{} held for review", report.held));
    }
    if report.conflicts > 0 {
        parts.push(format!(
            "{} not sent, see the DanceBrainz list",
            report.conflicts
        ));
    }
    if report.failed > 0 {
        parts.push(format!("{} not sent yet", report.failed));
    }
    let mut text = if parts.is_empty() {
        "DanceBrainz is up to date.".to_owned()
    } else {
        format!("DanceBrainz changes: {}.", parts.join(", "))
    };
    if let Some(why) = &report.stopped {
        text.push_str(&format!(" Waiting: {why}."));
    }
    text
}

/// The meter as the detail pane shows it.
pub(crate) fn meter_text(meter: Option<&MeterChoice>) -> String {
    meter.map_or_else(|| "No meter known".to_owned(), ToString::to_string)
}

/// `text`, followed by the state of `field` when a change to it is queued,
/// and the error for a tooltip.
pub(crate) fn with_state(shown: &Shown, field: Field, text: String) -> (String, Option<String>) {
    match shown.states.get(&field) {
        Some((state, error)) => (format!("{text} ({})", state.label()), error.clone()),
        None => (text, None),
    }
}

/// The dances as the detail pane shows them, with the queued changes.
pub(crate) fn dances_text(shown: &Shown) -> String {
    if shown.dances.is_empty() {
        return "No dance known".to_owned();
    }
    shown
        .dances
        .iter()
        .map(|dance| {
            let mut notes = Vec::new();
            if let Some(q) = &dance.qualifier {
                notes.push(q.clone());
            }
            if dance.removing {
                notes.push("to be removed".to_owned());
            }
            if let Some(state) = dance.change {
                notes.push(state.label().to_owned());
            }
            if notes.is_empty() {
                dance.name.clone()
            } else {
                format!("{} ({})", dance.name, notes.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The shoes as the detail pane shows them: a change that waits first, then
/// the viewer's rating from DanceBrainz, with what others gave.
pub(crate) fn shoes_text(view: &TrackView) -> String {
    let others = view
        .ratings
        .as_ref()
        .and_then(|r| r.median.filter(|_| r.raters > 0).map(|m| (m, r.raters)))
        .map(|(median, raters)| {
            format!(
                "; median {median:.1} from {raters} {}",
                if raters == 1 { "dancer" } else { "dancers" }
            )
        })
        .unwrap_or_default();
    let mine = match view.shown.shoes {
        Some(pending) => pending,
        None => view.ratings.as_ref().and_then(|r| r.mine),
    };
    let base = match mine {
        Some(value) => format!("{value} of 5 (yours)"),
        None if view.shown.shoes.is_some() => "Your rating is cleared".to_owned(),
        None => "Not rated by you".to_owned(),
    };
    format!("{base}{others}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_send_is_summed_up_in_one_line() {
        assert_eq!(
            report_text(&FlushReport::default()),
            "DanceBrainz is up to date."
        );
        let report = FlushReport {
            sent: 2,
            held: 1,
            conflicts: 1,
            failed: 1,
            stopped: Some("DanceBrainz could not be reached".to_owned()),
            changed: Vec::new(),
        };
        assert_eq!(
            report_text(&report),
            "DanceBrainz changes: 2 sent, 1 held for review, 1 not sent, see the DanceBrainz \
             list, 1 not sent yet. Waiting: DanceBrainz could not be reached."
        );
    }

    #[test]
    fn dances_show_their_queued_changes() {
        use studio_core::dance_edits::{ShownDance, State};
        let mut shown = Shown::default();
        assert_eq!(dances_text(&shown), "No dance known");
        shown.dances = vec![
            ShownDance {
                name: "Bourrée".to_owned(),
                qualifier: Some("both".to_owned()),
                change: None,
                removing: false,
            },
            ShownDance {
                name: "Mazurka".to_owned(),
                qualifier: None,
                change: Some(State::Queued),
                removing: true,
            },
        ];
        assert_eq!(
            dances_text(&shown),
            "Bourrée (both); Mazurka (to be removed, not sent yet)"
        );
        shown
            .states
            .insert(Field::Meter, (State::Conflict, Some("why".to_owned())));
        assert_eq!(
            with_state(&shown, Field::Meter, "8/4".to_owned()),
            (
                "8/4 (changed on DanceBrainz, not sent)".to_owned(),
                Some("why".to_owned())
            )
        );
        assert_eq!(
            with_state(&shown, Field::Shoes, "x".to_owned()),
            ("x".to_owned(), None)
        );
    }

    #[test]
    fn shoes_show_a_waiting_change_before_the_rating() {
        let mut view = TrackView {
            shown: Shown::default(),
            ratings: Some(Ratings {
                mine: Some(2),
                median: Some(3.5),
                raters: 4,
            }),
        };
        assert_eq!(
            shoes_text(&view),
            "2 of 5 (yours); median 3.5 from 4 dancers"
        );
        view.shown.shoes = Some(Some(5));
        assert!(shoes_text(&view).starts_with("5 of 5 (yours)"));
        view.shown.shoes = Some(None);
        assert!(shoes_text(&view).starts_with("Your rating is cleared"));
        assert_eq!(shoes_text(&TrackView::default()), "Not rated by you");
    }

    #[test]
    fn a_meter_shows_its_sub_meter() {
        let meter = MeterChoice {
            meter: "8/4".to_owned(),
            sub: Some("3+3+2".to_owned()),
        };
        assert_eq!(meter_text(Some(&meter)), "8/4 - 3+3+2");
        assert_eq!(meter_text(None), "No meter known");
    }
}
