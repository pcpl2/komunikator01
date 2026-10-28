# Akceptacja planu 1 (fundament)

Data: 2026-10-28. Maszyna deweloperska: Windows 11, **bez mikrofonu**.

## Zweryfikowane automatycznie / lokalnie

| Punkt | Wynik |
|---|---|
| cargo test --workspace | wszystkie testy przechodzą |
| Serwer + 2 klientów CLI lokalnie | logowanie, lista pokoju, dołączanie/wychodzenie, mute — OK |
| Odtwarzanie WASAPI (render-only) | 100 ramek w 1,96 s — tempo czasu rzeczywistego |
| RAM klienta (release, połączony, bez mikrofonu) | working set 13,6 MB, prywatna 2,5 MB |
| Rozmiar binariów (release) | komunikator.exe 3,6 MB, komunikator-server.exe 3,5 MB |
| Restart serwera w trakcie | test automatyczny reconnects_after_server_restart — OK |

## Do sprawdzenia przez graczy (wymaga mikrofonów)

1. Dwa komputery z klientem: słychać się nawzajem.
2. Po 30 min rozmowy: Get-Process komunikator — working set < 30 MB, średnie CPU < 1%.
3. Restart serwera w trakcie: dźwięk wraca sam w <= 5 s.
4. Wyjęcie słuchawek USB w trakcie: dźwięk wraca na urządzeniu domyślnym.
5. Gra/muzyka NIE jest ściszana przez Windows (ducking).
6. Gracz na głośnikach: czy druga strona słyszy echo? Jeśli tak — dokładamy WebRTC AEC (spec 6.2, opcja B).
7. cargo run -p client-win --example loopback (mikrofon -> głośniki 10 s) na komputerze z mikrofonem.

Wyniki: _do uzupełnienia_.
