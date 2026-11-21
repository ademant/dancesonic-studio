-- Schema version 9: remote control through folksonic's relay (#38).
--
-- The relay is a third switch, off until the user turns it on. A phone's
-- last transport may now be the relay; SQLite cannot change a CHECK in
-- place, so remote_device is rebuilt with its rows (keys, counters,
-- revocations) carried over unchanged. Nothing references the table.

ALTER TABLE remote_settings
    ADD COLUMN relay INTEGER NOT NULL DEFAULT 0 CHECK (relay IN (0, 1));

CREATE TABLE remote_device_v9 (
    id              TEXT    PRIMARY KEY CHECK (length(id) = 32),
    name            TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    device_key      BLOB    NOT NULL CHECK (length(device_key) = 65),
    noise_key       BLOB    NOT NULL UNIQUE CHECK (length(noise_key) = 32),
    last_counter    INTEGER NOT NULL CHECK (last_counter >= 0),
    paired_at       INTEGER NOT NULL CHECK (paired_at >= 0),
    last_seen_at    INTEGER CHECK (last_seen_at >= 0),
    last_transport  TEXT    CHECK (last_transport IN ('ble', 'network', 'relay')),
    revoked_at      INTEGER CHECK (revoked_at >= 0)
) STRICT;

INSERT INTO remote_device_v9
    (id, name, device_key, noise_key, last_counter, paired_at, last_seen_at,
     last_transport, revoked_at)
SELECT id, name, device_key, noise_key, last_counter, paired_at, last_seen_at,
       last_transport, revoked_at
FROM remote_device;

DROP TABLE remote_device;
ALTER TABLE remote_device_v9 RENAME TO remote_device;
