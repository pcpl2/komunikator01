//! A server session that reconnects on its own until told to stop or a fatal error.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use protocol::audio::{encode_upstream, parse_downstream, MAX_AUDIO_PAYLOAD, MAX_DATAGRAM};
use protocol::{
    encode_frame, Auth, ClientMsg, FrameDecoder, MemberId, MemberInfo, RejectReason, RoomChoice,
    RoomErrorReason, RoomId, RoomInfo, ServerMsg, CLOSE_BLOCKED, PROTOCOL_VERSION,
};
use quinn::{Connection, ConnectionError, Endpoint, RecvStream, SendStream, VarInt};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use super::{client_config, Backoff, SERVER_NAME};
use crate::audio::frame::{EncodedFrame, IncomingFrame, MAX_PACKET};
use crate::chat::{self, ChatTextError};
use crate::e2e::{ChatReplay, E2e, E2eEvent, Identity, KnownDevices, Output, TAG_LEN};
use crate::lobby::{Lobby, RoomChange};

/// Close code the server uses when this account logged in elsewhere.
const CLOSE_REPLACED: u32 = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
const AUDIO_OUT_CAPACITY: usize = 64;
const AUDIO_IN_CAPACITY: usize = 256;
/// How often the E2E state machine is ticked (key switches, handshake retries).
const E2E_TICK: Duration = Duration::from_millis(100);
/// Queued control messages must make write progress this often, or we reconnect.
const CTRL_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Unwritten control bytes beyond this mean the server is not reading; reconnect.
const CTRL_QUEUE_MAX: usize = 256 * 1024;

// A sealed frame (largest Opus packet plus tag) must fit an audio datagram.
const _: () = assert!(MAX_PACKET + TAG_LEN <= MAX_AUDIO_PAYLOAD);

#[derive(Clone)]
pub struct SessionParams {
    /// "host:port"
    pub server: String,
    pub fingerprint: [u8; 32],
    pub username: String,
    /// How the first login proves who I am; after any `Welcome` the session logs in again
    /// with the token it was given.
    pub credentials: Credentials,
    /// This device's long-term E2E identity.
    pub identity: Arc<Identity>,
    /// TOFU store of peer devices, shared with whoever accepts or verifies keys.
    pub trust: Arc<Mutex<KnownDevices>>,
}

impl std::fmt::Debug for SessionParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionParams")
            .field("server", &self.server)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// What the session logs in with.
#[derive(Clone, PartialEq, Eq)]
pub enum Credentials {
    Password(String),
    /// From an earlier `Welcome`; single use.
    Token([u8; 32]),
    /// Creates the account with a one-time invite code.
    Register { invite: [u8; 16], password: String },
}

/// Names the kind only, so secrets never end up in a log.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::Password(_) => f.write_str("Password(<ukryte>)"),
            Credentials::Token(_) => f.write_str("Token(<ukryte>)"),
            Credentials::Register { .. } => f.write_str("Register { invite: <ukryte>, password: <ukryte> }"),
        }
    }
}

impl Credentials {
    fn auth(&self) -> Auth {
        match self {
            Credentials::Password(p) => Auth::Password(p.clone()),
            Credentials::Token(t) => Auth::Token(*t),
            Credentials::Register { invite, password } => {
                Auth::Register { invite: *invite, password: password.clone() }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Connecting,
    /// The server issued a new session token; store it for the next start (the one used to
    /// log in is spent). Never log it.
    TokenIssued([u8; 32]),
    /// `members` are everyone online (me included), whatever their room.
    Connected { you: MemberId, room: Option<RoomId>, rooms: Vec<RoomInfo>, members: Vec<MemberInfo> },
    MemberJoined(MemberInfo),
    MemberLeft(MemberId),
    MemberMoved { id: MemberId, room: Option<RoomId> },
    RoomCreated(RoomInfo),
    RoomDeleted(RoomId),
    RoomError(RoomErrorReason),
    Chat(ChatLine),
    /// A chat message from `from` that could not be decrypted; its content is unknown.
    ChatUnreadable { from: MemberId, name: String },
    ChatNotSent(ChatSendError),
    Disconnected { retry_in: Duration },
    Fatal(FatalReason),
    E2e(E2eEvent),
}

/// A chat message of my room, decrypted and sanitized (or my own, as sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    pub from: MemberId,
    pub name: String,
    pub text: String,
    pub own: bool,
    pub at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatSendError {
    Empty,
    TooLong,
    NotInRoom,
    NotConnected,
}

/// Requests to the running session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    /// The trust store entry for this member's device was changed (accepted or verified).
    TrustChanged(MemberId),
    JoinRoom(RoomId),
    LeaveRoom,
    CreateRoom(String),
    DeleteRoom(RoomId),
    SendChat(String),
}

/// Errors after which the session stops for good instead of reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatalReason {
    BadCredentials,
    VersionMismatch { server: u16 },
    SessionReplaced,
    /// The stored session token was spent, expired or revoked; a password is needed.
    LoginRequired,
    InviteInvalid,
    NameTaken,
    NameInvalid,
    PasswordTooShort,
    AccountBlocked,
}

pub struct SessionHandle {
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
    /// Filled from the capture thread with try_send; frames are dropped while disconnected.
    pub audio_out: mpsc::Sender<EncodedFrame>,
    /// Drained from the render thread with try_recv.
    pub audio_in: mpsc::Receiver<IncomingFrame>,
    pub commands: mpsc::UnboundedSender<SessionCommand>,
    stop: watch::Sender<bool>,
}

impl SessionHandle {
    pub fn shutdown(&self) {
        let _ = self.stop.send(true);
    }
}

pub fn spawn_session(rt: &tokio::runtime::Handle, params: SessionParams) -> SessionHandle {
    let (events_tx, events) = mpsc::unbounded_channel();
    let (audio_out, out_rx) = mpsc::channel(AUDIO_OUT_CAPACITY);
    let (in_tx, audio_in) = mpsc::channel(AUDIO_IN_CAPACITY);
    let (commands, cmd_rx) = mpsc::unbounded_channel();
    let (stop, stop_rx) = watch::channel(false);
    let session = random_session_id();
    let task = Task {
        params,
        session,
        events: events_tx,
        out_rx,
        in_tx,
        cmd_rx,
        stop: stop_rx,
        next_epoch: 0,
        chat_replay: ChatReplay::default(),
        seq: WireSeq::default(),
        room_choice: RoomChoice::Default,
    };
    rt.spawn(task.run());
    SessionHandle { events, audio_out, audio_in, commands, stop }
}

enum Outcome {
    /// Connection lost or never established; retry after a delay.
    Retry,
    Fatal(FatalReason),
    Stop,
}

struct Task {
    params: SessionParams,
    /// Sent with every Hello of this run; see `ClientMsg::Hello::session`.
    session: u64,
    events: mpsc::UnboundedSender<SessionEvent>,
    out_rx: mpsc::Receiver<EncodedFrame>,
    in_tx: mpsc::Sender<IncomingFrame>,
    cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
    stop: watch::Receiver<bool>,
    /// First sender-key epoch for the next room key set; carried across rooms and reconnects
    /// so no epoch repeats within a run.
    next_epoch: u16,
    /// Chat replay log for the whole run, lent to the current `E2e`: rebuilding E2E state
    /// (re-entering a room, reconnecting) must not let old chat blobs open again.
    chat_replay: ChatReplay,
    seq: WireSeq,
    /// Sent in every `Hello`, so a reconnect returns to the last room the server confirmed.
    room_choice: RoomChoice,
}

/// Maps capture sequence numbers to the ones sent (and used as AEAD nonces): strictly
/// increasing for the whole run, even if capture restarts its numbering, while keeping
/// capture gaps so receivers still see losses.
#[derive(Default)]
struct WireSeq {
    offset: u32,
    last: Option<u32>,
}

impl WireSeq {
    fn map(&mut self, capture: u32) -> u32 {
        let mut wire = capture.wrapping_add(self.offset);
        if let Some(last) = self.last.filter(|&l| wire <= l) {
            wire = last.wrapping_add(1);
            self.offset = wire.wrapping_sub(capture);
        }
        self.last = Some(wire);
        wire
    }
}

/// Wall clock in Unix milliseconds (0 before the epoch), for signed announces.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Task {
    async fn run(mut self) {
        let mut backoff = Backoff::new();
        loop {
            self.emit(SessionEvent::Connecting);
            match self.connect_once(&mut backoff).await {
                Outcome::Stop => return,
                Outcome::Fatal(reason) => {
                    self.emit(SessionEvent::Fatal(reason));
                    return;
                }
                Outcome::Retry => {
                    let retry_in = backoff.next_delay();
                    self.emit(SessionEvent::Disconnected { retry_in });
                    if self.wait(retry_in).await {
                        return;
                    }
                }
            }
        }
    }

    fn emit(&self, e: SessionEvent) {
        let _ = self.events.send(e);
    }

    /// Sleeps while discarding stale outgoing audio and answering commands the way an
    /// offline session can; true when asked to stop.
    async fn wait(&mut self, delay: Duration) -> bool {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return false,
                _ = self.stop.changed() => return true,
                f = self.out_rx.recv() => if f.is_none() { return true },
                Some(c) = self.cmd_rx.recv() => match c {
                    SessionCommand::JoinRoom(r) => self.room_choice = RoomChoice::Room(r),
                    SessionCommand::LeaveRoom => self.room_choice = RoomChoice::Nowhere,
                    SessionCommand::SendChat(_) => {
                        self.emit(SessionEvent::ChatNotSent(ChatSendError::NotConnected))
                    }
                    SessionCommand::TrustChanged(_)
                    | SessionCommand::CreateRoom(_)
                    | SessionCommand::DeleteRoom(_) => {}
                },
            }
        }
    }

    async fn connect_once(&mut self, backoff: &mut Backoff) -> Outcome {
        let Some(conn) = self.connect().await else { return Outcome::Retry };
        let hello = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            username: self.params.username.clone(),
            auth: self.params.credentials.auth(),
            session: self.session,
            room: self.room_choice,
        };
        let login = async {
            let (mut send, mut recv) = conn.open_bi().await.ok()?;
            send.write_all(&encode_frame(&hello)).await.ok()?;
            let mut dec = FrameDecoder::new();
            let reply = read_msg(&mut recv, &mut dec).await;
            Some((send, recv, dec, reply))
        };
        // A wedged server can keep the connection alive with keep-alives yet never answer.
        let result = tokio::select! {
            r = timeout(LOGIN_TIMEOUT, login) => r,
            _ = self.stop.changed() => return Outcome::Stop,
        };
        // The send half must stay open: dropping it would finish the stream and log us out.
        let Ok(Some((mut send, mut recv, mut dec, reply))) = result else {
            conn.close(VarInt::from_u32(0), b"");
            return Outcome::Retry;
        };
        let (lobby, entered, queued) = match reply {
            Some(ServerMsg::Welcome { you, room, rooms, members, token }) => {
                backoff.reset();
                // The credentials just used are spent (a token) or must not be sent again (an
                // invite); every later login of this run uses the fresh token.
                self.params.credentials = Credentials::Token(token);
                self.emit(SessionEvent::TokenIssued(token));
                self.welcomed(you, room, rooms, members)
            }
            Some(ServerMsg::Rejected { reason }) => {
                return match reason {
                    RejectReason::BadCredentials => Outcome::Fatal(FatalReason::BadCredentials),
                    RejectReason::VersionMismatch { server } => {
                        Outcome::Fatal(FatalReason::VersionMismatch { server })
                    }
                    RejectReason::RateLimited => Outcome::Retry,
                    RejectReason::TokenInvalid => Outcome::Fatal(FatalReason::LoginRequired),
                    RejectReason::InviteInvalid => Outcome::Fatal(FatalReason::InviteInvalid),
                    RejectReason::NameTaken => Outcome::Fatal(FatalReason::NameTaken),
                    RejectReason::NameInvalid => Outcome::Fatal(FatalReason::NameInvalid),
                    RejectReason::PasswordTooShort => Outcome::Fatal(FatalReason::PasswordTooShort),
                    RejectReason::AccountBlocked => Outcome::Fatal(FatalReason::AccountBlocked),
                }
            }
            Some(ServerMsg::SessionReplaced) => return Outcome::Fatal(FatalReason::SessionReplaced),
            _ => return closed_outcome(&conn),
        };
        let mut link = Link::new(&conn, &mut send, lobby);
        let mut started = self.room_changes(&mut link, entered.into_iter().collect());
        for c in queued {
            started = started.and_then(|()| self.command(&mut link, c));
        }
        let outcome = match started {
            Ok(()) => self.connected_loop(&mut link, &mut recv, &mut dec).await,
            Err(o) => o,
        };
        // Keys of the next connection must not reuse an epoch of this one.
        if let Some(e2e) = link.e2e.take() {
            self.retire(e2e);
        }
        conn.close(VarInt::from_u32(0), b"");
        outcome
    }

    /// Starts a connection's state from the server's `Welcome`: returns my lobby view, the room
    /// I start in, and the commands issued while connecting that still apply.
    fn welcomed(
        &mut self,
        you: MemberId,
        room: Option<RoomId>,
        rooms: Vec<RoomInfo>,
        members: Vec<MemberInfo>,
    ) -> (Lobby, Option<RoomChange>, Vec<SessionCommand>) {
        // Audio captured while connecting is stale; sending it would play as a burst.
        while self.out_rx.try_recv().is_ok() {}
        // Where the server actually put me (the room asked for may have been full or gone) is
        // where a reconnect should return.
        self.room_choice = room.map_or(RoomChoice::Nowhere, RoomChoice::Room);
        let mut queued = Vec::new();
        while let Ok(c) = self.cmd_rx.try_recv() {
            match c {
                // Member ids from an old connection mean nothing now, and a new E2e reads the
                // trust store fresh anyway.
                SessionCommand::TrustChanged(_) => {}
                // Typed while offline: sending it later, maybe into another room, would surprise.
                SessionCommand::SendChat(_) => {
                    self.emit(SessionEvent::ChatNotSent(ChatSendError::NotConnected))
                }
                c => queued.push(c),
            }
        }
        let (lobby, entered) = Lobby::new(you, room, rooms.clone(), members.clone());
        self.emit(SessionEvent::Connected { you, room, rooms, members });
        (lobby, entered, queued)
    }

    /// Takes back what outlives one room's E2E state: the epoch counter and the replay log.
    fn retire(&mut self, old: E2e) {
        self.next_epoch = old.next_epoch();
        self.chat_replay = old.into_chat_replay();
    }

    async fn connect(&self) -> Option<Connection> {
        let addr = resolve(&self.params.server).await?;
        let bind: SocketAddr = if addr.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let mut endpoint = Endpoint::client(bind).ok()?;
        endpoint.set_default_client_config(client_config(self.params.fingerprint).ok()?);
        let connecting = endpoint.connect(addr, SERVER_NAME).ok()?;
        match timeout(CONNECT_TIMEOUT, connecting).await {
            Ok(Ok(conn)) => Some(conn),
            Ok(Err(e)) => {
                tracing::debug!("connect failed: {e}");
                None
            }
            Err(_) => None,
        }
    }

    async fn connected_loop(
        &mut self,
        link: &mut Link<'_>,
        recv: &mut RecvStream,
        dec: &mut FrameDecoder,
    ) -> Outcome {
        let mut tick = tokio::time::interval(E2E_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut up = Vec::with_capacity(MAX_DATAGRAM);
        let mut sealed = Vec::with_capacity(MAX_AUDIO_PAYLOAD);
        let mut plain = Vec::with_capacity(MAX_AUDIO_PAYLOAD);
        let (mut unsealed, mut unopened) = (0u64, 0u64);
        loop {
            // `read_msg` is cancel-safe: partial input stays in `dec`, and quinn reads are.
            let handled = tokio::select! {
                m = read_msg(recv, dec) => match m {
                    Some(m) => self.server_msg(link, m),
                    None => return closed_outcome(link.conn),
                },
                // `write` is cancel-safe: if another branch wins, nothing was written.
                w = link.send.write(&link.ctrl_out[link.ctrl_sent..]), if link.ctrl_pending() => match w {
                    Ok(n) => {
                        link.ctrl_sent += n;
                        link.ctrl_deadline = Instant::now() + CTRL_WRITE_TIMEOUT;
                        continue;
                    }
                    Err(_) => return closed_outcome(link.conn),
                },
                _ = tokio::time::sleep_until(link.ctrl_deadline.into()), if link.ctrl_pending() => {
                    tracing::debug!("control stream stalled; reconnecting");
                    return Outcome::Retry;
                }
                _ = tick.tick() => match link.e2e.as_mut() {
                    Some(e2e) => {
                        let out = e2e.tick(Instant::now());
                        link.output(out, &self.events)
                    }
                    None => continue,
                },
                Some(c) = self.cmd_rx.recv() => self.command(link, c),
                _ = self.stop.changed() => return Outcome::Stop,
                d = link.conn.read_datagram() => match d {
                    Ok(d) => {
                        // Outside any room nobody may send to me; such frames are dropped.
                        if let (Ok(p), Some(e2e)) = (parse_downstream(&d), link.e2e.as_mut()) {
                            let now = Instant::now();
                            let frame = e2e
                                .open(p.sender, p.epoch, p.seq, p.payload, now, &mut plain)
                                .then(|| IncomingFrame::new(p.sender, p.seq, &plain))
                                .flatten();
                            match frame {
                                Some(f) => {
                                    let _ = self.in_tx.try_send(f);
                                }
                                None => {
                                    unopened += 1;
                                    tracing::debug!(unopened, "dropped audio frame that did not open");
                                }
                            }
                        }
                        continue;
                    }
                    Err(_) => return closed_outcome(link.conn),
                },
                f = self.out_rx.recv() => match f {
                    Some(f) => {
                        let seq = self.seq.map(f.seq);
                        // In no room there is nobody to talk to.
                        if let Some(e2e) = link.e2e.as_mut() {
                            match e2e.seal(seq, f.payload(), &mut sealed) {
                                Some(epoch) => {
                                    encode_upstream(epoch, seq, &sealed, &mut up);
                                    let _ = link.conn.send_datagram(Bytes::copy_from_slice(&up));
                                }
                                None => {
                                    unsealed += 1;
                                    tracing::debug!(unsealed, "dropped audio frame that could not be sealed");
                                }
                            }
                        }
                        continue;
                    }
                    None => return Outcome::Stop,
                },
            };
            if let Err(o) = handled {
                return o;
            }
        }
    }

    /// Mirrors lobby changes to the caller for everyone online; E2E only follows my room.
    fn server_msg(&mut self, link: &mut Link<'_>, m: ServerMsg) -> Result<(), Outcome> {
        let now = Instant::now();
        let changes = match m {
            ServerMsg::MemberJoined(m) => {
                self.emit(SessionEvent::MemberJoined(m.clone()));
                link.lobby.member_joined(m)
            }
            ServerMsg::MemberLeft { id } => {
                self.emit(SessionEvent::MemberLeft(id));
                link.lobby.member_left(id)
            }
            ServerMsg::MemberMoved { id, room } => {
                if id == link.lobby.me() {
                    self.room_choice = room.map_or(RoomChoice::Nowhere, RoomChoice::Room);
                }
                self.emit(SessionEvent::MemberMoved { id, room });
                link.lobby.member_moved(id, room)
            }
            ServerMsg::RoomCreated(r) => {
                self.emit(SessionEvent::RoomCreated(r.clone()));
                link.lobby.room_created(r)
            }
            ServerMsg::RoomDeleted { room } => {
                if self.room_choice == RoomChoice::Room(room) {
                    self.room_choice = RoomChoice::Default;
                }
                self.emit(SessionEvent::RoomDeleted(room));
                link.lobby.room_deleted(room)
            }
            ServerMsg::RoomError { reason } => {
                self.emit(SessionEvent::RoomError(reason));
                return Ok(());
            }
            ServerMsg::Chat { from, blob } => {
                self.incoming_chat(link, from, &blob);
                return Ok(());
            }
            ServerMsg::Announce { from, blob } => {
                return match link.e2e.as_mut() {
                    Some(e2e) => {
                        let out = e2e.announce(from, &blob, now, now_ms());
                        link.output(out, &self.events)
                    }
                    None => Ok(()),
                };
            }
            ServerMsg::Relay { from, blob } => {
                return match link.e2e.as_mut() {
                    Some(e2e) => {
                        let out = e2e.relay(from, &blob, now);
                        link.output(out, &self.events)
                    }
                    None => Ok(()),
                };
            }
            ServerMsg::SessionReplaced => return Err(Outcome::Fatal(FatalReason::SessionReplaced)),
            ServerMsg::Welcome { .. } | ServerMsg::Rejected { .. } => return Ok(()),
        };
        self.room_changes(link, changes)
    }

    /// Keeps E2E in step with my room: entering one starts a fresh key set for it, leaving
    /// drops it, and arrivals and departures there are passed on.
    fn room_changes(&mut self, link: &mut Link<'_>, changes: Vec<RoomChange>) -> Result<(), Outcome> {
        let now = Instant::now();
        for change in changes {
            let out = match change {
                RoomChange::Entered { room, members } => {
                    if let Some(old) = link.e2e.take() {
                        self.retire(old);
                    }
                    let (e2e, out) = E2e::new(
                        self.params.identity.clone(),
                        self.params.trust.clone(),
                        link.lobby.me(),
                        self.params.username.clone(),
                        room,
                        &members,
                        self.next_epoch,
                        std::mem::take(&mut self.chat_replay),
                        now,
                        now_ms(),
                    );
                    link.e2e = Some(e2e);
                    out
                }
                RoomChange::Exited => {
                    if let Some(old) = link.e2e.take() {
                        self.retire(old);
                    }
                    continue;
                }
                RoomChange::Joined(m) => match link.e2e.as_mut() {
                    Some(e2e) => e2e.member_joined(&m, now),
                    None => continue,
                },
                RoomChange::Left(id) => match link.e2e.as_mut() {
                    Some(e2e) => e2e.member_left(id, now),
                    None => continue,
                },
            };
            link.output(out, &self.events)?;
        }
        Ok(())
    }

    fn command(&mut self, link: &mut Link<'_>, c: SessionCommand) -> Result<(), Outcome> {
        let msg = match c {
            SessionCommand::TrustChanged(id) => {
                return match link.e2e.as_mut() {
                    Some(e2e) => {
                        let out = e2e.trust_changed(id);
                        link.output(out, &self.events)
                    }
                    None => Ok(()),
                };
            }
            SessionCommand::JoinRoom(room) => ClientMsg::JoinRoom { room },
            SessionCommand::LeaveRoom => ClientMsg::LeaveRoom,
            SessionCommand::CreateRoom(name) => ClientMsg::CreateRoom { name },
            SessionCommand::DeleteRoom(room) => ClientMsg::DeleteRoom { room },
            SessionCommand::SendChat(text) => match self.outgoing_chat(link, &text) {
                Some(msg) => msg,
                None => return Ok(()),
            },
        };
        link.queue(&[msg])
    }

    /// Seals `text` for my room and reports it as sent; `None` (with the reason reported)
    /// when it cannot be sent.
    fn outgoing_chat(&mut self, link: &mut Link<'_>, text: &str) -> Option<ClientMsg> {
        let text = match chat::prepare_outgoing(text) {
            Ok(t) => t,
            Err(e) => {
                let reason = match e {
                    ChatTextError::Empty => ChatSendError::Empty,
                    ChatTextError::TooLong => ChatSendError::TooLong,
                };
                self.emit(SessionEvent::ChatNotSent(reason));
                return None;
            }
        };
        let Some(e2e) = link.e2e.as_mut() else {
            self.emit(SessionEvent::ChatNotSent(ChatSendError::NotInRoom));
            return None;
        };
        // Length was checked above, so only a spent key (2^32 messages) gets here.
        let Some(msg) = e2e.seal_chat(&text) else {
            tracing::warn!("chat key spent; message not sent");
            return None;
        };
        self.emit(SessionEvent::Chat(ChatLine {
            from: link.lobby.me(),
            name: self.params.username.clone(),
            text,
            own: true,
            at: SystemTime::now(),
        }));
        Some(msg)
    }

    fn incoming_chat(&mut self, link: &mut Link<'_>, from: MemberId, blob: &[u8]) {
        let opened = match link.e2e.as_mut() {
            // The name the sender's announce verified against: the server can relabel ids in
            // the lobby, but not the name a peer's key is bound to.
            Some(e2e) => e2e
                .open_chat(from, blob)
                .map(|text| (e2e.peer_name(from).unwrap_or("?").to_owned(), text))
                .map_err(|e| format!("{e:?}")),
            None => Err("not in a room".to_owned()),
        };
        match opened {
            Ok((name, text)) => self.emit(SessionEvent::Chat(ChatLine {
                from,
                name,
                text: chat::sanitize(&text),
                own: false,
                at: SystemTime::now(),
            })),
            Err(kind) => {
                // The kind only; never the content.
                tracing::debug!(from, error = %kind, "chat message did not open");
                let name = link.lobby.name_of(from).unwrap_or("?").to_owned();
                self.emit(SessionEvent::ChatUnreadable { from, name });
            }
        }
    }
}

/// One live connection with its lobby view and, while I am in a room, its E2E state.
struct Link<'a> {
    conn: &'a Connection,
    /// Control stream to the server.
    send: &'a mut SendStream,
    lobby: Lobby,
    /// Keys for my current room; `None` while in no room.
    e2e: Option<E2e>,
    /// Encoded control messages not yet written; `ctrl_out[ctrl_sent..]` is pending. Written
    /// from the main loop so a server that stops reading cannot block audio or shutdown.
    ctrl_out: Vec<u8>,
    ctrl_sent: usize,
    /// Pending control bytes must make progress by then, or the connection is dropped.
    ctrl_deadline: Instant,
}

impl<'a> Link<'a> {
    fn new(conn: &'a Connection, send: &'a mut SendStream, lobby: Lobby) -> Self {
        Self {
            conn,
            send,
            lobby,
            e2e: None,
            ctrl_out: Vec::new(),
            ctrl_sent: 0,
            ctrl_deadline: Instant::now(),
        }
    }

    fn ctrl_pending(&self) -> bool {
        self.ctrl_sent < self.ctrl_out.len()
    }

    /// Reports the state machine's events and queues its messages for the control stream.
    fn output(&mut self, out: Output, events: &mpsc::UnboundedSender<SessionEvent>) -> Result<(), Outcome> {
        for e in out.events {
            let _ = events.send(SessionEvent::E2e(e));
        }
        self.queue(&out.send)
    }

    /// Queues messages for the control stream. Fails when the queue grows past
    /// `CTRL_QUEUE_MAX` (the server is not reading).
    fn queue(&mut self, msgs: &[ClientMsg]) -> Result<(), Outcome> {
        if msgs.is_empty() {
            return Ok(());
        }
        if !self.ctrl_pending() {
            self.ctrl_out.clear();
            self.ctrl_sent = 0;
            self.ctrl_deadline = Instant::now() + CTRL_WRITE_TIMEOUT;
        } else {
            self.ctrl_out.drain(..self.ctrl_sent);
            self.ctrl_sent = 0;
        }
        for m in msgs {
            self.ctrl_out.extend_from_slice(&encode_frame(m));
        }
        if self.ctrl_out.len() - self.ctrl_sent > CTRL_QUEUE_MAX {
            tracing::debug!("control queue overflow; reconnecting");
            return Err(Outcome::Retry);
        }
        Ok(())
    }
}

fn closed_outcome(conn: &Connection) -> Outcome {
    match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed(c)) if c.error_code == VarInt::from_u32(CLOSE_REPLACED) => {
            Outcome::Fatal(FatalReason::SessionReplaced)
        }
        Some(ConnectionError::ApplicationClosed(c)) if c.error_code == VarInt::from_u32(CLOSE_BLOCKED) => {
            Outcome::Fatal(FatalReason::AccountBlocked)
        }
        _ => Outcome::Retry,
    }
}

/// Not a secret: only distinguishes this run's reconnects from other logins.
fn random_session_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.finish()
}

/// Prefers IPv4 when the name resolves to both families.
async fn resolve(server: &str) -> Option<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(server).await.ok()?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied()
}

async fn read_msg(recv: &mut RecvStream, dec: &mut FrameDecoder) -> Option<ServerMsg> {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(m) = dec.next::<ServerMsg>().ok()? {
            return Some(m);
        }
        let n = recv.read(&mut buf).await.ok()??;
        dec.push(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session task with no connection, plus the caller's ends of its channels.
    fn task() -> (Task, SessionHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let trust = KnownDevices::load(&dir.path().join("known.toml")).unwrap();
        let params = SessionParams {
            server: "127.0.0.1:1".into(),
            fingerprint: [0; 32],
            username: "ja".into(),
            credentials: Credentials::Password(String::new()),
            identity: Arc::new(Identity::generate()),
            trust: Arc::new(Mutex::new(trust)),
        };
        let (events_tx, events) = mpsc::unbounded_channel();
        let (audio_out, out_rx) = mpsc::channel(1);
        let (in_tx, audio_in) = mpsc::channel(1);
        let (commands, cmd_rx) = mpsc::unbounded_channel();
        let (stop, stop_rx) = watch::channel(false);
        let task = Task {
            params,
            session: 1,
            events: events_tx,
            out_rx,
            in_tx,
            cmd_rx,
            stop: stop_rx,
            next_epoch: 0,
            chat_replay: ChatReplay::default(),
            seq: WireSeq::default(),
            room_choice: RoomChoice::Room(7),
        };
        (task, SessionHandle { events, audio_out, audio_in, commands, stop }, dir)
    }

    fn member(id: MemberId, room: Option<RoomId>) -> MemberInfo {
        MemberInfo { id, name: format!("m{id}"), room }
    }

    #[test]
    fn reconnect_asks_for_the_room_the_server_gave() {
        let (mut t, _h, _dir) = task();
        // Asked for room 7 (full), landed in the default room 1.
        t.welcomed(1, Some(1), Vec::new(), vec![member(1, Some(1))]);
        assert_eq!(t.room_choice, RoomChoice::Room(1));
        t.welcomed(1, None, Vec::new(), vec![member(1, None)]);
        assert_eq!(t.room_choice, RoomChoice::Nowhere);
    }

    #[test]
    fn chat_typed_while_connecting_is_reported_not_sent() {
        let (mut t, mut h, _dir) = task();
        h.commands.send(SessionCommand::SendChat("hej".into())).unwrap();
        h.commands.send(SessionCommand::TrustChanged(3)).unwrap();
        h.commands.send(SessionCommand::JoinRoom(2)).unwrap();
        let (_, _, queued) = t.welcomed(1, Some(1), Vec::new(), vec![member(1, Some(1))]);
        assert_eq!(queued, vec![SessionCommand::JoinRoom(2)]);
        assert_eq!(h.events.try_recv(), Ok(SessionEvent::ChatNotSent(ChatSendError::NotConnected)));
        assert!(matches!(h.events.try_recv(), Ok(SessionEvent::Connected { .. })));
    }

    #[test]
    fn credentials_debug_hides_secrets() {
        let all = [
            Credentials::Password("Tajne-Hasło".into()),
            Credentials::Token([0xAB; 32]),
            Credentials::Register { invite: [0xCD; 16], password: "Tajne-Hasło".into() },
        ];
        for c in all {
            let text = format!("{c:?} {c:#?}");
            assert!(!text.contains("Tajne") && !text.contains("171") && !text.contains("205"), "{text}");
        }
    }

    #[test]
    fn wire_seq_keeps_gaps_and_never_goes_back() {
        let mut s = WireSeq::default();
        let got: Vec<u32> = [1, 2, 5, 6, 1, 2, 9, 3].iter().map(|&c| s.map(c)).collect();
        // Gaps survive; a capture restart (back to 1) continues past the last value sent.
        assert_eq!(got, [1, 2, 5, 6, 7, 8, 15, 16]);
    }
}
