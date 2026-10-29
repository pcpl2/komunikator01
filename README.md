# Komunikator

Lekki komunikator głosowy dla małej grupy graczy: natywny klient na Windows (Rust, WASAPI, Opus) i serwer przekazujący dźwięk (QUIC) na Linuksie.

Stan: **plan 5, konta z zaproszeniami.** Klient ma natywne okno Windows z ikoną w zasobniku: pokoje, czat, wyciszanie skrótami, okna pierwszego uruchomienia, ustawień i kodu bezpieczeństwa. Znajomi zakładają konta z jednorazowym zaproszeniem od administratora, a program pamięta logowanie, więc hasło wpisuje się tylko raz.

## Budowanie

```
cargo build --release --workspace          # Windows: komunikator.exe (okno), komunikator-cli.exe, komunikator-server.exe
cargo build --release -p server            # Debian: tylko serwer
```

Wymagania: Rust (stable) i CMake (do zbudowania libopus). Klient jest linkowany ze statycznym CRT (`.cargo/config.toml`), więc `komunikator.exe` działa bez VC++ Redistributable.

Wdrożenie serwera na Debianie (usługa systemd, firewall) i publikowanie wydań: [docs/wydanie.md](docs/wydanie.md); gotowe pliki w `deploy/`.

## Serwer

Konta zakłada się z zaproszeń; hasła nie trafiają do pliku konfiguracji.

1. Przygotuj konfigurację na podstawie `server.example.toml`: `bind`, `data_dir` i `public_address` (adres, który trafi do zaproszeń).
2. Załóż użytkownika systemowego dla serwera i katalog danych dostępny tylko dla niego (baza `komunikator.db` zawiera hashe haseł Argon2, a `key.der` to klucz prywatny serwera):
   ```
   sudo useradd --system --home-dir /var/lib/komunikator komunikator
   sudo install -d -m 700 -o komunikator -g komunikator /var/lib/komunikator
   ```
3. Uruchom serwer jako ten użytkownik (np. usługą systemd z `User=komunikator`):
   ```
   sudo -u komunikator komunikator-server run --config /etc/komunikator/server.toml
   ```
   Przy pierwszym uruchomieniu powstają w `data_dir` certyfikat (`cert.der`, `key.der`, potem się nie zmienia) i baza `komunikator.db`. Serwer wypisuje `fingerprint: <64 znaki hex>` — odcisk certyfikatu; zaproszenia zawierają go same, więc przepisuje się go tylko przy logowaniu przez `Mam już konto`.
4. Otwórz w firewallu port **UDP 7700** (albo ten ustawiony w `bind`).
5. Pierwsze konto (Twoje): utwórz zaproszenie, zarejestruj się nim w programie (`Mam zaproszenie`), a potem nadaj sobie uprawnienia administratora:
   ```
   sudo -u komunikator komunikator-server invite --config /etc/komunikator/server.toml
   sudo -u komunikator komunikator-server make-admin --config /etc/komunikator/server.toml ala
   ```
   Znajomych zapraszasz tak samo (bez `make-admin`), patrz niżej.

Polecenia administracyjne uruchamiaj jako ten sam użytkownik co serwer (`sudo -u komunikator …`): pliki, które utworzyłby ktoś inny, nie byłyby dla serwera dostępne. Polecenia odmawiają pracy, gdy w `data_dir` nie ma `cert.der` i `komunikator.db` (zwykle zły `--config`) — oba tworzy dopiero `run`.

**Aktualizacja z wersji z listą `[[users]]`:** wpisy `[[users]]` są przy starcie importowane do bazy tylko wtedy, gdy takiego konta jeszcze w niej nie ma; potem liczy się wyłącznie baza. Jeśli wpis różni się od konta w bazie (hash hasła albo `admin`) lub koliduje z kontem różniącym się tylko wielkością liter, serwer ostrzega o tym w logu przy starcie i wpis pomija. Zmieniaj takie konta poleceniami `reset-password`, `make-admin` / `revoke-admin` i `block`, a wpisy `[[users]]` po pierwszym starcie usuń z konfiguracji.

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
| `hash-password` | hash hasła do `[[users]]` w konfiguracji (tylko starsze instalacje; konta z listy są importowane przy starcie, jeśli jeszcze ich nie ma w bazie) |

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

Po zalogowaniu program pamięta sesję (token od serwera, odnawiany przy każdym logowaniu) i zapomina hasło: kolejne uruchomienia łączą się bez pytania. Nazwę zapisuje tak, jak zna ją serwer (wielkość liter przy logowaniu nie ma znaczenia). Gdy sesja wygaśnie (np. administrator zresetował hasło), okno `Mam już konto` otwiera się samo z wpisanym serwerem i nazwą. Gdy to samo konto zalogowało się gdzie indziej, wystarczy otworzyć `Konto…` i nacisnąć `OK` z pustym hasłem — puste hasło przy zapamiętanej sesji oznacza „użyj zapamiętanej sesji”. Wybranie `Mam zaproszenie` przy zalogowanym koncie nie usuwa jego sesji, dopóki nowe konto nie zostanie założone. Ustawienia są w `%APPDATA%\komunikator\settings.toml` (token i hasło chronione DPAPI, tylko dla tego konta Windows).

- U góry: `Mikrofon` i `Dźwięk` (wyciszenie mikrofonu / wszystkich), `Opuść pokój`, `Nowy pokój…` (po utworzeniu wchodzisz do niego), `Ustawienia…`.
- Po lewej pokoje z osobami: 🔊 przy mówiącej osobie, `(Ty)` przy Tobie, głośność i wyciszenie u Ciebie, a w Twoim pokoju stan zaufania (`[nowa osoba]`, `[NOWE URZĄDZENIE]`, `[ZMIENIONY KLUCZ]`, `[zweryfikowany ✓]`). Dwuklik na pokoju: wejście. Prawy przycisk na pokoju: `Wejdź`, `Usuń pokój`; na osobie: głośność 0–200%, `Wycisz u mnie` i `Kod bezpieczeństwa…` (osoby z Twojego pokoju: porównajcie kod głosowo i wpisz jego pierwszą grupę, żeby oznaczyć urządzenie jako zweryfikowane albo zaakceptować zmieniony klucz).
- Po prawej czat pokoju; Enter wysyła. Kliknięcie wiadomości z adresem kopiuje pierwszy adres do schowka, prawy przycisk pozwala wybrać adres.
- Krzyżyk chowa okno do zasobnika. Ikona w zasobniku: zielona (połączono), czerwona (wyciszony), ciemnoczerwona (wyciszeni wszyscy), szara (rozłączono), pomarańczowa kropka = nowa wiadomość. Lewy klik wycisza mikrofon; prawy: `Pokaż okno`, `Wycisz mikrofon`, `Wycisz wszystkich`, `Wyjdź`.
- `Ustawienia…`: mikrofon i głośniki, czułość wykrywania mowy (z podglądem poziomu mikrofonu), skróty globalne `Wycisz mikrofon` / `Wycisz wszystkich` (wymagają Ctrl, Alt lub Shift; zajęty skrót okno pokazuje, a drugie `OK` zapisuje go mimo to), dźwięki, `Uruchamiaj z Windows`, `Uruchamiaj jako administrator` (gdy gra działa jako administrator i nie przepuszcza skrótów) oraz `Konto…` — zmiana konta łączy od nowa.
- Opcje: `--minimized` (start w zasobniku), `--data-dir <katalog>` (zamiast `%APPDATA%\komunikator`), `--smoke` (autotest: okno na sekundę, zapis ustawień, wyjście).

#### Aktualizacje

Program sam pobiera nowe wersje ze swojego serwera: przy starcie (zanim się połączy), co 6 godzin i od razu, gdy serwer zgłosi niezgodną wersję programu. Przyjmuje tylko wydanie podpisane kluczem wbudowanym w program (`crates/client-win/release-key.pub`), ściśle nowsze od własnej wersji, o zgodnym rozmiarze i skrócie SHA-256; serwer nie ma klucza i nie może podsunąć niczego innego ani starszej wersji. Na pasku stanu widać `pobieranie aktualizacji…`, potem `aktualizacja gotowa — zostanie zainstalowana, gdy nie będziesz w rozmowie`. Instalacja czeka, aż nie będziesz w pokoju z inną osobą: wtedy program zapisuje ustawienia, kończy połączenie, podmienia plik `komunikator.exe` (nowy obok jako `komunikator.exe.new`, stary odkładany jako `komunikator.exe.old` i usuwany przy następnym starcie) i uruchamia się ponownie z komunikatem `zaktualizowano do wersji X`. Gdy wydanie jest, ale nie da się go sprawdzić, pobrać albo zainstalować, pojawia się `aktualizacja nieudana: <powód>`, a program działa dalej w starej wersji; niedostępny serwer nie wywołuje żadnego komunikatu. Rozłączenie w trakcie rozmowy nie przerywa czekania: instalacja następuje dopiero, gdy po ponownym połączeniu nie jesteś w pokoju z inną osobą. Przy każdym starcie program zapisuje swoją wersję w `komunikator.version` obok pliku exe. Program musi leżeć w folderze, w którym możesz zapisywać pliki. Kompilacja z kluczem z samych zer (domyślny w repozytorium) nie aktualizuje się wcale. Kanał aktualizacji: [docs/updates.md](docs/updates.md).

### Wiersz poleceń: `komunikator-cli.exe`

```
set KOMUNIKATOR_PASSWORD=dlugie-haslo
komunikator-cli --invite kom1-… --user ala
komunikator-cli
```

`--invite` zakłada konto (hasło ze zmiennej `KOMUNIKATOR_PASSWORD`, co najmniej 8 znaków) i zapisuje serwer, odcisk, nazwę i sesję w `settings.toml` w katalogu danych; kolejne uruchomienia bez opcji używają zapamiętanej sesji. Istniejące konto loguje się hasłem (z tej samej zmiennej) i jest zapamiętywane tak samo: `komunikator-cli --server adres.serwera:7700 --fingerprint <odcisk> --user ala`. Gdy zapamiętana sesja wygaśnie, program wypisuje dokładnie to polecenie. Zapis następuje dopiero po udanym logowaniu, więc odrzucona próba nie zmienia zapamiętanego konta.

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
- `--check-update` sprawdza raz serwer (z `--server` i `--fingerprint`, z `--invite` albo zapamiętany w `settings.toml`), instaluje nowsze podpisane wydanie w miejsce `komunikator.exe` leżącego obok `komunikator-cli.exe` (nowsze zarówno od samego `komunikator-cli`, jak i od wersji zapisanej przez okno w `komunikator.version`, więc nigdy nie cofa wersji okna), wypisuje wynik i kończy działanie. Uruchomione okno przejdzie na nową wersję po ponownym starcie. Bez tej opcji `komunikator-cli` nigdy niczego nie aktualizuje.

Dźwięk jest szyfrowany end-to-end: serwer nie może go odsłuchać. „Nowe urządzenie” oznacza, że znajomy ma nową tożsamość (np. nowy komputer): porównaj kod z nim głosowo. „Zmieniony klucz” (to samo urządzenie, inny klucz) to możliwe podszywanie się. Szczegóły i ograniczenia: [docs/e2e.md](docs/e2e.md).

Klient po zerwaniu połączenia łączy się ponownie sam. Po odłączeniu słuchawek przechodzi na urządzenie domyślne.

## Testy

```
cargo test --workspace
```
