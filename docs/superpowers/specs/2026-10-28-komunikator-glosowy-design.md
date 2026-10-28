# Komunikator głosowy dla graczy — projekt

Data: 2026-10-28
Status: szkic do akceptacji

Oznaczenia: **[ustalone]** — decyzja użytkownika; **[decyzja projektanta]** — wybór domyślny, do zmiany po przeczytaniu; **[dla wykonawcy]** — świadomie zostawione budującemu.

---

## 1. Intencja

Mała grupa znajomych (do 5 osób) rozmawia głosowo podczas wspólnego grania na Windows. Obecne narzędzia zawodzą:

- **Discord** — bardzo ciężki (RAM/CPU), zabiera zasoby grze.
- **Telegram (rozmowy grupowe)** — zrywa połączenie w trakcie i się zacina.
- **TeamSpeak** — przestał im odpowiadać; wolą własne rozwiązanie.

Celem jest komunikator, którego nie widać w zużyciu zasobów i który nie zrywa rozmowy przez cały wieczór. Natywny — **żadnego WebView, Electrona, przeglądarki** **[ustalone]**.

## 2. Cele

1. **Lekkość:** klient w trakcie rozmowy < 30 MB RAM, < 1% CPU na typowym współczesnym procesorze; brak wpływu na FPS gry. **[decyzja projektanta — progi]**
2. **Stabilność:** rozmowa trwa godzinami bez zerwań; po chwilowej utracie sieci klient sam wraca do pokoju w ciągu kilku sekund, bez działania użytkownika.
3. **Niskie opóźnienie:** usta→ucho < 150 ms między graczami w Polsce/Francji przy serwerze w PL lub FR (cel: ~80–100 ms).
4. **Otwarty mikrofon** z automatycznym wykrywaniem mowy, tłumieniem szumów i **usuwaniem echa** (część graczy gra na głośnikach) **[ustalone]**.
5. **Kilka pokoi**, swobodne przechodzenie między nimi **[ustalone]**.
6. **Głośność każdej osoby osobno** + wyciszenie mikrofonu (mute) i wyciszenie wszystkich (deafen) **[ustalone]**.
7. **Proste konta** (login + hasło) **[ustalone]**.
8. **Szyfrowanie end-to-end** — serwer nie ma dostępu do treści rozmów (głos i czat) **[ustalone]**.
9. **Wyciszanie mikrofonu globalnym skrótem klawiszowym ORAZ przyciskiem w oknie** (gracze mają kilka monitorów, okno może stać na drugim) **[ustalone]**.
10. **Prosty czat tekstowy w pokoju** — szybka wiadomość, np. przesłanie IP serwera gry **[ustalone]**.

## 3. Nie-cele

Wideo, udostępnianie ekranu, przesyłanie plików, emotki/reakcje, edycja wiadomości, boty, nagrywanie, klient mobilny/macOS/Linux, federacja wielu serwerów, połączenia P2P, push-to-talk jako tryb główny (może dojść później, nie teraz).

**Nakładki w grach — wykluczone** **[ustalone]**.

## 4. Anty-cele (porażka, nawet jeśli „działa”)

- Spadek FPS lub mikroprzycięcia w grze z powodu komunikatora.
- **Windows ściszający dźwięk gry** w momencie startu rozmowy (systemowy „ducking” dla aplikacji komunikacyjnych) — musi być wyłączony programowo.
- Echo słyszane przez innych, gdy ktoś gra na głośnikach.
- Rozmowa zrywana przy przełączaniu tras sieciowych (problem Telegrama) — dlatego jedna stała ścieżka przez serwer.
- Ciężki runtime (WebView, .NET z dużym narzutem, Electron).
- Serwer mogący odsłuchać rozmowę lub przeczytać czat.
- Czat rozrastający się w „drugiego Discorda” (kanały, wątki, historia na lata).

## 5. Ograniczenia

- Klient: **Windows 10/11 x64** **[ustalone: Windows]**.
- Serwer: **jeden**, **Debian**, jeszcze niewybrany — możliwe lokalizacje: **Polska lub Francja** **[ustalone]**. Wybór między nimi: ta, do której suma pingów graczy jest mniejsza (zmierzyć przed zakupem) **[decyzja projektanta — kryterium]**. Projekt nie zależy od tego wyboru.
- Skala: projektowane na ≤ 5 osób jednocześnie; nie może się wysypać przy ~10–20 (np. goście), ale nie optymalizujemy pod to.
- Język: **Rust** dla klienta i serwera **[ustalone — zaakceptowana propozycja]**.

## 6. Architektura

```
 [Klient Win] ─┐                                   ┌─ [Klient Win]
 [Klient Win] ─┼── QUIC/UDP (1 port) ──► [Serwer Debian] ──┼─ [Klient Win]
               └── sterowanie + czat + datagramy audio (treść zaszyfrowana E2E) ──┘
```

- **Model: serwer przekazuje dźwięk (SFU), nie miksuje** **[ustalone: wariant 2]**. Każdy klient wysyła jeden strumień; serwer rozsyła go pozostałym w pokoju. Miksowanie po stronie klienta.
- Serwer rozwiązuje też problem zmiennych IP — klienci łączą się zawsze na jego stały adres/domenę.

### 6.1 Transport **[decyzja projektanta]**

- **QUIC (biblioteka `quinn`)** na jednym porcie UDP:
  - niezawodne strumienie — sterowanie (logowanie, pokoje, lista osób, wymiana kluczy) i czat,
  - **datagramy QUIC** (bez retransmisji) — audio.
- Uzasadnienie: jedno połączenie, wbudowany TLS 1.3 (chroni logowanie i metadane przed obserwatorem w sieci), keep-alive, migracja połączenia przy zmianie IP klienta (np. restart routera) bez zrywania sesji.
- Certyfikat serwera: Let's Encrypt, jeśli jest domena; w przeciwnym razie samopodpisany z odciskiem przypiętym w konfiguracji klienta. **[dla wykonawcy]**
- Keep-alive co ~5 s; timeout bezczynności ~15 s; po zerwaniu — ponowne łączenie z narastającym odstępem (0,5 s → max 5 s), automatyczny powrót do ostatniego pokoju.

### 6.2 Audio (klient)

- **Wejście/wyjście: WASAPI** (przez `windows-rs`, tryb współdzielony, zdarzeniowy), 48 kHz mono do przetwarzania. Wybór urządzeń w ustawieniach + śledzenie zmiany urządzenia domyślnego (podłączenie słuchawek w trakcie).
- **Kodek: Opus**, ramki 20 ms, tryb VoIP, **CBR ~32 kbit/s** (CBR, bo VBR pod szyfrowaniem zdradza treść przez rozmiary pakietów), FEC włączone, PLC przy utracie.
- **Przetwarzanie mikrofonu:** usuwanie echa (AEC) → tłumienie szumów (NS) → automatyczna regulacja wzmocnienia (AGC) → wykrywanie mowy (VAD). Przy ciszy klient nie wysyła pakietów (z krótkim „ogonem” ~300 ms po końcu mowy).
- **AEC — decyzja do podjęcia w fazie 1 (spike)** **[decyzja projektanta: kryterium]**:
  - Opcja A: wbudowane przetwarzanie Windows dla komunikacji (strumień w kategorii `Communications`; na Windows 11 `IAcousticEchoCancellationControl` do wskazania urządzenia referencyjnego). Zaleta: natywne, zero dodatkowego CPU. Wada: jakość zależy od sterownika karty dźwiękowej.
  - Opcja B: moduł audio WebRTC (AEC3 + NS + AGC), np. przez crate `webrtc-audio-processing`. Zaleta: przewidywalna jakość na każdym sprzęcie.
  - **Start od A** (natywne, lżejsze), przetwarzanie jako wymienny etap w kodzie, tak by B dało się dołożyć bez przebudowy **[decyzja projektanta]**.
  - Gracze przetestują echo **na gotowej wersji**, a nie w spike'u **[ustalone]**. Jeśli A daje słyszalne echo choć u jednego z nich — dokładamy B (jako opcję w ustawieniach lub domyślnie, zależnie od wyników).
- **Wyłączenie systemowego ściszania innych aplikacji:** `IAudioClientDuckingControl::SetDuckingOptionsForCurrentStream(AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS)` (lub równoważne) — obowiązkowe (anty-cel). Zweryfikować w spike'u.
- **Odbiór:** osobny adaptacyjny bufor jittera na nadawcę (cel 40–60 ms, rośnie przy niestabilnej sieci), dekodowanie, głośność per osoba (0–200%), miks, wyjście.
- **Wątki:** wątek audio o podwyższonym priorytecie (MMCSS „Pro Audio”/„Audio”), bez alokacji w gorącej ścieżce; sieć w osobnym wątku (tokio lub prosta pętla — **[dla wykonawcy]**).

### 6.3 Szyfrowanie end-to-end **[ustalone: E2E; szczegóły — decyzja projektanta]**

- **Tożsamość:** każde urządzenie generuje parę kluczy (Ed25519 do podpisu, X25519 do uzgadniania) przy pierwszym uruchomieniu; przechowywana lokalnie, chroniona DPAPI. Klucz publiczny rejestrowany na serwerze przy koncie.
- **Kanały parami:** do wymiany kluczy między dwoma klientami — protokół Noise (crate `snow`, np. wzorzec `KK`/`IK`), wiadomości przekazywane przez serwer strumieniem sterującym.
- **Klucze nadawcy (sender keys):** każdy uczestnik pokoju ma własny losowy klucz symetryczny, którym szyfruje swoje pakiety audio i wiadomości czatu raz (serwer rozsyła ten sam pakiet wszystkim). Klucz rozsyłany pozostałym przez kanały parami. Audio i czat używają osobnych kluczy wyprowadzonych (HKDF) z klucza nadawcy, żeby liczniki/nonce się nie zderzały.
- **Rotacja:** gdy ktoś wchodzi do pokoju lub z niego wychodzi, każdy uczestnik generuje nowy klucz nadawcy i rozsyła go. Nowy klucz ma numer epoki; przez krótki czas akceptujemy poprzednią epokę, żeby rotacja nie powodowała przerwy w dźwięku.
- **Pakiet audio:** `[id nadawcy | epoka | licznik]` (jawny, jako AAD) + `ChaCha20-Poly1305(Opus)`. Nonce z licznika. Odrzucanie powtórzeń.
- **Zaufanie (TOFU):** klient zapamiętuje klucz tożsamości każdego znajomego przy pierwszym kontakcie i **wyraźnie ostrzega**, gdy się zmieni (np. nowy komputer znajomego). Opcjonalnie: krótki kod bezpieczeństwa do porównania głosowo.
- **Model zagrożenia: serwer traktujemy jako potencjalnie wrogi** (np. przejęty VPS), nie tylko „ciekawski” **[decyzja projektanta]**. Konsekwencje, lekkie przy 5 osobach:
  - każdy uczestnik ogłasza wejście do pokoju wiadomością **podpisaną swoim kluczem tożsamości** (pokój, epoka, znacznik czasu); klient uznaje za członka tylko kogoś z ważnym podpisem;
  - **klient rozsyła swój klucz nadawcy wyłącznie osobom, które sam wyświetla na liście pokoju** — nie da się dodać „ducha”, którego nikt nie widzi;
  - osoba z kluczem nieznanym wcześniej (nowe konto / nowe urządzenie) jest w interfejsie wyraźnie oznaczona, dopóki ktoś jej nie zweryfikuje;
  - kod bezpieczeństwa pozostaje opcjonalny (wystarczy do wykrycia podmiany klucza przy pierwszym kontakcie, ale nie wymuszamy).
- **Wiele urządzeń:** jedno konto może mieć kilka urządzeń (każde z własnym kluczem tożsamości, lista kluczy przy koncie), ale **aktywna jest jedna sesja naraz** — zalogowanie się na innym komputerze rozłącza poprzednią. TOFU działa per urządzenie; nowe urządzenie znajomego pokazuje się jako „nowe urządzenie użytkownika X” (informacja), a zmiana klucza istniejącego urządzenia jako ostrzeżenie **[decyzja projektanta]**.
- Serwer widzi tylko metadane: kto w jakim pokoju, kiedy nadaje/pisze, długość wiadomości. Treści nie zna.
- Prymitywy wyłącznie z audytowanych bibliotek (RustCrypto/`ring`, `snow`); żadnej własnej kryptografii niskiego poziomu. Protokół ma być krótko opisany w `docs/` i pokryty testami.

### 6.4 Czat tekstowy w pokoju **[ustalone: czat; zakres — decyzja projektanta]**

- Każdy pokój ma prosty czat. Wiadomość widzą osoby **obecne w pokoju w chwili wysłania**.
- **Brak historii na serwerze.** Serwer tylko przekazuje zaszyfrowaną wiadomość (strumień sterujący, niezawodny). Klient trzyma historię w pamięci do zamknięcia aplikacji. Wynika to naturalnie z E2E z rotacją kluczy: osoba dołączająca później nie ma kluczy do starszych wiadomości.
- Wyłącznie tekst (UTF-8, limit ~2000 znaków). Adresy URL i ciągi typu `IP:port` rozpoznawane i **kopiowane do schowka kliknięciem** (do wklejenia w grze).
- Nowa wiadomość, gdy okno jest zwinięte: krótki dźwięk (wyłączalny) + licznik/znacznik na ikonie w zasobniku. Bez powiadomień systemowych typu toast, które mogłyby wyskakiwać nad grą **[decyzja projektanta]**.
- Wiadomości pokazują autora i godzinę; bez edycji, usuwania, reakcji, załączników.

### 6.5 Konta i dostęp

- Login + hasło; serwer przechowuje skrót **Argon2id**. Hasło służy tylko do logowania na serwer — nie ma związku z kluczami E2E.
- **Rejestracja kodem zaproszenia** generowanym przez admina (`komunikator-server invite`); bez e-maili **[decyzja projektanta]**.
- Admin = osoba z dostępem do serwera; zarządzanie przez CLI serwera (zaproszenia, blokowanie kont, reset hasła, pokoje).
- Klient pamięta sesję (token odświeżany), żeby nie logować się przy każdym uruchomieniu.

### 6.6 Pokoje

- Lista pokoi trwała (przetrwa restart serwera). Każdy zalogowany może utworzyć pokój; usuwać — twórca lub admin **[decyzja projektanta]**.
- Użytkownik jest w jednym pokoju naraz (lub w żadnym). Zmiana pokoju = podwójne kliknięcie.
- Wszyscy widzą, kto jest w którym pokoju i kto akurat mówi.

### 6.7 Interfejs klienta

- **Natywne okno Win32** (`windows-rs` lub `native-windows-gui`) **[decyzja projektanta]**. Bez WebView. (Alternatywa rozważona: `iced` — lekki, ale nie natywne kontrolki.)
- **Okno główne:** po lewej drzewo pokoi z osobami (wskaźnik mówienia; przy każdej osobie regulacja głośności — suwak lub menu kontekstowe — i lokalne wyciszenie); po prawej czat bieżącego pokoju (lista wiadomości + pole wpisywania); **duże przyciski Mute i Deafen** widoczne zawsze; opcja „zawsze na wierzchu”.
- **Ikona w zasobniku:** stan (połączony / wyciszony / rozłączony / nowa wiadomość), kliknięcie przełącza mute, menu: pokaż okno, mute, deafen, wyjdź. Zamknięcie okna = zwinięcie do zasobnika.
- **Globalne skróty klawiszowe** (konfigurowalne): mute, deafen. Muszą działać, gdy gra ma fokus.
  - Gracze grają w bardzo różne gry, w tym z anti-cheatami **[ustalone]**, więc mechanizm musi być „grzeczny” wobec anti-cheatów **[decyzja projektanta]**:
  - podstawowo **`RegisterHotKey`** — standardowe API, bez wstrzykiwania i bez hooków;
  - **bez `WH_KEYBOARD_LL` i bez żadnej ingerencji w procesy gier** (żadnego wstrzykiwania DLL, czytania pamięci, nakładek);
  - jeśli skrót nie działa przy grze uruchomionej jako administrator — opcja w ustawieniach „uruchamiaj jako administrator”; w ostateczności zostaje przycisk w oknie na drugim monitorze i ikona w zasobniku.
  - Bez płatnego certyfikatu podpisu kodu na start (jednorazowe ostrzeżenie SmartScreen akceptowalne); do ponownego rozważenia, jeśli antywirusy/anti-cheaty zaczną blokować klienta.
- **Krótkie dźwięki potwierdzenia** mute/unmute, wejścia/wyjścia osoby z pokoju i nowej wiadomości (żeby nie trzeba było patrzeć na ekran); każdy wyłączalny.
- **Ustawienia:** urządzenia wej./wyj., czułość VAD (z podglądem poziomu), skróty, dźwięki, autostart z Windows. Plik TOML w `%APPDATA%`. Głośności osób zapamiętywane po nazwie użytkownika.
- Wsparcie wysokiego DPI i wielu monitorów (okno pamięta pozycję i rozmiar).

### 6.8 Serwer

- Jeden plik binarny (Rust), usługa **systemd** na Debianie, jeden port UDP (konfigurowalny).
- Dane: **SQLite** (konta, klucze publiczne, pokoje, zaproszenia). Brak przechowywania wiadomości czatu. Konfiguracja: TOML.
- Gorąca ścieżka: odbiór datagramu → sprawdzenie, czy nadawca jest w pokoju → przekazanie do pozostałych. Bez dekodowania.
- Ograniczenie częstotliwości prób logowania; limity rozmiaru/liczby pakietów i wiadomości na klienta.
- Logi przez journald; bez logowania treści.

### 6.9 Instalacja i aktualizacje — **[decyzja projektanta; użytkownik jeszcze nie zdecydował]**

- Klient jako **pojedynczy plik .exe** (bez instalatora), statycznie linkowany.
- **Auto-aktualizacja z własnego serwera:** klient przy starcie sprawdza wersję, pobiera nowy plik, weryfikuje **podpis Ed25519** (klucz publiczny wbudowany w klienta), podmienia się i restartuje — tylko gdy użytkownik nie jest w rozmowie.
- **Klucz podpisu wydań trzymany offline** na komputerze osoby budującej wydania, **nigdy na serwerze** — inaczej przejęty serwer mógłby podsunąć klienta wykradającego rozmowy (spójne z modelem zagrożenia w 6.3) **[decyzja projektanta]**. Serwer tylko hostuje pliki.
- Format protokołu wersjonowany od pierwszej wersji (numer wersji w uzgadnianiu połączenia), żeby stary i nowy klient mogły współistnieć w okresie aktualizacji.
- Pierwsza konfiguracja: adres serwera + kod zaproszenia (lub gotowy plik konfiguracyjny od admina).

## 7. Testowanie

- Testy jednostkowe: kodowanie/dekodowanie pakietów, protokół sterujący, kryptografia (wektory testowe, odrzucanie powtórzeń, rotacja epok), rozpoznawanie `IP:port`/URL w czacie.
- Testy integracyjne: serwer + kilku klientów bez audio (symulowane ramki i wiadomości).
- Testy sieciowe: utrata pakietów 5–10%, jitter, chwilowe zerwanie łącza (`tc netem` na serwerze, `clumsy` na Windows) — dźwięk ma się zdegradować płynnie i wrócić sam.
- Pomiary: RAM/CPU klienta w trakcie 2-godzinnej rozmowy z włączoną grą; opóźnienie usta→ucho.
- Test echa: gracz na głośnikach, pozostali oceniają.

## 8. Proponowana kolejność prac **[decyzja projektanta]**

Gracze **nie potrzebują wcześniejszej wersji roboczej** **[ustalone]** — kolejność może iść od fundamentów; pierwsze wydanie dla znajomych to wersja kompletna. Po testach w realnym użyciu (echo, skróty w grach) — iteracja poprawek.

1. **Spike audio:** WASAPI → przetwarzanie (AEC A) → Opus → dekodowanie → głośniki lokalnie; weryfikacja wyłączenia duckingu i zużycia zasobów.
2. Serwer + klient bez interfejsu: logowanie, jeden pokój, przekazywanie audio, ponowne łączenie.
3. E2E.
4. Wiele pokoi, głośność per osoba, mute/deafen, czat.
5. Interfejs Win32 + zasobnik + skróty + dźwięki.
6. Konta z zaproszeniami, CLI admina.
7. Auto-aktualizacja i pakowanie; wdrożenie serwera (systemd).

## 9. Zostawione wykonawcy

Struktura crate'ów (workspace: `client`, `server`, `protocol`), format wiadomości sterujących (np. `postcard`/`bincode`), wybór runtime'u async, dokładne parametry bufora jittera i VAD, mechanizm skrótów, szczegóły certyfikatu TLS — w ramach powyższych celów i anty-celów.
