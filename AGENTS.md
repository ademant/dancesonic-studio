# AGENTS.md

Guidance for AI coding agents working in this repository.

DanceSonic Studio is a desktop library tool and player for Ubuntu, Xubuntu and Ubuntu Budgie. It plays from a Subsonic-compatible server (folksonic first), enriches tracks with DanceBrainz metadata, analyses local files with folksentia, and can be controlled remotely by the DanceSonic Android app (ademant/dancesonic). Rust, plain GTK4, SQLite, AGPL-3.0. The plan and phases are in the roadmap issue #23.

## Workflow

1. **Discuss first.** For anything beyond a typo or a one-line fix, explain the approach and its trade-offs and wait for the user's yes.
2. **Then file the issue.** Create a GitHub issue (`gh issue create --repo ademant/dancesonic-studio`) only after the user approved that specific proposal, never as a side effect of a general discussion. Give it the matching `phase N: …` label and milestone (see #23). Discussion and issue do not have to be followed by implementation right away.
3. **Implement** on a feature branch, never on `main`. One branch may carry several issues, but each issue gets its own commit(s).
4. **Close by commit.** The commit that completes an issue carries a `Closes #N` line. Every closed issue gets its **own line**; `Closes #3, #5` closes only #3 on GitHub:

   ```
   feat: connect to folksonic with an API key

   Closes #3
   Closes #5

   Co-Authored-By: Claude <model> <noreply@anthropic.com>
   ```

   `<model>` is the model doing the work; do not copy it from an older commit. PR bodies end with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
5. **Verify before every commit** (see Code quality). Never commit with failing checks; never disable or delete a test to get green.
6. **Push, open PRs and merge only when the user asks.** Merge only with green CI. After pushing a commit that closes several issues, check each with `gh issue view N --json state`.

Do not add features beyond the issue's scope. If an issue turns out to be wrong or too large, stop and discuss instead of improvising.

## Architecture

- Cargo workspace with two crates:
  - `core`: server clients, database, sync, playback model (queue, tempo), analysis queue, tagging, remote protocol. No GTK, no GStreamer. Testable on its own.
  - `app`: GTK4 UI, GStreamer playback, MPRIS, BlueZ, Secret Service glue.
- Dependencies point one way: `app` → `core`. `core` defines traits for everything external (HTTP, clock, secret store, BLE, the folksentia process) so tests can fake them.
- **Plain GTK4 through `gtk4-rs`, no libadwaita.** The app must follow the system theme on GNOME (Wayland), Xfce (X11) and Budgie. Standard widgets and symbolic icons only; no hard-coded colours in CSS.
- **Threads:** the GTK main loop never blocks. Network, database and file I/O run off the main thread (a Tokio runtime on worker threads, or `gio` async); results come back through channels (`async-channel`) to `glib::MainContext::spawn_local`. Widgets are touched only on the main thread.
- One source of truth per fact: tempo comes from folksonic (ademant/dancesonic#82); the dance-type model lives in one place in `core`.

## Rust

- Latest stable Rust, **edition 2024**, toolchain pinned in `rust-toolchain.toml`; `rust-version` set in the workspace. Commit `Cargo.lock`.
- Shared lints in `[workspace.lints]`:
  - `unsafe_code = "forbid"`. If a binding ever needs `unsafe`, discuss it first and isolate it with a `// SAFETY:` comment.
  - Clippy: `all` as errors, plus `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`, `print_stdout`, `print_stderr` denied outside tests.
- **Errors:** `thiserror` enums in `core`, one per module boundary, carrying context; `anyhow` only at the top of `app`. Propagate with `?`; no swallowed errors (`let _ =` on a `Result` needs a comment why). No `unwrap()`/`expect()` in production code.
- **Types:** newtypes for IDs (`SongId`, `Mbid`, `DeviceId`), enums for closed sets, `#[non_exhaustive]` where the server may add values. Parse, don't validate: convert untrusted input into typed values at the boundary.
- **Serde:** DTOs separate from domain types; optional server fields as `Option<T>`; `#[serde(default)]` instead of failing on missing lists; unknown fields ignored for server responses, denied for our own protocol messages.
- Async: structured concurrency; every spawned task is owned and cancelled on shutdown. Timeouts on every network call. Retries with backoff only for idempotent requests.
- Logging with `tracing`; `debug` for detail, `info` for lifecycle, no secrets (see Security).
- Small, boring, explicit code. Doc comments on public items; inline comments only for the non-obvious "why".
- **Dependencies:** few, maintained, license-compatible with AGPL-3.0. Every new dependency gets a one-line justification in the commit message or PR. Disable default features you do not need. `cargo deny` checks licenses, advisories, bans and sources.

## SQLite

- `rusqlite` with the `bundled` feature (a known SQLite version with FTS5). One database file under `$XDG_DATA_HOME/dancesonic-studio/`, directory mode `0700`, file mode `0600`.
- **Every connection** sets: `PRAGMA foreign_keys = ON`, `journal_mode = WAL` (once), `synchronous = NORMAL`, `busy_timeout`. Check them in a test.
- **One writer connection** behind a dedicated thread or mutex; separate read-only connections for queries. Writes use `BEGIN IMMEDIATE` transactions and stay short. No database access on the GTK main thread.
- **Parameters only.** Never build SQL from strings with user or server data; `prepare_cached` for repeated statements. Identifiers (table/column names) are never dynamic.
- **Schema:**
  - `STRICT` tables; explicit `NOT NULL`, `CHECK` and `FOREIGN KEY ... ON DELETE` clauses.
  - Server IDs and MBIDs as `TEXT`, timestamps as epoch milliseconds `INTEGER`, booleans as `INTEGER CHECK (x IN (0,1))`.
  - An index on every foreign key and every column used in hot `WHERE`/`JOIN`/`ORDER BY`. Check hot queries with `EXPLAIN QUERY PLAN`.
  - FTS5 for library search, kept in sync by triggers.
- **Migrations:** versioned with `PRAGMA user_version`, forward only, each in one transaction, run at start before anything else. Never destructive for user data (playlists, ratings, edit queue, analysis, pairings). Run `PRAGMA foreign_key_check` after migrating. **Every migration step has a test** that starts from the previous schema with data.
- Audio files (offline tracks) live on disk, not in the database; the database stores their path relative to the storage root, size and hash.

## Security

Studio holds credentials, listens for remote control, runs a subprocess and reads untrusted files and server responses. Treat each as an attack surface.

- **Secrets:** API keys, device keys and pairing keys only in the Secret Service (`oo7`), never in a file, the database or the config. No Secret Service → session-only, and say so in the UI. Hold secrets in `secrecy::SecretString` / `zeroize` types; their `Debug`/`Display` print `[redacted]`.
- **Logging:** never log keys, tokens, signed URLs, full API URLs, QR contents or pairing secrets. Redact before formatting, not after.
- **Server connection:** OpenSubsonic API key auth only (`apikeyauth`), key in the `X-Api-Key` header, never in a URL; no token auth, no `p=`. HTTPS by default via `rustls`; plain http only behind the warned opt-in and only to a private address, enforced in every HTTP client. No redirects across schemes or hosts. Certificate pinning where configured.
- **Remote control (#19, #20, #24):** off by default. Pairing only by the QR code shown in Studio plus explicit acceptance in Studio. Every command authenticated with the phone's device key, with a replay counter that is persisted; revoked phones refused. Rate limits on pairing and commands. Message size limits before parsing. The network listener binds to the local network only; mDNS and BLE advertising only while enabled.
- **Cryptography:** no home-made crypto. Use vetted crates (`rustls`, `snow`, `p256`/`ed25519-dalek`, `getrandom`); compare secrets in constant time (`subtle`).
- **Untrusted input:** server JSON, DanceBrainz data, audio tags, `jobs.json`, folksentia output and protocol messages are untrusted. Bound their size, validate ranges (e.g. BPM 20–300), and never use them to build file paths or SQL. File names from servers are sanitised and confined to the storage root; reject `..`, absolute paths and symlink escapes.
- **Subprocesses (folksentia):** `std::process::Command` with an argument list, never a shell. Resolve the binary explicitly, pass a minimal environment, set a timeout, cap stdout/stderr size, run at low CPU and I/O priority, and kill on cancel.
- **Files:** never delete or modify user music files except through the tag editor (#15) with preview and undo. Write files atomically (temp file + rename).
- Run `cargo deny check` (advisories) in CI; fix or explicitly justify every advisory.

## Testing

- Unit tests next to the code; integration tests in `core/tests/`.
- Network: a mock HTTP server (`wiremock`) for Subsonic, folksonic and DanceBrainz, including `status: failed`, missing optional fields, a server without `apikeyauth`, the http opt-in and pinning.
- Database: in-memory or temp-file databases; a test for each migration step.
- Security paths are tested, not assumed: wrong key, expired or reused pairing secret, replayed and revoked commands, oversized messages, path traversal in file names.
- External programs and services (folksentia, Secret Service, BlueZ, GStreamer where possible) are faked behind traits.
- Every bug fix ships with a regression test.

## Code quality

Before every commit, all of these must pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo deny check
```

If a check cannot be run, say so explicitly instead of reporting the work as done. UI changes are also checked by starting the app (Wayland and, where possible, X11).

## Git

- Feature branches named after the work, e.g. `feat/3-api-key-connect` or `phase-1-foundation` for several issues.
- Do not commit generated files, local configs, databases, keys or certificates.
- A PR description states what changed, why, how it was tested, and any security or migration impact.
