//! Who is online and which voice room each member is in. Generic over the connection
//! type for testability.

use std::collections::BTreeMap;

use protocol::{
    normalize_room_name, MemberId, MemberInfo, RoomChoice, RoomErrorReason, RoomId, RoomInfo,
    MAX_ROOMS, MAX_ROOM_MEMBERS,
};

pub struct LoginOutcome<C> {
    pub id: MemberId,
    /// Where the member landed; `None` for no room.
    pub room: Option<RoomId>,
    /// A previous session of the same user, now evicted.
    pub replaced: Option<(MemberId, C)>,
}

struct Member<C> {
    name: String,
    room: Option<RoomId>,
    conn: C,
}

pub struct Registry<C: Clone> {
    rooms: BTreeMap<RoomId, RoomInfo>,
    members: BTreeMap<MemberId, Member<C>>,
    pub(crate) next_id: MemberId,
}

impl<C: Clone> Registry<C> {
    pub fn new(rooms: Vec<RoomInfo>) -> Self {
        Self {
            rooms: rooms.into_iter().map(|r| (r.id, r)).collect(),
            members: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub fn login(&mut self, name: &str, conn: C, choice: RoomChoice) -> LoginOutcome<C> {
        let replaced = self
            .members
            .iter()
            .find(|(_, m)| m.name == name)
            .map(|(id, _)| *id)
            .and_then(|id| self.members.remove(&id).map(|m| (id, m.conn)));
        let room = match choice {
            RoomChoice::Room(r) if self.rooms.contains_key(&r) && !self.is_full(r) => Some(r),
            RoomChoice::Room(_) | RoomChoice::Default => self.default_room(),
            RoomChoice::Nowhere => None,
        };
        let id = self.allocate_id();
        self.members.insert(id, Member { name: name.to_owned(), room, conn });
        LoginOutcome { id, room, replaced }
    }

    pub fn snapshot(&self) -> (Vec<RoomInfo>, Vec<MemberInfo>) {
        let rooms = self.rooms.values().cloned().collect();
        let members = self.members.iter().map(|(id, m)| info(*id, m)).collect();
        (rooms, members)
    }

    pub fn leave(&mut self, id: MemberId) -> Option<MemberInfo> {
        self.members.remove(&id).map(|m| info(id, &m))
    }

    /// `Ok(false)` when nothing changed (already there, or not online).
    pub fn move_to(&mut self, id: MemberId, room: Option<RoomId>) -> Result<bool, RoomErrorReason> {
        let Some(current) = self.members.get(&id).map(|m| m.room) else { return Ok(false) };
        if current == room {
            return Ok(false);
        }
        if let Some(r) = room {
            if !self.rooms.contains_key(&r) {
                return Err(RoomErrorReason::NotFound);
            }
            if self.is_full(r) {
                return Err(RoomErrorReason::Full);
            }
        }
        if let Some(m) = self.members.get_mut(&id) {
            m.room = room;
        }
        Ok(true)
    }

    /// The others in my room; empty when I am in none.
    pub fn room_peers(&self, id: MemberId) -> Vec<(MemberId, C)> {
        let Some(room) = self.room_of(id) else { return Vec::new() };
        self.members
            .iter()
            .filter(|(other, m)| **other != id && m.room == Some(room))
            .map(|(other, m)| (*other, m.conn.clone()))
            .collect()
    }

    /// Everyone online but `id`.
    pub fn others(&self, id: MemberId) -> Vec<C> {
        self.members
            .iter()
            .filter(|(other, _)| **other != id)
            .map(|(_, m)| m.conn.clone())
            .collect()
    }

    /// The room `id` is in; `None` when in no room or not online.
    pub fn room_of(&self, id: MemberId) -> Option<RoomId> {
        self.members.get(&id).and_then(|m| m.room)
    }

    /// The normalized name, if a room by it may be created.
    pub fn check_new_room(&self, name: &str) -> Result<String, RoomErrorReason> {
        let name = normalize_room_name(name).ok_or(RoomErrorReason::InvalidName)?;
        let lower = name.to_lowercase();
        if self.rooms.values().any(|r| r.name.to_lowercase() == lower) {
            return Err(RoomErrorReason::NameTaken);
        }
        if self.rooms.len() >= MAX_ROOMS {
            return Err(RoomErrorReason::TooManyRooms);
        }
        Ok(name)
    }

    pub fn add_room(&mut self, room: RoomInfo) {
        self.rooms.insert(room.id, room);
    }

    pub fn check_delete(&self, room: RoomId, by: &str, admin: bool) -> Result<(), RoomErrorReason> {
        let info = self.rooms.get(&room).ok_or(RoomErrorReason::NotFound)?;
        if info.creator != by && !admin {
            return Err(RoomErrorReason::NotAllowed);
        }
        if self.members.values().any(|m| m.room == Some(room)) {
            return Err(RoomErrorReason::NotEmpty);
        }
        Ok(())
    }

    pub fn remove_room(&mut self, room: RoomId) {
        self.rooms.remove(&room);
    }

    pub fn room_ids(&self) -> Vec<RoomId> {
        self.rooms.keys().copied().collect()
    }

    pub fn has_room(&self, room: RoomId) -> bool {
        self.rooms.contains_key(&room)
    }

    pub fn members_in(&self, room: RoomId) -> Vec<MemberId> {
        self.members.iter().filter(|(_, m)| m.room == Some(room)).map(|(id, _)| *id).collect()
    }

    /// Everyone online.
    pub fn everyone(&self) -> Vec<C> {
        self.members.values().map(|m| m.conn.clone()).collect()
    }

    pub fn get_mut(&mut self, id: MemberId) -> Option<&mut C> {
        self.members.get_mut(&id).map(|m| &mut m.conn)
    }

    pub fn get(&self, id: MemberId) -> Option<&C> {
        self.members.get(&id).map(|m| &m.conn)
    }

    pub fn contains(&self, id: MemberId) -> bool {
        self.members.contains_key(&id)
    }

    /// The lowest room id, unless that room is full.
    fn default_room(&self) -> Option<RoomId> {
        let (&id, _) = self.rooms.first_key_value()?;
        (!self.is_full(id)).then_some(id)
    }

    fn is_full(&self, room: RoomId) -> bool {
        self.members.values().filter(|m| m.room == Some(room)).count() >= MAX_ROOM_MEMBERS
    }

    fn allocate_id(&mut self) -> MemberId {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if id != 0 && !self.members.contains_key(&id) {
                return id;
            }
        }
    }
}

fn info<C>(id: MemberId, m: &Member<C>) -> MemberInfo {
    MemberInfo { id, name: m.name.clone(), room: m.room }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: RoomId, name: &str, creator: &str) -> RoomInfo {
        RoomInfo { id, name: name.into(), creator: creator.into() }
    }

    /// Default room 1 ("Ogólny") plus "Gra" (5, by ala).
    fn registry() -> Registry<u32> {
        Registry::new(vec![room(5, "Gra", "ala"), room(1, "Ogólny", "")])
    }

    #[test]
    fn login_default_goes_to_lowest_room() {
        let mut r = registry();
        let a = r.login("ala", 1, RoomChoice::Default);
        assert_eq!(a.room, Some(1));
        let (rooms, members) = r.snapshot();
        assert_eq!(rooms.iter().map(|r| r.id).collect::<Vec<_>>(), [1, 5]);
        assert_eq!(members, [MemberInfo { id: a.id, name: "ala".into(), room: Some(1) }]);
    }

    #[test]
    fn missing_room_choice_falls_back_to_default() {
        let mut r = registry();
        assert_eq!(r.login("ala", 1, RoomChoice::Room(5)).room, Some(5));
        assert_eq!(r.login("ola", 2, RoomChoice::Room(99)).room, Some(1));
    }

    #[test]
    fn nowhere_has_no_room_peers() {
        let mut r = registry();
        let a = r.login("ala", 1, RoomChoice::Nowhere);
        let b = r.login("ola", 2, RoomChoice::Nowhere);
        assert_eq!(a.room, None);
        assert!(r.room_peers(a.id).is_empty());
        assert!(r.room_peers(b.id).is_empty());
        assert_eq!(r.others(a.id), [2]);
    }

    #[test]
    fn room_peers_are_same_room_only() {
        let mut r = registry();
        let a = r.login("ala", 1, RoomChoice::Default);
        let b = r.login("ola", 2, RoomChoice::Default);
        r.login("ewa", 3, RoomChoice::Room(5));
        assert_eq!(r.room_peers(a.id), [(b.id, 2)]);
        let mut others = r.others(a.id);
        others.sort();
        assert_eq!(others, [2, 3]);
    }

    #[test]
    fn move_to_unknown_room_is_not_found() {
        let mut r = registry();
        let a = r.login("ala", 1, RoomChoice::Default);
        assert_eq!(r.move_to(a.id, Some(99)), Err(RoomErrorReason::NotFound));
        assert_eq!(r.move_to(a.id, Some(1)), Ok(false));
        assert_eq!(r.move_to(a.id, Some(5)), Ok(true));
        assert_eq!(r.move_to(a.id, None), Ok(true));
        assert_eq!(r.move_to(a.id, None), Ok(false));
    }

    #[test]
    fn full_room_refuses() {
        let mut r = registry();
        for i in 0..MAX_ROOM_MEMBERS as u32 {
            let m = r.login(&format!("u{i}"), i, RoomChoice::Nowhere);
            assert_eq!(r.move_to(m.id, Some(5)), Ok(true));
        }
        let late = r.login("late", 99, RoomChoice::Nowhere);
        assert_eq!(r.move_to(late.id, Some(5)), Err(RoomErrorReason::Full));
        // A full requested room falls back to the default one.
        assert_eq!(r.login("other", 100, RoomChoice::Room(5)).room, Some(1));
    }

    #[test]
    fn room_names_are_unique_ignoring_case() {
        let r = registry();
        assert_eq!(r.check_new_room("OGÓLNY"), Err(RoomErrorReason::NameTaken));
        assert_eq!(r.check_new_room("  "), Err(RoomErrorReason::InvalidName));
        assert_eq!(r.check_new_room("  Nowy "), Ok("Nowy".to_string()));
    }

    #[test]
    fn too_many_rooms() {
        let mut r = registry();
        for id in 10..10 + MAX_ROOMS as u32 - 2 {
            r.add_room(room(id, &format!("p{id}"), "ala"));
        }
        assert_eq!(r.check_new_room("ostatni"), Err(RoomErrorReason::TooManyRooms));
        r.remove_room(10);
        assert!(r.check_new_room("ostatni").is_ok());
    }

    #[test]
    fn delete_rules() {
        let mut r = registry();
        assert_eq!(r.check_delete(99, "ala", false), Err(RoomErrorReason::NotFound));
        assert_eq!(r.check_delete(5, "ola", false), Err(RoomErrorReason::NotAllowed));
        assert_eq!(r.check_delete(5, "ala", false), Ok(()));
        assert_eq!(r.check_delete(5, "ewa", true), Ok(()));
        let m = r.login("ola", 2, RoomChoice::Room(5));
        assert_eq!(r.check_delete(5, "ala", false), Err(RoomErrorReason::NotEmpty));
        r.move_to(m.id, None).unwrap();
        r.remove_room(5);
        assert_eq!(r.snapshot().0.len(), 1);
    }

    #[test]
    fn same_name_login_replaces_previous() {
        let mut r = registry();
        let first = r.login("ala", 1, RoomChoice::Default);
        let second = r.login("ala", 2, RoomChoice::Default);
        assert_eq!(second.replaced, Some((first.id, 1)));
        assert!(!r.contains(first.id));
        assert!(r.contains(second.id));
        assert_eq!(r.leave(first.id), None);
        assert_eq!(r.snapshot().1.len(), 1);
    }

    #[test]
    fn ids_skip_zero_and_in_use() {
        let mut r = registry();
        r.next_id = u16::MAX;
        let a = r.login("ala", 1, RoomChoice::Default);
        assert_eq!(a.id, u16::MAX);
        let b = r.login("ola", 2, RoomChoice::Default);
        assert_eq!(b.id, 1);
        r.next_id = u16::MAX;
        let c = r.login("ewa", 3, RoomChoice::Default);
        assert!(c.id != 0 && c.id != a.id && c.id != b.id);
    }
}
