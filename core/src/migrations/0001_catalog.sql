-- Schema version 1: servers and the catalog they mirror.
--
-- Catalog rows (artist, album, song, playlist) belong to a server and are
-- deleted with it. References between catalog rows (album.artist_id,
-- song.album_id, song.artist_id) are plain indexed columns, not foreign keys:
-- the server is the authority for its catalog, and a sync must not fail
-- because the server sends a song before its album or keeps a song whose
-- album it removed. Queries join them with LEFT JOIN.
--
-- Times are epoch milliseconds. Server ids and MBIDs are TEXT.

CREATE TABLE server (
    id            INTEGER PRIMARY KEY,
    base_url      TEXT    NOT NULL UNIQUE CHECK (length(base_url) BETWEEN 1 AND 2048),
    created_at    INTEGER NOT NULL,
    -- serverTime of the last applied change feed answer; NULL before the first sync.
    sync_since    INTEGER,
    -- When the last sync finished; NULL before the first sync.
    synced_at     INTEGER
) STRICT;

CREATE TABLE artist (
    server_id     INTEGER NOT NULL REFERENCES server (id) ON DELETE CASCADE,
    id            TEXT    NOT NULL CHECK (length(id) BETWEEN 1 AND 512),
    name          TEXT    NOT NULL,
    sort_name     TEXT,
    mbid          TEXT,
    synced_at     INTEGER NOT NULL,
    PRIMARY KEY (server_id, id)
) STRICT;
CREATE INDEX artist_name ON artist (server_id, coalesce(sort_name, name) COLLATE NOCASE);
CREATE INDEX artist_synced ON artist (server_id, synced_at);

CREATE TABLE album (
    server_id     INTEGER NOT NULL REFERENCES server (id) ON DELETE CASCADE,
    id            TEXT    NOT NULL CHECK (length(id) BETWEEN 1 AND 512),
    artist_id     TEXT,
    artist_name   TEXT,
    name          TEXT    NOT NULL,
    sort_name     TEXT,
    year          INTEGER CHECK (year BETWEEN 0 AND 9999),
    mbid          TEXT,
    cover_art     TEXT,
    synced_at     INTEGER NOT NULL,
    PRIMARY KEY (server_id, id)
) STRICT;
CREATE INDEX album_artist ON album (server_id, artist_id);
CREATE INDEX album_synced ON album (server_id, synced_at);

CREATE TABLE song (
    server_id     INTEGER NOT NULL REFERENCES server (id) ON DELETE CASCADE,
    id            TEXT    NOT NULL CHECK (length(id) BETWEEN 1 AND 512),
    album_id      TEXT,
    artist_id     TEXT,
    title         TEXT    NOT NULL,
    album_name    TEXT,
    artist_name   TEXT,
    track         INTEGER CHECK (track >= 0),
    disc          INTEGER CHECK (disc >= 0),
    year          INTEGER CHECK (year BETWEEN 0 AND 9999),
    duration_ms   INTEGER CHECK (duration_ms >= 0),
    -- Recording MBID: the key for DanceBrainz.
    mbid          TEXT,
    -- Tempo from folksonic only (ademant/dancesonic#82).
    bpm           REAL    CHECK (bpm BETWEEN 20 AND 300),
    tempo_confidence REAL CHECK (tempo_confidence BETWEEN 0 AND 1),
    bars_per_min  REAL    CHECK (bars_per_min > 0),
    bar_pulses    INTEGER CHECK (bar_pulses > 0),
    track_gain    REAL,
    track_peak    REAL    CHECK (track_peak >= 0),
    size_bytes    INTEGER CHECK (size_bytes >= 0),
    suffix        TEXT,
    cover_art     TEXT,
    synced_at     INTEGER NOT NULL,
    PRIMARY KEY (server_id, id)
) STRICT;
CREATE INDEX song_album ON song (server_id, album_id, disc, track);
CREATE INDEX song_artist ON song (server_id, artist_id);
CREATE INDEX song_mbid ON song (mbid) WHERE mbid IS NOT NULL;
CREATE INDEX song_synced ON song (server_id, synced_at);

-- Full-text search over songs, kept in sync by triggers.
CREATE VIRTUAL TABLE song_fts USING fts5 (
    title, artist_name, album_name,
    content = 'song', content_rowid = 'rowid',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER song_fts_insert AFTER INSERT ON song BEGIN
    INSERT INTO song_fts (rowid, title, artist_name, album_name)
    VALUES (new.rowid, new.title, new.artist_name, new.album_name);
END;
CREATE TRIGGER song_fts_delete AFTER DELETE ON song BEGIN
    INSERT INTO song_fts (song_fts, rowid, title, artist_name, album_name)
    VALUES ('delete', old.rowid, old.title, old.artist_name, old.album_name);
END;
CREATE TRIGGER song_fts_update AFTER UPDATE OF title, artist_name, album_name ON song BEGIN
    INSERT INTO song_fts (song_fts, rowid, title, artist_name, album_name)
    VALUES ('delete', old.rowid, old.title, old.artist_name, old.album_name);
    INSERT INTO song_fts (rowid, title, artist_name, album_name)
    VALUES (new.rowid, new.title, new.artist_name, new.album_name);
END;

CREATE TABLE playlist (
    server_id     INTEGER NOT NULL REFERENCES server (id) ON DELETE CASCADE,
    id            TEXT    NOT NULL CHECK (length(id) BETWEEN 1 AND 512),
    name          TEXT    NOT NULL,
    synced_at     INTEGER NOT NULL,
    PRIMARY KEY (server_id, id)
) STRICT;
CREATE INDEX playlist_synced ON playlist (server_id, synced_at);

-- Entries arrive with their playlist, so they reference it with a real
-- foreign key. song_id stays a plain column (see the note at the top).
CREATE TABLE playlist_entry (
    server_id     INTEGER NOT NULL,
    playlist_id   TEXT    NOT NULL,
    position      INTEGER NOT NULL CHECK (position >= 0),
    song_id       TEXT    NOT NULL,
    PRIMARY KEY (server_id, playlist_id, position),
    FOREIGN KEY (server_id, playlist_id) REFERENCES playlist (server_id, id) ON DELETE CASCADE
) STRICT;
CREATE INDEX playlist_entry_song ON playlist_entry (server_id, song_id);
