//! Persistent room list. Chat content is never stored, only room names and creators.

use std::path::Path;

use protocol::{RoomId, RoomInfo, DEFAULT_ROOM_NAME};
use rusqlite::{params, Connection, ErrorCode};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("room name already taken")]
    NameTaken,
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

/// Callers serialize access (the connection is not shared across threads).
pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let db = Connection::open(path)?;
        // AUTOINCREMENT so ids of deleted rooms are never handed out again.
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS rooms(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                creator TEXT NOT NULL
            )",
        )?;
        let count: i64 = db.query_row("SELECT COUNT(*) FROM rooms", [], |r| r.get(0))?;
        if count == 0 {
            db.execute("INSERT INTO rooms(name, creator) VALUES (?1, '')", [DEFAULT_ROOM_NAME])?;
        }
        Ok(Store { db })
    }

    pub fn rooms(&self) -> anyhow::Result<Vec<RoomInfo>> {
        let mut stmt = self.db.prepare("SELECT id, name, creator FROM rooms ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(RoomInfo { id: r.get(0)?, name: r.get(1)?, creator: r.get(2)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn create_room(&self, name: &str, creator: &str) -> Result<RoomInfo, StoreError> {
        match self
            .db
            .execute("INSERT INTO rooms(name, creator) VALUES (?1, ?2)", params![name, creator])
        {
            Ok(_) => Ok(RoomInfo {
                id: self.db.last_insert_rowid() as RoomId,
                name: name.to_string(),
                creator: creator.to_string(),
            }),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::ConstraintViolation => {
                Err(StoreError::NameTaken)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Returns whether a room with this id existed.
    pub fn delete_room(&self, id: RoomId) -> anyhow::Result<bool> {
        Ok(self.db.execute("DELETE FROM rooms WHERE id = ?1", [id])? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_in(dir: &tempfile::TempDir) -> Store {
        Store::open(&dir.path().join("komunikator.db")).unwrap()
    }

    #[test]
    fn fresh_store_has_default_room() {
        let dir = tempfile::tempdir().unwrap();
        let rooms = open_in(&dir).rooms().unwrap();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].name, "Ogólny");
        assert_eq!(rooms[0].creator, "");
    }

    #[test]
    fn rooms_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        store.create_room("Gra", "ala").unwrap();
        drop(store);
        let rooms = open_in(&dir).rooms().unwrap();
        assert_eq!(rooms.len(), 2);
        assert_eq!(rooms[1].name, "Gra");
        assert_eq!(rooms[1].creator, "ala");
    }

    #[test]
    fn duplicate_name_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        store.create_room("Gra", "ala").unwrap();
        assert!(matches!(store.create_room("gra", "ola"), Err(StoreError::NameTaken)));
    }

    #[test]
    fn deleted_ids_are_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        let a = store.create_room("A", "ala").unwrap();
        assert!(store.delete_room(a.id).unwrap());
        let b = store.create_room("B", "ala").unwrap();
        assert!(b.id > a.id);
        assert!(!store.delete_room(a.id).unwrap());
    }
}
