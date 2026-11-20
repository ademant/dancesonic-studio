-- Schema version 6: server tracks kept offline (#17).
--
-- The user marks albums and playlists "keep offline"; their songs are
-- downloaded in their original format into Studio's offline folder and play
-- without the server. Pins and copies are keyed by the server's address, not
-- its row id, so they can outlive a sign-out when the user chooses to keep
-- them and are found again after signing in once more.
--
-- Files are named by a SHA-256 of server and song id, never by anything the
-- server sends; `sha256` is the digest of the file's content, for integrity
-- checks. Times are epoch milliseconds.

CREATE TABLE offline_pin (
    base_url   TEXT    NOT NULL CHECK (length(base_url) BETWEEN 1 AND 2048),
    kind       TEXT    NOT NULL CHECK (kind IN ('album', 'playlist')),
    item_id    TEXT    NOT NULL CHECK (length(item_id) BETWEEN 1 AND 512),
    pinned_at  INTEGER NOT NULL CHECK (pinned_at >= 0),
    PRIMARY KEY (base_url, kind, item_id)
) STRICT;

CREATE TABLE offline_track (
    base_url     TEXT    NOT NULL CHECK (length(base_url) BETWEEN 1 AND 2048),
    song_id      TEXT    NOT NULL CHECK (length(song_id) BETWEEN 1 AND 512),
    file_name    TEXT    NOT NULL UNIQUE CHECK (length(file_name) = 70 AND file_name LIKE '%.audio'),
    size_bytes   INTEGER NOT NULL CHECK (size_bytes > 0),
    sha256       TEXT    NOT NULL CHECK (length(sha256) = 64),
    stored_at    INTEGER NOT NULL CHECK (stored_at >= 0),
    PRIMARY KEY (base_url, song_id)
) STRICT;

-- One row: where offline copies live and how much space they may take.
-- A NULL folder is the default under the data directory.
CREATE TABLE offline_settings (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    folder       TEXT    CHECK (folder IS NULL OR (length(folder) BETWEEN 2 AND 4096 AND folder LIKE '/%')),
    limit_bytes  INTEGER NOT NULL CHECK (limit_bytes > 0)
) STRICT;
INSERT INTO offline_settings (id, folder, limit_bytes) VALUES (1, NULL, 10737418240);
