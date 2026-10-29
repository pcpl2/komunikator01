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

Klucz pozwala później rozsyłać aktualizacje automatycznie. Bez niego workflow klienta się nie uruchomi.

```
cargo build --release -p release
target\release\komunikator-release keygen --out release.key
```

1. W repozytorium na GitHubie: `Settings → Secrets and variables → Actions → New repository secret`. Nazwa `RELEASE_SIGNING_KEY`, wartość to zawartość pliku `release.key`. Workflow sam wyliczy z niego klucz publiczny i wbuduje go w program, a w repozytorium nic nie trzeba zmieniać.
2. Plik `release.key` przenieś w bezpieczne miejsce, np. na pendrive i do menedżera haseł. Nigdy nie wrzucaj go do repozytorium.
3. Jeśli wolisz, żeby GitHub nie miał klucza tajnego, zamiast sekretu dodaj zmienną `RELEASE_PUBLIC_KEY` z kluczem publicznym (zakładka `Variables`) i podpisuj aktualizacje u siebie. Różnice: `docs/wydanie.md`, sekcja „Klucz wydań klienta”.

### 3. Wydanie obrazu serwera (GitHub Actions)

1. W repozytorium na GitHubie wejdź w `Actions → Wydanie serwera → Run workflow`. Zostaw gałąź `main` i wpisz wersję `0.1.0`.
2. Workflow uruchamia testy, buduje obraz Dockera i publikuje go w ghcr.io jako:
   - `ghcr.io/pcpl2/komunikator-server:0.1.0`;
   - `ghcr.io/pcpl2/komunikator-server:latest`.

   Tworzy też release `server-v0.1.0` z digestem obrazu, bez plików do pobrania.
3. Obraz widać w repozytorium w `Packages` po prawej stronie. Jest prywatny tak jak repozytorium.

Każde kolejne wydanie serwera dostaje wyższą wersję wpisaną w tym samym polu.

### 4. Postaw serwer (Debian z Dockerem)

Docker i dostęp do prywatnego ghcr.io (logowanie, `docker pull` obrazu `ghcr.io/pcpl2/komunikator-server`) zapewnia administrator serwera.

1. Repozytorium jest prywatne, więc pliki konfiguracji skopiuj ze swojego komputera:
   ```
   ssh serwer 'sudo install -d -o $USER /opt/komunikator'
   scp deploy/docker-compose.yml deploy/komunikator.toml serwer:/opt/komunikator/
   ```
2. W `/opt/komunikator/komunikator.toml` ustaw `public_address` na prawdziwy adres serwera z portem, np. `"gra.example.pl:7700"`. Ten adres trafia do zaproszeń.
3. Otwórz port **UDP** 7700 w firewallu dostawcy serwera (panel VPS), a jeśli serwer stoi za routerem, przekieruj na nim UDP 7700. TCP nie wystarczy. W samym systemie Docker otwiera ten port sam, z pominięciem `ufw`.
4. Uruchom serwer:
   ```
   cd /opt/komunikator
   mkdir -p data updates
   sudo chown 65532:65532 data
   docker compose up -d
   docker compose logs komunikator
   ```

W logu powinny być linie `listening: 0.0.0.0:7700` i `fingerprint: …` (64 znaki). Zanotuj odcisk: przyda się w kroku 5. Nie zmieni się, dopóki istnieje katalog `/opt/komunikator/data`.

Katalog `data` trzyma certyfikat i klucz serwera oraz bazę kont. To zwykły katalog na hoście, a nie wolumen Dockera, więc nie zniknie przy `docker compose down -v` ani `docker volume prune`. Należy do uid 65532, bo z tym użytkownikiem działa serwer. Rób jego kopię zapasową: `docs/wydanie.md`, sekcja „Administracja”. Podkatalog `updates` jest zamontowany w kontenerze tylko do odczytu. Trafiają do niego aktualizacje klienta (patrz „Kolejne wersje” niżej).

### 5. Wydanie klienta z wbudowanym serwerem

1. W repozytorium na GitHubie (`Settings → Secrets and variables → Actions → New repository secret`) dodaj dwa sekrety:
   - `SERVER_ADDRESS`: ten sam adres co `public_address`, np. `gra.example.pl:7700`;
   - `SERVER_FINGERPRINT`: 64 znaki z linii `fingerprint:`.
2. Uruchom `Actions → Wydanie klienta → Run workflow` z wersją `0.1.0`.
3. Powstanie release `client-v0.1.0` z `komunikator.exe` dla graczy. Program ma wbudowany adres serwera i klucz wydań, a okno logowania pyta tylko o nazwę i hasło. Bez sekretów z kroków 2 i 5 workflow się zatrzyma.

### 6. Twoje konto administratora

```
docker exec komunikator komunikator-server invite --config /etc/komunikator/komunikator.toml
```

1. Skopiuj wypisany kod `kom1-…`.
2. Zarejestruj się nim w programie, tak jak gracz w części B.
3. Nadaj sobie uprawnienia administratora:

```
docker exec komunikator komunikator-server make-admin --config /etc/komunikator/komunikator.toml <twoja-nazwa>
```

### 7. Zaproszenia dla znajomych

Wygeneruj osobny kod dla każdej osoby tym samym poleceniem `invite`. Kod jest jednorazowy i domyślnie ważny 7 dni.

Wyślij każdemu prywatną wiadomością `komunikator.exe` z release `client-v0.1.0` i jego kod, nie na publicznym kanale. Repozytorium jest prywatne, więc znajomi nie pobiorą pliku z GitHuba sami.

Później przydadzą się polecenia (każde z `docker exec komunikator komunikator-server … --config /etc/komunikator/komunikator.toml`):
- `users`: kto ma konto;
- `invites`: które zaproszenia zostały użyte;
- `block <nazwa>`: zablokowanie konta.

### Kolejne wersje

**Serwer:**
1. `Actions → Wydanie serwera → Run workflow` z wyższą wersją.
2. Na serwerze:
   ```
   cd /opt/komunikator
   docker compose pull
   docker compose up -d
   ```
   Trwające rozmowy zerwą się na kilka sekund, a programy połączą się same.

**Klient:**
1. `Actions → Wydanie klienta → Run workflow` z wyższą wersją.
2. Z release pobierz `komunikator-X.Y.Z.exe` i `manifest.bin`.
3. Wgraj je do `/opt/komunikator/updates`. Najpierw plik wydania, a manifest na końcu pod tymczasową nazwą, zamienianą jednym `mv`:
   ```
   scp komunikator-0.2.0.exe serwer:/opt/komunikator/updates/
   scp manifest.bin serwer:/opt/komunikator/updates/manifest.bin.tmp
   ssh serwer 'mv /opt/komunikator/updates/manifest.bin.tmp /opt/komunikator/updates/manifest.bin'
   ```
   Restart nie jest potrzebny. Programy graczy pobiorą aktualizację same, gdy nie będą w rozmowie.

## B. Gracz

1. Utwórz folder, np. `C:\Gry\Komunikator`, i wrzuć do niego `komunikator.exe`. Program musi móc zapisywać w tym folderze, więc nie wybieraj `Program Files`.
2. Uruchom program. Jeśli Windows pokaże „System Windows ochronił ten komputer”, kliknij **Więcej informacji → Uruchom mimo to**. Program nie ma płatnego podpisu wydawcy, dlatego to ostrzeżenie pojawia się tylko za pierwszym razem.
3. Wybierz **Mam zaproszenie** i wklej cały kod `kom1-…`. Kto ma już konto, wybiera **Mam już konto** i wpisuje tylko nazwę i hasło.
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

Zapiszcie, co nie działa. Takie uwagi poprawia się w kolejnej wersji, która dotrze do wszystkich automatycznie: workflow „Wydanie klienta” i wgranie aktualizacji na serwer opisuje `docs/wydanie.md`.
