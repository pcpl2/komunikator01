//! Who is online and which rooms exist, as seen by one client. Sans-IO: fed with the
//! server's lobby events, it reports what changed in *my* room so callers can react
//! (rekey, mixer routing, chat roster) without re-deriving it.

use std::collections::BTreeMap;

use protocol::{MemberId, MemberInfo, RoomId, RoomInfo};

#[derive(Clone, Debug)]
pub struct Lobby {
    you: MemberId,
    room: Option<RoomId>,
    rooms: BTreeMap<RoomId, RoomInfo>,
    members: BTreeMap<MemberId, MemberInfo>,
}

/// A change of my own room's population, or of which room I am in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomChange {
    /// I am now in `room`; `members` are the others already there.
    Entered { room: RoomId, members: Vec<MemberInfo> },
    Exited,
    Joined(MemberInfo),
    Left(MemberId),
}

impl Lobby {
    pub fn new(
        you: MemberId,
        room: Option<RoomId>,
        rooms: Vec<RoomInfo>,
        members: Vec<MemberInfo>,
    ) -> (Lobby, Option<RoomChange>) {
        let mut lobby = Lobby {
            you,
            room,
            rooms: rooms.into_iter().map(|r| (r.id, r)).collect(),
            members: members.into_iter().map(|m| (m.id, m)).collect(),
        };
        // Keep my own record consistent with the room the server put me in.
        if let Some(me) = lobby.members.get_mut(&you) {
            me.room = room;
        }
        let entered = room.map(|room| lobby.entered(room));
        (lobby, entered)
    }

    pub fn me(&self) -> MemberId {
        self.you
    }

    pub fn my_room(&self) -> Option<RoomId> {
        self.room
    }

    /// Rooms in ascending id order.
    pub fn rooms(&self) -> impl Iterator<Item = &RoomInfo> {
        self.rooms.values()
    }

    /// Members (including me) currently in `room`; `None` means online but in no room.
    pub fn members_in(&self, room: Option<RoomId>) -> Vec<&MemberInfo> {
        self.members.values().filter(|m| m.room == room).collect()
    }

    pub fn name_of(&self, id: MemberId) -> Option<&str> {
        self.members.get(&id).map(|m| m.name.as_str())
    }

    pub fn member_joined(&mut self, m: MemberInfo) -> Vec<RoomChange> {
        let id = m.id;
        let room = m.room;
        let previous = self.members.insert(id, m.clone());
        if id == self.you {
            // Only reachable if the server echoes me back; my room is tracked by moves.
            return Vec::new();
        }
        let was_here = previous.is_some_and(|p| p.room.is_some() && p.room == self.room);
        let is_here = room.is_some() && room == self.room;
        match (was_here, is_here) {
            (false, true) => vec![RoomChange::Joined(m)],
            (true, false) => vec![RoomChange::Left(id)],
            _ => Vec::new(),
        }
    }

    pub fn member_left(&mut self, id: MemberId) -> Vec<RoomChange> {
        let Some(gone) = self.members.remove(&id) else {
            return Vec::new();
        };
        if id != self.you && gone.room.is_some() && gone.room == self.room {
            vec![RoomChange::Left(id)]
        } else {
            Vec::new()
        }
    }

    pub fn member_moved(&mut self, id: MemberId, room: Option<RoomId>) -> Vec<RoomChange> {
        if id == self.you {
            return self.move_me(room);
        }
        // An unknown member has no name to show, so only known ones are tracked.
        let Some(member) = self.members.get_mut(&id) else {
            return Vec::new();
        };
        let before = member.room;
        member.room = room;
        let was_here = before.is_some() && before == self.room;
        let is_here = room.is_some() && room == self.room;
        match (was_here, is_here) {
            (false, true) => vec![RoomChange::Joined(member.clone())],
            (true, false) => vec![RoomChange::Left(id)],
            _ => Vec::new(),
        }
    }

    pub fn room_created(&mut self, r: RoomInfo) -> Vec<RoomChange> {
        self.rooms.insert(r.id, r);
        Vec::new()
    }

    /// The server never deletes an occupied room; if it does anyway and I am in it, that
    /// counts as me leaving it.
    pub fn room_deleted(&mut self, id: RoomId) -> Vec<RoomChange> {
        self.rooms.remove(&id);
        if self.room == Some(id) {
            self.move_me(None)
        } else {
            Vec::new()
        }
    }

    fn move_me(&mut self, room: Option<RoomId>) -> Vec<RoomChange> {
        let mut out = Vec::new();
        if self.room.is_some() {
            out.push(RoomChange::Exited);
        }
        self.room = room;
        if let Some(me) = self.members.get_mut(&self.you) {
            me.room = room;
        }
        if let Some(room) = room {
            out.push(self.entered(room));
        }
        out
    }

    fn entered(&self, room: RoomId) -> RoomChange {
        let members = self
            .members
            .values()
            .filter(|m| m.id != self.you && m.room == Some(room))
            .cloned()
            .collect();
        RoomChange::Entered { room, members }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: MemberId, name: &str, room: Option<RoomId>) -> MemberInfo {
        MemberInfo { id, name: name.into(), room }
    }

    fn r(id: RoomId, name: &str) -> RoomInfo {
        RoomInfo { id, name: name.into(), creator: String::new() }
    }

    fn lobby() -> Lobby {
        let (l, _) = Lobby::new(
            1,
            Some(10),
            vec![r(10, "Ogólny"), r(11, "Ranked")],
            vec![
                m(1, "ja", Some(10)),
                m(2, "ola", Some(10)),
                m(3, "ala", Some(11)),
                m(4, "ela", None),
            ],
        );
        l
    }

    #[test]
    fn new_enters_starting_room() {
        let (l, ch) = Lobby::new(
            1,
            Some(10),
            vec![r(10, "Ogólny")],
            vec![m(1, "ja", Some(10)), m(2, "ola", Some(10)), m(3, "ala", None)],
        );
        assert_eq!(ch, Some(RoomChange::Entered { room: 10, members: vec![m(2, "ola", Some(10))] }));
        assert_eq!(l.my_room(), Some(10));
        assert_eq!(l.me(), 1);
    }

    #[test]
    fn new_in_no_room_emits_nothing() {
        let (l, ch) = Lobby::new(1, None, vec![r(10, "Ogólny")], vec![m(1, "ja", None)]);
        assert_eq!(ch, None);
        assert_eq!(l.my_room(), None);
    }

    #[test]
    fn other_moving_in_and_out_of_my_room() {
        let mut l = lobby();
        assert_eq!(l.member_moved(3, Some(10)), vec![RoomChange::Joined(m(3, "ala", Some(10)))]);
        assert_eq!(l.member_moved(3, Some(11)), vec![RoomChange::Left(3)]);
        assert_eq!(l.member_moved(3, None), vec![]);
        // Moves between other rooms are silent.
        assert_eq!(l.member_moved(4, Some(11)), vec![]);
        assert_eq!(l.members_in(Some(11)).len(), 1);
    }

    #[test]
    fn my_move_exits_then_enters() {
        let mut l = lobby();
        assert_eq!(
            l.member_moved(1, Some(11)),
            vec![
                RoomChange::Exited,
                RoomChange::Entered { room: 11, members: vec![m(3, "ala", Some(11))] },
            ]
        );
        assert_eq!(l.my_room(), Some(11));
    }

    #[test]
    fn my_leave_only_exits() {
        let mut l = lobby();
        assert_eq!(l.member_moved(1, None), vec![RoomChange::Exited]);
        assert_eq!(l.my_room(), None);
        assert_eq!(l.member_moved(1, None), vec![]);
        assert_eq!(
            l.member_moved(1, Some(10)),
            vec![RoomChange::Entered { room: 10, members: vec![m(2, "ola", Some(10))] }]
        );
    }

    #[test]
    fn member_online_in_other_room_is_silent() {
        let mut l = lobby();
        assert_eq!(l.member_joined(m(5, "ula", Some(11))), vec![]);
        assert_eq!(l.member_joined(m(6, "ada", None)), vec![]);
        assert_eq!(
            l.member_joined(m(7, "iza", Some(10))),
            vec![RoomChange::Joined(m(7, "iza", Some(10)))]
        );
        assert_eq!(l.name_of(5), Some("ula"));
        assert_eq!(l.name_of(99), None);
    }

    #[test]
    fn offline_member_in_my_room_left() {
        let mut l = lobby();
        assert_eq!(l.member_left(2), vec![RoomChange::Left(2)]);
        assert_eq!(l.member_left(3), vec![]);
        assert_eq!(l.member_left(99), vec![]);
        assert_eq!(l.name_of(2), None);
    }

    #[test]
    fn rooms_created_and_deleted() {
        let mut l = lobby();
        assert_eq!(l.room_created(r(5, "Nowy")), vec![]);
        assert_eq!(l.rooms().map(|r| r.id).collect::<Vec<_>>(), vec![5, 10, 11]);
        assert_eq!(l.room_deleted(5), vec![]);
        assert_eq!(l.rooms().map(|r| r.id).collect::<Vec<_>>(), vec![10, 11]);
        // Deleting my own room (server should never) counts as leaving it.
        assert_eq!(l.room_deleted(10), vec![RoomChange::Exited]);
        assert_eq!(l.my_room(), None);
    }

    #[test]
    fn odd_events_never_panic() {
        let mut l = lobby();
        assert_eq!(l.member_joined(m(1, "ja", Some(11))), vec![]);
        assert_eq!(l.member_moved(99, Some(10)), vec![]);
        assert_eq!(l.room_deleted(777), vec![]);
    }
}
