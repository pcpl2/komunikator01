//! Klient ma być jednym plikiem exe bez zależności od VC++ Redistributable
//! (spec 6.9): sprawdzamy to w tablicy importów zbudowanych plików.
#![cfg(windows)]

fn u16_at(b: &[u8], off: usize) -> usize {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap()) as usize
}

fn u32_at(b: &[u8], off: usize) -> usize {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize
}

/// Nazwy DLL z katalogu importów pliku PE (DOS -> PE -> katalog danych nr 1).
fn imported_dlls(b: &[u8]) -> Vec<String> {
    assert_eq!(&b[..2], b"MZ", "brak nagłówka DOS");
    let pe = u32_at(b, 0x3C);
    assert_eq!(&b[pe..pe + 4], b"PE\0\0", "brak nagłówka PE");
    let coff = pe + 4;
    let sections = u16_at(b, coff + 2);
    let opt_size = u16_at(b, coff + 16);
    let opt = coff + 20;
    // PE32 ma katalogi danych od 96, PE32+ od 112.
    let dirs = match u16_at(b, opt) {
        0x10B => opt + 96,
        0x20B => opt + 112,
        m => panic!("nieznany typ pliku PE: {m:#x}"),
    };
    let import_rva = u32_at(b, dirs + 8);
    if import_rva == 0 {
        return Vec::new();
    }
    let table = opt + opt_size;
    let to_offset = |rva: usize| -> usize {
        for i in 0..sections {
            let s = table + i * 40;
            let (virt, size, raw) = (u32_at(b, s + 12), u32_at(b, s + 8).max(u32_at(b, s + 16)), u32_at(b, s + 20));
            if rva >= virt && rva < virt + size {
                return rva - virt + raw;
            }
        }
        panic!("RVA {rva:#x} poza sekcjami");
    };
    let mut names = Vec::new();
    let mut desc = to_offset(import_rva);
    while b[desc..desc + 20].iter().any(|&x| x != 0) {
        let start = to_offset(u32_at(b, desc + 12));
        let len = b[start..].iter().position(|&x| x == 0).unwrap();
        names.push(String::from_utf8_lossy(&b[start..start + len]).to_ascii_lowercase());
        desc += 20;
    }
    names
}

fn assert_static_crt(path: &str) {
    let bytes = std::fs::read(path).unwrap();
    let dlls = imported_dlls(&bytes);
    assert!(dlls.iter().any(|d| d == "kernel32.dll"), "parser nie widzi importów: {dlls:?}");
    let bad: Vec<_> = dlls
        .iter()
        .filter(|d| d.starts_with("vcruntime") || d.starts_with("msvcp") || d.starts_with("api-ms-win-crt-"))
        .collect();
    assert!(bad.is_empty(), "{path} importuje dynamiczne CRT: {bad:?}");
}

#[test]
fn exe_has_no_vcruntime_import() {
    assert_static_crt(env!("CARGO_BIN_EXE_komunikator"));
    assert_static_crt(env!("CARGO_BIN_EXE_komunikator-cli"));
}
