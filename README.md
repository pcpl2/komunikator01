# Komunikator

Lekki komunikator głosowy dla małej grupy graczy: natywny klient na Windows (Rust, WASAPI, Opus) i serwer przekazujący dźwięk (QUIC) na Linuksie.

Stan: **plan 1, fundament.** Klient działa w wierszu poleceń i ma jeden pokój. Szyfrowanie E2E, interfejs graficzny, czat i konta z zaproszeniami pojawią się w kolejnych planach (`docs/superpowers/`).

## Budowanie

```
cargo build --release --workspace          # Windows: komunikator.exe + komunikator-server.exe
cargo build --release -p server            # Debian: tylko serwer
```

Wymagania: Rust (stable) i CMake (do zbudowania libopus).

## Serwer

1. Wygeneruj hash hasła dla każdej osoby:
   ```
   echo 'haslo' | komunikator-server hash-password
   ```
2. Przygotuj konfigurację na podstawie `server.example.toml`: adres, katalog danych i listę `[[users]]` z hashami.
3. Uruchom serwer:
   ```
   komunikator-server run --config server.toml
   ```
   Przy starcie serwer wypisuje `fingerprint: <64 znaki hex>`. To odcisk certyfikatu, który trzeba przekazać znajomym. Certyfikat powstaje przy pierwszym uruchomieniu w `data_dir` (`cert.der`, `key.der`) i potem się nie zmienia.
4. Otwórz w firewallu port **UDP 7700** (albo ten ustawiony w `bind`).

## Klient (Windows)

```
set KOMUNIKATOR_PASSWORD=haslo
komunikator --server adres.serwera:7700 --fingerprint <odcisk> --user ala
```

- `m` + Enter: wycisza i włącza mikrofon.
- `d` + Enter: wycisza wszystkich (deafen).
- `q` + Enter: kończy działanie.
- `--list-devices` wypisuje urządzenia. Wybrane urządzenie ustawiasz przez `--capture-device <id>` i `--render-device <id>`; bez tych opcji używane są domyślne urządzenia komunikacyjne Windows.

Klient po zerwaniu połączenia łączy się ponownie sam. Po odłączeniu słuchawek przechodzi na urządzenie domyślne.

## Testy

```
cargo test --workspace
```
