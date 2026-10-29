//! Persistent rooms and accounts. Chat content is never stored. Invite codes and session
//! tokens are kept only as SHA-256 hashes.

use std::path::Path;
use std::time::Duration;

use protocol::{RoomId, RoomInfo, DEFAULT_ROOM_NAME};
use rusqlite::{params, Connection, ErrorCode, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::config::UserEntry;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("room name already taken")]
    NameTaken,
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("invite is invalid, used or expired")]
    InviteInvalid,
    #[error("user name already taken")]
    NameTaken,
    #[error(transparent)]
    Db(anyhow::Error),
}

impl From<rusqlite::Error> for RegisterError {
    fn from(e: rusqlite::Error) -> Self {
        RegisterError::Db(e.into())
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub hash: String,
    pub blocked: bool,
    pub admin: bool,
    pub created: u64,
}

/// What the admin sees about an invite; the code itself is not recoverable.
#[derive(Debug, Clone)]
pub struct InviteInfo {
    pub created: u64,
    pub expires: u64,
    pub used_by: Option<String>,
}

fn digest(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// Accounts are looked up case-insensitively, so "Ala" and "ala" are one person.
fn key(name: &str) -> String {
    name.to_lowercase()
}

fn account_row(r: &rusqlite::Row) -> rusqlite::Result<Account> {
    Ok(Account {
        name: r.get(0)?,
        hash: r.get(1)?,
        blocked: r.get(2)?,
        admin: r.get(3)?,
        created: r.get::<_, i64>(4)? as u64,
    })
}

/// Callers serialize access (the connection is not shared across threads).
pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        // AUTOINCREMENT so ids of deleted rooms are never handed out again.
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS rooms(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                creator TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS accounts(
                name TEXT NOT NULL,
                name_key TEXT PRIMARY KEY,
                hash TEXT NOT NULL,
                blocked INTEGER NOT NULL DEFAULT 0,
                admin INTEGER NOT NULL DEFAULT 0,
                created INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS invites(
                code_hash BLOB PRIMARY KEY,
                created INTEGER NOT NULL,
                expires INTEGER NOT NULL,
                used_by TEXT
            );
            CREATE TABLE IF NOT EXISTS tokens(
                token_hash BLOB PRIMARY KEY,
                account_key TEXT NOT NULL,
                expires INTEGER NOT NULL
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

    /// Runs `f` in one write transaction (taken up front so concurrent writers queue instead of
    /// failing at commit); rolls back if `f` fails.
    fn immediate<T, E: From<rusqlite::Error>>(
        &self,
        f: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        match f() {
            Ok(v) => {
                self.db.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    pub fn account(&self, name: &str) -> anyhow::Result<Option<Account>> {
        Ok(self
            .db
            .query_row(
                "SELECT name, hash, blocked, admin, created FROM accounts WHERE name_key = ?1",
                [key(name)],
                account_row,
            )
            .optional()?)
    }

    pub fn accounts(&self) -> anyhow::Result<Vec<Account>> {
        let mut stmt = self
            .db
            .prepare("SELECT name, hash, blocked, admin, created FROM accounts ORDER BY name_key")?;
        let rows = stmt.query_map([], account_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn create_invite(&self, code: &[u8; 16], now: u64, ttl_s: u64) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT INTO invites(code_hash, created, expires) VALUES (?1, ?2, ?3)",
            params![digest(code), now as i64, now.saturating_add(ttl_s) as i64],
        )?;
        Ok(())
    }

    /// Consumes the invite and creates the account atomically; a failed attempt (for example a
    /// taken name) leaves the invite usable.
    pub fn register(
        &self,
        code: &[u8; 16],
        name: &str,
        hash: &str,
        now: u64,
    ) -> Result<Account, RegisterError> {
        self.immediate(|| {
            let code_hash = digest(code);
            let invite: Option<(i64, Option<String>)> = self
                .db
                .query_row(
                    "SELECT expires, used_by FROM invites WHERE code_hash = ?1",
                    [&code_hash],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match invite {
                Some((expires, None)) if (now as i64) < expires => {}
                _ => return Err(RegisterError::InviteInvalid),
            }
            match self.db.execute(
                "INSERT INTO accounts(name, name_key, hash, created) VALUES (?1, ?2, ?3, ?4)",
                params![name, key(name), hash, now as i64],
            ) {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == ErrorCode::ConstraintViolation =>
                {
                    return Err(RegisterError::NameTaken);
                }
                Err(e) => return Err(e.into()),
            }
            self.db.execute(
                "UPDATE invites SET used_by = ?1 WHERE code_hash = ?2",
                params![name, code_hash],
            )?;
            Ok(Account {
                name: name.to_string(),
                hash: hash.to_string(),
                blocked: false,
                admin: false,
                created: now,
            })
        })
    }

    pub fn invites(&self) -> anyhow::Result<Vec<InviteInfo>> {
        let mut stmt = self
            .db
            .prepare("SELECT created, expires, used_by FROM invites ORDER BY created, rowid")?;
        let rows = stmt.query_map([], |r| {
            Ok(InviteInfo {
                created: r.get::<_, i64>(0)? as u64,
                expires: r.get::<_, i64>(1)? as u64,
                used_by: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn issue_token(
        &self,
        account: &str,
        token: &[u8; 32],
        now: u64,
        ttl_s: u64,
    ) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT INTO tokens(token_hash, account_key, expires) VALUES (?1, ?2, ?3)",
            params![digest(token), key(account), now.saturating_add(ttl_s) as i64],
        )?;
        Ok(())
    }

    /// Single use: the row is deleted even when it has expired.
    pub fn redeem_token(&self, token: &[u8; 32], now: u64) -> anyhow::Result<Option<String>> {
        let row: Option<(String, i64)> = self
            .db
            .query_row(
                "DELETE FROM tokens WHERE token_hash = ?1 RETURNING account_key, expires",
                [digest(token)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.filter(|(_, expires)| (now as i64) < *expires).map(|(account, _)| account))
    }

    pub fn revoke_tokens(&self, account: &str) -> anyhow::Result<usize> {
        Ok(self.db.execute("DELETE FROM tokens WHERE account_key = ?1", [key(account)])?)
    }

    /// Returns whether the account exists. Tokens are revoked so a stolen one dies with the
    /// old password.
    pub fn set_password(&self, name: &str, hash: &str) -> anyhow::Result<bool> {
        self.update_account("UPDATE accounts SET hash = ?2 WHERE name_key = ?1", name, hash, true)
    }

    pub fn set_blocked(&self, name: &str, blocked: bool) -> anyhow::Result<bool> {
        self.update_account(
            "UPDATE accounts SET blocked = ?2 WHERE name_key = ?1",
            name,
            blocked,
            blocked,
        )
    }

    pub fn set_admin(&self, name: &str, admin: bool) -> anyhow::Result<bool> {
        self.update_account("UPDATE accounts SET admin = ?2 WHERE name_key = ?1", name, admin, false)
    }

    fn update_account(
        &self,
        sql: &str,
        name: &str,
        value: impl rusqlite::ToSql,
        revoke: bool,
    ) -> anyhow::Result<bool> {
        self.immediate(|| -> anyhow::Result<bool> {
            let found = self.db.execute(sql, params![key(name), value])? > 0;
            if found && revoke {
                self.revoke_tokens(name)?;
            }
            Ok(found)
        })
    }

    pub fn blocked_accounts(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt =
            self.db.prepare("SELECT name FROM accounts WHERE blocked = 1 ORDER BY name_key")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Returns how many accounts were added; existing ones are left untouched.
    pub fn import_config_users(&self, users: &[UserEntry], now: u64) -> anyhow::Result<usize> {
        self.immediate(|| -> anyhow::Result<usize> {
            let mut added = 0;
            for u in users {
                added += self.db.execute(
                    "INSERT OR IGNORE INTO accounts(name, name_key, hash, admin, created)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![u.name, key(&u.name), u.password_hash, u.admin, now as i64],
                )?;
            }
            Ok(added)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_in(dir: &tempfile::TempDir) -> Store {
        Store::open(&dir.path().join("komunikator.db")).unwrap()
    }

    const CODE: [u8; 16] = [7; 16];
    const TOKEN: [u8; 32] = [9; 32];

    fn invited(store: &Store, code: &[u8; 16], now: u64) {
        store.create_invite(code, now, 100).unwrap();
    }

    #[test]
    fn usernames_are_unique_and_clean() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        invited(&store, &[1; 16], 0);
        invited(&store, &[2; 16], 0);
        let acc = store.register(&[1; 16], "Ala", "h1", 5).unwrap();
        assert_eq!((acc.name.as_str(), acc.created, acc.blocked, acc.admin), ("Ala", 5, false, false));
        assert!(matches!(store.register(&[2; 16], "ala", "h2", 6), Err(RegisterError::NameTaken)));
        // A failed registration must not burn the invite.
        assert!(store.register(&[2; 16], "Ola", "h2", 6).is_ok());
        assert_eq!(store.account("ALA").unwrap().unwrap().name, "Ala");
        assert!(store.account("nikt").unwrap().is_none());
        assert_eq!(store.accounts().unwrap().len(), 2);
    }

    #[test]
    fn invite_is_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        invited(&store, &CODE, 0);
        store.register(&CODE, "Ala", "h", 1).unwrap();
        assert!(matches!(store.register(&CODE, "Ola", "h", 2), Err(RegisterError::InviteInvalid)));
        assert!(matches!(store.register(&[0; 16], "Ewa", "h", 2), Err(RegisterError::InviteInvalid)));
        let info = store.invites().unwrap();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].used_by.as_deref(), Some("Ala"));
    }

    #[test]
    fn expired_invite_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        invited(&store, &CODE, 1000);
        assert!(matches!(store.register(&CODE, "Ala", "h", 1100), Err(RegisterError::InviteInvalid)));
        assert!(store.register(&CODE, "Ala", "h", 1099).is_ok());
    }

    #[test]
    fn token_is_single_use_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        store.issue_token("ala", &TOKEN, 10, 50).unwrap();
        assert_eq!(store.redeem_token(&TOKEN, 20).unwrap().as_deref(), Some("ala"));
        assert_eq!(store.redeem_token(&TOKEN, 21).unwrap(), None);
        store.issue_token("ala", &TOKEN, 10, 50).unwrap();
        assert_eq!(store.redeem_token(&TOKEN, 60).unwrap(), None);
        assert_eq!(store.redeem_token(&TOKEN, 61).unwrap(), None, "expired token is deleted too");
    }

    #[test]
    fn password_change_and_block_revoke_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        invited(&store, &CODE, 0);
        store.register(&CODE, "Ala", "old", 0).unwrap();
        store.issue_token("ala", &TOKEN, 0, 50).unwrap();
        assert!(store.set_password("ALA", "new").unwrap());
        assert_eq!(store.redeem_token(&TOKEN, 1).unwrap(), None);
        assert_eq!(store.account("ala").unwrap().unwrap().hash, "new");
        assert!(!store.set_password("nikt", "x").unwrap());

        store.issue_token("ala", &TOKEN, 0, 50).unwrap();
        assert!(store.set_blocked("ala", true).unwrap());
        assert_eq!(store.redeem_token(&TOKEN, 1).unwrap(), None);
        assert_eq!(store.blocked_accounts().unwrap(), vec!["Ala".to_string()]);
        assert!(store.set_blocked("ala", false).unwrap());
        assert!(store.blocked_accounts().unwrap().is_empty());
        assert!(!store.set_blocked("nikt", true).unwrap());

        assert!(store.set_admin("ala", true).unwrap());
        assert!(store.account("ala").unwrap().unwrap().admin);
        assert!(!store.set_admin("nikt", true).unwrap());

        store.issue_token("ala", &[1; 32], 0, 50).unwrap();
        store.issue_token("ala", &[2; 32], 0, 50).unwrap();
        assert_eq!(store.revoke_tokens("Ala").unwrap(), 2);
    }

    #[test]
    fn config_users_are_imported_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        let users = vec![
            UserEntry { name: "Ala".into(), password_hash: "ha".into(), admin: true },
            UserEntry { name: "ola".into(), password_hash: "ho".into(), admin: false },
        ];
        assert_eq!(store.import_config_users(&users, 3).unwrap(), 2);
        assert_eq!(store.import_config_users(&users, 4).unwrap(), 0);
        let ala = store.account("ala").unwrap().unwrap();
        assert_eq!((ala.hash.as_str(), ala.admin, ala.created), ("ha", true, 3));
        assert!(!store.account("ola").unwrap().unwrap().admin);
    }

    #[test]
    fn codes_and_tokens_are_stored_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(&dir);
        let code = [0xA5u8; 16];
        let token = [0x5Au8; 32];
        invited(&store, &code, 0);
        store.issue_token("ala", &token, 0, 50).unwrap();
        // Force the pages out of the WAL into the main file.
        store.db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
        drop(store);
        let mut bytes = std::fs::read(dir.path().join("komunikator.db")).unwrap();
        if let Ok(wal) = std::fs::read(dir.path().join("komunikator.db-wal")) {
            bytes.extend(wal);
        }
        let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(!has(&code) && !has(&token));
        assert!(has(&Sha256::digest(code)) && has(&Sha256::digest(token)));
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
