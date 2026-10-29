# Wdrożenie serwera i wydawanie nowych wersji

Szczegóły administrowania kontami i pokojami są w [README, sekcje „Serwer” i „Administracja”](../README.md); kanał aktualizacji opisuje [docs/updates.md](updates.md).

Serwer działa jako kontener Dockera. Obraz `ghcr.io/pcpl2/komunikator-server` jest prywatny i powstaje w GitHub Actions (sekcja „Wydania przez GitHub Actions”). Obraz waży kilka MB: statyczny plik na distroless, bez powłoki, uruchamiany jako użytkownik bez uprawnień (uid 65532). Wszystkie dane trzyma wolumen `/data`:
- certyfikat i klucz serwera (`cert.der`, `key.der`);
- bazę kont i pokoi (`komunikator.db`);
- aktualizacje klienta (`updates/`). Ten katalog jest zamontowany z hosta (`./updates` obok `docker-compose.yml`) tylko do odczytu.

## Pierwsze uruchomienie serwera

Na serwerze z Debianem zainstaluj Dockera i plugin compose ([instrukcja Dockera dla Debiana](https://docs.docker.com/engine/install/debian/)).

Zaloguj się do ghcr.io. Potrzebny jest token GitHub (classic) z uprawnieniem `read:packages`: `Settings → Developer settings → Personal access tokens`. Wpisz go jako hasło:

```
docker login ghcr.io -u pcpl2
```

Skopiuj z repozytorium `deploy/docker-compose.yml` i `deploy/komunikator.toml` do jednego katalogu, np. `/opt/komunikator`, i utwórz w nim pusty podkatalog `updates`. W `komunikator.toml` ustaw `public_address` na prawdziwy adres serwera (domena albo IP i port 7700). Ten adres trafia do zaproszeń. Potem uruchom:

```
cd /opt/komunikator
mkdir -p updates
docker compose up -d
docker compose logs komunikator
```

W logu powinna pojawić się linia `fingerprint: …`, czyli odcisk certyfikatu serwera. Odcisk się nie zmienia, dopóki istnieje wolumen `komunikator-data`. Usunięcie wolumenu oznacza nowy odcisk, utratę kont i konieczność rozdania wszystkim nowych zaproszeń.

## Firewall: UDP 7700

Otwórz port **UDP** 7700, a jeśli serwer stoi za routerem, przekieruj go na serwer. Ten sam port obsługuje rozmowy i aktualizacje.

Docker sam dopisuje reguły dla opublikowanych portów i omija przy tym `ufw`. Port z `docker-compose.yml` jest więc otwarty nawet wtedy, gdy `ufw` go nie wpuszcza. Firewall dostawcy (panel VPS) trzeba otworzyć osobno.

## Administracja

Polecenia administracyjne uruchamia się w działającym kontenerze:

```
docker exec komunikator komunikator-server invite --config /etc/komunikator/komunikator.toml
docker exec komunikator komunikator-server users --config /etc/komunikator/komunikator.toml
```

Wygodny skrót do `~/.bashrc`:

```
alias kom='docker exec komunikator komunikator-server'
# potem: kom invite --config /etc/komunikator/komunikator.toml
```

Pierwszy administrator:
1. Utwórz zaproszenie (`invite`).
2. Zarejestruj się nim w programie (`Mam zaproszenie`).
3. Nadaj sobie uprawnienia: `docker exec komunikator komunikator-server make-admin --config /etc/komunikator/komunikator.toml <nazwa>`.

Dla `reset-password` hasło czytane jest ze standardowego wejścia, więc dodaj `-i`: `docker exec -i komunikator komunikator-server reset-password …`.

**Kopia zapasowa** (bazę, certyfikat i klucz serwera trzymaj w bezpiecznym miejscu):

```
docker run --rm -v komunikator_komunikator-data:/data -v "$PWD":/kopia alpine tar czf /kopia/komunikator-data.tgz -C /data .
```

Pełną nazwę wolumenu (z przedrostkiem projektu compose) pokazuje `docker volume ls`.

## Aktualizacja serwera

Gdy workflow „Wydanie serwera” opublikuje nowy obraz:

```
cd /opt/komunikator
docker compose pull
docker compose up -d
```

Restart przerywa trwające rozmowy na kilka sekund, a klienci łączą się ponownie sami. Żeby przypiąć konkretną wersję zamiast `latest`, wpisz w `docker-compose.yml` np. `image: ghcr.io/pcpl2/komunikator-server:0.2.0`.

## Wybór lokalizacji: PL czy FR

Serwer powinien leżeć tam, gdzie opóźnienie do wszystkich graczy jest najmniejsze. Przed wyborem zmierz ping z komputera każdego gracza do obu kandydatów (spec, sekcja 5):

```
ping -n 20 serwer-pl.example.pl
ping -n 20 serwer-fr.example.pl
```

Porównaj średnią i największy czas oraz utratę pakietów. Wybierz lokalizację z najmniejszą największą średnią wśród graczy, a nie z najlepszym pojedynczym wynikiem. Rozmowa przez serwer dodaje dwukrotność pingu do opóźnienia między dwiema osobami.

## Wydania przez GitHub Actions

Oba workflow uruchamia się tylko ręcznie: `Actions → Wydanie serwera / Wydanie klienta → Run workflow`, z wersją wpisaną w polu `version` (np. `0.2.0`). Push niczego nie buduje. Każde uruchomienie:
1. sprawdza, czy wersja ma postać `X.Y.Z` i jest wyższa od ostatniego wydania tej części; tytuł uruchomienia to np. „Wydanie serwera 0.2.0”;
2. wpisuje ją do `Cargo.toml` (i `Cargo.lock`) tylko w kopii roboczej na czas budowania, bez commita;
3. uruchamia testy i buduje;
4. tworzy release na GitHubie z tagiem na commicie, z którego uruchomiono workflow.

Serwer i klient mają osobne wersje:

| Część | Tag | Wynik |
|---|---|---|
| Serwer | `server-vX.Y.Z` | obraz `ghcr.io/pcpl2/komunikator-server:X.Y.Z` i `:latest`; release z digestem obrazu |
| Klient | `client-vX.Y.Z` | release: `komunikator.exe`, `komunikator-cli.exe`, `SHA256SUMS`, a z kluczem także `manifest.bin` i `komunikator-X.Y.Z.exe` |

Wersje w `Cargo.toml` w repozytorium dotyczą tylko budowania lokalnego i nie trzeba ich zmieniać. Wersja ma zawsze postać `x.y.z`, same cyfry. Klient przyjmuje tylko wersję ściśle nowszą od własnej, a workflow pilnuje tego samego.

Obraz serwera dostaje etykietę `org.opencontainers.image.source` z adresem repozytorium. Dzięki temu pakiet w ghcr.io jest prywatny tak jak repozytorium i dziedziczy jego uprawnienia. Workflow loguje się do ghcr.io wbudowanym tokenem, więc nie potrzebuje żadnych sekretów.

### Klucz wydań klienta

Jednorazowo, na swoim komputerze:

```
cargo build --release -p release
target\release\komunikator-release keygen --out release.key
```

Wypisany klucz publiczny wpisz do `crates/client-win/release-key.pub` (pierwszy wiersz niebędący komentarzem) i zrób commit. Workflow klienta odmawia budowania z kluczem z samych zer. Plik `release.key` zachowaj w bezpiecznym miejscu: jego utrata uniemożliwi aktualizowanie już rozdanych programów.

Masz dwie możliwości:

- **Podpis w GitHubie:** dodaj sekret repozytorium `RELEASE_SIGNING_KEY` z zawartością `release.key`. Release dostanie gotową, podpisaną aktualizację. Klucz przestaje wtedy być offline: ktoś, kto przejmie Twoje konto GitHub albo repozytorium, mógłby podpisać i rozesłać graczom dowolny program. Włącz na GitHubie weryfikację dwuetapową i nie dawaj nikomu prawa zapisu do repozytorium.
- **Klucz offline (bez sekretu):** release zawiera niepodpisane `komunikator.exe`. Pobierz go i podpisz u siebie. Podpis sprawdza wersję i klucz wbudowane w exe, więc podpisujesz dokładnie ten plik, który pobrali gracze:
  ```
  komunikator-release sign --key release.key --exe komunikator.exe --out wydanie
  ```
  W katalogu `wydanie` powstaną `komunikator-X.Y.Z.exe` i `manifest.bin`.

### Publikowanie aktualizacji klienta

Skopiuj oba pliki do katalogu `updates` obok `docker-compose.yml` na serwerze. Restart nie jest potrzebny.

```
scp komunikator-0.2.0.exe serwer:/opt/komunikator/updates/
scp manifest.bin serwer:/opt/komunikator/updates/manifest.bin.tmp
ssh serwer 'mv /opt/komunikator/updates/manifest.bin.tmp /opt/komunikator/updates/manifest.bin'
```

Kolejność ma znaczenie: najpierw plik wydania, a manifest na końcu, pod tymczasową nazwą zamienianą jednym `mv`. Dzięki temu klient nigdy nie zobaczy manifestu wskazującego na brakujący albo niepełny plik.

Wymagania dla plików:
- muszą być zwykłymi plikami, nie dowiązaniami, bo serwer pomija dowiązania;
- muszą być czytelne dla wszystkich (tryb 644, domyślny), bo kontener działa jako uid 65532.

Klienci znajdą wydanie przy następnym sprawdzeniu (przy starcie i co 6 godzin), a pobiorą je i zainstalują, gdy nie będą w rozmowie. Dokładną nazwę pliku wydania podaje `komunikator-release sign` i pole `file` manifestu. Stare pliki `komunikator-*.exe` z `updates` można po prostu usunąć.
