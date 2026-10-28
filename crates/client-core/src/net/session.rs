//! A server session that reconnects on its own until told to stop or a fatal error.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use protocol::audio::{encode_upstream, parse_downstream};
use protocol::{
    encode_frame, ClientMsg, FrameDecoder, MemberId, MemberInfo, RejectReason, ServerMsg,
    PROTOCOL_VERSION,
};
use quinn::{Connection, ConnectionError, Endpoint, RecvStream, VarInt};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use super::{client_config, Backoff, SERVER_NAME};
use crate::audio::frame::{EncodedFrame, IncomingFrame};

/// Close code the server uses when this account logged in elsewhere.
const CLOSE_REPLACED: u32 = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
const AUDIO_OUT_CAPACITY: usize = 64;
const AUDIO_IN_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub struct SessionParams {
    /// "host:port"
    pub server: String,
    pub fingerprint: [u8; 32],
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Connecting,
    Connected { you: MemberId, members: Vec<MemberInfo> },
    MemberJoined(MemberInfo),
    MemberLeft(MemberId),
    Disconnected { retry_in: Duration },
    Fatal(FatalReason),
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
    let (stop, stop_rx) = watch::channel(false);
    let session = random_session_id();
    let task = Task { params, session, events: events_tx, out_rx, in_tx, stop: stop_rx };
    rt.spawn(task.run());
    SessionHandle { events, audio_out, audio_in, stop }
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
    stop: watch::Receiver<bool>,
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
        let Ok(Some((_send, mut recv, mut dec, reply))) = result else {
            conn.close(VarInt::from_u32(0), b"");
            return Outcome::Retry;
        };
        match reply {
            Some(ServerMsg::Welcome { you, members }) => {
                backoff.reset();
                self.emit(SessionEvent::Connected { you, members });
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
        }
        let outcome = self.connected_loop(&conn, &mut recv, &mut dec).await;
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
        conn: &Connection,
        recv: &mut RecvStream,
        dec: &mut FrameDecoder,
    ) -> Outcome {
        let events = self.events.clone();
        let ctrl = async move {
            loop {
                match read_msg(recv, dec).await {
                    Some(ServerMsg::MemberJoined(m)) => {
                        let _ = events.send(SessionEvent::MemberJoined(m));
                    }
                    Some(ServerMsg::MemberLeft { id }) => {
                        let _ = events.send(SessionEvent::MemberLeft(id));
                    }
                    Some(ServerMsg::SessionReplaced) => {
                        return Outcome::Fatal(FatalReason::SessionReplaced)
                    }
                    Some(_) => {}
                    None => return closed_outcome(conn),
                }
            }
        };
        tokio::pin!(ctrl);
        let mut up = Vec::with_capacity(64);
        loop {
            tokio::select! {
                o = &mut ctrl => return o,
                _ = self.stop.changed() => return Outcome::Stop,
                d = conn.read_datagram() => match d {
                    Ok(d) => {
                        if let Ok(p) = parse_downstream(&d) {
                            if let Some(f) = IncomingFrame::new(p.sender, p.seq, p.payload) {
                                let _ = self.in_tx.try_send(f);
                            }
                        }
                    }
                    Err(_) => return closed_outcome(conn),
                },
                f = self.out_rx.recv() => match f {
                    Some(f) => {
                        encode_upstream(f.seq, f.payload(), &mut up);
                        let _ = conn.send_datagram(Bytes::copy_from_slice(&up));
                    }
                    None => return Outcome::Stop,
                },
            }
        }
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
