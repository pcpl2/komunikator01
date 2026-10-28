//! Wire format shared by the client and the server.

pub mod audio;
pub mod control;
pub mod framing;

pub use control::{ClientMsg, MemberId, MemberInfo, RejectReason, ServerMsg};
pub use framing::{encode_frame, FrameDecoder, FrameError, MAX_FRAME};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u16 = 1;
pub const ALPN: &[u8] = b"komunikator/1";
pub const KEEP_ALIVE_MS: u64 = 5_000;
pub const IDLE_TIMEOUT_MS: u64 = 15_000;
pub const DEFAULT_PORT: u16 = 7700;
