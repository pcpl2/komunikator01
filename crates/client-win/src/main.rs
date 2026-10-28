//! Headless voice client: komunikator --server host:port --fingerprint HEX --user NAME

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use std::collections::BTreeMap;

use clap::Parser;
use client_core::e2e::{AnnounceError, E2eEvent, IdentityPublic, KnownDevices, Trust};
use protocol::MemberId;
use client_core::net::session::{spawn_session, SessionCommand, SessionEvent, SessionParams};

#[derive(Parser)]
#[command(name = "komunikator", version, about = "Lekki komunikator głosowy")]
struct Cli {
    /// Server address, host:port
    #[arg(long)]
    server: Option<String>,
    /// Server certificate fingerprint (64 hex chars, printed by the server at start)
    #[arg(long)]
    fingerprint: Option<String>,
    #[arg(long)]
    user: Option<String>,
    /// Capture device id (see --list-devices); default communications microphone otherwise
    #[arg(long)]
    capture_device: Option<String>,
    /// Render device id (see --list-devices); default communications output otherwise
    #[arg(long)]
    render_device: Option<String>,
    /// Directory for identity.bin and known_devices.toml (default: %APPDATA%\komunikator)
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// Print audio devices and exit
    #[arg(long)]
    list_devices: bool,
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use client_core::audio::codec::Encoder;
    use client_core::audio::vad::Vad;
    use client_win::identity_store::IdentityStatus;
    use client_win::pipeline::{capture_loop, render_loop, Controls};
    use client_win::supervisor::Supervised;
    use client_win::wasapi::{self, enter_pro_audio, WasapiCapture, WasapiRender};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    if cli.list_devices {
        for (id, name) in wasapi::list_capture_devices() {
            println!("mikrofon : {name}\n           {id}");
        }
        for (id, name) in wasapi::list_render_devices() {
            println!("głośniki : {name}\n           {id}");
        }
        return Ok(());
    }
    let (Some(server), Some(fp), Some(user)) = (cli.server, cli.fingerprint, cli.user) else {
        anyhow::bail!("wymagane: --server, --fingerprint, --user");
    };
    let fingerprint = protocol::parse_fingerprint(&fp)
        .ok_or_else(|| anyhow::anyhow!("--fingerprint musi mieć 64 znaki hex"))?;
    let password = std::env::var("KOMUNIKATOR_PASSWORD")
        .map_err(|_| anyhow::anyhow!("ustaw hasło w zmiennej KOMUNIKATOR_PASSWORD"))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let data_dir = cli.data_dir.unwrap_or_else(|| {
        std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("komunikator")
    });
    let (identity, status) = client_win::identity_store::load_or_create_identity(&data_dir)?;
    match status {
        IdentityStatus::Loaded => {}
        IdentityStatus::Created => println!("utworzono nową tożsamość urządzenia (klucz szyfrowania)"),
        IdentityStatus::Replaced { broken } => println!(
            "UWAGA: nie udało się odczytać poprzedniej tożsamości urządzenia (zachowana jako {}).\n\
             Utworzono nową. Znajomi zobaczą ostrzeżenie o zmianie klucza i muszą je zaakceptować.",
            broken.display()
        ),
    }
    let identity = Arc::new(identity);
    let trust_path = data_dir.join("known_devices.toml");
    let trust = Arc::new(Mutex::new(KnownDevices::load(&trust_path)?));
    let mut session = spawn_session(
        rt.handle(),
        SessionParams { server, fingerprint, username: user, password, identity, trust: trust.clone() },
    );
    // The session stops when the handle's stop sender drops, so only the channels move out.
    let audio_in = std::mem::replace(&mut session.audio_in, tokio::sync::mpsc::channel(1).1);
    let audio_out = session.audio_out.clone();

    let controls = Arc::new(Controls::default());
    let stop = Arc::new(AtomicBool::new(false));

    let capture = {
        let (controls, stop, device) = (controls.clone(), stop.clone(), cli.capture_device);
        std::thread::Builder::new().name("capture".into()).spawn(move || {
            let _mmcss = enter_pro_audio();
            let src = Supervised::new(move || WasapiCapture::open(device.as_deref()), std::time::Instant::now);
            match Encoder::new() {
                Ok(enc) => capture_loop(src, Vad::new(0.5), enc, audio_out, controls, stop),
                Err(e) => eprintln!("opus: {e}"),
            }
        })?
    };
    let render = {
        let (controls, stop, device) = (controls.clone(), stop.clone(), cli.render_device);
        std::thread::Builder::new().name("render".into()).spawn(move || {
            let _mmcss = enter_pro_audio();
            let sink = Supervised::new(move || WasapiRender::open(device.as_deref()), std::time::Instant::now);
            render_loop(sink, audio_in, controls, stop);
        })?
    };

    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if cmd_tx.send(line.trim().to_lowercase()).is_err() {
                break;
            }
        }
    });

    println!("komendy: m = mikrofon wł./wył., d = wycisz wszystkich, k = lista osób i kody, z <id> = zweryfikowany, a <id> = akceptuj zmieniony klucz, q = wyjście");
    let mut roster = Roster::default();
    let exit_code = rt.block_on(async {
        loop {
            tokio::select! {
                e = session.events.recv() => match e {
                    Some(SessionEvent::Fatal(reason)) => {
                        println!("błąd: {reason:?}");
                        return 1;
                    }
                    Some(e) => {
                        roster.observe(&e);
                        print_event(&e);
                        if matches!(e, SessionEvent::E2e(_)) {
                            warn_if_unsaved(&trust);
                        }
                    }
                    None => return 1,
                },
                c = cmd_rx.recv() => match c.as_deref() {
                    Some("m") => toggle("mikrofon wyciszony", &controls.muted),
                    Some("d") => toggle("dźwięk wyciszony", &controls.deafened),
                    Some("k") => roster.print(),
                    Some("q") | None => return 0,
                    Some(line) => {
                        if let Some((verb, arg)) = line.split_once(' ') {
                            if matches!(verb, "z" | "a") {
                                roster.trust_command(verb == "z", arg.trim(), &trust, &session.commands);
                            }
                        }
                    }
                },
            }
        }
    });

    stop.store(true, Ordering::Relaxed);
    session.shutdown();
    let _ = capture.join();
    let _ = render.join();
    std::process::exit(exit_code);
}

fn toggle(label: &str, flag: &AtomicBool) {
    let now = !flag.fetch_xor(true, Ordering::Relaxed);
    println!("{label}: {}", if now { "TAK" } else { "nie" });
}

fn print_event(e: &SessionEvent) {
    match e {
        SessionEvent::Connecting => println!("łączenie…"),
        SessionEvent::Connected { you, members } => {
            let names: Vec<_> = members.iter().map(|m| m.name.as_str()).collect();
            println!("połączono (id {you}); w pokoju: {}", if names.is_empty() { "nikt".into() } else { names.join(", ") });
        }
        SessionEvent::MemberJoined(m) => println!("+ {} dołącza", m.name),
        SessionEvent::MemberLeft(id) => println!("- uczestnik {id} wychodzi"),
        SessionEvent::Disconnected { retry_in } => {
            println!("rozłączono, ponowna próba za {:.1} s", retry_in.as_secs_f32())
        }
        SessionEvent::Fatal(r) => println!("błąd: {r:?}"),
        SessionEvent::E2e(E2eEvent::Member { id, name, trust, code, .. }) => match trust {
            Trust::NewUser => println!("e2e: nowa osoba: {name} (id {id}); kod bezpieczeństwa: {code}"),
            Trust::NewDevice => println!(
                "e2e: UWAGA: {name} (id {id}) używa nowego urządzenia; kod bezpieczeństwa: {code}"
            ),
            Trust::KeyChanged => println!(
                "e2e: OSTRZEŻENIE: zmieniony klucz u {name} (id {id})! Może to być podszywanie się. \
                 Porównaj kod {code} głosowo; jeśli to na pewno ta osoba: a {id}. Do tego czasu nie dostaje Twojego dźwięku."
            ),
            Trust::Known => println!("e2e: {name} (id {id}) — znane urządzenie; kod: {code}"),
            Trust::Verified => println!("e2e: {name} (id {id}) — zweryfikowany; kod: {code}"),
        },
        SessionEvent::E2e(E2eEvent::Rejected { id, reason }) => {
            println!("e2e: odrzucono ogłoszenie klucza od uczestnika {id}: {}", describe_reject(reason))
        }
    }
}

fn describe_reject(r: &AnnounceError) -> &'static str {
    match r {
        AnnounceError::Malformed => "uszkodzone",
        AnnounceError::BadSignature => "zły podpis",
        AnnounceError::NameMismatch => "nazwa nie zgadza się z serwerem",
        AnnounceError::WrongRoom => "inny pokój",
        AnnounceError::Stale => "nieaktualne (sprawdź zegar komputera)",
        AnnounceError::OwnIdentity => "ta sama tożsamość co Twoja",
    }
}

fn warn_if_unsaved(trust: &Mutex<KnownDevices>) {
    if trust.lock().map(|t| t.save_failed()).unwrap_or(false) {
        println!(
            "UWAGA: nie udało się zapisać listy zaufanych urządzeń (known_devices.toml). \
             Zmiany obowiązują do zamknięcia programu, potem znajomi znów wyglądają jak nowi."
        );
    }
}

#[derive(Default)]
struct Roster {
    names: BTreeMap<MemberId, String>,
    e2e: BTreeMap<MemberId, (String, IdentityPublic, Trust, String)>,
}

impl Roster {
    fn observe(&mut self, e: &SessionEvent) {
        match e {
            SessionEvent::Connected { members, .. } => {
                self.names = members.iter().map(|m| (m.id, m.name.clone())).collect();
                self.e2e.clear();
            }
            SessionEvent::MemberJoined(m) => {
                self.names.insert(m.id, m.name.clone());
            }
            SessionEvent::MemberLeft(id) => {
                self.names.remove(id);
                self.e2e.remove(id);
            }
            SessionEvent::E2e(E2eEvent::Member { id, name, identity, trust, code }) => {
                self.e2e.insert(*id, (name.clone(), *identity, *trust, code.clone()));
            }
            SessionEvent::E2e(E2eEvent::Rejected { id, .. }) => {
                self.e2e.remove(id);
            }
            _ => {}
        }
    }

    fn print(&self) {
        if self.names.is_empty() {
            println!("w pokoju nikogo nie ma");
        }
        for (id, name) in &self.names {
            match self.e2e.get(id) {
                Some((_, _, trust, code)) => {
                    println!("{id:>4}  {name}  {}  kod: {code}", trust_label(*trust))
                }
                None => println!("{id:>4}  {name}  brak ważnego klucza (bez szyfrowania do czasu ogłoszenia)"),
            }
        }
    }

    fn trust_command(
        &self,
        verify: bool,
        arg: &str,
        trust: &Mutex<KnownDevices>,
        commands: &tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    ) {
        let Ok(id) = arg.parse::<MemberId>() else {
            println!("podaj numer uczestnika z listy (k), np. {} 3", if verify { "z" } else { "a" });
            return;
        };
        let Some((name, identity, trust_state, _)) = self.e2e.get(&id) else {
            println!("uczestnik {id} nie ma jeszcze klucza; zobacz listę: k");
            return;
        };
        {
            let Ok(mut store) = trust.lock() else { return };
            if verify {
                if *trust_state == Trust::KeyChanged {
                    println!("klucz {name} się zmienił; najpierw porównaj kod i zaakceptuj: a {id}");
                    return;
                }
                store.verify(name, identity);
                println!("{name} oznaczony jako zweryfikowany");
            } else {
                store.accept(name, identity);
                println!("zaakceptowano klucz {name}; to urządzenie jest znane, ale niezweryfikowane");
            }
        }
        warn_if_unsaved(trust);
        let _ = commands.send(SessionCommand::TrustChanged(id));
    }
}

fn trust_label(t: Trust) -> &'static str {
    match t {
        Trust::Verified => "zweryfikowany",
        Trust::Known => "znany",
        Trust::NewUser => "nowa osoba",
        Trust::NewDevice => "NOWE URZĄDZENIE",
        Trust::KeyChanged => "ZMIENIONY KLUCZ (a <id> po sprawdzeniu)",
    }
}

#[cfg(not(windows))]
fn main() {
    let _ = Cli::parse();
    eprintln!("komunikator działa tylko na Windows");
}
