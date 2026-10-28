use std::io::Read;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "komunikator-server", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the relay server.
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    /// Read a password from stdin and print its Argon2id hash.
    HashPassword,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Run { config } => {
            let _config = server::ServerConfig::load(&config)?;
            Ok(())
        }
        Cmd::HashPassword => {
            let mut pw = String::new();
            std::io::stdin().read_to_string(&mut pw)?;
            println!("{}", server::auth::hash_password(pw.trim_end_matches(['\r', '\n'])));
            Ok(())
        }
    }
}
