//! Voice relay server: authenticates clients and forwards their audio datagrams.

pub mod auth;
pub mod config;

pub use config::{ServerConfig, UserEntry};
