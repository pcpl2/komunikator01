//! Messages exchanged on the reliable control stream.

use serde::{Deserialize, Serialize};

pub type MemberId = u16;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub id: MemberId,
    pub name: String,
    /// `None` while the member is online but in no room.
    pub room: Option<RoomId>,
}

// Variant order of `ClientMsg`, `ServerMsg` and `RejectReason` is wire format (postcard
// encodes the index): append new variants only, so old and new peers still agree on
// `Hello` and `Rejected` and can negotiate the version.

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    Hello {
        version: u16,
        username: String,
        password: String,
        /// Random per client run, kept across reconnects. Lets the server tell a client
        /// coming back from a network drop from one that was already replaced elsewhere.
        session: u64,
        /// Where to land after login.
        room: RoomChoice,
    },
    /// Opaque key-exchange blob for every other member of my room.
    Announce { blob: Vec<u8> },
    /// Opaque blob for one member of my room.
    Relay { to: MemberId, blob: Vec<u8> },
    JoinRoom { room: RoomId },
    LeaveRoom,
    CreateRoom { name: String },
    DeleteRoom { room: RoomId },
    /// Sealed chat message for my room; the server only forwards it.
    Chat { blob: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    Welcome {
        you: MemberId,
        room: Option<RoomId>,
        rooms: Vec<RoomInfo>,
        /// Everyone online, me included.
        members: Vec<MemberInfo>,
    },
    /// Came online.
    MemberJoined(MemberInfo),
    /// Went offline.
    MemberLeft {
        id: MemberId,
    },
    /// Index 3 in every version, so any client can read a `VersionMismatch`.
    Rejected {
        reason: RejectReason,
    },
    SessionReplaced,
    Announce { from: MemberId, blob: Vec<u8> },
    Relay { from: MemberId, blob: Vec<u8> },
    MemberMoved {
        id: MemberId,
        room: Option<RoomId>,
    },
    RoomCreated(RoomInfo),
    RoomDeleted {
        room: RoomId,
    },
    RoomError {
        reason: RoomErrorReason,
    },
    Chat { from: MemberId, blob: Vec<u8> },
}

/// The `version` of a frame body that starts like a `ClientMsg::Hello` of any protocol
/// version (variant 0, then the u16 version), even if the rest no longer decodes.
pub fn hello_version(body: &[u8]) -> Option<u16> {
    match postcard::take_from_bytes::<(u32, u16)>(body) {
        Ok(((0, version), _)) => Some(version),
        _ => None,
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    BadCredentials,
    VersionMismatch { server: u16 },
    RateLimited,
}

pub type RoomId = u32;

pub const MAX_ROOM_NAME_CHARS: usize = 32;
pub const MAX_USERNAME_CHARS: usize = 32;
pub const MAX_ROOMS: usize = 50;
pub const MAX_ROOM_MEMBERS: usize = 20;
/// Largest sealed chat blob the server will forward.
pub const MAX_CHAT_BLOB: usize = 8192;
pub const DEFAULT_ROOM_NAME: &str = "Ogólny";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RoomInfo {
    pub id: RoomId,
    pub name: String,
    /// Empty for the built-in room.
    pub creator: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum RoomErrorReason {
    InvalidName,
    NameTaken,
    TooManyRooms,
    NotFound,
    NotAllowed,
    NotEmpty,
    Full,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomChoice {
    Default,
    Room(RoomId),
    Nowhere,
}

/// Bidi controls can visually reorder a name (spoofing another room's).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Trims the name; `None` if it is empty, too long, or contains control or bidi characters.
pub fn normalize_room_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_ROOM_NAME_CHARS
        || name.chars().any(|c| c.is_control() || is_bidi_control(c))
    {
        return None;
    }
    Some(name.to_string())
}

/// Trims the name; `None` unless it is 1..=32 chars of letters (any script), digits, `_`, `-`, `.`.
/// Whitespace, control and bidi characters are never allowed.
pub fn normalize_username(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_USERNAME_CHARS
        || !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return None;
    }
    Some(name.to_string())
}
