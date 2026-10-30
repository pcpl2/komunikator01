# Wdrożenie serwera i wydawanie nowych wersji

Szczegóły administrowania kontami i pokojami są w [README, sekcje „Serwer” i „Administracja”](../README.md); kanał aktualizacji opisuje [docs/updates.md](updates.md).

Serwer działa jako kontener Dockera. Obraz `ghcr.io/pcpl2/komunikator-server` jest prywatny i powstaje w GitHub Actions (sekcja „Wydania przez GitHub Actions”). Obraz waży kilka MB: statyczny plik na distroless, bez powłoki, uruchamiany jako użytkownik bez uprawnień (uid 65532). Wszystkie dane trzyma katalog `/opt/komunikator/data` na hoście, zamontowany w kontenerze jako `/data`. To zwykły katalog, nie wolumen Dockera, więc nie zniknie przy `docker compose down -v` ani `docker volume prune`. Są w nim:
- certyfikat i klucz serwera (`cert.der`, `key.der`);
- bazę kont i pokoi (`komunikator.db`);
- aktualizacje klienta (`updates/`). Ten katalog jest zamontowany z hosta (`./updates` obok `docker-compose.yml`) tylko do odczytu.

## Pierwsze uruchomienie serwera

Docker z pluginem compose i dostęp do prywatnego ghcr.io (logowanie, pobranie obrazu) zapewnia administrator serwera.

Skopiuj z repozytorium `deploy/docker-compose.yml` i `deploy/komunikator.toml` do jednego katalogu, np. `/opt/komunikator`. Utwórz w nim podkatalogi `data` (dane serwera; właścicielem musi być uid 65532, z którym działa serwer) i `updates`. W `komunikator.toml` ustaw `public_address` na prawdziwy adres serwera (domena albo IP i port 7700). Ten adres trafia do zaproszeń. Potem uruchom:

```
cd /opt/komunikator
mkdir -p data updates
sudo chown 65532:65532 data
docker compose up -d
docker compose logs komunikator
```

W logu powinna pojawić się linia `fingerprint: …`, czyli odcisk certyfikatu serwera. Odcisk się nie zmienia, dopóki istnieje katalog `data`. Jego utrata oznacza nowy odcisk, utratę kont i konieczność rozdania wszystkim nowych zaproszeń, więc rób kopię zapasową.

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
cd /opt/komunikator
docker compose stop
sudo tar czf komunikator-data-$(date +%F).tgz data
docker compose start
```

Zatrzymanie na kilka sekund daje spójną kopię bazy. Przywrócenie: rozpakuj archiwum w `/opt/komunikator` i uruchom kontener. Archiwum zawiera klucz prywatny serwera, więc trzymaj je poza serwerem i tylko dla siebie.

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
| Klient | `client-vX.Y.Z` | release: instalator `komunikator-setup-X.Y.Z.exe`, `komunikator.exe` (wersja przenośna), `komunikator-cli.exe`, `SHA256SUMS`, a z kluczem także `manifest.bin` i `komunikator-X.Y.Z.exe` |

Instalator (Inno Setup, `installer/komunikator.iss`) workflow buduje z tego samego `komunikator.exe`. Instaluje dla bieżącego użytkownika, bez UAC, do `%LOCALAPPDATA%\Programs\Komunikator`, bo program aktualizuje się sam i potrzebuje folderu z prawem zapisu. Pyta o uruchamianie z Windows i skrót na pulpicie. Gdy program działa, prosi o jego zamknięcie. Jeśli na runnerze nie ma Inno Setup, krok instaluje go przez `choco`. Lokalnie: `ISCC.exe /DAppVersion=X.Y.Z installer\komunikator.iss` (bierze `target\release\komunikator.exe`, wynik w `dist`). Ikonę programu i instalatora rysuje `crates/client-win/icon/make_icon.py`.

Wersje w `Cargo.toml` w repozytorium dotyczą tylko budowania lokalnego i nie trzeba ich zmieniać. Wersja ma zawsze postać `x.y.z`, same cyfry. Klient przyjmuje tylko wersję ściśle nowszą od własnej, a workflow pilnuje tego samego.

Obraz serwera dostaje etykietę `org.opencontainers.image.source` z adresem repozytorium. Dzięki temu pakiet w ghcr.io jest prywatny tak jak repozytorium i dziedziczy jego uprawnienia. Workflow loguje się do ghcr.io wbudowanym tokenem, więc nie potrzebuje żadnych sekretów.

### Klucz wydań klienta

Jednorazowo, na swoim komputerze:

```
cargo build --release -p release
target\release\komunikator-release keygen --out release.key
```

Plik `release.key` zachowaj w bezpiecznym miejscu: jego utrata uniemożliwi aktualizowanie już rozdanych programów.

Workflow klienta wbudowuje w program klucz publiczny, czyli nadpisuje `crates/client-win/release-key.pub` tylko w swojej kopii roboczej. Szuka go w tej kolejności:
1. wylicza go z sekretu `RELEASE_SIGNING_KEY`;
2. bierze ze zmiennej repozytorium `RELEASE_PUBLIC_KEY`;
3. bierze z pliku w repozytorium.

Klucza z samych zer (wartość w repozytorium, używana przy budowaniu lokalnym) workflow nie przyjmuje. Programy zbudowane lokalnie się nie aktualizują.

Masz dwie możliwości:

- **Podpis w GitHubie:** dodaj sekret repozytorium `RELEASE_SIGNING_KEY` z zawartością `release.key` (`Settings → Secrets and variables → Actions`). Release dostanie gotową, podpisaną aktualizację. Klucz przestaje wtedy być offline: ktoś, kto przejmie Twoje konto GitHub albo repozytorium, mógłby podpisać i rozesłać graczom dowolny program. Włącz na GitHubie weryfikację dwuetapową i nie dawaj nikomu prawa zapisu do repozytorium.
- **Klucz offline:** zamiast sekretu dodaj zmienną repozytorium `RELEASE_PUBLIC_KEY` z kluczem publicznym (`komunikator-release public --key release.key`). Release zawiera wtedy niepodpisane `komunikator.exe`. Pobierz go i podpisz u siebie. Podpis sprawdza wersję i klucz wbudowane w exe, więc podpisujesz dokładnie ten plik, który pobrali gracze:
  ```
  komunikator-release sign --key release.key --exe komunikator.exe --out wydanie
  ```
  W katalogu `wydanie` powstaną `komunikator-X.Y.Z.exe` i `manifest.bin`.

### Serwer wbudowany w program

Workflow klienta wkompilowuje w program adres i odcisk certyfikatu serwera z sekretów (albo zmiennych) repozytorium:
- `SERVER_ADDRESS`, np. `gra.example.pl:7700`;
- `SERVER_FINGERPRINT`, czyli 64 znaki z linii `fingerprint:` w logu serwera.

W efekcie:
- okno „Mam już konto” pyta tylko o nazwę i hasło;
- `komunikator-cli --user ola` łączy się z tym serwerem bez `--server` i `--fingerprint`.

Oba sekrety są wymagane: bez nich albo przy złej postaci workflow się zatrzymuje. Zaproszenia działają jak dotąd. Konto założone zaproszeniem do innego serwera zostaje przy swoim adresie. Program zbudowany lokalnie, bez wbudowanego serwera, pokazuje pola adresu i odcisku jak dawniej.

Odcisk zmienia się wyłącznie po utracie katalogu `data` (bez kopii zapasowej). Wtedy trzeba zaktualizować sekret i wydać nowego klienta.

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
