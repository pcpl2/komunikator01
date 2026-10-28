# Plan 3: pokoje, czat, głośność per osoba — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Several persistent rooms with free switching, an end-to-end encrypted per-room text chat, per-person volume and local mute, deafen that also silences the mic — all usable from the headless CLI client.

**Architecture:** Member ids become server-wide; every member is in one room or none. The server keeps rooms in SQLite and confines announces, relays, chat and audio to the sender's room. The client tracks rooms in a sans-IO `Lobby` and creates a fresh `E2e` (bound to the room id) whenever it enters a room. Chat is sealed with a key derived from the same sender key as audio. Volumes are per username in a `VolumeBook`, applied by member id through a lock-free `GainTable` read by the render thread.

**Tech Stack:** Rust 2021 workspace (protocol, server, client-core, client-win), quinn 0.11, postcard, chacha20poly1305 0.10, hkdf 0.12, rusqlite (new, feature `bundled`).

**Spec:** `docs/superpowers/specs/2026-10-28-komunikator-glosowy-design.md` — sections 2 (goals 5, 6, 10), 4 (anti-goals), 6.3 (E2E), 6.4 (chat), 6.6 (rooms), 7 (tests). E2E background: `docs/e2e.md`.

**Out of scope (plan 4):** Win32 window, tray, global hotkeys, sounds, settings file, speaking indicator, clipboard. Accounts/invites move to plan 5, auto-update and deployment to plan 6.

## Global Constraints

- No WebView, no hooks, no injection; client stays a native Windows exe (spec 1, 6.7).
- Server never stores or logs chat content; it only forwards sealed blobs (spec 6.4, 6.8).
- Chat: UTF-8 text only, limit 2000 characters, no history on the server, client keeps it in memory (spec 6.4).
- The server is hostile (spec 6.3): every cryptographic decision must hold if the server lies about rooms or membership.
- Only audited primitives (RustCrypto, `snow`); no hand-rolled crypto (spec 6.3).
- Render and capture loops must not allocate or block in the hot path (spec 6.2).
- Per-person volume range 0–200 % (spec 6.2); volumes remembered by username (spec 6.7).
- All user-facing text in Polish with correct diacritics.
- `PROTOCOL_VERSION` becomes 3; ALPN stays `komunikator/1`.
- New dependency allowed: `rusqlite` with feature `bundled` (latest release; add with `cargo add -p server rusqlite --features bundled`). No other new crates.

## Review Focus

1. A hostile peer sends chat text with terminal escape sequences or bidi overrides → the CLI prints it without control characters (Task 3, `sanitize_strips_controls_and_bidi`).
2. A member moves rooms and the hostile server keeps feeding it the old room's chat → it cannot be opened, because the room id is in the AAD (Task 4, `chat_from_other_room_does_not_open`).
3. Reconnect after the last room was deleted → the client lands in the default room, not nowhere and not in an error loop (Task 2, `missing_room_choice_falls_back_to_default`; Task 6, `deleted_room_falls_back_to_default_on_reconnect`).
4. Room names differing only by Unicode case ("Ogólny" / "OGÓLNY") → `NameTaken` (Task 2, `room_names_are_unique_ignoring_case`).
5. A 2000-character message made of 4-byte characters → the sealed blob still fits `MAX_CHAT_BLOB` (Task 4, `longest_chat_fits_blob_limit`).

---

### Task 1: Room types and the SQLite room store

**Files:**
- Modify: `crates/protocol/src/control.rs`, `crates/protocol/src/lib.rs` (additive only)
- Create: `crates/server/src/store.rs`; Modify: `crates/server/src/lib.rs` (`pub mod store;`), `crates/server/Cargo.toml`

**Interfaces:**
- Produces (protocol, re-exported from `lib.rs`):
  - `pub type RoomId = u32;`
  - `#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)] pub struct RoomInfo { pub id: RoomId, pub name: String, pub creator: String }` (`creator` empty for the built-in room)
  - `pub enum RoomErrorReason { InvalidName, NameTaken, TooManyRooms, NotFound, NotAllowed, NotEmpty, Full }` (same derives)
  - `pub enum RoomChoice { Default, Room(RoomId), Nowhere }` (same derives, plus `Copy`)
  - consts `MAX_ROOM_NAME_CHARS: usize = 32`, `MAX_ROOMS: usize = 50`, `MAX_ROOM_MEMBERS: usize = 20`, `MAX_CHAT_BLOB: usize = 8192`, `DEFAULT_ROOM_NAME: &str = "Ogólny"`
  - `pub fn normalize_room_name(name: &str) -> Option<String>` — trims; `None` if empty, longer than 32 `char`s, or containing any `char::is_control`.
- Produces (server): `pub struct Store`, `Store::open(path: &Path) -> anyhow::Result<Store>`, `fn rooms(&self) -> anyhow::Result<Vec<RoomInfo>>` (ascending id), `fn create_room(&self, name: &str, creator: &str) -> Result<RoomInfo, StoreError>`, `fn delete_room(&self, id: RoomId) -> anyhow::Result<bool>`; `pub enum StoreError { NameTaken, Db(rusqlite::Error) }` (thiserror). The database file is `data_dir/komunikator.db` (opened by Task 2).

- [ ] **Step 1: Write failing tests** — protocol: `normalize_room_name_rules` (`"  Gra  "` → `"Gra"`, `""`/`"   "` → None, 32 × `'ż'` ok, 33 × `'ż'` None, `"a\nb"` None). Server `store.rs` unit tests with `tempfile`:
  - `fresh_store_has_default_room`: `rooms()` == one room named `"Ogólny"`, creator `""`.
  - `rooms_survive_reopen`: create `"Gra"` by `"ala"`, drop, reopen → both rooms, `"Gra"` has creator `"ala"`.
  - `duplicate_name_is_taken`: `create_room("gra", ..)` after `"Gra"` → `Err(StoreError::NameTaken)` (SQLite `COLLATE NOCASE`; ASCII folding is enough here, Task 2 handles Unicode case).
  - `deleted_ids_are_not_reused`: create A, delete A (true), create B → `B.id > A.id`; deleting A again → false.
- [ ] **Step 2: Run** `cargo test -p protocol -p server` — Expected: FAIL (items missing).
- [ ] **Step 3: Implement.** Table `rooms(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE COLLATE NOCASE, creator TEXT NOT NULL)`. `open` creates the table and inserts the default room when the table is empty. `Store` wraps a `rusqlite::Connection`; callers serialize access (Task 2).
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass.
- [ ] **Step 5: Commit** `feat(server): room types and SQLite room store`.

### Task 2: Protocol v3 and server rooms

**Files:**
- Modify: `crates/protocol/src/control.rs`, `crates/protocol/src/lib.rs`
- Modify: `crates/server/src/room.rs` (rewrite as `Registry`), `crates/server/src/conn.rs`, `crates/server/src/lib.rs`, `crates/server/src/config.rs`, `server.example.toml`
- Modify: `crates/server/tests/relay.rs`; Create: `crates/server/tests/rooms.rs`
- Modify (compile adaptation only): `crates/client-core/src/net/session.rs`, `crates/client-core/tests/`, `crates/client-win/src/main.rs`

**Interfaces:**
- Consumes: Task 1 types and `Store`.
- Produces (wire, `PROTOCOL_VERSION = 3`):
  - `MemberInfo { id, name, room: Option<RoomId> }`
  - `ClientMsg::Hello { version, username, password, session, room: RoomChoice }`, plus `JoinRoom { room: RoomId }`, `LeaveRoom`, `CreateRoom { name: String }`, `DeleteRoom { room: RoomId }`, `Chat { blob: Vec<u8> }`; `Announce`, `Relay` unchanged.
  - `ServerMsg::Welcome { you, room: Option<RoomId>, rooms: Vec<RoomInfo>, members: Vec<MemberInfo> }` (members = everyone online, me included), `MemberJoined(MemberInfo)` (came online), `MemberLeft { id }` (went offline), `MemberMoved { id, room: Option<RoomId> }`, `RoomCreated(RoomInfo)`, `RoomDeleted { room }`, `RoomError { reason: RoomErrorReason }`, `Chat { from, blob }`; `Rejected`, `SessionReplaced`, `Announce`, `Relay` unchanged.
- Produces (server): `UserEntry { name, password_hash, #[serde(default)] admin: bool }`; `Registry<C>` in `room.rs`: `new(rooms: Vec<RoomInfo>)`, `login(&mut self, name, conn, choice: RoomChoice) -> LoginOutcome<C> { id, room, replaced }`, `snapshot(&self) -> (Vec<RoomInfo>, Vec<MemberInfo>)`, `leave(id) -> Option<MemberInfo>`, `move_to(id, room: Option<RoomId>) -> Result<bool /*changed*/, RoomErrorReason>`, `room_peers(id) -> Vec<(MemberId, C)>` (same room, not me; empty when I am in none), `others(id) -> Vec<C>` (everyone online but me), `check_new_room(&self, name) -> Result<String, RoomErrorReason>`, `add_room(RoomInfo)`, `check_delete(&self, room, by: &str, admin: bool) -> Result<(), RoomErrorReason>`, `remove_room(room)`, plus the existing `get`, `get_mut`, `contains`.

Server semantics:
- Default room = lowest room id. `RoomChoice::Room(r)` → `r` if it exists and is not full, else the default room; `Default` → default room; `Nowhere` → no room. A full default room means no room.
- Login: `Welcome` to me; `MemberJoined` to everyone else online; then, under the same lock, the stored announces of my room peers (as today after `Welcome`).
- `JoinRoom`/`LeaveRoom`: an error goes back as `RoomError` to the sender only. On change: clear the mover's stored announce, send `MemberMoved` to everyone online (mover included), then replay the stored announces of the new room's members to the mover, all under the registry lock. Joining the room you are in sends nothing.
- `CreateRoom`: `check_new_room` (normalized name; `InvalidName`; `NameTaken` comparing `to_lowercase()`; `TooManyRooms` at `MAX_ROOMS`), then `Store::create_room`, `add_room`, `RoomCreated` to everyone online. The creator does not move.
- `DeleteRoom`: `NotFound`; `NotAllowed` unless creator == username or the user is admin; `NotEmpty` while anyone is inside; then `Store::delete_room`, `remove_room`, `RoomDeleted` to everyone online.
- Store calls run while holding the registry lock (rare, sub-millisecond; no create/delete races). Store errors other than `NameTaken` are logged without content and answered with `RoomError { NotFound }`.
- `Announce`, `Relay`, `Chat` and audio datagrams act only within the sender's room: `Relay` only to a member of the same room; a member in no room is never heard. A `Chat` blob over `MAX_CHAT_BLOB` → close with `CLOSE_PROTOCOL`; chats also pass a separate bucket `CHAT_RATE_PER_S = 5`, `CHAT_BURST = 20` (excess dropped). `ServerMsg::Chat { from, blob }` goes to room peers only and is never logged.
- `MAX_ROOM_MEMBERS` reached → `RoomError { Full }`.

Client compile adaptation (behaviour preserved; real rooms come in Task 6): `Hello` sends `room: RoomChoice::Default`; on `Welcome` the session builds `E2e` from the members whose `room` equals mine and emits `Connected { you, members }` with those; `MemberJoined` reaches the E2e only when its room equals mine; other new messages are ignored.

- [ ] **Step 1: Write failing tests.** `room.rs` unit tests: `login_default_goes_to_lowest_room`, `missing_room_choice_falls_back_to_default`, `nowhere_has_no_room_peers`, `room_peers_are_same_room_only`, `move_to_unknown_room_is_not_found`, `full_room_refuses` (fill to 20 with `move_to`), `room_names_are_unique_ignoring_case` (`"Ogólny"` vs `"OGÓLNY"` → `NameTaken`), `too_many_rooms` (50), `delete_rules` (`NotAllowed` for a stranger, ok for creator and admin, `NotEmpty` when occupied), `same_name_login_replaces_previous` (kept). Integration `tests/rooms.rs` (helpers copied from `relay.rs`, Hello with a `RoomChoice`; `ewa` is admin in its config):
  - `welcome_lists_rooms_and_places_me_in_default`
  - `audio_and_chat_stay_in_room`: ala and ola in "Gra", ewa in the default room → ewa gets neither the datagram nor the `Chat`.
  - `move_is_broadcast_and_announces_replayed`: ola announces in "Gra"; ala `JoinRoom(gra)` → all three get `MemberMoved`; ala then gets ola's `Announce`.
  - `announce_is_cleared_on_move`: ola announces in the default room, moves to "Gra"; ewa logging in to the default room gets no `Announce` from ola.
  - `relay_across_rooms_is_dropped`
  - `create_and_delete_rooms` (broadcasts, `NameTaken`, `InvalidName`, `NotAllowed`, `NotEmpty`, admin deletes another's room)
  - `rooms_survive_restart` (restart on the same `data_dir`, Hello with `RoomChoice::Room(gra)` → `Welcome.room == Some(gra)`)
  - `oversized_chat_disconnects`, `chat_flood_is_limited` (send 100 → peer receives at most `CHAT_BURST + 5`)
  Existing `relay.rs` tests move to the new message shapes; everyone lands in the default room, so their expectations hold.
- [ ] **Step 2: Run** `cargo test -p server` — Expected: FAIL to compile (new variants missing).
- [ ] **Step 3: Implement** protocol v3, `Registry`, `conn.rs` wiring (`Shared` gets `registry`, `store: Mutex<Store>`, `admins: HashSet<String>`); `start` opens `data_dir/komunikator.db`; `server.example.toml` gets `admin = true` on the sample user with a Polish comment and notes that `data_dir` also holds the room database. Then the client adaptation above.
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass, client-core session tests included.
- [ ] **Step 5: Commit** `feat: protocol v3 with server-side rooms and chat relay`.

### Task 3: Chat text helpers

**Files:** Create `crates/client-core/src/chat.rs`; Modify `crates/client-core/src/lib.rs`.

**Interfaces:**
- Produces: `pub const MAX_CHAT_CHARS: usize = 2000;` `pub enum ChatTextError { Empty, TooLong }`; `pub fn prepare_outgoing(text: &str) -> Result<String, ChatTextError>` (sanitize, trim, then check length); `pub fn sanitize(text: &str) -> String` (drops every `char::is_control` except `'\n'` and `'\t'`, and the bidi controls U+200E, U+200F, U+202A–U+202E, U+2066–U+2069; keeps at most `MAX_CHAT_CHARS` chars); `pub enum LinkKind { Url, HostPort }`; `pub struct Link { pub range: std::ops::Range<usize>, pub kind: LinkKind }` (byte range into the input); `pub fn find_links(text: &str) -> Vec<Link>` (in order of appearance).

Link rules: split on whitespace; strip leading `([{"'<` and trailing `.,;:!?)]}"'>` from each token. `Url`: starts with `http://` or `https://` (ASCII case-insensitive) with something after it. `HostPort`: IPv4 (four 0–255 decimal octets) with optional `:port`; `[IPv6]:port` (inside parsed by `std::net::Ipv6Addr`); `host:port` where the host has at least one dot, labels of `[A-Za-z0-9-]` not starting or ending with `-`, last label alphabetic. Port: digits only, 1–65535.

- [ ] **Step 1: Write failing tests:**
  - `finds_ipv4_with_port` — `"wbijaj 192.168.1.10:27015 teraz"` → one `HostPort`, `&text[range] == "192.168.1.10:27015"`.
  - `finds_host_port_before_full_stop` — `"serwer: play.example.com:25565."` → `"play.example.com:25565"`.
  - `finds_url_without_trailing_comma` — `"https://example.com/a?b=c, i tyle"` → `Url` `"https://example.com/a?b=c"`.
  - `finds_bare_ipv4_in_parens` — `"(10.0.0.1)"` → `"10.0.0.1"`.
  - `finds_ipv6_with_port` — `"[2001:db8::1]:7777"`.
  - `rejects_non_links` — each of `"999.1.1.1:80"`, `"1.2.3.4:0"`, `"1.2.3.4:70000"`, `"godzina 12:30"`, `"a:b"`, `"http://"` → empty.
  - `ranges_are_byte_offsets` — `"żółw 1.2.3.4:5 i https://x.pl"` → slices `"1.2.3.4:5"` and `"https://x.pl"`.
  - `prepare_outgoing_rules` — `"  hej \n"` → `"hej"`; `"   "` → `Empty`; 2000 × `'😀'` ok; 2001 × `'a'` → `TooLong`.
  - `sanitize_strips_controls_and_bidi` — `"a\x1b[2Jb\u{202E}c\nd\te\u{7}"` → `"a[2Jbc\nd\te"`.
- [ ] **Step 2: Run** `cargo test -p client-core chat::` — Expected: FAIL.
- [ ] **Step 3: Implement** (no regex crate).
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `feat(client-core): chat text validation and link detection`.

### Task 4: Room-bound E2E and chat encryption

**Files:** Create `crates/client-core/src/e2e/chat.rs`; Modify `crates/client-core/src/e2e/{mod.rs, group.rs, audio.rs}`, `crates/client-core/src/net/session.rs` (pass the room), `docs/e2e.md`.

**Interfaces:**
- Consumes: Task 2 (`RoomId`, `ClientMsg::Chat`, `MAX_CHAT_BLOB`), Task 3 (`MAX_CHAT_CHARS`).
- Produces:
  - `E2e::new(identity, trust, me, my_name, room: RoomId, members, next_epoch, now, now_ms)` — `room` replaces the `ROOM` const; announces are signed and verified for it.
  - `SenderKey::chat_key(&self) -> [u8; 32]` — HKDF-SHA256, empty salt, info `b"komunikator/chat/v1"`.
  - `pub struct ChatSealer` (`new(key: &SenderKey)`, `seal(&mut self, room: RoomId, me: MemberId, text: &str) -> Option<Vec<u8>>`), `pub struct ChatOpener` (`new()`, `add_key(&mut self, key: &SenderKey)`, `open(&mut self, room: RoomId, sender: MemberId, blob: &[u8]) -> Result<String, ChatError>`), `pub enum ChatError { NoKey, Malformed, UnknownEpoch, Replay, BadTag, NotUtf8 }`.
  - `E2e::seal_chat(&mut self, text: &str) -> Option<ClientMsg>` (a `ClientMsg::Chat`), `E2e::open_chat(&mut self, from: MemberId, blob: &[u8]) -> Result<String, ChatError>` (`NoKey` when the member has no link).

Wire blob: postcard of `struct SealedChat { epoch: u16, counter: u32, ct: Vec<u8> }`. AAD = `b"komunikator/chat/v1"` ‖ room u32 LE ‖ sender u16 LE ‖ epoch u16 LE ‖ counter u32 LE. Nonce = counter LE in the first 4 of 12 bytes, rest zero. The sealer counts from 0 per key and refuses after `u32::MAX`. The opener keeps the 3 newest epochs, never replaces a known epoch's key, and per epoch opens only strictly increasing counters (the control stream is ordered). `E2e` seals chat with the same switched key as audio (rebuild the `ChatSealer` wherever `sealer` is replaced) and feeds each `Link`'s `ChatOpener` wherever it feeds that link's `AudioOpener`, so the key-floor invariant covers chat. Opened text longer than `MAX_CHAT_CHARS` → `Malformed`.

- [ ] **Step 1: Write failing tests** (group tests use the existing `Net` harness, built with a room id):
  - `chat_roundtrip_between_members` — a seals `"192.168.1.10:27015"`, b opens it exactly.
  - `chat_replay_is_rejected` — the same blob again → `Replay`.
  - `tampered_chat_fails` — flip a ciphertext byte → `BadTag`.
  - `chat_from_other_room_does_not_open` — same key, sealed for room 2, opened for room 1 (`ChatSealer`/`ChatOpener` level) → `BadTag`.
  - `newcomer_cannot_read_older_chat` — a blob sealed before c's link opened; after c joins and keys flow, c opening it → `UnknownEpoch` or `NoKey`.
  - `chat_after_rotation_still_opens_previous_epoch` — sealed on epoch N; after the rotation to N+1 is delivered, the old blob still opens.
  - `longest_chat_fits_blob_limit` — 2000 × `'😀'` sealed → `blob.len() <= MAX_CHAT_BLOB`.
  - `announce_for_other_room_is_rejected` — an announce signed for room 2 reaching an `E2e` of room 1 → `E2eEvent::Rejected { reason: AnnounceError::WrongRoom, .. }`.
- [ ] **Step 2: Run** `cargo test -p client-core e2e::` — Expected: FAIL.
- [ ] **Step 3: Implement**; the session passes its current room to `E2e::new`; add a "Czat" section to `docs/e2e.md` (key derivation, AAD, replay rule, why newcomers cannot read older messages).
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass.
- [ ] **Step 5: Commit** `feat(client-core): room-bound announces and encrypted chat`.

### Task 5: Client lobby state

**Files:** Create `crates/client-core/src/lobby.rs`; Modify `crates/client-core/src/lib.rs`.

**Interfaces:**
- Produces: `#[derive(Clone, Debug)] pub struct Lobby`; `#[derive(Debug, Clone, PartialEq, Eq)] pub enum RoomChange { Entered { room: RoomId, members: Vec<MemberInfo> }, Exited, Joined(MemberInfo), Left(MemberId) }`; `Lobby::new(you: MemberId, room: Option<RoomId>, rooms: Vec<RoomInfo>, members: Vec<MemberInfo>) -> (Lobby, Option<RoomChange>)` (the `Entered` for my starting room, members excluding me); mutators returning `Vec<RoomChange>`: `member_joined(&mut self, m: MemberInfo)`, `member_left(&mut self, id: MemberId)`, `member_moved(&mut self, id: MemberId, room: Option<RoomId>)`, `room_created(&mut self, r: RoomInfo)`, `room_deleted(&mut self, id: RoomId)`; queries `me() -> MemberId`, `my_room() -> Option<RoomId>`, `rooms() -> impl Iterator<Item = &RoomInfo>` (ascending id), `members_in(&self, room: Option<RoomId>) -> Vec<&MemberInfo>`, `name_of(&self, id) -> Option<&str>`. The session keeps one; the CLI (Task 8) and the GUI (plan 4) keep their own copy fed from session events.

Rules: my own move → `Exited` (if I was in a room) then `Entered` (if the target is a room); someone entering my room (move or coming online) → `Joined`; leaving it (move or going offline) → `Left`; events about other rooms change state only.

- [ ] **Step 1: Write failing tests:** `new_enters_starting_room`, `new_in_no_room_emits_nothing`, `other_moving_in_and_out_of_my_room`, `my_move_exits_then_enters`, `my_leave_only_exits`, `member_online_in_other_room_is_silent`, `offline_member_in_my_room_left`, `rooms_created_and_deleted`.
- [ ] **Step 2: Run** `cargo test -p client-core lobby::` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `feat(client-core): lobby state for rooms and members`.

### Task 6: Session with rooms and chat

**Files:** Modify `crates/client-core/src/net/session.rs`, `crates/client-core/tests/common/mod.rs`, `crates/client-core/tests/{session.rs, e2e_session.rs}`, `crates/client-win/src/main.rs` (compile only; commands come in Task 8); Create `crates/client-core/tests/rooms_session.rs`.

**Interfaces:**
- Consumes: Tasks 2–5.
- Produces:
  - `SessionCommand` (`Debug, Clone, PartialEq, Eq`; no longer `Copy`): `TrustChanged(MemberId)`, `JoinRoom(RoomId)`, `LeaveRoom`, `CreateRoom(String)`, `DeleteRoom(RoomId)`, `SendChat(String)`.
  - `SessionEvent`: `Connecting`, `Connected { you, room: Option<RoomId>, rooms: Vec<RoomInfo>, members: Vec<MemberInfo> }` (all online members), `MemberJoined(MemberInfo)`, `MemberLeft(MemberId)`, `MemberMoved { id, room: Option<RoomId> }`, `RoomCreated(RoomInfo)`, `RoomDeleted(RoomId)`, `RoomError(RoomErrorReason)`, `Chat(ChatLine)`, `ChatUnreadable { from: MemberId, name: String }`, `ChatNotSent(ChatSendError)`, `Disconnected { retry_in }`, `Fatal(FatalReason)`, `E2e(E2eEvent)`.
  - `pub struct ChatLine { pub from: MemberId, pub name: String, pub text: String, pub own: bool, pub at: SystemTime }`; `pub enum ChatSendError { Empty, TooLong, NotInRoom, NotConnected }`.

Behaviour:
- The task keeps `room_choice: RoomChoice` (starts `Default`), changed only by a server-confirmed `MemberMoved` of me (`Room(r)` / `Nowhere`) and by `RoomDeleted` of that room (back to `Default`), and sends it in every `Hello`, so a reconnect returns to the last room.
- `Link` holds a `Lobby` and `e2e: Option<E2e>`. `RoomChange::Entered` → a new `E2e` for that room, `next_epoch` carried from the previous `E2e` (or the task's); `Exited` → save `next_epoch`, drop the `E2e`; `Joined`/`Left` → `member_joined`/`member_left`. Tick, seal and open run only with an `E2e`; audio while in no room is dropped.
- `SendChat`: `chat::prepare_outgoing` errors → `ChatNotSent(Empty | TooLong)`; no room → `ChatNotSent(NotInRoom)`; otherwise queue `seal_chat` and emit `Chat { own: true, .. }`.
- Incoming `Chat`: `open_chat` → `Chat(ChatLine)` with `chat::sanitize` applied and the name from the lobby; any error → `ChatUnreadable` plus a `debug!` naming the error kind, never the content.
- While disconnected (`wait`): `JoinRoom(r)` sets `room_choice = Room(r)`, `LeaveRoom` sets `Nowhere`, `SendChat` → `ChatNotSent(NotConnected)`, other commands are dropped. The drain at `Welcome` stays for stale `TrustChanged`.

- [ ] **Step 1: Write failing tests** in `rooms_session.rs` (add `create_room(handle, name) -> RoomId` to `common/mod.rs`, waiting for `RoomCreated`):
  - `chat_reaches_room_member_encrypted` — ala and ola in the default room; ala sends `"192.168.1.10:27015"`; ala gets `Chat { own: true }`, ola gets the same text with `name == "ala"`.
  - `audio_follows_room_moves` — ola moves to "Gra" → ala's frames stop reaching ola; ala moves to "Gra" → frames flow again within 3 s.
  - `reconnect_returns_to_last_room` — ala in "Gra"; server restarted on the same data dir → the next `Connected.room == Some(gra)`.
  - `deleted_room_falls_back_to_default_on_reconnect`
  - `chat_errors` — `SendChat("   ")` → `ChatNotSent(Empty)`; after `LeaveRoom`, `SendChat("x")` → `ChatNotSent(NotInRoom)`.
  - `member_rejoining_is_heard_again` — ala and ola talking; ola's session shuts down and a new ola session (same device) logs in; audio flows both ways within 3 s (plan 2 backlog: one member reconnects while the other stays).
  Existing session and e2e tests move to the new `Connected` shape.
- [ ] **Step 2: Run** `cargo test -p client-core --test rooms_session` — Expected: FAIL.
- [ ] **Step 3: Implement**; keep `client-win/src/main.rs` compiling (print new events minimally).
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass.
- [ ] **Step 5: Commit** `feat(client-core): session with rooms and encrypted chat`.

### Task 7: Per-person volume and deafen

**Files:** Create `crates/client-core/src/audio/volume.rs`; Modify `crates/client-core/src/audio/mod.rs`, `crates/client-win/src/pipeline.rs`, `crates/client-win/src/main.rs` (pass a `GainTable`).

**Interfaces:**
- Produces:
  - `#[derive(Serialize, Deserialize, Default, Clone, Debug)] pub struct VolumeBook` keyed by username: `percent(&self, name) -> u16` (default 100), `set_percent(&mut self, name, p: u16)` (clamped to 0–200), `is_muted(&self, name) -> bool`, `toggle_muted(&mut self, name) -> bool` (new state), `gain(&self, name) -> f32` (0.0 when muted, else percent / 100).
  - `pub struct GainTable` — 64 fixed slots of atomics, no locks or allocation after `new()`: `new() -> GainTable`, `set(&self, id: MemberId, gain: f32) -> bool` (false when full), `remove(&self, id)`, `get(&self, id) -> f32` (1.0 when absent). One writer (the UI thread), one reader (render); a torn update may affect one 20 ms frame.
  - `render_loop(sink, inp, controls, gains: Arc<GainTable>, stop)` — each voice is accumulated with `gains.get(id)` before the final clip.
  - Capture sends nothing while `muted` **or** `deafened`. Ruling: deafen also silences the mic, as users of voice apps expect; the flags stay separate, so undeafen restores the previous mute state.

- [ ] **Step 1: Write failing tests:** `volume_book_defaults_and_clamps` (300 → 200), `muting_keeps_percent` (set 150, mute → gain 0, unmute → 1.5), `volume_book_roundtrips_toml`, `gain_table_set_get_remove_and_full` (65th id → false), pipeline `render_applies_per_sender_gain` (two senders; gain 0.5 on one halves its contribution; gain 0 leaves only the other), `deafened_sends_nothing`.
- [ ] **Step 2: Run** `cargo test -p client-core volume` and `cargo test -p client-win pipeline` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass.
- [ ] **Step 5: Commit** `feat: per-person volume, local mute and deafen that silences the mic`.

### Task 8: CLI commands, docs, acceptance list

**Files:** Modify `crates/client-win/src/main.rs`, `crates/client-win/Cargo.toml` (feature `Win32_System_SystemInformation` for `GetLocalTime`), `README.md`; Create `docs/acceptance/plan-3.md`.

**Interfaces:**
- Consumes: Tasks 3, 5, 6, 7 (`find_links`, `Lobby`, `SessionCommand`, `SessionEvent`, `VolumeBook`, `GainTable`).

Commands (new Polish `HELP`):
- `p` — rooms with their members, my room marked `*`; room number = `RoomId`.
- `w <nr>` — enter a room; `o` — leave the room; `n <nazwa>` — create a room; `u <nr>` — delete a room.
- `t <tekst>` — send to the room chat. Received lines print as `[HH:MM] imię: tekst` (local time via `GetLocalTime`; own lines as `ty`), then one `  link N: …` line per `find_links` match. `ChatUnreadable` prints `nie udało się odszyfrować wiadomości od <imię> (klucze jeszcze się wymieniają)`. `ChatNotSent` reasons get Polish messages.
- `g <id> <0-200>` — a person's volume in %; `x <id>` — local mute of a person on/off. Both update the `VolumeBook` by name and the `GainTable` by id; the main loop re-applies the book on `Connected`, `MemberJoined` and `MemberMoved`, and removes ids on `MemberLeft`.
- `k` — people in my room with trust and code (as today, limited to the room).
- `z <id> <grupa>` / `a <id> <grupa>` — plan 2 backlog: the second argument must equal the first 4-digit group of that person's safety code, otherwise refuse with `wpisz pierwszą grupę kodu, który porównaliście, np. z 3 1234`.
- `m`, `d`, `q` unchanged. `RoomError` reasons: `niepoprawna nazwa (1–32 znaki)`, `nazwa zajęta`, `za dużo pokoi`, `pokój nie istnieje`, `nie możesz usunąć cudzego pokoju`, `w pokoju ktoś jest`, `pokój pełny`.

- [ ] **Step 1: Write failing tests** (unit tests in `main.rs`, no network): `z_requires_matching_code_group` (wrong group → no `TrustChanged`; right group → sent), `room_error_messages_are_polish` (each `RoomErrorReason` maps to its message above), `chat_line_lists_links` (the formatting helper returns the text line plus one link line per match).
- [ ] **Step 2: Run** `cargo test -p client-win` — Expected: FAIL.
- [ ] **Step 3: Implement**; README gets the new commands and the `admin` flag; `docs/acceptance/plan-3.md` lists the players' checks: switching rooms mid-game, chat with an IP pasted into a game, volume 0/100/200 % on a loud friend, deafen silences both ways, the room list after a server restart.
- [ ] **Step 4: Run** `cargo test --workspace` and `cargo build --release` — Expected: all pass, no warnings.
- [ ] **Step 5: Commit** `feat(client-win): CLI rooms, chat and volume commands`.

### Task 9: Plan 2 backlog fixes

**Files:** Modify `crates/client-core/src/e2e/trust.rs`, `crates/client-core/src/e2e/pair.rs` (comment only).

- [ ] **Step 1: Write failing test** `non_utf8_trust_file_is_set_aside` — write bytes `[0xff, 0xfe, 0x00]` to `known_devices.toml`; `KnownDevices::load` returns `Ok`, the store is empty and `recovered_from()` is `Some(path)` ending in `known_devices.toml.broken`.
- [ ] **Step 2: Run** `cargo test -p client-core trust::` — Expected: FAIL (load returns `Err` with `InvalidData`).
- [ ] **Step 3: Implement** — read bytes and treat invalid UTF-8 exactly like a malformed file. Fix the `pair.rs` comment about restoring snow's handshake state so it states only what the code guarantees (message 2's `ee`/`se` `mix_key` re-derives the key before decrypting).
- [ ] **Step 4: Run** `cargo test --workspace` — Expected: all pass.
- [ ] **Step 5: Commit** `fix(client-core): non-UTF-8 trust file no longer blocks startup`.
