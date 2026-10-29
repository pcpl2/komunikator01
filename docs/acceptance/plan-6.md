# Akceptacja planu 6 (podpisane aktualizacje, statyczny exe, wdrożenie na Debianie)

Testy automatyczne obejmują podpisywanie i weryfikację manifestu, odrzucanie starszych wersji i zmienionych plików, kanał `komunikator-update/1` z prawdziwym serwerem oraz brak importów dynamicznego CRT w plikach exe (`crates/client-win/tests/static_crt.rs`). Poniższe punkty wymagają prawdziwego serwera i znajomych. Procedura wydania: [docs/wydanie.md](../wydanie.md).

## Do sprawdzenia przez graczy

1. **Wydanie opublikowane w trakcie rozmowy:** dwie osoby rozmawiają w pokoju; administrator publikuje nowsze wydanie (`docs/wydanie.md`, „Publikowanie wydania”). Rozmowa trwa bez przerwy, a na pasku stanu pojawia się `aktualizacja gotowa — zostanie zainstalowana, gdy nie będziesz w rozmowie`. Gdy gracz opuści pokój, program sam się zamyka i uruchamia ponownie z komunikatem `zaktualizowano do wersji X`.
2. **Zmieniony plik jest odrzucany:** na serwerze zmień jeden bajt pliku wydania w `/var/lib/komunikator/updates/` (manifest zostaje stary). Klient pokazuje `aktualizacja nieudana: <powód>`, nie podmienia programu i dalej działa w starej wersji. Po przywróceniu pliku następne sprawdzenie kończy się powodzeniem.
3. **Czysty Windows bez VC++ Redistributable:** skopiuj sam `komunikator.exe` na świeżą instalację Windows 11 (albo maszynę wirtualną, na której nie instalowano pakietów Visual C++) i uruchom: okno się otwiera, program łączy się z serwerem i rozmawia. Nie pojawia się błąd o brakującym `VCRUNTIME140.dll` ani `api-ms-win-crt-*.dll`.
4. **Serwer przeżywa restart:** `sudo reboot` na serwerze; po starcie `systemctl status komunikator` pokazuje `active (running)`, a klienci łączą się ponownie bez wpisywania czegokolwiek (ten sam odcisk certyfikatu, te same konta). `journalctl -u komunikator -b` nie zawiera treści rozmów ani haseł.

Wyniki: _do uzupełnienia_.
