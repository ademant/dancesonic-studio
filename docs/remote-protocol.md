# DanceSonic remote control protocol, version 1

The protocol between the DanceSonic Android app (the *phone*) and DanceSonic
Studio (*Studio*), so the phone can control playback on the desktop
(ademant/dancesonic-studio#19, ademant/dancesonic#88). The same messages run
over Bluetooth Low Energy, through folksonic's relay and over the local
network. Neither side ever needs an IP address typed or stored.

Machine-readable parts:

- [`remote-protocol.schema.json`](remote-protocol.schema.json): JSON Schema of
  every message.
- [`remote-protocol.openapi.yaml`](remote-protocol.openapi.yaml): the HTTPS
  endpoints of the network transport.

The reference implementation is `core/src/remote/` in this repository.

## Conventions

- Binary values are base64url without padding (`b64u`).
- `u64 BE` is an unsigned 64-bit integer, big-endian.
- `||` is byte concatenation. Labels such as `"dancesonic-cmd-v1\0"` are the
  ASCII bytes followed by one zero byte.
- Messages are JSON objects with a `type` field. Unknown fields are refused;
  unknown `type` values are answered with an error and otherwise ignored.
- Times are Unix seconds (`exp`) or milliseconds (`*_ms`).

## Keys

**Studio** has one long-term identity, created once and kept in the Secret
Service until the user resets it:

| Key | Algorithm | Use |
|---|---|---|
| TLS key | ECDSA P-256 | Studio's self-signed TLS certificate (network) |
| Noise key | X25519 | Studio's static key in the Noise handshake (Bluetooth) |
| BLE service | random UUID v4 | the GATT service Studio advertises |

Studio's **fingerprint** binds both public keys:

```
fp = SHA-256("dancesonic-studio-id-v1\0" || SPKI_DER(TLS key) || noise_public)
```

`SPKI_DER` is the DER `SubjectPublicKeyInfo` of the TLS key, exactly as in the
certificate. `noise_public` is the 32-byte X25519 public key.

The **phone** creates two keys per paired Studio, never shared between Studios:

| Key | Algorithm | Where on Android | Use |
|---|---|---|---|
| device key | ECDSA P-256 | Keystore, non-exportable | signs pairing, `hello` and every command |
| Noise key | X25519 | app storage, encrypted with a Keystore key | the phone's static key on Bluetooth |

`device_key` on the wire is the 65-byte uncompressed SEC1 point.
`noise_key` is the 32-byte X25519 public key. Signatures are ECDSA P-256 with
SHA-256, DER-encoded, as `java.security.Signature("SHA256withECDSA")` makes them.

## Pairing

Pairing happens only by scanning the QR code Studio shows under "Pair a phone",
followed by the user accepting the phone in Studio. One pairing covers both
transports. There is no Bluetooth system pairing (bonding).

### QR code

```
dancesonic-studio:pair?v=1&fp=<b64u 32>&s=<b64u 16>&exp=<unix s>&ble=<uuid>&name=<text>
```

| Field | Meaning |
|---|---|
| `v` | protocol version, `1` |
| `fp` | Studio's fingerprint |
| `s` | one-time pairing secret, 16 random bytes |
| `exp` | when the secret expires: 5 minutes after the code is shown |
| `ble` | Studio's BLE service UUID |
| `name` | Studio's display name, at most 64 characters; only for display |

The code holds no IP address, API key or server credential. A new secret is
made every time "Pair a phone" is opened. A secret is used once: the first
valid proof consumes it, whether the user then accepts or rejects the phone.
After 5 wrong proofs the secret is void.

### Flow

1. The phone scans the code, checks `v` and `exp`, and creates its device key
   and Noise key.
2. It connects over Bluetooth or the network (see Transports) and checks
   Studio's fingerprint. On a mismatch it stops: this is not the Studio that
   showed the code.
3. It sends `pair_request`:

   ```
   T = "dancesonic-pair-v1\0" || fp || device_key || noise_key || binding
   proof = HMAC-SHA256(key = s, T)
   sig = ECDSA(device key, T)
   ```

   `binding` is the 32-byte channel binding of the connection (see
   Transports), so a proof cannot be replayed on another connection.
4. Studio checks the proof in constant time, the signature and the expiry, then
   answers `pair_pending` and shows the phone's `name` to the user.
5. The user accepts (`pair_accepted` with the `device` id) or rejects
   (`pair_rejected`). Studio stores `device_key`, `noise_key` and the name; the
   phone stores `fp`, the BLE service UUID and the device id.

The connection stays open after `pair_accepted`; the phone continues with
`hello`.

### Through folksonic, by comparing codes (#38)

A phone signed in to the same folksonic account as Studio can pair without the
QR code, over the relay only:

1. The phone reads Studio's record from `GET /api/relay/devices` (see the
   relay transport) and takes `fp` from it. The record is only a claim: the
   server could have replaced it.
2. It opens a relay session (Noise XX, prologue bound to that `fp`) and checks
   `identity` against `fp` as on Bluetooth.
3. It sends `pair_code_request`, signed like a `pair_request` but without a
   proof, since there is no secret:

   ```
   T = "dancesonic-pair-code-v1\0" || fp || device_key || noise_key || binding
   sig = ECDSA(device key, T)
   ```

   `noise_key` must be the Noise static key of this session.
4. Both sides show the comparison code of the session:

   ```
   h = SHA-256("dancesonic-compare-v1\0" || binding)
   code = (big-endian u32 of h[0..4]) mod 1 000 000, as six digits
   ```

   Studio answers `pair_pending` and asks the user, showing the phone's name
   and the code. The user allows the phone only if both screens show the same
   code. A server in the middle runs two handshakes with different hashes, so
   the codes differ.
5. As with the QR code: `pair_accepted` or `pair_rejected`.

Studio refuses `pair_code_request` on any other transport
(`not_on_transport`), takes at most one a second per connection, and lets only
one wait for the user at a time (`rate_limited` otherwise).

## Sessions and commands

### `hello`

Every connection of a paired phone starts with `hello`:

```
H = "dancesonic-hello-v1\0" || device || u64 BE(counter) || binding
hello = { "type": "hello", "device": <id>, "counter": <n>, "sig": ECDSA(device key, H) }
```

Studio answers `welcome` with its name, the protocol version, the transport and
the features it offers on it.

### Commands

```
C = "dancesonic-cmd-v1\0" || device || u64 BE(counter) || body
command = { "type": "command", "counter": <n>, "body": "<JSON text>", "sig": ECDSA(device key, C) }
```

`body` is a JSON **string** holding the command object. The signature covers
its exact UTF-8 bytes, so no canonical JSON is needed. `device` is the id from
`hello`.

### Counter

Each phone keeps one counter per Studio, across both transports and across app
restarts. It increases by at least 1 for every `hello` and every command.
Studio stores the last counter it accepted per phone, durably, *before* acting
on a message. A message whose counter is not greater than the stored one is
refused (`error` with code `replay`). A transport switch therefore cannot
replay or lose a command: the phone simply continues counting on the other
transport.

### Replies and events

Every command gets one `result` with `id` equal to the command's counter:
`ok: true` with optional `data`, or `ok: false` with an `error` code.

Studio sends `state` whenever playback changes, and every 5 seconds while
playing. Song, dance and playlist references are IDs only: the folksonic song
ID for server tracks, or `local:<n>` for a track that exists only in Studio's
local folders. The phone resolves names and covers from its own library cache.

### Command bodies

| `cmd` | Fields | Transports |
|---|---|---|
| `play`, `pause`, `toggle`, `stop`, `next`, `previous` | | both |
| `seek` | `position_ms` | both |
| `set_rate` | `rate`, 0.5 to 1.5 | both |
| `set_bpm` | `bpm`, 20 to 300: the target BPM; Studio sets the rate from the track's BPM | both |
| `play_songs` | `songs` (at most 1000 IDs), `start` | both |
| `enqueue` | `songs` (at most 1000 IDs); when nothing is queued they start playing | both |
| `play_dance` | `tag_id`: plays the dance's songs | both |
| `next_of_dance` | `tag_id`: plays a song of this dance next, now | both |
| `play_playlist` | `playlist`, `start` | both |
| `get_state` | | both |
| `library_artists`, `library_playlists`, `library_dances` | | network |
| `library_albums` | `artist` | network |
| `library_songs` | one of `album`, `artist`, `playlist`, `dance`, `query` | network |
| `cover` | `id`: answer `data.image` (b64u, at most 256 KiB) and `data.content_type` | network |

Answers of the library commands (`data`):

| `cmd` | `data` |
|---|---|
| `library_artists` | `[{ "id", "name" }]` |
| `library_albums` | `[{ "id", "name", "year" }]` |
| `library_playlists` | `[{ "id", "name" }]` |
| `library_dances` | `[{ "tag_id", "name" }]` |
| `library_songs` | `[{ "id", "title", "artist", "album", "duration_ms", "bpm" }]`, at most 1000 |

`year`, `artist`, `album`, `duration_ms` and `bpm` may be `null`.

### Features

`welcome.features` lists what the transport offers:

- Bluetooth: `playback`, `tempo`, `dance`, `queue`, `network_hint`
- Network: `playback`, `tempo`, `dance`, `queue`, `library`, `cover`

A command outside the transport's features is refused with
`error: "not_on_transport"`.

## Transports

Phones try the transports in this order: Bluetooth, the folksonic relay, the
local network.

### Bluetooth LE (first)

- **GATT service:** the UUID from the QR code, advertised only while "Allow
  remote control over Bluetooth" is on. The advertisement carries the service
  UUID and nothing else.
- **Characteristics:**
  - `rx` `7c3b0001-5b9e-4c2c-9c6a-2d1f5e0a4b10`: phone to Studio, write and
    write without response.
  - `tx` `7c3b0002-5b9e-4c2c-9c6a-2d1f5e0a4b10`: Studio to phone, notify.
- **Framing:** a message is split into frames of at most `ATT MTU - 3` bytes.
  Each frame is one header byte followed by a chunk. Header bit 0 is `FIN` (the
  message's last frame); the other bits are 0. A message, reassembled, is at
  most 16 KiB plus Noise overhead; a larger one closes the connection.
- **Noise:** `Noise_XX_25519_ChaChaPoly_BLAKE2s`, phone as initiator, Studio as
  responder. Prologue: `"dancesonic-ble-v1\0" || service UUID (16 bytes)`.
  Every protocol message after the handshake is one Noise transport message.
- **Fingerprint check:** Studio's first transport message is
  `identity { tls_spki }`. The phone computes `fp` from `tls_spki` and
  Studio's Noise static key from the handshake, and closes the connection if it
  does not match.
- **Channel binding:** the Noise handshake hash (32 bytes).
- **Phone key:** a paired phone must use the Noise key registered at pairing; an
  unknown key may only send `pair_request`.
- **Network hint:** after `welcome`, and whenever its addresses change, Studio
  sends `network_hint` with its current local addresses and HTTPS port, so the
  phone can open the network transport where mDNS is blocked.

### folksonic relay (second)

For rooms where the phone and Studio cannot see each other but both reach the
same folksonic server (ademant/folksonic#159). Off by default; "Allow remote
control through folksonic" turns it on, and it runs only while Studio is
signed in.

- **Record:** Studio publishes `PUT /api/relay/devices/{id}` with
  `kind: "studio"`, its name and
  `publicKey = b64u(0x01 || noise_public (32) || BLE service UUID (16) || tls_spki)`.
  `id = "studio-" || b64u(SHA-256("dancesonic-relay-id-v1\0" || fp)[0..12])`,
  so it changes when Studio's identity is reset. Studio removes the record
  (`DELETE`) when the switch goes off and before signing out. Only public
  keys are in it.
- **Connection:** each device holds one WebSocket to
  `GET /api/relay/connect?device={id}`, the credential in a header
  (`X-Api-Key` or `DPoP`), never in the URL. HTTPS only, unless the server was
  added with the warned plain-http opt-in for a private address.
- **Relay frames:** binary messages `[id length (1)][device id][payload]`;
  the id is the recipient when sending and the sender when receiving. At most
  16 KiB per message and about 25 per second. A text message
  `{"type":"undelivered","to":…}` from folksonic means the other device is not
  connected; Studio ends that phone's session.
- **Payload:** `[kind (1)][Noise message]`: kind 1 is the phone's first
  handshake message (it starts a new session and replaces any older one from
  that device), kind 3 a later handshake message, kind 2 a transport message.
- **Noise:** `Noise_XX_25519_ChaChaPoly_BLAKE2s`, phone as initiator.
  Prologue: `"dancesonic-relay-v1\0" || fp`. The fingerprint check
  (`identity`), the phone key rule and the channel binding are as on
  Bluetooth.
- **Chunks:** the plaintext of each transport message is `[flags (1)][chunk]`,
  bit 0 of `flags` = `FIN`, the chunk at most 15 KiB. A protocol message is
  at most 64 KiB from the phone; Studio's may be larger (library answers,
  covers). A tampered, replayed, reordered or missing frame fails Noise and
  ends the session.
- **Keepalive:** a phone sends `get_state` at least every 30 seconds; Studio
  ends a relay session silent for 90 seconds. folksonic itself drops a
  connection after 60 seconds without traffic, so both sides ping.
- The relay carries the same features as the local network.

### Local network (third, and for browsing)

- **Discovery:** mDNS service `_dancesonic._tcp`, announced only while "Allow
  remote control" is on. TXT records: `v=1` and
  `fp=<b64u of the first 8 bytes of SHA-256("dancesonic-mdns-v1\0" || fp)>`.
  The phone picks the instance whose `fp` tag matches the paired Studio. The
  tag only selects; the TLS check below authenticates.
- **TLS:** TLS 1.3 only, with Studio's self-signed certificate. The phone does
  not use the system trust store. It reads `noise_key` from `GET /v1/info`
  (unauthenticated) and checks that `fp` computed from the certificate's
  `SubjectPublicKeyInfo` and that `noise_key` equals the pinned one, before
  sending anything else. No plain http.
- **Session:** `GET /v1/session` upgrades to a WebSocket. One message per text
  frame, at most 64 KiB.
- **Channel binding:** the TLS exporter with label
  `EXPORTER-dancesonic-studio-v1`, no context, 32 bytes (RFC 8446 section 7.5).
- Studio accepts connections only from private, link-local, unique-local or
  loopback addresses and closes any other at once. It tries port 47800 first
  and takes a free one when that is in use; phones learn the port from mDNS or
  the network hint.
- mDNS is re-announced whenever Studio's addresses change, for example when a
  laptop moves to another Wi-Fi.

### Switching

If one transport drops, the phone opens the next one and sends `hello` with its
next counter. Studio's play queue and playback do not depend on the connection.
Both sides know the active transport: `welcome.transport` names it, and Studio
shows **BT**, **Relay** or **Wi-Fi**. A phone paired on any transport uses the
same keys and counter on all of them.

## Limits and revocation

- Pairing: 5 wrong proofs per secret; at most 1 `pair_request` or
  `pair_code_request` per second per connection; one code pairing waiting for
  the user at a time.
- Commands: at most 20 per second per phone; more are refused with
  `rate_limited`.
- At most 4 connections at once.
- Messages: 64 KiB on the network and the relay, 16 KiB on Bluetooth,
  checked before parsing.
- Relay: at most 8 phones with a session or a handshake at once.
- Paired phones are listed in Studio and can be revoked. A revoked phone's
  `hello` or command gets `bye` with `reason: "revoked"`, and the connection
  closes.

## Error codes

`bad_message`, `unknown_type`, `not_paired`, `revoked`, `replay`, `bad_signature`,
`bad_proof`, `expired`, `secret_used`, `rate_limited`, `not_on_transport`,
`unknown_song`, `failed`.

## Security notes

- A transport link is never the only proof: every command is signed with the
  device key and counted.
- Scanning the QR code alone never pairs: the user accepts the phone in Studio.
- folksonic sees only who talks to whom, and when: every relay message is
  Noise ciphertext. It cannot pair a phone of its own unnoticed, because the
  user compares the codes.
- Nothing secret is logged: no QR content, pairing secret, proof, signature or
  key.
- The network hint is sent only inside the Noise session, never in the
  advertisement.
