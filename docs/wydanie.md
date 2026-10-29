# Wdrożenie serwera i wydawanie nowych wersji

Szczegóły administrowania kontami i pokojami są w [README, sekcje „Serwer” i „Administracja”](../README.md); kanał aktualizacji opisuje [docs/updates.md](updates.md). Tu zebrano kroki dla Debiana z systemd.

## Budowanie serwera na Debianie

```
sudo apt install build-essential pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
git clone <adres repozytorium> komunikator && cd komunikator
cargo build --release -p server
```

Powstaje jeden plik: `target/release/komunikator-server`. Serwer nie koduje dźwięku (nie potrzebuje libopus), a SQLite jest wbudowany w plik (`rusqlite` z `bundled`), więc do budowania wystarczą `build-essential` i `pkg-config`, a na serwerze nic więcej nie trzeba instalować.

## Instalacja

```
sudo install -m 755 target/release/komunikator-server /usr/local/bin/
sudo useradd --system --home-dir /var/lib/komunikator --shell /usr/sbin/nologin komunikator
sudo install -d /etc/komunikator
sudo install -m 644 deploy/komunikator.toml /etc/komunikator/komunikator.toml
sudo install -m 644 deploy/komunikator.service /etc/systemd/system/komunikator.service
sudoedit /etc/komunikator/komunikator.toml      # ustaw public_address
sudo systemctl daemon-reload
sudo systemctl enable --now komunikator
```

Katalog `/var/lib/komunikator` zakłada systemd (`StateDirectory`, tryb 0700, właściciel `komunikator`). Usługa loguje do journald i nie zapisuje treści rozmów:

```
journalctl -u komunikator -f
```

Przy każdym starcie serwer wypisuje do logu `fingerprint: …`, czyli odcisk certyfikatu serwera.

## Firewall: UDP 7700

nftables (dopisz regułę do łańcucha `input` w `/etc/nftables.conf` i wykonaj `sudo systemctl reload nftables`):

```
udp dport 7700 accept
```

ufw:

```
sudo ufw allow 7700/udp
```

Za routerem trzeba jeszcze przekierować UDP 7700 na serwer. Ten sam port obsługuje rozmowy i aktualizacje.

## Pierwszy administrator

Polecenia uruchamiaj jako użytkownik usługi:

```
sudo -u komunikator komunikator-server invite --config /etc/komunikator/komunikator.toml
# zarejestruj się w programie (Mam zaproszenie), potem:
sudo -u komunikator komunikator-server make-admin --config /etc/komunikator/komunikator.toml ala
```

## Wybór lokalizacji: PL czy FR

Serwer powinien leżeć tam, gdzie opóźnienie do wszystkich graczy jest najmniejsze. Przed wyborem zmierz ping z komputera każdego gracza do obu kandydatów (spec, sekcja 5):

```
ping -n 20 serwer-pl.example.pl
ping -n 20 serwer-fr.example.pl
```

Porównaj średnią i największy czas oraz utratę pakietów. Wybierz lokalizację z najmniejszą największą średnią wśród graczy, a nie z najlepszym pojedynczym wynikiem. Rozmowa przez serwer dodaje dwukrotność pingu do opóźnienia między dwiema osobami.

## Publikowanie wydania

Klucz podpisujący leży tylko na Twoim komputerze, nigdy na serwerze.

1. **Jednorazowo:** na komputerze offline `komunikator-release keygen --out release.key`. Skopiuj wypisany klucz publiczny do `crates/client-win/release-key.pub` (pierwszy wiersz niebędący komentarzem) i zrób commit. Plik `release.key` zachowaj w bezpiecznym miejscu; jego utrata uniemożliwi aktualizowanie już rozdanych programów.
2. Zwiększ wersję w `Cargo.toml` (`[workspace.package] version`). Wersja ma zawsze postać `x.y.z`, same cyfry (bez dopisków typu `-rc1`). Klient przyjmie tylko wersję ściśle nowszą od własnej.
3. Zbuduj: `cargo build --release` (Windows). W `target\release\` powstaną `komunikator.exe` (to jest wydanie) i narzędzie `komunikator-release.exe`.
4. Podpisz:
   ```
   target\release\komunikator-release sign --key release.key --exe target\release\komunikator.exe --out wydanie
   ```
   Wersję narzędzie odczytuje ze znacznika wbudowanego w `komunikator.exe`; `--version x.y.z` jest tylko dodatkowym sprawdzeniem i musi się z nią zgadzać. Podpis zostanie odrzucony, jeśli plik nie jest programem okienkowym (np. `komunikator-cli.exe` albo serwer), jeśli zbudowano go z kluczem z samych zer albo z innym kluczem publicznym niż ten, którym podpisujesz. W katalogu `wydanie` powstaną exe pod nazwą wydania i `manifest.bin`.
5. Wgraj na serwer do `/var/lib/komunikator/updates/` (katalog musi należeć do `komunikator`). Najpierw plik wydania, potem `manifest.bin` pod tymczasową nazwą, którą na końcu jednym `mv` podmieniasz na właściwą: klient nigdy nie zobaczy manifestu wskazującego na brakujący albo niepełny plik.
   ```
   scp wydanie/komunikator-0.2.0.exe wydanie/manifest.bin serwer:/tmp/
   ssh serwer 'set -e
     sudo install -d -m 755 -o komunikator -g komunikator /var/lib/komunikator/updates
     sudo install -m 644 -o komunikator -g komunikator /tmp/komunikator-0.2.0.exe /var/lib/komunikator/updates/
     sudo install -m 644 -o komunikator -g komunikator /tmp/manifest.bin /var/lib/komunikator/updates/manifest.bin.tmp
     sudo mv /var/lib/komunikator/updates/manifest.bin.tmp /var/lib/komunikator/updates/manifest.bin'
   ```
   Dokładną nazwę pliku wydania podaje `komunikator-release sign` i pole `file` manifestu. Restart serwera nie jest potrzebny.

Klienci znajdą wydanie przy następnym sprawdzeniu, a pobiorą je i zainstalują, gdy nie będą w rozmowie.

## Wydania przez GitHub Actions

Dwa workflow uruchamia się tylko ręcznie: `Actions → Wydanie serwera / Wydanie klienta → Run workflow`. Push niczego nie buduje. Każde uruchomienie:

1. czyta wersję z `Cargo.toml` i przerywa, jeśli tag już istnieje;
2. uruchamia testy, buduje i tworzy release na GitHubie z tagiem;
3. jeśli zaznaczono `deploy`, wgrywa wynik na serwer.

Serwer i klient mają osobne wersje:

| Część | Wersja w | Tag | Pliki w release |
|---|---|---|---|
| Serwer | `crates/server/Cargo.toml` (`version`) | `server-vX.Y.Z` | `komunikator-server-X.Y.Z-linux-x86_64` (statyczny, musl), `SHA256SUMS` |
| Klient | `Cargo.toml` (`[workspace.package] version`) | `client-vX.Y.Z` | `komunikator.exe`, `komunikator-cli.exe`, `manifest.bin`, `SHA256SUMS` |

Nowe wydanie: podnieś wersję, zrób commit, wypchnij, uruchom workflow. Wdrożenie serwera restartuje usługę, więc trwające rozmowy zerwą się na kilka sekund, a klienci połączą się sami. Wdrożenie klienta tylko wgrywa aktualizację do `/var/lib/komunikator/updates/`. Programy graczy pobiorą ją, gdy nie będą w rozmowie.

### Jednorazowe przygotowanie

**Środowisko `produkcja`** (`Settings → Environments → New environment`, nazwa `produkcja`): włącz `Required reviewers`, a wgranie na serwer będzie czekało na Twoje zatwierdzenie. Sekrety wdrożenia dodaj w tym środowisku:

| Sekret | Wartość |
|---|---|
| `DEPLOY_HOST` | adres serwera |
| `DEPLOY_PORT` | port SSH (opcjonalny, domyślnie 22) |
| `DEPLOY_USER` | użytkownik do wdrożeń, np. `deploy` |
| `DEPLOY_SSH_KEY` | klucz prywatny SSH tego użytkownika (osobny, tylko do wdrożeń) |
| `DEPLOY_KNOWN_HOSTS` | wynik `ssh-keyscan -p <port> <adres>` sprawdzony z odciskiem z serwera |

**Sekret repozytorium `RELEASE_SIGNING_KEY`** (opcjonalny): zawartość pliku `release.key`. Z nim workflow klienta sam podpisuje wydanie i może je wgrać na serwer. Bez niego release dostaje niepodpisane `komunikator.exe` i `komunikator-cli.exe`, uruchomienie z `deploy` jest odrzucane, a aktualizację podpisujesz i wgrywasz ręcznie, jak w sekcji „Publikowanie wydania”.

**Użytkownik wdrożeń na serwerze**:

```
sudo useradd --create-home --shell /bin/bash deploy
sudo install -d -m 700 -o deploy -g deploy /home/deploy/.ssh
echo '<klucz publiczny SSH>' | sudo tee /home/deploy/.ssh/authorized_keys
sudo chown deploy:deploy /home/deploy/.ssh/authorized_keys
sudo chmod 600 /home/deploy/.ssh/authorized_keys
sudo visudo -f /etc/sudoers.d/komunikator-deploy
```

Do pliku sudoers wpisz:

```
deploy ALL=(root) NOPASSWD: /usr/bin/install, /usr/bin/mv, /usr/bin/systemctl restart komunikator
```

### Klucz wydań w GitHubie: co to zmienia

Po dodaniu `RELEASE_SIGNING_KEY` klucz przestaje być offline. Ktoś, kto przejmie Twoje konto GitHub albo repozytorium, mógłby podpisać i rozesłać graczom dowolny program. Dlatego:

- włącz na GitHubie weryfikację dwuetapową;
- wymagaj zatwierdzenia środowiska `produkcja`;
- nie dawaj nikomu prawa zapisu do repozytorium.

Jeśli wolisz trzymać klucz offline, nie dodawaj sekretu i uruchamiaj workflow klienta bez `deploy`. Pobierz z release `komunikator.exe`, podpisz go lokalnie (`komunikator-release sign --exe komunikator.exe …`) i wgraj, jak w sekcji „Publikowanie wydania”. Podpis sprawdza wersję i klucz wbudowane w exe, więc podpisujesz dokładnie ten plik, który pobrali gracze.
