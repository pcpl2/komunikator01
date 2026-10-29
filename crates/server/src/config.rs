//! Server configuration loaded from TOML.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    /// Imported into the account store at start; new accounts come from invites.
    #[serde(default)]
    pub users: Vec<UserEntry>,
    /// Address players put in invite links, e.g. `serwer.example.pl:7700`.
    #[serde(default)]
    pub public_address: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserEntry {
    pub name: String,
    pub password_hash: String,
    /// May delete rooms created by others.
    #[serde(default)]
    pub admin: bool,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], protocol::DEFAULT_PORT))
}

impl ServerConfig {
    pub fn load(path: &Path) -> anyhow::Result<ServerConfig> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_example() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../server.example.toml");
        let c = ServerConfig::load(Path::new(path)).unwrap();
        assert_eq!(c.bind, "0.0.0.0:7700".parse().unwrap());
        assert_eq!(c.users.len(), 1);
        assert_eq!(c.users[0].name, "ala");
        assert!(c.users[0].admin);
    }

    #[test]
    fn bind_defaults_to_port_7700() {
        let c: ServerConfig = toml::from_str("data_dir = \"/tmp\"").unwrap();
        assert_eq!(c.bind.port(), 7700);
        assert!(c.users.is_empty());
    }

    #[test]
    fn admin_defaults_to_false() {
        let text = "data_dir = \"/tmp\"\n[[users]]\nname = \"ola\"\npassword_hash = \"h\"";
        let c: ServerConfig = toml::from_str(text).unwrap();
        assert!(!c.users[0].admin);
    }
}
