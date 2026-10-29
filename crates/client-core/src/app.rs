//! The GUI's state: a pure model fed with session events, user actions and audio
//! activity. It returns the side effects to run and keeps a view model the window only
//! has to draw, so all behaviour is testable without Win32.

use std::collections::{BTreeMap, BTreeSet};

use protocol::{MemberId, RoomErrorReason, RoomId, PROTOCOL_VERSION};

use crate::chat::{find_links, sanitize_name};
use crate::e2e::{E2eEvent, IdentityPublic, Trust};
use crate::lobby::{Lobby, RoomChange};
use crate::net::session::{ChatLine, ChatSendError, FatalReason, SessionCommand, SessionEvent};
use crate::settings::Settings;

const MAX_CHAT_LINES: usize = 500;

const WRONG_GROUP: &str = "wpisz pierwszą grupę kodu, który porównaliście";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
    JoinRoom(RoomId),
    LeaveRoom,
    CreateRoom(String),
    DeleteRoom(RoomId),
    SendChat(String),
    /// `name` as shown in the view; volumes are remembered by it.
    SetVolume { name: String, percent: u16 },
    ToggleLocalMute(String),
    /// `group` is the first 4-digit group of the safety code, typed by the user.
    VerifyMember { id: MemberId, group: String },
    AcceptMember { id: MemberId, group: String },
    CopyLink(String),
    WindowShown,
    WindowHidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundKind {
    Muted,
    Unmuted,
    Joined,
    Left,
    Chat,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Command(SessionCommand),
    SetGain { id: MemberId, gain: f32 },
    ClearGains,
    PlaySound(SoundKind),
    CopyToClipboard(String),
    SaveSettings,
    /// Mark this device as verified (or accept its changed key) in the trust store, then
    /// tell the session. `name` is the raw username the store is keyed by.
    Trust { verify: bool, id: MemberId, name: String, identity: IdentityPublic },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Connected,
    Disconnected { retry_in_s: u32 },
    Fatal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub id: MemberId,
    pub name: String,
    pub me: bool,
    pub speaking: bool,
    pub percent: u16,
    pub locally_muted: bool,
    pub trust: Option<Trust>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomView {
    pub id: RoomId,
    pub name: String,
    pub members: Vec<MemberView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatView {
    /// "HH:MM", local time at receipt.
    pub time: String,
    pub author: String,
    /// May span several lines; the UI shows the rest as continuation rows.
    pub text: String,
    pub own: bool,
    pub links: Vec<String>,
    /// A notice from the program, not a message from a person.
    pub system: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub status: Status,
    pub rooms: Vec<RoomView>,
    pub my_room: Option<RoomId>,
    pub chat: Vec<ChatView>,
    pub unread: u32,
    pub muted: bool,
    pub deafened: bool,
    pub notice: Option<String>,
}

/// What the E2E layer reported about a member of my room.
struct E2eInfo {
    /// Raw, as the trust store is keyed by it.
    name: String,
    identity: IdentityPublic,
    trust: Trust,
    code: String,
}

pub struct AppState {
    settings: Settings,
    clock: Box<dyn Fn() -> (u8, u8)>,
    lobby: Option<Lobby>,
    /// Only for members of my room; dropped when they or I leave it.
    e2e: BTreeMap<MemberId, E2eInfo>,
    speaking: BTreeSet<MemberId>,
    me_speaking: bool,
    window_shown: bool,
    /// The room I asked to create; I enter it when the server announces it.
    pending_room: Option<String>,
    view: View,
    version: u64,
    /// The view changed since the last version bump.
    dirty: bool,
}

impl AppState {
    pub fn with_clock(settings: Settings, clock: Box<dyn Fn() -> (u8, u8)>) -> AppState {
        AppState {
            settings,
            clock,
            lobby: None,
            e2e: BTreeMap::new(),
            speaking: BTreeSet::new(),
            me_speaking: false,
            window_shown: true,
            pending_room: None,
            view: View {
                status: Status::Connecting,
                rooms: Vec::new(),
                my_room: None,
                chat: Vec::new(),
                unread: 0,
                muted: false,
                deafened: false,
                notice: None,
            },
            version: 0,
            dirty: false,
        }
    }

    pub fn on_event(&mut self, e: SessionEvent) -> Vec<Effect> {
        let mut out = Vec::new();
        match e {
            SessionEvent::Connecting => self.set_status(Status::Connecting),
            // The GUI stores it before the event gets here; nothing to show.
            SessionEvent::TokenIssued(_) => {}
            SessionEvent::Connected { you, room, rooms, members } => {
                self.lobby = Some(Lobby::new(you, room, rooms, members).0);
                self.e2e.clear();
                self.speaking.clear();
                self.set_status(Status::Connected);
                // Every member id is new after a (re)connect.
                out.push(Effect::ClearGains);
                let ids: Vec<MemberId> = self.lobby.iter().flat_map(|l| l.members().map(|m| m.id)).collect();
                out.extend(ids.into_iter().filter_map(|id| self.gain_for(id)));
            }
            SessionEvent::Disconnected { retry_in } => {
                self.lobby = None;
                self.pending_room = None;
                self.e2e.clear();
                self.speaking.clear();
                let secs = retry_in.as_secs() + u64::from(retry_in.subsec_nanos() > 0);
                self.set_status(Status::Disconnected { retry_in_s: u32::try_from(secs).unwrap_or(u32::MAX) });
            }
            SessionEvent::Fatal(reason) => self.set_status(Status::Fatal(describe_fatal(reason))),
            SessionEvent::MemberJoined(m) => {
                let id = m.id;
                let changes = self.lobby.as_mut().map(|l| l.member_joined(m)).unwrap_or_default();
                out.extend(self.gain_for(id));
                self.room_changes(changes, &mut out);
            }
            SessionEvent::MemberLeft(id) => {
                let changes = self.lobby.as_mut().map(|l| l.member_left(id)).unwrap_or_default();
                self.e2e.remove(&id);
                self.room_changes(changes, &mut out);
            }
            SessionEvent::MemberMoved { id, room } => {
                let changes = self.lobby.as_mut().map(|l| l.member_moved(id, room)).unwrap_or_default();
                out.extend(self.gain_for(id));
                self.room_changes(changes, &mut out);
            }
            SessionEvent::RoomCreated(r) => {
                let mine = self.lobby.as_ref().and_then(|l| l.name_of(l.me())) == Some(r.creator.as_str());
                if mine && self.pending_room.as_deref() == Some(r.name.as_str()) {
                    self.pending_room = None;
                    out.push(Effect::Command(SessionCommand::JoinRoom(r.id)));
                }
                if let Some(l) = self.lobby.as_mut() {
                    l.room_created(r);
                }
            }
            SessionEvent::RoomDeleted(id) => {
                let changes = self.lobby.as_mut().map(|l| l.room_deleted(id)).unwrap_or_default();
                self.room_changes(changes, &mut out);
            }
            SessionEvent::RoomError(reason) => {
                self.pending_room = None;
                self.set_notice(Some(describe_room_error(&reason).to_owned()));
            }
            SessionEvent::Chat(line) => out.extend(self.chat_line(line)),
            SessionEvent::ChatUnreadable { name, .. } => self.system_line(format!(
                "nie udało się odszyfrować wiadomości od {} (klucze jeszcze się wymieniają)",
                sanitize_name(&name)
            )),
            SessionEvent::ChatNotSent(reason) => {
                self.system_line(format!("nie wysłano wiadomości: {}", describe_chat_not_sent(reason)))
            }
            SessionEvent::E2e(E2eEvent::Member { id, name, identity, trust, code }) => {
                if self.in_my_room(id) {
                    self.e2e.insert(id, E2eInfo { name, identity, trust, code });
                }
            }
            SessionEvent::E2e(E2eEvent::Rejected { id, .. }) => {
                self.e2e.remove(&id);
            }
        }
        self.finish();
        out
    }

    pub fn on_action(&mut self, a: Action) -> Vec<Effect> {
        let mut out = Vec::new();
        match a {
            Action::ToggleMute => {
                self.view.muted = !self.view.muted;
                self.dirty = true;
                out.extend(self.mute_sound(self.view.muted));
            }
            Action::ToggleDeafen => {
                self.view.deafened = !self.view.deafened;
                self.dirty = true;
                out.extend(self.mute_sound(self.view.deafened));
            }
            Action::JoinRoom(id) => out.push(Effect::Command(SessionCommand::JoinRoom(id))),
            Action::LeaveRoom => out.push(Effect::Command(SessionCommand::LeaveRoom)),
            Action::CreateRoom(name) => {
                // As the server will name it, so its announcement can be recognised.
                let name = protocol::normalize_room_name(&name).unwrap_or(name);
                self.pending_room = Some(name.clone());
                out.push(Effect::Command(SessionCommand::CreateRoom(name)));
            }
            Action::DeleteRoom(id) => out.push(Effect::Command(SessionCommand::DeleteRoom(id))),
            Action::SendChat(text) => out.push(Effect::Command(SessionCommand::SendChat(text))),
            Action::SetVolume { name, percent } => {
                self.settings.volumes.set_percent(&name, percent);
                out.extend(self.gains_for_name(&name));
                out.push(Effect::SaveSettings);
            }
            Action::ToggleLocalMute(name) => {
                self.settings.volumes.toggle_muted(&name);
                out.extend(self.gains_for_name(&name));
                out.push(Effect::SaveSettings);
            }
            Action::VerifyMember { id, group } => out.extend(self.trust_action(true, id, &group)),
            Action::AcceptMember { id, group } => out.extend(self.trust_action(false, id, &group)),
            Action::CopyLink(link) => {
                self.set_notice(Some(format!("skopiowano: {link}")));
                out.push(Effect::CopyToClipboard(link));
            }
            Action::WindowShown => {
                self.window_shown = true;
                if self.view.unread != 0 {
                    self.view.unread = 0;
                    self.dirty = true;
                }
            }
            Action::WindowHidden => self.window_shown = false,
        }
        self.finish();
        out
    }

    pub fn on_activity(&mut self, speaking: &[MemberId], me_speaking: bool) -> bool {
        self.speaking = speaking.iter().copied().collect();
        self.me_speaking = me_speaking;
        let before = self.version;
        self.finish();
        self.version != before
    }

    /// Forgets everything about the current session before another one (a changed account)
    /// starts; the chat history and mute state stay.
    pub fn reset_session(&mut self) {
        self.lobby = None;
        self.e2e.clear();
        self.speaking.clear();
        self.pending_room = None;
        self.set_status(Status::Connecting);
        self.finish();
    }

    /// The session could not even start (e.g. the account was changed and the new client
    /// failed); shown like a fatal session error until the next `reset_session`.
    pub fn session_failed(&mut self, reason: String) {
        self.reset_session();
        self.set_status(Status::Fatal(reason));
        self.finish();
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    pub fn view_version(&self) -> u64 {
        self.version
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    pub fn muted(&self) -> bool {
        self.view.muted
    }

    pub fn deafened(&self) -> bool {
        self.view.deafened
    }

    fn room_changes(&mut self, changes: Vec<RoomChange>, out: &mut Vec<Effect>) {
        for c in changes {
            let sound = match c {
                // A new room means new keys: nothing is known about anyone there yet.
                RoomChange::Entered { .. } | RoomChange::Exited => {
                    self.e2e.clear();
                    None
                }
                RoomChange::Joined(_) => Some(SoundKind::Joined),
                RoomChange::Left(id) => {
                    self.e2e.remove(&id);
                    Some(SoundKind::Left)
                }
            };
            if let Some(k) = sound.filter(|_| self.settings.sounds.join_leave) {
                out.push(Effect::PlaySound(k));
            }
        }
    }

    fn mute_sound(&self, now: bool) -> Option<Effect> {
        let kind = if now { SoundKind::Muted } else { SoundKind::Unmuted };
        self.settings.sounds.mute.then_some(Effect::PlaySound(kind))
    }

    fn in_my_room(&self, id: MemberId) -> bool {
        self.lobby.as_ref().is_some_and(|l| {
            id != l.me() && l.my_room().is_some() && l.members_in(l.my_room()).iter().any(|m| m.id == id)
        })
    }

    /// Another member's remembered gain, looked up by their shown name.
    fn gain_for(&self, id: MemberId) -> Option<Effect> {
        let l = self.lobby.as_ref()?;
        if id == l.me() {
            return None;
        }
        let name = sanitize_name(l.name_of(id)?);
        Some(Effect::SetGain { id, gain: self.settings.volumes.gain(&name) })
    }

    fn gains_for_name(&self, name: &str) -> Vec<Effect> {
        let Some(l) = &self.lobby else { return Vec::new() };
        l.members()
            .filter(|m| sanitize_name(&m.name) == name)
            .filter_map(|m| self.gain_for(m.id))
            .collect()
    }

    fn trust_action(&mut self, verify: bool, id: MemberId, group: &str) -> Option<Effect> {
        let Some(info) = self.e2e.get(&id) else {
            self.set_notice(Some("ta osoba nie ma jeszcze klucza".to_owned()));
            return None;
        };
        // Typing the group forces a look at the code instead of a reflexive click.
        let group = group.trim();
        let well_formed = group.len() == 4 && group.bytes().all(|b| b.is_ascii_digit());
        if !well_formed || info.code.split(' ').next() != Some(group) {
            self.set_notice(Some(WRONG_GROUP.to_owned()));
            return None;
        }
        let shown = sanitize_name(&info.name);
        let changed = info.trust == Trust::KeyChanged;
        let refusal = match (verify, changed) {
            (true, true) => Some(format!("klucz {shown} się zmienił; najpierw porównaj kod i zaakceptuj nowy klucz")),
            (false, false) => Some(format!("klucz {shown} się nie zmienił, nie ma czego akceptować")),
            _ => None,
        };
        if refusal.is_some() {
            self.set_notice(refusal);
            return None;
        }
        let effect = Effect::Trust { verify, id, name: info.name.clone(), identity: info.identity };
        self.set_notice(None);
        Some(effect)
    }

    fn chat_line(&mut self, line: ChatLine) -> Option<Effect> {
        let links = find_links(&line.text).into_iter().map(|l| line.text[l.range].to_owned()).collect();
        let own = line.own;
        let time = self.time();
        self.push_chat(ChatView { time, author: sanitize_name(&line.name), text: line.text, own, links, system: false });
        if own || self.window_shown {
            return None;
        }
        self.view.unread += 1;
        self.settings.sounds.chat.then_some(Effect::PlaySound(SoundKind::Chat))
    }

    fn system_line(&mut self, text: String) {
        let time = self.time();
        self.push_chat(ChatView { time, author: String::new(), text, own: false, links: Vec::new(), system: true });
    }

    fn push_chat(&mut self, line: ChatView) {
        self.view.chat.push(line);
        if self.view.chat.len() > MAX_CHAT_LINES {
            let extra = self.view.chat.len() - MAX_CHAT_LINES;
            self.view.chat.drain(..extra);
        }
        self.dirty = true;
    }

    fn time(&self) -> String {
        let (h, m) = (self.clock)();
        format!("{h:02}:{m:02}")
    }

    fn set_status(&mut self, s: Status) {
        if self.view.status != s {
            self.view.status = s;
            self.dirty = true;
        }
    }

    fn set_notice(&mut self, n: Option<String>) {
        if self.view.notice != n {
            self.view.notice = n;
            self.dirty = true;
        }
    }

    /// Rebuilds the derived part of the view and bumps the version if anything changed.
    fn finish(&mut self) {
        let rooms = self.build_rooms();
        let my_room = self.lobby.as_ref().and_then(Lobby::my_room);
        if rooms != self.view.rooms || my_room != self.view.my_room {
            self.view.rooms = rooms;
            self.view.my_room = my_room;
            self.dirty = true;
        }
        if self.dirty {
            self.version += 1;
            self.dirty = false;
        }
    }

    fn build_rooms(&self) -> Vec<RoomView> {
        let Some(l) = &self.lobby else { return Vec::new() };
        let book = &self.settings.volumes;
        l.rooms()
            .map(|r| RoomView {
                id: r.id,
                name: sanitize_name(&r.name),
                members: l
                    .members_in(Some(r.id))
                    .into_iter()
                    .map(|m| {
                        let me = m.id == l.me();
                        let name = sanitize_name(&m.name);
                        let e2e = if me { None } else { self.e2e.get(&m.id) };
                        MemberView {
                            id: m.id,
                            me,
                            speaking: if me { self.me_speaking } else { self.speaking.contains(&m.id) },
                            percent: if me { 100 } else { book.percent(&name) },
                            locally_muted: !me && book.is_muted(&name),
                            trust: e2e.map(|e| e.trust),
                            code: e2e.map(|e| e.code.clone()),
                            name,
                        }
                    })
                    .collect(),
            })
            .collect()
    }
}

pub fn describe_room_error(r: &RoomErrorReason) -> &'static str {
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

pub fn describe_chat_not_sent(r: ChatSendError) -> &'static str {
    match r {
        ChatSendError::Empty => "pusta wiadomość",
        ChatSendError::TooLong => "za długa (limit 2000 znaków)",
        ChatSendError::NotInRoom => "nie jesteś w żadnym pokoju",
        ChatSendError::NotConnected => "brak połączenia z serwerem",
    }
}

pub fn describe_fatal(r: FatalReason) -> String {
    match r {
        FatalReason::BadCredentials => "zły login lub hasło".into(),
        FatalReason::VersionMismatch { server } => format!(
            "niezgodna wersja programu (serwer: {server}, Twoja: {PROTOCOL_VERSION}) — zaktualizuj komunikator"
        ),
        FatalReason::SessionReplaced => "zalogowano się na to konto w innym miejscu".into(),
        FatalReason::LoginRequired => "sesja wygasła — zaloguj się ponownie".into(),
        FatalReason::InviteInvalid => "kod zaproszenia jest nieważny lub już użyty".into(),
        FatalReason::NameTaken => "ta nazwa jest już zajęta".into(),
        FatalReason::NameInvalid => "nazwa: 1–32 znaki, litery, cyfry, _ - .".into(),
        FatalReason::PasswordTooShort => "hasło musi mieć co najmniej 8 znaków".into(),
        FatalReason::AccountBlocked => "konto zostało zablokowane przez administratora".into(),
    }
}

pub fn trust_label(t: Trust) -> &'static str {
    match t {
        Trust::Verified => "zweryfikowany",
        Trust::Known => "znany, niezweryfikowany",
        Trust::NewUser => "nowa osoba",
        Trust::NewDevice => "NOWE URZĄDZENIE, niezweryfikowane",
        Trust::KeyChanged => "ZMIENIONY KLUCZ",
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use protocol::{MemberInfo, RoomInfo};

    use super::*;

    fn state() -> AppState {
        AppState::with_clock(Settings::default(), Box::new(|| (9, 5)))
    }

    fn m(id: MemberId, name: &str, room: Option<RoomId>) -> MemberInfo {
        MemberInfo { id, name: name.into(), room }
    }

    fn r(id: RoomId, name: &str) -> RoomInfo {
        RoomInfo { id, name: name.into(), creator: String::new() }
    }

    fn connected() -> SessionEvent {
        SessionEvent::Connected {
            you: 1,
            room: Some(10),
            rooms: vec![r(10, "Ogólny"), r(11, "Ranked")],
            members: vec![m(1, "ja", Some(10)), m(2, "ola", Some(10)), m(3, "ala", Some(11)), m(4, "ela", None)],
        }
    }

    fn online() -> AppState {
        let mut s = state();
        s.on_event(connected());
        s
    }

    fn chat(from: MemberId, name: &str, text: &str, own: bool) -> SessionEvent {
        SessionEvent::Chat(ChatLine { from, name: name.into(), text: text.into(), own, at: SystemTime::UNIX_EPOCH })
    }

    fn room(s: &AppState, id: RoomId) -> &RoomView {
        s.view().rooms.iter().find(|r| r.id == id).expect("room in view")
    }

    fn names(s: &AppState, id: RoomId) -> Vec<String> {
        room(s, id).members.iter().map(|m| m.name.clone()).collect()
    }

    fn member(s: &AppState, id: MemberId) -> MemberView {
        s.view().rooms.iter().flat_map(|r| &r.members).find(|m| m.id == id).expect("member in view").clone()
    }

    fn sounds(effects: &[Effect]) -> Vec<SoundKind> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::PlaySound(k) => Some(*k),
                _ => None,
            })
            .collect()
    }

    const ID: IdentityPublic = IdentityPublic { sign: [1; 32], dh: [2; 32], device: [3; 16] };

    fn e2e(id: MemberId, name: &str, trust: Trust) -> SessionEvent {
        SessionEvent::E2e(E2eEvent::Member {
            id,
            name: name.into(),
            identity: ID,
            trust,
            code: "3141 1111 2222 3333".into(),
        })
    }

    #[test]
    fn connected_builds_rooms_and_members() {
        let mut settings = Settings::default();
        settings.volumes.set_percent("ola", 50);
        let mut s = AppState::with_clock(settings, Box::new(|| (9, 5)));
        assert_eq!(s.view().status, Status::Connecting);
        let effects = s.on_event(connected());
        assert_eq!(
            effects,
            vec![
                Effect::ClearGains,
                Effect::SetGain { id: 2, gain: 0.5 },
                Effect::SetGain { id: 3, gain: 1.0 },
                Effect::SetGain { id: 4, gain: 1.0 },
            ]
        );
        let v = s.view();
        assert_eq!(v.status, Status::Connected);
        assert_eq!(v.my_room, Some(10));
        assert_eq!(v.rooms.iter().map(|r| r.id).collect::<Vec<_>>(), vec![10, 11]);
        assert_eq!(room(&s, 10).name, "Ogólny");
        assert_eq!(names(&s, 10), vec!["ja", "ola"]);
        assert_eq!(names(&s, 11), vec!["ala"]);
        assert!(member(&s, 1).me);
        assert!(!member(&s, 2).me);
        assert_eq!(member(&s, 2).percent, 50);
        assert_eq!(member(&s, 2).trust, None);
    }

    #[test]
    fn member_events_update_rooms() {
        let mut s = online();
        let e = s.on_event(SessionEvent::MemberJoined(m(5, "ula", Some(11))));
        assert_eq!(e, vec![Effect::SetGain { id: 5, gain: 1.0 }]);
        assert_eq!(names(&s, 11), vec!["ala", "ula"]);
        let e = s.on_event(SessionEvent::MemberMoved { id: 3, room: Some(10) });
        assert!(e.contains(&Effect::SetGain { id: 3, gain: 1.0 }));
        assert_eq!(names(&s, 10), vec!["ja", "ola", "ala"]);
        s.on_event(SessionEvent::MemberLeft(2));
        assert_eq!(names(&s, 10), vec!["ja", "ala"]);
        s.on_event(SessionEvent::RoomCreated(r(12, "Nowy")));
        assert_eq!(s.view().rooms.iter().map(|r| r.id).collect::<Vec<_>>(), vec![10, 11, 12]);
        s.on_event(SessionEvent::RoomDeleted(12));
        assert_eq!(s.view().rooms.len(), 2);
        s.on_event(SessionEvent::MemberMoved { id: 1, room: Some(11) });
        assert_eq!(s.view().my_room, Some(11));
        s.on_event(SessionEvent::Disconnected { retry_in: Duration::from_millis(1500) });
        assert_eq!(s.view().status, Status::Disconnected { retry_in_s: 2 });
        assert!(s.view().rooms.is_empty());
        assert_eq!(s.view().my_room, None);
        s.on_event(SessionEvent::Connecting);
        assert_eq!(s.view().status, Status::Connecting);
    }

    #[test]
    fn toggle_mute_plays_sound_and_respects_setting() {
        let mut s = state();
        assert_eq!(s.on_action(Action::ToggleMute), vec![Effect::PlaySound(SoundKind::Muted)]);
        assert!(s.muted() && s.view().muted);
        assert_eq!(s.on_action(Action::ToggleMute), vec![Effect::PlaySound(SoundKind::Unmuted)]);
        assert!(!s.muted() && !s.view().muted);
        assert_eq!(s.on_action(Action::ToggleDeafen), vec![Effect::PlaySound(SoundKind::Muted)]);
        assert!(s.deafened() && s.view().deafened);
        assert_eq!(s.on_action(Action::ToggleDeafen), vec![Effect::PlaySound(SoundKind::Unmuted)]);
        s.settings_mut().sounds.mute = false;
        assert_eq!(s.on_action(Action::ToggleMute), vec![]);
        assert!(s.muted());
        assert_eq!(s.on_action(Action::ToggleDeafen), vec![]);
        assert!(s.deafened());
    }

    #[test]
    fn join_leave_sound_only_for_my_room() {
        let mut s = online();
        assert_eq!(sounds(&s.on_event(SessionEvent::MemberJoined(m(5, "ula", Some(11))))), vec![]);
        assert_eq!(sounds(&s.on_event(SessionEvent::MemberJoined(m(6, "iza", None)))), vec![]);
        assert_eq!(
            sounds(&s.on_event(SessionEvent::MemberMoved { id: 3, room: Some(10) })),
            vec![SoundKind::Joined]
        );
        assert_eq!(sounds(&s.on_event(SessionEvent::MemberMoved { id: 3, room: None })), vec![SoundKind::Left]);
        assert_eq!(sounds(&s.on_event(SessionEvent::MemberLeft(2))), vec![SoundKind::Left]);
        assert_eq!(sounds(&s.on_event(SessionEvent::MemberLeft(5))), vec![]);
        // My own moves are not "someone joined".
        assert_eq!(sounds(&s.on_event(SessionEvent::MemberMoved { id: 1, room: Some(11) })), vec![]);
        s.settings_mut().sounds.join_leave = false;
        assert_eq!(sounds(&s.on_event(SessionEvent::MemberJoined(m(7, "ada", Some(11))))), vec![]);
        assert_eq!(names(&s, 11), vec!["ja", "ada"]);
    }

    #[test]
    fn chat_sound_and_unread_only_when_hidden() {
        let mut s = online();
        assert_eq!(s.on_event(chat(2, "ola", "hej", false)), vec![]);
        assert_eq!(s.view().unread, 0);
        s.on_action(Action::WindowHidden);
        assert_eq!(s.on_event(chat(2, "ola", "jesteś?", false)), vec![Effect::PlaySound(SoundKind::Chat)]);
        assert_eq!(s.view().unread, 1);
        assert_eq!(s.on_event(chat(1, "ja", "tak", true)), vec![]);
        assert_eq!(s.on_event(SessionEvent::ChatUnreadable { from: 2, name: "ola".into() }), vec![]);
        assert_eq!(s.view().unread, 1);
        s.settings_mut().sounds.chat = false;
        assert_eq!(s.on_event(chat(2, "ola", "halo", false)), vec![]);
        assert_eq!(s.view().unread, 2);
        s.on_action(Action::WindowShown);
        assert_eq!(s.view().unread, 0);
    }

    #[test]
    fn chat_line_links_are_offered() {
        let mut s = online();
        s.on_event(chat(2, "ola", "wbijaj 192.168.1.10:27015 albo https://x.pl\ndruga linia", false));
        assert_eq!(
            s.view().chat,
            vec![ChatView {
                time: "09:05".into(),
                author: "ola".into(),
                text: "wbijaj 192.168.1.10:27015 albo https://x.pl\ndruga linia".into(),
                own: false,
                links: vec!["192.168.1.10:27015".into(), "https://x.pl".into()],
                system: false,
            }]
        );
        assert_eq!(
            s.on_action(Action::CopyLink("https://x.pl".into())),
            vec![Effect::CopyToClipboard("https://x.pl".into())]
        );
    }

    #[test]
    fn volume_is_applied_by_name_and_saved() {
        let mut s = online();
        assert_eq!(
            s.on_action(Action::SetVolume { name: "ola".into(), percent: 150 }),
            vec![Effect::SetGain { id: 2, gain: 1.5 }, Effect::SaveSettings]
        );
        assert_eq!(s.settings().volumes.percent("ola"), 150);
        assert_eq!(member(&s, 2).percent, 150);
        assert_eq!(
            s.on_action(Action::ToggleLocalMute("ola".into())),
            vec![Effect::SetGain { id: 2, gain: 0.0 }, Effect::SaveSettings]
        );
        assert!(member(&s, 2).locally_muted);
        // Someone not online is still remembered.
        assert_eq!(s.on_action(Action::SetVolume { name: "ewa".into(), percent: 20 }), vec![Effect::SaveSettings]);
        assert_eq!(s.settings().volumes.percent("ewa"), 20);
        // A reconnect gives new ids; the book is applied again by name.
        assert!(s.on_event(connected()).contains(&Effect::SetGain { id: 2, gain: 0.0 }));
        assert_eq!(
            s.on_event(SessionEvent::MemberJoined(m(8, "ewa", None))),
            vec![Effect::SetGain { id: 8, gain: 0.2 }]
        );
    }

    #[test]
    fn verify_requires_code_group() {
        let mut s = online();
        s.on_event(e2e(2, "ola", Trust::Known));
        assert_eq!(member(&s, 2).trust, Some(Trust::Known));
        assert_eq!(member(&s, 2).code.as_deref(), Some("3141 1111 2222 3333"));
        for bad in ["", "2718", "31", "1111", "3141 1111"] {
            assert_eq!(s.on_action(Action::VerifyMember { id: 2, group: bad.into() }), vec![], "{bad}");
            assert_eq!(s.view().notice.as_deref(), Some(WRONG_GROUP));
        }
        assert_eq!(
            s.on_action(Action::VerifyMember { id: 2, group: "3141".into() }),
            vec![Effect::Trust { verify: true, id: 2, name: "ola".into(), identity: ID }]
        );
        // Trust applies to my room only: it goes when she leaves it.
        s.on_event(SessionEvent::MemberMoved { id: 2, room: Some(11) });
        assert_eq!(member(&s, 2).trust, None);
        assert_eq!(member(&s, 2).code, None);
        assert_eq!(s.on_action(Action::VerifyMember { id: 2, group: "3141".into() }), vec![]);
        // ...and when I leave.
        s.on_event(e2e(4, "ela", Trust::Known));
        s.on_event(SessionEvent::MemberMoved { id: 4, room: Some(10) });
        s.on_event(e2e(4, "ela", Trust::Known));
        assert_eq!(member(&s, 4).trust, Some(Trust::Known));
        s.on_event(SessionEvent::MemberMoved { id: 1, room: None });
        assert_eq!(s.on_action(Action::VerifyMember { id: 4, group: "3141".into() }), vec![]);
        s.on_event(SessionEvent::MemberMoved { id: 1, room: Some(10) });
        assert_eq!(member(&s, 4).trust, None);
    }

    #[test]
    fn group_must_be_four_digits_even_if_the_code_is_odd() {
        let mut s = online();
        for (code, group) in [("", ""), (" 1111", ""), ("abcd 1111", "abcd"), ("12 34", "12")] {
            s.on_event(SessionEvent::E2e(E2eEvent::Member {
                id: 2,
                name: "ola".into(),
                identity: ID,
                trust: Trust::Known,
                code: code.into(),
            }));
            assert_eq!(s.on_action(Action::VerifyMember { id: 2, group: group.into() }), vec![], "{code:?}");
            assert_eq!(s.view().notice.as_deref(), Some(WRONG_GROUP));
        }
    }

    #[test]
    fn accept_only_when_key_changed() {
        let mut s = online();
        s.on_event(e2e(2, "ola", Trust::Known));
        assert_eq!(s.on_action(Action::AcceptMember { id: 2, group: "3141".into() }), vec![]);
        assert!(s.view().notice.is_some());
        s.on_event(e2e(2, "ola", Trust::KeyChanged));
        assert_eq!(s.on_action(Action::VerifyMember { id: 2, group: "3141".into() }), vec![]);
        assert_eq!(s.on_action(Action::AcceptMember { id: 2, group: "9999".into() }), vec![]);
        assert_eq!(s.view().notice.as_deref(), Some(WRONG_GROUP));
        assert_eq!(
            s.on_action(Action::AcceptMember { id: 2, group: "3141".into() }),
            vec![Effect::Trust { verify: false, id: 2, name: "ola".into(), identity: ID }]
        );
    }

    #[test]
    fn names_are_sanitized() {
        let mut s = state();
        s.on_event(SessionEvent::Connected {
            you: 1,
            room: Some(10),
            rooms: vec![r(10, "Ran\u{202E}ked")],
            members: vec![m(1, "ja", Some(10)), m(2, "o\u{1b}[2Jla", Some(10))],
        });
        assert_eq!(room(&s, 10).name, "Ranked");
        assert_eq!(names(&s, 10), vec!["ja", "o[2Jla"]);
        // A known id keeps its first name.
        s.on_event(SessionEvent::MemberJoined(m(2, "ewa", Some(10))));
        assert_eq!(names(&s, 10), vec!["ja", "o[2Jla"]);
        s.on_event(chat(2, "o\u{1b}[2Jla", "hej", false));
        assert_eq!(s.view().chat[0].author, "o[2Jla");
        s.on_event(SessionEvent::ChatUnreadable { from: 2, name: "o\u{202E}la".into() });
        let line = &s.view().chat[1];
        assert!(line.system && line.author.is_empty());
        assert_eq!(line.text, "nie udało się odszyfrować wiadomości od ola (klucze jeszcze się wymieniają)");
        // The volume book is keyed by the shown name, the one the UI sends back.
        assert_eq!(
            s.on_action(Action::SetVolume { name: "o[2Jla".into(), percent: 10 }),
            vec![Effect::SetGain { id: 2, gain: 0.1 }, Effect::SaveSettings]
        );
    }

    #[test]
    fn view_is_unchanged_without_events() {
        let mut s = online();
        let v = s.view_version();
        assert!(s.on_activity(&[2], false));
        assert!(member(&s, 2).speaking);
        assert!(!member(&s, 1).speaking);
        assert_eq!(s.view_version(), v + 1);
        assert!(!s.on_activity(&[2], false));
        assert_eq!(s.view_version(), v + 1);
        assert!(s.on_activity(&[], true));
        assert!(member(&s, 1).speaking && !member(&s, 2).speaking);
        let v = s.view_version();
        s.on_action(Action::WindowShown);
        s.on_event(SessionEvent::RoomCreated(r(10, "Ogólny")));
        s.on_event(SessionEvent::MemberMoved { id: 1, room: Some(10) });
        assert_eq!(s.view_version(), v);
        s.on_event(chat(2, "ola", "hej", false));
        assert_eq!(s.view_version(), v + 1);
    }

    #[test]
    fn created_room_is_joined() {
        let mut s = online();
        assert_eq!(
            s.on_action(Action::CreateRoom("  Nowy ".into())),
            vec![Effect::Command(SessionCommand::CreateRoom("Nowy".into()))]
        );
        // Somebody else's room, even with the same name, is not mine to enter.
        let other = RoomInfo { id: 12, name: "Nowy".into(), creator: "ola".into() };
        assert_eq!(s.on_event(SessionEvent::RoomCreated(other)), vec![]);
        let mine = RoomInfo { id: 13, name: "Nowy".into(), creator: "ja".into() };
        assert_eq!(
            s.on_event(SessionEvent::RoomCreated(mine.clone())),
            vec![Effect::Command(SessionCommand::JoinRoom(13))]
        );
        // Only once.
        let again = RoomInfo { id: 14, ..mine.clone() };
        assert_eq!(s.on_event(SessionEvent::RoomCreated(again)), vec![]);
        // A refused name forgets the wish.
        s.on_action(Action::CreateRoom("Nowy".into()));
        s.on_event(SessionEvent::RoomError(RoomErrorReason::NameTaken));
        let later = RoomInfo { id: 15, ..mine };
        assert_eq!(s.on_event(SessionEvent::RoomCreated(later)), vec![]);
    }

    #[test]
    fn session_that_could_not_start_is_fatal() {
        let mut s = online();
        s.session_failed("nie udało się połączyć: brak sieci".into());
        assert_eq!(s.view().status, Status::Fatal("nie udało się połączyć: brak sieci".into()));
        assert!(s.view().rooms.is_empty());
        s.reset_session();
        assert_eq!(s.view().status, Status::Connecting);
    }

    #[test]
    fn reset_session_forgets_the_lobby() {
        let mut s = online();
        s.on_event(chat(2, "ola", "hej", false));
        s.on_action(Action::ToggleMute);
        s.reset_session();
        assert_eq!(s.view().status, Status::Connecting);
        assert!(s.view().rooms.is_empty());
        assert_eq!(s.view().my_room, None);
        assert_eq!(s.view().chat.len(), 1);
        assert!(s.muted());
    }

    #[test]
    fn chat_history_is_capped() {
        let mut s = online();
        for i in 0..600 {
            s.on_event(chat(2, "ola", &i.to_string(), false));
        }
        assert_eq!(s.view().chat.len(), 500);
        assert_eq!(s.view().chat[0].text, "100");
        assert_eq!(s.view().chat[499].text, "599");
    }

    #[test]
    fn polish_notices_for_room_errors_and_fatal() {
        use RoomErrorReason::*;
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
        let mut s = online();
        s.on_event(SessionEvent::RoomError(NameTaken));
        assert_eq!(s.view().notice.as_deref(), Some("nazwa zajęta"));
        s.on_event(SessionEvent::ChatNotSent(ChatSendError::NotInRoom));
        let line = s.view().chat.last().unwrap();
        assert!(line.system);
        assert_eq!(line.time, "09:05");
        assert_eq!(line.text, "nie wysłano wiadomości: nie jesteś w żadnym pokoju");
        assert_eq!(describe_chat_not_sent(ChatSendError::Empty), "pusta wiadomość");
        assert_eq!(describe_chat_not_sent(ChatSendError::TooLong), "za długa (limit 2000 znaków)");
        assert_eq!(describe_chat_not_sent(ChatSendError::NotConnected), "brak połączenia z serwerem");
        assert_eq!(trust_label(Trust::Verified), "zweryfikowany");
        assert_eq!(trust_label(Trust::KeyChanged), "ZMIENIONY KLUCZ");
        assert_eq!(describe_fatal(FatalReason::BadCredentials), "zły login lub hasło");
        assert_eq!(
            describe_fatal(FatalReason::VersionMismatch { server: 9 }),
            format!("niezgodna wersja programu (serwer: 9, Twoja: {PROTOCOL_VERSION}) — zaktualizuj komunikator")
        );
        assert_eq!(describe_fatal(FatalReason::SessionReplaced), "zalogowano się na to konto w innym miejscu");
        s.on_event(SessionEvent::Fatal(FatalReason::BadCredentials));
        assert_eq!(s.view().status, Status::Fatal("zły login lub hasło".into()));
    }

    #[test]
    fn fatal_texts_for_account_errors() {
        let all = [
            (FatalReason::LoginRequired, "sesja wygasła — zaloguj się ponownie"),
            (FatalReason::InviteInvalid, "kod zaproszenia jest nieważny lub już użyty"),
            (FatalReason::NameTaken, "ta nazwa jest już zajęta"),
            (FatalReason::NameInvalid, "nazwa: 1–32 znaki, litery, cyfry, _ - ."),
            (FatalReason::PasswordTooShort, "hasło musi mieć co najmniej 8 znaków"),
            (FatalReason::AccountBlocked, "konto zostało zablokowane przez administratora"),
        ];
        for (reason, text) in all {
            assert_eq!(describe_fatal(reason), text);
        }
        assert_eq!(protocol::MIN_PASSWORD_CHARS, 8, "the text names the limit");
        // A new token changes nothing on screen; the GUI stores it.
        let mut s = online();
        let before = s.view().clone();
        assert_eq!(s.on_event(SessionEvent::TokenIssued([9; 32])), vec![]);
        assert_eq!(*s.view(), before);
    }
}
