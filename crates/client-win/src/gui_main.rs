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
    /// Started by a self-update (shows the new version)
    #[arg(long, hide = true)]
    updated: bool,
}

/// Lets `komunikator-release sign` recognise this program and read its version and key.
static RELEASE_MARKER: [u8; client_win::updater::MARKER_LEN] = client_win::updater::release_marker();

#[cfg(windows)]
fn main() {
    use client_win::ui::app::{run, Options};

    // Kept in the binary through LTO and strip.
    std::hint::black_box(&RELEASE_MARKER);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let code =
        run(Options { minimized: cli.minimized, data_dir: cli.data_dir, smoke: cli.smoke, updated: cli.updated });
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    std::hint::black_box(&RELEASE_MARKER);
    let _ = Cli::parse();
    eprintln!("komunikator działa tylko na Windows");
}
