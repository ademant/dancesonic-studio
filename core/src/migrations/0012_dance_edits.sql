-- Schema version 12: contributing dances, meter and shoes to DanceBrainz (#16).
--
-- dancebrainz_device is the pairing of this desktop with a DanceBrainz
-- account. Its private key lives in the Secret Service, never here; this row
-- only says which server and which device id, and whether the owner approved.
--
-- dance_edit is the edit queue. Every change is kept here until DanceBrainz
-- confirmed it, so it survives a restart and can be sent later. `edit` is a
-- JSON object Studio wrote: what to change and what was shown when the change
-- was made, so a send can tell whether DanceBrainz changed in the meantime.
-- `state` is
--   queued    not sent yet (`error` says why the last try failed)
--   held      sent, held for review on a moderated server (`change_id`)
--   conflict  changed on DanceBrainz since, so not sent
--   rejected  refused by a moderator
-- Times are epoch milliseconds. MBIDs are TEXT, lower-case, 36 characters.

CREATE TABLE dancebrainz_device (
    server      TEXT    PRIMARY KEY CHECK (length(server) BETWEEN 1 AND 2048),
    device_id   INTEGER NOT NULL CHECK (device_id > 0),
    approved    INTEGER NOT NULL CHECK (approved IN (0, 1)),
    paired_at   INTEGER NOT NULL CHECK (paired_at >= 0),
    expires_at  INTEGER CHECK (expires_at IS NULL OR expires_at >= 0)
) STRICT;

CREATE TABLE dance_edit (
    id          INTEGER PRIMARY KEY,
    mbid        TEXT    NOT NULL CHECK (length(mbid) = 36),
    field       TEXT    NOT NULL CHECK (field IN ('dance', 'meter', 'shoes')),
    edit        TEXT    NOT NULL CHECK (length(edit) <= 16384 AND json_valid(edit)
                                        AND json_type(edit) = 'object'),
    state       TEXT    NOT NULL DEFAULT 'queued'
                        CHECK (state IN ('queued', 'held', 'conflict', 'rejected')),
    error       TEXT    CHECK (error IS NULL OR length(error) <= 1024),
    change_id   INTEGER CHECK (change_id IS NULL OR change_id > 0),
    queued_at   INTEGER NOT NULL CHECK (queued_at >= 0)
) STRICT;
CREATE INDEX dance_edit_by_track ON dance_edit (mbid, field);
CREATE INDEX dance_edit_by_state ON dance_edit (state, id);
