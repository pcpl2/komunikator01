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
        Cmd::Run { config } => run(server::ServerConfig::load(&config)?),
        Cmd::HashPassword => {
            let mut pw = String::new();
            std::io::stdin().read_to_string(&mut pw)?;
            println!("{}", server::auth::hash_password(pw.trim_end_matches(['\r', '\n'])));
            Ok(())
        }
    }
}

fn run(config: server::ServerConfig) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let srv = server::start(config).await?;
        println!("listening: {}", srv.local_addr());
        println!("fingerprint: {}", protocol::fingerprint_hex(&srv.fingerprint()));
        tokio::signal::ctrl_c().await?;
        srv.shutdown().await;
        Ok(())
    })
}
