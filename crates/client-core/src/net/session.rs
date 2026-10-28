//! A server session that reconnects on its own until told to stop or a fatal error.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use protocol::audio::{encode_upstream, parse_downstream, MAX_AUDIO_PAYLOAD, MAX_DATAGRAM};
use protocol::{
    encode_frame, ClientMsg, FrameDecoder, MemberId, MemberInfo, RejectReason, ServerMsg,
    PROTOCOL_VERSION,
};
use quinn::{Connection, ConnectionError, Endpoint, RecvStream, SendStream, VarInt};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use super::{client_config, Backoff, SERVER_NAME};
use crate::audio::frame::{EncodedFrame, IncomingFrame, MAX_PACKET};
use crate::e2e::{E2e, E2eEvent, Identity, KnownDevices, Output, TAG_LEN};

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
    pub password: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Connecting,
    Connected { you: MemberId, members: Vec<MemberInfo> },
    MemberJoined(MemberInfo),
    MemberLeft(MemberId),
    Disconnected { retry_in: Duration },
    Fatal(FatalReason),
    E2e(E2eEvent),
}

/// Requests to the running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommand {
    /// The trust store entry for this member's device was changed (accepted or verified).
    TrustChanged(MemberId),
}

/// Errors after which the session stops for good instead of reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatalReason {
    BadCredentials,
    VersionMismatch { server: u16 },
    SessionReplaced,
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
        seq: WireSeq::default(),
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
    /// First sender-key epoch for the next connection; carried across reconnects.
    next_epoch: u16,
    seq: WireSeq,
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

    /// Sleeps while discarding stale outgoing audio; true when asked to stop.
    async fn wait(&mut self, delay: Duration) -> bool {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return false,
                _ = self.stop.changed() => return true,
                f = self.out_rx.recv() => if f.is_none() { return true },
            }
        }
    }

    async fn connect_once(&mut self, backoff: &mut Backoff) -> Outcome {
        let Some(conn) = self.connect().await else { return Outcome::Retry };
        let hello = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            username: self.params.username.clone(),
            password: self.params.password.clone(),
            session: self.session,
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
        let (e2e, first) = match reply {
            Some(ServerMsg::Welcome { you, members }) => {
                backoff.reset();
                // Audio captured while connecting is stale; sending it would play as a burst.
                while self.out_rx.try_recv().is_ok() {}
                // Member ids from an old connection mean nothing now, and the new E2e reads
                // the trust store fresh anyway.
                while self.cmd_rx.try_recv().is_ok() {}
                let (e2e, first) = E2e::new(
                    self.params.identity.clone(),
                    self.params.trust.clone(),
                    you,
                    self.params.username.clone(),
                    &members,
                    self.next_epoch,
                    Instant::now(),
                    now_ms(),
                );
                self.emit(SessionEvent::Connected { you, members });
                (e2e, first)
            }
            Some(ServerMsg::Rejected { reason }) => {
                return match reason {
                    RejectReason::BadCredentials => Outcome::Fatal(FatalReason::BadCredentials),
                    RejectReason::VersionMismatch { server } => {
                        Outcome::Fatal(FatalReason::VersionMismatch { server })
                    }
                    RejectReason::RateLimited => Outcome::Retry,
                }
            }
            Some(ServerMsg::SessionReplaced) => return Outcome::Fatal(FatalReason::SessionReplaced),
            _ => return closed_outcome(&conn),
        };
        let mut link = Link::new(&conn, &mut send, e2e);
        let outcome = match link.output(first, &self.events) {
            Ok(()) => self.connected_loop(&mut link, &mut recv, &mut dec).await,
            Err(o) => o,
        };
        // Keys of the next connection must not reuse an epoch of this one.
        self.next_epoch = link.e2e.next_epoch();
        conn.close(VarInt::from_u32(0), b"");
        outcome
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
            let out = tokio::select! {
                m = read_msg(recv, dec) => {
                    let now = Instant::now();
                    match m {
                        Some(ServerMsg::MemberJoined(m)) => {
                            let out = link.e2e.member_joined(&m, now);
                            self.emit(SessionEvent::MemberJoined(m));
                            out
                        }
                        Some(ServerMsg::MemberLeft { id }) => {
                            self.emit(SessionEvent::MemberLeft(id));
                            link.e2e.member_left(id, now)
                        }
                        Some(ServerMsg::Announce { from, blob }) => {
                            link.e2e.announce(from, &blob, now, now_ms())
                        }
                        Some(ServerMsg::Relay { from, blob }) => link.e2e.relay(from, &blob, now),
                        Some(ServerMsg::SessionReplaced) => {
                            return Outcome::Fatal(FatalReason::SessionReplaced)
                        }
                        Some(_) => continue,
                        None => return closed_outcome(link.conn),
                    }
                }
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
                _ = tick.tick() => link.e2e.tick(Instant::now()),
                Some(c) = self.cmd_rx.recv() => match c {
                    SessionCommand::TrustChanged(id) => link.e2e.trust_changed(id),
                },
                _ = self.stop.changed() => return Outcome::Stop,
                d = link.conn.read_datagram() => match d {
                    Ok(d) => {
                        if let Ok(p) = parse_downstream(&d) {
                            let now = Instant::now();
                            let frame = link
                                .e2e
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
                        match link.e2e.seal(seq, f.payload(), &mut sealed) {
                            Some(epoch) => {
                                encode_upstream(epoch, seq, &sealed, &mut up);
                                let _ = link.conn.send_datagram(Bytes::copy_from_slice(&up));
                            }
                            None => {
                                unsealed += 1;
                                tracing::debug!(unsealed, "dropped audio frame that could not be sealed");
                            }
                        }
                        continue;
                    }
                    None => return Outcome::Stop,
                },
            };
            if let Err(o) = link.output(out, &self.events) {
                return o;
            }
        }
    }
}

/// One live connection with its E2E state.
struct Link<'a> {
    conn: &'a Connection,
    /// Control stream to the server.
    send: &'a mut SendStream,
    e2e: E2e,
    /// Encoded control messages not yet written; `ctrl_out[ctrl_sent..]` is pending. Written
    /// from the main loop so a server that stops reading cannot block audio or shutdown.
    ctrl_out: Vec<u8>,
    ctrl_sent: usize,
    /// Pending control bytes must make progress by then, or the connection is dropped.
    ctrl_deadline: Instant,
}

impl<'a> Link<'a> {
    fn new(conn: &'a Connection, send: &'a mut SendStream, e2e: E2e) -> Self {
        Self { conn, send, e2e, ctrl_out: Vec::new(), ctrl_sent: 0, ctrl_deadline: Instant::now() }
    }

    fn ctrl_pending(&self) -> bool {
        self.ctrl_sent < self.ctrl_out.len()
    }

    /// Reports the state machine's events and queues its messages for the control stream.
    /// Fails when the queue grows past `CTRL_QUEUE_MAX` (the server is not reading).
    fn output(&mut self, out: Output, events: &mpsc::UnboundedSender<SessionEvent>) -> Result<(), Outcome> {
        for e in out.events {
            let _ = events.send(SessionEvent::E2e(e));
        }
        if out.send.is_empty() {
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
        for m in &out.send {
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
        Some(ConnectionError::ApplicationClosed(c))
            if c.error_code == VarInt::from_u32(CLOSE_REPLACED) =>
        {
            Outcome::Fatal(FatalReason::SessionReplaced)
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
    use super::WireSeq;

    #[test]
    fn wire_seq_keeps_gaps_and_never_goes_back() {
        let mut s = WireSeq::default();
        let got: Vec<u32> = [1, 2, 5, 6, 1, 2, 9, 3].iter().map(|&c| s.map(c)).collect();
        // Gaps survive; a capture restart (back to 1) continues past the last value sent.
        assert_eq!(got, [1, 2, 5, 6, 7, 8, 15, 16]);
    }
}
