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
    Rejected {
        reason: RejectReason,
    },
    SessionReplaced,
    Announce { from: MemberId, blob: Vec<u8> },
    Relay { from: MemberId, blob: Vec<u8> },
    Chat { from: MemberId, blob: Vec<u8> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    BadCredentials,
    VersionMismatch { server: u16 },
    RateLimited,
}

pub type RoomId = u32;

pub const MAX_ROOM_NAME_CHARS: usize = 32;
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

/// Trims the name; `None` if it is empty, too long, or contains control characters.
pub fn normalize_room_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_ROOM_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return None;
    }
    Some(name.to_string())
}
