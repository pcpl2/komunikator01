//! End-to-end encryption: device identity and signed room announcements.

pub mod announce;
pub mod audio;
pub mod identity;
pub mod trust;

pub use announce::{sign_announce, verify_announce, Announce, AnnounceError, ANNOUNCE_MAX_SKEW_MS};
pub use identity::{Identity, IdentityPublic};
pub use trust::{safety_code, KnownDevices, Trust};
pub use audio::{AudioOpener, AudioSealer, SenderKey, TAG_LEN};
