//! Changes to DanceBrainz made in Studio: dances, meter and shoes (#16).
//!
//! Every change goes into one queue in the database first, so it survives a
//! restart and waits while DanceBrainz cannot be reached. The queue is sent
//! in order with the paired device ([`crate::dancebrainz_device`]). Each
//! queued change keeps what was shown when it was made; before it is sent,
//! the track is read again, and a change whose field DanceBrainz changed in
//! the meantime is not sent but marked as a conflict, as the phone does
//! (ademant/dancesonic#75). A change a moderated server holds for review
//! stays in the queue as held until DanceBrainz decides it.
//!
//! Dances are added and removed one by one, so a bulk edit of a selection
//! keeps each track's other dances. Meter and shoes are set; a newer setting
//! replaces one that was not sent yet.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::dancebrainz::{DANCE_KIND, DanceTag, Mbid, Qualifier};
use crate::dancebrainz_device::{
    ChangeState, CurrentTag, Device, DeviceError, METER_KIND, TagValue, Written,
};
use crate::db::{Database, DbError};

/// Most changes the queue holds.
pub const MAX_QUEUE: i64 = 10_000;

/// Longest dance or meter name, path or error kept.
const MAX_TEXT: usize = 512;

/// The fields Studio edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    Dance,
    Meter,
    Shoes,
}

impl Field {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dance => "dance",
            Self::Meter => "meter",
            Self::Shoes => "shoes",
        }
    }
}

/// A meter, and the sub-meter under it when one is chosen. Names contain
/// `/` (`8/4`), so the two are kept apart rather than joined in a path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeterChoice {
    pub meter: String,
    #[serde(default)]
    pub sub: Option<String>,
}

impl std::fmt::Display for MeterChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.sub {
            Some(sub) => write!(f, "{} - {sub}", self.meter),
            None => f.write_str(&self.meter),
        }
    }
}

/// One change to one recording.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Edit {
    /// Adds the dance at `path` (from the autocomplete), keeping the others.
    AddDance {
        path: String,
        name: String,
        qualifier: String,
    },
    /// Removes the dance `name`. `shown` are the dances shown when the
    /// change was made.
    RemoveDance { name: String, shown: Vec<String> },
    /// Sets the meter, or clears it with `None`. `shown` is the meter shown
    /// when the change was made.
    SetMeter {
        to: Option<MeterChoice>,
        shown: Option<MeterChoice>,
    },
    /// Sets the viewer's shoes rating, 1 to 5, or clears it.
    SetShoes { to: Option<u8> },
}

impl Edit {
    pub fn field(&self) -> Field {
        match self {
            Self::AddDance { .. } | Self::RemoveDance { .. } => Field::Dance,
            Self::SetMeter { .. } => Field::Meter,
            Self::SetShoes { .. } => Field::Shoes,
        }
    }

    /// Checks a change before it is queued.
    fn check(&self) -> Result<(), DbError> {
        let bad = |what: &str| {
            Err(DbError::Invalid(format!(
                "the {what} of a DanceBrainz change"
            )))
        };
        let ok_text = |t: &str| !t.trim().is_empty() && t.len() <= MAX_TEXT;
        match self {
            Self::AddDance {
                path,
                name,
                qualifier,
            } => {
                if !ok_text(path) || !ok_text(name) {
                    return bad("dance");
                }
                if Qualifier::parse(qualifier).is_none() {
                    return bad("qualifier");
                }
            }
            Self::RemoveDance { name, shown } => {
                if !ok_text(name) || shown.len() > 100 || !shown.iter().all(|s| ok_text(s)) {
                    return bad("dance");
                }
            }
            Self::SetMeter { to, shown } => {
                for choice in [to, shown].into_iter().flatten() {
                    if !ok_text(&choice.meter) || choice.sub.as_deref().is_some_and(|s| !ok_text(s))
                    {
                        return bad("meter");
                    }
                }
            }
            Self::SetShoes { to } => {
                if to.is_some_and(|v| !(1..=5).contains(&v)) {
                    return bad("shoes rating");
                }
            }
        }
        Ok(())
    }

    /// A short description for the queue list.
    pub fn describe(&self) -> String {
        match self {
            Self::AddDance {
                name, qualifier, ..
            } => format!("Add dance {name} ({qualifier})"),
            Self::RemoveDance { name, .. } => format!("Remove dance {name}"),
            Self::SetMeter { to: Some(to), .. } => format!("Set meter to {to}"),
            Self::SetMeter { to: None, .. } => "Clear the meter".to_owned(),
            Self::SetShoes { to: Some(to) } => format!("Set shoes to {to} of 5"),
            Self::SetShoes { to: None } => "Clear the shoes rating".to_owned(),
        }
    }
}

/// Where a queued change stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// Not sent yet.
    Queued,
    /// Sent; a moderated server holds it for review.
    Held,
    /// DanceBrainz changed the field since; not sent.
    Conflict,
    /// A moderator refused it.
    Rejected,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Held => "held",
            Self::Conflict => "conflict",
            Self::Rejected => "rejected",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "queued" => Some(Self::Queued),
            "held" => Some(Self::Held),
            "conflict" => Some(Self::Conflict),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }

    /// The words the window shows next to a field.
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "not sent yet",
            Self::Held => "held for review",
            Self::Conflict => "changed on DanceBrainz, not sent",
            Self::Rejected => "rejected by a moderator",
        }
    }

    /// Whether the user has to look at it: it will not go away by itself.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Conflict | Self::Rejected)
    }
}

/// One change in the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedEdit {
    pub id: i64,
    pub mbid: Mbid,
    pub edit: Edit,
    pub state: State,
    /// Why the last send failed, or what the conflict is.
    pub error: Option<String>,
    pub change_id: Option<i64>,
    pub queued_at: i64,
}

const CONFLICT_DANCES: &str = "The dances changed on DanceBrainz since you saw them, so nothing \
     was sent. Dismiss this and remove the dance again if you still want it.";
const CONFLICT_METER: &str = "The meter changed on DanceBrainz since you saw it, so nothing was \
     sent. Dismiss this and choose the meter again if you still want it.";
const METER_GONE: &str =
    "DanceBrainz no longer has this meter. Dismiss this and choose the meter again.";

/// Queues `edit` for `mbid`. A newer meter or shoes setting replaces one not
/// sent yet. Adding a dance whose removal is not sent yet cancels the
/// removal, and the other way round; a removal of a dance DanceBrainz does
/// not have (it is only queued) just cancels the addition.
pub fn queue(tx: &Transaction<'_>, mbid: &Mbid, edit: &Edit, now: i64) -> Result<(), DbError> {
    edit.check()?;
    let field = edit.field();
    let waiting = edits_of(tx, mbid)?
        .into_iter()
        .filter(|e| e.state == State::Queued && e.edit.field() == field);
    let mut drop = Vec::new();
    // A removal of a dance DanceBrainz does not have only cancels its
    // queued addition.
    let mut insert = match edit {
        Edit::RemoveDance { name, shown } => shown.contains(name),
        _ => true,
    };
    for old in waiting {
        let same = match (&old.edit, edit) {
            (Edit::SetMeter { .. }, Edit::SetMeter { .. })
            | (Edit::SetShoes { .. }, Edit::SetShoes { .. }) => true,
            (Edit::AddDance { name: a, .. }, Edit::AddDance { name: b, .. })
            | (Edit::RemoveDance { name: a, .. }, Edit::AddDance { name: b, .. }) => a == b,
            (Edit::RemoveDance { name: a, .. }, Edit::RemoveDance { name: b, .. }) => {
                insert &= a != b;
                false
            }
            (Edit::AddDance { name: a, .. }, Edit::RemoveDance { name: b, .. }) => a == b,
            _ => false,
        };
        if same {
            drop.push(old.id);
        }
    }
    for id in drop {
        tx.execute("DELETE FROM dance_edit WHERE id = ?1", [id])?;
    }
    if !insert {
        return Ok(());
    }
    let count: i64 = tx.query_row("SELECT count(*) FROM dance_edit", [], |r| r.get(0))?;
    if count >= MAX_QUEUE {
        return Err(DbError::Invalid(
            "the DanceBrainz queue is full; send or dismiss changes first".to_owned(),
        ));
    }
    let json =
        serde_json::to_string(edit).map_err(|_| DbError::Invalid("a DanceBrainz change".into()))?;
    tx.prepare_cached(
        "INSERT INTO dance_edit (mbid, field, edit, state, queued_at)
         VALUES (?1, ?2, ?3, 'queued', ?4)",
    )?
    .execute(params![mbid.as_str(), field.as_str(), json, now])?;
    Ok(())
}

/// Every change in the queue, oldest first.
pub fn edits(conn: &Connection) -> Result<Vec<QueuedEdit>, DbError> {
    let mut select = conn.prepare_cached(
        "SELECT id, mbid, edit, state, error, change_id, queued_at FROM dance_edit ORDER BY id",
    )?;
    let rows = select.query_map([], row_of)?;
    collect(rows)
}

/// The changes of one recording, oldest first.
pub fn edits_of(conn: &Connection, mbid: &Mbid) -> Result<Vec<QueuedEdit>, DbError> {
    let mut select = conn.prepare_cached(
        "SELECT id, mbid, edit, state, error, change_id, queued_at FROM dance_edit
         WHERE mbid = ?1 ORDER BY id",
    )?;
    let rows = select.query_map([mbid.as_str()], row_of)?;
    collect(rows)
}

type Row = (
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
    i64,
);

fn row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn collect(rows: impl Iterator<Item = rusqlite::Result<Row>>) -> Result<Vec<QueuedEdit>, DbError> {
    let mut out = Vec::new();
    for row in rows {
        let (id, mbid, edit, state, error, change_id, queued_at) = row?;
        let invalid = || DbError::Invalid("a queued DanceBrainz change".to_owned());
        out.push(QueuedEdit {
            id,
            mbid: Mbid::parse(&mbid).map_err(|_| invalid())?,
            edit: serde_json::from_str(&edit).map_err(|_| invalid())?,
            state: State::parse(&state).ok_or_else(invalid)?,
            error,
            change_id,
            queued_at,
        });
    }
    Ok(out)
}

/// The title of a recording for the queue list: from the server's library,
/// or else from a local file. `None` when neither has it.
pub fn title_of(conn: &Connection, mbid: &Mbid) -> Result<Option<String>, DbError> {
    let title = conn
        .prepare_cached("SELECT title FROM song WHERE mbid = ?1 LIMIT 1")?
        .query_row([mbid.as_str()], |r| r.get::<_, String>(0))
        .optional()?;
    if title.is_some() {
        return Ok(title);
    }
    Ok(conn
        .prepare_cached(
            "SELECT title FROM local_track WHERE mbid = ?1 AND title IS NOT NULL LIMIT 1",
        )?
        .query_row([mbid.as_str()], |r| r.get::<_, String>(0))
        .optional()?)
}

/// How many changes wait to be sent.
pub fn waiting_count(conn: &Connection) -> Result<i64, DbError> {
    Ok(conn.query_row(
        "SELECT count(*) FROM dance_edit WHERE state = 'queued'",
        [],
        |r| r.get(0),
    )?)
}

/// Removes a change from the queue, such as a conflict the user has seen.
pub fn dismiss(tx: &Transaction<'_>, id: i64) -> Result<(), DbError> {
    tx.execute("DELETE FROM dance_edit WHERE id = ?1", [id])?;
    Ok(())
}

/// The state each field of a recording shows: the one that needs the user
/// most, then what is not sent yet, then what is held.
pub fn field_states(edits: &[QueuedEdit]) -> BTreeMap<Field, (State, Option<String>)> {
    let rank = |s: State| match s {
        State::Conflict | State::Rejected => 2,
        State::Queued => 1,
        State::Held => 0,
    };
    let mut out: BTreeMap<Field, (State, Option<String>)> = BTreeMap::new();
    for edit in edits {
        let field = edit.edit.field();
        let better = out
            .get(&field)
            .is_none_or(|(state, _)| rank(edit.state) > rank(*state));
        if better {
            out.insert(field, (edit.state, edit.error.clone()));
        }
    }
    out
}

/// A dance as the window shows it: from the cache, with queued changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownDance {
    pub name: String,
    pub qualifier: Option<String>,
    /// The state of a change to it, when one is in the queue.
    pub change: Option<State>,
    /// The change removes it.
    pub removing: bool,
}

/// What a recording shows: the cached DanceBrainz data with the queued
/// changes laid over it, so a change made offline is visible at once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    pub dances: Vec<ShownDance>,
    pub meter: Option<MeterChoice>,
    /// A shoes rating that waits, as `Some(value)`; `None` when no change
    /// waits and the rating read from DanceBrainz applies.
    pub shoes: Option<Option<u8>>,
    pub states: BTreeMap<Field, (State, Option<String>)>,
}

/// Lays `edits` over the cached `tags` of one recording.
pub fn shown(tags: &[DanceTag], edits: &[QueuedEdit]) -> Shown {
    let mut dances: Vec<ShownDance> = tags
        .iter()
        .filter(|t| t.kind == DANCE_KIND)
        .map(|t| ShownDance {
            name: t.name.clone(),
            qualifier: t.qualifier.map(|q| q.as_str().to_owned()),
            change: None,
            removing: false,
        })
        .collect();
    let mut meter = meter_of(tags);
    let mut shoes = None;
    let open = edits.iter().filter(|e| !e.state.needs_attention());
    for edit in open {
        match &edit.edit {
            Edit::AddDance {
                name, qualifier, ..
            } => match dances.iter_mut().find(|d| &d.name == name) {
                Some(dance) => {
                    dance.qualifier = Some(qualifier.clone());
                    dance.change = Some(edit.state);
                }
                None => dances.push(ShownDance {
                    name: name.clone(),
                    qualifier: Some(qualifier.clone()),
                    change: Some(edit.state),
                    removing: false,
                }),
            },
            Edit::RemoveDance { name, .. } => {
                if let Some(dance) = dances.iter_mut().find(|d| &d.name == name) {
                    dance.change = Some(edit.state);
                    dance.removing = true;
                }
            }
            Edit::SetMeter { to, .. } => meter.clone_from(to),
            Edit::SetShoes { to } => shoes = Some(*to),
        }
    }
    Shown {
        dances,
        meter,
        shoes,
        states: field_states(edits),
    }
}

/// The meter of cached tags: a sub-meter with its meter when there is one.
pub fn meter_of(tags: &[DanceTag]) -> Option<MeterChoice> {
    let meters = tags.iter().filter(|t| t.kind == METER_KIND);
    let mut found = None;
    for tag in meters {
        let parent = tag.path.len().checked_sub(2).and_then(|i| tag.path.get(i));
        match parent {
            Some(meter) => {
                return Some(MeterChoice {
                    meter: meter.clone(),
                    sub: Some(tag.name.clone()),
                });
            }
            None => {
                found.get_or_insert(MeterChoice {
                    meter: tag.name.clone(),
                    sub: None,
                });
            }
        }
    }
    found
}

/// The meter of tags read from DanceBrainz now.
fn current_meter(tags: &[CurrentTag]) -> Option<MeterChoice> {
    let meters = tags.iter().filter(|t| t.kind == METER_KIND);
    let mut found = None;
    for tag in meters {
        match &tag.parent {
            Some(meter) => {
                return Some(MeterChoice {
                    meter: meter.clone(),
                    sub: Some(tag.name.clone()),
                });
            }
            None => {
                found.get_or_insert(MeterChoice {
                    meter: tag.name.clone(),
                    sub: None,
                });
            }
        }
    }
    found
}

/// The value that stands for `choice`: the sub-meter under its meter, or the
/// meter at the top level. A name alone is not enough, since `3+3+2` sits
/// under both `8/4` and `3/4`.
pub fn resolve_meter(values: &[TagValue], choice: &MeterChoice) -> Option<i64> {
    let meter = values
        .iter()
        .find(|v| v.parent_id.is_none() && v.name == choice.meter)?;
    match &choice.sub {
        None => Some(meter.id),
        Some(sub) => values
            .iter()
            .find(|v| v.parent_id == Some(meter.id) && &v.name == sub)
            .map(|v| v.id),
    }
}

/// What one send did.
#[derive(Debug, PartialEq, Eq)]
enum Sent {
    Done,
    Held(Option<i64>),
    StillHeld,
    Conflict(&'static str),
    Rejected,
}

/// What a flush of the queue did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FlushReport {
    /// Changes DanceBrainz took.
    pub sent: usize,
    /// Changes now held for review.
    pub held: usize,
    /// Changes not sent because DanceBrainz changed, or a moderator refused.
    pub conflicts: usize,
    /// Changes that failed and stay queued.
    pub failed: usize,
    /// Why the rest of the queue was not tried, when it stopped early.
    pub stopped: Option<String>,
    /// Recordings whose DanceBrainz data changed, to read again.
    pub changed: Vec<Mbid>,
}

/// Sends the queue in order, and follows the held changes. A change that
/// fails stays queued with its error; when DanceBrainz cannot be reached, or
/// the device is no longer paired, the rest waits for the next try.
pub async fn flush(db: Arc<Database>, device: &Device) -> Result<FlushReport, DbError> {
    let all = blocking(&db, edits).await?;
    let mut report = FlushReport::default();
    let mut meters: Option<Vec<TagValue>> = None;
    for edit in all {
        let result = match edit.state {
            State::Queued => send(device, &edit, &mut meters).await,
            State::Held => follow(device, edit.change_id).await,
            State::Conflict | State::Rejected => continue,
        };
        let id = edit.id;
        match result {
            Ok(Sent::Done) => {
                report.sent += 1;
                if !report.changed.contains(&edit.mbid) {
                    report.changed.push(edit.mbid.clone());
                }
                write(&db, move |tx| dismiss(tx, id)).await?;
            }
            Ok(Sent::Held(change)) => {
                report.held += 1;
                write(&db, move |tx| {
                    tx.execute(
                        "UPDATE dance_edit SET state = 'held', error = NULL, change_id = ?2
                         WHERE id = ?1",
                        params![id, change],
                    )?;
                    Ok(())
                })
                .await?;
            }
            Ok(Sent::StillHeld) => {}
            Ok(Sent::Conflict(why)) => {
                report.conflicts += 1;
                set_state(&db, id, State::Conflict, Some(why.to_owned())).await?;
            }
            Ok(Sent::Rejected) => {
                report.conflicts += 1;
                set_state(&db, id, State::Rejected, None).await?;
            }
            Err(error) => {
                let stop = error.is_offline() || error.is_unpaired();
                if edit.state == State::Queued {
                    report.failed += 1;
                    let text = clip(&error.to_string());
                    write(&db, move |tx| {
                        tx.execute(
                            "UPDATE dance_edit SET error = ?2 WHERE id = ?1",
                            params![id, text],
                        )?;
                        Ok(())
                    })
                    .await?;
                }
                if stop {
                    report.stopped = Some(error.to_string());
                    break;
                }
            }
        }
    }
    Ok(report)
}

async fn send(
    device: &Device,
    queued: &QueuedEdit,
    meters: &mut Option<Vec<TagValue>>,
) -> Result<Sent, DeviceError> {
    let mbid = &queued.mbid;
    match &queued.edit {
        Edit::SetShoes { to } => {
            device.set_shoes(mbid, *to).await?;
            Ok(Sent::Done)
        }
        Edit::AddDance {
            path,
            name,
            qualifier,
        } => {
            let Some(qualifier) = Qualifier::parse(qualifier) else {
                return Err(DeviceError::Invalid);
            };
            let current = device.tags(mbid).await?;
            let there = current
                .iter()
                .any(|t| t.kind == DANCE_KIND && &t.name == name && t.qualifier == Some(qualifier));
            if there {
                return Ok(Sent::Done);
            }
            Ok(written(device.add_dance(mbid, path, qualifier).await?))
        }
        Edit::RemoveDance { name, shown } => {
            let current = device.tags(mbid).await?;
            let dances: Vec<&CurrentTag> =
                current.iter().filter(|t| t.kind == DANCE_KIND).collect();
            if !dances.iter().any(|t| &t.name == name) {
                return Ok(Sent::Done);
            }
            let mut others_now: Vec<&str> = dances
                .iter()
                .map(|t| t.name.as_str())
                .filter(|n| n != name)
                .collect();
            let mut others_shown: Vec<&str> = shown
                .iter()
                .map(String::as_str)
                .filter(|n| n != name)
                .collect();
            others_now.sort_unstable();
            others_shown.sort_unstable();
            if others_now != others_shown {
                return Ok(Sent::Conflict(CONFLICT_DANCES));
            }
            let keep: Vec<(i64, Option<Qualifier>)> = dances
                .iter()
                .filter(|t| &t.name != name)
                .map(|t| (t.tag_id, t.qualifier))
                .collect();
            Ok(written(device.replace(mbid, DANCE_KIND, &keep).await?))
        }
        Edit::SetMeter { to, shown } => {
            let current = current_meter(&device.tags(mbid).await?);
            if &current == to {
                return Ok(Sent::Done);
            }
            if &current != shown {
                return Ok(Sent::Conflict(CONFLICT_METER));
            }
            let Some(choice) = to else {
                return Ok(written(device.replace(mbid, METER_KIND, &[]).await?));
            };
            if meters.is_none() {
                *meters = Some(device.meter_values().await?);
            }
            let values = meters.as_deref().unwrap_or_default();
            let Some(id) = resolve_meter(values, choice) else {
                return Ok(Sent::Conflict(METER_GONE));
            };
            Ok(written(
                device.replace(mbid, METER_KIND, &[(id, None)]).await?,
            ))
        }
    }
}

fn written(written: Written) -> Sent {
    match written {
        Written::Applied => Sent::Done,
        Written::Held(change) => Sent::Held(change),
    }
}

/// Asks how a held change was decided. One without an id cannot be asked
/// for; it stays held until the user dismisses it.
async fn follow(device: &Device, change: Option<i64>) -> Result<Sent, DeviceError> {
    let Some(change) = change else {
        return Ok(Sent::StillHeld);
    };
    Ok(match device.change_state(change).await? {
        ChangeState::Applied => Sent::Done,
        ChangeState::Rejected => Sent::Rejected,
        ChangeState::Pending => Sent::StillHeld,
    })
}

async fn set_state(
    db: &Arc<Database>,
    id: i64,
    state: State,
    error: Option<String>,
) -> Result<(), DbError> {
    write(db, move |tx| {
        tx.execute(
            "UPDATE dance_edit SET state = ?2, error = ?3 WHERE id = ?1",
            params![id, state.as_str(), error],
        )?;
        Ok(())
    })
    .await
}

fn clip(text: &str) -> String {
    let mut end = text.len().min(MAX_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

async fn write<T: Send + 'static>(
    db: &Arc<Database>,
    f: impl FnOnce(&Transaction<'_>) -> Result<T, DbError> + Send + 'static,
) -> Result<T, DbError> {
    let db = Arc::clone(db);
    tokio::task::spawn_blocking(move || db.write(f))
        .await
        .map_err(|_| DbError::Invalid("the database task stopped".to_owned()))?
}

async fn blocking<T: Send + 'static>(
    db: &Arc<Database>,
    f: impl FnOnce(&Connection) -> Result<T, DbError> + Send + 'static,
) -> Result<T, DbError> {
    let db = Arc::clone(db);
    tokio::task::spawn_blocking(move || f(&db.reader()?))
        .await
        .map_err(|_| DbError::Invalid("the database task stopped".to_owned()))?
}

/// This desktop's pairing with DanceBrainz, as the database records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    /// The server's address, the key of the device key in the Secret Service.
    pub server: String,
    pub device_id: i64,
    /// The owner approved the device.
    pub approved: bool,
    /// When the owner's approval window closes, while not approved.
    pub expires_at: Option<i64>,
}

/// The pairing, when there is one.
pub fn pairing(conn: &Connection) -> Result<Option<Pairing>, DbError> {
    Ok(conn
        .query_row(
            "SELECT server, device_id, approved, expires_at FROM dancebrainz_device
             ORDER BY paired_at DESC LIMIT 1",
            [],
            |r| {
                Ok(Pairing {
                    server: r.get(0)?,
                    device_id: r.get(1)?,
                    approved: r.get::<_, i64>(2)? == 1,
                    expires_at: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Records a pairing, replacing any other: Studio pairs with one account.
pub fn save_pairing(tx: &Transaction<'_>, pairing: &Pairing, now: i64) -> Result<(), DbError> {
    tx.execute("DELETE FROM dancebrainz_device", [])?;
    tx.execute(
        "INSERT INTO dancebrainz_device (server, device_id, approved, paired_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            pairing.server,
            pairing.device_id,
            i64::from(pairing.approved),
            now,
            pairing.expires_at
        ],
    )?;
    Ok(())
}

/// Marks the pairing approved.
pub fn approve(tx: &Transaction<'_>) -> Result<(), DbError> {
    tx.execute(
        "UPDATE dancebrainz_device SET approved = 1, expires_at = NULL",
        [],
    )?;
    Ok(())
}

/// Forgets the pairing. The queue stays, to be sent after the next pairing.
pub fn forget_pairing(tx: &Transaction<'_>) -> Result<(), DbError> {
    tx.execute("DELETE FROM dancebrainz_device", [])?;
    Ok(())
}

#[cfg(test)]
mod tests;
