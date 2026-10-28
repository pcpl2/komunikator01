//! What the main window shows, as plain strings and diffs, kept apart from Win32 so it can be
//! tested.

use client_core::app::{ChatView, MemberView, RoomView, Status, View};
use client_core::e2e::Trust;
use protocol::{MemberId, RoomId};

use super::icons::TrayState;

/// The member menu's volume choices, in percent.
pub const VOLUME_STEPS: [u16; 8] = [0, 25, 50, 75, 100, 125, 150, 200];

pub fn mute_label(muted: bool) -> &'static str {
    if muted { "Mikrofon: WYCISZONY" } else { "Mikrofon: wł." }
}

pub fn deafen_label(deafened: bool) -> &'static str {
    if deafened { "Dźwięk: WYCISZONY" } else { "Dźwięk: wł." }
}

/// A member's tree line; trust marks are shown only for my room, the only one with keys.
pub fn member_text(m: &MemberView, in_my_room: bool) -> String {
    let mut s = String::from(if m.speaking { "🔊 " } else { "   " });
    s.push_str(&m.name);
    if m.me {
        s.push_str(" (Ty)");
    }
    if m.percent != 100 {
        s.push_str(&format!(" — {}%", m.percent));
    }
    if m.locally_muted {
        s.push_str(" — wyciszony u Ciebie");
    }
    let mark = match m.trust.filter(|_| in_my_room) {
        Some(Trust::NewUser) => " [nowa osoba]",
        Some(Trust::NewDevice) => " [NOWE URZĄDZENIE]",
        Some(Trust::KeyChanged) => " [ZMIENIONY KLUCZ]",
        Some(Trust::Verified) => " [zweryfikowany ✓]",
        Some(Trust::Known) | None => "",
    };
    s.push_str(mark);
    s
}

pub fn room_text(r: &RoomView) -> String {
    format!("{} ({})", r.name, r.members.len())
}

/// The list rows for one message: the first carries time and author, the rest are indented so
/// a message cannot pass itself off as another one.
pub fn chat_rows(c: &ChatView) -> Vec<String> {
    let mut lines = c.text.split('\n');
    let first = lines.next().unwrap_or_default();
    let head = if c.system {
        format!("[{}] {first}", c.time)
    } else {
        format!("[{}] {}: {first}", c.time, c.author)
    };
    std::iter::once(head).chain(lines.map(|l| format!("    {l}"))).collect()
}

/// How to turn the shown chat `old` into `new`: drop this many messages from the top, keep
/// the next `kept`, append the rest of `new`. Anything else is "drop all, append all".
pub fn chat_diff(old: &[ChatView], new: &[ChatView]) -> ChatDiff {
    // The model only appends and trims the front, so the shortest drop that fits is the one.
    (0..old.len())
        .find_map(|dropped| {
            let kept = old.len() - dropped;
            (kept <= new.len() && old[dropped..] == new[..kept]).then_some(ChatDiff { dropped, kept })
        })
        .unwrap_or(ChatDiff { dropped: old.len(), kept: 0 })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatDiff {
    pub dropped: usize,
    pub kept: usize,
}

/// Same rooms (ids and names) with the same members in the same order: only item texts change.
pub fn same_tree_shape(a: &[RoomView], b: &[RoomView]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.id == y.id
                && x.name == y.name
                && x.members.len() == y.members.len()
                && x.members.iter().zip(&y.members).all(|(m, n)| m.id == n.id)
        })
}

pub fn status_text(s: &Status) -> String {
    match s {
        Status::Connecting => "łączenie…".into(),
        Status::Connected => "połączono".into(),
        Status::Disconnected { retry_in_s } => format!("rozłączono, ponowna próba za {retry_in_s} s"),
        Status::Fatal(msg) => format!("błąd: {msg}"),
    }
}

pub fn tray_state(v: &View) -> TrayState {
    if v.status != Status::Connected {
        TrayState::Disconnected
    } else if v.deafened {
        TrayState::Deafened
    } else if v.muted {
        TrayState::Muted
    } else {
        TrayState::Connected
    }
}

pub fn tray_tip(v: &View) -> &'static str {
    match tray_state(v) {
        TrayState::Disconnected => "Komunikator — rozłączono",
        TrayState::Muted | TrayState::Deafened => "Komunikator — wyciszony",
        TrayState::Connected => "Komunikator — połączono",
    }
}

/// What a tree item stands for, packed into its lParam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeItem {
    Room(RoomId),
    Member(MemberId),
}

impl TreeItem {
    pub fn to_param(self) -> isize {
        match self {
            TreeItem::Room(id) => (1 << 32) | id as isize,
            TreeItem::Member(id) => (2 << 32) | id as isize,
        }
    }

    pub fn from_param(p: isize) -> Option<TreeItem> {
        let id = (p & 0xFFFF_FFFF) as u32;
        match p >> 32 {
            1 => Some(TreeItem::Room(id)),
            2 => MemberId::try_from(id).ok().map(TreeItem::Member),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str) -> MemberView {
        MemberView {
            id: 2,
            name: name.into(),
            me: false,
            speaking: false,
            percent: 100,
            locally_muted: false,
            trust: None,
            code: None,
        }
    }

    fn line(text: &str) -> ChatView {
        ChatView {
            time: "09:05".into(),
            author: "ola".into(),
            text: text.into(),
            own: false,
            links: Vec::new(),
            system: false,
        }
    }

    fn view(status: Status) -> View {
        View {
            status,
            rooms: Vec::new(),
            my_room: None,
            chat: Vec::new(),
            unread: 0,
            muted: false,
            deafened: false,
            notice: None,
        }
    }

    #[test]
    fn toggle_labels() {
        assert_eq!(mute_label(false), "Mikrofon: wł.");
        assert_eq!(mute_label(true), "Mikrofon: WYCISZONY");
        assert_eq!(deafen_label(false), "Dźwięk: wł.");
        assert_eq!(deafen_label(true), "Dźwięk: WYCISZONY");
    }

    #[test]
    fn member_text_marks() {
        let mut m = member("ola");
        assert_eq!(member_text(&m, true), "   ola");
        m.speaking = true;
        assert_eq!(member_text(&m, true), "🔊 ola");
        m.percent = 150;
        m.locally_muted = true;
        m.trust = Some(Trust::KeyChanged);
        assert_eq!(member_text(&m, true), "🔊 ola — 150% — wyciszony u Ciebie [ZMIENIONY KLUCZ]");
        // Outside my room there are no keys, so no trust mark.
        assert_eq!(member_text(&m, false), "🔊 ola — 150% — wyciszony u Ciebie");

        let mut m = member("ola");
        for (t, mark) in [
            (Trust::NewUser, "   ola [nowa osoba]"),
            (Trust::NewDevice, "   ola [NOWE URZĄDZENIE]"),
            (Trust::Verified, "   ola [zweryfikowany ✓]"),
            (Trust::Known, "   ola"),
        ] {
            m.trust = Some(t);
            assert_eq!(member_text(&m, true), mark);
        }

        let mut me = member("ja");
        me.me = true;
        assert_eq!(member_text(&me, true), "   ja (Ty)");
    }

    #[test]
    fn room_text_counts_members() {
        let r = RoomView { id: 1, name: "Ogólny".into(), members: vec![member("a"), member("b"), member("c")] };
        assert_eq!(room_text(&r), "Ogólny (3)");
    }

    #[test]
    fn chat_rows_format() {
        assert_eq!(chat_rows(&line("cześć")), ["[09:05] ola: cześć"]);
        assert_eq!(chat_rows(&line("a\nb\nc")), ["[09:05] ola: a", "    b", "    c"]);
        let mut sys = line("rozłączono");
        sys.system = true;
        sys.author = String::new();
        assert_eq!(chat_rows(&sys), ["[09:05] rozłączono"]);
    }

    #[test]
    fn chat_diff_appends_and_trims() {
        let a = [line("1"), line("2"), line("3")];
        assert_eq!(chat_diff(&a, &a), ChatDiff { dropped: 0, kept: 3 });
        assert_eq!(chat_diff(&a[..2], &a), ChatDiff { dropped: 0, kept: 2 });
        assert_eq!(chat_diff(&[], &a), ChatDiff { dropped: 0, kept: 0 });
        // The oldest message fell off the front while a new one arrived.
        let b = [line("2"), line("3"), line("4")];
        assert_eq!(chat_diff(&a, &b), ChatDiff { dropped: 1, kept: 2 });
        // Everything replaced: drop all, keep nothing.
        assert_eq!(chat_diff(&a, &[line("x")]), ChatDiff { dropped: 3, kept: 0 });
        assert_eq!(chat_diff(&a, &[]), ChatDiff { dropped: 3, kept: 0 });
    }

    #[test]
    fn tree_shape_ignores_member_details() {
        let room = |members| RoomView { id: 1, name: "Ogólny".into(), members };
        let a = [room(vec![member("ola")])];
        let mut speaking = member("ola");
        speaking.speaking = true;
        speaking.percent = 50;
        assert!(same_tree_shape(&a, &[room(vec![speaking])]));
        let mut other = member("ola");
        other.id = 3;
        assert!(!same_tree_shape(&a, &[room(vec![other])]));
        assert!(!same_tree_shape(&a, &[room(vec![member("ola"), member("ela")])]));
        assert!(!same_tree_shape(&a, &[RoomView { id: 1, name: "Inny".into(), members: vec![member("ola")] }]));
        assert!(!same_tree_shape(&a, &[]));
    }

    #[test]
    fn status_and_tray() {
        assert_eq!(status_text(&Status::Connecting), "łączenie…");
        assert_eq!(status_text(&Status::Connected), "połączono");
        assert_eq!(status_text(&Status::Disconnected { retry_in_s: 3 }), "rozłączono, ponowna próba za 3 s");
        assert_eq!(status_text(&Status::Fatal("zły login lub hasło".into())), "błąd: zły login lub hasło");

        let mut v = view(Status::Connected);
        assert_eq!((tray_state(&v), tray_tip(&v)), (TrayState::Connected, "Komunikator — połączono"));
        v.muted = true;
        assert_eq!((tray_state(&v), tray_tip(&v)), (TrayState::Muted, "Komunikator — wyciszony"));
        v.deafened = true;
        assert_eq!((tray_state(&v), tray_tip(&v)), (TrayState::Deafened, "Komunikator — wyciszony"));
        let v = view(Status::Disconnected { retry_in_s: 1 });
        assert_eq!((tray_state(&v), tray_tip(&v)), (TrayState::Disconnected, "Komunikator — rozłączono"));
        let v = view(Status::Connecting);
        assert_eq!(tray_state(&v), TrayState::Disconnected);
    }

    #[test]
    fn tree_item_param_roundtrip() {
        for item in [TreeItem::Room(0), TreeItem::Room(u32::MAX), TreeItem::Member(7), TreeItem::Member(u16::MAX)] {
            assert_eq!(TreeItem::from_param(item.to_param()), Some(item));
        }
        assert_eq!(TreeItem::from_param(0), None);
    }
}
