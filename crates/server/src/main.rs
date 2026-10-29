use std::io::Read;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use server::admin;

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
    /// Create an invite and print the string to give a friend.
    Invite {
        #[command(flatten)]
        data: Data,
        /// How many days the invite stays valid.
        #[arg(long, default_value_t = 7)]
        days: u64,
        /// Address players connect to (host:port); defaults to `public_address` from the config.
        #[arg(long)]
        server: Option<String>,
    },
    /// List accounts.
    Users(Data),
    /// List invites (never their codes).
    Invites(Data),
    /// Block an account and disconnect it.
    Block(Named),
    /// Unblock an account.
    Unblock(Named),
    /// Make an account an administrator.
    MakeAdmin(Named),
    /// Take administrator rights away.
    RevokeAdmin(Named),
    /// Set a new password read from stdin (at least 8 characters) and end the account's sessions.
    ResetPassword(Named),
    /// List rooms.
    Rooms(Data),
    /// Create a room.
    CreateRoom {
        #[command(flatten)]
        data: Data,
        name: String,
    },
    /// Delete a room, even an occupied one (its members are moved out).
    DeleteRoom {
        #[command(flatten)]
        data: Data,
        id: protocol::RoomId,
    },
}

/// Where the server keeps its data.
#[derive(Args)]
struct Data {
    #[arg(long)]
    config: PathBuf,
}

#[derive(Args)]
struct Named {
    #[command(flatten)]
    data: Data,
    name: String,
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
        Cmd::Invite { data, days, server } => print(admin::invite(&data.load()?, days, server.as_deref())),
        Cmd::Users(d) => print(admin::users(&d.load()?)),
        Cmd::Invites(d) => print(admin::invites(&d.load()?)),
        Cmd::Block(n) => print(admin::block(&n.data.load()?, &n.name)),
        Cmd::Unblock(n) => print(admin::unblock(&n.data.load()?, &n.name)),
        Cmd::MakeAdmin(n) => print(admin::make_admin(&n.data.load()?, &n.name)),
        Cmd::RevokeAdmin(n) => print(admin::revoke_admin(&n.data.load()?, &n.name)),
        Cmd::ResetPassword(n) => {
            let mut pw = String::new();
            std::io::stdin().read_to_string(&mut pw)?;
            let pw = pw.trim_end_matches(['\r', '\n']);
            print(admin::reset_password(&n.data.load()?, &n.name, pw))
        }
        Cmd::Rooms(d) => print(admin::rooms(&d.load()?)),
        Cmd::CreateRoom { data, name } => print(admin::create_room(&data.load()?, &name)),
        Cmd::DeleteRoom { data, id } => print(admin::delete_room(&data.load()?, id)),
    }
}

impl Data {
    fn load(&self) -> anyhow::Result<server::ServerConfig> {
        server::ServerConfig::load(&self.config)
    }
}

fn print(out: anyhow::Result<String>) -> anyhow::Result<()> {
    print!("{}", out?);
    Ok(())
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
        wait_for_stop_signal().await?;
        srv.shutdown().await;
        Ok(())
    })
}

/// Ctrl+C everywhere; on Unix also SIGTERM (systemd stop) so clients get a clean close.
async fn wait_for_stop_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
