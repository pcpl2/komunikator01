# Pierwsze uruchomienie

Kroki od zera do pierwszej rozmowy. Część A robi administrator (Ty), część B każdy gracz. Szczegóły: [wydanie.md](wydanie.md) i [README](../README.md).

## A. Administrator

### 1. Wybierz lokalizację serwera (PL czy FR)

Każdy gracz u siebie uruchamia w `cmd`:

```
ping -n 20 <adres-kandydata-PL>
ping -n 20 <adres-kandydata-FR>
```

i podaje średni czas oraz utratę pakietów. Wybierz lokalizację, w której **najgorszy** gracz ma najniższy ping.

### 2. Klucz wydań (jednorazowo, na Windows)

Klucz pozwala później rozsyłać aktualizacje automatycznie. Zrób to przed zbudowaniem wersji, którą dostaną gracze. Inaczej każdą następną wersję trzeba będzie rozesłać ręcznie.

```
cargo build --release -p release
target\release\komunikator-release keygen --out release.key
```

1. Wypisany klucz publiczny wklej do `crates/client-win/release-key.pub` w miejsce zer (pierwszy wiersz niebędący komentarzem), potem zrób commit.
2. Plik `release.key` przenieś w bezpieczne miejsce, np. na pendrive i do menedżera haseł. Nie kopiuj go na serwer ani do repozytorium. Bez niego nie wydasz aktualizacji dla rozdanych programów.

### 3. Zbuduj program dla graczy (Windows)

Wymagania: Rust i CMake.

```
cargo build --release -p client-win
```

Plik dla graczy to `target\release\komunikator.exe`. To jeden plik, który nie wymaga instalacji.

### 4. Postaw serwer (Debian)

Repozytorium nie ma zdalnego serwera, więc przenieś kod paczką:

```
git bundle create komunikator.bundle main
scp komunikator.bundle uzytkownik@serwer:~
```

Na serwerze:

```
sudo apt install build-essential pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
git clone komunikator.bundle komunikator && cd komunikator
cargo build --release -p server

sudo install -m 755 target/release/komunikator-server /usr/local/bin/
sudo useradd --system --home-dir /var/lib/komunikator --shell /usr/sbin/nologin komunikator
sudo install -d /etc/komunikator
sudo install -m 644 deploy/komunikator.toml /etc/komunikator/komunikator.toml
sudo install -m 644 deploy/komunikator.service /etc/systemd/system/komunikator.service
sudoedit /etc/komunikator/komunikator.toml
```

W pliku konfiguracji ustaw `public_address` na prawdziwy adres serwera (domena albo IP) z portem, np. `"gra.example.pl:7700"`. Ten adres trafi do zaproszeń.

```
sudo systemctl daemon-reload
sudo systemctl enable --now komunikator
journalctl -u komunikator -n 20
```

W logu powinna pojawić się linia `fingerprint: …`. Oznacza ona, że serwer działa.

### 5. Otwórz port UDP 7700

```
sudo ufw allow 7700/udp
```

Przy nftables dopisz `udp dport 7700 accept` do łańcucha `input`. Jeśli serwer stoi za routerem, przekieruj na nim UDP 7700. Port **UDP** jest wymagany, TCP nie wystarczy.

### 6. Twoje konto administratora

```
sudo -u komunikator komunikator-server invite --config /etc/komunikator/komunikator.toml
```

1. Skopiuj wypisany kod `kom1-…`.
2. Zarejestruj się nim w programie, tak jak gracz w części B.
3. Nadaj sobie uprawnienia administratora:

```
sudo -u komunikator komunikator-server make-admin --config /etc/komunikator/komunikator.toml <twoja-nazwa>
```

### 7. Zaproszenia dla znajomych

Wygeneruj osobny kod dla każdej osoby. Kod jest jednorazowy i domyślnie ważny 7 dni:

```
sudo -u komunikator komunikator-server invite --config /etc/komunikator/komunikator.toml
```

Wyślij każdemu `komunikator.exe` i jego kod prywatną wiadomością, nie na publicznym kanale.

Później przydadzą się polecenia:
- `users`: kto ma konto;
- `invites`: które zaproszenia zostały użyte;
- `block <nazwa>`: zablokowanie konta.

## B. Gracz

1. Utwórz folder, np. `C:\Gry\Komunikator`, i wrzuć do niego `komunikator.exe`. Program musi móc zapisywać w tym folderze, więc nie wybieraj `Program Files`.
2. Uruchom program. Jeśli Windows pokaże „System Windows ochronił ten komputer”, kliknij **Więcej informacji → Uruchom mimo to**. Program nie ma płatnego podpisu wydawcy, dlatego to ostrzeżenie pojawia się tylko za pierwszym razem.
3. Wybierz **Mam zaproszenie** i wklej cały kod `kom1-…`.
4. Wpisz nazwę użytkownika (1–32 znaki: litery, cyfry, `_ - .`) oraz hasło (co najmniej 8 znaków, dwa razy). Hasło wpisujesz tylko raz, bo program zapamiętuje sesję.
5. Otwórz **Ustawienia…** i ustaw:
   - mikrofon i głośniki/słuchawki;
   - czułość mowy: mów normalnie i obserwuj pasek poziomu;
   - skróty `Wycisz mikrofon` i `Wycisz wszystkich`, np. `Ctrl+Alt+M` i `Ctrl+Alt+D`. Skrót musi zawierać Ctrl, Alt lub Shift;
   - opcjonalnie **Uruchamiaj z Windows**.
6. Kliknij dwukrotnie pokój „Ogólny” po lewej stronie, żeby do niego wejść.
7. **Weryfikacja (raz na każdą osobę):** przy nowych osobach widać `[nowa osoba]`.
   - Kliknij osobę prawym przyciskiem i wybierz **Kod bezpieczeństwa…**.
   - Przeczytajcie sobie kody na głos.
   - Jeśli się zgadzają, wpisz pierwszą grupę cyfr. Osoba dostaje znaczek `[zweryfikowany ✓]`.
   - Gdy kiedyś pojawi się `[ZMIENIONY KLUCZ]` albo `[NOWE URZĄDZENIE]`, zweryfikujcie się ponownie, zanim zaczniecie rozmawiać o czymś prywatnym.

Krzyżyk chowa okno do zasobnika, ale program działa dalej. Kliknięcie ikony lewym przyciskiem wycisza mikrofon. Kolor ikony oznacza:
- zielony: połączono;
- czerwony: mikrofon wyciszony;
- szary: brak połączenia.

Jeśli gra działa jako administrator i nie przepuszcza skrótów, włącz w ustawieniach **Uruchamiaj jako administrator**.

## C. Pierwszy wspólny wieczór: co sprawdzić

Na wspólnej sesji przejdźcie listy w `docs/acceptance/plan-1.md` … `plan-6.md`. Najważniejsze punkty:

- czy wszyscy się słyszą i jak duże jest opóźnienie;
- **echo**, gdy ktoś gra na głośnikach;
- skróty klawiszowe w trakcie gry na pełnym ekranie;
- regulacja głośności poszczególnych osób, wyciszanie, czat i kopiowanie IP serwera gry;
- czy po krótkim zerwaniu internetu program łączy się sam;
- zużycie pamięci i procesora po kilku godzinach (Menedżer zadań).

Zapiszcie, co nie działa. Takie uwagi poprawia się w kolejnej wersji, która dotrze do wszystkich automatycznie, jeśli zrobiłeś krok A.2.
