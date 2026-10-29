# Kanał aktualizacji (plan 6)

Klient pobiera nowe wydania z tego samego serwera i portu UDP co rozmowy, ale osobnym protokołem QUIC o ALPN `komunikator-update/1`. Ten protokół i format wiadomości opisany niżej są **zamrożone na zawsze**: nowsze wersje głównego protokołu (`PROTOCOL_VERSION`) mogą się zmieniać, a stary klient wciąż musi umieć pobrać aktualizację z nowego serwera i odwrotnie. Zmiana formatu wymaga nowego ALPN (np. `komunikator-update/2`), a serwer powinien dalej obsługiwać `/1`. Kod: `crates/server/src/updates.rs`, `crates/protocol/src/update.rs`; założenia w specyfikacji, sekcja 6.9.

## Model zaufania

Klucz podpisujący wydania (Ed25519) jest przechowywany offline i nigdy nie trafia na serwer. Serwer tylko udostępnia pliki: niczego nie weryfikuje i nie ma czym. Klient ufa wyłącznie manifestowi z poprawnym podpisem klucza wbudowanego w program, plikowi o zgodnym rozmiarze i skrócie SHA-256 oraz wersji ściśle nowszej od własnej (brak cofania wersji). Wrogi serwer może najwyżej nie podać aktualizacji.

## Połączenie

- TLS 1.3 z przypiętym odciskiem certyfikatu serwera, jak przy rozmowach; ALPN `komunikator-update/1`.
- Bez logowania.
- Jedno żądanie na połączenie: klient otwiera jeden dwukierunkowy strumień, wysyła żądanie i zamyka swoją stronę strumienia; serwer odpowiada, zamyka swoją stronę i kończy połączenie.
- Niepoprawne żądanie (nieznany rodzaj, zła długość, nadmiarowe bajty) serwer kończy zamknięciem połączenia z kodem 2, bez odpowiedzi.
- Limity: najwyżej 4 jednoczesne połączenia aktualizacji z jednego adresu IP i 20 łącznie; nadmiarowe połączenie jest od razu zamykane z kodem 3. Otwarcie strumienia i całe żądanie muszą zmieścić się w 10 s od nawiązania połączenia, odpowiedź z manifestem w kolejnych 10 s, a przesłanie pliku trwa najwyżej 10 minut; klient, który nie odbiera danych, traci połączenie po tym czasie. Po odczytaniu żądania serwer nie pozwala otworzyć kolejnych strumieni.

## Żądania i odpowiedzi

Wszystkie liczby w kolejności little-endian.

| Żądanie | Bajty |
|---|---|
| manifest | `01` |
| plik | `02`, długość nazwy (u8), bajty nazwy |

Odpowiedź na żądanie manifestu: długość (u32) i tyle bajtów pliku `data_dir/updates/manifest.bin`. Długość 0 oznacza brak wydania (brak pliku albo plik większy niż 4096 bajtów, `MAX_MANIFEST`).

Odpowiedź na żądanie pliku: długość (u64) i tyle bajtów pliku. Serwer wysyła plik tylko wtedy, gdy:

- nazwa jest dokładnie równa polu `file` bieżącego manifestu (odczytanemu bez sprawdzania podpisu),
- nazwa jest „zwykła”: 1–64 znaki `[A-Za-z0-9._-]`, nie zaczyna się od kropki (`plain_file_name`), więc nigdy nie jest ścieżką,
- plik `data_dir/updates/<nazwa>` istnieje, jest zwykłym plikiem (nie dowiązaniem symbolicznym; dowiązań serwer nie podąża, dotyczy to też `manifest.bin`) i ma najwyżej 64 MiB (`MAX_RELEASE_SIZE`).

W każdym innym przypadku odpowiedzią jest długość 0. Serwer nie czyta żadnej innej ścieżki niż `manifest.bin` i plik nazwany w manifeście. Plik jest wysyłany kawałkami po 64 KiB, bez wczytywania całości do pamięci. Jeśli plik skurczy się w trakcie wysyłania, serwer przerywa strumień (reset) zamiast udawać kompletną odpowiedź; klient i tak sprawdza rozmiar i skrót.

## Manifest

`manifest.bin` to koperta postcard:

```
SignedManifest { body: Vec<u8>, signature: ([u8; 32], [u8; 32]) }
```

`body` to postcard struktury `Manifest { version: { major: u16, minor: u16, patch: u16 }, file: String, size: u64, sha256: [u8; 32] }`. `signature` to 64 bajty podpisu Ed25519 (dwie połowy po 32 bajty) nad `"komunikator/release/v1" ‖ body`. Manifest nie może przekraczać 4096 bajtów.

## Publikowanie wydania

1. Na komputerze z kluczem offline: `komunikator-release sign --key <klucz> --exe <exe> --version <x.y.z> --out <katalog>` zapisuje do katalogu wyjściowego exe pod nazwą wydania oraz `manifest.bin`.
2. Wgraj oba pliki do `data_dir/updates/` na serwerze. Najpierw plik wydania, potem `manifest.bin`, żeby klient nigdy nie dostał manifestu wskazującego na brakujący plik.

Serwer czyta pliki przy każdym żądaniu, więc restart nie jest potrzebny.

## Logi

Dla każdego żądania serwer zapisuje adres IP, rodzaj żądania (`manifest` lub `file`) i liczbę wysłanych bajtów; odmowy z powodu limitu też trafiają do logu.
