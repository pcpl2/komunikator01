//! Messages exchanged on the reliable control stream.

use serde::{Deserialize, Serialize};

pub type MemberId = u16;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub id: MemberId,
    pub name: String,
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
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    Welcome {
        you: MemberId,
        members: Vec<MemberInfo>,
    },
    MemberJoined(MemberInfo),
    MemberLeft {
        id: MemberId,
    },
    Rejected {
        reason: RejectReason,
    },
    SessionReplaced,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    BadCredentials,
    VersionMismatch { server: u16 },
    RateLimited,
}
