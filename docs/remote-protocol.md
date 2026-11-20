# DanceSonic remote control protocol, version 1

The protocol between the DanceSonic Android app (the *phone*) and DanceSonic
Studio (*Studio*), so the phone can control playback on the desktop
(ademant/dancesonic-studio#19, ademant/dancesonic#88). The same messages run
over Bluetooth Low Energy and over the local network. Neither side ever needs
an IP address typed or stored.

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

### Bluetooth LE (preferred)

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

### Local network (fallback, and for browsing)

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

If one transport drops, the phone opens the other and sends `hello` with its
next counter. Studio's play queue and playback do not depend on the connection.
Both sides know the active transport: `welcome.transport` names it, and Studio
shows **BT** or **Wi-Fi**.

## Limits and revocation

- Pairing: 5 wrong proofs per secret; at most 1 `pair_request` per second per
  connection.
- Commands: at most 20 per second per phone; more are refused with
  `rate_limited`.
- At most 4 connections at once.
- Messages: 64 KiB on the network, 16 KiB on Bluetooth, checked before parsing.
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
- Nothing secret is logged: no QR content, pairing secret, proof, signature or
  key.
- The network hint is sent only inside the Noise session, never in the
  advertisement.
