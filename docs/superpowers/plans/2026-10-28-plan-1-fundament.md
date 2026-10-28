# Plan 1 — Fundament: serwer przekazujący audio + klient bez interfejsu

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dwóch (do pięciu) klientów na Windows rozmawia głosowo przez serwer QUIC w jednym pokoju, z otwartym mikrofonem, Opusem, buforem jittera i automatycznym ponownym łączeniem — bez GUI i jeszcze bez E2E.

**Architecture:** Workspace Rust z czterema crate'ami: `protocol` (czysty format wiadomości i datagramów + wspólne stałe), `server` (lib + bin; przekazuje datagramy audio bez dekodowania), `client-core` (wieloplatformowa logika klienta: sieć, kodek, VAD, jitter, mikser — testowalna bez sprzętu audio), `client-win` (WASAPI + spięcie potoku + CLI). Audio płynie datagramami QUIC, sterowanie jednym strumieniem dwukierunkowym z ramkami długość+postcard.

**Tech Stack:** Rust 1.91 (stable, MSVC), `quinn 0.11` + `rustls 0.23` (provider `ring`), `rcgen 0.14`, `tokio 1`, `serde` + `postcard 1`, `opus 0.4` (libopus budowany przez CMake — zainstalowany), `argon2`, `sha2`, `windows 0.62`, `clap 4`, `toml`, `tracing`, `thiserror`, `anyhow`; dev: `tempfile`.

**Spec:** `docs/superpowers/specs/2026-10-28-komunikator-glosowy-design.md`

**Kolejne plany (poza zakresem tego):** 2 — E2E; 3 — wiele pokoi, czat, głośności per osoba, GUI Win32, zasobnik, skróty; 4 — konta w SQLite z zaproszeniami, CLI admina, auto-aktualizacja, systemd. W tym planie użytkownicy są wpisani w konfigurację serwera (z hashami Argon2id) — tymczasowo, do planu 4.

## Global Constraints

- Klient: Windows 10/11 x64 (`x86_64-pc-windows-msvc`). Serwer: Debian (Linux x86_64). Crate'y `protocol`, `server`, `client-core` muszą kompilować się i przechodzić testy także na Windows.
- Żadnego WebView/Electrona/przeglądarki.
- Cele klienta w rozmowie: < 30 MB RAM (working set), < 1% CPU, opóźnienie usta→ucho < 150 ms.
- Jeden port UDP serwera (domyślnie `7700`); QUIC: keep-alive 5 s, idle timeout 15 s, ALPN `komunikator/1`.
- Ponowne łączenie: odstępy 0,5 s → 1 s → 2 s → 4 s → 5 s (maks.), automatyczny powrót do pokoju.
- Audio: 48 kHz mono, ramka 20 ms = 960 próbek, Opus VoIP **CBR 32 kbit/s**, inband FEC, oczekiwana utrata 10%.
- VAD: przy ciszy nie wysyłamy pakietów; „ogon” 300 ms (15 ramek) po końcu mowy.
- Bufor jittera: cel 40–60 ms, rośnie przy niestabilnej sieci.
- Wątki audio: MMCSS „Pro Audio”, bez alokacji w pętli audio.
- Systemowe ściszanie innych aplikacji (ducking) wyłączone dla naszych strumieni.
- Protokół wersjonowany: `PROTOCOL_VERSION = 1` w `Hello`.
- Serwer nie loguje treści.
- Hasła: Argon2id. Maks. 5 nieudanych logowań na IP na 60 s.
- Jedna aktywna sesja na konto: nowe logowanie rozłącza poprzednią.

## Review Focus

1. **Podwójne logowanie tego samego konta** — stara sesja dostaje `SessionReplaced` i **nie** łączy się ponownie (inaczej dwa klienty wyrzucałyby się w pętli). Test: Task 7 `replaced_session_does_not_reconnect`.
2. **Restart serwera / zerwanie sieci w trakcie rozmowy** — klient sam wraca do pokoju, dźwięk płynie dalej. Test: Task 7 `reconnects_after_server_restart`.
3. **Odłączenie słuchawek / zmiana urządzenia w trakcie** — strumień otwiera się ponownie, pętla audio nie staje. Test: Task 11 `supervisor_reopens_after_invalidation`.
4. **Przestawione, zduplikowane, spóźnione pakiety i przekręcenie licznika `u32`** — bufor jittera nie zawiesza się. Test: Task 10 `handles_seq_wraparound`, `drops_duplicates_and_late`.
5. **Śmieciowe lub zbyt duże dane od jednego klienta** (ramka > 64 KiB, błędny postcard, datagram > 1200 B, zalew datagramów) — serwer odrzuca/rozłącza tylko jego, reszta rozmawia dalej. Test: Task 6 `oversized_frame_disconnects_only_offender`, `datagram_flood_is_rate_limited`.

---

## Struktura plików

```
Cargo.toml                                  # workspace, wspólne wersje zależności
server.example.toml
crates/protocol/src/lib.rs                  # PROTOCOL_VERSION, stałe transportu, re-eksporty
crates/protocol/src/control.rs              # ClientMsg, ServerMsg, MemberInfo, RejectReason
crates/protocol/src/framing.rs              # encode_frame, FrameDecoder, MAX_FRAME
crates/protocol/src/audio.rs                # datagramy upstream/downstream
crates/server/src/{lib.rs,main.rs}          # start()/RunningServer; CLI run, hash-password
crates/server/src/config.rs                 # ServerConfig (TOML)
crates/server/src/auth.rs                   # Argon2id, LoginLimiter
crates/server/src/tls.rs                    # certyfikat samopodpisany + odcisk
crates/server/src/room.rs                   # Room<C>
crates/server/src/conn.rs                   # obsługa połączenia, relay, TokenBucket
crates/server/tests/relay.rs
crates/client-core/src/net/{mod.rs,tls.rs,backoff.rs,session.rs}
crates/client-core/src/audio/{mod.rs,frame.rs,codec.rs,mixer.rs,vad.rs,jitter.rs}
crates/client-core/tests/session.rs
crates/client-win/src/main.rs               # komunikator.exe (CLI)
crates/client-win/src/wasapi/{mod.rs,capture.rs,render.rs,mmcss.rs}
crates/client-win/src/supervisor.rs
crates/client-win/src/pipeline.rs
crates/client-win/examples/loopback.rs
```

---

### Task 1: Workspace i protokół sterujący

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `crates/protocol/Cargo.toml`, `crates/protocol/src/{lib.rs,control.rs,framing.rs}`
- Test: `#[cfg(test)]` w `framing.rs`

**Interfaces:**
- Produces:
  - `pub const PROTOCOL_VERSION: u16 = 1;` `pub const ALPN: &[u8] = b"komunikator/1";` `pub const KEEP_ALIVE_MS: u64 = 5_000;` `pub const IDLE_TIMEOUT_MS: u64 = 15_000;` `pub const DEFAULT_PORT: u16 = 7700;`
  - `pub type MemberId = u16;`
  - `#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] pub struct MemberInfo { pub id: MemberId, pub name: String }`
  - `pub enum ClientMsg { Hello { version: u16, username: String, password: String } }`
  - `pub enum ServerMsg { Welcome { you: MemberId, members: Vec<MemberInfo> }, MemberJoined(MemberInfo), MemberLeft { id: MemberId }, Rejected { reason: RejectReason }, SessionReplaced }`
  - `pub enum RejectReason { BadCredentials, VersionMismatch { server: u16 }, RateLimited }`
  - `pub const MAX_FRAME: usize = 65_536;`
  - `pub fn encode_frame<T: Serialize>(msg: &T) -> Vec<u8>` — `u32` LE długość + postcard
  - `pub struct FrameDecoder` — `new()`, `push(&mut self, bytes: &[u8])`, `next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError>`
  - `pub enum FrameError { TooLarge(usize), Malformed }`

- [ ] **Step 1:** `git init`; workspace `Cargo.toml` (`members = ["crates/*"]`, `resolver = "2"`, `[workspace.dependencies]` z Tech Stack; `[profile.release]`: `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`); `.gitignore`: `/target`.
- [ ] **Step 2: Testy w `framing.rs`**

```rust
#[test] fn roundtrip_single() { /* encode_frame(&ClientMsg::Hello{..}); push; next() == Ok(Some(msg)); next() == Ok(None) */ }
#[test] fn split_across_pushes() { /* push bajt po bajcie → Some(msg) dopiero po ostatnim */ }
#[test] fn two_frames_in_one_push() { /* dwa Some, potem None */ }
#[test] fn rejects_oversized_length() { /* prefiks = 65_537 → Err(FrameError::TooLarge(65_537)) od razu po 4 bajtach */ }
#[test] fn rejects_garbage() { /* prefiks 3, dane [0xFF;3] → Err(FrameError::Malformed) */ }
```

- [ ] **Step 3:** `cargo test -p protocol` → FAIL (brak typów).
- [ ] **Step 4:** Implementacja; `FrameDecoder` trzyma `Vec<u8>` i usuwa zużyte bajty.
- [ ] **Step 5:** `cargo test -p protocol` → PASS.
- [ ] **Step 6:** Commit `feat(protocol): control messages and length-prefixed framing`.

---

### Task 2: Format datagramów audio

**Files:**
- Create: `crates/protocol/src/audio.rs`

**Interfaces:**
- Produces:
  - `pub const AUDIO_KIND: u8 = 1;` `pub const MAX_AUDIO_PAYLOAD: usize = 1000;` `pub const MAX_DATAGRAM: usize = 1200;`
  - Upstream (klient→serwer): `[kind u8][seq u32 LE][payload]`
  - Downstream (serwer→klient): `[kind u8][sender u16 LE][seq u32 LE][payload]` — **nadawcę wpisuje serwer** na podstawie połączenia.
  - `pub fn encode_upstream(seq: u32, payload: &[u8], out: &mut Vec<u8>)`
  - `pub fn parse_upstream(d: &[u8]) -> Result<(u32, &[u8]), DatagramError>`
  - `pub fn upstream_to_downstream(sender: MemberId, d: &[u8]) -> Result<Vec<u8>, DatagramError>`
  - `pub struct Downstream<'a> { pub sender: MemberId, pub seq: u32, pub payload: &'a [u8] }`
  - `pub fn parse_downstream(d: &[u8]) -> Result<Downstream<'_>, DatagramError>`
  - `pub enum DatagramError { TooShort, UnknownKind(u8), PayloadTooLarge(usize) }`

- [ ] **Step 1: Testy**

```rust
#[test] fn upstream_roundtrip() { /* encode_upstream(7, b"abc") → parse_upstream == Ok((7, &b"abc"[..])) */ }
#[test] fn relay_stamps_sender() { /* upstream_to_downstream(42, up) → parse_downstream: sender 42, seq 7, payload b"abc" */ }
#[test] fn rejects_short_and_unknown() { /* [] → TooShort; [9,0,0,0,0] → UnknownKind(9) */ }
#[test] fn rejects_large_payload() { /* payload 1001 B → PayloadTooLarge(1001) */ }
```

- [ ] **Step 2:** `cargo test -p protocol` → FAIL.
- [ ] **Step 3:** Implementacja.
- [ ] **Step 4:** `cargo test -p protocol` → PASS.
- [ ] **Step 5:** Commit `feat(protocol): audio datagram format`.

---

### Task 3: Serwer — konfiguracja i uwierzytelnianie

**Files:**
- Create: `crates/server/Cargo.toml` (lib + bin `komunikator-server`), `crates/server/src/{lib.rs,main.rs,config.rs,auth.rs}`, `server.example.toml`

**Interfaces:**
- Produces:
  - `pub struct ServerConfig { pub bind: SocketAddr /* domyślnie 0.0.0.0:7700 */, pub data_dir: PathBuf, pub users: Vec<UserEntry> }`, `pub struct UserEntry { pub name: String, pub password_hash: String }`
  - `ServerConfig::load(path: &Path) -> anyhow::Result<ServerConfig>`
  - `pub fn hash_password(pw: &str) -> String` (Argon2id, parametry domyślne crate'u `argon2`, format PHC)
  - `pub fn verify_password(hash: &str, pw: &str) -> bool` (niepoprawny hash → `false`)
  - `pub struct LoginLimiter` — `new(max_failures: u32, window: Duration)` (używane: 5, 60 s), `is_blocked(&mut self, ip: IpAddr, now: Instant) -> bool`, `record_failure(&mut self, ip: IpAddr, now: Instant)`
  - CLI: `komunikator-server hash-password` (hasło ze stdin → hash na stdout); `komunikator-server run --config <path>` (w tym tasku: tylko wczytuje konfigurację; pełne działanie w Task 6)

- [ ] **Step 1: Testy**

```rust
#[test] fn verify_correct_and_wrong() { let h = hash_password("tajne"); assert!(verify_password(&h, "tajne")); assert!(!verify_password(&h, "inne")); }
#[test] fn verify_garbage_hash_is_false() { assert!(!verify_password("nie-hash", "x")); }
#[test] fn limiter_blocks_after_five_and_expires() { /* 5× record_failure w t0 → is_blocked(t0+1s); !is_blocked(t0+61s) */ }
#[test] fn limiter_is_per_ip() { /* blokada 1.2.3.4 nie blokuje 5.6.7.8 */ }
#[test] fn config_parses_example() { /* load(server.example.toml) → bind 0.0.0.0:7700, 1 użytkownik */ }
```

- [ ] **Step 2:** `cargo test -p server` → FAIL.
- [ ] **Step 3:** Implementacja.
- [ ] **Step 4:** `cargo test -p server` → PASS.
- [ ] **Step 5:** Commit `feat(server): config, argon2 auth, login limiter`.

---

### Task 4: Serwer — certyfikat TLS i stan pokoju

**Files:**
- Create: `crates/server/src/{tls.rs,room.rs}`

**Interfaces:**
- Produces:
  - `pub struct ServerIdentity { pub cert: CertificateDer<'static>, pub key: PrivatePkcs8KeyDer<'static>, pub fingerprint: [u8; 32] }`
  - `pub fn load_or_create_identity(dir: &Path) -> anyhow::Result<ServerIdentity>` — pliki `cert.der`, `key.der` w `dir`; generacja `rcgen` z SAN `komunikator`; `fingerprint` = SHA-256 z DER certyfikatu.
  - `pub fn fingerprint_hex(fp: &[u8; 32]) -> String` (64 znaki, małe litery) i `pub fn parse_fingerprint(s: &str) -> Option<[u8; 32]>` — obie w `protocol/src/lib.rs` (używa ich też klient).
  - `pub struct Room<C: Clone>`:
    - `join(&mut self, name: &str, conn: C) -> JoinOutcome<C>`; `pub struct JoinOutcome<C> { pub id: MemberId, pub members: Vec<MemberInfo> /* pozostali */, pub replaced: Option<(MemberId, C)> }`
    - `leave(&mut self, id: MemberId) -> Option<MemberInfo>` (nieistniejące `id` → `None`)
    - `peers(&self, id: MemberId) -> Vec<C>` (wszyscy poza `id`)
    - `contains(&self, id: MemberId) -> bool`
  - Id członków: rosnący `u16` z zawijaniem, z pominięciem `0` i zajętych.

- [ ] **Step 1: Testy**

```rust
#[test] fn identity_persists() { /* dwa wywołania na tym samym tempdir → ten sam fingerprint */ }
#[test] fn fingerprint_hex_roundtrip() { /* parse_fingerprint(fingerprint_hex(x)) == Some(x); "zz" → None */ }
#[test] fn join_lists_others() { /* a, potem b → outcome(b).members == [a] */ }
#[test] fn same_name_replaces_previous() { /* "ala" ×2 → replaced == Some((id1, conn1)); !contains(id1) */ }
#[test] fn leave_after_replace_is_noop() { /* leave(id1) → None; id2 nadal jest */ }
#[test] fn peers_excludes_self() { /* 3 członków → peers(a).len() == 2 */ }
#[test] fn ids_skip_zero_and_in_use() { /* licznik na u16::MAX → kolejne id != 0 i nie koliduje */ }
```

- [ ] **Step 2:** `cargo test -p server` → FAIL.
- [ ] **Step 3:** Implementacja.
- [ ] **Step 4:** `cargo test --workspace` → PASS.
- [ ] **Step 5:** Commit `feat(server): persistent self-signed identity and room state`.

---

### Task 5: Klient — przypięty certyfikat i backoff

**Files:**
- Create: `crates/client-core/Cargo.toml`, `crates/client-core/src/{lib.rs,net/mod.rs,net/tls.rs,net/backoff.rs}`

**Interfaces:**
- Consumes: stałe z `protocol`.
- Produces:
  - `pub const SERVER_NAME: &str = "komunikator";`
  - `pub fn client_config(fingerprint: [u8; 32]) -> anyhow::Result<quinn::ClientConfig>` — własny `rustls::client::danger::ServerCertVerifier`: akceptuje wyłącznie certyfikat, którego SHA-256 == `fingerprint`; podpisy handshake przez `rustls::crypto::verify_tls13_signature` providera `ring`; ALPN `protocol::ALPN`; transport: keep-alive `KEEP_ALIVE_MS`, `max_idle_timeout` `IDLE_TIMEOUT_MS`, `datagram_receive_buffer_size` 64 KiB.
  - `pub struct Backoff` — `new()`, `next_delay(&mut self) -> Duration`, `reset(&mut self)`.
  - (Serwer w Task 6 buduje swój `TransportConfig` z tych samych stałych `protocol`.)

- [ ] **Step 1: Testy**

```rust
#[test] fn backoff_sequence() { /* 500ms, 1s, 2s, 4s, 5s, 5s */ }
#[test] fn backoff_reset() { /* po reset() → 500ms */ }
#[test] fn verifier_accepts_matching_and_rejects_other() { /* dwa certyfikaty rcgen; verify_server_cert: pasujący → Ok, inny → Err */ }
```

- [ ] **Step 2:** `cargo test -p client-core` → FAIL.
- [ ] **Step 3:** Implementacja.
- [ ] **Step 4:** `cargo test -p client-core` → PASS.
- [ ] **Step 5:** Commit `feat(client-core): pinned-cert QUIC config and reconnect backoff`.

---

### Task 6: Serwer — połączenia i przekazywanie audio

**Files:**
- Create: `crates/server/src/conn.rs`; Modify: `crates/server/src/{lib.rs,main.rs}`
- Test: `crates/server/tests/relay.rs` (dev-dependency `client-core` — tylko dla `client_config`)

**Interfaces:**
- Consumes: Task 1–5.
- Produces:
  - `pub async fn start(config: ServerConfig) -> anyhow::Result<RunningServer>`
  - `RunningServer::local_addr(&self) -> SocketAddr`, `fingerprint(&self) -> [u8; 32]`, `async fn shutdown(self)`
  - `komunikator-server run --config <path>` wypisuje `fingerprint: <hex>` i działa do Ctrl+C.
  - `pub struct TokenBucket` — `new(rate_per_s: u32, burst: u32)` (używane: 100, 20), `try_take(&mut self, now: Instant) -> bool`.

Przebieg: klient otwiera strumień bi → `Hello` → limiter IP zablokowany ⇒ `Rejected{RateLimited}` → `version != PROTOCOL_VERSION` ⇒ `Rejected{VersionMismatch{server: 1}}` → złe hasło ⇒ `Rejected{BadCredentials}` + `record_failure` → `Room::join`. Zastąpiona sesja: `SessionReplaced`, potem zamknięcie (kod aplikacji `1`). Nowy: `Welcome`; pozostali: `MemberJoined`. Datagramy: `parse_upstream` → `TokenBucket` → `upstream_to_downstream(id, ..)` → `send_datagram` do `peers(id)` (lista klonowana pod blokadą, wysyłka poza nią). Błąd framingu, niepoprawny `ClientMsg`, datagram > `MAX_DATAGRAM` → zamknięcie **tylko** tego połączenia (kod `2`). Zamknięcie → `leave` + `MemberLeft` do pozostałych. Stan: `Arc<std::sync::Mutex<Room<quinn::Connection>>>`.

- [ ] **Step 1: Testy w `tests/relay.rs`** (pomocnik `connect(addr, fp, name, pw)`; użytkownicy w configu: `ala`, `ola`, `ewa`, hasło `x`; `data_dir` = tempdir)

```rust
#[tokio::test] async fn welcome_and_member_events() { /* ala → Welcome{members: []}; ola → Welcome{members:[ala]}; ala dostaje MemberJoined(ola); ola zamyka → ala dostaje MemberLeft{id: ola} */ }
#[tokio::test] async fn relays_audio_to_others_only() { /* 3 klientów; ala wysyła upstream seq 5 → ola i ewa: sender=id(ala), seq=5, ten sam payload; ala nic nie dostaje w 300 ms */ }
#[tokio::test] async fn bad_password_rejected() { /* Rejected{BadCredentials} */ }
#[tokio::test] async fn version_mismatch_rejected() { /* Hello{version: 99} → Rejected{VersionMismatch{server: 1}} */ }
#[tokio::test] async fn sixth_attempt_is_rate_limited() { /* 5× złe hasło, potem dobre → Rejected{RateLimited} */ }
#[tokio::test] async fn second_login_replaces_first() { /* ala ×2 → pierwsza: SessionReplaced i zamknięcie z kodem 1; druga: Welcome */ }
#[tokio::test] async fn oversized_frame_disconnects_only_offender() { /* ewa wysyła prefiks 70_000 → połączenie ewy zamknięte (kod 2); audio ala→ola nadal dochodzi */ }
#[tokio::test] async fn datagram_flood_is_rate_limited() { /* ala wysyła 500 datagramów naraz → ola dostaje ≤ 120; połączenie ali otwarte */ }
#[test] fn token_bucket_refills() { /* burst 20 → 20× true, 21. false; po 100 ms +10 */ }
```

- [ ] **Step 2:** `cargo test -p server` → FAIL.
- [ ] **Step 3:** Implementacja `conn.rs`, `start`, `run`.
- [ ] **Step 4:** `cargo test --workspace` → PASS.
- [ ] **Step 5:** Commit `feat(server): QUIC accept loop, login, audio relay with rate limits`.

---

### Task 7: Klient — sesja z automatycznym ponownym łączeniem

**Files:**
- Create: `crates/client-core/src/net/session.rs`, `crates/client-core/src/audio/{mod.rs,frame.rs}` (na razie tylko typy ramek)
- Test: `crates/client-core/tests/session.rs` (dev-dependency `server`)

**Interfaces:**
- Consumes: `client_config`, `Backoff`, `SERVER_NAME`, `protocol`.
- Produces:
  - `#[derive(Clone, Copy)] pub struct EncodedFrame { pub seq: u32, pub len: u16, pub data: [u8; 256] }`
  - `#[derive(Clone, Copy)] pub struct IncomingFrame { pub sender: MemberId, pub seq: u32, pub len: u16, pub data: [u8; 256] }`
  - `pub struct SessionParams { pub server: String /* "host:port" */, pub fingerprint: [u8; 32], pub username: String, pub password: String }`
  - `pub enum SessionEvent { Connecting, Connected { you: MemberId, members: Vec<MemberInfo> }, MemberJoined(MemberInfo), MemberLeft(MemberId), Disconnected { retry_in: Duration }, Fatal(FatalReason) }`
  - `pub enum FatalReason { BadCredentials, VersionMismatch { server: u16 }, SessionReplaced }` — po `Fatal` brak ponowień. `RateLimited` traktowany jak zwykłe rozłączenie z backoffem.
  - `pub fn spawn_session(rt: &tokio::runtime::Handle, params: SessionParams) -> SessionHandle`
  - `pub struct SessionHandle { pub events: tokio::sync::mpsc::Receiver<SessionEvent>, pub audio_out: tokio::sync::mpsc::Sender<EncodedFrame> /* cap 64; try_send z wątku audio */, pub audio_in: tokio::sync::mpsc::Receiver<IncomingFrame> /* cap 256; try_recv z wątku audio */ }` + `pub fn shutdown(&self)`.
  - Po każdym `Welcome` → `Backoff::reset()`. Ramki do wysłania w stanie rozłączonym są porzucane.

- [ ] **Step 1: Testy**

```rust
#[tokio::test] async fn two_clients_exchange_audio() { /* serwer; 2 sesje → Connected; a.audio_out.try_send(seq 1, "hej") → b.audio_in: sender=id(a), seq 1, "hej" */ }
#[tokio::test] async fn reconnects_after_server_restart() { /* serwer na stałym porcie i data_dir; shutdown → Disconnected{retry_in: 500ms}; start ponownie (ten sam fingerprint) → Connected w < 5 s; audio znów dochodzi */ }
#[tokio::test] async fn replaced_session_does_not_reconnect() { /* ala#1, potem ala#2 → #1: Fatal(SessionReplaced), przez 3 s brak Connecting; #2 pozostaje Connected */ }
#[tokio::test] async fn bad_credentials_is_fatal() { /* Fatal(BadCredentials), brak dalszych prób w 2 s */ }
#[tokio::test] async fn wrong_fingerprint_never_connects() { /* fingerprint [0;32] → w 2 s tylko Connecting/Disconnected */ }
```

- [ ] **Step 2:** `cargo test -p client-core --test session` → FAIL.
- [ ] **Step 3:** Implementacja: jedno zadanie tokio na sesję; `select!` po strumieniu sterującym, `read_datagram`, `audio_out.recv()`, sygnale shutdown.
- [ ] **Step 4:** `cargo test --workspace` → PASS.
- [ ] **Step 5:** Commit `feat(client-core): session with auto-reconnect and fatal handling`.

---

### Task 8: Kodek Opus i mikser

**Files:**
- Create: `crates/client-core/src/audio/{codec.rs,mixer.rs}`; Modify: `audio/frame.rs`, `audio/mod.rs`

**Interfaces:**
- Produces:
  - w `frame.rs`: `pub const SAMPLE_RATE: u32 = 48_000; pub const FRAME_SAMPLES: usize = 960; pub type Pcm = [f32; FRAME_SAMPLES];`
  - `pub struct Encoder` — `new() -> anyhow::Result<Self>` (Application::Voip, bitrate 32 000, VBR wyłączony, inband FEC, packet loss 10%); `encode(&mut self, pcm: &Pcm, out: &mut [u8; 256]) -> anyhow::Result<usize>`
  - `pub struct Decoder` — `new() -> anyhow::Result<Self>`, `decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut Pcm) -> anyhow::Result<()>` (`None` = PLC)
  - `pub fn mix(sources: &[(&Pcm, f32)], out: &mut Pcm)` — suma × wzmocnienie, obcięcie do [-1, 1]; pusta lista → cisza.

- [ ] **Step 1: Testy**

```rust
#[test] fn encode_is_cbr_size() { /* sinus 440 Hz −20 dBFS, 50 ramek → każda 78..=82 B */ }
#[test] fn roundtrip_preserves_tone() { /* po 10 ramkach rozgrzewki RMS zdekodowanego w ±3 dB od wejścia */ }
#[test] fn plc_produces_frame() { /* decode(None, false) → Ok */ }
#[test] fn mix_sums_and_clips() { /* 0.6+0.6 → 1.0; gain 0.5 → połowa; pusta lista → zera */ }
```

- [ ] **Step 2:** `cargo test -p client-core` → FAIL.
- [ ] **Step 3:** Implementacja (`opus 0.4`).
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(client-core): opus codec wrappers and mixer`.

---

### Task 9: Wykrywanie mowy (VAD)

**Files:**
- Create: `crates/client-core/src/audio/vad.rs`

**Interfaces:**
- Produces: `pub struct Vad` — `new(sensitivity: f32 /* 0.0–1.0, domyślnie 0.5 */)`, `process(&mut self, pcm: &Pcm) -> bool` (true = wysyłaj), `set_sensitivity(&mut self, s: f32)`, `level_dbfs(&self) -> f32` (podgląd w GUI w planie 3).

Algorytm: RMS ramki w dBFS; poziom szumu adaptacyjny — natychmiast w dół do poziomu cichszej ramki, w górę +0,5 dB/s tylko na ramkach, które nie otwierają bramki; próg = `max(noise_floor + (18 − 12·sensitivity) dB, −50 dBFS)`; po ostatniej ramce powyżej progu bramka otwarta jeszcze 15 ramek.

- [ ] **Step 1: Testy**

```rust
#[test] fn silence_is_closed() { /* 100 ramek zer → wszystkie false */ }
#[test] fn speech_opens_immediately() { /* 50 ramek szumu −60 dBFS, potem sinus −20 dBFS → true */ }
#[test] fn hangover_is_15_frames() { /* po sinusie: 15 ramek ciszy → true, 16. → false */ }
#[test] fn steady_fan_noise_closes() { /* 500 ramek szumu −40 dBFS → ostatnie 100 false */ }
```

- [ ] **Step 2:** FAIL → **Step 3:** implementacja → **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(client-core): energy VAD with adaptive floor and hangover`.

---

### Task 10: Bufor jittera

**Files:**
- Create: `crates/client-core/src/audio/jitter.rs`

**Interfaces:**
- Consumes: `Decoder`, `Pcm`.
- Produces:
  - `pub struct JitterBuffer` — `new()`, `push(&mut self, seq: u32, payload: &[u8])`, `pop(&mut self, dec: &mut Decoder, out: &mut Pcm) -> Pop`, `target_depth(&self) -> u8`
  - `pub enum Pop { Audio, Silence }` (`Silence` = `out` wyzerowane)
  - Bez alokacji po `new()`: 16 slotów `[u8; 256]` indeksowanych `seq % 16`.

Algorytm:
- Porównania sekwencji: `seq.wrapping_sub(next) as i32`.
- `push`: odrzuć duplikat i spóźniony (`< next_seq`); `seq ≥ next_seq + 16` → reset (nowa wypowiedź).
- Start odtwarzania, gdy zbuforowano `target_depth` ramek (start 2 = 40 ms; min 2, maks 10); `next_seq` = najniższy zbuforowany.
- `pop` w trakcie odtwarzania: jest `next_seq` → dekoduj; brak, ale jest `next_seq+1` → `decode(Some(next+1), fec=true)`; brak obu → PLC i `target_depth += 1` (maks 10). 5 brakujących z rzędu → koniec wypowiedzi: `Silence`, czekanie na nowy start.
- 500 ramek (10 s) bez braków i bufor > `target_depth` → `target_depth -= 1` (min 2).

- [ ] **Step 1: Testy** (pakiety z prawdziwego `Encoder`)

```rust
#[test] fn waits_for_target_depth() { /* push 1 → pop == Silence; push 2. → pop == Audio */ }
#[test] fn reorders() { /* push 11, 10, 12 → 3× Audio; kolejność 10,11,12 (licznik dekodów w Decoder-mocku lub porównanie z referencyjnym dekodowaniem) */ }
#[test] fn drops_duplicates_and_late() { /* duplikat i seq < next nie zmieniają liczby zbuforowanych */ }
#[test] fn loss_uses_fec_then_plc_and_grows_target() { /* luka 1 → Audio (FEC); luka 2 → Audio (PLC), target_depth == 3 */ }
#[test] fn ends_spurt_after_5_missing() { /* po ostatnim pakiecie: 5× Audio (PLC), potem Silence */ }
#[test] fn handles_seq_wraparound() { /* seq u32::MAX-1, MAX, 0, 1 → 4× Audio, bez resetu */ }
#[test] fn new_spurt_after_gap_resets() { /* 10..12, potem 500..502 → odtwarza od 500 po osiągnięciu target_depth */ }
```

- [ ] **Step 2:** FAIL → **Step 3:** implementacja → **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(client-core): adaptive jitter buffer with FEC/PLC`.

---

### Task 11: WASAPI — przechwytywanie, odtwarzanie, nadzór urządzeń

**Files:**
- Create: `crates/client-win/Cargo.toml` (bin `komunikator`; `windows` z feature'ami `Win32_Media_Audio`, `Win32_System_Com`, `Win32_Media_KernelStreaming`, `Win32_System_Threading`, `Win32_Foundation`, `Win32_Devices_FunctionDiscovery`), `src/wasapi/{mod.rs,capture.rs,render.rs,mmcss.rs}`, `src/supervisor.rs`, `examples/loopback.rs`

**Interfaces:**
- Produces:
  - `pub trait FrameSource { fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError>; }`, `pub trait FrameSink { fn write_frame(&mut self, pcm: &Pcm) -> Result<(), AudioError>; }`
  - `pub enum AudioError { DeviceInvalidated, Other(String) }` — `AUDCLNT_E_DEVICE_INVALIDATED` → `DeviceInvalidated`
  - `pub struct WasapiCapture` (impl `FrameSource`) — `open(device_id: Option<&str>) -> Result<Self, AudioError>` (`None` = domyślne urządzenie komunikacyjne, rola `eCommunications`): tryb współdzielony, zdarzeniowy, `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` → 48 kHz mono f32; `IAudioClient2::SetClientProperties` z `AudioCategory_Communications`; `IAudioClientDuckingControl::SetDuckingOptionsForCurrentStream(AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS)` — brak interfejsu ⇒ `tracing::warn!` i dalej; akumulacja pakietów do pełnej ramki 960.
  - `pub struct WasapiRender` (impl `FrameSink`) — analogicznie; bufor 40 ms.
  - `pub fn list_devices() -> Vec<(String /*id*/, String /*nazwa*/)>` dla wejść i wyjść (`list_capture_devices`, `list_render_devices`)
  - `pub fn enter_pro_audio() -> MmcssGuard` (`AvSetMmThreadCharacteristicsW("Pro Audio")`, revert w `Drop`)
  - `pub struct Supervised<T, F, Clock>` — `new(opener: F, clock: Clock)` gdzie `F: FnMut() -> Result<T, AudioError>`, `Clock: Fn() -> Instant`; przy `DeviceInvalidated` lub nieudanym otwarciu ponawia `opener` co 1 s; w międzyczasie `read_frame` zwraca ciszę, `write_frame` porzuca ramkę. Implementuje `FrameSource`/`FrameSink`, gdy `T` je implementuje.

- [ ] **Step 1: Testy `supervisor.rs`**

```rust
#[test] fn supervisor_reopens_after_invalidation() { /* fake: 3 ramki OK, potem DeviceInvalidated; read_frame zwraca zera; zegar +1 s → opener wywołany 2. raz, dane znów płyną */ }
#[test] fn supervisor_retries_until_open_succeeds() { /* opener: Err, Err, Ok (co 1 s zegara) → 3 wywołania, bez paniki, potem dane */ }
```

- [ ] **Step 2:** `cargo test -p client-win` → FAIL.
- [ ] **Step 3:** Implementacja `supervisor.rs`, potem `wasapi/*` i `examples/loopback.rs` (mikrofon → głośniki przez 10 s).
- [ ] **Step 4:** `cargo test -p client-win` → PASS.
- [ ] **Step 5: Weryfikacja ręczna:** `cargo run -p client-win --example loopback` → słychać własny głos; odtwarzana równolegle muzyka/gra **nie jest ściszana**. Wynik (i wersja Windows) w opisie commitu.
- [ ] **Step 6:** Commit `feat(client-win): WASAPI capture/render with communications category and device supervision`.

---

### Task 12: Potok audio, CLI i akceptacja

**Files:**
- Create: `crates/client-win/src/{pipeline.rs,main.rs}`, `README.md`, `docs/acceptance/plan-1.md`

**Interfaces:**
- Consumes: wszystko powyżej.
- Produces:
  - `pub struct Controls { pub muted: AtomicBool, pub deafened: AtomicBool }` (w `Arc`; plan 3 podepnie GUI i skróty)
  - `pub fn capture_loop(src: impl FrameSource, vad: Vad, enc: Encoder, out: mpsc::Sender<EncodedFrame>, controls: Arc<Controls>, stop: Arc<AtomicBool>)` — `muted` ⇒ nic nie wysyła; `seq` rośnie tylko dla wysłanych ramek; `try_send` (pełna kolejka ⇒ porzuć).
  - `pub fn render_loop(sink: impl FrameSink, inp: mpsc::Receiver<IncomingFrame>, controls: Arc<Controls>, stop: Arc<AtomicBool>)` — co ramkę: opróżnij `inp` do `JitterBuffer` danego `sender` (do 16 nadawców, prealokowane wraz z `Decoder`; nadawca nieaktywny 60 s zwalnia slot), `pop` każdego, `mix` z wzmocnieniem 1.0, `deafened` ⇒ cisza; zapis.
  - CLI: `komunikator --server <host:port> --fingerprint <hex64> --user <name> [--capture-device <id>] [--render-device <id>] [--list-devices]`; hasło ze zmiennej `KOMUNIKATOR_PASSWORD`; wypisuje `SessionEvent` czytelnie; stdin: `m`+Enter = mute, `d`+Enter = deafen, `q`+Enter = wyjście. `Fatal(...)` ⇒ komunikat i kod wyjścia 1.

- [ ] **Step 1: Testy**

```rust
#[test] fn muted_sends_nothing() { /* fake źródło z sinusem, muted=true → 0 ramek */ }
#[test] fn silence_sends_nothing_speech_sends() { /* zera → 0 ramek; sinus −20 dBFS → ramki z seq 0,1,2… */ }
#[test] fn render_mixes_two_senders() { /* zakodowany sinus od sender 1 i 2 → po target_depth fake sink dostaje niezerowy sygnał */ }
#[test] fn deafened_outputs_silence() { /* jw., deafened=true → same zera */ }
```

- [ ] **Step 2:** FAIL → **Step 3:** implementacja + `main.rs` (wątki capture/render z `enter_pro_audio()` i `Supervised<WasapiCapture/Render>`; runtime tokio `multi_thread` z 1 workerem) → **Step 4:** `cargo test --workspace` → PASS.
- [ ] **Step 5: Akceptacja ręczna** (wyniki w `docs/acceptance/plan-1.md`):
  1. Serwer (`komunikator-server run --config server.toml`) lokalnie lub na VPS; dwa komputery z klientem → słychać się nawzajem.
  2. Po 30 min rozmowy: `Get-Process komunikator | Select WorkingSet64, CPU` → working set < 30 MB; średnie CPU < 1%.
  3. Restart serwera w trakcie → dźwięk wraca sam w ≤ 5 s.
  4. Wyjęcie słuchawek USB w trakcie → dźwięk wraca na urządzeniu domyślnym bez restartu klienta.
  5. Gra/muzyka nie jest ściszana przez Windows.
  6. Gracz na głośnikach: czy druga strona słyszy echo? Zapisz wynik — decyduje o dołożeniu WebRTC AEC (spec 6.2, opcja B).
- [ ] **Step 6:** `cargo build --release --workspace` na Windows oraz `cargo build --release -p server` na Debianie → oba binaria powstają; `README.md` opisuje: `hash-password`, konfigurację, uruchomienie serwera (port UDP 7700 otwarty w firewallu) i klienta.
- [ ] **Step 7:** Commit `feat(client-win): audio pipeline and headless CLI client`.
