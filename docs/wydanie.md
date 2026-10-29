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
