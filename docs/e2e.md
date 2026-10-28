# Szyfrowanie end-to-end (plan 2)

Dźwięk jest szyfrowany na urządzeniu nadawcy i odszyfrowywany dopiero u odbiorców. Serwer przekazuje bajty, których nie umie odczytać. Opis zgodny z kodem w `crates/client-core/src/e2e/`; założenia w specyfikacji, sekcja 6.3. Model zagrożenia: serwer może być wrogi (np. przejęty VPS).

## Tożsamość urządzenia

Każde urządzenie ma długoterminową tożsamość: klucz Ed25519 (podpisy), klucz X25519 (Noise) i losowy 16-bajtowy identyfikator urządzenia. Na Windows plik `identity.bin` (katalog `--data-dir`, domyślnie `%APPDATA%\komunikator`) to wynik DPAPI w zakresie bieżącego użytkownika; plik nie działa na innym koncie ani komputerze. Gdy pliku nie da się odszyfrować, jest odkładany jako `identity.bin.broken` i powstaje nowa tożsamość. Nowa tożsamość ma nowy losowy identyfikator urządzenia, więc znajomi zobaczą Cię jako „nowe urządzenie” (dźwięk działa dalej) i powinni porównać z Tobą kod bezpieczeństwa. Ostrzeżenie „zmieniony klucz” pojawia się tylko wtedy, gdy ten sam identyfikator urządzenia przyjdzie z innym kluczem, np. przy podszywaniu się. Przy innym błędzie odczytu (np. brak dostępu) klient się zatrzymuje, nic nie nadpisując.

## Ogłoszenie

Po wejściu do pokoju klient wysyła ogłoszenie podpisane tożsamością: nazwa użytkownika, pokój, klucze publiczne, znacznik czasu. Podpis obejmuje stałą domenową `komunikator/announce/v1`. Odbiorca przyjmuje ogłoszenie tylko, gdy podpis jest poprawny, nazwa zgadza się z nazwą, którą serwer podaje na liście pokoju, pokój jest właściwy, a znacznik czasu różni się od lokalnego zegara o najwyżej 1 godzinę (dlatego zegar komputera musi być w przybliżeniu poprawny). Serwer buforuje ostatnie ogłoszenie i podaje je nowo wchodzącym. Klient ogłasza się ponownie co 30 minut, żeby bufor nie zbliżył się do limitu 1 godziny. Odrzucone ogłoszenie jest wypisywane w CLI. Urządzenie nigdy nie paruje się samo ze sobą.

Klucze dostają wyłącznie osoby, które klient sam widzi na liście pokoju i których ogłoszenie przeszło weryfikację. Serwer nie może dopisać „ducha”.

## Kanały parami (Noise KK)

Dla każdej pary urządzeń powstaje kanał Noise `Noise_KK_25519_ChaChaPoly_BLAKE2s`, relayowany przez serwer. Obie strony znają już swoje klucze statyczne z ogłoszeń. Handshake zaczyna urządzenie z mniejszym kluczem podpisu. Prolog wiąże oba klucze podpisu. Po handshake kanał przechodzi w tryb bezstanowy: każda wiadomość niesie jawny 64-bitowy nonce (`nonce u64 LE ‖ szyfrogram`), a odbiorca przyjmuje tylko ściśle rosnące nonce. Zgubione lub przestawione przez serwer wiadomości nie rozsynchronizowują kanału, a powtórki są odrzucane. Kanałem idzie jedna rzecz: klucz nadawcy z numerem epoki.

## Klucze nadawcy i epoki

Każdy uczestnik ma własny losowy 32-bajtowy klucz nadawcy, ważny przez jedną epokę (numer 16-bitowy, z zawijaniem). Klucz audio to `HKDF-SHA256(klucz nadawcy, info = "komunikator/audio/v1")`. Ramka audio szyfruje Opus algorytmem ChaCha20-Poly1305:

- nonce 12 bajtów: licznik ramek `seq` (u32, little-endian) w pierwszych 4 bajtach, reszta zera;
- AAD 9 bajtów: rodzaj datagramu, id nadawcy, epoka, `seq` (jawne w nagłówku, ale uwierzytelnione);
- nadawca nie użyje dwa razy tego samego `seq` pod jednym kluczem; odbiorca odrzuca powtórzenia.

Serwer rozsyła ten sam zaszyfrowany datagram wszystkim i nie zna żadnego klucza.

## Rotacja

Gdy ktoś wchodzi do pokoju lub z niego wychodzi, każdy uczestnik tworzy nowy klucz nadawcy (nowa epoka) i rozsyła go kanałami parami. Przełączenie na nowy klucz następuje po 500 ms, żeby odbiorcy zdążyli go dostać. Kolejne zmiany w tym czasie są łączone w jedną rotację, ale przełączenie przesuwa się najwyżej o 1 s ponad pierwszy termin, więc ciągłe wchodzenie i wychodzenie nie utrzyma starego klucza u kogoś, kto już wyszedł. Odbiorca przez 2 s po zmianie nadal przyjmuje poprzednią epokę, więc rotacja nie robi przerwy w dźwięku. Odbiorca trzyma najwyżej trzy klucze danego nadawcy: najnowszy, ten, którym ostatnio przyszedł dźwięk, oraz poprzedni najnowszy, dopóki najnowszym nie przyjdzie pierwsza ramka. Dzięki temu nadawca, który milczał (VAD) w chwili przełączenia, nie traci słuchaczy, gdy w międzyczasie przyjdzie kolejna rotacja.

Nowa osoba nie usłyszy rozmowy sprzed swojego wejścia, a wychodzący nie usłyszy rozmowy po wyjściu. Zasada dolna (key floor): tożsamość nigdy nie dostaje klucza, którym zaszyfrowano już dźwięk przed otwarciem jej kanału. Jeśli taki klucz byłby jedynym dostępnym, nadawca najpierw robi rotację i wysyła świeży, jeszcze nieużyty.

## Pokój w ogłoszeniu

Ogłoszenie jest podpisane dla konkretnego pokoju (numer pokoju z `Welcome`). Klient przyjmuje tylko ogłoszenia podpisane dla swojego pokoju; ogłoszenie z innego pokoju jest odrzucane („zły pokój”), nawet jeśli serwer twierdzi, że nadawca siedzi obok. Serwer nie przeniesie więc niczyjego urządzenia do innego pokoju bez wiedzy uczestników.

## Czat

Wiadomości czatu są szyfrowane tym samym kluczem nadawcy co dźwięk, ale z osobnym kluczem pochodnym: `HKDF-SHA256(klucz nadawcy, sól pusta, info = "komunikator/chat/v1")`, więc klucz czatu i klucz audio są od siebie niezależne. Wiadomość to postcard struktury `{ epoka u16, licznik u32, szyfrogram }`, szyfrowana ChaCha20-Poly1305:

- nonce 12 bajtów: licznik (u32, little-endian) w pierwszych 4 bajtach, reszta zera; licznik liczy od 0 dla każdego klucza, a po `u32::MAX` nadawca odmawia dalszego szyfrowania tym kluczem;
- AAD: `"komunikator/chat/v1"` ‖ pokój u32 LE ‖ id nadawcy u16 LE ‖ epoka u16 LE ‖ licznik u32 LE. Serwer nie przeniesie wiadomości do innego pokoju ani nie przypisze jej komuś innemu.

Odbiorca trzyma trzy najnowsze epoki każdego nadawcy (wiadomość wysłana tuż przed rotacją otwiera się po niej), nigdy nie podmienia klucza znanej epoki i dla każdego klucza przyjmuje tylko ściśle rosnące liczniki: strumień sterujący jest uporządkowany, więc powtórka lub stara wiadomość jest odrzucana. Najwyższy otwarty licznik jest zapamiętany dla pary (tożsamość, od której klucz przyszedł, klucz) — nie dla numeru osoby ani epoki — przez cały czas działania programu (także po zmianie pokoju i ponownym połączeniu), dla 64 ostatnio używanych kluczy. Inny uczestnik, który przekaże cudzy klucz własnym kanałem, nie zablokuje więc wiadomości jego właściciela. Serwer nie wyzeruje go więc, odbudowując kanał, „usuwając i dodając” osobę ani podmieniając urządzenie, a znajomy po restarcie programu (te same epoki, ale nowe losowe klucze) nie jest brany za powtórkę. Tekst po odszyfrowaniu musi być poprawnym UTF-8 o długości najwyżej 2000 znaków. Serwer nie przechowuje czatu; przekazuje tylko zaszyfrowane bloby.

Nowa osoba nie przeczyta wiadomości sprzed swojego wejścia: czat szyfruje zawsze ten sam klucz, którym w danej chwili szyfrowany jest dźwięk, a klucze dla czatu odbiorca dostaje dokładnie tam, gdzie klucze dźwięku. Zasada dolna (key floor) obejmuje więc i czat: nowa osoba dostaje tylko klucze, którymi nic jeszcze nie zaszyfrowano przed otwarciem jej kanału. Wychodzący z tego samego powodu nie przeczyta wiadomości po rotacji.

## Zaufanie (TOFU)

Klient zapamiętuje tożsamości w `known_devices.toml` (nazwa, id urządzenia, klucz podpisu, flaga weryfikacji; zapis atomowy przez plik tymczasowy z `fsync`). Stany:

| Stan | Znaczenie | Dźwięk |
|---|---|---|
| nowa osoba | pierwszy kontakt z tą nazwą | tak, z widocznym oznaczeniem |
| nowe urządzenie | znana nazwa, nowe urządzenie; zostaje w tym stanie przy każdym połączeniu, dopóki nie zweryfikujesz kodu (`z <id>`) | tak, z widocznym ostrzeżeniem |
| znany, niezweryfikowany | jedyne zapisane urządzenie tej osoby, ten sam klucz co poprzednio, kod niesprawdzony | tak |
| zweryfikowany | użytkownik potwierdził kod bezpieczeństwa (`z <id>`) | tak |
| zmieniony klucz | to samo urządzenie, inny klucz podpisu | **nie** (do `a <id>`) |

Kod bezpieczeństwa to cztery grupy po 4 cyfry, wyliczone ze skrótu SHA-256 obu kluczy podpisu (kolejność bez znaczenia, więc obie strony widzą ten sam kod). Porównujecie go głosowo lub innym kanałem. Status „niezweryfikowany” jest zawsze widoczny na liście (`k`) i w komunikatach, dopóki kod nie zostanie potwierdzony. Jeśli pliku zaufania nie da się odczytać (uszkodzony), jest odkładany jako `known_devices.toml.broken`, klient zaczyna od pustej listy i wyraźnie o tym ostrzega: wszyscy znajomi pojawią się jako nowe osoby, a weryfikacje trzeba powtórzyć. Jeśli nie można zapisać pliku zaufania, klient wypisuje ostrzeżenie, a zmiany obowiązują do zamknięcia programu.

## Co widzi serwer

Kto jest w pokoju, kiedy wchodzi i wychodzi, kiedy i jak dużo kto nadaje (rozmiar i tempo datagramów), rozmiary wiadomości sterujących, ogłoszenia (nazwa, klucze publiczne) oraz zaszyfrowane kanały parami. Nie widzi treści dźwięku ani kluczy nadawcy.

## Czego to nie chroni

- Metadanych: kto z kim, kiedy i jak długo mówi, wzorców mówienia (VAD zdradza, kiedy ktoś milczy).
- Przed odmową usługi: serwer może gubić, opóźniać lub blokować ruch albo odmówić ogłoszeń.
- Nowa osoba i nowe urządzenie nadal dostają klucze, tylko z widocznym ostrzeżeniem. Chroni to tylko użytkownik, który to ostrzeżenie zauważy i porówna kod. Wrogi serwer może podsunąć własne urządzenie pod nową nazwą; wykryje go wyłącznie weryfikacja kodu.
- Przejętego komputera: kto ma dostęp do konta Windows użytkownika, ma jego tożsamość.
- Osoby, która już jest w pokoju i słyszy rozmowę (może ją nagrać).
