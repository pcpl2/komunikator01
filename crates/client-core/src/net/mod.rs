//! Connection to the relay server.

pub mod backoff;
pub mod tls;

pub use backoff::Backoff;
pub use tls::{client_config, SERVER_NAME};
