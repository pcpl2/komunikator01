# Akceptacja planu 4 (okno programu, zasobnik, skróty, dźwięki, ustawienia)

Maszyna deweloperska: Windows 11, **bez mikrofonu**. Testy automatyczne obejmują model okna (`AppState`), ustawienia, hasło DPAPI, skróty (`RegisterHotKey`, wykrywanie zajętego skrótu), dźwięki, ikony, okna dialogowe (walidacja, Enter/Esc, zajęty skrót) i autotest okna (`komunikator --smoke`); poniższe punkty wymagają prawdziwych graczy.

## Do sprawdzenia przez graczy

1. **Pierwsze uruchomienie:** usuń `%APPDATA%\komunikator\settings.toml` i uruchom `komunikator.exe`. Okno pyta o adres, odcisk, nazwę i hasło; błędne pola są opisane po polsku obok pola; `Anuluj` kończy program, `OK` łączy z serwerem.
2. **Skróty w grach:** ustaw `Wycisz mikrofon` i `Wycisz wszystkich` w `Ustawieniach`, wejdź do gry pełnoekranowej i użyj skrótów: działają za każdym razem, także w grze z anti-cheatem (np. Valorant / FACEIT / EAC) i w grze uruchomionej jako administrator (wtedy z zaznaczonym `Uruchamiaj jako administrator`). Gra nie zgłasza programu jako podejrzanego.
3. **Klik w ikonę w zasobniku wycisza:** lewy klik przełącza mikrofon (ikona czerwona / zielona), prawy otwiera menu `Pokaż okno`, `Wycisz mikrofon`, `Wycisz wszystkich`, `Wyjdź`.
4. **Dźwięki słyszalne, ale krótkie:** wyciszenie, wejście/wyjście osoby i nowa wiadomość przy zminimalizowanym oknie dają krótki, cichy sygnał; każdy da się wyłączyć w `Ustawieniach`.
5. **Drugi monitor z innym DPI:** przeciągnij okno na monitor ze skalowaniem 150 % (lub 100 %), sprawdź ostrość czcionek i układ; okna `Ustawienia`, `Nowy pokój` i `Kod bezpieczeństwa` otwierają się nad oknem programu w dobrej skali.
6. **Restart pamięta okno i głośności:** zmień rozmiar i położenie okna, ustaw komuś 50 % głośności, wyjdź z programu (`Wyjdź`) i uruchom ponownie: okno w tym samym miejscu, głośność tej osoby 50 %.
7. **Link z czatu wklejony do gry:** kolega wysyła `192.168.1.10:27015`; kliknięcie wiadomości kopiuje adres, `Ctrl+V` w konsoli gry wkleja go.
8. **Ustawienia urządzeń i czułości:** zmiana mikrofonu lub głośników w `Ustawieniach` działa od razu, bez rozłączania; pasek poziomu mikrofonu rusza się przy mówieniu; wyższa czułość łapie cichszą mowę.
9. **Kod bezpieczeństwa:** prawy klik na osobie w swoim pokoju → `Kod bezpieczeństwa…`; obie osoby widzą ten sam kod, po wpisaniu pierwszej grupy i `Zweryfikuj` osoba ma `[zweryfikowany ✓]`. Po reinstalacji u kolegi pojawia się `ZMIENIONY KLUCZ` i przycisk `Akceptuj nowy klucz`.
10. **Nowy pokój:** `Nowy pokój…` → nazwa → po chwili jesteś w nowym pokoju.
11. **Zmiana konta:** `Ustawienia…` → `Konto…`, zmiana nazwy użytkownika → program łączy się od nowa jako ta osoba.
12. **Zasoby przez 2 godziny rozmowy:** Menedżer zadań, karta Szczegóły: `komunikator.exe` ma < 30 MB pamięci (zestaw roboczy) i < 1 % CPU przez całą 2-godzinną sesję, także przy zminimalizowanym oknie.
13. **Dźwięk gry nie jest ściszany:** w trakcie rozmowy dźwięk gry ma tę samą głośność co bez komunikatora (Windows nie włącza „ściszania komunikacji”).
14. **Drugie uruchomienie:** uruchom program drugi raz, gdy siedzi w zasobniku → pokazuje się istniejące okno, nie ma drugiej ikony.
15. **Ikona w zasobniku po zmianie DPI:** przeciągnij okno na monitor z innym skalowaniem, wycisz skrótem → ikona w zasobniku robi się czerwona.

Wyniki: _do uzupełnienia_.
