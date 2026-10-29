//! The server baked in at build time (env `KOMUNIKATOR_SERVER`, `KOMUNIKATOR_FINGERPRINT`, set by
//! the release workflow), used wherever the user has not given one. Unset in local builds.

/// `host:port` of the built-in server, if any.
pub fn server() -> Option<String> {
    nonempty(option_env!("KOMUNIKATOR_SERVER"))
}

/// The built-in server's certificate fingerprint (64 hex), if any.
pub fn fingerprint() -> Option<String> {
    nonempty(option_env!("KOMUNIKATOR_FINGERPRINT")).map(|f| f.to_ascii_lowercase())
}

fn nonempty(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_values_are_unset() {
        assert_eq!(nonempty(None), None);
        assert_eq!(nonempty(Some("  ")), None);
        assert_eq!(nonempty(Some(" serwer.pl:7700 ")).as_deref(), Some("serwer.pl:7700"));
    }

    #[test]
    fn built_in_values_are_well_formed() {
        if let Some(fp) = fingerprint() {
            assert!(protocol::parse_fingerprint(&fp).is_some(), "KOMUNIKATOR_FINGERPRINT: {fp}");
        }
        if let Some(s) = server() {
            assert!(s.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok()), "KOMUNIKATOR_SERVER: {s}");
        }
    }
}
