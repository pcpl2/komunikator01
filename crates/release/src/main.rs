use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "komunikator-release", about = "Podpisywanie wydań Komunikatora (offline)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generuje nowy klucz podpisujący (nie nadpisuje istniejącego pliku)
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Wypisuje klucz publiczny odpowiadający plikowi z kluczem tajnym
    Public {
        #[arg(long)]
        key: PathBuf,
    },
    /// Podpisuje exe i zapisuje wydanie (exe + manifest.bin) w katalogu wyjściowym
    Sign {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        exe: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Keygen { out } => {
            release::keygen(&out)?;
            let public = release::public_hex(&out)?;
            println!("Klucz publiczny: {public}");
            println!(
                "Plik {} trzymaj offline i nigdy nie wgrywaj go na serwer. \
                 Wpisz klucz publiczny do crates/client-win/release-key.pub przed budowaniem klientów.",
                out.display()
            );
        }
        Cmd::Public { key } => println!("{}", release::public_hex(&key)?),
        Cmd::Sign { key, exe, version, out } => {
            let m = release::sign(&key, &exe, &version, &out)?;
            println!("Podpisano wersję {} ({} bajtów).", m.version, m.size);
            println!(
                "Wgraj {} i manifest.bin z katalogu {} do data_dir/updates/ na serwerze.",
                m.file,
                out.display()
            );
        }
    }
    Ok(())
}
