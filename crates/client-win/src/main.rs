//! Headless voice client: komunikator --server host:port --fingerprint HEX --user NAME

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use std::collections::BTreeMap;

use clap::Parser;
use client_core::audio::volume::{GainTable, VolumeBook};
use client_core::chat::find_links;
use client_core::e2e::{AnnounceError, E2eEvent, IdentityPublic, KnownDevices, Trust};
use client_core::lobby::Lobby;
use client_core::net::session::{
    spawn_session, ChatLine, ChatSendError, SessionCommand, SessionEvent, SessionParams,
};
use protocol::{MemberId, RoomErrorReason, RoomId};

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
             Utworzono nową. Znajomi zobaczą Cię jako „nowe urządzenie” i powinni porównać z Tobą kod bezpieczeństwa.",
            broken.display()
        ),
    }
    let identity = Arc::new(identity);
    let trust_path = data_dir.join("known_devices.toml");
    let known = KnownDevices::load(&trust_path)?;
    if let Some(broken) = known.recovered_from() {
        println!(
            "UWAGA: nie udało się odczytać listy zaufanych urządzeń (odłożona jako {}).\n\
             Zaczynamy od pustej listy: wszyscy znajomi pojawią się jako nowe osoby, \
             a wcześniejsze weryfikacje kodów trzeba powtórzyć.",
            broken.display()
        );
    }
    let trust = Arc::new(Mutex::new(known));
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
    let gains = Arc::new(GainTable::new());
    let render = {
        let (controls, stop, device) = (controls.clone(), stop.clone(), cli.render_device);
        let gains = gains.clone();
        std::thread::Builder::new().name("render".into()).spawn(move || {
            let _mmcss = enter_pro_audio();
            let sink = Supervised::new(move || WasapiRender::open(device.as_deref()), std::time::Instant::now);
            render_loop(sink, audio_in, controls, gains, stop);
        })?
    };

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
                        println!("błąd: {reason:?}");
                        return 1;
                    }
                    Some(e) => {
                        roster.observe(&e);
                        observe_lobby(&mut lobby, &e);
                        match &e {
                            SessionEvent::MemberLeft(id) => gains.remove(*id),
                            SessionEvent::Connected { .. }
                            | SessionEvent::MemberJoined(_)
                            | SessionEvent::MemberMoved { .. } => apply_book(&roster, &book, &gains),
                            _ => {}
                        }
                        print_event(&e, local_hm());
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
                                println!("podaj numer osoby z listy (p), np. {verb} 3");
                                continue;
                            };
                            if verb == "x" {
                                let muted = toggle_mute(&name, id, &mut book, &gains);
                                println!("{name}: wyciszony u Ciebie: {}", if muted { "TAK" } else { "nie" });
                            } else {
                                match set_volume(&name, id, words.next().unwrap_or(""), &mut book, &gains) {
                                    Ok(p) => println!("{name}: głośność {p}%"),
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

    stop.store(true, Ordering::Relaxed);
    session.shutdown();
    let _ = capture.join();
    let _ = render.join();
    std::process::exit(exit_code);
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

/// Roster line for a member without a verified key: audio is never sent in the clear.
const NO_KEY: &str = "brak klucza — nie słyszycie się do czasu ogłoszenia";

fn toggle(label: &str, flag: &AtomicBool) {
    let now = !flag.fetch_xor(true, Ordering::Relaxed);
    println!("{label}: {}", if now { "TAK" } else { "nie" });
}

fn print_event(e: &SessionEvent, now: (u16, u16)) {
    match e {
        SessionEvent::Connecting => println!("łączenie…"),
        SessionEvent::Connected { you, room, members, .. } => {
            let names: Vec<_> = members
                .iter()
                .filter(|m| m.id != *you && room.is_some() && m.room == *room)
                .map(|m| m.name.as_str())
                .collect();
            println!("połączono (id {you}); w pokoju: {}", if names.is_empty() { "nikt".into() } else { names.join(", ") });
        }
        SessionEvent::MemberJoined(m) => println!("+ {} dołącza", m.name),
        SessionEvent::MemberLeft(id) => println!("- uczestnik {id} wychodzi"),
        SessionEvent::MemberMoved { id, room: Some(r) } => println!("uczestnik {id} przechodzi do pokoju {r}"),
        SessionEvent::MemberMoved { id, room: None } => println!("uczestnik {id} wychodzi z pokoju"),
        SessionEvent::RoomCreated(r) => println!("nowy pokój: {} (id {})", r.name, r.id),
        SessionEvent::RoomDeleted(id) => println!("usunięto pokój {id}"),
        SessionEvent::RoomError(reason) => println!("błąd pokoju: {}", describe_room_error(reason)),
        SessionEvent::Chat(line) => format_chat(line, now).iter().for_each(|l| println!("{l}")),
        SessionEvent::ChatUnreadable { name, .. } => {
            println!("nie udało się odszyfrować wiadomości od {name} (klucze jeszcze się wymieniają)")
        }
        SessionEvent::ChatNotSent(reason) => println!("nie wysłano wiadomości: {}", describe_chat_error(*reason)),
        SessionEvent::Disconnected { retry_in } => {
            println!("rozłączono, ponowna próba za {:.1} s", retry_in.as_secs_f32())
        }
        SessionEvent::Fatal(r) => println!("błąd: {r:?}"),
        SessionEvent::E2e(E2eEvent::Member { id, name, trust, code, .. }) => match trust {
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
        },
        SessionEvent::E2e(E2eEvent::Rejected { id, reason }) => {
            println!("e2e: odrzucono ogłoszenie klucza od uczestnika {id}: {}", describe_reject(reason))
        }
    }
}

fn describe_room_error(r: &RoomErrorReason) -> &'static str {
    match r {
        RoomErrorReason::InvalidName => "niepoprawna nazwa (1–32 znaki)",
        RoomErrorReason::NameTaken => "nazwa zajęta",
        RoomErrorReason::TooManyRooms => "za dużo pokoi",
        RoomErrorReason::NotFound => "pokój nie istnieje",
        RoomErrorReason::NotAllowed => "nie możesz usunąć cudzego pokoju",
        RoomErrorReason::NotEmpty => "w pokoju ktoś jest",
        RoomErrorReason::Full => "pokój pełny",
    }
}

fn describe_chat_error(r: ChatSendError) -> &'static str {
    match r {
        ChatSendError::Empty => "pusta wiadomość",
        ChatSendError::TooLong => "za długa (limit 2000 znaków)",
        ChatSendError::NotInRoom => "nie jesteś w żadnym pokoju (wejdź do pokoju: w <nr>)",
        ChatSendError::NotConnected => "brak połączenia z serwerem",
    }
}

/// The text line, then one line per link so it can be copied out of the console.
fn format_chat(line: &ChatLine, (h, m): (u16, u16)) -> Vec<String> {
    let who = if line.own { "ty" } else { line.name.as_str() };
    let mut out = vec![format!("[{h:02}:{m:02}] {who}: {}", line.text)];
    for (i, link) in find_links(&line.text).iter().enumerate() {
        out.push(format!("  link {}: {}", i + 1, &line.text[link.range.clone()]));
    }
    out
}

/// Rooms in id order with their members; my room is marked with `*`.
fn format_rooms(lobby: &Lobby) -> Vec<String> {
    let names = |room| {
        let mut n: Vec<_> = lobby.members_in(room).iter().map(|m| m.name.clone()).collect();
        n.sort();
        n.join(", ")
    };
    let mut out = Vec::new();
    for r in lobby.rooms() {
        let mark = if lobby.my_room() == Some(r.id) { '*' } else { ' ' };
        out.push(format!("{mark} {:>2}  {}", r.id, r.name));
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

/// Pushes remembered volumes (kept by name) to the render thread's id-keyed table.
fn apply_book(roster: &Roster, book: &VolumeBook, gains: &GainTable) {
    for (id, (name, _)) in &roster.members {
        gains.set(*id, book.gain(name));
    }
}

fn set_volume(name: &str, id: MemberId, arg: &str, book: &mut VolumeBook, gains: &GainTable) -> Result<u16, String> {
    let percent = arg
        .parse::<u16>()
        .ok()
        .filter(|p| *p <= 200)
        .ok_or_else(|| "podaj głośność 0–200 (%), np. g 3 150".to_string())?;
    book.set_percent(name, percent);
    gains.set(id, book.gain(name));
    Ok(percent)
}

/// Returns whether the person is now muted.
fn toggle_mute(name: &str, id: MemberId, book: &mut VolumeBook, gains: &GainTable) -> bool {
    let muted = book.toggle_muted(name);
    gains.set(id, book.gain(name));
    muted
}

#[cfg(windows)]
fn local_hm() -> (u16, u16) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wHour, t.wMinute)
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
            SessionEvent::MemberJoined(m) if m.id != self.you => {
                self.members.insert(m.id, (m.name.clone(), m.room));
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
            match self.e2e.get(id) {
                Some((_, _, trust, code)) => {
                    println!("{id:>4}  {name}  {}  kod: {code}", trust_label(*trust))
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
        // Typing the group forces a look at the code instead of a reflexive "z 3".
        if words.next() != code.split(' ').next() {
            println!("wpisz pierwszą grupę kodu, który porównaliście, np. {verb} 3 1234");
            return;
        }
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
                if *trust_state != Trust::KeyChanged {
                    println!("klucz {name} się nie zmienił, nie ma czego akceptować");
                    return;
                }
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
        Trust::Known => "znany, niezweryfikowany",
        Trust::NewUser => "nowa osoba",
        Trust::NewDevice => "NOWE URZĄDZENIE, niezweryfikowane",
        Trust::KeyChanged => "ZMIENIONY KLUCZ (a <id> <grupa> po sprawdzeniu)",
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
        assert!(toggle_mute("ola", 7, &mut book, &gains));
        assert_eq!(gains.get(7), 0.0);
        assert!(!toggle_mute("ola", 7, &mut book, &gains));
        assert_eq!(gains.get(7), 1.5);
    }
}
