-- Schema version 7: phones paired for remote control (#19, #20, #24).
--
-- One row per phone accepted in Studio. Only public keys are stored here; the
-- private keys stay on the phone, and Studio's own keys are in the Secret
-- Service. `last_counter` is the highest message counter accepted from the
-- phone, on any transport: a message must carry a higher one, so a replay is
-- refused even after a restart or a switch between Bluetooth and the network.
-- A revoked phone keeps its row (so its key stays refused) until the user
-- removes it. Times are epoch milliseconds.

CREATE TABLE remote_device (
    id              TEXT    PRIMARY KEY CHECK (length(id) = 32),
    name            TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    device_key      BLOB    NOT NULL CHECK (length(device_key) = 65),
    noise_key       BLOB    NOT NULL UNIQUE CHECK (length(noise_key) = 32),
    last_counter    INTEGER NOT NULL CHECK (last_counter >= 0),
    paired_at       INTEGER NOT NULL CHECK (paired_at >= 0),
    last_seen_at    INTEGER CHECK (last_seen_at >= 0),
    last_transport  TEXT    CHECK (last_transport IN ('ble', 'network')),
    revoked_at      INTEGER CHECK (revoked_at >= 0)
) STRICT;
