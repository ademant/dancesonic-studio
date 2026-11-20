-- Schema version 2: what DanceBrainz says about recordings, cached locally
-- for enrichment (#9). Nothing here is needed for playback; a lookup that
-- fails leaves the tracks playable and simply without dances.
--
-- A row in dance_lookup records that a recording was looked up, even when it
-- has no tags, so an untagged recording is not asked for again every time.
-- Times are epoch milliseconds. MBIDs are TEXT, lower-case, 36 characters.

CREATE TABLE dance_lookup (
    mbid        TEXT    PRIMARY KEY CHECK (length(mbid) = 36),
    fetched_at  INTEGER NOT NULL CHECK (fetched_at >= 0)
) STRICT;

CREATE TABLE dance_tag (
    mbid        TEXT    NOT NULL REFERENCES dance_lookup (mbid) ON DELETE CASCADE,
    tag_id      INTEGER NOT NULL CHECK (tag_id > 0),
    kind        TEXT    NOT NULL CHECK (length(kind) BETWEEN 1 AND 64),
    name        TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 512),
    -- JSON array of the names from the family down to this tag.
    path        TEXT    NOT NULL CHECK (length(path) BETWEEN 2 AND 65536),
    qualifier   TEXT    CHECK (qualifier IN ('learn', 'disco', 'both')),
    updated_at  TEXT    NOT NULL CHECK (length(updated_at) BETWEEN 1 AND 64),
    PRIMARY KEY (mbid, tag_id)
) STRICT;
CREATE INDEX dance_tag_by_tag ON dance_tag (tag_id);
