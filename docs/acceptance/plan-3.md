# Akceptacja planu 3 (pokoje, czat, głośność osób)

Maszyna deweloperska: Windows 11, **bez mikrofonu**. Testy automatyczne obejmują protokół, serwer (pokoje, SQLite, czat), sesję klienta i polecenia CLI; poniższe punkty wymagają prawdziwych graczy.

## Do sprawdzenia przez graczy (wymaga mikrofonów)

1. **Zmiana pokoju w trakcie gry:** `w <nr>` i `o` podczas rozmowy; po wejściu do nowego pokoju słychać tamtejsze osoby w ciągu ok. 1 s, bez trzasków, a poprzedni pokój przestaje być słyszalny. Kody bezpieczeństwa (`k`) dotyczą tylko osób z bieżącego pokoju.
2. **Czat z adresem IP wklejonym do gry:** `t 192.168.1.10:27015` u jednej osoby; u drugiej pojawia się `[HH:MM] imię: …` i osobna linia `  link 1: 192.168.1.10:27015`, którą da się skopiować i wkleić w grze. Czat nie działa poza pokojem (komunikat po polsku).
3. **Głośność 0 / 100 / 200 % na głośnym koledze:** `g <id> 0` (cisza), `g <id> 100`, `g <id> 200` (dwukrotnie głośniej, bez zniekształceń na normalnej mowie); `x <id>` wycisza lokalnie i przywraca poprzedni poziom. Ustawienie wraca po ponownym wejściu tej osoby (pamiętane po nazwie).
4. **Deafen wycisza w obie strony:** `d` — nie słychać nikogo, a rozmówcy nie słyszą nas; ponowne `d` przywraca oba kierunki.
5. **Lista pokoi po restarcie serwera:** po `n Ranked` i restarcie serwera pokój nadal istnieje (`p`), puste pokoje założone przez użytkowników zachowują nazwy, a klienci łączą się ponownie sami.
6. **Uprawnienia:** zwykły użytkownik nie może usunąć cudzego pokoju (`nie możesz usunąć cudzego pokoju`); konto z `admin = true` może, ale nie pokój, w którym ktoś jest (`w pokoju ktoś jest`).
7. RAM i CPU po 30 min rozmowy bez zmian względem planu 2 (working set < 30 MB, CPU < 1%).

Wyniki: _do uzupełnienia_.
