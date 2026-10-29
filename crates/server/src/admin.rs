//! Admin subcommands. They open the database directly, so they work whether or not the server
//! is running; a running server notices account and room changes within a few seconds.

use anyhow::{bail, Context};
use protocol::{encode_invite, normalize_room_name, Invite, RoomId, MAX_ROOMS, MAX_SERVER_LEN, MIN_PASSWORD_CHARS};

use crate::auth::{hash_password, random_invite_code};
use crate::store::{Store, StoreError};
use crate::{tls, unix_now, ServerConfig};

const DAY_S: u64 = 24 * 60 * 60;

/// Refuses a data directory the server has never run in (most likely a wrong `--config`):
/// opening it would silently create an empty database or a new server identity there.
fn check_data_dir(config: &ServerConfig) -> anyhow::Result<()> {
    for file in ["cert.der", "komunikator.db"] {
        let path = config.data_dir.join(file);
        if !path.is_file() {
            bail!(
                "nie ma pliku {} — czy --config wskazuje plik konfiguracji działającego serwera? \
                 Katalog danych (data_dir) tworzy dopiero `komunikator-server run`.",
                path.display()
            );
        }
    }
    Ok(())
}

fn open(config: &ServerConfig) -> anyhow::Result<Store> {
    check_data_dir(config)?;
    Store::open(&config.data_dir.join("komunikator.db")).context("otwieranie bazy danych")
}

/// Creates an invite code and returns the text to show the admin: the invite string and what
/// to tell the friend.
pub fn invite(config: &ServerConfig, days: u64, server: Option<&str>) -> anyhow::Result<String> {
    let server = server
        .or(config.public_address.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context(
            "nie wiadomo, pod jakim adresem znajomi mają się łączyć: podaj go w opcji \
             --server host:port albo ustaw public_address w pliku konfiguracji",
        )?;
    check_address(server)?;
    if days == 0 {
        bail!("zaproszenie musi być ważne co najmniej 1 dzień");
    }
    let store = open(config)?;
    let fingerprint = tls::load_or_create_identity(&config.data_dir)?.fingerprint;
    let code = random_invite_code();
    store.create_invite(&code, unix_now(), days.saturating_mul(DAY_S))?;
    let text = encode_invite(&Invite { server: server.to_string(), fingerprint, code });
    Ok(format!(
        "Zaproszenie (ważne {days} dni, jednorazowe):\n\n{text}\n\n\
         Przekaż je znajomemu bezpiecznym kanałem. Niech uruchomi program, wybierze \
         „Mam zaproszenie”, wklei cały kod, a potem wybierze nazwę użytkownika i hasło.\n"
    ))
}

/// `host:port` with a port, and short enough for an invite.
fn check_address(server: &str) -> anyhow::Result<()> {
    let valid = server.len() <= MAX_SERVER_LEN
        && server
            .rsplit_once(':')
            .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0));
    if !valid {
        bail!("adres serwera „{server}” musi mieć postać host:port (np. serwer.example.pl:7700)");
    }
    Ok(())
}

pub fn users(config: &ServerConfig) -> anyhow::Result<String> {
    let rows = open(config)?
        .accounts()?
        .into_iter()
        .map(|a| vec![a.name, yes_no(a.admin), yes_no(a.blocked), date(a.created)])
        .collect();
    Ok(table(&["Nazwa", "Admin", "Zablokowany", "Utworzono"], rows))
}

/// Never shows codes: the store keeps only their hashes.
pub fn invites(config: &ServerConfig) -> anyhow::Result<String> {
    let now = unix_now();
    let rows = open(config)?
        .invites()?
        .into_iter()
        .map(|i| {
            let used = match (&i.used_by, i.expires <= now) {
                (Some(name), _) => name.clone(),
                (None, true) => "(wygasło)".to_string(),
                (None, false) => "(nieużyte)".to_string(),
            };
            vec![date(i.created), date(i.expires), used]
        })
        .collect();
    Ok(table(&["Utworzono", "Wygasa", "Użyte przez"], rows))
}

pub fn block(config: &ServerConfig, name: &str) -> anyhow::Result<String> {
    account_flag(config, name, "zablokowane", |s, n| s.set_blocked(n, true))
}

pub fn unblock(config: &ServerConfig, name: &str) -> anyhow::Result<String> {
    account_flag(config, name, "odblokowane", |s, n| s.set_blocked(n, false))
}

pub fn make_admin(config: &ServerConfig, name: &str) -> anyhow::Result<String> {
    account_flag(config, name, "ma uprawnienia administratora", |s, n| s.set_admin(n, true))
}

pub fn revoke_admin(config: &ServerConfig, name: &str) -> anyhow::Result<String> {
    account_flag(config, name, "nie ma już uprawnień administratora", |s, n| s.set_admin(n, false))
}

fn account_flag(
    config: &ServerConfig,
    name: &str,
    done: &str,
    change: impl FnOnce(&Store, &str) -> anyhow::Result<bool>,
) -> anyhow::Result<String> {
    if !change(&open(config)?, name)? {
        bail!("nie ma konta „{name}”");
    }
    Ok(format!("Konto „{name}” {done}.\n"))
}

/// Sets a new password and logs the account out everywhere (its session tokens are revoked).
pub fn reset_password(config: &ServerConfig, name: &str, password: &str) -> anyhow::Result<String> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        bail!("hasło musi mieć co najmniej {MIN_PASSWORD_CHARS} znaków");
    }
    let store = open(config)?;
    let hash = hash_password(password);
    if !store.set_password(name, &hash)? {
        bail!("nie ma konta „{name}”");
    }
    Ok(format!("Hasło konta „{name}” zostało zmienione, a zapamiętane sesje wygasły.\n"))
}

pub fn rooms(config: &ServerConfig) -> anyhow::Result<String> {
    let rows = open(config)?
        .rooms()?
        .into_iter()
        .map(|r| vec![r.id.to_string(), r.name, r.creator])
        .collect();
    Ok(table(&["Nr", "Nazwa", "Założył(a)"], rows))
}

pub fn create_room(config: &ServerConfig, name: &str) -> anyhow::Result<String> {
    let name = normalize_room_name(name).context("nieprawidłowa nazwa pokoju")?;
    let store = open(config)?;
    if store.rooms()?.len() >= MAX_ROOMS {
        bail!("za dużo pokoi (najwięcej {MAX_ROOMS})");
    }
    match store.create_room(&name, "") {
        Ok(room) => Ok(format!("Utworzono pokój „{}” (nr {}).\n", room.name, room.id)),
        Err(StoreError::NameTaken) => bail!("pokój „{name}” już istnieje"),
        Err(e) => Err(e.into()),
    }
}

/// Also deletes an occupied room: the running server then moves its members out.
pub fn delete_room(config: &ServerConfig, id: RoomId) -> anyhow::Result<String> {
    if !open(config)?.delete_room(id)? {
        bail!("nie ma pokoju o numerze {id}");
    }
    Ok(format!("Usunięto pokój nr {id}.\n"))
}

fn yes_no(v: bool) -> String {
    if v { "tak" } else { "nie" }.to_string()
}

/// `YYYY-MM-DD HH:MM` in UTC.
fn date(unix: u64) -> String {
    let days = (unix / DAY_S) as i64;
    let secs = unix % DAY_S;
    // Days since 1970-01-01 to a civil date (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// Left-aligned columns; widths count characters so Polish letters line up.
fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| {
            rows.iter().map(|r| r[i].chars().count()).chain([headers[i].chars().count()]).max().unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let padded: Vec<String> =
            cells.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}", w = *w)).collect();
        format!("{}\n", padded.join("  ").trim_end())
    };
    let mut out = line(headers.to_vec());
    for row in &rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_formats_utc() {
        assert_eq!(date(0), "1970-01-01 00:00");
        assert_eq!(date(1_709_210_096), "2024-02-29 12:34");
        assert_eq!(date(4_102_444_800), "2100-01-01 00:00");
    }

    #[test]
    fn table_aligns_polish_letters() {
        let t = table(&["Nazwa", "Admin"], vec![vec!["Żaneta".into(), "tak".into()], vec!["ala".into(), "nie".into()]]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines, ["Nazwa   Admin", "Żaneta  tak", "ala     nie"]);
    }
}
