//! Native Windows client: main window and tray.
#![windows_subsystem = "windows"]

use clap::Parser;

#[derive(Parser)]
#[command(name = "komunikator", version, about = "Lekki komunikator głosowy")]
struct Cli {
    /// Start hidden, in the tray
    #[arg(long)]
    minimized: bool,
    /// Directory for settings.toml, identity.bin and known_devices.toml (default: %APPDATA%\komunikator)
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// Open the window and tray, run for a second, save settings and exit (self-test)
    #[arg(long)]
    smoke: bool,
}

#[cfg(windows)]
fn main() {
    use client_win::ui::app::{run, Options};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let code = run(Options { minimized: cli.minimized, data_dir: cli.data_dir, smoke: cli.smoke });
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    let _ = Cli::parse();
    eprintln!("komunikator działa tylko na Windows");
}
