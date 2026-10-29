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
    /// Podpisuje komunikator.exe (program okienkowy) i zapisuje wydanie (exe + manifest.bin)
    /// w katalogu wyjściowym. Wersję i klucz publiczny odczytuje ze znacznika wbudowanego w exe.
    Sign {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        exe: PathBuf,
        /// Opcjonalne sprawdzenie: musi się zgadzać z wersją wbudowaną w exe
        #[arg(long)]
        version: Option<String>,
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
            let m = release::sign(&key, &exe, version.as_deref(), &out)?;
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
