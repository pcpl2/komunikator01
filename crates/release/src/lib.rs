//! Offline release signing: the secret key lives only on the maintainer's machine.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use ed25519_dalek::SigningKey;
use protocol::update::{sign_manifest, Manifest, Version, MAX_RELEASE_SIZE, RELEASE_MARKER_PREFIX};
use sha2::{Digest, Sha256};

/// Lowercase hex, the format of the secret file and of `release-key.pub`.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex_32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Writes a fresh random seed to a new file; never overwrites an existing key.
pub fn keygen(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("brak losowości systemowej: {e}"))?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("nie można utworzyć pliku klucza {} (czy już istnieje?)", path.display()))?;
    writeln!(file, "{}", to_hex(&seed))?;
    Ok(seed)
}

/// Reads the secret seed from a file written by `keygen`.
pub fn load_key(path: &Path) -> anyhow::Result<SigningKey> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("nie można odczytać klucza {}", path.display()))?;
    let seed = from_hex_32(&text).ok_or_else(|| anyhow::anyhow!("plik klucza ma nieprawidłowy format"))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn public_hex(key: &Path) -> anyhow::Result<String> {
    Ok(to_hex(&load_key(key)?.verifying_key().to_bytes()))
}

/// What the window program says about itself: its version and the release key it trusts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReleaseMarker {
    version: Version,
    key: [u8; 32],
}

/// Every well-formed release marker in `bytes` must say the same thing; none means the exe
/// is not the window program (the CLI, the server, anything else).
fn find_marker(bytes: &[u8]) -> anyhow::Result<ReleaseMarker> {
    let prefix = RELEASE_MARKER_PREFIX.as_bytes();
    let mut found: Option<ReleaseMarker> = None;
    let mut versions = Vec::new();
    for start in bytes.windows(prefix.len()).enumerate().filter(|(_, w)| *w == prefix).map(|(i, _)| i) {
        let rest = &bytes[start + prefix.len()..];
        let Some(end) = rest.iter().take(128).position(|&b| b == 0) else { continue };
        let Ok(text) = std::str::from_utf8(&rest[..end]) else { continue };
        let Some((version, key)) = text.split_once(":KEY:") else { continue };
        let Some(key) = from_hex_32(key) else { continue };
        let Some(version) = Version::parse(version) else {
            versions.push(version.to_owned());
            continue;
        };
        let marker = ReleaseMarker { version, key };
        match found {
            Some(f) if f != marker => anyhow::bail!("exe zawiera sprzeczne znaczniki wydania"),
            _ => found = Some(marker),
        }
    }
    if let Some(m) = found {
        return Ok(m);
    }
    match versions.first() {
        Some(v) => anyhow::bail!("exe ma wersję {v}, a wydania muszą mieć postać x.y.z (same cyfry)"),
        None => anyhow::bail!(
            "w exe nie ma znacznika wydania: podpisać można tylko komunikator.exe (program okienkowy) \
             zbudowany z tego repozytorium, nie komunikator-cli.exe ani serwer"
        ),
    }
}

/// Copies the exe into `out` under its release name and writes the signed manifest next to it.
/// The version is the one built into the exe; `version`, if given, must agree with it. The exe
/// must trust the key it is signed with, or its users could never update again.
pub fn sign(key: &Path, exe: &Path, version: Option<&str>, out: &Path) -> anyhow::Result<Manifest> {
    let given = version
        .map(|v| Version::parse(v).ok_or_else(|| anyhow::anyhow!("wersja musi mieć postać x.y.z (same cyfry)")))
        .transpose()?;
    let size = std::fs::metadata(exe)
        .with_context(|| format!("nie można odczytać {}", exe.display()))?
        .len();
    anyhow::ensure!(size <= MAX_RELEASE_SIZE, "plik exe jest większy niż {MAX_RELEASE_SIZE} bajtów");
    let signing = load_key(key)?;
    let bytes = std::fs::read(exe)?;
    let marker = find_marker(&bytes)?;
    if let Some(given) = given.filter(|g| *g != marker.version) {
        anyhow::bail!(
            "podana wersja {given} nie zgadza się z wersją wbudowaną w exe ({}); \
             zwiększ wersję w Cargo.toml i zbuduj ponownie albo pomiń --version",
            marker.version
        );
    }
    anyhow::ensure!(
        marker.key != [0; 32],
        "exe zbudowano z kluczem z samych zer (release-key.pub) i nigdy więcej by się nie zaktualizował: \
         wpisz klucz publiczny do crates/client-win/release-key.pub i zbuduj ponownie"
    );
    let ours = signing.verifying_key().to_bytes();
    anyhow::ensure!(
        marker.key == ours,
        "exe zbudowano z innym kluczem publicznym ({}) niż ten, którym podpisujesz ({}): \
         jego użytkownicy odrzuciliby to wydanie",
        to_hex(&marker.key),
        to_hex(&ours)
    );
    let version = marker.version;
    let file = format!("komunikator-{version}.exe");
    let manifest = Manifest { version, file, size: bytes.len() as u64, sha256: Sha256::digest(&bytes).into() };
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join(&manifest.file), &bytes)?;
    std::fs::write(out.join("manifest.bin"), sign_manifest(&signing, &manifest))?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use protocol::update::verify_manifest;
    use sha2::{Digest, Sha256};

    #[test]
    fn keygen_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release.key");
        keygen(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(keygen(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// A stand-in for the window program: some bytes around its release marker.
    fn fake_exe(dir: &Path, version: &str, key_hex: &str) -> std::path::PathBuf {
        let exe = dir.join("komunikator.exe");
        let marker = format!("{RELEASE_MARKER_PREFIX}{version}:KEY:{key_hex}\0");
        std::fs::write(&exe, [&b"MZ fake exe "[..], marker.as_bytes(), b" more bytes"].concat()).unwrap();
        exe
    }

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, [u8; 32]) {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("release.key");
        let seed = keygen(&key).unwrap();
        (dir, key, seed)
    }

    fn our_hex(seed: &[u8; 32]) -> String {
        to_hex(&SigningKey::from_bytes(seed).verifying_key().to_bytes())
    }

    #[test]
    fn sign_produces_verifiable_manifest() {
        let (dir, key, seed) = setup();
        let exe = fake_exe(dir.path(), "1.2.3", &our_hex(&seed));
        let out = dir.path().join("out");

        let m = sign(&key, &exe, Some("1.2.3"), &out).unwrap();

        let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let bytes = std::fs::read(out.join("manifest.bin")).unwrap();
        assert_eq!(verify_manifest(&public, &bytes).unwrap(), m);
        assert_eq!(m.file, "komunikator-1.2.3.exe");
        let copied = std::fs::read(out.join(&m.file)).unwrap();
        assert_eq!(copied, std::fs::read(&exe).unwrap());
        assert_eq!(m.size, copied.len() as u64);
        assert_eq!(m.sha256, <[u8; 32]>::from(Sha256::digest(&copied)));
    }

    #[test]
    fn the_version_comes_from_the_exe() {
        let (dir, key, seed) = setup();
        let exe = fake_exe(dir.path(), "0.4.1", &our_hex(&seed));
        let out = dir.path().join("out");
        let m = sign(&key, &exe, None, &out).unwrap();
        assert_eq!(m.version, Version::parse("0.4.1").unwrap());
        assert_eq!(m.file, "komunikator-0.4.1.exe");
        // A version that disagrees with the build (a forgotten bump) is refused.
        let err = sign(&key, &exe, Some("0.4.2"), &dir.path().join("out2")).unwrap_err();
        assert!(format!("{err}").contains("0.4.1"), "{err}");
        assert!(!dir.path().join("out2").join("manifest.bin").exists());
    }

    #[test]
    fn an_exe_without_the_marker_is_refused() {
        let (dir, key, _) = setup();
        // The CLI, the server or anything else: no marker.
        let exe = dir.path().join("komunikator-cli.exe");
        std::fs::write(&exe, format!("MZ {RELEASE_MARKER_PREFIX} but nothing valid after it")).unwrap();
        let out = dir.path().join("out");
        let err = sign(&key, &exe, None, &out).unwrap_err();
        assert!(format!("{err}").contains("znacznika"), "{err}");
        assert!(!out.join("manifest.bin").exists());
    }

    #[test]
    fn an_exe_built_with_another_key_is_refused() {
        let (dir, key, _) = setup();
        let out = dir.path().join("out");
        let other = to_hex(&SigningKey::from_bytes(&[3; 32]).verifying_key().to_bytes());
        let exe = fake_exe(dir.path(), "1.0.0", &other);
        assert!(format!("{}", sign(&key, &exe, None, &out).unwrap_err()).contains("innym kluczem"));
        // The committed placeholder: such a build would never update again.
        let exe = fake_exe(dir.path(), "1.0.0", &"0".repeat(64));
        assert!(format!("{}", sign(&key, &exe, None, &out).unwrap_err()).contains("zer"));
        assert!(!out.join("manifest.bin").exists());
    }

    #[test]
    fn conflicting_markers_are_refused() {
        let (dir, key, seed) = setup();
        let hex = our_hex(&seed);
        let exe = dir.path().join("komunikator.exe");
        let a = format!("{RELEASE_MARKER_PREFIX}1.0.0:KEY:{hex}\0");
        let b = format!("{RELEASE_MARKER_PREFIX}1.0.1:KEY:{hex}\0");
        std::fs::write(&exe, format!("MZ{a}xx{b}")).unwrap();
        assert!(sign(&key, &exe, None, &dir.path().join("out")).is_err());
        // The same marker twice is still one release.
        std::fs::write(&exe, format!("MZ{a}xx{a}")).unwrap();
        assert!(sign(&key, &exe, None, &dir.path().join("out")).is_ok());
    }

    #[test]
    fn sign_rejects_bad_version_and_oversize() {
        let (dir, key, seed) = setup();
        let out = dir.path().join("out");
        let exe = fake_exe(dir.path(), "1.2", &our_hex(&seed));
        assert!(sign(&key, &exe, None, &out).is_err());
        let exe = fake_exe(dir.path(), "1.2.3", &our_hex(&seed));
        assert!(sign(&key, &exe, Some("v1.2.3"), &out).is_err());

        let big = dir.path().join("big.exe");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_RELEASE_SIZE + 1).unwrap();
        assert!(sign(&key, &big, Some("1.2.3"), &out).is_err());
        assert!(!out.join("manifest.bin").exists());
    }
}
