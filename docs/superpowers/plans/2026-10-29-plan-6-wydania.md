# Plan 6: podpisane aktualizacje, pakowanie, wdrożenie serwera — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The client is one self-contained `.exe` that updates itself from the group's own server, accepting only releases signed with an Ed25519 key kept offline; the server runs as a hardened systemd service on Debian.

**Architecture:** The server hosts release files in `data_dir/updates/` and serves them over a second, frozen ALPN (`komunikator-update/1`) on the same QUIC port and pinned certificate, so even a client too old to log in can update. A release is a manifest (version, file name, size, SHA-256) signed by the offline key with a separate `komunikator-release` tool; the client embeds the public key, verifies the manifest and the file hash, refuses anything not newer than itself, renames its running exe aside, writes the new one, restarts, and does so only when the player is not in a conversation.

**Tech Stack:** Rust 2021, quinn (second ALPN), ed25519-dalek, sha2, postcard (all present); systemd unit; no new crates.

**Spec:** `docs/superpowers/specs/2026-10-28-komunikator-glosowy-design.md` — sections 6.8 (server: systemd, single binary, journald), 6.9 (single statically linked exe, auto-update with Ed25519 signature, offline key, only when not in a call, versioned protocol), 4 (anti-goals), 6.3 (hostile server).

## Global Constraints

- The release signing key never touches the server; the server only hosts files. A hostile server must not be able to make a client run anything not signed by the offline key, nor downgrade it (spec 6.9, 6.3).
- The client updates itself only when the player is not in a conversation (spec 6.9).
- The update channel must keep working across protocol versions: `komunikator-update/1` and its message format are frozen forever (spec 6.9).
- Client = single `.exe`, statically linked CRT, no installer (spec 6.9).
- Server: one binary, systemd service, one UDP port, logs to journald, no content in logs (spec 6.8).
- All user-facing text in Polish with correct diacritics.
- No new crates.

## Review Focus

1. A hostile server serves an old but validly signed release → refused as a downgrade (Task 1 `older_or_equal_version_is_refused`).
2. A hostile server serves a manifest naming `..\..\evil.exe` or an absolute path → refused by the server and the client (Task 3 `file_name_must_be_plain`, Task 1 `manifest_file_name_is_plain`).
3. The update arrives while the player is talking in a room with friends → nothing happens until they leave the room or are alone (Task 4 `update_waits_while_in_conversation`).
4. Power loss or a crash halfway through the swap → the next start still has a working exe (Task 4 `interrupted_swap_recovers`).
5. A client too old to log in (VersionMismatch) → it still updates through the frozen channel (Task 4 `version_mismatch_triggers_update_check`; Task 3 tests use the raw ALPN).

---

### Task 1: Signed release manifest

**Files:** Create `crates/protocol/src/update.rs`; Modify `crates/protocol/src/lib.rs`, `crates/protocol/Cargo.toml` (`ed25519-dalek`, `sha2` from the workspace).

**Interfaces:**
- Produces:
  - `pub const UPDATE_ALPN: &[u8] = b"komunikator-update/1";`
  - `#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)] pub struct Manifest { pub version: Version, pub file: String, pub size: u64, pub sha256: [u8; 32] }`; `#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct Version { pub major: u16, pub minor: u16, pub patch: u16 }` with `Version::parse(&str) -> Option<Version>` (`"1.2.3"`) and `Display`.
  - `pub fn sign_manifest(key: &ed25519_dalek::SigningKey, m: &Manifest) -> Vec<u8>` → postcard of `SignedManifest { body: Vec<u8> /* postcard(Manifest) */, signature: [u8; 64] }`, signature over `b"komunikator/release/v1" || body`.
  - `pub fn verify_manifest(public: &[u8; 32], bytes: &[u8]) -> Result<Manifest, UpdateError>` (`verify_strict`; `pub enum UpdateError { Malformed, BadSignature, BadFileName }`). `pub fn plain_file_name(name: &str) -> bool`: 1–64 chars of `[A-Za-z0-9._-]`, not starting with `.`.
  - `pub fn is_newer(candidate: Version, current: Version) -> bool` (strictly greater).
  - `pub const MAX_MANIFEST: usize = 4096; pub const MAX_RELEASE_SIZE: u64 = 64 * 1024 * 1024;`

- [ ] **Step 1: Write failing tests:** `version_parse_and_order`, `sign_verify_roundtrip`, `tampered_body_or_signature_fails`, `wrong_key_fails`, `manifest_file_name_is_plain` (`"..\\evil.exe"`, `"a/b"`, `".hidden"`, `""`, 65 chars → `BadFileName` even with a valid signature), `older_or_equal_version_is_refused`, `oversized_manifest_is_malformed`.
- [ ] **Step 2: Run** `cargo test -p protocol update` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass.
- [ ] **Step 5: Commit** `feat(protocol): signed release manifest`.

### Task 2: Release tool

**Files:** Create `crates/release/Cargo.toml`, `crates/release/src/main.rs`, `crates/release/src/lib.rs` (the workspace picks up `crates/*`).

**Interfaces:**
- Binary `komunikator-release`:
  - `keygen --out <secret-file>` — random 32-byte seed written as hex to a new file (refuses to overwrite); prints the public key hex and a Polish note: keep the file offline, put the public key into `crates/client-win/release-key.pub` before building clients.
  - `public --key <secret-file>` — prints the public key hex.
  - `sign --key <secret-file> --exe <path> --version <x.y.z> --out <dir>` — copies the exe to `<dir>/komunikator-<x.y.z>.exe`, writes `<dir>/manifest.bin`; refuses an exe larger than `MAX_RELEASE_SIZE`; prints where to upload (`data_dir/updates/` on the server).
- Library functions behind the commands for in-process tests: `keygen(path: &Path) -> anyhow::Result<[u8; 32]>`, `sign(key: &Path, exe: &Path, version: &str, out: &Path) -> anyhow::Result<Manifest>`.

- [ ] **Step 1: Write failing tests:** `keygen_refuses_overwrite`, `sign_produces_verifiable_manifest` (verify with the public key of the same seed; file copied byte-identical; hash matches), `sign_rejects_bad_version_and_oversize`.
- [ ] **Step 2: Run** `cargo test -p release` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass.
- [ ] **Step 5: Commit** `feat: offline release signing tool`.

### Task 3: Server update channel

**Files:** Modify `crates/server/src/lib.rs` (`quic_config` advertises both ALPNs), `crates/server/src/conn.rs` (dispatch by negotiated ALPN); Create `crates/server/src/updates.rs`, `crates/server/tests/updates.rs`, `docs/updates.md`.

**Interfaces:**
- Frozen wire format on `komunikator-update/1` (documented in `docs/updates.md`, Polish): the client opens one bidirectional stream, sends one request and finishes its side; the server answers and finishes.
  - Request: byte `1` = manifest; or byte `2` + u8 length + file name bytes = file.
  - Response to `1`: u32 LE length + the bytes of `data_dir/updates/manifest.bin`, or length 0 when there is none.
  - Response to `2`: u64 LE length + file bytes — only when the name equals the `file` field of the current manifest (read without verifying; the server has no key) and passes `plain_file_name`, and the file exists; otherwise length 0.
- No login on this ALPN. Limits: one request per connection; at most 4 concurrent update connections per IP and 20 in total (excess closed); responses bounded by `MAX_MANIFEST` / `MAX_RELEASE_SIZE`. The server reads no path other than `data_dir/updates/manifest.bin` and the manifest-named file.
- Logs: IP, request kind, bytes sent.

- [ ] **Step 1: Write failing tests** (`tests/updates.rs`, raw quinn client with `UPDATE_ALPN`): `no_release_returns_empty`, `serves_manifest_and_named_file` (signed with a test key through `protocol::update`), `file_name_must_be_plain` (a manifest naming `..\\x` → file request returns empty), `other_names_are_refused`, `main_alpn_still_logs_in`, `update_connections_are_limited`.
- [ ] **Step 2: Run** `cargo test -p server --test updates` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass.
- [ ] **Step 5: Commit** `feat(server): frozen update channel`.

### Task 4: Client self-update

**Files:** Create `crates/client-core/src/update.rs`, `crates/client-win/src/updater.rs`, `crates/client-win/release-key.pub`; Modify `crates/client-core/src/{lib.rs, app.rs, net/mod.rs}`, `crates/client-win/src/{lib.rs, gui_main.rs, main.rs}`, `crates/client-win/src/ui/{app.rs, window.rs}`, `README.md`.

**Interfaces:**
- client-core `update`:
  - `pub async fn fetch_manifest(server: &str, fingerprint: [u8; 32]) -> anyhow::Result<Option<Vec<u8>>>` and `pub async fn fetch_file(server: &str, fingerprint: [u8; 32], name: &str, max: u64) -> anyhow::Result<Vec<u8>>` over `UPDATE_ALPN` with the pinned certificate (extend `net::client_config` to take the ALPN), 30 s overall timeout each.
  - `pub enum Decision { UpToDate, Available(Manifest) }`; `pub fn decide(manifest: &Manifest, current: Version) -> Decision`; `pub fn check_file(m: &Manifest, bytes: &[u8]) -> bool` (size and SHA-256).
  - `pub fn may_apply(in_room_with_others: bool) -> bool` — false while I am in a room with at least one other member.
- client-win `updater`:
  - `pub const RELEASE_KEY: &str = include_str!("../release-key.pub");` — the file holds 64 hex chars on its first non-comment line; all zeros (the committed placeholder) disables updates: logged once, no UI error.
  - `pub fn stage(exe: &Path, bytes: &[u8]) -> io::Result<PathBuf>` → `komunikator.exe.new` next to the exe (+ `sync_all`).
  - `pub fn swap(exe: &Path) -> io::Result<()>`: rename the running exe → `.old`, rename `.new` → exe; if the second rename fails, rename `.old` back.
  - `pub fn cleanup_on_start(exe: &Path)`: if the exe is missing but `.old` exists (interrupted swap), rename `.old` back; then delete leftover `.old` and `.new`.
  - `pub fn restart(exe: &Path, args: &[OsString]) -> io::Result<()>` — spawn detached with the same args plus `--updated`; the caller then exits.
- GUI flow (`ui/app.rs`): `cleanup_on_start` first; check at start (before the session connects), every 6 h, and immediately on `Fatal(VersionMismatch)`. `Available` → download, `check_file`, `stage`; apply when `may_apply` (re-checked on every lobby change); notices `pobieranie aktualizacji…`, `aktualizacja gotowa — zostanie zainstalowana, gdy nie będziesz w rozmowie`, `aktualizacja nieudana: <powód>`; after a `--updated` start, `zaktualizowano do wersji X`. Applying = save settings, shut down session and engine, `swap`, `restart`, exit. The single-instance guard must let the restarted process start (release the mutex before spawning).
- CLI: `--check-update` checks and applies once, prints Polish status, exits; the regular CLI never updates itself.
- Current version comes from `env!("CARGO_PKG_VERSION")`.

- [ ] **Step 1: Write failing tests:** client-core `decide_only_newer`, `check_file_size_and_hash`, `update_waits_while_in_conversation`, `fetch_from_test_server` (integration with `server::start` and a signed release in `data_dir/updates`); client-win `interrupted_swap_recovers`, `swap_rolls_back_on_failure`, `zero_key_disables_updates`; app `version_mismatch_triggers_update_check`.
- [ ] **Step 2: Run** `cargo test -p client-core update && cargo test -p client-win updater` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` and `cargo build --release` — Expected: all pass, no warnings; `gui_smoke` passes.
- [ ] **Step 5: Commit** `feat: signed self-update of the Windows client`.

### Task 5: Packaging and server deployment

**Files:** Create `.cargo/config.toml`, `deploy/komunikator.service`, `deploy/komunikator.toml`, `docs/wydanie.md`, `docs/acceptance/plan-6.md`, `crates/client-win/tests/static_crt.rs`; Modify `README.md`.

**Interfaces:**
- `.cargo/config.toml`: `[target.x86_64-pc-windows-msvc] rustflags = ["-C", "target-feature=+crt-static"]`.
- Test `exe_has_no_vcruntime_import` (Windows): parse the PE import directory of the built `komunikator.exe` and `komunikator-cli.exe` by hand with `std` (DOS header → PE header → optional header data directory 1 → import descriptors → DLL names) and assert no `VCRUNTIME*`, `MSVCP*` or `api-ms-win-crt-*` import.
- `deploy/komunikator.service`: `Type=simple`, `User=komunikator`, `ExecStart=/usr/local/bin/komunikator-server run --config /etc/komunikator/komunikator.toml`, `StateDirectory=komunikator`, `StateDirectoryMode=0700`, `Restart=on-failure`, hardening (`NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `ReadWritePaths=/var/lib/komunikator`, empty `CapabilityBoundingSet=`, `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`), `LimitNOFILE=4096`. `deploy/komunikator.toml` = the production config (`data_dir = "/var/lib/komunikator"`, `public_address` placeholder).
- `docs/wydanie.md` (Polish): building the server on Debian (`apt install build-essential pkg-config`, rustup, `cargo build --release -p server`), installing the binary and unit, creating the user (`useradd --system`), enabling the service, opening UDP 7700 (nftables and ufw examples), `journalctl -u komunikator`, first admin (`invite`, `make-admin` as the service user), choosing PL vs FR by measuring ping (spec 5), and the release procedure (offline `keygen`, public key into `release-key.pub`, bump the version, `cargo build --release`, `komunikator-release sign`, copy `manifest.bin` and the exe to `/var/lib/komunikator/updates/`).
- `docs/acceptance/plan-6.md`: players' checks — a release published during a call installs after leaving the room; a tampered file is refused; the exe runs on a clean Windows without the VC++ redistributable; the server survives a reboot.

- [ ] **Step 1: Write failing test** `exe_has_no_vcruntime_import`.
- [ ] **Step 2: Run** `cargo test -p client-win --test static_crt` — Expected: FAIL (dynamic CRT today).
- [ ] **Step 3: Implement** the config, unit file and docs.
- [ ] **Step 4: Run** `cargo test --workspace` and `cargo build --release` — Expected: all pass, no warnings.
- [ ] **Step 5: Commit** `build: static CRT, systemd unit and release docs`.
