//! Membership of the single voice room. Generic over the connection type for testability.

use std::collections::BTreeMap;

use protocol::{MemberId, MemberInfo};

pub struct JoinOutcome<C> {
    pub id: MemberId,
    /// Everyone else already in the room.
    pub members: Vec<MemberInfo>,
    /// A previous session of the same user, now evicted.
    pub replaced: Option<(MemberId, C)>,
}

struct Member<C> {
    name: String,
    conn: C,
}

pub struct Room<C: Clone> {
    members: BTreeMap<MemberId, Member<C>>,
    pub(crate) next_id: MemberId,
}

impl<C: Clone> Default for Room<C> {
    fn default() -> Self {
        Self { members: BTreeMap::new(), next_id: 1 }
    }
}

impl<C: Clone> Room<C> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn join(&mut self, name: &str, conn: C) -> JoinOutcome<C> {
        let replaced = self
            .members
            .iter()
            .find(|(_, m)| m.name == name)
            .map(|(id, _)| *id)
            .and_then(|id| self.members.remove(&id).map(|m| (id, m.conn)));
        let id = self.allocate_id();
        let members = self
            .members
            .iter()
            .map(|(id, m)| MemberInfo { id: *id, name: m.name.clone() })
            .collect();
        self.members.insert(id, Member { name: name.to_owned(), conn });
        JoinOutcome { id, members, replaced }
    }

    pub fn leave(&mut self, id: MemberId) -> Option<MemberInfo> {
        self.members.remove(&id).map(|m| MemberInfo { id, name: m.name })
    }

    pub fn peers(&self, id: MemberId) -> Vec<C> {
        self.members
            .iter()
            .filter(|(other, _)| **other != id)
            .map(|(_, m)| m.conn.clone())
            .collect()
    }

    pub fn contains(&self, id: MemberId) -> bool {
        self.members.contains_key(&id)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(m: &[MemberInfo]) -> Vec<&str> {
        m.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn join_lists_others() {
        let mut r = Room::new();
        let a = r.join("ala", 1);
        assert!(a.members.is_empty());
        let b = r.join("ola", 2);
        assert_eq!(names(&b.members), ["ala"]);
        assert_eq!(b.members[0].id, a.id);
        assert!(b.replaced.is_none());
    }

    #[test]
    fn same_name_replaces_previous() {
        let mut r = Room::new();
        let first = r.join("ala", 1);
        let second = r.join("ala", 2);
        assert_eq!(second.replaced, Some((first.id, 1)));
        assert!(second.members.is_empty());
        assert!(!r.contains(first.id));
        assert!(r.contains(second.id));
    }

    #[test]
    fn leave_after_replace_is_noop() {
        let mut r = Room::new();
        let first = r.join("ala", 1);
        let second = r.join("ala", 2);
        assert_eq!(r.leave(first.id), None);
        assert!(r.contains(second.id));
    }

    #[test]
    fn peers_excludes_self() {
        let mut r = Room::new();
        let a = r.join("ala", 1);
        r.join("ola", 2);
        r.join("ewa", 3);
        let mut p = r.peers(a.id);
        p.sort();
        assert_eq!(p, [2, 3]);
    }

    #[test]
    fn ids_skip_zero_and_in_use() {
        let mut r = Room::new();
        r.next_id = u16::MAX;
        let a = r.join("ala", 1);
        assert_eq!(a.id, u16::MAX);
        let b = r.join("ola", 2);
        assert_eq!(b.id, 1);
        r.next_id = u16::MAX;
        let c = r.join("ewa", 3);
        assert!(c.id != 0 && c.id != a.id && c.id != b.id);
    }
}
