# Komunikator

Lekki komunikator głosowy dla małej grupy graczy: natywny klient na Windows (Rust, WASAPI, Opus) i serwer przekazujący dźwięk (QUIC) na Linuksie.

Stan: **plan 5, konta z zaproszeniami.** Klient ma natywne okno Windows z ikoną w zasobniku: pokoje, czat, wyciszanie skrótami, okna pierwszego uruchomienia, ustawień i kodu bezpieczeństwa. Znajomi zakładają konta z jednorazowym zaproszeniem od administratora, a program pamięta logowanie, więc hasło wpisuje się tylko raz.

## Budowanie

```
cargo build --release --workspace          # Windows: komunikator.exe (okno), komunikator-cli.exe, komunikator-server.exe
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

## Administracja

Administratorem jest każdy, kto ma dostęp do powłoki serwera. Wszystkie polecenia przyjmują `--config <plik.toml>` (stąd biorą `data_dir`) i działają na bazie bezpośrednio, więc nie wymagają zatrzymania serwera: działający serwer zauważa blokady i zmiany pokoi w ciągu kilku sekund.

| Polecenie | Działanie |
|---|---|
| `invite [--days 7] [--server host:port]` | tworzy jednorazowe zaproszenie i wypisuje jego tekst (`kom1-…`) wraz z instrukcją dla znajomego; adres z `--server`, a bez niego z `public_address` w konfiguracji |
| `users` | konta: nazwa, administrator, zablokowany, data utworzenia |
| `invites` | zaproszenia: utworzone, wygasa, kto użył (kody nie są nigdzie zapisane w postaci jawnej) |
| `block <nazwa>` / `unblock <nazwa>` | blokuje konto (także rozłącza je w ciągu kilku sekund, jeśli jest online) / odblokowuje |
| `make-admin <nazwa>` / `revoke-admin <nazwa>` | nadaje / odbiera prawo usuwania cudzych pokoi |
| `reset-password <nazwa>` | nowe hasło czytane ze standardowego wejścia (minimum 8 znaków); zapamiętane sesje konta wygasają |
| `rooms` | pokoje z numerami |
| `create-room <nazwa>` / `delete-room <nr>` | zakłada pokój / usuwa pokój, także zajęty (osoby w nim trafiają do „bez pokoju”) |
| `hash-password` | hash hasła do `[[users]]` w konfiguracji (te konta są importowane przy starcie) |

Przykład zaproszenia znajomego (ustaw `public_address = "gra.example.pl:7700"` w konfiguracji):

```
komunikator-server invite --config server.toml --days 3
```

Polecenie wypisuje tekst w rodzaju `kom1-1A2B3-…`. Przekaż go znajomemu bezpiecznym kanałem: w programie wybiera `Mam zaproszenie`, wkleja cały tekst, potem wybiera nazwę użytkownika i hasło. Zaproszenie zawiera adres serwera i odcisk certyfikatu, więc nic więcej nie trzeba przepisywać. Wygasa po podanej liczbie dni albo po pierwszym użyciu.

## Klient (Windows)

### Okno: `komunikator.exe`

Przy pierwszym uruchomieniu program pyta o konto (`Anuluj` kończy program):

- `Mam zaproszenie`: wklej cały `Kod zaproszenia` (`kom1-…`) od administratora, wybierz `Nazwę użytkownika` (1–32 znaki: litery, cyfry, `_ - .`) i `Hasło` (co najmniej 8 znaków, wpisane dwa razy). Zaproszenie zawiera adres serwera i odcisk certyfikatu.
- `Mam już konto`: `Adres serwera` (host:port), `Odcisk certyfikatu` (64 znaki z wydruku serwera), `Nazwa użytkownika` i `Hasło`.

Po zalogowaniu program pamięta sesję (token od serwera, odnawiany przy każdym logowaniu) i zapomina hasło: kolejne uruchomienia łączą się bez pytania. Gdy sesja wygaśnie (np. administrator zresetował hasło), okno `Mam już konto` otwiera się samo z wpisanym serwerem i nazwą. Ustawienia są w `%APPDATA%\komunikator\settings.toml` (token i hasło chronione DPAPI, tylko dla tego konta Windows).

- U góry: `Mikrofon` i `Dźwięk` (wyciszenie mikrofonu / wszystkich), `Opuść pokój`, `Nowy pokój…` (po utworzeniu wchodzisz do niego), `Ustawienia…`.
- Po lewej pokoje z osobami: 🔊 przy mówiącej osobie, `(Ty)` przy Tobie, głośność i wyciszenie u Ciebie, a w Twoim pokoju stan zaufania (`[nowa osoba]`, `[NOWE URZĄDZENIE]`, `[ZMIENIONY KLUCZ]`, `[zweryfikowany ✓]`). Dwuklik na pokoju: wejście. Prawy przycisk na pokoju: `Wejdź`, `Usuń pokój`; na osobie: głośność 0–200%, `Wycisz u mnie` i `Kod bezpieczeństwa…` (osoby z Twojego pokoju: porównajcie kod głosowo i wpisz jego pierwszą grupę, żeby oznaczyć urządzenie jako zweryfikowane albo zaakceptować zmieniony klucz).
- Po prawej czat pokoju; Enter wysyła. Kliknięcie wiadomości z adresem kopiuje pierwszy adres do schowka, prawy przycisk pozwala wybrać adres.
- Krzyżyk chowa okno do zasobnika. Ikona w zasobniku: zielona (połączono), czerwona (wyciszony), ciemnoczerwona (wyciszeni wszyscy), szara (rozłączono), pomarańczowa kropka = nowa wiadomość. Lewy klik wycisza mikrofon; prawy: `Pokaż okno`, `Wycisz mikrofon`, `Wycisz wszystkich`, `Wyjdź`.
- `Ustawienia…`: mikrofon i głośniki, czułość wykrywania mowy (z podglądem poziomu mikrofonu), skróty globalne `Wycisz mikrofon` / `Wycisz wszystkich` (wymagają Ctrl, Alt lub Shift; zajęty skrót okno pokazuje, a drugie `OK` zapisuje go mimo to), dźwięki, `Uruchamiaj z Windows`, `Uruchamiaj jako administrator` (gdy gra działa jako administrator i nie przepuszcza skrótów) oraz `Konto…` — zmiana konta łączy od nowa.
- Opcje: `--minimized` (start w zasobniku), `--data-dir <katalog>` (zamiast `%APPDATA%\komunikator`), `--smoke` (autotest: okno na sekundę, zapis ustawień, wyjście).

### Wiersz poleceń: `komunikator-cli.exe`

```
set KOMUNIKATOR_PASSWORD=dlugie-haslo
komunikator-cli --invite kom1-… --user ala
komunikator-cli
```

`--invite` zakłada konto (hasło ze zmiennej `KOMUNIKATOR_PASSWORD`, co najmniej 8 znaków) i zapisuje serwer, odcisk, nazwę i sesję w `settings.toml` w katalogu danych; kolejne uruchomienia bez opcji używają zapamiętanej sesji. Można też zalogować się hasłem bez zapisywania czegokolwiek: `komunikator-cli --server adres.serwera:7700 --fingerprint <odcisk> --user ala`.

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
- `--data-dir <katalog>`: gdzie leżą `identity.bin` (tożsamość chroniona DPAPI), `known_devices.toml` i `settings.toml` (domyślnie `%APPDATA%\komunikator`, ten sam co dla okna).

Dźwięk jest szyfrowany end-to-end: serwer nie może go odsłuchać. „Nowe urządzenie” oznacza, że znajomy ma nową tożsamość (np. nowy komputer): porównaj kod z nim głosowo. „Zmieniony klucz” (to samo urządzenie, inny klucz) to możliwe podszywanie się. Szczegóły i ograniczenia: [docs/e2e.md](docs/e2e.md).

Klient po zerwaniu połączenia łączy się ponownie sam. Po odłączeniu słuchawek przechodzi na urządzenie domyślne.

## Testy

```
cargo test --workspace
```
