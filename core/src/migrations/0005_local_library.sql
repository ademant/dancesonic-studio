-- Schema version 5: local music folders (#10).
--
-- The user picks folders; Studio scans them and reads the tags. Files stay
-- where they are and are never changed or deleted by Studio. A track's path is
-- stored relative to its folder; rows go with their folder. A local track
-- whose recording MBID matches a server song is the same recording: the
-- library shows it once, as available locally and on the server.
--
-- `fingerprint` (SHA-256 of the size and the first and last 64 KiB) finds a
-- file again after it was moved or renamed. Times are epoch milliseconds.

CREATE TABLE local_folder (
    id          INTEGER PRIMARY KEY,
    path        TEXT    NOT NULL UNIQUE CHECK (length(path) BETWEEN 2 AND 4096 AND path LIKE '/%'),
    added_at    INTEGER NOT NULL CHECK (added_at >= 0),
    scanned_at  INTEGER CHECK (scanned_at >= 0)
) STRICT;

CREATE TABLE local_track (
    id            INTEGER PRIMARY KEY,
    folder_id     INTEGER NOT NULL REFERENCES local_folder (id) ON DELETE CASCADE,
    -- Relative to the folder: never absolute, never with a `..` component.
    rel_path      TEXT    NOT NULL CHECK (
                      length(rel_path) BETWEEN 1 AND 4096
                      AND rel_path NOT LIKE '/%'
                      AND rel_path <> '..'
                      AND rel_path NOT LIKE '../%'
                      AND rel_path NOT LIKE '%/../%'
                      AND rel_path NOT LIKE '%/..'),
    size_bytes    INTEGER NOT NULL CHECK (size_bytes >= 0),
    modified_ms   INTEGER NOT NULL,
    fingerprint   TEXT    NOT NULL CHECK (length(fingerprint) = 64),
    title         TEXT,
    artist        TEXT,
    album         TEXT,
    album_artist  TEXT,
    track         INTEGER CHECK (track >= 0),
    disc          INTEGER CHECK (disc >= 0),
    year          INTEGER CHECK (year BETWEEN 0 AND 9999),
    duration_ms   INTEGER CHECK (duration_ms >= 0),
    -- Recording MBID: the key that matches a server song and DanceBrainz.
    mbid          TEXT,
    release_mbid  TEXT,
    artist_mbid   TEXT,
    -- From the file's tag; used only when no server knows the recording.
    bpm           REAL    CHECK (bpm BETWEEN 20 AND 300),
    scanned_at    INTEGER NOT NULL CHECK (scanned_at >= 0),
    UNIQUE (folder_id, rel_path)
) STRICT;
CREATE INDEX local_track_mbid ON local_track (mbid) WHERE mbid IS NOT NULL;
CREATE INDEX local_track_fingerprint ON local_track (folder_id, fingerprint);
CREATE INDEX local_track_artist ON local_track (coalesce(album_artist, artist) COLLATE NOCASE, album COLLATE NOCASE);
