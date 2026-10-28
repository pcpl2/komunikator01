//! End-to-end encryption: device identity and signed room announcements.

pub mod announce;
pub mod identity;

pub use announce::{sign_announce, verify_announce, Announce, AnnounceError, ANNOUNCE_MAX_SKEW_MS};
pub use identity::{Identity, IdentityPublic};
