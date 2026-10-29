//! Headless voice client. First start with an invite: komunikator-cli --invite KOD --user NAME
//! (password in KOMUNIKATOR_PASSWORD); later starts need no options. An existing account logs
//! in with --server host:port --fingerprint HEX --user NAME (password likewise), which is
//! remembered the same way.

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use std::collections::BTreeMap;

use clap::Parser;
use client_core::app::{describe_chat_not_sent, describe_fatal, describe_room_error, trust_label};
use client_core::audio::volume::{GainTable, VolumeBook};
use client_core::chat::{find_links, sanitize_name};
use client_core::e2e::{AnnounceError, E2eEvent, IdentityPublic, KnownDevices, Trust};
use client_core::lobby::Lobby;
use client_core::net::session::{
    ChatLine, ChatSendError, FatalReason, SessionCommand, SessionEvent,
};
use protocol::invite::{decode_invite, Invite};
use protocol::{MemberId, RoomId};

#[derive(Parser)]
#[command(name = "komunikator-cli", version, about = "Lekki komunikator głosowy")]
struct Cli {
    /// Server address, host:port
    #[arg(long)]
    server: Option<String>,
    /// Server certificate fingerprint (64 hex chars, printed by the server at start)
    #[arg(long)]
    fingerprint: Option<String>,
    #[arg(long)]
    user: Option<String>,
    /// Invite code from the server admin: creates the account (with --user; password from
    /// KOMUNIKATOR_PASSWORD) and remembers it in settings.toml for later starts
    #[arg(long)]
    invite: Option<String>,
    /// Capture device id (see --list-devices); default communications microphone otherwise
    #[arg(long)]
    capture_device: Option<String>,
    /// Render device id (see --list-devices); default communications output otherwise
    #[arg(long)]
    render_device: Option<String>,
    /// Directory for identity.bin, known_devices.toml and settings.toml (default: %APPDATA%\komunikator)
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// Print audio devices and exit
    #[arg(long)]
    list_devices: bool,
    /// Update komunikator.exe next to this program from the server, if a newer signed release
    /// is there, and exit (the server from --server/--fingerprint, --invite or settings.toml).
    /// Close the window program (for the same --data-dir) first; it is refused while it runs
    #[arg(long)]
    check_update: bool,
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use client_win::{startup, wasapi};

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
    if cli.check_update {
        return check_update(&cli);
    }
    use client_core::net::session::Credentials;
    use client_core::settings::SettingsStore;
    use client_win::vault::{Secret, Vault};

    let password = std::env::var("KOMUNIKATOR_PASSWORD").ok();
    let login = choose_login(cli.invite.as_deref(), cli.server, cli.fingerprint, cli.user, password)
        .map_err(anyhow::Error::msg)?;
    let given_data_dir = cli.data_dir.clone();
    let data_dir = cli.data_dir.unwrap_or_else(startup::default_data_dir);
    let settings_path = data_dir.join("settings.toml");
    let vault = Vault::for_data_dir(&data_dir);
    // The settings the account is saved to: the new token (and the account given on the
    // command line) are written with every token issued.
    let (account, mut saved) = match login {
        Login::Given { server, fingerprint, user, password } => {
            let saved = pending_account(&settings_path, &server, &fingerprint, &user);
            let account =
                startup::Account { server, fingerprint, username: user, credentials: Credentials::Password(password) };
            (account, saved)
        }
        Login::Register { invite, user, password } => {
            let saved = pending_account(&settings_path, &invite.server, &invite.fingerprint, &user);
            let account = startup::Account {
                server: invite.server,
                fingerprint: invite.fingerprint,
                username: user,
                credentials: Credentials::Register { invite: invite.code, password },
            };
            (account, saved)
        }
        Login::Saved => {
            let (store, s, broken) = SettingsStore::load(&settings_path);
            if let Some(Ok(p)) = broken {
                println!("UWAGA: plik ustawień był uszkodzony i został odłożony jako {}", p.display());
            }
            let credentials = vault.load().map(|secret| match secret {
                Secret::Token(t) => Credentials::Token(t),
                Secret::Password(p) => Credentials::Password(p),
            });
            let fingerprint = protocol::parse_fingerprint(&s.fingerprint);
            let (true, Some(fingerprint), Some(credentials)) = (s.has_account(), fingerprint, credentials) else {
                anyhow::bail!(
                    "brak zapisanego konta w {}: użyj --invite <kod> --user <nazwa> \
                     albo --server, --fingerprint i --user",
                    settings_path.display()
                );
            };
            let account =
                startup::Account { server: s.server.clone(), fingerprint, username: s.username.clone(), credentials };
            (account, (store, s))
        }
    };
    let (store, saved) = &mut saved;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let (client, notices) = startup::start_client(
        rt.handle(),
        &data_dir,
        account,
        startup::Audio { capture: cli.capture_device, render: cli.render_device, sensitivity: 0.5 },
    )?;
    for n in notices {
        println!("{}", n.text);
    }
    let startup::Client { mut session, engine, trust, controls, gains, .. } = client;

    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if cmd_tx.send(line.trim().to_owned()).is_err() {
                break;
            }
        }
    });

    println!("{HELP}");
    let mut roster = Roster::default();
    let mut lobby: Option<Lobby> = None;
    let mut book = VolumeBook::default();
    let exit_code = rt.block_on(async {
        loop {
            tokio::select! {
                e = session.events.recv() => match e {
                    Some(SessionEvent::Fatal(reason)) => {
                        println!("błąd: {}", describe_fatal(reason));
                        if reason == FatalReason::LoginRequired {
                            println!("{}", relogin_hint(saved, given_data_dir.as_deref()));
                        }
                        return 1;
                    }
                    Some(SessionEvent::TokenIssued(token)) => remember_token(&vault, store, saved, &token.0),
                    Some(SessionEvent::SignedInAs(name)) => {
                        // The server's spelling; peers know me by it.
                        if saved.username != name {
                            saved.username = name;
                            if let Err(e) = store.save(saved) {
                                println!("UWAGA: nie udało się zapisać nazwy w settings.toml: {e}");
                            }
                        }
                    }
                    Some(e) => {
                        // Before the lobby applies the event: a leaver's name is gone after.
                        let lobby_text = lobby_line(lobby.as_ref(), &e);
                        roster.observe(&e);
                        observe_lobby(&mut lobby, &e);
                        let applied = match &e {
                            SessionEvent::MemberLeft(id) => {
                                gains.remove(*id);
                                true
                            }
                            // Every id is new after a reconnect; old slots would never be freed.
                            SessionEvent::Connected { .. } => {
                                gains.clear();
                                apply_book(&roster, &book, &gains)
                            }
                            SessionEvent::MemberJoined(_) | SessionEvent::MemberMoved { .. } => {
                                apply_book(&roster, &book, &gains)
                            }
                            _ => true,
                        };
                        if let Some(text) = lobby_text {
                            println!("{text}");
                        }
                        print_event(&e, local_hm());
                        if !applied {
                            println!("{GAINS_FULL}");
                        }
                        if matches!(e, SessionEvent::E2e(_)) {
                            warn_if_unsaved(&trust);
                        }
                    }
                    None => return 1,
                },
                c = cmd_rx.recv() => {
                    let Some(line) = c else { return 0 };
                    let (verb, arg) = match line.split_once(char::is_whitespace) {
                        Some((v, a)) => (v.to_lowercase(), a.trim()),
                        None => (line.to_lowercase(), ""),
                    };
                    let send = |cmd| {
                        let _ = session.commands.send(cmd);
                    };
                    match verb.as_str() {
                        "q" => return 0,
                        "m" => toggle("mikrofon wyciszony", &controls.muted),
                        "d" => toggle("dźwięk wyciszony", &controls.deafened),
                        "k" => roster.print(),
                        "p" => match &lobby {
                            Some(l) => format_rooms(l).iter().for_each(|l| println!("{l}")),
                            None => println!("brak połączenia z serwerem"),
                        },
                        "w" => match arg.parse::<RoomId>() {
                            Ok(id) => send(SessionCommand::JoinRoom(id)),
                            Err(_) => println!("podaj numer pokoju z listy (p), np. w 2"),
                        },
                        "o" => send(SessionCommand::LeaveRoom),
                        "n" if !arg.is_empty() => send(SessionCommand::CreateRoom(arg.to_owned())),
                        "n" => println!("podaj nazwę pokoju, np. n Ranked"),
                        "u" => match arg.parse::<RoomId>() {
                            Ok(id) => send(SessionCommand::DeleteRoom(id)),
                            Err(_) => println!("podaj numer pokoju z listy (p), np. u 2"),
                        },
                        "t" => send(SessionCommand::SendChat(arg.to_owned())),
                        "g" | "x" => {
                            let mut words = arg.split_whitespace();
                            let person = words.next().and_then(|w| w.parse::<MemberId>().ok());
                            let Some((id, name)) = person.and_then(|id| {
                                let name = roster.members.get(&id)?.0.clone();
                                Some((id, name))
                            }) else {
                                println!("podaj numer osoby z listy (k), np. {verb} 3");
                                continue;
                            };
                            let shown = sanitize_name(&name);
                            if verb == "x" {
                                match toggle_mute(&name, id, &mut book, &gains) {
                                    Ok(muted) => println!(
                                        "{shown}: wyciszony u Ciebie: {}",
                                        if muted { "TAK" } else { "nie" }
                                    ),
                                    Err(msg) => println!("{msg}"),
                                }
                            } else {
                                match set_volume(&name, id, words.next().unwrap_or(""), &mut book, &gains) {
                                    Ok(p) => println!("{shown}: głośność {p}%"),
                                    Err(msg) => println!("{msg}"),
                                }
                            }
                        }
                        "z" | "a" => roster.trust_command(verb == "z", arg, &trust, &session.commands),
                        _ => println!("{HELP}"),
                    }
                }
            }
        }
    });

    session.shutdown();
    engine.stop();
    std::process::exit(exit_code);
}

/// Where the login comes from.
enum Login {
    /// `--invite` and `--user`: create the account and remember it.
    Register { invite: Invite, user: String, password: String },
    /// `--server`, `--fingerprint` and `--user`: log in with the password and remember it.
    Given { server: String, fingerprint: [u8; 32], user: String, password: String },
    /// No options: the account saved in settings.toml.
    Saved,
}

/// Checks which options go together; the error is for the user, in Polish.
fn choose_login(
    invite: Option<&str>,
    server: Option<String>,
    fingerprint: Option<String>,
    user: Option<String>,
    password: Option<String>,
) -> Result<Login, String> {
    const NO_PASSWORD: &str = "ustaw hasło w zmiennej KOMUNIKATOR_PASSWORD";
    if let Some(invite) = invite {
        if server.is_some() || fingerprint.is_some() {
            return Err("zaproszenie zawiera już adres serwera i odcisk: nie podawaj --server ani --fingerprint".into());
        }
        let user = user.ok_or("do --invite podaj też --user <nazwa>")?;
        let invite = decode_invite(invite).map_err(|e| format!("niepoprawny kod zaproszenia: {e}"))?;
        return Ok(Login::Register { invite, user, password: password.ok_or(NO_PASSWORD)? });
    }
    match (server, fingerprint, user) {
        (None, None, None) => Ok(Login::Saved),
        (Some(server), Some(fp), Some(user)) => {
            let fingerprint = protocol::parse_fingerprint(&fp).ok_or("--fingerprint musi mieć 64 znaki hex")?;
            Ok(Login::Given { server, fingerprint, user, password: password.ok_or(NO_PASSWORD)? })
        }
        _ => Err("wymagane: --server, --fingerprint i --user (albo --invite i --user)".into()),
    }
}

/// Keeps the new token in Windows Credential Manager and the account in settings.toml; a
/// failure is only reported (the session goes on, the next start will ask for a password).
#[cfg(windows)]
fn remember_token(
    vault: &client_win::vault::Vault,
    store: &client_core::settings::SettingsStore,
    s: &client_core::settings::Settings,
    token: &[u8; 32],
) {
    let label = client_win::vault::account_label(&s.username, &s.server);
    let saved = vault.store(&client_win::vault::Secret::Token(*token), &label).and_then(|()| Ok(store.save(s)?));
    if let Err(e) = saved {
        println!("UWAGA: nie udało się zapamiętać logowania: {e:#}");
    }
}

/// The settings with a not yet confirmed account filled in; written only with its first token,
/// so a refused login leaves the saved account as it was.
#[cfg(windows)]
fn pending_account(
    path: &std::path::Path,
    server: &str,
    fingerprint: &[u8; 32],
    user: &str,
) -> (client_core::settings::SettingsStore, client_core::settings::Settings) {
    let (store, mut s, _) = client_core::settings::SettingsStore::load(path);
    s.server = server.to_owned();
    s.fingerprint = protocol::fingerprint_hex(fingerprint);
    s.username = user.to_owned();
    (store, s)
}

/// What to run when the remembered session is no longer accepted (spent, expired or revoked).
fn relogin_hint(s: &client_core::settings::Settings, data_dir: Option<&std::path::Path>) -> String {
    let dir = data_dir.map(|d| format!(" --data-dir \"{}\"", d.display())).unwrap_or_default();
    format!(
        "zaloguj się ponownie hasłem (zapamięta nową sesję):\n  \
         komunikator-cli --server {} --fingerprint {} --user {}{dir}\n\
         z hasłem w zmiennej KOMUNIKATOR_PASSWORD",
        s.server, s.fingerprint, s.username
    )
}

const HELP: &str = "komendy:
  p            pokoje i osoby (* = Twój pokój)
  w <nr>       wejdź do pokoju      o            wyjdź z pokoju
  n <nazwa>    załóż pokój          u <nr>       usuń pokój
  t <tekst>    napisz na czacie pokoju
  g <id> <0-200>  głośność osoby w %      x <id>  wycisz/odcisz osobę u siebie
  m            mikrofon wł./wył.    d            wycisz wszystkich
  k            osoby w pokoju, zaufanie i kody
  z <id> <grupa>  zweryfikowany (grupa = pierwsze 4 cyfry kodu, który porównaliście)
  a <id> <grupa>  akceptuj zmieniony klucz (jw.)
  q            wyjście";

/// The render table has no free slot: the setting is kept by name but not in effect.
const GAINS_FULL: &str = "UWAGA: brak miejsca w tabeli głośności — ustawienie zapamiętane, \
                          ale ta osoba gra teraz z głośnością 100% (zadziała po ponownym połączeniu)";

/// Roster line for a member without a verified key: audio is never sent in the clear.
const NO_KEY: &str = "brak klucza — nie słyszycie się do czasu ogłoszenia";

fn toggle(label: &str, flag: &AtomicBool) {
    let now = !flag.fetch_xor(true, Ordering::Relaxed);
    println!("{label}: {}", if now { "TAK" } else { "nie" });
}

fn print_event(e: &SessionEvent, now: (u16, u16)) {
    match e {
        SessionEvent::Connecting => println!("łączenie…"),
        // Handled (and never printed) in the main loop.
        SessionEvent::TokenIssued(_) | SessionEvent::SignedInAs(_) => {}
        SessionEvent::Connected { you, room, members, .. } => {
            let names: Vec<_> = members
                .iter()
                .filter(|m| m.id != *you && room.is_some() && m.room == *room)
                .map(|m| sanitize_name(&m.name))
                .collect();
            println!("połączono (id {you}); w pokoju: {}", if names.is_empty() { "nikt".into() } else { names.join(", ") });
        }
        SessionEvent::MemberJoined(m) => println!("+ {} dołącza", sanitize_name(&m.name)),
        // Printed from `lobby_line`, which needs the lobby as it was before the event.
        SessionEvent::MemberLeft(_) | SessionEvent::MemberMoved { .. } | SessionEvent::RoomDeleted(_) => {}
        SessionEvent::RoomCreated(r) => println!("nowy pokój: {} (id {})", sanitize_name(&r.name), r.id),
        SessionEvent::RoomError(reason) => println!("błąd pokoju: {}", describe_room_error(reason)),
        SessionEvent::Chat(line) => format_chat(line, now).iter().for_each(|l| println!("{l}")),
        SessionEvent::ChatUnreadable { name, .. } => println!(
            "nie udało się odszyfrować wiadomości od {} (klucze jeszcze się wymieniają)",
            sanitize_name(name)
        ),
        SessionEvent::ChatNotSent(reason) => println!("nie wysłano wiadomości: {}", cli_chat_not_sent(*reason)),
        SessionEvent::Disconnected { retry_in } => {
            println!("rozłączono, ponowna próba za {:.1} s", retry_in.as_secs_f32())
        }
        SessionEvent::Fatal(r) => println!("błąd: {}", describe_fatal(*r)),
        SessionEvent::E2e(E2eEvent::Member { id, name, trust, code, .. }) => {
            let name = sanitize_name(name);
            match trust {
                Trust::NewUser => println!("e2e: nowa osoba: {name} (id {id}); kod bezpieczeństwa: {code}"),
                Trust::NewDevice => println!(
                    "e2e: UWAGA: {name} (id {id}) używa nowego, niezweryfikowanego urządzenia; \
                     kod bezpieczeństwa: {code} (po porównaniu: z {id})"
                ),
                Trust::KeyChanged => println!(
                    "e2e: OSTRZEŻENIE: zmieniony klucz u {name} (id {id})! Może to być podszywanie się. \
                     Porównaj kod {code} głosowo; jeśli to na pewno ta osoba: a {id}. Do tego czasu nie dostaje Twojego dźwięku."
                ),
                Trust::Known => println!(
                    "e2e: {name} (id {id}) — znany, niezweryfikowany; porównaj kod {code}, potem: z {id}"
                ),
                Trust::Verified => println!("e2e: {name} (id {id}) — zweryfikowany; kod: {code}"),
            }
        }
        SessionEvent::E2e(E2eEvent::Rejected { id, reason }) => {
            println!("e2e: odrzucono ogłoszenie klucza od uczestnika {id}: {}", describe_reject(reason))
        }
    }
}

/// The line for a leave, move or room deletion, from the lobby as it was *before* the event
/// (afterwards a leaver's name and a deleted room's name are gone). My own moves are called out.
fn lobby_line(lobby: Option<&Lobby>, e: &SessionEvent) -> Option<String> {
    let person = |id: MemberId| {
        lobby.and_then(|l| l.name_of(id)).map_or_else(|| format!("uczestnik {id}"), sanitize_name)
    };
    let room = |id: RoomId| {
        lobby
            .and_then(|l| l.rooms().find(|r| r.id == id))
            .map_or_else(|| id.to_string(), |r| sanitize_name(&r.name))
    };
    let me = |id: MemberId| lobby.is_some_and(|l| l.me() == id);
    Some(match e {
        SessionEvent::MemberLeft(id) => format!("- {} wychodzi", person(*id)),
        SessionEvent::MemberMoved { id, room: Some(r) } if me(*id) => format!("przechodzisz do pokoju {}", room(*r)),
        SessionEvent::MemberMoved { id, room: None } if me(*id) => "opuszczasz pokój".into(),
        SessionEvent::MemberMoved { id, room: Some(r) } => {
            format!("{} przechodzi do pokoju {}", person(*id), room(*r))
        }
        SessionEvent::MemberMoved { id, room: None } => format!("{} wychodzi z pokoju", person(*id)),
        SessionEvent::RoomDeleted(id) => format!("usunięto pokój {}", room(*id)),
        _ => return None,
    })
}

/// The text lines, then one line per link so it can be copied out of the console. Lines after
/// the first are indented, so a message cannot pass itself off as another one.
fn format_chat(line: &ChatLine, (h, m): (u16, u16)) -> Vec<String> {
    let who = if line.own { "ty".to_owned() } else { sanitize_name(&line.name) };
    let mut lines = line.text.split('\n');
    let first = lines.next().unwrap_or_default();
    let mut out = vec![format!("[{h:02}:{m:02}] {who}: {first}")];
    out.extend(lines.map(|l| format!("    {l}")));
    for (i, link) in find_links(&line.text).iter().enumerate() {
        out.push(format!("  link {}: {}", i + 1, &line.text[link.range.clone()]));
    }
    out
}

/// Rooms in id order with their members; my room is marked with `*`.
fn format_rooms(lobby: &Lobby) -> Vec<String> {
    let names = |room| {
        let mut n: Vec<_> = lobby.members_in(room).iter().map(|m| sanitize_name(&m.name)).collect();
        n.sort();
        n.join(", ")
    };
    let mut out = Vec::new();
    for r in lobby.rooms() {
        let mark = if lobby.my_room() == Some(r.id) { '*' } else { ' ' };
        out.push(format!("{mark} {:>2}  {}", r.id, sanitize_name(&r.name)));
        let here = names(Some(r.id));
        if !here.is_empty() {
            out.push(format!("      {here}"));
        }
    }
    let outside = names(None);
    if !outside.is_empty() {
        out.push(format!("poza pokojami: {outside}"));
    }
    out
}

fn observe_lobby(lobby: &mut Option<Lobby>, e: &SessionEvent) {
    match e {
        SessionEvent::Connected { you, room, rooms, members } => {
            *lobby = Some(Lobby::new(*you, *room, rooms.clone(), members.clone()).0);
        }
        SessionEvent::Disconnected { .. } => *lobby = None,
        _ => {
            let Some(l) = lobby else { return };
            match e {
                SessionEvent::MemberJoined(m) => drop(l.member_joined(m.clone())),
                SessionEvent::MemberLeft(id) => drop(l.member_left(*id)),
                SessionEvent::MemberMoved { id, room } => drop(l.member_moved(*id, *room)),
                SessionEvent::RoomCreated(r) => drop(l.room_created(r.clone())),
                SessionEvent::RoomDeleted(id) => drop(l.room_deleted(*id)),
                _ => {}
            }
        }
    }
}

/// Pushes remembered volumes (kept by name) to the render thread's id-keyed table; false if
/// some did not fit.
fn apply_book(roster: &Roster, book: &VolumeBook, gains: &GainTable) -> bool {
    let mut all = true;
    for (id, (name, _)) in &roster.members {
        all &= gains.set(*id, book.gain(name));
    }
    all
}

/// The setting is remembered by name even when the table is full (`GAINS_FULL`).
fn set_volume(name: &str, id: MemberId, arg: &str, book: &mut VolumeBook, gains: &GainTable) -> Result<u16, String> {
    let percent = arg
        .parse::<u16>()
        .ok()
        .filter(|p| *p <= 200)
        .ok_or_else(|| "podaj głośność 0–200 (%), np. g 3 150".to_string())?;
    book.set_percent(name, percent);
    if !gains.set(id, book.gain(name)) {
        return Err(GAINS_FULL.into());
    }
    Ok(percent)
}

/// Returns whether the person is now muted; remembered by name even when the table is full.
fn toggle_mute(name: &str, id: MemberId, book: &mut VolumeBook, gains: &GainTable) -> Result<bool, String> {
    let muted = book.toggle_muted(name);
    if !gains.set(id, book.gain(name)) {
        return Err(GAINS_FULL.into());
    }
    Ok(muted)
}

#[cfg(windows)]
fn local_hm() -> (u16, u16) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wHour, t.wMinute)
}

/// The shared text, plus the CLI command that helps.
fn cli_chat_not_sent(r: ChatSendError) -> String {
    match r {
        ChatSendError::NotInRoom => format!("{} (wejdź do pokoju: w <nr>)", describe_chat_not_sent(r)),
        _ => describe_chat_not_sent(r).to_owned(),
    }
}

/// The shared label, plus the CLI command for a changed key.
fn cli_trust_label(t: Trust) -> String {
    match t {
        Trust::KeyChanged => format!("{} (a <id> <grupa> po sprawdzeniu)", trust_label(t)),
        _ => trust_label(t).to_owned(),
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
    you: MemberId,
    room: Option<RoomId>,
    /// Everyone online except me, with their room.
    members: BTreeMap<MemberId, (String, Option<RoomId>)>,
    e2e: BTreeMap<MemberId, (String, IdentityPublic, Trust, String)>,
}

impl Roster {
    fn observe(&mut self, e: &SessionEvent) {
        match e {
            SessionEvent::Connected { you, room, members, .. } => {
                self.you = *you;
                self.room = *room;
                self.members =
                    members.iter().filter(|m| m.id != *you).map(|m| (m.id, (m.name.clone(), m.room))).collect();
                self.e2e.clear();
            }
            // Like the lobby, a repeat for a known id keeps the first name.
            SessionEvent::MemberJoined(m) if m.id != self.you => {
                self.members.entry(m.id).or_insert_with(|| (m.name.clone(), m.room)).1 = m.room;
            }
            SessionEvent::MemberLeft(id) => {
                self.members.remove(id);
                self.e2e.remove(id);
            }
            // A new room means new keys for everyone.
            SessionEvent::MemberMoved { id, room } if *id == self.you => {
                self.room = *room;
                self.e2e.clear();
            }
            SessionEvent::MemberMoved { id, room } => {
                if let Some(m) = self.members.get_mut(id) {
                    m.1 = *room;
                }
                if room.is_none() || *room != self.room {
                    self.e2e.remove(id);
                }
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
        if self.room.is_none() {
            println!("nie jesteś w żadnym pokoju");
            return;
        }
        let here: Vec<_> = self.members.iter().filter(|(_, (_, room))| *room == self.room).collect();
        if here.is_empty() {
            println!("w pokoju nikogo nie ma");
        }
        for (id, (name, _)) in here {
            let name = sanitize_name(name);
            match self.e2e.get(id) {
                Some((_, _, trust, code)) => {
                    println!("{id:>4}  {name}  {}  kod: {code}", cli_trust_label(*trust))
                }
                None => println!("{id:>4}  {name}  {NO_KEY}"),
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
        let verb = if verify { "z" } else { "a" };
        let mut words = arg.split_whitespace();
        let Some(id) = words.next().and_then(|w| w.parse::<MemberId>().ok()) else {
            println!("podaj numer uczestnika z listy (k) i pierwszą grupę kodu, np. {verb} 3 1234");
            return;
        };
        let Some((name, identity, trust_state, code)) = self.e2e.get(&id) else {
            println!("uczestnik {id} nie ma jeszcze klucza; zobacz listę: k");
            return;
        };
        // The raw name keys the trust store; only the printed one is cleaned.
        let shown = sanitize_name(name);
        // Typing the group forces a look at the code instead of a reflexive "z 3".
        if words.next() != code.split(' ').next() {
            println!("wpisz pierwszą grupę kodu, który porównaliście, np. {verb} 3 1234");
            return;
        }
        {
            let Ok(mut store) = trust.lock() else { return };
            if verify {
                if *trust_state == Trust::KeyChanged {
                    println!("klucz {shown} się zmienił; najpierw porównaj kod i zaakceptuj: a {id}");
                    return;
                }
                store.verify(name, identity);
                println!("{shown} oznaczony jako zweryfikowany");
            } else {
                if *trust_state != Trust::KeyChanged {
                    println!("klucz {shown} się nie zmienił, nie ma czego akceptować");
                    return;
                }
                store.accept(name, identity);
                println!("zaakceptowano klucz {shown}; to urządzenie jest znane, ale niezweryfikowane");
            }
        }
        warn_if_unsaved(trust);
        let _ = commands.send(SessionCommand::TrustChanged(id));
    }
}

/// Checks the server once and installs a newer signed release over `komunikator.exe` next to
/// this program. The release is the window program; this exe is never replaced.
#[cfg(windows)]
fn check_update(cli: &Cli) -> anyhow::Result<()> {
    use client_core::update::{self, current_version, Decision};
    use client_win::updater;

    let Some(key) = updater::release_key() else {
        println!("ta wersja nie ma klucza wydań — samoaktualizacja jest wyłączona");
        return Ok(());
    };
    // The window program updates itself; two updaters would swap the same files.
    let data_dir = cli.data_dir.clone().unwrap_or_else(client_win::startup::default_data_dir);
    if client_win::ui::instance::running(&data_dir) {
        anyhow::bail!("zamknij okno komunikatora przed aktualizacją z wiersza poleceń");
    }
    let (server, fingerprint) = update_server(cli)?;
    let exe = std::env::current_exe()?.with_file_name("komunikator.exe");
    updater::cleanup_on_start(&exe);
    if !exe.is_file() {
        anyhow::bail!("nie ma {} — aktualizowany jest program okienkowy obok komunikator-cli.exe", exe.display());
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    // The window program may already be newer than this CLI: never install below it.
    let current = updater::installed_version(&exe, current_version());
    println!("sprawdzanie aktualizacji na {server} (zainstalowana wersja: {current})…");
    let m = match rt.block_on(update::check(&server, fingerprint, &key, current)) {
        Ok(Decision::UpToDate) => {
            println!("brak nowszej wersji");
            return Ok(());
        }
        Ok(Decision::Available(m)) => m,
        Err(e) => anyhow::bail!("aktualizacja nieudana: {e}"),
    };
    println!("pobieranie wersji {}…", m.version);
    let bytes = rt
        .block_on(update::download(&server, fingerprint, &m))
        .map_err(|e| anyhow::anyhow!("aktualizacja nieudana: {e}"))?;
    let failed = |e: std::io::Error| anyhow::anyhow!("aktualizacja nieudana: {}", updater::describe_io(&e));
    updater::stage(&exe, &bytes).map_err(failed)?;
    updater::swap(&exe).map_err(failed)?;
    if let Err(e) = updater::write_version_stamp(&exe, m.version) {
        println!("UWAGA: nie udało się zapisać komunikator.version: {}", updater::describe_io(&e));
    }
    println!("zaktualizowano {} do wersji {}; uruchomiony komunikator zmieni się po ponownym starcie", exe.display(), m.version);
    Ok(())
}

/// The server to update from: given on the command line (an invite carries one too), else the
/// saved account's.
#[cfg(windows)]
fn update_server(cli: &Cli) -> anyhow::Result<(String, [u8; 32])> {
    if let Some(code) = &cli.invite {
        let invite = decode_invite(code).map_err(|_| anyhow::anyhow!("niepoprawny kod zaproszenia"))?;
        return Ok((invite.server, invite.fingerprint));
    }
    if let (Some(server), Some(fp)) = (&cli.server, &cli.fingerprint) {
        let fp = protocol::parse_fingerprint(fp).ok_or_else(|| anyhow::anyhow!("odcisk musi mieć 64 znaki szesnastkowe"))?;
        return Ok((server.clone(), fp));
    }
    let data_dir = cli.data_dir.clone().unwrap_or_else(client_win::startup::default_data_dir);
    let path = data_dir.join("settings.toml");
    let (_, s, _) = client_core::settings::SettingsStore::load(&path);
    match protocol::parse_fingerprint(&s.fingerprint) {
        Some(fp) if !s.server.is_empty() => Ok((s.server, fp)),
        _ => anyhow::bail!(
            "brak zapisanego serwera w {}: podaj --server i --fingerprint albo --invite",
            path.display()
        ),
    }
}

#[cfg(not(windows))]
fn main() {
    let _ = Cli::parse();
    eprintln!("komunikator działa tylko na Windows");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_key_text_does_not_suggest_plaintext_audio() {
        assert!(!NO_KEY.contains("bez szyfrowania"));
        assert!(NO_KEY.contains("nie słyszycie się"));
    }

    #[test]
    fn accept_is_refused_unless_key_changed() {
        let dir = tempfile::tempdir().unwrap();
        let trust = Mutex::new(KnownDevices::load(&dir.path().join("k.toml")).unwrap());
        let id = IdentityPublic { sign: [1; 32], dh: [2; 32], device: [3; 16] };
        {
            let mut t = trust.lock().unwrap();
            t.check("ala", &id);
            t.verify("ala", &id);
        }
        let mut roster = Roster::default();
        roster.e2e.insert(5, ("ala".into(), id, Trust::Verified, "0000 1111 2222 3333".into()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        roster.trust_command(false, "5 0000", &trust, &tx);
        assert!(rx.try_recv().is_err(), "no TrustChanged for a stray accept");
        assert_eq!(trust.lock().unwrap().check("ala", &id), Trust::Verified);
        // A changed key is accepted.
        roster.e2e.insert(5, ("ala".into(), id, Trust::KeyChanged, "0000 1111 2222 3333".into()));
        roster.trust_command(false, "5 0000", &trust, &tx);
        assert_eq!(rx.try_recv(), Ok(SessionCommand::TrustChanged(5)));
    }

    #[test]
    fn z_requires_matching_code_group() {
        let dir = tempfile::tempdir().unwrap();
        let trust = Mutex::new(KnownDevices::load(&dir.path().join("k.toml")).unwrap());
        let id = IdentityPublic { sign: [1; 32], dh: [2; 32], device: [3; 16] };
        trust.lock().unwrap().check("ala", &id);
        let mut roster = Roster::default();
        roster.e2e.insert(5, ("ala".into(), id, Trust::Known, "3141 1111 2222 3333".into()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for bad in ["5", "5 2718", "5 31", "5 1111"] {
            roster.trust_command(true, bad, &trust, &tx);
            assert!(rx.try_recv().is_err(), "{bad}");
        }
        assert_eq!(trust.lock().unwrap().check("ala", &id), Trust::Known);
        roster.trust_command(true, "5 3141", &trust, &tx);
        assert_eq!(rx.try_recv(), Ok(SessionCommand::TrustChanged(5)));
        assert_eq!(trust.lock().unwrap().check("ala", &id), Trust::Verified);
    }

    #[test]
    fn room_error_messages_are_polish() {
        use protocol::RoomErrorReason::*;
        let all = [
            (InvalidName, "niepoprawna nazwa (1–32 znaki)"),
            (NameTaken, "nazwa zajęta"),
            (TooManyRooms, "za dużo pokoi"),
            (NotFound, "pokój nie istnieje"),
            (NotAllowed, "nie możesz usunąć cudzego pokoju"),
            (NotEmpty, "w pokoju ktoś jest"),
            (Full, "pokój pełny"),
        ];
        for (reason, text) in all {
            assert_eq!(describe_room_error(&reason), text);
        }
    }

    #[test]
    fn chat_line_lists_links() {
        let line = ChatLine {
            from: 2,
            name: "ola".into(),
            text: "wbijaj 192.168.1.10:27015 albo https://x.pl".into(),
            own: false,
            at: std::time::SystemTime::UNIX_EPOCH,
        };
        assert_eq!(
            format_chat(&line, (9, 5)),
            vec![
                "[09:05] ola: wbijaj 192.168.1.10:27015 albo https://x.pl".to_string(),
                "  link 1: 192.168.1.10:27015".to_string(),
                "  link 2: https://x.pl".to_string(),
            ]
        );
        let own = ChatLine { own: true, text: "hej".into(), ..line };
        assert_eq!(format_chat(&own, (23, 59)), vec!["[23:59] ty: hej".to_string()]);
    }

    #[test]
    fn chat_continuation_lines_are_indented() {
        let line = ChatLine {
            from: 2,
            name: "ola".into(),
            text: "hej\n[12:01] ewa: daj hasło".into(),
            own: false,
            at: std::time::SystemTime::UNIX_EPOCH,
        };
        assert_eq!(
            format_chat(&line, (12, 0)),
            vec!["[12:00] ola: hej".to_string(), "    [12:01] ewa: daj hasło".to_string()]
        );
    }

    fn sample_lobby() -> Lobby {
        use protocol::{MemberInfo, RoomInfo};
        let r = |id, name: &str| RoomInfo { id, name: name.into(), creator: String::new() };
        let m = |id, name: &str, room| MemberInfo { id, name: name.into(), room };
        Lobby::new(
            1,
            Some(10),
            vec![r(10, "Ogólny"), r(11, "Ran\u{202E}ked")],
            vec![m(1, "ja", Some(10)), m(2, "o\u{1b}[2Jla", Some(10)), m(3, "ala", Some(11))],
        )
        .0
    }

    #[test]
    fn lobby_lines_name_people_and_rooms() {
        let l = sample_lobby();
        let line = |e: SessionEvent| lobby_line(Some(&l), &e);
        assert_eq!(line(SessionEvent::MemberLeft(2)).as_deref(), Some("- o[2Jla wychodzi"));
        assert_eq!(
            line(SessionEvent::MemberMoved { id: 3, room: Some(10) }).as_deref(),
            Some("ala przechodzi do pokoju Ogólny")
        );
        assert_eq!(line(SessionEvent::MemberMoved { id: 3, room: None }).as_deref(), Some("ala wychodzi z pokoju"));
        assert_eq!(
            line(SessionEvent::MemberMoved { id: 1, room: Some(11) }).as_deref(),
            Some("przechodzisz do pokoju Ranked")
        );
        assert_eq!(line(SessionEvent::MemberMoved { id: 1, room: None }).as_deref(), Some("opuszczasz pokój"));
        assert_eq!(line(SessionEvent::RoomDeleted(11)).as_deref(), Some("usunięto pokój Ranked"));
        assert_eq!(line(SessionEvent::MemberLeft(99)).as_deref(), Some("- uczestnik 99 wychodzi"));
        assert_eq!(line(SessionEvent::Connecting), None);
        assert_eq!(
            lobby_line(None, &SessionEvent::MemberMoved { id: 3, room: Some(12) }).as_deref(),
            Some("uczestnik 3 przechodzi do pokoju 12")
        );
    }

    #[test]
    fn room_list_hides_control_and_bidi_characters() {
        assert_eq!(
            format_rooms(&sample_lobby()),
            vec!["* 10  Ogólny", "      ja, o[2Jla", "  11  Ranked", "      ala"]
        );
    }

    #[test]
    fn roster_keeps_the_first_name_of_an_id() {
        use protocol::MemberInfo;
        let mut roster = Roster::default();
        let joined = |name: &str, room| SessionEvent::MemberJoined(MemberInfo { id: 2, name: name.into(), room });
        roster.observe(&joined("ola", Some(1)));
        roster.observe(&joined("ewa", Some(3)));
        assert_eq!(roster.members.get(&2), Some(&("ola".to_string(), Some(3))));
    }

    #[test]
    fn fatal_messages_are_polish() {
        use client_core::net::session::FatalReason;
        assert_eq!(describe_fatal(FatalReason::BadCredentials), "zły login lub hasło");
        assert_eq!(
            describe_fatal(FatalReason::VersionMismatch { server: 9 }),
            format!("niezgodna wersja programu (serwer: 9, Twoja: {}) — zaktualizuj komunikator", protocol::PROTOCOL_VERSION)
        );
        assert_eq!(describe_fatal(FatalReason::SessionReplaced), "zalogowano się na to konto w innym miejscu");
    }

    #[test]
    fn login_options() {
        use protocol::invite::encode_invite;
        let invite = Invite { server: "serwer.pl:7700".into(), fingerprint: [0xAB; 32], code: [5; 16] };
        let code = encode_invite(&invite);
        let some = |s: &str| Some(s.to_owned());
        let pw = some("długie hasło");
        let err = |r: Result<Login, String>| r.err().unwrap_or_default();

        match choose_login(Some(&code), None, None, some("ola"), pw.clone()) {
            Ok(Login::Register { invite: i, user, password }) => {
                assert_eq!((i, user.as_str(), password.as_str()), (invite.clone(), "ola", "długie hasło"))
            }
            _ => panic!("expected a registration"),
        }
        assert!(matches!(choose_login(None, None, None, None, None), Ok(Login::Saved)));
        let fp = "ab".repeat(32);
        match choose_login(None, some("serwer.pl:7700"), Some(fp.clone()), some("ola"), pw.clone()) {
            Ok(Login::Given { server, fingerprint, user, .. }) => {
                assert_eq!((server.as_str(), fingerprint, user.as_str()), ("serwer.pl:7700", [0xAB; 32], "ola"))
            }
            _ => panic!("expected a password login"),
        }

        assert_eq!(err(choose_login(Some(&code), None, None, None, pw.clone())), "do --invite podaj też --user <nazwa>");
        assert_eq!(
            err(choose_login(Some(&code), some("a:1"), None, some("ola"), pw.clone())),
            "zaproszenie zawiera już adres serwera i odcisk: nie podawaj --server ani --fingerprint"
        );
        assert_eq!(
            err(choose_login(Some("kom1-xyz"), None, None, some("ola"), pw.clone())),
            "niepoprawny kod zaproszenia: to nie wygląda na zaproszenie"
        );
        assert_eq!(err(choose_login(Some(&code), None, None, some("ola"), None)), "ustaw hasło w zmiennej KOMUNIKATOR_PASSWORD");
        assert_eq!(
            err(choose_login(None, None, None, some("ola"), pw.clone())),
            "wymagane: --server, --fingerprint i --user (albo --invite i --user)"
        );
        assert_eq!(
            err(choose_login(None, some("a:1"), some("abc"), some("ola"), pw)),
            "--fingerprint musi mieć 64 znaki hex"
        );
        assert_eq!(
            err(choose_login(None, some("a:1"), Some(fp), some("ola"), None)),
            "ustaw hasło w zmiennej KOMUNIKATOR_PASSWORD"
        );
    }

    #[test]
    fn expired_session_hint_names_the_command() {
        let s = client_core::settings::Settings {
            server: "serwer.pl:7700".into(),
            fingerprint: "ab".repeat(32),
            username: "Ola".into(),
            ..Default::default()
        };
        assert_eq!(
            relogin_hint(&s, None),
            format!(
                "zaloguj się ponownie hasłem (zapamięta nową sesję):\n  \
                 komunikator-cli --server serwer.pl:7700 --fingerprint {} --user Ola\n\
                 z hasłem w zmiennej KOMUNIKATOR_PASSWORD",
                "ab".repeat(32)
            )
        );
        let with_dir = relogin_hint(&s, Some(std::path::Path::new("D:\\dane kom")));
        assert!(with_dir.contains("--user Ola --data-dir \"D:\\dane kom\""), "{with_dir}");
    }

    #[test]
    fn full_volume_table_is_reported() {
        let mut book = VolumeBook::default();
        let gains = GainTable::new();
        for id in 1..=64 {
            assert!(gains.set(id, 1.0));
        }
        assert_eq!(set_volume("ola", 100, "50", &mut book, &gains), Err(GAINS_FULL.to_string()));
        assert_eq!(toggle_mute("ola", 100, &mut book, &gains), Err(GAINS_FULL.to_string()));
        assert_eq!(book.percent("ola"), 50, "still remembered for the next connection");
    }

    #[test]
    fn room_list_marks_my_room() {
        use protocol::{MemberInfo, RoomInfo};
        let r = |id, name: &str| RoomInfo { id, name: name.into(), creator: String::new() };
        let m = |id, name: &str, room| MemberInfo { id, name: name.into(), room };
        let (lobby, _) = Lobby::new(
            1,
            Some(10),
            vec![r(10, "Ogólny"), r(11, "Ranked")],
            vec![m(1, "ja", Some(10)), m(2, "ola", Some(10)), m(3, "ala", Some(11)), m(4, "ela", None)],
        );
        assert_eq!(
            format_rooms(&lobby),
            vec!["* 10  Ogólny", "      ja, ola", "  11  Ranked", "      ala", "poza pokojami: ela"]
        );
    }

    #[test]
    fn volume_commands_update_book_and_table() {
        let mut book = VolumeBook::default();
        let gains = GainTable::new();
        assert_eq!(set_volume("ola", 7, "150", &mut book, &gains), Ok(150));
        assert_eq!(gains.get(7), 1.5);
        assert!(set_volume("ola", 7, "201", &mut book, &gains).is_err());
        assert!(set_volume("ola", 7, "głośno", &mut book, &gains).is_err());
        assert_eq!(book.percent("ola"), 150);
        assert_eq!(toggle_mute("ola", 7, &mut book, &gains), Ok(true));
        assert_eq!(gains.get(7), 0.0);
        assert_eq!(toggle_mute("ola", 7, &mut book, &gains), Ok(false));
        assert_eq!(gains.get(7), 1.5);
    }
}
