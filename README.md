# Komunikator

Lekki komunikator głosowy dla małej grupy graczy: natywny klient na Windows (Rust, WASAPI, Opus) i serwer przekazujący dźwięk (QUIC) na Linuksie.

Stan: **plan 3, pokoje i czat.** Klient działa w wierszu poleceń: wiele pokoi, czat tekstowy i głośność każdej osoby. Interfejs graficzny i konta z zaproszeniami pojawią się w kolejnych planach (`docs/superpowers/`).

## Budowanie

```
cargo build --release --workspace          # Windows: komunikator.exe + komunikator-server.exe
cargo build --release -p server            # Debian: tylko serwer
```

Wymagania: Rust (stable) i CMake (do zbudowania libopus).

## Serwer

1. Wygeneruj hash hasła dla każdej osoby:
   ```
   echo 'haslo' | komunikator-server hash-password
   ```
2. Przygotuj konfigurację na podstawie `server.example.toml`: adres, katalog danych i listę `[[users]]` z hashami.
3. Uruchom serwer (w `[[users]]` flaga `admin = true` pozwala usuwać pokoje założone przez innych; domyślnie `false`):
   ```
   komunikator-server run --config server.toml
   ```
   Przy starcie serwer wypisuje `fingerprint: <64 znaki hex>`. To odcisk certyfikatu, który trzeba przekazać znajomym. Certyfikat powstaje przy pierwszym uruchomieniu w `data_dir` (`cert.der`, `key.der`) i potem się nie zmienia.
4. Otwórz w firewallu port **UDP 7700** (albo ten ustawiony w `bind`).

## Klient (Windows)

```
set KOMUNIKATOR_PASSWORD=haslo
komunikator --server adres.serwera:7700 --fingerprint <odcisk> --user ala
```

Polecenia (wpisz i naciśnij Enter):

| Polecenie | Działanie |
|---|---|
| `p` | pokoje z osobami; Twój pokój oznaczony `*`, numer pokoju to jego identyfikator |
| `w <nr>` / `o` | wejdź do pokoju / wyjdź z pokoju |
| `n <nazwa>` / `u <nr>` | załóż pokój / usuń pokój (tylko własny; administrator usuwa też cudze, ale nie zajęte) |
| `t <tekst>` | napisz na czacie pokoju (do 2000 znaków, szyfrowany end-to-end, bez historii na serwerze) |
| `g <id> <0-200>` | głośność osoby w %, pamiętana po nazwie użytkownika |
| `x <id>` | wycisz / odcisz osobę tylko u siebie |
| `m` / `d` | mikrofon wł./wył. / wycisz wszystkich (deafen) |
| `k` | osoby w Twoim pokoju z numerami, stanem zaufania i kodem bezpieczeństwa |
| `z <id> <grupa>` / `a <id> <grupa>` | oznacz jako zweryfikowaną / zaakceptuj zmieniony klucz; `<grupa>` to pierwsze 4 cyfry kodu, który porównaliście głosowo (np. `z 3 1234`) |
| `q` | wyjście |

Odebrane wiadomości wyglądają jak `[14:05] ala: tekst`; każdy adres (URL lub host:port) w wiadomości dostaje osobną linię `  link 1: …` do skopiowania.

- `--list-devices` wypisuje urządzenia. Wybrane urządzenie ustawiasz przez `--capture-device <id>` i `--render-device <id>`; bez tych opcji używane są domyślne urządzenia komunikacyjne Windows.
- `--data-dir <katalog>`: gdzie leżą `identity.bin` (tożsamość chroniona DPAPI) i `known_devices.toml` (domyślnie `%APPDATA%\komunikator`).

Dźwięk jest szyfrowany end-to-end: serwer nie może go odsłuchać. „Nowe urządzenie” oznacza, że znajomy ma nową tożsamość (np. nowy komputer): porównaj kod z nim głosowo. „Zmieniony klucz” (to samo urządzenie, inny klucz) to możliwe podszywanie się. Szczegóły i ograniczenia: [docs/e2e.md](docs/e2e.md).

Klient po zerwaniu połączenia łączy się ponownie sam. Po odłączeniu słuchawek przechodzi na urządzenie domyślne.

## Testy

```
cargo test --workspace
```
