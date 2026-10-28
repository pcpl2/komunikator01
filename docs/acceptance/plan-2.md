# Akceptacja planu 2 (szyfrowanie end-to-end)

Data: 2026-10-28. Maszyna deweloperska: Windows 11, **bez mikrofonu**.

## Zweryfikowane automatycznie / lokalnie

| Punkt | Wynik |
|---|---|
| cargo test --workspace | 164 testy przechodzą (w tym testy sesji E2E i magazynu tożsamości) |
| cargo build --release | bez ostrzeżeń |
| Tożsamość w DPAPI | `identity.bin` utworzony, wczytany ponownie z tym samym kluczem publicznym, nie zawiera jawnych bajtów tożsamości; uszkodzony plik odkładany jako `identity.bin.broken` (testy jednostkowe) |
| Serwer + 2 klienty CLI lokalnie (`ala`, `olek`, osobne `--data-dir`) | obaj widzą „nowa osoba” i **taki sam kod bezpieczeństwa** (`4739 1746 0096 2245`); `k` pokazuje listę z id, stanem i kodem; `z 2` u `ala` zmienia stan na „zweryfikowany”; `a 1` u `olek` zmienia na „znany”; po wyjściu `olek` komunikat „wychodzi” |
| Pliki zaufania | `known_devices.toml` zapisany u obu, flaga `verified = true` po `z` |

Nie sprawdzono tu: przepływu dźwięku (brak mikrofonu; klienty działały bez urządzenia audio), komunikatów „nowe urządzenie” i „OSTRZEŻENIE: zmieniony klucz” w żywej sesji (pokrywają je testy automatyczne w `client-core`), zużycia RAM/CPU.

## Do sprawdzenia przez graczy (wymaga mikrofonów)

1. Dwa komputery: słychać się nawzajem, tak jak w planie 1.
2. Porównanie kodów bezpieczeństwa głosowo (`k`): kody u obu osób identyczne; po zgodności `z <id>`.
3. Wejście kogoś w trakcie rozmowy: słychać go i on słyszy innych w ciągu ok. 1 s, bez trzasków; nie słyszy tego, co było mówione przed jego wejściem.
4. Wyjście kogoś w trakcie rozmowy: pozostali nie mają przerwy w dźwięku (okno 2 s na poprzednią epokę).
5. Usunięcie `identity.bin` (lub uruchomienie z innego `--data-dir`) na jednym komputerze: pozostali widzą wyraźne ostrzeżenie „zmieniony klucz” (przy tej samej nazwie i urządzeniu) albo „nowe urządzenie”, a dźwięk dla zmienionego klucza wraca dopiero po `a <id>`.
6. RAM i CPU po 30 min rozmowy bez zmian względem planu 1 (working set < 30 MB, CPU < 1%).
7. Restart serwera w trakcie: dźwięk wraca sam, kody i zaufanie bez zmian.

Wyniki: _do uzupełnienia_.
